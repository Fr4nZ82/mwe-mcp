// SPDX-License-Identifier: AGPL-3.0-or-later
//! Applicative WAL for structure-proposal apply and REM nightly cycles
//! (see applicative WAL).
//!
//! ## Why an *applicative* `WAL` on top of `SQLite` WAL
//!
//! `SQLite`'s own WAL keeps the database file consistent across crashes,
//! but a `structure_proposal_apply` (or a REM cycle) is not a single
//! transaction: it interleaves filesystem writes (`.tmp` + rename
//! dances), DB updates, marker propagation, and cross-link rewrites.
//! No single SQL `COMMIT` can roll those back together.
//!
//! The applicative WAL is the protocol on top: before each step we
//! insert a row in `proposal_ops_log` (or `rem_ops_log`) with
//! `status=pending`, perform the step, then flip to `done`. On startup
//! we look for rows that never reached `done` (typically because the
//! process crashed mid-step) and hand them to a rollback driver — the
//! driver is supplied by the caller because the inverse of each step
//! kind is step-kind specific (e.g. restore from `_snapshots/proposal/`,
//! delete a half-applied DB row).
//!
//! ## Scope
//!
//! This module ships the **journaling primitives**: lifecycle helpers
//! (`begin` → `complete`/`fail`) and the recovery scan that returns
//! every stale row older than a configurable cutoff. The per-step-kind
//! rollback logic lives next to the step it inverts — in
//! `mwe-core::wiki` for filesystem writes, `mwe-core::rem` for REM ops,
//! etc. — alongside the corresponding step implementations.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Default staleness window before an in-flight op is considered
/// abandoned by the recovery scan (`started_at < now() - 5min`).
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Lifecycle status of a journaled op.
///
/// Stored as `TEXT` in `SQLite` for human inspectability — the cost of
/// one `VARCHAR` per row is negligible next to the operational benefit
/// of being able to `SELECT … FROM proposal_ops_log` and read what
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// Row inserted, step has not started executing yet. Visible to the
    /// recovery scan after the staleness window.
    Pending,
    /// Step is actively executing. Visible to the recovery scan after
    /// the staleness window.
    InProgress,
    /// Step completed successfully. Invisible to the recovery scan.
    Done,
    /// Step failed (either at runtime or via the recovery scan flipping
    /// a stale row). Invisible to the recovery scan; kept for audit.
    Failed,
}

impl OpStatus {
    /// Stable lowercase string (matches what we write to TEXT columns
    /// and what migrations document).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Parse a status string from the DB; rows with an unrecognized
    /// status are treated as `Failed` upstream by the recovery code, so
    /// the parse function is fallible for the caller to handle that.
    pub fn parse(s: &str) -> Result<Self, WalError> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(WalError::UnknownStatus(other.to_owned())),
        }
    }
}

