// SPDX-License-Identifier: AGPL-3.0-or-later
//! Workdir snapshot — the backup half of disaster recovery.
//!
//! Under DB-authoritative storage neither half of the workdir is
//! reconstructible from the other: `engine.db` holds the facts (claim
//! text, ACL, validity, embeddings, buffers) and the `.md` tree holds
//! their prose renders, styles, and narrative links. The unit of backup
//! is therefore the **workdir snapshot** — both halves taken together
//! (discipline + restore procedure:
//! backup & DR).
//!
//! [`snapshot_workdir`] takes a *hot* snapshot, safe next to a running
//! `mwe-mcp serve` (no lockfile taken):
//!
//! 1. **DB first** — `VACUUM INTO` produces a transactionally
//!    consistent point-in-time copy of `engine.db` without stopping
//!    writers (the source is opened read-only; WAL mode lets readers
//!    coexist with the single writer).
//! 2. **Files second** — the rest of the workdir is copied (`wikis/`,
//!    `prompts/`, `mwe-mcp.env`, …), skipping the DB sidecars, the
//!    lockfile, `logs/`, and in-flight write markers.
//!
//! The order is load-bearing. With the file tree at least as new as the
//! DB image, every divergence the snapshot can contain is one the
//! engine already self-heals: a marker without a row is a standard wiki
//! render residue (rewritten at the next compile) or a smart-wiki fact
//! the reindex re-creates from its inline form; a rendered row whose
//! marker was hand-deleted inside the skew window replays the
//! operator's forget gesture on the first sweep. The reverse order (DB
//! image newer than the files) could instead tombstone committed facts
//! whose render never made it into the copy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::db::ENGINE_DB_FILENAME;
use crate::lockfile::LOCKFILE_NAME;
use crate::watcher::MARKER_SUFFIX;

/// Name of the logs directory inside a workdir — operational output,
/// excluded from snapshots.
const LOGS_DIR_NAME: &str = "logs";

/// `engine_meta` key: unix-seconds stamp of the last automatic
/// snapshot attempt.
///
/// The scheduler compares it against `backup.interval_secs` at each
/// due-check, so a restart never re-fires a snapshot inside the
/// interval.
pub const META_LAST_AUTO_UNIX: &str = "backup.last_auto_unix";

/// `engine_meta` key holding the JSON [`AutoSnapshotReport`] of the last
/// automatic run — the Backup console's status line.
pub const META_LAST_AUTO_REPORT: &str = "backup.last_auto_report";

/// Outcome of one automatic snapshot run, persisted as JSON under
/// [`META_LAST_AUTO_REPORT`] so the dashboard can show it after the
/// fact (the scheduler has no other operator-visible surface).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSnapshotReport {
    /// `true` when the snapshot completed.
    pub ok: bool,
    /// RFC-3339 completion (or failure) stamp.
    pub at: String,
    /// Destination directory of the snapshot (empty on early failures).
    pub dest: String,
    /// One-line summary on success, or the error message on failure.
    pub detail: String,
}

/// Resolved runtime schedule of the automatic-snapshot loop.
///
/// What [`crate::config::BackupConfig::resolved_schedule`] produces and
/// the server's backup scheduler consumes. Shared behind `Arc<RwLock>`
/// with the dashboard Backup console so a settings save hot-applies at
/// the next due-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSchedule {
    /// `false` = the loop idles (no snapshots, no pruning).
    pub enabled: bool,
    /// Distance between consecutive automatic snapshots, in seconds.
    pub interval_secs: u64,
    /// Snapshots home (auto, manual-suggested, and safety snapshots).
    pub dir: PathBuf,
    /// Automatic snapshots kept by the post-run prune; `0` keeps all.
    pub retention_auto: u32,
}

/// Default snapshots home: `<workdir-name>-snapshots`, a sibling of
/// the workdir.
///
/// Guaranteed outside the workdir (the snapshot guard refuses an
/// overlapping destination) and on the same filesystem by default.
#[must_use]
pub fn default_snapshots_dir(workdir: &Path) -> PathBuf {
    let name = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mwe-mcp");
    workdir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}-snapshots"))
}

