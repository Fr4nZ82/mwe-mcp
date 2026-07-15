// SPDX-License-Identifier: AGPL-3.0-or-later
//! Runtime housekeeping — the hygiene sweeps the hot paths defer.
//!
//! One entry point, [`run`], invoked at `serve` boot and after a dashboard
//! wiki deletion. It drains three kinds of residue that otherwise
//! accumulate unbounded (each observed live on the dogfood deployment):
//!
//! - **Expired authorization codes** — a code self-deletes on redemption
//!   (single-use), but an abandoned OAuth flow leaves its row behind
//!   forever. (`issue_auth_code` also purges opportunistically, so a busy
//!   server self-cleans between boots.)
//! - **Stale refresh-token rows** — rotation and fresh connects prune
//!   their own connection inline (see
//!   [`crate::oauth_server::rotate_refresh_token`]), but rows revoked by
//!   a dashboard disconnect or expired while idle have no inline owner.
//!   Per connection the newest stale row is **kept** while no active row
//!   exists: the `consumers` table carries no wiki column, so that row is
//!   the durable record binding the consumer to its smart wiki — the
//!   dangling-consumer sweep below reads the binding from it.
//! - **Dangling web-agent consumers** — deleting a smart wiki from the
//!   dashboard leaves the consumer row that authored it (plus its
//!   delegations and OAuth rows) pointing at nothing. A consumer is
//!   dangling only when **every** wiki its refresh rows name is gone;
//!   token-registered consumers (`system_user_id` set — they own no
//!   OAuth rows) are never touched, and a consumer whose wiki still
//!   exists survives disconnection (a reconnect reuses it).
//!
//! Current state documented in
//! the web-agent-oauth design note (§ Housekeeping).

use std::collections::{BTreeMap, HashSet};

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::error::Result;
use crate::oauth_server::now_iso;
use crate::types::WikiId;
use crate::wiki::WikiTree;

/// What one [`run`] swept. All counts are rows removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HousekeepingReport {
    /// Expired `webagentoauth_codes` rows purged.
    pub auth_codes_purged: u64,
    /// Stale (revoked / expired) `webagentoauth_refresh` rows pruned.
    pub stale_refresh_pruned: u64,
    /// `consumers` rows removed because every wiki they authored is gone.
    pub dangling_consumers_removed: u64,
    /// `consumer_delegations` rows removed with their consumer.
    pub delegations_removed: u64,
}

impl HousekeepingReport {
    /// `true` when the sweep found nothing to remove.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }
}

/// Run every housekeeping sweep once. Sweeps are independent; the
/// dangling-consumer pass runs **before** the stale-refresh prune so it
/// still sees each consumer's wiki bindings at full resolution.
///
/// # Errors
///
/// As [`sqlx::Error`] — a failed sweep aborts the run (callers treat the
/// whole pass as best-effort and log instead of dying).
pub async fn run(pool: &SqlitePool, tree: &WikiTree) -> Result<HousekeepingReport> {
    let now = now_iso();
    let auth_codes_purged = purge_expired_auth_codes(pool, &now).await?;
    let (dangling_consumers_removed, delegations_removed) =
        sweep_dangling_consumers(pool, tree).await?;
    let stale_refresh_pruned = prune_stale_refresh_rows(pool, &now).await?;
    Ok(HousekeepingReport {
        auth_codes_purged,
        stale_refresh_pruned,
        dangling_consumers_removed,
        delegations_removed,
    })
}

