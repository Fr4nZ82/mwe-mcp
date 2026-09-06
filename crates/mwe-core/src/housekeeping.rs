// SPDX-License-Identifier: AGPL-3.0-or-later
//! Runtime housekeeping — the hygiene sweeps the hot paths defer.
//!
//! One entry point, [`run`], invoked at `serve` boot, once a day while the
//! server is up, and after a dashboard wiki deletion. It drains eight
//! kinds of residue that otherwise accumulate unbounded — the first three
//! observed live on the dogfood deployment:
//!
//! - **Expired authorization codes** — a code self-deletes on redemption
//!   (single-use), but an abandoned OAuth flow leaves its row behind
//!   forever. (`issue_auth_code` also purges opportunistically, so a busy
//!   server self-cleans between boots.)
//! - **Stale refresh-token rows** — rotation and fresh connects prune
//!   their own connection inline (see
//!   [`crate::oauth_server::rotate_refresh_token`]), but rows revoked by
//!   a dashboard disconnect or expired while idle have no inline subject.
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
//! - **Expired revocations** — `token_revoke` appends a `jti` to
//!   `token_blacklist` and nothing takes it out again, while
//!   [`crate::jwt::BlacklistCache`] reloads the whole table every 60 s.
//!   A row past its `expires_at` names a token the signature check
//!   already refuses, so keeping it only makes that reload bigger.
//! - **Aged `wiki_events`** — the queue is append-only and every
//!   `events_poll` scans it, so an unswept row costs every consumer on
//!   every poll for ever.
//!
//! The last three are the operator's [retention
//! windows](crate::config::RetentionConfig), which is why they take their
//! ages from the config rather than from a constant here:
//!
//! - **Aged `tool_executions`** — one row per tool call for ever,
//!   answering a question about the recent past.
//! - **Spent undo images** — the page bodies a push kept so it could be
//!   undone. The row stays and only `pre_image_json` goes, which is the
//!   value the op-log already reads as "no revert possible from here", so
//!   the window on the images *is* the undo window.
//! - **Aged trash** — the wiki subtrees a deletion moved to
//!   `<workdir>/trash/` instead of erasing. Full copies of a memory, in
//!   cleartext, that nobody is coming back for.

use std::collections::{BTreeMap, HashSet};

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::config::RetentionConfig;
use crate::error::Result;
use crate::events::EventKind;
use crate::oauth_server::now_iso;
use crate::reminders::ALREADY_RUNG_DAYS;
use crate::types::WikiId;
use crate::wiki::WikiTree;

/// Age past which a `wiki_events` row is deleted.
///
/// Thirty days is what the queue's readers need. A consumer drains it
/// continuously ([`crate::events::poll_events`]), so a row a month old is
/// one nobody came for, and it is scanned on every poll from then on. The
/// one reader that looks further back is honoured where the sweep runs —
/// see [`purge_aged_events`].
const EVENT_RETENTION_DAYS: i64 = 30;

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
    /// `token_blacklist` rows dropped because the token they revoke has
    /// expired on its own.
    pub expired_revocations_purged: u64,
    /// `wiki_events` rows dropped past their retention.
    pub events_purged: u64,
    /// `tool_executions` rows dropped past `retention.audit_days`.
    pub audit_rows_purged: u64,
    /// `wiki_admin_op_log` rows whose undo image was dropped past
    /// `retention.undo_days`. The rows themselves stay.
    pub undo_images_dropped: u64,
    /// Directories removed from `<workdir>/trash/` past
    /// `retention.trash_days`.
    pub trash_dirs_removed: u64,
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
pub async fn run(
    pool: &SqlitePool,
    tree: &WikiTree,
    retention: &RetentionConfig,
) -> Result<HousekeepingReport> {
    let now = now_iso();
    let clock = chrono::Utc::now();
    let auth_codes_purged = purge_expired_auth_codes(pool, &now).await?;
    let (dangling_consumers_removed, delegations_removed) =
        sweep_dangling_consumers(pool, tree).await?;
    let stale_refresh_pruned = prune_stale_refresh_rows(pool, &now).await?;
    let expired_revocations_purged = purge_expired_revocations(pool, &now).await?;
    let events_purged = purge_aged_events(pool, clock).await?;
    let audit_rows_purged = purge_aged_audit(pool, clock, retention.audit_days).await?;
    let undo_images_dropped = drop_spent_undo_images(pool, clock, retention.undo_days).await?;
    let trash_dirs_removed = purge_aged_trash(tree, clock, retention.trash_days);
    Ok(HousekeepingReport {
        auth_codes_purged,
        stale_refresh_pruned,
        dangling_consumers_removed,
        delegations_removed,
        expired_revocations_purged,
        events_purged,
        audit_rows_purged,
        undo_images_dropped,
        trash_dirs_removed,
    })
}

