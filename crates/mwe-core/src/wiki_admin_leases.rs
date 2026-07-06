// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional cooperative lease for `wiki_admin_push` coordination
//! across multiple smart consumers of the same owner — see
//! smart-wikis.
//!
//! The lease is **opt-in**: a smart consumer that does not call
//! [`acquire`] sees the existing push semantics (subject to future
//! `expected_op_log_head` optimistic concurrency, deferred from M.12).
//! A smart consumer that *does* call [`acquire`] declares "I am
//! authoritative on this smart-wiki for the next TTL window" —
//! any other consumer attempting `wiki_admin_push` while the lease
//! is active gets [`AcquireError::WikiLockedByLease`] mapped to a
//! `423 wiki_locked_by_lease` wire code (carrying `held_by_consumer_id`
//! + `expires_at`).
//!
//! Re-acquire from the same `(sender_id, consumer_id)` extends the
//! existing row instead of inserting a sibling — the lease is "I am
//! still here, please push my window out", not a syntactic mutex.
//!
//! REM ships a nightly `lease_expirer` sub-job that prunes:
//!
//! - Active rows where `expires_at < now - cleanup_grace` (treated as
//!   crashed-without-release; the grace prevents racing a slow client
//!   that is about to re-acquire).
//! - Released rows older than `released_at + retention` (audit
//!   retention, default 7 days).
//!
//! Scope explicitly outside MVP:
//!
//! - Collaborative-write across multiple owners (`group:` `shared_with`
//!   members pushing to the same smart-wiki). The lease coordinates
//!   between devices of the *same* user; cross-user collaborative
//!   write is deferred — see [memory model](../../../docs/concepts/memory-model.md).
//! - Pre-emption (revoke a lease held by another consumer). Out of
//!   scope: if a lease was acquired by a crashed laptop, wait for
//!   the TTL to expire or wait for REM's `lease_expirer` (which picks
//!   it up `cleanup_grace` after expiry). Reasonable defaults make
//!   the worst case "next REM cycle"; the user can always shorten
//!   the TTL when acquiring.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::types::WikiId;
use crate::wiki_admin::AdminCaller;

/// Default lease TTL when the caller does not specify one (seconds).
pub const DEFAULT_TTL_SECS: i64 = 60;

/// Maximum lease TTL the server will grant in a single
/// [`acquire`] call (seconds).
///
/// Pre-emption is out of MVP, so the cap matters: a laptop that
/// crashes after acquiring a 10-minute lease blocks every other device
/// until REM's sweep picks the stale row up.
pub const MAX_TTL_SECS: i64 = 300;

/// Grace period before the `lease_expirer` REM sub-job prunes an
/// active-but-expired row, in seconds.
///
/// Lets a slow client win the re-acquire race after a brief network
/// hiccup instead of seeing its lease vanish under it.
pub const EXPIRER_CLEANUP_GRACE_SECS: i64 = 3_600;

/// How long released rows linger in the table for audit before the
/// expirer removes them, in seconds. Seven days is a comfortable
/// reading window for the `/wikis/<id>/op-log` dashboard.
pub const EXPIRER_RELEASED_RETENTION_SECS: i64 = 7 * 24 * 3_600;

/// Errors returned by [`acquire`].
#[derive(Debug, Error)]
pub enum AcquireError {
    /// Caller's JWT does not carry `consumer_class=smart`.
    #[error("requires consumer_class=smart (see protocollo.md §2)")]
    RequiresSmart,
    /// Requested TTL is outside `[1, MAX_TTL_SECS]`.
    #[error("ttl_sec {requested} out of range [1, {cap}]")]
    InvalidTtl {
        /// Value the caller asked for.
        requested: i64,
        /// Server-side cap ([`MAX_TTL_SECS`]).
        cap: i64,
    },
    /// Another smart consumer holds an active lease on this wiki.
    #[error(
        "wiki {wiki_id} is locked by lease held by consumer {held_by_consumer_id:?} until {expires_at}"
    )]
    WikiLockedByLease {
        /// Target wiki id.
        wiki_id: WikiId,
        /// `consumer_id` of the holding device, when set.
        held_by_consumer_id: Option<String>,
        /// `sender_id` of the holding user (always set).
        held_by_sender_id: String,
        /// ISO-8601 UTC `expires_at` of the holding lease.
        expires_at: String,
    },
    /// Database failure.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}

