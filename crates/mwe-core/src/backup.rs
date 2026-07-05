// SPDX-License-Identifier: AGPL-3.0-or-later
//! Workdir snapshot — the backup half of disaster recovery.
//!
//! Under DB-authoritative storage neither half of the workdir is
//! reconstructible from the other: `engine.db` holds the facts (claim
//! text, ACL, validity, embeddings, buffers) and the `.md` tree holds
//! their prose renders, styles, and narrative links. The unit of backup
//! is therefore the **workdir snapshot** — both halves taken together
//! (discipline + restore procedure:
//! [backup & DR](../../../wiki/design-notes/backup-and-dr.md)).
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

use thiserror::Error;

use crate::db::ENGINE_DB_FILENAME;
use crate::lockfile::LOCKFILE_NAME;
use crate::watcher::MARKER_SUFFIX;

/// Name of the logs directory inside a workdir — operational output,
/// excluded from snapshots.
const LOGS_DIR_NAME: &str = "logs";

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
/// `logs/`, and in-flight `*.mwe-write-in-progress` markers.
fn copy_tree(src: &Path, dest: &Path, top_level: bool, report: &mut BackupReport) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if top_level
            && (name_str == LOGS_DIR_NAME
                || name_str == LOCKFILE_NAME
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
}