/// The cutoff `days` before `now`, or `None` when the window is `0` —
/// which every retention window reads as *keep for ever*.
fn cutoff(now: chrono::DateTime<chrono::Utc>, days: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    (days > 0).then(|| now - chrono::Duration::days(days))
}

/// Delete audit rows past `retention.audit_days`.
///
/// The trail answers *who called what, when, and did it fail* — a
/// question about the recent past, one row per tool call. It is not the
/// memory: nothing recalls from it, and the facts a call filed are in
/// `fact_index` either way.
async fn purge_aged_audit(
    pool: &SqlitePool,
    now: chrono::DateTime<chrono::Utc>,
    days: i64,
) -> Result<u64> {
    let Some(cutoff) = cutoff(now, days) else {
        return Ok(0);
    };
    let res = sqlx::query("DELETE FROM tool_executions WHERE timestamp < ?")
        .bind(cutoff.to_rfc3339())
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Drop the undo image of pushes older than `retention.undo_days`,
/// keeping the row.
///
/// `pre_image_json` holds every page body a push overwrote, so the log is
/// a second copy of the memory in proportion to how much has been pushed.
/// `NULL` is not a hole in the row: it is what the op-log surface already
/// reads as "no revert possible from here", and it is what a `system`
/// revert row has carried since the column existed — so the window on the
/// images is exactly the window in which an undo is offered.
async fn drop_spent_undo_images(
    pool: &SqlitePool,
    now: chrono::DateTime<chrono::Utc>,
    days: i64,
) -> Result<u64> {
    let Some(cutoff) = cutoff(now, days) else {
        return Ok(0);
    };
    let res = sqlx::query(
        "UPDATE wiki_admin_op_log SET pre_image_json = NULL
          WHERE pre_image_json IS NOT NULL AND ts < ?",
    )
    .bind(cutoff.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Remove the wiki subtrees in `<workdir>/trash/` deleted more than
/// `retention.trash_days` ago.
///
/// **When the deletion happened is read from the directory's name**, not
/// from its timestamps: moving a directory into the trash does not touch
/// its own mtime, so a wiki last written to a year ago would look a year
/// old the moment it was deleted. `wiki_delete` names each one
/// `<wiki-id>__<YYYYMMDD>T<HHMMSS>Z`, and that suffix is the record of
/// the moment.
///
/// Anything in `trash/` whose name does not carry a stamp the engine
/// wrote is left where it is, for ever. It is somebody's own file, put
/// there by hand, and this sweep is not entitled to it.
///
/// Best-effort by design: an unreadable trash directory, or a removal
/// that fails, is logged and skipped — never an error that aborts the
/// rest of the housekeeping run.
fn purge_aged_trash(tree: &WikiTree, now: chrono::DateTime<chrono::Utc>, days: i64) -> u64 {
    let Some(cutoff) = cutoff(now, days) else {
        return 0;
    };
    let trash_root = tree.workdir().join("trash");
    let entries = match std::fs::read_dir(&trash_root) {
        Ok(e) => e,
        // No trash directory is the normal case: nothing has been deleted.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            warn!(%error, path = %trash_root.display(), "housekeeping: trash unreadable; skipped");
            return 0;
        },
    };

    let mut removed = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(deleted_at) = deletion_stamp(name) else {
            continue;
        };
        if deleted_at >= cutoff {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                info!(
                    path = %path.display(),
                    deleted_at = %deleted_at.to_rfc3339(),
                    retention_days = days,
                    "housekeeping: trashed wiki subtree removed past its retention"
                );
            },
            Err(error) => {
                warn!(%error, path = %path.display(), "housekeeping: trash removal failed");
            },
        }
    }
    removed
}

