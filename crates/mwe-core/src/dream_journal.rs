// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable history of dream runs — the journal behind the admin Dream console.
//!
//! A "dream" is one of the three memory-consolidation compositions in
//! [`crate::dream`] (the cheap `light` promotion, the `compile` narrative pass,
//! and the `full` nightly REM — see the
//! REM cycle design note). Both the
//! dashboard (operator-triggered runs) and the server's interval scheduler
//! (nightly / interval runs) write a row here when a run finishes, so the admin
//! Dream page can show a history and open each run's full log.
//!
//! The journal is bounded: [`record_run`] prunes to the newest [`MAX_HISTORY`]
//! rows after every insert. That is a resource cap, not a semantic gate — the
//! caller decides *which* runs are worth recording (a scheduled light tick that
//! scanned nothing is skipped upstream so the history stays meaningful).
//!
//! Storage lives in the `dream_runs` table (migration `0054`; the
//! `pages_failed` / `pages_degraded` counts are migration `0055`). The DB is the
//! single home for the history: the scheduler and the dashboard run in different
//! layers and share only the `SQLite` pool, so a DB row is the one place both
//! can write and the only one that survives a restart.
//!
//! A run that **completed** but left pages failed or degraded is not plain ok:
//! the structured `pages_failed` / `pages_degraded` counts (fed from the
//! compile report via [`crate::dream::journal_counts`]) let the console mark
//! the row, and the summary string carries the same counts for the existing
//! rendering.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// How many runs the journal keeps. Older rows are pruned after each insert.
pub const MAX_HISTORY: i64 = 100;

/// Which composition a recorded run was. Stored as [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamKind {
    /// Cheap, frequent promotion-then-compile ([`crate::dream::run_light`]).
    Light,
    /// Narrative compile only ([`crate::dream::run_compile`]).
    Compile,
    /// Full nightly / on-demand REM ([`crate::dream::run_full`]).
    Full,
}

impl DreamKind {
    /// The DB token (`'light'` | `'compile'` | `'full'`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Compile => "compile",
            Self::Full => "full",
        }
    }

    /// Human label for the console (matches the on-demand console buttons).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Compile => "Compile",
            Self::Full => "Full REM",
        }
    }

    /// Parse a stored token. Unknown values fall back to [`Self::Full`] — we
    /// control every write, so this is unreachable in practice and only keeps
    /// the read path total.
    #[must_use]
    fn from_db(s: &str) -> Self {
        match s {
            "light" => Self::Light,
            "compile" => Self::Compile,
            _ => Self::Full,
        }
    }
}

/// What triggered a recorded run. Stored as [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamTrigger {
    /// An operator clicked a button on the Dream console.
    Manual,
    /// The server's interval / nightly scheduler.
    Scheduled,
}

impl DreamTrigger {
    /// The DB token (`'manual'` | `'scheduled'`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
        }
    }

    /// Human label for the console.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
        }
    }

    /// Parse a stored token. Unknown values fall back to [`Self::Scheduled`]
    /// (the read path stays total; writes are always one of the two tokens).
    #[must_use]
    fn from_db(s: &str) -> Self {
        match s {
            "manual" => Self::Manual,
            _ => Self::Scheduled,
        }
    }
}

/// One recorded dream run, as read back for the console.
#[derive(Debug, Clone)]
pub struct DreamRun {
    /// Monotonic row id — also the fetch key for the per-row log modal.
    pub id: i64,
    /// Which composition ran.
    pub kind: DreamKind,
    /// What triggered it.
    pub trigger: DreamTrigger,
    /// `true` when the run completed; `false` when it errored.
    pub ok: bool,
    /// Pages whose compile soft-failed during the run (0 when the run itself
    /// errored before compiling — the `ok` flag covers that case). A nonzero
    /// count on an `ok` run marks it degraded-not-clean.
    pub pages_failed: i64,
    /// Pages the compiler rewrote in degraded mode (guard-only append).
    pub pages_degraded: i64,
    /// One-line outcome for the table row (or the error message when `!ok`).
    pub summary: String,
    /// Full report dump for the modal (or the error detail when `!ok`).
    pub log_text: String,
    /// RFC-3339 start stamp.
    pub started_at: String,
    /// RFC-3339 completion stamp.
    pub finished_at: String,
}

