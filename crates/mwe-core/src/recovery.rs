// SPDX-License-Identifier: AGPL-3.0-or-later
//! Staged recovery — the restore/reset half of disaster recovery.
//!
//! A running server cannot safely replace its own workdir: the sqlite
//! pool holds `engine.db` open, the wiki watcher holds the tree, and
//! every in-flight request assumes both. Recovery is therefore
//! **staged**: the dashboard writes a one-shot marker
//! ([`RECOVERY_FILENAME`]) into the workdir, and the request is applied
//! at the **next server start** — after the single-writer lockfile is
//! taken, before anything opens the DB — the only moment nothing else
//! has a handle on the state.
//!
//! Two actions, both preceded by an **automatic safety snapshot** into
//! the snapshots home (so even a recovery aimed at the wrong target is
//! itself recoverable):
//!
//! - **Restore** ([`RecoveryAction::Restore`]) — bring the workdir back
//!   to a chosen snapshot: everything except `logs/` and the live
//!   lockfile is replaced by the snapshot's content.
//! - **Reset** ([`RecoveryAction::Reset`]) — wipe the *memory* while
//!   keeping the *installation*: the memory tables are cleared in one
//!   transaction and the `wikis/`, `media/`, and training-spool trees
//!   are removed, while accounts, enrollment, consumers, tokens,
//!   OAuth state, custom skills, config, env, and `prompts/` survive.
//!   Identity wikis are re-scaffolded for every enrolled principal, and
//!   `profile_initialized` is cleared so the welcome wizard re-seeds
//!   each profile on next login.
//!
//! The marker is consumed **before** the action runs (one shot — a
//! failing recovery never becomes a boot loop). A failure before the
//! point of no return leaves the workdir untouched and boots normally,
//! reported via the outcome; a failure after it aborts the boot with an
//! error naming the safety snapshot. Either way the outcome is
//! persisted under [`META_LAST_RECOVERY`] for the Backup console.
//!
//! Snapshots deliberately exclude the marker (see [`crate::backup`]):
//! a snapshot that embedded one would re-trigger the recovery every
//! time it was restored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::backup::{self, BackupError};
use crate::wiki::{IdentityKind, WikiTree, atomic_write, create_identity_wiki};

/// Name of the one-shot staged-recovery marker inside the workdir.
pub const RECOVERY_FILENAME: &str = "recovery-pending.json";

/// `engine_meta` key holding the JSON [`RecoveryOutcome`] of the last
/// applied (or refused) recovery — the Backup console's report line.
pub const META_LAST_RECOVERY: &str = "recovery.last";

/// Errors raised by this module.
///
/// An `Err` out of [`apply_pending`] is **fatal by contract** — the
/// workdir may be half-applied and the caller must abort the boot;
/// every refusal that leaves the workdir untouched is an `Ok` outcome
/// with `ok: false` instead.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// Underlying filesystem error.
    #[error("recovery io: {0}")]
    Io(#[from] std::io::Error),
    /// Marker JSON that does not parse.
    #[error("recovery marker: {0}")]
    Json(#[from] serde_json::Error),
    /// Marker write failure (atomic write surface).
    #[error("recovery marker write: {0}")]
    Wiki(#[from] crate::wiki::WikiError),
    /// A recovery is already pending — cancel it first.
    #[error("a recovery is already pending — cancel it before scheduling another")]
    AlreadyPending,
    /// The action failed **after** its point of no return; the workdir
    /// must not serve as-is. The detail names the safety snapshot.
    #[error("{detail}")]
    Fatal {
        /// Operator-facing description, including where the safety
        /// snapshot was written.
        detail: String,
    },
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, RecoveryError>;

/// What a staged recovery does when applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum RecoveryAction {
    /// Replace the workdir's content with the snapshot at `snapshot`.
    Restore {
        /// Absolute path of the snapshot directory to restore from.
        snapshot: PathBuf,
    },
    /// Clear the memory, keep the installation (see the module docs for
    /// the exact preserve/clear split).
    Reset,
}

impl RecoveryAction {
    /// Human-facing label ("restore from `<name>`" / "memory reset").
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Restore { snapshot } => format!(
                "restore from {}",
                snapshot
                    .file_name()
                    .map_or_else(|| snapshot.to_string_lossy(), |n| n.to_string_lossy())
            ),
            Self::Reset => "memory reset".to_owned(),
        }
    }
}