/// The moment a trashed directory was deleted, read from the
/// `<wiki-id>__<YYYYMMDD>T<HHMMSS>Z` name `wiki_delete` writes.
///
/// `None` for any other name — including a wiki id that itself contains
/// `__`, since the stamp is taken from the **last** separator.
fn deletion_stamp(name: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (_, stamp) = name.rsplit_once("__")?;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Delete authorization codes past their expiry.
async fn purge_expired_auth_codes(pool: &SqlitePool, now: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM webagentoauth_codes WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Delete revocations whose token has expired anyway.
///
/// The blacklist answers one question — "was this live token revoked?" —
/// and a token past its `exp` fails the signature check before the
/// blacklist is ever consulted, so its row can only make
/// [`crate::jwt::BlacklistCache`]'s 60-second reload larger.
async fn purge_expired_revocations(pool: &SqlitePool, now: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM token_blacklist WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Delete `wiki_events` rows past [`EVENT_RETENTION_DAYS`], except
/// `reminder_due` rows, which are kept for the whole horizon their own
/// idempotence probe reads ([`ALREADY_RUNG_DAYS`]).
///
/// The exception is the rule, not a special case: a row may go once
/// nothing can still read it, and the reminder sweep reads a year back to
/// decide whether a dated commitment already rang. Deleting its row sooner
/// would let the same commitment ring twice.
///
/// The cost of the retention is a consumer that has been away longer than
/// the window: notices it never polled are gone rather than waiting.
async fn purge_aged_events(pool: &SqlitePool, now: chrono::DateTime<chrono::Utc>) -> Result<u64> {
    let cutoff = (now - chrono::Duration::days(EVENT_RETENTION_DAYS)).to_rfc3339();
    let reminder_cutoff = (now - chrono::Duration::days(ALREADY_RUNG_DAYS)).to_rfc3339();
    let res = sqlx::query(
        "DELETE FROM wiki_events
          WHERE created_at < ?
            AND (kind <> ? OR created_at < ?)",
    )
    .bind(&cutoff)
    .bind(EventKind::ReminderDue.as_str())
    .bind(&reminder_cutoff)
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

    /// Insert a stale (already revoked) refresh row directly: the public API
    /// cannot accumulate one, because its inline prune removes them.
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

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
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

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
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

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
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
        // rows only, none active — seeded directly, because the API's inline
        // prune does not let them accumulate.
        seed_wiki(&tree, "franz-idle");
        seed_consumer(&pool, "idle", None).await;
        for hash in ["h1", "h2", "h3", "h4"] {
            insert_stale_refresh(&pool, "idle", "franz-idle", hash).await;
        }
        assert_eq!(refresh_rows(&pool, "idle").await, 4);

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
        // Three pruned, the newest stale row kept as the wiki binding;
        // the consumer survives (its wiki still exists).
        assert_eq!(report.stale_refresh_pruned, 3);
        assert_eq!(refresh_rows(&pool, "idle").await, 1);
        assert!(consumer_exists(&pool, "idle").await);

        // Once the wiki goes away, the kept binding row routes the
        // consumer into the dangling sweep.
        std::fs::remove_dir_all(tree.wikis_dir().join("franz-idle")).unwrap();
        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
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

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
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
        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
        assert_eq!(report.auth_codes_purged, 1);
        assert!(report.stale_refresh_pruned == 0 && report.dangling_consumers_removed == 0);
    }

    #[tokio::test]
    async fn noop_run_reports_noop() {
        let (pool, tree, _dir) = fixture().await;
        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
        assert!(report.is_noop());
    }

    /// A revoked token whose own `exp` has passed is refused by the
    /// signature check, so its row buys nothing and goes; a revocation
    /// still covering a live token stays.
    #[tokio::test]
    async fn expired_revocations_are_purged_and_live_ones_kept() {
        let (pool, tree, _dir) = fixture().await;
        for (jti, expires) in [
            ("dead", "2000-01-01T00:00:00+00:00"),
            ("live", "2999-01-01T00:00:00+00:00"),
        ] {
            sqlx::query(
                "INSERT INTO token_blacklist (jti, revoked_at, expires_at, reason)
                 VALUES (?, ?, ?, 'test')",
            )
            .bind(jti)
            .bind(now_iso())
            .bind(expires)
            .execute(&pool)
            .await
            .unwrap();
        }

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
        assert_eq!(report.expired_revocations_purged, 1);
        let left: Vec<String> = sqlx::query_scalar("SELECT jti FROM token_blacklist")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["live".to_owned()]);
    }

    /// Insert one audit row `age_days` old.
    async fn insert_audit(pool: &SqlitePool, tool: &str, age_days: i64) {
        let ts = (chrono::Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        sqlx::query(
            "INSERT INTO tool_executions
                 (timestamp, tool_name, sender_id, device_label, latency_ms)
             VALUES (?, ?, 'alice', 'mcp', 1)",
        )
        .bind(ts)
        .bind(tool)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn audit_tools(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT tool_name FROM tool_executions ORDER BY tool_name")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// The audit trail is kept for `retention.audit_days` and no longer.
    #[tokio::test]
    async fn audit_rows_past_the_window_go_and_the_rest_stay() {
        let (pool, tree, _dir) = fixture().await;
        insert_audit(&pool, "recent", 1).await;
        insert_audit(&pool, "aged", 91).await;

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(report.audit_rows_purged, 1);
        assert_eq!(audit_tools(&pool).await, vec!["recent".to_owned()]);
    }

    /// `0` is the operator saying "keep everything", not "keep nothing".
    #[tokio::test]
    async fn a_zero_window_keeps_the_whole_audit_trail() {
        let (pool, tree, _dir) = fixture().await;
        insert_audit(&pool, "ancient", 4_000).await;

        let retention = RetentionConfig {
            audit_days: 0,
            ..RetentionConfig::default()
        };
        let report = run(&pool, &tree, &retention).await.unwrap();

        assert_eq!(report.audit_rows_purged, 0);
        assert_eq!(audit_tools(&pool).await, vec!["ancient".to_owned()]);
    }

    /// Insert one push row `age_days` old, carrying an undo image.
    async fn insert_push(pool: &SqlitePool, wiki: &str, age_days: i64) {
        let ts = (chrono::Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        sqlx::query(
            "INSERT INTO wiki_admin_op_log
                 (wiki_id, sender_id, op_kind, payload_hash, pages_affected, ts,
                  actor_kind, pre_image_json)
             VALUES (?, 'alice', 'push_upsert', 'h', 1, ?, 'smart_consumer',
                     '{\"pages\":[]}')",
        )
        .bind(wiki)
        .bind(ts)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The undo image is what ages out; the row that says who pushed what
    /// stays, because the audit and the undo are two different questions.
    #[tokio::test]
    async fn a_spent_undo_image_goes_and_its_row_stays() {
        let (pool, tree, _dir) = fixture().await;
        insert_push(&pool, "alice", 3).await;
        insert_push(&pool, "bob", 31).await;

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(report.undo_images_dropped, 1);
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT wiki_id, pre_image_json FROM wiki_admin_op_log ORDER BY wiki_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "both pushes are still in the log");
        assert_eq!(rows[0].0, "alice");
        assert!(rows[0].1.is_some(), "the recent push can still be undone");
        assert_eq!(rows[1].0, "bob");
        assert!(rows[1].1.is_none(), "the old one cannot, and says so");
    }

    /// Put a directory in the trash as `wiki_delete` names it.
    fn seed_trash(tree: &WikiTree, name: &str) -> std::path::PathBuf {
        let dir = tree.workdir().join("trash").join(name);
        std::fs::create_dir_all(dir.join("pages")).unwrap();
        std::fs::write(dir.join("pages").join("a.md"), "body").unwrap();
        dir
    }

    /// A deleted wiki waits in the trash for the window and is then gone
    /// — and the moment of the deletion is read from the name, because
    /// moving a directory does not touch its own mtime.
    #[tokio::test]
    async fn trash_past_the_window_is_removed_and_the_rest_is_left_alone() {
        let (pool, tree, _dir) = fixture().await;
        let stamp = |age_days: i64| {
            (chrono::Utc::now() - chrono::Duration::days(age_days))
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        };
        let recent = seed_trash(&tree, &format!("franz-recent__{}", stamp(2)));
        let aged = seed_trash(&tree, &format!("franz-aged__{}", stamp(40)));
        // Not ours: no stamp the engine wrote, so no claim on it — whatever
        // an operator put in `trash/` by hand stays there for ever.
        let by_hand = seed_trash(&tree, "notes-i-moved-here");

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(report.trash_dirs_removed, 1);
        assert!(!aged.exists(), "the aged subtree is gone");
        assert!(recent.exists(), "the recent one is still recoverable");
        assert!(by_hand.exists(), "an unstamped directory is never swept");
    }

    /// The queue is swept at [`EVENT_RETENTION_DAYS`], and a `reminder_due`
    /// row outlives it: the reminder sweep reads a year back to decide
    /// whether a commitment already rang.
    #[tokio::test]
    async fn aged_events_are_purged_and_reminders_kept_for_their_probe() {
        let (pool, tree, _dir) = fixture().await;
        let now = chrono::Utc::now();
        let insert = |kind: EventKind, age_days: i64| {
            let created = (now - chrono::Duration::days(age_days)).to_rfc3339();
            sqlx::query(
                "INSERT INTO wiki_events (kind, wiki_id, fact_id, payload, created_at)
                 VALUES (?, 'alice', ?, '{}', ?)",
            )
            .bind(kind.as_str())
            .bind(format!("{}-{age_days}d", kind.as_str()))
            .bind(created)
            .execute(&pool)
        };
        insert(EventKind::StructureApplied, 1).await.unwrap();
        insert(EventKind::StructureApplied, 31).await.unwrap();
        insert(EventKind::ReminderDue, 31).await.unwrap();
        insert(EventKind::ReminderDue, ALREADY_RUNG_DAYS + 1)
            .await
            .unwrap();

        let report = run(&pool, &tree, &RetentionConfig::default())
            .await
            .unwrap();
        assert_eq!(report.events_purged, 2);
        let mut left: Vec<String> = sqlx::query_scalar("SELECT fact_id FROM wiki_events")
            .fetch_all(&pool)
            .await
            .unwrap();
        left.sort();
        assert_eq!(
            left,
            vec![
                "reminder_due-31d".to_owned(),
                "structure_applied-1d".to_owned()
            ]
        );
    }
}