/// Errors specific to the WAL layer. We keep them out of the global
/// [`crate::Error`] enum so the recovery driver can match exhaustively
/// on the failure modes without dragging in IO/JSON/etc.
#[derive(Debug, Error)]
pub enum WalError {
    /// Underlying DB error (sqlx surface).
    #[error("wal db error: {0}")]
    Db(#[from] sqlx::Error),
    /// A row stored an unrecognized status string (schema drift or
    /// manual DB edit). Recovery treats this as `Failed`.
    #[error("unknown wal status: {0}")]
    UnknownStatus(String),
}

/// `SELECT *` tuple shape for `proposal_ops_log`, used by `query_as`
/// in the recovery scan. Hoisted out of the call site to satisfy
/// clippy's type-complexity lint and to share between rust-analyzer
/// and humans without re-counting commas.
type ProposalOpTuple = (
    i64,            // op_id
    String,         // proposal_id
    i64,            // step_idx
    String,         // kind
    Option<String>, // payload_json
    String,         // status
    String,         // started_at
    Option<String>, // completed_at
    Option<String>, // error_msg
);

/// Same shape for `rem_ops_log`.
type RemOpTuple = (
    i64,            // op_id
    String,         // cycle_id
    String,         // operation_kind
    Option<String>, // target_wiki_id
    Option<String>, // snapshot_path
    String,         // status
    String,         // started_at
    Option<String>, // completed_at
    Option<String>, // error_msg
);

/// A row from `proposal_ops_log`. Mirrors the table 1:1; `kind` and
/// `payload_json` stay opaque at this layer (interpreted by the
/// per-kind rollback driver).
#[derive(Debug, Clone)]
pub struct ProposalOpRow {
    /// Auto-increment primary key — server-generated.
    pub op_id: i64,
    /// Logical proposal this op belongs to.
    pub proposal_id: String,
    /// 0-based ordinal of the step within the proposal.
    pub step_idx: i64,
    /// Step-kind identifier (`"file_write"`, `"db_update"`, …).
    pub kind: String,
    /// JSON payload (opaque at this layer).
    pub payload_json: Option<String>,
    /// Current lifecycle status.
    pub status: OpStatus,
    /// ISO 8601 timestamp when the row was inserted.
    pub started_at: String,
    /// ISO 8601 timestamp when the row reached a terminal status.
    pub completed_at: Option<String>,
    /// Error class / message stored on `Failed`.
    pub error_msg: Option<String>,
}

/// A row from `rem_ops_log`. Same shape and conventions as
/// [`ProposalOpRow`], scoped to REM nightly cycles.
#[derive(Debug, Clone)]
pub struct RemOpRow {
    /// Auto-increment primary key.
    pub op_id: i64,
    /// Logical REM cycle this op belongs to.
    pub cycle_id: String,
    /// Operation kind ("promotion", "revalidate", …).
    pub operation_kind: String,
    /// Wiki targeted by the op (None for global ops).
    pub target_wiki_id: Option<String>,
    /// Path of the pre-op snapshot, relative to workdir.
    pub snapshot_path: Option<String>,
    /// Current lifecycle status.
    pub status: OpStatus,
    /// ISO 8601 timestamp when the row was inserted.
    pub started_at: String,
    /// ISO 8601 timestamp when the row reached a terminal status.
    pub completed_at: Option<String>,
    /// Error class / message stored on `Failed`.
    pub error_msg: Option<String>,
}

/// Insert a new `proposal_ops_log` row in `Pending` status.
///
/// Returns the generated `op_id`. The caller will flip it through
/// [`mark_in_progress`] → [`complete_proposal_op`] /
/// [`fail_proposal_op`] as the step runs.
pub async fn begin_proposal_op(
    pool: &SqlitePool,
    proposal_id: &str,
    step_idx: i64,
    kind: &str,
    payload_json: Option<&str>,
) -> Result<i64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO proposal_ops_log
             (proposal_id, step_idx, kind, payload_json, status, started_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING op_id",
    )
    .bind(proposal_id)
    .bind(step_idx)
    .bind(kind)
    .bind(payload_json)
    .bind(OpStatus::Pending.as_str())
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Flip a proposal op from `Pending` to `InProgress`. Returns the
/// number of rows updated (0 if the row was already terminal — caller
/// can treat that as a no-op).
pub async fn mark_in_progress(pool: &SqlitePool, op_id: i64) -> Result<u64, WalError> {
    let res = sqlx::query(
        "UPDATE proposal_ops_log
            SET status = ?
          WHERE op_id = ? AND status = ?",
    )
    .bind(OpStatus::InProgress.as_str())
    .bind(op_id)
    .bind(OpStatus::Pending.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Mark a proposal op as `Done` and stamp `completed_at`.
pub async fn complete_proposal_op(pool: &SqlitePool, op_id: i64) -> Result<u64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE proposal_ops_log
            SET status = ?, completed_at = ?
          WHERE op_id = ?",
    )
    .bind(OpStatus::Done.as_str())
    .bind(now)
    .bind(op_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Mark a proposal op as `Failed`, stamping `completed_at` and the
/// error class.
///
/// Used both by the live step (when its SQL/IO surfaces an error) and
/// by the recovery scan (when it gives up on a stale row).
pub async fn fail_proposal_op(
    pool: &SqlitePool,
    op_id: i64,
    reason: &str,
) -> Result<u64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE proposal_ops_log
            SET status = ?, completed_at = ?, error_msg = ?
          WHERE op_id = ?",
    )
    .bind(OpStatus::Failed.as_str())
    .bind(now)
    .bind(reason)
    .bind(op_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Recovery scan over `proposal_ops_log`.
///
/// Returns every row whose `status IN ('pending', 'in_progress')` and
/// whose `started_at < now - older_than`. The expected caller is the
/// startup path of `mwe-mcp serve`, which then drives the per-kind
/// rollback and ultimately calls [`fail_proposal_op`] on each
/// returned row so the cycle is closed.
pub async fn scan_stale_proposal_ops(
    pool: &SqlitePool,
    older_than: Duration,
) -> Result<Vec<ProposalOpRow>, WalError> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::from_std(older_than).unwrap_or_default())
        .to_rfc3339();

    let rows: Vec<ProposalOpTuple> = sqlx::query_as(
        "SELECT op_id, proposal_id, step_idx, kind, payload_json,
                status, started_at, completed_at, error_msg
           FROM proposal_ops_log
          WHERE status IN ('pending', 'in_progress')
            AND started_at < ?
          ORDER BY proposal_id, step_idx DESC",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(ProposalOpRow {
                op_id: r.0,
                proposal_id: r.1,
                step_idx: r.2,
                kind: r.3,
                payload_json: r.4,
                status: OpStatus::parse(&r.5)?,
                started_at: r.6,
                completed_at: r.7,
                error_msg: r.8,
            })
        })
        .collect()
}