/// Delete authorization codes past their expiry.
async fn purge_expired_auth_codes(pool: &SqlitePool, now: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM webagentoauth_codes WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Delete stale (revoked or expired) refresh rows, keeping — per
/// `(sender, consumer, wiki)` connection — the newest stale row when the
/// connection has no active row left: that survivor is the wiki-binding
/// record the dangling-consumer sweep keys on. A connection that *does*
/// hold an active row needs no stale keeper and drains completely.
async fn prune_stale_refresh_rows(pool: &SqlitePool, now: &str) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM webagentoauth_refresh
          WHERE (revoked_at IS NOT NULL OR expires_at < ?)
            AND rowid NOT IN (
                SELECT MAX(r2.rowid)
                  FROM webagentoauth_refresh r2
                 WHERE (r2.revoked_at IS NOT NULL OR r2.expires_at < ?)
                   AND NOT EXISTS (
                       SELECT 1 FROM webagentoauth_refresh a
                        WHERE a.sender_id = r2.sender_id
                          AND a.consumer_id = r2.consumer_id
                          AND a.wiki_id = r2.wiki_id
                          AND a.revoked_at IS NULL
                          AND a.expires_at >= ?)
                 GROUP BY r2.sender_id, r2.consumer_id, r2.wiki_id)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Remove web-agent consumers whose every authored wiki is gone from
/// disk, cascading to their delegations and OAuth rows. Returns
/// `(consumers_removed, delegations_removed)`.
async fn sweep_dangling_consumers(pool: &SqlitePool, tree: &WikiTree) -> Result<(u64, u64)> {
    // Snapshot the wikis present on disk, once. A walk failure — a single
    // malformed / half-written `_meta.md`, or a transient IO fault anywhere
    // under `wikis/` — is **not** evidence that a consumer is dangling; it
    // means we cannot judge. Fail safe: skip the sweep rather than risk
    // mass-deleting OAuth / registration / delegation rows, which are
    // DB-authoritative and not rebuildable from the markdown surface. (Probing
    // per id with `locate` would instead collapse any such error into "not
    // found" via its own `walk()?`, wrongly judging *every* consumer gone.)
    let present: HashSet<WikiId> = match tree.walk() {
        Ok(wikis) => wikis.into_iter().map(|w| w.meta.wiki_id).collect(),
        Err(error) => {
            warn!(%error, "housekeeping: wiki tree unreadable; skipping dangling-consumer sweep");
            return Ok((0, 0));
        },
    };

    // Wiki bindings are read from the refresh rows (`consumers` has no
    // wiki column); the join keeps token-registered consumers out.
    let bindings: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT r.consumer_id, r.wiki_id
           FROM webagentoauth_refresh r
           JOIN consumers c ON c.consumer_id = r.consumer_id
          WHERE c.system_user_id IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut by_consumer: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (consumer_id, wiki_id) in bindings {
        by_consumer.entry(consumer_id).or_default().push(wiki_id);
    }

    let mut consumers_removed = 0u64;
    let mut delegations_removed = 0u64;
    for (consumer_id, wikis) in by_consumer {
        let all_gone = wikis.iter().all(|raw| {
            // Gone = parses to a real id the fresh walk did not list. An
            // unparseable id matches no present wiki, so it is gone too
            // (it never named a real directory).
            WikiId::parse(raw).map_or(true, |id| !present.contains(&id))
        });
        if !all_gone {
            continue;
        }
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM webagentoauth_refresh WHERE consumer_id = ?")
            .bind(&consumer_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM webagentoauth_codes WHERE consumer_id = ?")
            .bind(&consumer_id)
            .execute(&mut *tx)
            .await?;
        let deleg = sqlx::query("DELETE FROM consumer_delegations WHERE consumer_id = ?")
            .bind(&consumer_id)
            .execute(&mut *tx)
            .await?;
        // Re-check the class guard inside the tx: a concurrent register
        // may have just claimed the id for a token consumer.
        let gone =
            sqlx::query("DELETE FROM consumers WHERE consumer_id = ? AND system_user_id IS NULL")
                .bind(&consumer_id)
                .execute(&mut *tx)
                .await?;
        tx.commit().await?;
        delegations_removed += deleg.rows_affected();
        if gone.rows_affected() > 0 {
            consumers_removed += 1;
            info!(
                consumer_id,
                wikis = ?wikis,
                "housekeeping: dangling web-agent consumer removed (authored wikis gone)"
            );
        }
    }
    Ok((consumers_removed, delegations_removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_server::{RefreshGrant, issue_refresh_token, revoke_connection};
    use std::time::Duration;

    async fn fixture() -> (SqlitePool, WikiTree, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        (pool, tree, dir)
    }

    /// Minimal locatable wiki: a directory with a parseable `_meta.md`.
    fn seed_wiki(tree: &WikiTree, id: &str) {
        let dir = tree.wikis_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::wiki::META_FILENAME),
            format!(
                "---\nwiki_id: {id}\nwiki_type: wiki-smart\nparent_wiki_id: null\n\
                 slug: {id}\ntitle: {id}\nacl_default: 'user:owner'\n---\n"
            ),
        )
        .unwrap();
    }

    async fn seed_consumer(pool: &SqlitePool, id: &str, system_user: Option<&str>) {
        if let Some(user) = system_user {
            // `consumers.system_user_id` is a foreign key into enrollment.
            sqlx::query("INSERT OR IGNORE INTO enrollment_users (user_id) VALUES (?)")
                .bind(user)
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO consumers (consumer_id, consumer_secret, registered_at, system_user_id)
             VALUES (?, 'x', ?, ?)",
        )
        .bind(id)
        .bind(now_iso())
        .bind(system_user)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a stale (already revoked) refresh row directly — the public
    /// API can no longer accumulate these, its inline prune removes them.
    async fn insert_stale_refresh(pool: &SqlitePool, consumer: &str, wiki: &str, hash: &str) {
        sqlx::query(
            "INSERT INTO webagentoauth_refresh
                 (token_hash, client_id, sender_id, consumer_id, wiki_id, created_at,
                  expires_at, revoked_at)
             VALUES (?, 'cid', 'franz', ?, ?, ?, '2999-01-01T00:00:00+00:00', ?)",
        )
        .bind(hash)
        .bind(consumer)
        .bind(wiki)
        .bind(now_iso())
        .bind(now_iso())
        .execute(pool)
        .await
        .unwrap();
    }

    fn grant(consumer: &str, wiki: &str) -> RefreshGrant {
        RefreshGrant {
            client_id: "cid".to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: consumer.to_owned(),
            wiki_id: wiki.to_owned(),
        }
    }

    async fn refresh_rows(pool: &SqlitePool, consumer: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM webagentoauth_refresh WHERE consumer_id = ?")
            .bind(consumer)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn consumer_exists(pool: &SqlitePool, consumer: &str) -> bool {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consumers WHERE consumer_id = ?")
            .bind(consumer)
            .fetch_one(pool)
            .await
            .unwrap();
        n > 0
    }

    #[tokio::test]
    async fn dangling_consumer_is_swept_with_its_rows() {
        let (pool, tree, _dir) = fixture().await;
        // `ghost` authored a wiki that no longer exists on disk.
        seed_consumer(&pool, "ghost", None).await;
        issue_refresh_token(
            &pool,
            &grant("ghost", "franz-ghost"),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO consumer_delegations
                 (consumer_id, allowed_sender_ids, granted_at, granted_by)
             VALUES ('ghost', '[]', ?, 'admin')",
        )
        .bind(now_iso())
        .execute(&pool)
        .await
        .unwrap();

        let report = run(&pool, &tree).await.unwrap();
        assert_eq!(report.dangling_consumers_removed, 1);
        assert_eq!(report.delegations_removed, 1);
        assert!(!consumer_exists(&pool, "ghost").await);
        assert_eq!(refresh_rows(&pool, "ghost").await, 0);
    }

    #[tokio::test]
    async fn living_wiki_and_token_consumers_survive() {
        let (pool, tree, _dir) = fixture().await;
        // `alive` authored a wiki that exists; `bot` is token-registered
        // (system_user_id set) and owns no OAuth rows at all.
        seed_wiki(&tree, "franz-alive");
        seed_consumer(&pool, "alive", None).await;
        issue_refresh_token(
            &pool,
            &grant("alive", "franz-alive"),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
        seed_consumer(&pool, "bot", Some("bot")).await;

        let report = run(&pool, &tree).await.unwrap();
        assert_eq!(report.dangling_consumers_removed, 0);
        assert!(consumer_exists(&pool, "alive").await);
        assert!(consumer_exists(&pool, "bot").await);
    }

    /// Regression: a single unreadable `_meta.md` anywhere in the tree must
    /// not turn the sweep into a mass delete. Before the walk-once fix,
    /// `locate` funneled the tree-walk error into "not found" and judged
    /// *every* web-agent consumer dangling — wiping OAuth/delegation state
    /// that cannot be rebuilt from the markdown surface.
    #[tokio::test]
    async fn unreadable_tree_never_sweeps_live_consumers() {
        let (pool, tree, _dir) = fixture().await;
        // A live consumer whose wiki exists on disk.
        seed_wiki(&tree, "franz-alive");
        seed_consumer(&pool, "alive", None).await;
        issue_refresh_token(
            &pool,
            &grant("alive", "franz-alive"),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
        // A second wiki dir whose `_meta.md` has no frontmatter fence, so
        // `WikiTree::walk` (hence `locate` for every id) returns Err.
        let broken = tree.wikis_dir().join("franz-broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join(crate::wiki::META_FILENAME),
            "no frontmatter here",
        )
        .unwrap();
        assert!(
            tree.walk().is_err(),
            "premise: the broken meta makes walk fail"
        );

        let report = run(&pool, &tree).await.unwrap();
        // The sweep bailed instead of judging `alive` dangling.
        assert_eq!(report.dangling_consumers_removed, 0);
        assert!(consumer_exists(&pool, "alive").await);
        assert_eq!(refresh_rows(&pool, "alive").await, 1);
    }

    #[tokio::test]
    async fn issue_and_rotate_prune_their_own_connection() {
        let (pool, tree, _dir) = fixture().await;
        seed_wiki(&tree, "franz-idle");
        seed_consumer(&pool, "idle", None).await;
        // The inline prune keeps a connection at ≤1 stale row through any
        // number of issue/revoke turns — this is the rotation-accumulation
        // fix exercised end to end via the public API.
        for _ in 0..4 {
            issue_refresh_token(
                &pool,
                &grant("idle", "franz-idle"),
                Duration::from_secs(3600),
            )
            .await
            .unwrap();
            revoke_connection(&pool, "franz", "idle").await.unwrap();
        }
        assert_eq!(refresh_rows(&pool, "idle").await, 1);
    }

    #[tokio::test]
    async fn disconnected_consumer_keeps_one_binding_row() {
        let (pool, tree, _dir) = fixture().await;
        // Wiki exists, but the connection is fully disconnected: stale
        // rows only, none active (seeded directly — the API's inline
        // prune no longer lets them accumulate).
        seed_wiki(&tree, "franz-idle");
        seed_consumer(&pool, "idle", None).await;
        for hash in ["h1", "h2", "h3", "h4"] {
            insert_stale_refresh(&pool, "idle", "franz-idle", hash).await;
        }
        assert_eq!(refresh_rows(&pool, "idle").await, 4);

        let report = run(&pool, &tree).await.unwrap();
        // Three pruned, the newest stale row kept as the wiki binding;
        // the consumer survives (its wiki still exists).
        assert_eq!(report.stale_refresh_pruned, 3);
        assert_eq!(refresh_rows(&pool, "idle").await, 1);
        assert!(consumer_exists(&pool, "idle").await);

        // Once the wiki goes away, the kept binding row routes the
        // consumer into the dangling sweep.
        std::fs::remove_dir_all(tree.wikis_dir().join("franz-idle")).unwrap();
        let report = run(&pool, &tree).await.unwrap();
        assert_eq!(report.dangling_consumers_removed, 1);
        assert!(!consumer_exists(&pool, "idle").await);
        assert_eq!(refresh_rows(&pool, "idle").await, 0);
    }

    #[tokio::test]
    async fn active_connection_drains_all_stale_rows() {
        let (pool, tree, _dir) = fixture().await;
        seed_wiki(&tree, "franz-hot");
        seed_consumer(&pool, "hot", None).await;
        // One active row + one stale leftover for the same connection
        // (seeded directly, as if revoked by a dashboard disconnect that
        // predates the inline prune).
        issue_refresh_token(&pool, &grant("hot", "franz-hot"), Duration::from_secs(3600))
            .await
            .unwrap();
        insert_stale_refresh(&pool, "hot", "franz-hot", "h-old").await;

        let report = run(&pool, &tree).await.unwrap();
        // The active row makes the stale keeper unnecessary.
        assert_eq!(report.stale_refresh_pruned, 1);
        assert_eq!(refresh_rows(&pool, "hot").await, 1);
    }

    #[tokio::test]
    async fn expired_auth_codes_are_purged() {
        let (pool, tree, _dir) = fixture().await;
        sqlx::query(
            "INSERT INTO webagentoauth_codes
                 (code_hash, client_id, redirect_uri, code_challenge, sender_id,
                  consumer_id, wiki_id, created_at, expires_at)
             VALUES ('h', 'cid', 'r', 'c', 'franz', 'x', 'w', ?, '2000-01-01T00:00:00+00:00')",
        )
        .bind(now_iso())
        .execute(&pool)
        .await
        .unwrap();
        let report = run(&pool, &tree).await.unwrap();
        assert_eq!(report.auth_codes_purged, 1);
        assert!(report.stale_refresh_pruned == 0 && report.dangling_consumers_removed == 0);
    }

    #[tokio::test]
    async fn noop_run_reports_noop() {
        let (pool, tree, _dir) = fixture().await;
        let report = run(&pool, &tree).await.unwrap();
        assert!(report.is_noop());
    }
}
