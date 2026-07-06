// SPDX-License-Identifier: AGPL-3.0-or-later
//! REM-emitted archival proposals (`archive_proposals` table — see
//! engine DB and migrations).
//!
//! Distinct from the `structure_proposals` flow (which carries forge /
//! promote / `dedup_merge` / bundle): archive proposals target whole
//! filesystem paths and answer the question "should we move this stale
//! corner of the wiki to `_archive/`". The decision is binary
//! (approve / reject) with an optional `selection` JSON for
//! partial-approval, and the apply step is just `wiki_forget` on the
//! selected facts plus a filesystem move — no LLM round-trip.
//!
//! This module ships the **emitter** only. The approval flow
//! and the apply step land with the dashboard archive view and a
//! dedicated reaper milestone.

use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

/// Errors raised by the archive emitter.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Underlying SQL failure.
    #[error("archive db: {0}")]
    Db(#[from] sqlx::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ArchiveError>;

/// Canonical reason strings stored in `archive_proposals.reason`.
pub mod reason {
    /// No `wiki_search` / `wiki_recall` hit in 365 days — REM weekly
    /// candidate, lowest-confidence archival signal.
    pub const NO_RECALL_HIT_365D: &str = "no_recall_hit_365d";
    /// No filesystem modification in 180 days. Reserved for a future
    /// sub-job that mixes recall and modification recency.
    pub const NO_MODIFY_180D: &str = "no_modify_180d";
}

/// Emit one `archive_proposals` row.
///
/// Returns the freshly minted `proposal_id`. The chassis is much
/// thinner than `structure_proposals` — no questionnaire, no
/// `applied_at`, no revert token — so the emitter is also thinner.
///
/// # Errors
///
/// - [`ArchiveError::Db`] for SQL failures.
pub async fn emit_archive_proposal(
    pool: &SqlitePool,
    wiki_id: &str,
    path: &str,
    reason_str: &str,
) -> Result<String> {
    let proposal_id = format!("ap-{}", Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO archive_proposals \
         (proposal_id, wiki_id, path, reason, proposed_at, status) \
         VALUES (?, ?, ?, ?, ?, 'pending')",
    )
    .bind(&proposal_id)
    .bind(wiki_id)
    .bind(path)
    .bind(reason_str)
    .bind(&now)
    .execute(pool)
    .await?;
    tracing::info!(
        proposal_id,
        wiki_id,
        path,
        reason = reason_str,
        "archive: proposal emitted"
    );
    Ok(proposal_id)
}

/// Coarse dedup: skip if an active (`pending` or `approved`) archive
/// proposal already exists for the same `(wiki_id, path)` pair.
///
/// # Errors
///
/// - [`ArchiveError::Db`] for SQL failures.
pub async fn already_proposed(pool: &SqlitePool, wiki_id: &str, path: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM archive_proposals \
         WHERE wiki_id = ? AND path = ? AND status IN ('pending', 'approved')",
    )
    .bind(wiki_id)
    .bind(path)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn fresh_pool() -> SqlitePool {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        db::open_or_init(dir.path()).await.expect("db open")
    }

    #[tokio::test]
    async fn emit_writes_pending_row() {
        let pool = fresh_pool().await;
        let pid = emit_archive_proposal(&pool, "alice", "alice/old.md", reason::NO_RECALL_HIT_365D)
            .await
            .unwrap();
        assert!(pid.starts_with("ap-"));
        let (wiki_id, path, reason_col, status): (String, String, String, String) = sqlx::query_as(
            "SELECT wiki_id, path, reason, status FROM archive_proposals WHERE proposal_id = ?",
        )
        .bind(&pid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(wiki_id, "alice");
        assert_eq!(path, "alice/old.md");
        assert_eq!(reason_col, "no_recall_hit_365d");
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn already_proposed_finds_pending_and_approved() {
        let pool = fresh_pool().await;
        let pid = emit_archive_proposal(&pool, "alice", "alice/old.md", reason::NO_RECALL_HIT_365D)
            .await
            .unwrap();
        assert!(
            already_proposed(&pool, "alice", "alice/old.md")
                .await
                .unwrap()
        );
        // Flip to rejected — should no longer match.
        sqlx::query("UPDATE archive_proposals SET status = 'rejected' WHERE proposal_id = ?")
            .bind(&pid)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !already_proposed(&pool, "alice", "alice/old.md")
                .await
                .unwrap()
        );
        // Approved still counts as active for dedup purposes.
        sqlx::query("UPDATE archive_proposals SET status = 'approved' WHERE proposal_id = ?")
            .bind(&pid)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            already_proposed(&pool, "alice", "alice/old.md")
                .await
                .unwrap()
        );
    }
}