/// The staged request, serialized as the marker's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRequest {
    /// What to do at the next boot.
    #[serde(flatten)]
    pub action: RecoveryAction,
    /// Admin `user_id` that scheduled it (audit line in the banner).
    pub requested_by: String,
    /// RFC-3339 stamp of the scheduling.
    pub requested_at: String,
}

/// Outcome of one applied (or refused) recovery, persisted as JSON
/// under [`META_LAST_RECOVERY`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryOutcome {
    /// Human label of the action ([`RecoveryAction::label`]).
    pub action: String,
    /// `true` when the action was applied.
    pub ok: bool,
    /// One-line summary on success, or the refusal reason.
    pub detail: String,
    /// Where the automatic safety snapshot was written, when one was
    /// taken.
    pub safety_snapshot: Option<String>,
    /// RFC-3339 completion stamp.
    pub finished_at: String,
}

/// Memory tables cleared by a reset — everything that holds or points
/// at memory *content*. Deliberately an explicit list (a future table
/// missed here survives a reset — conservative) next to the preserved
/// identity/installation set: `enrollment_users`, `enrollment_groups`,
/// `consumers`, `consumer_delegations`, `user_credentials`,
/// `user_invitations`, `user_2fa`, `user_2fa_recovery_codes`,
/// `pending_2fa`, `password_resets`, `token_blacklist`,
/// `webagentoauth_*`, `engine_meta`, `skills_custom`.
const MEMORY_TABLES: &[&str] = &[
    "fact_index",
    "capture_buffer",
    "wiki_events",
    "wiki_briefing_items",
    "archive_proposals",
    "structure_proposals",
    "structure_proposal_votes",
    "proposal_ops_log",
    "rem_ops_log",
    "wiki_admin_op_log",
    "wiki_admin_leases",
    "dream_runs",
    "compile_failures",
    "recall_traces",
    "recall_log",
    "recall_misses",
    "recent_exchanges",
    "disclosure_audit",
    "document_jobs",
    "document_job_segments",
    "media_catalog",
    "tool_executions",
];

/// Path of the marker inside `workdir`.
fn marker_path(workdir: &Path) -> PathBuf {
    workdir.join(RECOVERY_FILENAME)
}

/// Stage a recovery: write the marker. Refuses when one is already
/// pending (cancel first — two stacked requests have no defined order).
///
/// # Errors
///
/// [`RecoveryError::AlreadyPending`], or the marker's serialization /
/// write surface.
pub fn schedule(workdir: &Path, req: &RecoveryRequest) -> Result<()> {
    if marker_path(workdir).exists() {
        return Err(RecoveryError::AlreadyPending);
    }
    let json = serde_json::to_vec_pretty(req)?;
    atomic_write(&marker_path(workdir), &json)?;
    tracing::info!(
        action = %req.action.label(),
        requested_by = %req.requested_by,
        "recovery: staged (applies at next server start)"
    );
    Ok(())
}