/// Errors returned by [`release`].
#[derive(Debug, Error)]
pub enum ReleaseError {
    /// Caller's JWT does not carry `consumer_class=smart`.
    #[error("requires consumer_class=smart (see protocollo.md §2)")]
    RequiresSmart,
    /// `lease_id` was not found, was already released, or belongs to
    /// a different `(sender_id, consumer_id)` pair.
    #[error("lease {lease_id} not found, already released, or held by another consumer")]
    NotHeldByCaller {
        /// The lease id the caller passed.
        lease_id: String,
    },
    /// Database failure.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}

/// Result of a successful [`acquire`] call. The MCP layer maps this
/// to the wire shape of `wiki_admin_lease_acquire` (the `wiki_id`
/// is serialised via its `Display` impl).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireOutcome {
    /// Server-issued lease id (opaque, used by [`release`]).
    pub lease_id: String,
    /// Target wiki id.
    pub wiki_id: WikiId,
    /// `sender_id` recorded for the lease.
    pub sender_id: String,
    /// `consumer_id` recorded for the lease, when set.
    pub consumer_id: Option<String>,
    /// ISO-8601 UTC `acquired_at`.
    pub acquired_at: String,
    /// ISO-8601 UTC `expires_at`.
    pub expires_at: String,
    /// `true` if this call extended an existing row from the same
    /// `(sender_id, consumer_id)` instead of inserting a new one.
    /// Lets the consumer differentiate "first acquire" from "renewal"
    /// for telemetry.
    pub renewed: bool,
}

/// Result of a successful [`release`] call. The MCP layer maps
/// `wiki_id` via `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseOutcome {
    /// Released lease id (echoed for caller convenience).
    pub lease_id: String,
    /// Wiki the lease was held on.
    pub wiki_id: WikiId,
    /// ISO-8601 UTC `released_at`.
    pub released_at: String,
}

/// Snapshot of the currently active lease (if any) on a wiki — used
/// by `wiki_admin::push` to short-circuit with `WikiLockedByLease`
/// when a different consumer holds the row.
#[derive(Debug, Clone)]
pub struct ActiveLease {
    /// Lease id of the active row.
    pub lease_id: String,
    /// `sender_id` of the lease holder.
    pub sender_id: String,
    /// `consumer_id` of the lease holder, when set.
    pub consumer_id: Option<String>,
    /// ISO-8601 UTC `expires_at`.
    pub expires_at: String,
}

impl ActiveLease {
    /// `true` when this lease was acquired by the same logical
    /// consumer as the caller — same `sender_id` and same
    /// `consumer_id` (both `None` counts as "same anonymous device").
    #[must_use]
    pub fn held_by(&self, caller: &AdminCaller) -> bool {
        self.sender_id == caller.sender_id && self.consumer_id == caller.consumer_id
    }
}

/// Returns the active lease on `wiki_id` if one exists (not released,
/// not expired by clock). Single SQL round-trip.
///
/// # Errors
///
/// Bubbles `sqlx::Error` on database failures.
pub async fn active_lease_for(
    pool: &SqlitePool,
    wiki_id: &WikiId,
    now: DateTime<Utc>,
) -> Result<Option<ActiveLease>, sqlx::Error> {
    let row: Option<(String, String, Option<String>, String)> = sqlx::query_as(
        r"
        SELECT lease_id, sender_id, consumer_id, expires_at
        FROM wiki_admin_leases
        WHERE wiki_id = ?
          AND released_at IS NULL
          AND expires_at > ?
        ORDER BY acquired_at DESC
        LIMIT 1
        ",
    )
    .bind(wiki_id.as_str())
    .bind(now.to_rfc3339())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(lease_id, sender_id, consumer_id, expires_at)| ActiveLease {
            lease_id,
            sender_id,
            consumer_id,
            expires_at,
        },
    ))
}

