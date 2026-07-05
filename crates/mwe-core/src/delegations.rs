// SPDX-License-Identifier: AGPL-3.0-or-later
//! Consumer delegation cache — runtime half of `X-MWE-Act-As`.
//!
//! ## What this module does
//!
//! A multi-user consumer bot (Samvise, a Discord bridge, a Slack
//! plugin) holds a single JWT with a `consumer_id` claim and, per
//! tool call, advertises which end-user it is acting on behalf of via
//! the HTTP header `X-MWE-Act-As: <user_id>`. The MCP middleware must
//! decide, for each call, whether the consumer has been delegated to
//! impersonate that user; the answer lives in the
//! [`consumer_delegations`](../../migrations/0014_consumer_delegations.sql)
//! table (`schemi.md §1.11`).
//!
//! Querying the table on every tool call is wasteful — delegation
//! edits are infrequent (admin clicks "Save" on the dashboard) while
//! tool calls are hot. We keep the whole table mirrored in memory and
//! refresh it on a TTL window mirroring [`jwt::BlacklistCache`]:
//!
//! - 60-second sliding TTL (`DELEGATION_REFRESH_INTERVAL`). A change
//!   made on the dashboard propagates to the middleware within that
//!   window without forcing token re-issuance.
//! - Explicit [`DelegationCache::refresh`] for the dashboard write
//!   paths (issue / edit / delete delegation) so an admin sees their
//!   own change reflected on the next tool call instead of waiting up
//!   to 60s.
//! - One snapshot serves every concurrent request: the lock window is
//!   bounded to the set lookup itself.
//!
//! The cache is **deliberately not persisted in the JWT**. A token
//! refresh does not snapshot the delegation; the new token
//! reads the current state of `consumer_delegations` exactly like the
//! old one (`manifesto.md §3.8`).
//!
//! ## Why not foreign keys
//!
//! `consumer_delegations.allowed_sender_ids` is a JSON array (see
//! `schemi.md §1.11`), so referential integrity is enforced
//! applicatively at write time by the dashboard form and at lookup
//! time here — an `allowed_sender_id` that points at a deleted user
//! simply never matches an effective sender, which is the right
//! failure mode (act-as for a no-longer-existing user is denied).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;

/// Refresh window for the in-memory snapshot. Matches the blacklist
/// TTL so the operator's mental model is uniform: any auth-relevant
/// change propagates within ~60s.
pub const DELEGATION_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Errors raised by the delegation cache.
#[derive(Debug, Error)]
pub enum DelegationError {
    /// Underlying SQL failure while refreshing the snapshot.
    #[error("delegations db: {0}")]
    Db(#[from] sqlx::Error),
    /// `allowed_sender_ids` could not be parsed as a JSON array of
    /// strings. We surface this distinctly from a generic SQL error
    /// because it points at a corrupted write rather than a transient
    /// DB blip.
    #[error("delegations json: row `{consumer_id}` has malformed allowed_sender_ids: {source}")]
    MalformedAllowedList {
        /// The consumer whose row failed to parse.
        consumer_id: String,
        /// The serde error encountered.
        #[source]
        source: serde_json::Error,
    },
}

/// In-memory snapshot of `consumer_delegations`, with a
/// [`DELEGATION_REFRESH_INTERVAL`] TTL.
///
/// Shape: `consumer_id -> set of allowed sender_id`. A consumer absent
/// from the map has no delegation rows at all (the bot can still hold
/// a token, it just cannot act-as).
pub struct DelegationCache {
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    map: HashMap<String, HashSet<String>>,
    refreshed_at: Option<Instant>,
}

impl DelegationCache {
    /// Build an empty cache. The first call to [`Self::is_allowed`]
    /// will populate it from the database.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                refreshed_at: None,
            }),
        }
    }

    /// Return `true` iff `consumer_id` has been granted the right to
    /// act-as `sender_id`. Refreshes the snapshot from the DB when
    /// stale or empty.
    pub async fn is_allowed(
        &self,
        pool: &SqlitePool,
        consumer_id: &str,
        sender_id: &str,
    ) -> Result<bool, DelegationError> {
        let needs_refresh = {
            let guard = self.inner.lock();
            guard
                .refreshed_at
                .is_none_or(|t| t.elapsed() >= DELEGATION_REFRESH_INTERVAL)
        };

        if needs_refresh {
            self.refresh(pool).await?;
        }

        let guard = self.inner.lock();
        Ok(guard
            .map
            .get(consumer_id)
            .is_some_and(|allowed| allowed.contains(sender_id)))
    }

    /// Force a reload from `consumer_delegations`. The dashboard
    /// delegation write paths call this so an admin's edit is visible
    /// to the next tool call instead of waiting up to 60s.
    pub async fn refresh(&self, pool: &SqlitePool) -> Result<(), DelegationError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT consumer_id, allowed_sender_ids FROM consumer_delegations")
                .fetch_all(pool)
                .await?;

        let mut next = HashMap::with_capacity(rows.len());
        for (consumer_id, allowed_json) in rows {
            let allowed: Vec<String> = serde_json::from_str(&allowed_json).map_err(|e| {
                DelegationError::MalformedAllowedList {
                    consumer_id: consumer_id.clone(),
                    source: e,
                }
            })?;
            next.insert(consumer_id, allowed.into_iter().collect::<HashSet<_>>());
        }

        let mut guard = self.inner.lock();
        guard.map = next;
        guard.refreshed_at = Some(Instant::now());
        drop(guard);
        Ok(())
    }
}