/// Provenance class of a snapshot in the snapshots home, derived from
/// its directory-name prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// `auto-*` — written by the scheduler; subject to retention.
    Auto,
    /// `manual-*` — the dashboard console's suggested naming; never
    /// pruned.
    Manual,
    /// `pre-restore-*` / `pre-reset-*` — the automatic safety snapshot
    /// a staged recovery takes before destroying anything; never
    /// pruned.
    Safety,
    /// Anything else (e.g. a CLI `--out` the operator pointed here);
    /// never pruned.
    Other,
}

impl SnapshotKind {
    fn of(name: &str) -> Self {
        if name.starts_with("auto-") {
            Self::Auto
        } else if name.starts_with("manual-") {
            Self::Manual
        } else if name.starts_with("pre-restore-") || name.starts_with("pre-reset-") {
            Self::Safety
        } else {
            Self::Other
        }
    }

    /// Short badge label for operator-facing listings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Safety => "safety",
            Self::Other => "other",
        }
    }
}

/// One snapshot found in the snapshots home.
#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    /// Directory name (unique within the home).
    pub name: String,
    /// Absolute path of the snapshot directory.
    pub path: PathBuf,
    /// Provenance class (from the name prefix).
    pub kind: SnapshotKind,
    /// Total bytes across the snapshot's files.
    pub bytes: u64,
    /// Modification time of the snapshot's `engine.db` copy — the
    /// moment the snapshot was taken (the DB is written first).
    pub taken_at: Option<std::time::SystemTime>,
}

/// True when `path` is a directory that looks like a workdir snapshot —
/// it carries an `engine.db` copy at its top level.
#[must_use]
pub fn is_snapshot_dir(path: &Path) -> bool {
    path.join(ENGINE_DB_FILENAME).is_file()
}

/// Enumerate the snapshots in `dir`, newest first.
///
/// A missing home is an empty list, not an error (nothing has been
/// snapshotted yet). Non-snapshot entries (files, directories without
/// an `engine.db`) are skipped.
///
/// # Errors
///
/// [`BackupError::Io`] for filesystem failures other than a missing
/// `dir`.
pub fn list_snapshots(dir: &Path) -> Result<Vec<SnapshotEntry>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !is_snapshot_dir(&path) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let taken_at = std::fs::metadata(path.join(ENGINE_DB_FILENAME))
            .and_then(|m| m.modified())
            .ok();
        out.push(SnapshotEntry {
            kind: SnapshotKind::of(&name),
            bytes: dir_size(&path)?,
            name,
            path,
            taken_at,
        });
    }
    // Newest first. The timestamp-suffixed names sort with their
    // mtimes, but mtime also orders foreign names correctly.
    out.sort_by_key(|s| std::cmp::Reverse(s.taken_at));
    Ok(out)
}

/// Remove the oldest `auto-*` snapshots beyond `keep` (`0` = keep all).
///
/// Only the scheduler's own snapshots are candidates — manual, safety,
/// and foreign snapshots are never touched. Returns the removed names.
///
/// # Errors
///
/// [`BackupError::Io`] for filesystem failures.
pub fn prune_auto_snapshots(dir: &Path, keep: u32) -> Result<Vec<String>> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let mut auto: Vec<SnapshotEntry> = list_snapshots(dir)?
        .into_iter()
        .filter(|s| s.kind == SnapshotKind::Auto)
        .collect();
    if auto.len() <= keep as usize {
        return Ok(Vec::new());
    }
    // `list_snapshots` is newest-first; everything past `keep` goes.
    let mut removed = Vec::new();
    for entry in auto.split_off(keep as usize) {
        std::fs::remove_dir_all(&entry.path)?;
        removed.push(entry.name);
    }
    Ok(removed)
}

/// A fresh `<prefix>-<UTC timestamp>` snapshot name.
#[must_use]
pub fn snapshot_name(prefix: &str) -> String {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    format!("{prefix}-{ts}")
}