/// Insert a new `rem_ops_log` row in `Pending` status. Mirror of
/// [`begin_proposal_op`] for REM cycles.
pub async fn begin_rem_op(
    pool: &SqlitePool,
    cycle_id: &str,
    operation_kind: &str,
    target_wiki_id: Option<&str>,
    snapshot_path: Option<&str>,
) -> Result<i64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO rem_ops_log
             (cycle_id, operation_kind, target_wiki_id, snapshot_path, status, started_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING op_id",
    )
    .bind(cycle_id)
    .bind(operation_kind)
    .bind(target_wiki_id)
    .bind(snapshot_path)
    .bind(OpStatus::Pending.as_str())
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Mark a REM op as `Done`. See [`complete_proposal_op`] for the
/// equivalent on the proposal side.
pub async fn complete_rem_op(pool: &SqlitePool, op_id: i64) -> Result<u64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE rem_ops_log
            SET status = ?, completed_at = ?
          WHERE op_id = ?",
    )
    .bind(OpStatus::Done.as_str())
    .bind(now)
    .bind(op_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Mark a REM op as `Failed`. See [`fail_proposal_op`] for the
/// equivalent on the proposal side.
pub async fn fail_rem_op(pool: &SqlitePool, op_id: i64, reason: &str) -> Result<u64, WalError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE rem_ops_log
            SET status = ?, completed_at = ?, error_msg = ?
          WHERE op_id = ?",
    )
    .bind(OpStatus::Failed.as_str())
    .bind(now)
    .bind(reason)
    .bind(op_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Recovery scan over `rem_ops_log`. See [`scan_stale_proposal_ops`]
/// for behavior and semantics.
pub async fn scan_stale_rem_ops(
    pool: &SqlitePool,
    older_than: Duration,
) -> Result<Vec<RemOpRow>, WalError> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::from_std(older_than).unwrap_or_default())
        .to_rfc3339();

    let rows: Vec<RemOpTuple> = sqlx::query_as(
        "SELECT op_id, cycle_id, operation_kind, target_wiki_id, snapshot_path,
                status, started_at, completed_at, error_msg
           FROM rem_ops_log
          WHERE status IN ('pending', 'in_progress')
            AND started_at < ?
          ORDER BY cycle_id, op_id",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(RemOpRow {
                op_id: r.0,
                cycle_id: r.1,
                operation_kind: r.2,
                target_wiki_id: r.3,
                snapshot_path: r.4,
                status: OpStatus::parse(&r.5)?,
                started_at: r.6,
                completed_at: r.7,
                error_msg: r.8,
            })
        })
        .collect()
}

// ---------- Generic apply driver ----------

/// Pluggable step-kind inverse used by the recovery driver.
///
/// [`Self::invert`] is called once per stale row before the driver
/// flips the row to `Failed`. Returning `Err` does **not** stop the
/// scan — the driver records the error class in the row's `error_msg`
/// and moves to the next row. The closure-shaped variant
/// [`NoopInverse`] is what every caller uses today, because both
/// ops sources (structure-proposal apply and REM) ship only idempotent
/// sub-steps in this phase.
#[allow(clippy::module_name_repetitions)]
pub trait OpInverse: Send + Sync {
    /// Best-effort reversal for `kind` with `payload_json`. The driver
    /// guarantees the row is still in `pending` / `in_progress` at the
    /// time of the call; the implementation must not assume anything
    /// about whether the original step had already touched disk or DB.
    /// Returning `Err` lets the driver record the reason in
    /// `error_msg` and proceed; returning `Ok` is treated as "inverted
    /// successfully" and the row is flipped to `Failed` with a generic
    /// `rolled_back_by_startup` reason.
    ///
    /// # Errors
    ///
    /// Inverse implementations choose their own failure modes; the
    /// driver maps them to a string via [`std::fmt::Display`].
    fn invert(
        &self,
        kind: &str,
        payload_json: Option<&str>,
        snapshot_path: Option<&str>,
    ) -> Result<(), String>;
}

