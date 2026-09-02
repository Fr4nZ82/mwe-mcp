// SPDX-License-Identifier: AGPL-3.0-or-later
//! Daily hygiene sweep — the clock for [`mwe_core::housekeeping::run`].
//!
//! `serve` sweeps once inline at boot. The residue that sweep drains —
//! abandoned authorization codes, stale refresh rows, web-agent consumers
//! whose smart wiki was deleted, revocations of tokens that have expired
//! anyway, aged `wiki_events` — accumulates while the server *runs*, so a
//! process that stays up for months is exactly the one that needs it most.
//! This loop gives it a tick.
//!
//! One tick a day, and no config section: nothing the sweep removes is
//! urgent — every row is already inert by the time it qualifies — while
//! each run walks the whole wiki tree, so a shorter cadence would cost
//! more than it buys. It is armed on a frozen instance too: the sweep
//! takes residue, never memory, which is why the boot sweep runs there
//! as well.

use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Distance between sweeps.
const INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Spawn the daily sweep loop.
///
/// The first sweep lands one full interval after boot: `serve` has just
/// run one inline, and repeating it immediately would only re-read an
/// empty result.
#[must_use]
pub fn spawn<S>(pool: SqlitePool, tree: WikiTree, shutdown: S) -> JoinHandle<()>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    info!(
        interval_secs = INTERVAL_SECS,
        "housekeeping scheduler: armed (daily sweep)"
    );
    tokio::spawn(async move {
        tokio::pin!(shutdown);

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(INTERVAL_SECS));
        // The first tick of a tokio interval fires immediately; the boot
        // sweep is that tick, so consume it here.
        ticker.tick().await;
        // A suspended host wants one sweep on resume, not a burst.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("housekeeping scheduler: shutdown signal received, exiting loop");
                    return;
                },
                _ = ticker.tick() => sweep(&pool, &tree).await,
            }
        }
    })
}

/// One sweep, logged the same way the boot sweep is.
async fn sweep(pool: &SqlitePool, tree: &WikiTree) {
    match mwe_core::housekeeping::run(pool, tree).await {
        Ok(report) if report.is_noop() => {},
        Ok(report) => info!(
            auth_codes_purged = report.auth_codes_purged,
            stale_refresh_pruned = report.stale_refresh_pruned,
            dangling_consumers_removed = report.dangling_consumers_removed,
            delegations_removed = report.delegations_removed,
            expired_revocations_purged = report.expired_revocations_purged,
            events_purged = report.events_purged,
            "daily housekeeping: swept"
        ),
        Err(error) => warn!(%error, "daily housekeeping failed; retried tomorrow"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep the ticker drives is the same one the boot path runs, so
    /// residue left behind while the server was up is gone by morning.
    #[tokio::test]
    async fn sweep_drains_residue() {
        let dir = tempfile::tempdir().unwrap();
        let pool = mwe_core::db::open_or_init(dir.path()).await.unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        sqlx::query(
            "INSERT INTO token_blacklist (jti, revoked_at, expires_at, reason)
             VALUES ('dead', '2000-01-01T00:00:00+00:00', '2000-01-02T00:00:00+00:00', 'test')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sweep(&pool, &tree).await;

        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_blacklist")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