/// Total bytes of the files under `path` (recursive).
fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total += dir_size(&entry.path())?;
        } else if ft.is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// Errors raised by [`snapshot_workdir`].
#[derive(Debug, Error)]
pub enum BackupError {
    /// Underlying filesystem error.
    #[error("backup io: {0}")]
    Io(#[from] std::io::Error),
    /// Underlying `SQLite` error (open / `VACUUM INTO`).
    #[error("backup db: {0}")]
    Db(#[from] sqlx::Error),
    /// The workdir has no `engine.db` — nothing to snapshot.
    #[error("workdir {0} has no engine.db to snapshot")]
    NoEngineDb(PathBuf),
    /// The destination directory exists and is not empty.
    #[error("destination {0} is not empty")]
    DestNotEmpty(PathBuf),
    /// The destination overlaps the workdir (either direction) — the
    /// copy would recurse into itself or shadow live state.
    #[error("destination {0} must be outside the workdir (and not contain it)")]
    DestOverlapsWorkdir(PathBuf),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, BackupError>;

/// What [`snapshot_workdir`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupReport {
    /// Size in bytes of the consistent `engine.db` copy.
    pub db_bytes: u64,
    /// Files copied from the workdir tree (the DB copy excluded).
    pub files_copied: usize,
    /// Total bytes of the copied files (the DB copy excluded).
    pub bytes_copied: u64,
}

/// Take a hot workdir snapshot into `dest` (created if missing; must be
/// empty and disjoint from the workdir). See the module docs for the
/// two-step order and why it is safe next to a live server.
///
/// # Errors
///
/// [`BackupError`] on a missing `engine.db`, a non-empty or overlapping
/// destination, or an underlying filesystem / `SQLite` failure.
pub async fn snapshot_workdir(workdir: &Path, dest: &Path) -> Result<BackupReport> {
    let db_path = crate::db::engine_db_path(workdir);
    if !db_path.is_file() {
        return Err(BackupError::NoEngineDb(workdir.to_path_buf()));
    }
    std::fs::create_dir_all(dest)?;
    if std::fs::read_dir(dest)?.next().is_some() {
        return Err(BackupError::DestNotEmpty(dest.to_path_buf()));
    }
    let workdir_canon = workdir.canonicalize()?;
    let dest_canon = dest.canonicalize()?;
    if dest_canon.starts_with(&workdir_canon) || workdir_canon.starts_with(&dest_canon) {
        return Err(BackupError::DestOverlapsWorkdir(dest.to_path_buf()));
    }

    // Step 1 — the DB, first. Read-only connection: a backup must never
    // mutate the source (no migrations, no pragma rewrites).
    let db_dest = dest_canon.join(ENGINE_DB_FILENAME);
    vacuum_into(&db_path, &db_dest).await?;
    let db_bytes = std::fs::metadata(&db_dest)?.len();

    // Step 2 — the file tree.
    let mut report = BackupReport {
        db_bytes,
        ..Default::default()
    };
    copy_tree(
        &workdir_canon,
        &dest_canon,
        /* top_level */ true,
        &mut report,
    )?;

    tracing::info!(
        workdir = %workdir_canon.display(),
        dest = %dest_canon.display(),
        db_bytes = report.db_bytes,
        files_copied = report.files_copied,
        bytes_copied = report.bytes_copied,
        "backup: workdir snapshot complete"
    );
    Ok(report)
}

/// `VACUUM INTO` a point-in-time copy of the `SQLite` database at
/// `src` into the (non-existent) file at `dest`.
async fn vacuum_into(src: &Path, dest: &Path) -> Result<()> {
    use sqlx::sqlite::SqliteConnectOptions;

    let opts = SqliteConnectOptions::new()
        .filename(src)
        .read_only(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = sqlx::SqlitePool::connect_with(opts).await?;
    // `VACUUM INTO` takes a filename literal, not a bind parameter —
    // escape embedded single quotes the SQL way.
    let dest_sql = dest.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{dest_sql}'"))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

/// Recursively copy the workdir tree, skipping operational state that
/// must not travel with a snapshot: the live DB + its WAL/SHM sidecars
/// (replaced by the `VACUUM INTO` copy), the single-writer lockfile,
/// `logs/`, a pending staged-recovery marker (restoring a snapshot that
/// embeds one would re-trigger the recovery in a loop), and in-flight
/// `*.mwe-write-in-progress` markers.
fn copy_tree(src: &Path, dest: &Path, top_level: bool, report: &mut BackupReport) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if top_level
            && (name_str == LOGS_DIR_NAME
                || name_str == LOCKFILE_NAME
                || name_str == crate::recovery::RECOVERY_FILENAME
                || name_str.starts_with(ENGINE_DB_FILENAME))
        {
            continue;
        }
        if name_str.ends_with(MARKER_SUFFIX) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&from, &to, false, report)?;
        } else if ft.is_file() {
            let bytes = std::fs::copy(&from, &to)?;
            report.files_copied += 1;
            report.bytes_copied += bytes;
        }
        // Symlinks and other special files are skipped on purpose: the
        // engine never writes them, so one inside a workdir is foreign.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_workdir(dir: &Path) -> sqlx::SqlitePool {
        let pool = crate::db::open_or_init(dir).await.expect("open_or_init");
        std::fs::create_dir_all(dir.join("wikis/alice")).unwrap();
        std::fs::write(
            dir.join("wikis/alice/_meta.md"),
            "---\nwiki_id: alice\ntitle: alice\nwiki_type: wiki-user\nslug: alice\nparent_wiki_id: null\nacl_default: user:alice\n---\n",
        )
        .unwrap();
        std::fs::write(dir.join("wikis/alice/intro.md"), "prose\n").unwrap();
        std::fs::write(dir.join("mwe-mcp.env"), "MWE_TOKEN_SECRET=s3cret\n").unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("logs/server.log"), "noise\n").unwrap();
        std::fs::write(dir.join(LOCKFILE_NAME), "1234\n").unwrap();
        std::fs::write(dir.join(format!("wikis/alice/intro.md{MARKER_SUFFIX}")), "").unwrap();
        pool
    }

    #[tokio::test]
    async fn snapshot_copies_db_and_tree_and_skips_operational_state() {
        let work = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("snap");

        // Keep the pool open across the snapshot: the hot scenario.
        let pool = seed_workdir(work.path()).await;
        let raw = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
        let fact = crate::fact_index::NewFact {
            authored_refs: Vec::new(),
            fact_id: crate::types::FactId::parse(&raw.to_string()).unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/intro.md".to_owned(),
            region_start: Some(0),
            region_end: Some(5),
            text: "claim".to_owned(),
            embedding: vec![0.0; 4],
            owner_id: "user:alice".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        crate::fact_index::insert(&pool, &fact)
            .await
            .expect("seed row");

        let report = snapshot_workdir(work.path(), &dest)
            .await
            .expect("snapshot");
        assert!(report.db_bytes > 0);
        assert!(report.files_copied >= 3); // _meta.md, intro.md, mwe-mcp.env

        // The DB copy is openable and carries the seeded row.
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dest.join(ENGINE_DB_FILENAME))
            .read_only(true);
        let copy = sqlx::SqlitePool::connect_with(opts)
            .await
            .expect("open copy");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fact_index")
            .fetch_one(&copy)
            .await
            .expect("count");
        assert_eq!(n, 1);

        // Tree copied; operational state excluded.
        assert!(dest.join("wikis/alice/intro.md").is_file());
        assert!(dest.join("mwe-mcp.env").is_file());
        assert!(!dest.join("logs").exists());
        assert!(!dest.join(LOCKFILE_NAME).exists());
        assert!(!dest.join("engine.db-wal").exists());
        assert!(
            !dest
                .join(format!("wikis/alice/intro.md{MARKER_SUFFIX}"))
                .exists()
        );
    }

    #[tokio::test]
    async fn snapshot_rejects_dest_inside_workdir() {
        let work = tempfile::tempdir().unwrap();
        let _pool = seed_workdir(work.path()).await;
        let err = snapshot_workdir(work.path(), &work.path().join("backup"))
            .await
            .expect_err("must reject");
        assert!(matches!(err, BackupError::DestOverlapsWorkdir(_)));
    }

    #[tokio::test]
    async fn snapshot_rejects_non_empty_dest() {
        let work = tempfile::tempdir().unwrap();
        let _pool = seed_workdir(work.path()).await;
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("stray"), "x").unwrap();
        let err = snapshot_workdir(work.path(), out.path())
            .await
            .expect_err("must reject");
        assert!(matches!(err, BackupError::DestNotEmpty(_)));
    }

    #[tokio::test]
    async fn snapshot_rejects_workdir_without_db() {
        let work = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = snapshot_workdir(work.path(), &out.path().join("snap"))
            .await
            .expect_err("must reject");
        assert!(matches!(err, BackupError::NoEngineDb(_)));
    }

    #[tokio::test]
    async fn snapshot_excludes_pending_recovery_marker() {
        let work = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let _pool = seed_workdir(work.path()).await;
        std::fs::write(
            work.path().join(crate::recovery::RECOVERY_FILENAME),
            "{\"action\":\"reset\"}",
        )
        .unwrap();

        let dest = out.path().join("snap");
        snapshot_workdir(work.path(), &dest).await.unwrap();
        assert!(
            !dest.join(crate::recovery::RECOVERY_FILENAME).exists(),
            "a snapshot must never embed a staged-recovery marker"
        );
    }

    #[test]
    fn default_snapshots_dir_is_a_sibling() {
        let dir = default_snapshots_dir(Path::new("/srv/mwe/work"));
        assert_eq!(dir, Path::new("/srv/mwe/work-snapshots"));
    }

    #[test]
    fn snapshot_kind_from_prefix() {
        assert_eq!(
            SnapshotKind::of("auto-20260720T010000Z"),
            SnapshotKind::Auto
        );
        assert_eq!(SnapshotKind::of("manual-x"), SnapshotKind::Manual);
        assert_eq!(SnapshotKind::of("pre-restore-x"), SnapshotKind::Safety);
        assert_eq!(SnapshotKind::of("pre-reset-x"), SnapshotKind::Safety);
        assert_eq!(SnapshotKind::of("my-backup"), SnapshotKind::Other);
    }

    /// A snapshot home with mixed provenance: listing finds only real
    /// snapshots, and the prune touches only the oldest `auto-*` ones.
    #[tokio::test]
    async fn list_and_prune_respect_provenance() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _pool = seed_workdir(work.path()).await;

        for name in ["auto-a", "auto-b", "auto-c", "manual-m", "pre-reset-s"] {
            snapshot_workdir(work.path(), &home.path().join(name))
                .await
                .unwrap();
        }
        // Noise: a stray file and a non-snapshot directory.
        std::fs::write(home.path().join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(home.path().join("not-a-snapshot")).unwrap();

        let listed = list_snapshots(home.path()).unwrap();
        assert_eq!(listed.len(), 5, "noise skipped");
        assert!(listed.iter().all(|s| s.bytes > 0));

        let removed = prune_auto_snapshots(home.path(), 1).unwrap();
        assert_eq!(removed.len(), 2, "3 autos, keep 1");
        assert!(removed.iter().all(|n| n.starts_with("auto-")));
        let left = list_snapshots(home.path()).unwrap();
        assert_eq!(left.len(), 3);
        assert_eq!(
            left.iter().filter(|s| s.kind == SnapshotKind::Auto).count(),
            1
        );
        // keep=0 disables pruning.
        assert!(prune_auto_snapshots(home.path(), 0).unwrap().is_empty());
    }

    #[test]
    fn missing_snapshots_home_lists_empty() {
        let home = tempfile::tempdir().unwrap();
        let gone = home.path().join("never-created");
        assert!(list_snapshots(&gone).unwrap().is_empty());
    }
}
