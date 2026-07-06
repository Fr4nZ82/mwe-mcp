// SPDX-License-Identifier: AGPL-3.0-or-later
//! `disclosure_audit` — append-only log of per-fact ACL changes made from
//! the consumer chat.
//!
//! The owner can broaden or narrow who reads their OWN fact straight from a
//! conversation via the `acl_changes` ingest verb (ingest
//! pipeline). Because that
//! change is applied act-first, every change leaves an immutable row here:
//! who acted, the previous and the new read-set, and whether the change
//! WIDENED disclosure (a new principal entered the effective read-set,
//! computed by [`crate::acl::widens`]). The table is durable engine state,
//! not a cache — it is the change LOG of a DB-authoritative column.
//!
//! Append-only: a row is never updated except to stamp `reverted_at` when
//! the born-applied receipt's revert restores the prior ACL
//! ([`mark_reverted`]).

use sqlx::SqlitePool;

use crate::fact_index::{self, PrevAcl};
use crate::types::{FactId, Principal};

/// Record one applied ACL change as an immutable audit row.
///
/// `prev` is the snapshot [`crate::fact_index::set_acl`] returned;
/// `new_owner` / `new_allow` / `new_sender` are what was stamped; `actor_id`
/// is the raw session sender that made the change; `widening` is the
/// disclosure signal from [`crate::acl::widens`].
///
/// Returns the freshly minted `audit_id` so the caller can later
/// [`mark_reverted`] it.
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on the principal lists.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &SqlitePool,
    fact_id: &FactId,
    wiki_id: &str,
    actor_id: &str,
    prev: &PrevAcl,
    new_owner: &Principal,
    new_allow: &[Principal],
    new_sender: Option<&Principal>,
    widening: bool,
) -> Result<i64, sqlx::Error> {
    let prev_allow_json = fact_index::principals_to_json(&prev.prev_allow_ids)
        .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let new_allow_json =
        fact_index::principals_to_json(new_allow).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO disclosure_audit (
            fact_id, wiki_id, actor_id,
            prev_owner_id, prev_allow_ids, prev_sender_id,
            new_owner_id, new_allow_ids, new_sender_id,
            widening, ts
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING audit_id",
    )
    .bind(fact_id.as_str())
    .bind(wiki_id)
    .bind(actor_id)
    .bind(prev.prev_owner_id.to_string())
    .bind(&prev_allow_json)
    .bind(prev.prev_sender_id.as_ref().map(ToString::to_string))
    .bind(new_owner.to_string())
    .bind(&new_allow_json)
    .bind(new_sender.map(ToString::to_string))
    .bind(i64::from(widening))
    .bind(&now)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Stamp `reverted_at` on an audit row — the revert half of the verb.
///
/// Idempotent: re-marking an already-reverted row overwrites the timestamp
/// (the revert is itself idempotent). Returns the number of rows touched
/// (0 when `audit_id` is unknown).
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn mark_reverted(pool: &SqlitePool, audit_id: i64) -> Result<u64, sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query("UPDATE disclosure_audit SET reverted_at = ? WHERE audit_id = ?")
        .bind(&now)
        .bind(audit_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    #[tokio::test]
    async fn record_then_mark_reverted_round_trips() {
        let pool = make_pool().await;
        let fact_id = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let prev = PrevAcl {
            prev_owner_id: "user:alice".parse().unwrap(),
            prev_allow_ids: vec![],
            prev_sender_id: None,
        };
        let new_allow = vec!["global".parse::<Principal>().unwrap()];
        let id = record(
            &pool,
            &fact_id,
            "alice",
            "alice",
            &prev,
            &"user:alice".parse().unwrap(),
            &new_allow,
            None,
            true,
        )
        .await
        .expect("record");
        assert!(id > 0);

        let (stored_id, widening, reverted): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT audit_id, widening, reverted_at FROM disclosure_audit WHERE audit_id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("row");
        assert_eq!(
            stored_id, id,
            "returned audit_id round-trips to the stored row"
        );
        assert_eq!(widening, 1);
        assert!(reverted.is_none());

        let touched = mark_reverted(&pool, id).await.expect("mark");
        assert_eq!(touched, 1);
        let reverted: Option<String> =
            sqlx::query_scalar("SELECT reverted_at FROM disclosure_audit WHERE audit_id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(reverted.is_some(), "reverted_at stamped");
    }
}