/// Acquire (or extend) a lease on `wiki_id`. See module docstring
/// for the invariants.
///
/// # Errors
///
/// One of [`AcquireError`], mapped at the MCP layer to wire codes
/// `403 requires_consumer_class_smart` / `400 invalid_input` /
/// `423 wiki_locked_by_lease` / `500 internal_error`.
pub async fn acquire(
    pool: &SqlitePool,
    caller: &AdminCaller,
    wiki_id: &WikiId,
    ttl_sec: Option<i64>,
) -> Result<AcquireOutcome, AcquireError> {
    if !caller.consumer_class.is_smart() {
        return Err(AcquireError::RequiresSmart);
    }
    let ttl = ttl_sec.unwrap_or(DEFAULT_TTL_SECS);
    if !(1..=MAX_TTL_SECS).contains(&ttl) {
        return Err(AcquireError::InvalidTtl {
            requested: ttl,
            cap: MAX_TTL_SECS,
        });
    }

    let now = Utc::now();
    let active = active_lease_for(pool, wiki_id, now).await?;

    let new_expires = now + Duration::seconds(ttl);
    let new_expires_iso = new_expires.to_rfc3339();
    let now_iso = now.to_rfc3339();
    let wiki = wiki_id.as_str();

    if let Some(existing) = active {
        if existing.held_by(caller) {
            sqlx::query(
                r"
                UPDATE wiki_admin_leases
                SET expires_at = ?
                WHERE lease_id = ?
                ",
            )
            .bind(&new_expires_iso)
            .bind(&existing.lease_id)
            .execute(pool)
            .await?;
            return Ok(AcquireOutcome {
                lease_id: existing.lease_id,
                wiki_id: wiki_id.clone(),
                sender_id: caller.sender_id.clone(),
                consumer_id: caller.consumer_id.clone(),
                acquired_at: now_iso,
                expires_at: new_expires_iso,
                renewed: true,
            });
        }
        return Err(AcquireError::WikiLockedByLease {
            wiki_id: wiki_id.clone(),
            held_by_consumer_id: existing.consumer_id,
            held_by_sender_id: existing.sender_id,
            expires_at: existing.expires_at,
        });
    }

    let lease_id = format!("wal_{}", uuid_like(&now, wiki_id, &caller.sender_id));
    sqlx::query(
        r"
        INSERT INTO wiki_admin_leases (lease_id, wiki_id, sender_id, consumer_id, acquired_at, expires_at)
        VALUES (?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&lease_id)
    .bind(wiki)
    .bind(caller.sender_id.as_str())
    .bind(caller.consumer_id.as_deref())
    .bind(&now_iso)
    .bind(&new_expires_iso)
    .execute(pool)
    .await?;

    Ok(AcquireOutcome {
        lease_id,
        wiki_id: wiki_id.clone(),
        sender_id: caller.sender_id.clone(),
        consumer_id: caller.consumer_id.clone(),
        acquired_at: now_iso,
        expires_at: new_expires_iso,
        renewed: false,
    })
}

/// Release a lease held by the caller.
///
/// Idempotent in the sense that releasing a lease that is already
/// released or expired (and so effectively gone) is still an error —
/// callers should not be asking to release things they do not own.
///
/// # Errors
///
/// One of [`ReleaseError`].
pub async fn release(
    pool: &SqlitePool,
    caller: &AdminCaller,
    lease_id: &str,
) -> Result<ReleaseOutcome, ReleaseError> {
    if !caller.consumer_class.is_smart() {
        return Err(ReleaseError::RequiresSmart);
    }
    let now = Utc::now();
    let now_iso = now.to_rfc3339();

    // SQL form below uses `IS` so a NULL caller `consumer_id` matches
    // a NULL stored `consumer_id` (SQLite treats `= NULL` as unknown).
    let row: Option<(String,)> = sqlx::query_as(
        r"
        UPDATE wiki_admin_leases
        SET released_at = ?
        WHERE lease_id = ?
          AND sender_id = ?
          AND consumer_id IS ?
          AND released_at IS NULL
        RETURNING wiki_id
        ",
    )
    .bind(&now_iso)
    .bind(lease_id)
    .bind(caller.sender_id.as_str())
    .bind(caller.consumer_id.as_deref())
    .fetch_optional(pool)
    .await?;

    let Some((wiki_id_raw,)) = row else {
        return Err(ReleaseError::NotHeldByCaller {
            lease_id: lease_id.to_owned(),
        });
    };
    Ok(ReleaseOutcome {
        lease_id: lease_id.to_owned(),
        wiki_id: WikiId::parse(&wiki_id_raw).unwrap_or_else(|_| {
            // Stored ids are always validated on insert; this branch
            // is defensive against migrations going sideways.
            WikiId::root()
        }),
        released_at: now_iso,
    })
}

/// Report shape returned by [`expire_stale`] — used by REM's
/// `lease_expirer` sub-job for cycle reporting.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpirerReport {
    /// Rows that were active-but-expired (treated as crashed
    /// without release) and got `released_at` stamped to `now`.
    pub stale_active_marked_released: u64,
    /// Released rows older than the retention window that were
    /// deleted outright.
    pub aged_released_rows_deleted: u64,
}