/// The pending request, if any. A malformed marker is a
/// [`RecoveryError::Json`] — [`apply_pending`] consumes it harmlessly,
/// while the dashboard surfaces it to the admin.
///
/// # Errors
///
/// [`RecoveryError::Io`] / [`RecoveryError::Json`].
pub fn pending(workdir: &Path) -> Result<Option<RecoveryRequest>> {
    match std::fs::read_to_string(marker_path(workdir)) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove the marker. `Ok(true)` when one was there.
///
/// # Errors
///
/// [`RecoveryError::Io`] for filesystem failures other than not-found.
pub fn cancel(workdir: &Path) -> Result<bool> {
    match std::fs::remove_file(marker_path(workdir)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Apply a pending recovery, if any. Call at server start, after the
/// single-writer lockfile is acquired and **before** anything opens
/// `engine.db` or the wiki tree.
///
/// `Ok(None)` — no marker. `Ok(Some(outcome))` — the marker was
/// consumed; `outcome.ok` says whether the action was applied or
/// refused-untouched. The outcome is also persisted under
/// [`META_LAST_RECOVERY`] (best-effort).
///
/// # Errors
///
/// Fatal by contract (see [`RecoveryError`]): the boot must abort. The
/// error detail names the safety snapshot when one was taken.
pub async fn apply_pending(
    workdir: &Path,
    snapshots_dir: &Path,
) -> Result<Option<RecoveryOutcome>> {
    let req = match pending(workdir) {
        Ok(None) => return Ok(None),
        Ok(Some(r)) => r,
        Err(RecoveryError::Json(e)) => {
            // One shot even when malformed: drop it, surface, boot on.
            let _ = cancel(workdir);
            let outcome = refusal("unknown recovery", format!("malformed marker removed: {e}"));
            record_outcome(workdir, &outcome).await;
            return Ok(Some(outcome));
        },
        Err(e) => return Err(e),
    };
    // Consume the marker before acting — a failing recovery must never
    // become a boot loop.
    cancel(workdir)?;
    tracing::info!(action = %req.action.label(), "recovery: applying staged request");
    let outcome = match &req.action {
        RecoveryAction::Restore { snapshot } => {
            apply_restore(workdir, snapshots_dir, snapshot).await?
        },
        RecoveryAction::Reset => apply_reset(workdir, snapshots_dir).await?,
    };
    record_outcome(workdir, &outcome).await;
    Ok(Some(outcome))
}

/// Build a refused-untouched outcome (and log it).
fn refusal(action: &str, detail: String) -> RecoveryOutcome {
    tracing::warn!(action, %detail, "recovery: refused — workdir untouched");
    RecoveryOutcome {
        action: action.to_owned(),
        ok: false,
        detail,
        safety_snapshot: None,
        finished_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Take the pre-action safety snapshot. `Ok(None)` when the workdir has
/// no `engine.db` (an empty target — nothing to protect).
async fn take_safety(
    workdir: &Path,
    snapshots_dir: &Path,
    prefix: &str,
) -> std::result::Result<Option<PathBuf>, BackupError> {
    let dest = snapshots_dir.join(backup::snapshot_name(prefix));
    match backup::snapshot_workdir(workdir, &dest).await {
        Ok(_) => Ok(Some(dest)),
        Err(BackupError::NoEngineDb(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn apply_restore(
    workdir: &Path,
    snapshots_dir: &Path,
    snapshot: &Path,
) -> Result<RecoveryOutcome> {
    let action = RecoveryAction::Restore {
        snapshot: snapshot.to_path_buf(),
    }
    .label();
    if !backup::is_snapshot_dir(snapshot) {
        return Ok(refusal(
            &action,
            format!(
                "{} is not a snapshot directory (no engine.db inside) — workdir untouched",
                snapshot.display()
            ),
        ));
    }
    let safety = match take_safety(workdir, snapshots_dir, "pre-restore").await {
        Ok(p) => p,
        Err(e) => {
            return Ok(refusal(
                &action,
                format!("safety snapshot failed ({e}) — workdir untouched"),
            ));
        },
    };
    let safety_hint = safety.as_ref().map_or_else(
        || "no safety snapshot was taken (workdir had no engine.db)".to_owned(),
        |p| format!("the pre-restore safety snapshot is at {}", p.display()),
    );

    // Point of no return: from here a failure is fatal for the boot.
    let fatal = |e: std::io::Error| RecoveryError::Fatal {
        detail: format!("{action} failed mid-apply: {e}; {safety_hint}"),
    };
    clear_workdir(workdir).map_err(fatal)?;
    let files = copy_recursive(snapshot, workdir).map_err(fatal)?;

    tracing::info!(%action, files, "recovery: restore applied");
    Ok(RecoveryOutcome {
        action,
        ok: true,
        detail: format!("{files} files restored"),
        safety_snapshot: safety.map(|p| p.display().to_string()),
        finished_at: chrono::Utc::now().to_rfc3339(),
    })
}

async fn apply_reset(workdir: &Path, snapshots_dir: &Path) -> Result<RecoveryOutcome> {
    let action = RecoveryAction::Reset.label();
    let safety = match take_safety(workdir, snapshots_dir, "pre-reset").await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Ok(refusal(
                &action,
                "workdir has no engine.db — nothing to reset".to_owned(),
            ));
        },
        Err(e) => {
            return Ok(refusal(
                &action,
                format!("safety snapshot failed ({e}) — workdir untouched"),
            ));
        },
    };
    let safety_hint = format!("the pre-reset safety snapshot is at {}", safety.display());

    // DB side, one transaction: an error before commit rolls back and
    // the reset is refused-untouched; after commit a failure is fatal.
    let pool = match crate::db::open_or_init(workdir).await {
        Ok(p) => p,
        Err(e) => {
            return Ok(refusal(
                &action,
                format!("opening engine.db failed ({e}) — workdir untouched"),
            ));
        },
    };
    let db_side = async {
        let mut tx = pool.begin().await?;
        // All-rows deletes across the whole set make FK order
        // irrelevant — defer enforcement to the (consistent) commit.
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *tx)
            .await?;
        for table in MEMORY_TABLES {
            sqlx::query(&format!("DELETE FROM {table}"))
                .execute(&mut *tx)
                .await?;
        }
        // Memory gone ⇒ profiles gone: send every user back through
        // the welcome wizard so their profile re-seeds.
        sqlx::query("UPDATE user_credentials SET profile_initialized = 0")
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    };
    if let Err(e) = db_side.await {
        pool.close().await;
        return Ok(refusal(
            &action,
            format!("clearing the memory tables failed ({e}) — rolled back, workdir untouched"),
        ));
    }

    // File side. A failure here is fatal: the DB is already cleared, and
    // leftover wiki files would be re-indexed straight back into it.
    let fatal = |detail: String| RecoveryError::Fatal {
        detail: format!("{action} failed mid-apply: {detail}; {safety_hint}"),
    };
    for dir in [
        crate::wiki::WIKIS_DIR,
        crate::media::MEDIA_DIR,
        crate::training_spool::TRAINING_SPOOL_DIR,
    ] {
        let path = workdir.join(dir);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|e| fatal(format!("removing {}: {e}", path.display())))?;
        }
    }

    // Re-scaffold the identity wikis of every enrolled principal so the
    // server wakes up with the same roster and empty memory.
    let scaffolded = rescaffold_identity_wikis(workdir, &pool)
        .await
        .map_err(fatal)?;
    pool.close().await;

    tracing::info!(%action, scaffolded, "recovery: memory reset applied");
    Ok(RecoveryOutcome {
        action,
        ok: true,
        detail: format!(
            "memory cleared; {scaffolded} identity wikis re-scaffolded; accounts, consumers, \
             tokens, config, and prompts preserved"
        ),
        safety_snapshot: Some(safety.display().to_string()),
        finished_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Re-create the empty identity-wiki scaffolds for every enrolled
/// user, agent, and group (the roster survives a reset — only the
/// memory goes). Returns the scaffold count; the error is the fatal
/// detail line.
async fn rescaffold_identity_wikis(
    workdir: &Path,
    pool: &sqlx::SqlitePool,
) -> std::result::Result<usize, String> {
    let tree = WikiTree::open(workdir).map_err(|e| format!("reopening the wiki tree: {e}"))?;
    let mut scaffolded = 0usize;
    let users: Vec<(String, i64)> =
        sqlx::query_as("SELECT user_id, is_agent FROM enrollment_users ORDER BY user_id")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("listing enrolled users: {e}"))?;
    for (user_id, is_agent) in users {
        let Ok(wiki_id) = crate::types::WikiId::parse(&user_id) else {
            tracing::warn!(%user_id, "reset: user id is not a valid wiki id — no scaffold");
            continue;
        };
        let kind = if is_agent == 0 {
            IdentityKind::User
        } else {
            IdentityKind::Agent
        };
        create_identity_wiki(&tree, &wiki_id, &user_id, kind)
            .map_err(|e| format!("scaffolding {user_id}: {e}"))?;
        scaffolded += 1;
    }
    let groups: Vec<String> = sqlx::query_scalar(
        "SELECT group_id FROM enrollment_groups WHERE group_id != 'global' ORDER BY group_id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("listing enrolled groups: {e}"))?;
    for group_id in groups {
        let Ok(wiki_id) = crate::types::WikiId::parse(&group_id) else {
            tracing::warn!(%group_id, "reset: group id is not a valid wiki id — no scaffold");
            continue;
        };
        create_identity_wiki(&tree, &wiki_id, &group_id, IdentityKind::Group)
            .map_err(|e| format!("scaffolding {group_id}: {e}"))?;
        scaffolded += 1;
    }
    Ok(scaffolded)
}

/// Remove every top-level workdir entry except operational state that
/// never travels with a snapshot: `logs/`, the live single-writer
/// lockfile, and (belt-and-suspenders — already consumed) the marker.
fn clear_workdir(workdir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(workdir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "logs" || name == crate::lockfile::LOCKFILE_NAME || name == RECOVERY_FILENAME {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Plain recursive copy (no exclusions — a snapshot carries no
/// operational residue by construction). Returns the file count.
fn copy_recursive(src: &Path, dest: &Path) -> std::io::Result<usize> {
    let mut files = 0;
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            files += copy_recursive(&from, &to)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(&from, &to)?;
            files += 1;
        }
    }
    Ok(files)
}

/// Persist the outcome under [`META_LAST_RECOVERY`] — best-effort (the
/// boot right after re-opens and migrates the same DB anyway).
async fn record_outcome(workdir: &Path, outcome: &RecoveryOutcome) {
    let Ok(json) = serde_json::to_string(outcome) else {
        return;
    };
    match crate::db::open_or_init(workdir).await {
        Ok(pool) => {
            if let Err(e) = crate::db::meta_set(&pool, META_LAST_RECOVERY, &json).await {
                tracing::warn!(error = %e, "recovery: outcome not persisted");
            }
            pool.close().await;
        },
        Err(e) => tracing::warn!(error = %e, "recovery: outcome not persisted (db open)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_reset() -> RecoveryRequest {
        RecoveryRequest {
            action: RecoveryAction::Reset,
            requested_by: "admin".to_owned(),
            requested_at: "2026-07-20T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn schedule_pending_cancel_round_trip() {
        let work = tempfile::tempdir().unwrap();
        assert!(pending(work.path()).unwrap().is_none());

        schedule(work.path(), &req_reset()).unwrap();
        let got = pending(work.path()).unwrap().expect("pending");
        assert_eq!(got, req_reset());

        // A second schedule is refused — cancel first.
        let err = schedule(work.path(), &req_reset()).expect_err("must refuse");
        assert!(matches!(err, RecoveryError::AlreadyPending));

        assert!(cancel(work.path()).unwrap());
        assert!(pending(work.path()).unwrap().is_none());
        assert!(!cancel(work.path()).unwrap());
    }

    #[test]
    fn marker_json_shape_is_kebab_tagged() {
        let req = RecoveryRequest {
            action: RecoveryAction::Restore {
                snapshot: PathBuf::from("/snaps/auto-1"),
            },
            requested_by: "admin".to_owned(),
            requested_at: "2026-07-20T00:00:00Z".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"restore\""), "{json}");
        assert!(json.contains("\"snapshot\":\"/snaps/auto-1\""), "{json}");
        let back: RecoveryRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[tokio::test]
    async fn apply_without_marker_is_none() {
        let work = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();
        let out = apply_pending(work.path(), snaps.path()).await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn malformed_marker_is_consumed_not_fatal() {
        let work = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();
        // Give the workdir a DB so the outcome can be recorded.
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        pool.close().await;
        std::fs::write(marker_path(work.path()), "{not json").unwrap();

        let out = apply_pending(work.path(), snaps.path())
            .await
            .unwrap()
            .expect("outcome");
        assert!(!out.ok);
        assert!(out.detail.contains("malformed"), "{}", out.detail);
        assert!(pending(work.path()).unwrap().is_none(), "marker consumed");
    }

    #[tokio::test]
    async fn restore_returns_workdir_to_snapshot_state() {
        let work = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();

        // Seed: a DB with a marker value, a wiki page, an env file.
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        crate::db::meta_set(&pool, "test.signal", "before")
            .await
            .unwrap();
        pool.close().await;
        std::fs::create_dir_all(work.path().join("wikis/alice")).unwrap();
        std::fs::write(work.path().join("wikis/alice/intro.md"), "old prose\n").unwrap();
        std::fs::write(work.path().join("mwe-mcp.env"), "SECRET=1\n").unwrap();
        std::fs::create_dir_all(work.path().join("logs")).unwrap();
        std::fs::write(work.path().join("logs/server.log"), "keep me\n").unwrap();

        // Snapshot, then diverge: mutate the DB, a page, add a file.
        let snap = snaps.path().join("manual-1");
        backup::snapshot_workdir(work.path(), &snap).await.unwrap();
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        crate::db::meta_set(&pool, "test.signal", "after")
            .await
            .unwrap();
        pool.close().await;
        std::fs::write(work.path().join("wikis/alice/intro.md"), "new prose\n").unwrap();
        std::fs::write(work.path().join("wikis/alice/stray.md"), "added later\n").unwrap();

        schedule(
            work.path(),
            &RecoveryRequest {
                action: RecoveryAction::Restore {
                    snapshot: snap.clone(),
                },
                requested_by: "admin".to_owned(),
                requested_at: "2026-07-20T00:00:00Z".to_owned(),
            },
        )
        .unwrap();
        let out = apply_pending(work.path(), snaps.path())
            .await
            .unwrap()
            .expect("outcome");
        assert!(out.ok, "{}", out.detail);
        assert!(pending(work.path()).unwrap().is_none(), "marker consumed");

        // Files back to snapshot state; post-snapshot additions gone.
        let prose = std::fs::read_to_string(work.path().join("wikis/alice/intro.md")).unwrap();
        assert_eq!(prose, "old prose\n");
        assert!(!work.path().join("wikis/alice/stray.md").exists());
        assert!(work.path().join("mwe-mcp.env").is_file());
        // Operational state survived the swap.
        assert!(work.path().join("logs/server.log").is_file());

        // DB back to snapshot state (modulo the recorded outcome).
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        let signal = crate::db::meta_get(&pool, "test.signal").await.unwrap();
        assert_eq!(signal.as_deref(), Some("before"));
        let last = crate::db::meta_get(&pool, META_LAST_RECOVERY)
            .await
            .unwrap()
            .expect("outcome recorded");
        let last: RecoveryOutcome = serde_json::from_str(&last).unwrap();
        assert!(last.ok);
        pool.close().await;

        // The automatic safety snapshot exists and captured the
        // diverged ("after") state.
        let listed = backup::list_snapshots(snaps.path()).unwrap();
        let safety = listed
            .iter()
            .find(|s| s.kind == backup::SnapshotKind::Safety)
            .expect("safety snapshot");
        assert!(safety.name.starts_with("pre-restore-"), "{}", safety.name);
        assert_eq!(
            std::fs::read_to_string(safety.path.join("wikis/alice/intro.md")).unwrap(),
            "new prose\n"
        );
    }

    #[tokio::test]
    async fn restore_refuses_non_snapshot_dir_untouched() {
        let work = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        pool.close().await;
        std::fs::write(work.path().join("mwe-mcp.env"), "SECRET=1\n").unwrap();
        let not_a_snap = snaps.path().join("empty");
        std::fs::create_dir_all(&not_a_snap).unwrap();

        schedule(
            work.path(),
            &RecoveryRequest {
                action: RecoveryAction::Restore {
                    snapshot: not_a_snap,
                },
                requested_by: "admin".to_owned(),
                requested_at: "2026-07-20T00:00:00Z".to_owned(),
            },
        )
        .unwrap();
        let out = apply_pending(work.path(), snaps.path())
            .await
            .unwrap()
            .expect("outcome");
        assert!(!out.ok);
        assert!(out.detail.contains("not a snapshot"), "{}", out.detail);
        assert!(
            work.path().join("mwe-mcp.env").is_file(),
            "workdir untouched"
        );
    }

    /// Seed a workdir for the reset test: enrollment (user + agent +
    /// group), a credential with an initialized profile, one fact,
    /// files on every memory surface, and preserved surfaces
    /// (prompts/, env).
    async fn seed_reset_workdir(work: &Path) {
        let pool = crate::db::open_or_init(work).await.unwrap();
        for (uid, is_agent) in [("alice", 0), ("hermesbot", 1)] {
            sqlx::query(
                "INSERT INTO enrollment_users (user_id, aliases, is_admin, is_agent)
                 VALUES (?, '[]', 0, ?)",
            )
            .bind(uid)
            .bind(is_agent)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO enrollment_groups (group_id, members) VALUES ('famiglia', '[\"alice\"]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO user_credentials (user_id, password_hash, hashed_at, profile_initialized)
             VALUES ('alice', 'phc', '2026-01-01', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
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
        crate::fact_index::insert(&pool, &fact).await.unwrap();
        pool.close().await;
        std::fs::create_dir_all(work.join("wikis/alice")).unwrap();
        std::fs::write(work.join("wikis/alice/intro.md"), "prose\n").unwrap();
        std::fs::create_dir_all(work.join("media")).unwrap();
        std::fs::write(work.join("media/blob"), "img").unwrap();
        std::fs::create_dir_all(work.join("training-spool")).unwrap();
        std::fs::write(work.join("training-spool/pair.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(work.join("prompts")).unwrap();
        std::fs::write(work.join("prompts/ingest.md"), "custom\n").unwrap();
        std::fs::write(work.join("mwe-mcp.env"), "SECRET=1\n").unwrap();
    }

    #[tokio::test]
    async fn reset_clears_memory_and_preserves_identity() {
        let work = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();
        seed_reset_workdir(work.path()).await;

        schedule(work.path(), &req_reset()).unwrap();
        let out = apply_pending(work.path(), snaps.path())
            .await
            .unwrap()
            .expect("outcome");
        assert!(out.ok, "{}", out.detail);

        // Memory gone.
        let pool = crate::db::open_or_init(work.path()).await.unwrap();
        let facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fact_index")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(facts, 0);
        assert!(!work.path().join("media").exists());
        assert!(!work.path().join("training-spool").exists());
        assert!(!work.path().join("wikis/alice/intro.md").exists());

        // Identity preserved; profile flag cleared.
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM enrollment_users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 2);
        let initialized: i64 = sqlx::query_scalar(
            "SELECT profile_initialized FROM user_credentials WHERE user_id = 'alice'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(initialized, 0, "welcome wizard re-armed");
        pool.close().await;

        // Identity wikis re-scaffolded for user, agent, and group;
        // preserved surfaces intact.
        for id in ["alice", "hermesbot", "famiglia"] {
            assert!(
                work.path().join(format!("wikis/{id}/_meta.md")).is_file(),
                "{id} scaffolded"
            );
        }
        assert_eq!(
            std::fs::read_to_string(work.path().join("prompts/ingest.md")).unwrap(),
            "custom\n"
        );
        assert!(work.path().join("mwe-mcp.env").is_file());

        // Safety snapshot captured the pre-reset state.
        let listed = backup::list_snapshots(snaps.path()).unwrap();
        let safety = listed
            .iter()
            .find(|s| s.name.starts_with("pre-reset-"))
            .expect("safety snapshot");
        assert_eq!(
            std::fs::read_to_string(safety.path.join("wikis/alice/intro.md")).unwrap(),
            "prose\n"
        );
    }
}
