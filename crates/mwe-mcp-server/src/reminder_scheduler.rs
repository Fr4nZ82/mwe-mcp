// SPDX-License-Identifier: AGPL-3.0-or-later
//! Due-commitment sweep — the runtime of the `reminders:` config section
//! (roadmap 8d).
//!
//! A cheap tick, not a cron: every [`CHECK_INTERVAL_SECS`] it asks
//! [`mwe_core::reminders::sweep`] whether any dated commitment the memory
//! holds has just come round, and emits one `reminder_due` notice per
//! commitment on the reverse channel. Delivery is the consumer's job — the
//! same poll it already runs for `fact_minted_for_you`.
//!
//! **Not a scheduler for the user's own alarms.** "Wake me at seven",
//! "post to the group every Thursday" belong to the assistant that was
//! asked; this loop fires only for facts already stored here. The reason
//! this one case cannot stay in the assistant's own scheduler is that only
//! the memory learns the appointment moved — a job written when the user
//! first asked freezes both the time and the wording.
//!
//! Idempotence and the catch-up bound both live in `mwe_core::reminders`
//! (a `(kind, fact_id)` probe, and a grace window that stops a first run
//! announcing the whole backlog), so this module is only the clock.
//!
//! The policy is read **once, at spawn**. Unlike the backup schedule there
//! is no console editing it, so there is nothing to hot-swap: an edit to
//! `reminders:` takes effect on the next restart, and this says so rather
//! than pretending otherwise.

use mwe_core::reminders::ReminderPolicy;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Cadence of the due-check. A commitment announces itself within this
/// much of its hour, plus the consumer's own poll interval (~30 s), which
/// is the resolution a reminder actually needs.
const CHECK_INTERVAL_SECS: u64 = 60;

/// Delay before the first sweep, so boot work settles first.
const BOOT_DELAY_SECS: u64 = 45;

/// Spawn the due-commitment loop.
///
/// Returns `None` when the section is disabled — no task, no tick.
pub fn spawn<S>(policy: ReminderPolicy, pool: SqlitePool, shutdown: S) -> Option<JoinHandle<()>>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    if !policy.enabled {
        info!("reminder scheduler: disabled by config");
        return None;
    }
    info!(
        check_interval_secs = CHECK_INTERVAL_SECS,
        day_hour_utc = policy.day_hour_utc,
        "reminder scheduler: armed (due-commitment sweep)"
    );
    Some(tokio::spawn(async move {
        tokio::pin!(shutdown);

        let boot = tokio::time::sleep(std::time::Duration::from_secs(BOOT_DELAY_SECS));
        tokio::pin!(boot);
        tokio::select! {
            () = &mut boot => {},
            () = &mut shutdown => return,
        }

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(CHECK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {},
                () = &mut shutdown => return,
            }

            match mwe_core::reminders::sweep(&pool, chrono::Utc::now(), &policy).await {
                Ok(report) if !report.emitted.is_empty() => {
                    info!(
                        emitted = report.emitted.len(),
                        already_rung = report.already_rung,
                        "reminder scheduler: commitments announced"
                    );
                },
                Ok(_) => {},
                Err(e) => {
                    warn!(error = %e, "reminder scheduler: sweep failed (retried next tick)");
                },
            }
        }
    }))
}