/// Sweep run by REM's `lease_expirer` sub-job.
///
/// Two passes in one call:
///
/// 1. **Stale-active**: rows with `released_at IS NULL` and
///    `expires_at < now - cleanup_grace_secs` are marked released
///    (`released_at = now`). The grace prevents racing a client that
///    is about to re-acquire after a brief stall.
/// 2. **Aged-released**: rows with `released_at < now - retention_secs`
///    are deleted. The retention window is the dashboard's `op-log`
///    visibility budget for past leases.
///
/// # Errors
///
/// Bubbles `sqlx::Error` on database failures.
pub async fn expire_stale(
    pool: &SqlitePool,
    now: DateTime<Utc>,
    cleanup_grace_secs: i64,
    retention_secs: i64,
) -> Result<ExpirerReport, sqlx::Error> {
    let stale_cutoff = (now - Duration::seconds(cleanup_grace_secs)).to_rfc3339();
    let stale_now = now.to_rfc3339();
    let mark = sqlx::query(
        r"
        UPDATE wiki_admin_leases
        SET released_at = ?
        WHERE released_at IS NULL
          AND expires_at < ?
        ",
    )
    .bind(&stale_now)
    .bind(&stale_cutoff)
    .execute(pool)
    .await?;

    let retention_cutoff = (now - Duration::seconds(retention_secs)).to_rfc3339();
    let drop = sqlx::query(
        r"
        DELETE FROM wiki_admin_leases
        WHERE released_at IS NOT NULL
          AND released_at < ?
        ",
    )
    .bind(&retention_cutoff)
    .execute(pool)
    .await?;

    Ok(ExpirerReport {
        stale_active_marked_released: mark.rows_affected(),
        aged_released_rows_deleted: drop.rows_affected(),
    })
}