impl DreamRun {
    /// `true` when the run completed but left pages failed or degraded — the
    /// "completed, but not clean" state the console badges as a warning.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        self.ok && (self.pages_failed > 0 || self.pages_degraded > 0)
    }
}

/// The raw `dream_runs` row, decoded before the enum tokens are parsed.
#[derive(sqlx::FromRow)]
struct RawRow {
    id: i64,
    kind: String,
    trigger_source: String,
    ok: bool,
    pages_failed: i64,
    pages_degraded: i64,
    summary: String,
    log_text: String,
    started_at: String,
    finished_at: String,
}

impl From<RawRow> for DreamRun {
    fn from(r: RawRow) -> Self {
        Self {
            id: r.id,
            kind: DreamKind::from_db(&r.kind),
            trigger: DreamTrigger::from_db(&r.trigger_source),
            ok: r.ok,
            pages_failed: r.pages_failed,
            pages_degraded: r.pages_degraded,
            summary: r.summary,
            log_text: r.log_text,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

/// `chrono::Utc::now()` as an RFC-3339 string.
///
/// The stamp shape the journal stores. Exposed so both the dashboard and the
/// server scheduler stamp runs the same way without each pulling in `chrono`.
#[must_use]
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Record one finished dream run, then prune the journal to [`MAX_HISTORY`].
///
/// Both timestamps are caller-supplied (RFC-3339): `started_at` is captured
/// before the run, `finished_at` after it. `summary` is the one-line shown in
/// the table; `log_text` is the full report dump shown in the modal. On failure
/// the caller passes `ok = false` with the error message as `summary` and the
/// error detail as `log_text` (and `(0, 0)` counts — nothing compiled).
/// `pages_failed` / `pages_degraded` are the run's per-page compile outcome
/// counts ([`crate::dream::journal_counts`]) — nonzero on an `ok` run marks it
/// completed-but-not-clean.
///
/// Recording is best-effort telemetry: a journal write must never fail or mask
/// the dream itself, so callers log-and-ignore the error rather than propagating
/// it.
///
/// # Errors
///
/// Surfaces the insert / prune SQL failure so the caller can log it.
#[allow(clippy::too_many_arguments)]
pub async fn record_run(
    pool: &SqlitePool,
    kind: DreamKind,
    trigger: DreamTrigger,
    ok: bool,
    pages_failed: i64,
    pages_degraded: i64,
    summary: &str,
    log_text: &str,
    started_at: &str,
    finished_at: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO dream_runs \
         (kind, trigger_source, ok, pages_failed, pages_degraded, summary, log_text, \
          started_at, finished_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(kind.as_str())
    .bind(trigger.as_str())
    .bind(ok)
    .bind(pages_failed)
    .bind(pages_degraded)
    .bind(summary)
    .bind(log_text)
    .bind(started_at)
    .bind(finished_at)
    .execute(pool)
    .await
    .context("dream_journal: insert run")?;

    // Keep only the newest MAX_HISTORY rows. `id` is a monotonic autoincrement,
    // so ordering by it descending is newest-first.
    sqlx::query(
        "DELETE FROM dream_runs \
         WHERE id NOT IN (SELECT id FROM dream_runs ORDER BY id DESC LIMIT ?)",
    )
    .bind(MAX_HISTORY)
    .execute(pool)
    .await
    .context("dream_journal: prune history")?;

    Ok(())
}

/// The most recent runs, newest first, capped at `limit`.
///
/// # Errors
///
/// Surfaces the select SQL failure.
pub async fn recent_runs(pool: &SqlitePool, limit: i64) -> Result<Vec<DreamRun>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, kind, trigger_source, ok, pages_failed, pages_degraded, summary, \
                log_text, started_at, finished_at \
         FROM dream_runs ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("dream_journal: recent_runs")?;
    Ok(rows.into_iter().map(DreamRun::from).collect())
}

/// One run by id, or `None` when it has been pruned / never existed. Powers the
/// per-row log modal.
///
/// # Errors
///
/// Surfaces the select SQL failure.
pub async fn get_run(pool: &SqlitePool, id: i64) -> Result<Option<DreamRun>> {
    let row = sqlx::query_as::<_, RawRow>(
        "SELECT id, kind, trigger_source, ok, pages_failed, pages_degraded, summary, \
                log_text, started_at, finished_at \
         FROM dream_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("dream_journal: get_run")?;
    Ok(row.map(DreamRun::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        (dir, pool)
    }

    #[tokio::test]
    async fn records_and_reads_newest_first() {
        let (_dir, pool) = pool().await;
        record_run(
            &pool,
            DreamKind::Light,
            DreamTrigger::Scheduled,
            true,
            0,
            0,
            "promoted 1",
            "log A",
            "2026-06-30T00:00:00Z",
            "2026-06-30T00:00:01Z",
        )
        .await
        .expect("record light");
        record_run(
            &pool,
            DreamKind::Full,
            DreamTrigger::Manual,
            false,
            0,
            0,
            "boom",
            "log B",
            "2026-06-30T01:00:00Z",
            "2026-06-30T01:00:02Z",
        )
        .await
        .expect("record full");

        let runs = recent_runs(&pool, 10).await.expect("recent");
        assert_eq!(runs.len(), 2);
        // Newest first: the full manual error run leads.
        assert_eq!(runs[0].kind, DreamKind::Full);
        assert_eq!(runs[0].trigger, DreamTrigger::Manual);
        assert!(!runs[0].ok);
        assert_eq!(runs[1].kind, DreamKind::Light);

        let one = get_run(&pool, runs[0].id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(one.log_text, "log B");
    }

    /// A completed run that left pages failed/degraded records the structured
    /// counts and reads back as needing attention — the journal shape behind
    /// "the outcome stops reading as plain ok".
    #[tokio::test]
    async fn records_failed_and_degraded_counts() {
        let (_dir, pool) = pool().await;
        record_run(
            &pool,
            DreamKind::Full,
            DreamTrigger::Scheduled,
            true,
            2,
            1,
            "compiled 5 pages — 2 pages FAILED · 1 degraded",
            "log",
            "2026-07-02T00:00:00Z",
            "2026-07-02T00:00:05Z",
        )
        .await
        .expect("record");

        let run = &recent_runs(&pool, 1).await.expect("recent")[0];
        assert!(run.ok, "the run itself completed");
        assert_eq!(run.pages_failed, 2);
        assert_eq!(run.pages_degraded, 1);
        assert!(run.needs_attention(), "ok + failures ⇒ attention");

        // A clean run does not need attention; neither does an errored one
        // (the error badge already covers it).
        record_run(
            &pool,
            DreamKind::Light,
            DreamTrigger::Scheduled,
            true,
            0,
            0,
            "clean",
            "log",
            "2026-07-02T01:00:00Z",
            "2026-07-02T01:00:01Z",
        )
        .await
        .expect("record clean");
        let clean = &recent_runs(&pool, 1).await.expect("recent")[0];
        assert!(!clean.needs_attention());
    }

    #[tokio::test]
    async fn prunes_to_max_history() {
        let (_dir, pool) = pool().await;
        for i in 0..(MAX_HISTORY + 5) {
            record_run(
                &pool,
                DreamKind::Light,
                DreamTrigger::Scheduled,
                true,
                0,
                0,
                &format!("run {i}"),
                "log",
                "2026-06-30T00:00:00Z",
                "2026-06-30T00:00:01Z",
            )
            .await
            .expect("record");
        }
        let runs = recent_runs(&pool, MAX_HISTORY + 100).await.expect("recent");
        assert_eq!(runs.len(), usize::try_from(MAX_HISTORY).unwrap());
        // The newest survives, the oldest five were pruned.
        assert_eq!(runs[0].summary, format!("run {}", MAX_HISTORY + 4));
    }
}
