// SPDX-License-Identifier: AGPL-3.0-or-later
//! Automatic-snapshot scheduler — the `backup:` config section's
//! runtime (roadmap 4d).
//!
//! A due-check loop, not a fire-on-tick interval like the REM
//! scheduler: every [`CHECK_INTERVAL_SECS`] it reads the shared
//! [`BackupSchedule`] handle (hot-swapped by the dashboard Backup
//! console) and compares "now" against the last-run stamp persisted in
//! `engine_meta` ([`mwe_core::backup::META_LAST_AUTO_UNIX`]). This
//! makes every knob hot-reloadable and — because the stamp lives in
//! the DB, not in process memory — a restart never re-fires a snapshot
//! that already happened inside the interval.
//!
//! One task, always spawned (a disabled schedule just idles the
//! due-check): the operator can enable automatic snapshots from the
//! dashboard without a restart. After each successful snapshot the
//! oldest `auto-*` snapshots beyond the retention are pruned; manual
//! and safety snapshots are never touched. A failed snapshot advances
//! the stamp too — one loud failure per interval beats hammering a
//! full `VACUUM INTO` against a broken destination every five minutes
//! — and the outcome lands in `engine_meta` either way for the console
//! status line ([`mwe_core::backup::META_LAST_AUTO_REPORT`]).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use mwe_core::backup::{
    self, AutoSnapshotReport, BackupSchedule, META_LAST_AUTO_REPORT, META_LAST_AUTO_UNIX,
};
use mwe_core::db;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Cadence of the due-check (cheap: one `engine_meta` read when the
/// schedule is enabled, nothing otherwise).
const CHECK_INTERVAL_SECS: u64 = 300;

/// Spawn the due-check loop.
///
/// `initial_delay_secs` comes from the boot config
/// (`backup.initial_delay_secs`); everything else is read from
/// `schedule` fresh at each check, so a console save applies without a
/// restart. `None` inside the lock (identity-only builds) idles.
#[must_use]
pub fn spawn<S>(
    initial_delay_secs: u64,
    schedule: Arc<RwLock<Option<BackupSchedule>>>,
    pool: SqlitePool,
    workdir: PathBuf,
    shutdown: S,
) -> JoinHandle<()>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    info!(
        initial_delay_secs,
        check_interval_secs = CHECK_INTERVAL_SECS,
        "backup scheduler: armed (due-check loop)"
    );
    tokio::spawn(async move {
        tokio::pin!(shutdown);

        let initial = tokio::time::sleep(std::time::Duration::from_secs(initial_delay_secs));
        tokio::pin!(initial);
        tokio::select! {
            () = &mut shutdown => {
                info!("backup scheduler: shutdown before first due-check");
                return;
            },
            () = &mut initial => {},
        }

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(CHECK_INTERVAL_SECS));
        // A suspended host wants one check on resume, not a burst.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("backup scheduler: shutdown signal received, exiting loop");
                    return;
                },
                _ = ticker.tick() => {
                    check_and_run(&schedule, &pool, &workdir).await;
                }
            }
        }
    })
}

/// Owned snapshot of the shared schedule handle.
fn snapshot_schedule(schedule: &Arc<RwLock<Option<BackupSchedule>>>) -> Option<BackupSchedule> {
    schedule
        .read()
        .expect("backup schedule rwlock poisoned")
        .clone()
}

/// Seconds since the unix epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