fn uuid_like(now: &DateTime<Utc>, wiki_id: &WikiId, sender_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(now.timestamp_nanos_opt().unwrap_or_default().to_be_bytes());
    h.update(wiki_id.as_str().as_bytes());
    h.update(sender_id.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::ConsumerClass;
    use sqlx::sqlite::SqlitePoolOptions;

    fn caller(sender: &str, consumer: Option<&str>) -> AdminCaller {
        AdminCaller {
            sender_id: sender.to_owned(),
            consumer_id: consumer.map(str::to_owned),
            consumer_class: ConsumerClass::Smart,
        }
    }

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

    async fn setup() -> (SqlitePool, WikiId) {
        let pool = make_pool().await;
        let wiki_id = WikiId::parse("companion-test").unwrap();
        (pool, wiki_id)
    }

    async fn insert_lease_row(
        pool: &SqlitePool,
        wiki: &WikiId,
        owner: &AdminCaller,
        lease_id: &str,
        acquired: &str,
        expires: &str,
        released: Option<&str>,
    ) {
        sqlx::query(
            r"
            INSERT INTO wiki_admin_leases (lease_id, wiki_id, sender_id, consumer_id, acquired_at, expires_at, released_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(lease_id)
        .bind(wiki.as_str())
        .bind(owner.sender_id.as_str())
        .bind(owner.consumer_id.as_deref())
        .bind(acquired)
        .bind(expires)
        .bind(released)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn acquire_inserts_new_row_for_first_caller() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", Some("cc-laptop"));
        let out = acquire(&pool, &alice, &wiki, Some(30)).await.unwrap();
        assert_eq!(out.sender_id, "alice");
        assert_eq!(out.consumer_id.as_deref(), Some("cc-laptop"));
        assert!(!out.renewed);
        assert!(out.lease_id.starts_with("wal_"));
    }

    #[tokio::test]
    async fn acquire_extends_when_same_consumer_re_acquires() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", Some("cc-laptop"));
        let first = acquire(&pool, &alice, &wiki, Some(30)).await.unwrap();
        let second = acquire(&pool, &alice, &wiki, Some(60)).await.unwrap();
        assert_eq!(
            second.lease_id, first.lease_id,
            "extending must reuse the same row"
        );
        assert!(second.renewed);
        assert!(
            second.expires_at >= first.expires_at,
            "extension must push expires_at forward (got {} -> {})",
            first.expires_at,
            second.expires_at,
        );
    }

    #[tokio::test]
    async fn acquire_locks_out_different_consumer_id_same_user() {
        let (pool, wiki) = setup().await;
        let laptop = caller("alice", Some("cc-laptop"));
        let desktop = caller("alice", Some("cc-desktop"));
        acquire(&pool, &laptop, &wiki, Some(60)).await.unwrap();
        let err = acquire(&pool, &desktop, &wiki, Some(60)).await.unwrap_err();
        let AcquireError::WikiLockedByLease {
            held_by_consumer_id,
            held_by_sender_id,
            ..
        } = err
        else {
            panic!("expected WikiLockedByLease, got {err:?}");
        };
        assert_eq!(held_by_consumer_id.as_deref(), Some("cc-laptop"));
        assert_eq!(held_by_sender_id, "alice");
    }

    #[tokio::test]
    async fn acquire_locks_out_different_sender_user() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", Some("cc-laptop"));
        let bob = caller("bob", Some("cc-laptop"));
        acquire(&pool, &alice, &wiki, Some(60)).await.unwrap();
        let err = acquire(&pool, &bob, &wiki, Some(60)).await.unwrap_err();
        assert!(matches!(err, AcquireError::WikiLockedByLease { .. }));
    }

    #[tokio::test]
    async fn acquire_rejects_non_smart_caller() {
        let (pool, wiki) = setup().await;
        let standard = AdminCaller {
            sender_id: "alice".into(),
            consumer_id: None,
            consumer_class: ConsumerClass::Standard,
        };
        let err = acquire(&pool, &standard, &wiki, Some(60))
            .await
            .unwrap_err();
        assert!(matches!(err, AcquireError::RequiresSmart));
    }

    #[tokio::test]
    async fn acquire_rejects_ttl_outside_range() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", None);
        let too_low = acquire(&pool, &alice, &wiki, Some(0)).await.unwrap_err();
        assert!(matches!(too_low, AcquireError::InvalidTtl { .. }));
        let too_high = acquire(&pool, &alice, &wiki, Some(MAX_TTL_SECS + 1))
            .await
            .unwrap_err();
        assert!(matches!(too_high, AcquireError::InvalidTtl { .. }));
    }

    #[tokio::test]
    async fn release_marks_row_released_and_unlocks_other_consumer() {
        let (pool, wiki) = setup().await;
        let laptop = caller("alice", Some("cc-laptop"));
        let desktop = caller("alice", Some("cc-desktop"));
        let acquired = acquire(&pool, &laptop, &wiki, Some(60)).await.unwrap();
        let outcome = release(&pool, &laptop, &acquired.lease_id).await.unwrap();
        assert_eq!(outcome.lease_id, acquired.lease_id);
        // Desktop can now acquire.
        let ok = acquire(&pool, &desktop, &wiki, Some(60)).await.unwrap();
        assert!(!ok.renewed);
        assert_ne!(ok.lease_id, acquired.lease_id);
    }

    #[tokio::test]
    async fn release_refuses_a_different_caller() {
        let (pool, wiki) = setup().await;
        let laptop = caller("alice", Some("cc-laptop"));
        let desktop = caller("alice", Some("cc-desktop"));
        let acquired = acquire(&pool, &laptop, &wiki, Some(60)).await.unwrap();
        let err = release(&pool, &desktop, &acquired.lease_id)
            .await
            .unwrap_err();
        assert!(matches!(err, ReleaseError::NotHeldByCaller { .. }));
    }

    #[tokio::test]
    async fn release_is_one_shot() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", None);
        let acquired = acquire(&pool, &alice, &wiki, Some(60)).await.unwrap();
        release(&pool, &alice, &acquired.lease_id).await.unwrap();
        let err = release(&pool, &alice, &acquired.lease_id)
            .await
            .unwrap_err();
        assert!(matches!(err, ReleaseError::NotHeldByCaller { .. }));
    }

    #[tokio::test]
    async fn expire_stale_marks_active_rows_past_grace_and_drops_aged_released() {
        let (pool, wiki) = setup().await;
        let alice = caller("alice", Some("cc-laptop"));
        let now = Utc::now();

        // (a) Active-but-expired row inserted with a backdated
        // acquired_at + expires_at so the grace window matters.
        insert_lease_row(
            &pool,
            &wiki,
            &alice,
            "wal_stale_a",
            &(now - Duration::hours(2)).to_rfc3339(),
            &(now - Duration::hours(2) + Duration::seconds(60)).to_rfc3339(),
            None,
        )
        .await;

        // (b) Released row beyond retention.
        insert_lease_row(
            &pool,
            &wiki,
            &alice,
            "wal_aged_b",
            &(now - Duration::days(10)).to_rfc3339(),
            &(now - Duration::days(10) + Duration::seconds(60)).to_rfc3339(),
            Some(&(now - Duration::days(8)).to_rfc3339()),
        )
        .await;

        // (c) Recently released row within retention — must survive.
        insert_lease_row(
            &pool,
            &wiki,
            &alice,
            "wal_fresh_c",
            &(now - Duration::hours(3)).to_rfc3339(),
            &(now - Duration::hours(3) + Duration::seconds(60)).to_rfc3339(),
            Some(&(now - Duration::hours(2)).to_rfc3339()),
        )
        .await;

        let report = expire_stale(
            &pool,
            now,
            EXPIRER_CLEANUP_GRACE_SECS,
            EXPIRER_RELEASED_RETENTION_SECS,
        )
        .await
        .unwrap();
        assert_eq!(report.stale_active_marked_released, 1);
        assert_eq!(report.aged_released_rows_deleted, 1);

        // Row (a) should now be released; (b) gone; (c) untouched.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wiki_admin_leases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);
        let still_active: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM wiki_admin_leases WHERE released_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(still_active.0, 0);
    }
}