impl Default for DelegationCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Upsert a delegation row.
///
/// Mirrors the dashboard form's write path so non-dashboard callers
/// (CLI helpers, tests) share the same shape. Does **not** touch the
/// cache — callers that want immediate propagation should invoke
/// [`DelegationCache::refresh`] after this. `allowed_sender_ids` is
/// serialised as the JSON array `consumer_delegations.allowed_sender_ids`
/// expects.
pub async fn upsert(
    pool: &SqlitePool,
    consumer_id: &str,
    allowed_sender_ids: &[String],
    granted_by: &str,
) -> Result<(), DelegationError> {
    let now = chrono::Utc::now().to_rfc3339();
    let allowed_json = serde_json::to_string(allowed_sender_ids).map_err(|e| {
        DelegationError::MalformedAllowedList {
            consumer_id: consumer_id.to_owned(),
            source: e,
        }
    })?;
    sqlx::query(
        "INSERT INTO consumer_delegations
            (consumer_id, allowed_sender_ids, granted_at, granted_by)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(consumer_id) DO UPDATE SET
            allowed_sender_ids = excluded.allowed_sender_ids,
            granted_at         = excluded.granted_at,
            granted_by         = excluded.granted_by",
    )
    .bind(consumer_id)
    .bind(&allowed_json)
    .bind(&now)
    .bind(granted_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Row shape for read-side helpers (dashboard listing, tests).
#[derive(Debug, Clone, Deserialize)]
pub struct DelegationRow {
    /// Stable consumer id, e.g. `samvise-prod`.
    pub consumer_id: String,
    /// JSON array decoded into a vector of user ids the consumer may
    /// impersonate.
    pub allowed_sender_ids: Vec<String>,
    /// ISO 8601 timestamp of the last write.
    pub granted_at: String,
    /// `sender_id` of the admin that last wrote this row.
    pub granted_by: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::tempdir;

    async fn setup() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        (pool, dir)
    }

    #[tokio::test]
    async fn empty_cache_returns_false_after_refresh() {
        let (pool, _dir) = setup().await;
        let cache = DelegationCache::new();
        assert!(
            !cache
                .is_allowed(&pool, "samvise-prod", "frodo")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn upsert_then_refresh_then_allowed() {
        let (pool, _dir) = setup().await;
        upsert(
            &pool,
            "samvise-prod",
            &["frodo".to_owned(), "galadriel".to_owned()],
            "frodo",
        )
        .await
        .unwrap();

        let cache = DelegationCache::new();
        // First call triggers the lazy refresh.
        assert!(
            cache
                .is_allowed(&pool, "samvise-prod", "frodo")
                .await
                .unwrap()
        );
        assert!(
            cache
                .is_allowed(&pool, "samvise-prod", "galadriel")
                .await
                .unwrap()
        );
        assert!(
            !cache
                .is_allowed(&pool, "samvise-prod", "gollum")
                .await
                .unwrap()
        );
        assert!(!cache.is_allowed(&pool, "other-bot", "frodo").await.unwrap());
    }

    #[tokio::test]
    async fn explicit_refresh_propagates_before_ttl() {
        let (pool, _dir) = setup().await;
        let cache = DelegationCache::new();

        // Warm the cache with an empty snapshot.
        cache.refresh(&pool).await.unwrap();
        assert!(
            !cache
                .is_allowed(&pool, "samvise-prod", "frodo")
                .await
                .unwrap()
        );

        // Admin grants delegation; we call refresh() right after the
        // write to simulate the dashboard's behaviour.
        upsert(&pool, "samvise-prod", &["frodo".to_owned()], "frodo")
            .await
            .unwrap();
        cache.refresh(&pool).await.unwrap();

        assert!(
            cache
                .is_allowed(&pool, "samvise-prod", "frodo")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn upsert_overwrites_previous_row() {
        let (pool, _dir) = setup().await;
        upsert(&pool, "samvise-prod", &["frodo".to_owned()], "frodo")
            .await
            .unwrap();
        upsert(
            &pool,
            "samvise-prod",
            &["galadriel".to_owned()],
            "galadriel",
        )
        .await
        .unwrap();

        let cache = DelegationCache::new();
        assert!(
            !cache
                .is_allowed(&pool, "samvise-prod", "frodo")
                .await
                .unwrap()
        );
        assert!(
            cache
                .is_allowed(&pool, "samvise-prod", "galadriel")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn refresh_surfaces_malformed_allowed_list() {
        let (pool, _dir) = setup().await;
        sqlx::query(
            "INSERT INTO consumer_delegations
                (consumer_id, allowed_sender_ids, granted_at, granted_by)
             VALUES (?, ?, ?, ?)",
        )
        .bind("samvise-prod")
        .bind("this is not json")
        .bind("2026-05-23T00:00:00Z")
        .bind("frodo")
        .execute(&pool)
        .await
        .unwrap();

        let cache = DelegationCache::new();
        let err = cache.refresh(&pool).await.unwrap_err();
        match err {
            DelegationError::MalformedAllowedList { consumer_id, .. } => {
                assert_eq!(consumer_id, "samvise-prod");
            },
            DelegationError::Db(e) => panic!("expected MalformedAllowedList, got Db: {e}"),
        }
    }
}