async fn check_and_run(
    schedule: &Arc<RwLock<Option<BackupSchedule>>>,
    pool: &SqlitePool,
    workdir: &std::path::Path,
) {
    let Some(sched) = snapshot_schedule(schedule) else {
        return;
    };
    if !sched.enabled {
        return;
    }
    let now = now_unix();
    let last: u64 = match db::meta_get(pool, META_LAST_AUTO_UNIX).await {
        Ok(v) => v.and_then(|s| s.parse().ok()).unwrap_or(0),
        Err(e) => {
            warn!(error = %e, "backup scheduler: due-check read failed, skipping");
            return;
        },
    };
    if now < last.saturating_add(sched.interval_secs) {
        return;
    }

    let dest = sched.dir.join(backup::snapshot_name("auto"));
    let report = match backup::snapshot_workdir(workdir, &dest).await {
        Ok(r) => {
            info!(
                dest = %dest.display(),
                db_bytes = r.db_bytes,
                files_copied = r.files_copied,
                skipped = r.skipped.len(),
                "backup scheduler: automatic snapshot complete"
            );
            // A snapshot that completed *minus something* is not the
            // same event as a clean one, and the operator learns it here
            // or nowhere: say it in the log and carry it into the report
            // the console renders.
            let skipped = if r.skipped.is_empty() {
                String::new()
            } else {
                let names: Vec<String> =
                    r.skipped.iter().map(|p| p.display().to_string()).collect();
                warn!(
                    dest = %dest.display(),
                    skipped = %names.join(", "),
                    "backup scheduler: snapshot complete but entries were UNREADABLE and left out"
                );
                format!(
                    " — {} entr{} left out (unreadable): {}",
                    names.len(),
                    if names.len() == 1 { "y" } else { "ies" },
                    names.join(", ")
                )
            };
            AutoSnapshotReport {
                ok: true,
                at: chrono::Utc::now().to_rfc3339(),
                dest: dest.display().to_string(),
                detail: format!(
                    "{} files, {} DB bytes{skipped}",
                    r.files_copied + 1,
                    r.db_bytes
                ),
            }
        },
        Err(e) => {
            warn!(dest = %dest.display(), error = %e, "backup scheduler: automatic snapshot FAILED");
            AutoSnapshotReport {
                ok: false,
                at: chrono::Utc::now().to_rfc3339(),
                dest: dest.display().to_string(),
                detail: e.to_string(),
            }
        },
    };

    // Stamp the attempt (success or failure — see module docs), then
    // persist the outcome for the console.
    if let Err(e) = db::meta_set(pool, META_LAST_AUTO_UNIX, &now.to_string()).await {
        warn!(error = %e, "backup scheduler: last-run stamp not persisted");
    }
    match serde_json::to_string(&report) {
        Ok(json) => {
            if let Err(e) = db::meta_set(pool, META_LAST_AUTO_REPORT, &json).await {
                warn!(error = %e, "backup scheduler: report not persisted");
            }
        },
        Err(e) => warn!(error = %e, "backup scheduler: report not serialized"),
    }

    if report.ok {
        match backup::prune_auto_snapshots(&sched.dir, sched.retention_auto) {
            Ok(removed) if removed.is_empty() => {},
            Ok(removed) => info!(
                removed = removed.len(),
                oldest = %removed.last().map(String::as_str).unwrap_or_default(),
                "backup scheduler: pruned automatic snapshots beyond retention"
            ),
            Err(e) => warn!(error = %e, "backup scheduler: retention prune failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The due-check fires when the stamp is stale, persists stamp +
    /// report, and prunes beyond retention; a fresh stamp idles it.
    #[tokio::test]
    async fn due_check_fires_stamps_and_prunes() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(work.path()).await.unwrap();
        std::fs::write(work.path().join("mwe-mcp.env"), "S=1\n").unwrap();

        // Two pre-existing autos; retention 2 ⇒ one prune after the run.
        for name in ["auto-a", "auto-b"] {
            backup::snapshot_workdir(work.path(), &home.path().join(name))
                .await
                .unwrap();
        }
        let schedule = Arc::new(RwLock::new(Some(BackupSchedule {
            enabled: true,
            interval_secs: 3600,
            dir: home.path().to_path_buf(),
            retention_auto: 2,
        })));

        check_and_run(&schedule, &pool, work.path()).await;

        let stamp = db::meta_get(&pool, META_LAST_AUTO_UNIX)
            .await
            .unwrap()
            .expect("stamp persisted");
        assert!(stamp.parse::<u64>().unwrap() > 0);
        let report: AutoSnapshotReport = serde_json::from_str(
            &db::meta_get(&pool, META_LAST_AUTO_REPORT)
                .await
                .unwrap()
                .expect("report persisted"),
        )
        .unwrap();
        assert!(report.ok, "{}", report.detail);
        let autos = backup::list_snapshots(home.path())
            .unwrap()
            .into_iter()
            .filter(|s| s.kind == backup::SnapshotKind::Auto)
            .count();
        assert_eq!(autos, 2, "3 after the run, pruned back to retention");

        // Fresh stamp ⇒ the next check is a no-op (no new snapshot).
        check_and_run(&schedule, &pool, work.path()).await;
        let autos_after = backup::list_snapshots(home.path())
            .unwrap()
            .into_iter()
            .filter(|s| s.kind == backup::SnapshotKind::Auto)
            .count();
        assert_eq!(autos_after, 2, "inside the interval: idle");
    }

    #[tokio::test]
    async fn disabled_or_absent_schedule_idles() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(work.path()).await.unwrap();

        let absent: Arc<RwLock<Option<BackupSchedule>>> = Arc::new(RwLock::new(None));
        check_and_run(&absent, &pool, work.path()).await;

        let disabled = Arc::new(RwLock::new(Some(BackupSchedule {
            enabled: false,
            interval_secs: 1,
            dir: home.path().to_path_buf(),
            retention_auto: 7,
        })));
        check_and_run(&disabled, &pool, work.path()).await;

        assert!(
            db::meta_get(&pool, META_LAST_AUTO_UNIX)
                .await
                .unwrap()
                .is_none(),
            "no run, no stamp"
        );
        assert!(backup::list_snapshots(home.path()).unwrap().is_empty());
    }
}