/// Inverse that does nothing — the no-op placeholder.
///
/// Used when the caller's ops are all idempotent and a future re-run
/// will simply re-do them. This is the baseline floor: REM cycles are
/// restartable and structure-proposal apply is stubbed.
pub struct NoopInverse;

impl OpInverse for NoopInverse {
    fn invert(
        &self,
        _kind: &str,
        _payload_json: Option<&str>,
        _snapshot_path: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// Outcome summary of a recovery sweep.
#[derive(Debug, Clone, Default)]
pub struct RollbackReport {
    /// How many rows were rolled back successfully (inverse returned
    /// `Ok` and the row was flipped to `Failed`).
    pub rolled_back: usize,
    /// How many rows could not be rolled back (inverse returned `Err`
    /// or the SQL flip itself failed). The row is still flipped to
    /// `Failed` with the error class recorded in `error_msg`.
    pub failed_rollbacks: usize,
}

/// Scan stale `proposal_ops_log` rows and run `inverse.invert` on each
/// before flipping it to `Failed`. Returns a [`RollbackReport`] summary.
///
/// Recovery semantics:
/// - Inverse `Ok` ⇒ `Failed` with `reason = "rolled_back_by_startup"`.
/// - Inverse `Err(reason)` ⇒ `Failed` with `reason = "rollback_failed: <reason>"`.
/// - SQL flip failure is logged + counted as a failed rollback; the
///   scan does not abort.
///
/// # Errors
///
/// - [`WalError::Db`] only when the initial `scan_stale_proposal_ops`
///   call fails. Per-row SQL failures are absorbed into the report.
pub async fn rollback_stale_proposals(
    pool: &SqlitePool,
    older_than: Duration,
    inverse: &dyn OpInverse,
) -> Result<RollbackReport, WalError> {
    let rows = scan_stale_proposal_ops(pool, older_than).await?;
    let mut report = RollbackReport::default();
    for row in rows {
        let payload = row.payload_json.as_deref();
        let reason = match inverse.invert(&row.kind, payload, None) {
            Ok(()) => {
                report.rolled_back += 1;
                "rolled_back_by_startup".to_owned()
            },
            Err(detail) => {
                report.failed_rollbacks += 1;
                format!("rollback_failed: {detail}")
            },
        };
        if let Err(e) = fail_proposal_op(pool, row.op_id, &reason).await {
            tracing::warn!(
                op_id = row.op_id,
                proposal_id = %row.proposal_id,
                err = %e,
                "wal recovery: SQL flip failed",
            );
            if report.rolled_back > 0 {
                report.rolled_back -= 1;
            }
            report.failed_rollbacks += 1;
        }
    }
    tracing::info!(
        rolled_back = report.rolled_back,
        failed_rollbacks = report.failed_rollbacks,
        "wal recovery: proposal ops swept",
    );
    Ok(report)
}

/// Mirror of [`rollback_stale_proposals`] for REM ops.
///
/// # Errors
///
/// - [`WalError::Db`] only when the initial `scan_stale_rem_ops`
///   call fails. Per-row SQL failures are absorbed into the report.
pub async fn rollback_stale_rems(
    pool: &SqlitePool,
    older_than: Duration,
    inverse: &dyn OpInverse,
) -> Result<RollbackReport, WalError> {
    let rows = scan_stale_rem_ops(pool, older_than).await?;
    let mut report = RollbackReport::default();
    for row in rows {
        let reason = match inverse.invert(
            &row.operation_kind,
            None, // REM rows carry snapshot_path, not payload_json
            row.snapshot_path.as_deref(),
        ) {
            Ok(()) => {
                report.rolled_back += 1;
                "rolled_back_by_startup".to_owned()
            },
            Err(detail) => {
                report.failed_rollbacks += 1;
                format!("rollback_failed: {detail}")
            },
        };
        if let Err(e) = fail_rem_op(pool, row.op_id, &reason).await {
            tracing::warn!(
                op_id = row.op_id,
                cycle_id = %row.cycle_id,
                err = %e,
                "wal recovery: SQL flip failed",
            );
            if report.rolled_back > 0 {
                report.rolled_back -= 1;
            }
            report.failed_rollbacks += 1;
        }
    }
    tracing::info!(
        rolled_back = report.rolled_back,
        failed_rollbacks = report.failed_rollbacks,
        "wal recovery: rem ops swept",
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_pool() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    #[test]
    fn status_roundtrip() {
        for s in [
            OpStatus::Pending,
            OpStatus::InProgress,
            OpStatus::Done,
            OpStatus::Failed,
        ] {
            assert_eq!(OpStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(matches!(
            OpStatus::parse("bogus"),
            Err(WalError::UnknownStatus(_))
        ));
    }

    /// Full proposal op lifecycle: begin → `in_progress` → complete.
    /// Verifies the row is left in `Done` with a `completed_at`.
    #[tokio::test]
    async fn proposal_op_happy_path() {
        let (_workdir, pool) = fresh_pool().await;
        let op_id = begin_proposal_op(&pool, "prop-1", 0, "file_write", Some(r#"{"path":"x"}"#))
            .await
            .expect("begin");
        assert_eq!(
            mark_in_progress(&pool, op_id).await.expect("in_progress"),
            1
        );
        assert_eq!(complete_proposal_op(&pool, op_id).await.expect("done"), 1);

        let (status, completed_at): (String, Option<String>) =
            sqlx::query_as("SELECT status, completed_at FROM proposal_ops_log WHERE op_id = ?")
                .bind(op_id)
                .fetch_one(&pool)
                .await
                .expect("fetch");
        assert_eq!(status, "done");
        assert!(completed_at.is_some(), "completed_at must be stamped");
    }

    /// Stale rows surface from the recovery scan; fresh rows do not.
    /// Forces `started_at` backwards via an UPDATE because the
    /// real-world cutoff is 5 minutes and we do not want to sleep.
    #[tokio::test]
    async fn scan_picks_up_only_stale_rows() {
        let (_workdir, pool) = fresh_pool().await;
        let stale = begin_proposal_op(&pool, "p-stale", 0, "file_write", None)
            .await
            .unwrap();
        let _fresh = begin_proposal_op(&pool, "p-fresh", 0, "db_update", None)
            .await
            .unwrap();

        // Backdate the stale row by 1 hour.
        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE proposal_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(stale)
            .execute(&pool)
            .await
            .expect("backdate");

        let rows = scan_stale_proposal_ops(&pool, Duration::from_secs(60))
            .await
            .expect("scan");
        assert_eq!(rows.len(), 1, "only the backdated row is stale");
        assert_eq!(rows[0].op_id, stale);
        assert_eq!(rows[0].proposal_id, "p-stale");
        assert_eq!(rows[0].status, OpStatus::Pending);
    }

    /// `fail_proposal_op` stamps both `completed_at` and `error_msg`,
    /// and the row stops being visible to the recovery scan afterwards.
    #[tokio::test]
    async fn fail_records_reason_and_clears_from_scan() {
        let (_workdir, pool) = fresh_pool().await;
        let op_id = begin_proposal_op(&pool, "p", 0, "file_write", None)
            .await
            .unwrap();

        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE proposal_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(op_id)
            .execute(&pool)
            .await
            .expect("backdate");

        fail_proposal_op(&pool, op_id, "crash_during_apply")
            .await
            .expect("fail");

        let (status, err_msg): (String, Option<String>) =
            sqlx::query_as("SELECT status, error_msg FROM proposal_ops_log WHERE op_id = ?")
                .bind(op_id)
                .fetch_one(&pool)
                .await
                .expect("fetch");
        assert_eq!(status, "failed");
        assert_eq!(err_msg.as_deref(), Some("crash_during_apply"));

        let rows = scan_stale_proposal_ops(&pool, Duration::from_secs(60))
            .await
            .expect("scan");
        assert!(rows.is_empty(), "failed rows must not be returned");
    }

    /// Same happy-path coverage for REM ops, since the two pipelines
    /// share the same protocol and we want a regression net for both.
    #[tokio::test]
    async fn rem_op_happy_path() {
        let (_workdir, pool) = fresh_pool().await;
        let op_id = begin_rem_op(&pool, "cycle-1", "promotion", Some("alice"), None)
            .await
            .expect("begin");
        assert_eq!(complete_rem_op(&pool, op_id).await.expect("done"), 1);

        let rows = scan_stale_rem_ops(&pool, Duration::from_secs(0))
            .await
            .expect("scan");
        assert!(rows.is_empty(), "completed rows must not be returned");
    }

    /// Default `NoopInverse` is enough for the baseline floor (REM cycle
    /// idempotency + stubbed proposal apply): every stale row is
    /// flipped to `Failed` with the canned reason.
    #[tokio::test]
    async fn rollback_stale_proposals_with_noop_flips_to_failed() {
        let (_workdir, pool) = fresh_pool().await;
        let stale = begin_proposal_op(&pool, "p-stale", 0, "file_write", None)
            .await
            .unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE proposal_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(stale)
            .execute(&pool)
            .await
            .unwrap();

        let report = rollback_stale_proposals(&pool, Duration::from_secs(60), &NoopInverse)
            .await
            .unwrap();
        assert_eq!(report.rolled_back, 1);
        assert_eq!(report.failed_rollbacks, 0);

        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT status, error_msg FROM proposal_ops_log WHERE op_id = ?")
                .bind(stale)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("rolled_back_by_startup"));
    }

    /// An inverse that returns `Err` records the reason in `error_msg`
    /// without bailing out of the sweep.
    #[tokio::test]
    async fn rollback_stale_proposals_records_inverse_failures() {
        struct AlwaysFails;
        impl OpInverse for AlwaysFails {
            fn invert(
                &self,
                _kind: &str,
                _payload_json: Option<&str>,
                _snapshot_path: Option<&str>,
            ) -> Result<(), String> {
                Err("snapshot vanished".into())
            }
        }

        let (_workdir, pool) = fresh_pool().await;
        let stale = begin_proposal_op(&pool, "p-stale", 0, "file_write", None)
            .await
            .unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE proposal_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(stale)
            .execute(&pool)
            .await
            .unwrap();

        let report = rollback_stale_proposals(&pool, Duration::from_secs(60), &AlwaysFails)
            .await
            .unwrap();
        assert_eq!(report.rolled_back, 0);
        assert_eq!(report.failed_rollbacks, 1);
        let reason: Option<String> =
            sqlx::query_scalar("SELECT error_msg FROM proposal_ops_log WHERE op_id = ?")
                .bind(stale)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            reason
                .unwrap_or_default()
                .starts_with("rollback_failed: snapshot vanished"),
            "reason must carry the inverse failure detail",
        );
    }

    /// Same coverage for the REM side.
    #[tokio::test]
    async fn rollback_stale_rems_with_noop_flips_to_failed() {
        let (_workdir, pool) = fresh_pool().await;
        let stale = begin_rem_op(&pool, "c1", "promotion", Some("alice"), None)
            .await
            .unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE rem_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(stale)
            .execute(&pool)
            .await
            .unwrap();

        let report = rollback_stale_rems(&pool, Duration::from_secs(60), &NoopInverse)
            .await
            .unwrap();
        assert_eq!(report.rolled_back, 1);
        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT status, error_msg FROM rem_ops_log WHERE op_id = ?")
                .bind(stale)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("rolled_back_by_startup"));
    }

    #[tokio::test]
    async fn rem_scan_returns_target_wiki_and_snapshot_path() {
        let (_workdir, pool) = fresh_pool().await;
        let op_id = begin_rem_op(
            &pool,
            "cycle-2",
            "archive_proposal",
            Some("frodo"),
            Some("_snapshots/rem/cycle-2/1/"),
        )
        .await
        .expect("begin");

        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE rem_ops_log SET started_at = ? WHERE op_id = ?")
            .bind(&old)
            .bind(op_id)
            .execute(&pool)
            .await
            .expect("backdate");

        let rows = scan_stale_rem_ops(&pool, Duration::from_secs(60))
            .await
            .expect("scan");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target_wiki_id.as_deref(), Some("frodo"));
        assert_eq!(
            rows[0].snapshot_path.as_deref(),
            Some("_snapshots/rem/cycle-2/1/")
        );
    }
}
