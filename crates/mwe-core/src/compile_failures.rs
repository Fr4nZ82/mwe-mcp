// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-page compile-failure ledger — the persistence behind the
//! `compile_failure_streak` operator notice.
//!
//! The narrative compiler ([`crate::compiler`]) soft-fails a page rather than
//! aborting the pass, and its degraded mode keeps a failing page from
//! starving — but a page the Cronista keeps failing must reach the operator,
//! not only the log. This ledger tracks **consecutive** failed/degraded
//! compiles per page (keyed by the workdir-relative `source_path`, the same
//! key `fact_index` uses): the compiler calls [`record_failure`] after every
//! failed or degraded page compile and [`reset`] after every clean one.
//!
//! A **degraded** compile (the guard-only append) still counts as a failure:
//! the page made progress, but the Cronista keeps failing there — the streak
//! ends only on a clean full rewrite.
//!
//! When a streak reaches one of [`NOTICE_THRESHOLDS`] exactly, the compiler
//! emits one `compile_failure_streak` event on `wiki_events` (the same
//! channel the `structure_applied` notices ride) — once per threshold per
//! streak, not every cycle. The thresholds are **observability thresholds on
//! a failure ledger, not semantic gates**: nothing about the memory's content
//! is decided here.
//!
//! Storage lives in the `compile_failures` table (migration `0055`).

use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// Streak lengths at which the compiler emits the `compile_failure_streak`
/// notice.
///
/// At exactly 2 (the page failed two compile passes in a row, the live prod
/// signature) and again at exactly 5 (still failing; the operator may have
/// missed the first). Between and beyond, the ledger keeps counting silently;
/// a clean rewrite resets it.
pub const NOTICE_THRESHOLDS: [i64; 2] = [2, 5];

/// One page's failing streak, as read back for tests / surfaces.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompileFailure {
    /// Workdir-relative page path (`wikis/<id>/<page>.md`).
    pub source_path: String,
    /// Consecutive failed/degraded compiles of this page.
    pub consecutive: i64,
    /// The most recent failure message.
    pub last_error: String,
    /// RFC-3339 UTC of the last increment.
    pub updated_at: String,
}

/// Record one failed (or degraded) compile of `source_path` and return the
/// new consecutive count. Upserts: the first failure inserts the row at 1,
/// every further failure increments it.
///
/// # Errors
///
/// Surfaces the SQL failure so the caller can log it (the ledger is
/// observability — a write failure must never fail the compile itself).
pub async fn record_failure(pool: &SqlitePool, source_path: &str, error: &str) -> Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO compile_failures (source_path, consecutive, last_error, updated_at) \
         VALUES (?, 1, ?, ?) \
         ON CONFLICT(source_path) DO UPDATE SET \
           consecutive = consecutive + 1, \
           last_error = excluded.last_error, \
           updated_at = excluded.updated_at \
         RETURNING consecutive",
    )
    .bind(source_path)
    .bind(error)
    .bind(&now)
    .fetch_one(pool)
    .await
    .context("compile_failures: record_failure")?;
    Ok(row.0)
}

/// End `source_path`'s failing streak: delete its ledger row.
///
/// Idempotent — called after **every clean** page compile (a page with no row
/// is a no-op). A degraded compile never resets (see the module docs).
///
/// # Errors
///
/// Surfaces the SQL failure so the caller can log it.
pub async fn reset(pool: &SqlitePool, source_path: &str) -> Result<()> {
    sqlx::query("DELETE FROM compile_failures WHERE source_path = ?")
        .bind(source_path)
        .execute(pool)
        .await
        .context("compile_failures: reset")?;
    Ok(())
}

/// Every page whose failing streak reached `min_consecutive`.
///
/// The bridge signal for re-opening a page's carried placements: a page
/// that keeps failing its compile is usually too massive for one
/// reliable render, and re-judging its placements lets split-by-mass
/// fire. A repair-cadence bound on an already-recorded outcome, not a
/// semantic gate.
///
/// # Errors
///
/// Surfaces the SQL failure.
pub async fn persistent(pool: &SqlitePool, min_consecutive: i64) -> Result<Vec<CompileFailure>> {
    let rows = sqlx::query_as::<_, CompileFailure>(
        "SELECT source_path, consecutive, last_error, updated_at \
         FROM compile_failures WHERE consecutive >= ? ORDER BY source_path",
    )
    .bind(min_consecutive)
    .fetch_all(pool)
    .await
    .context("compile_failures: persistent")?;
    Ok(rows)
}

/// The current streak for `source_path`, or `None` when the page is not
/// failing.
///
/// # Errors
///
/// Surfaces the SQL failure.
pub async fn get(pool: &SqlitePool, source_path: &str) -> Result<Option<CompileFailure>> {
    let row = sqlx::query_as::<_, CompileFailure>(
        "SELECT source_path, consecutive, last_error, updated_at \
         FROM compile_failures WHERE source_path = ?",
    )
    .bind(source_path)
    .fetch_optional(pool)
    .await
    .context("compile_failures: get")?;
    Ok(row)
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
    async fn persistent_lists_streaks_at_or_over_the_bar() {
        let (_dir, pool) = pool().await;
        record_failure(&pool, "wikis/a/one.md", "boom")
            .await
            .unwrap();
        record_failure(&pool, "wikis/a/two.md", "boom")
            .await
            .unwrap();
        record_failure(&pool, "wikis/a/two.md", "boom")
            .await
            .unwrap();
        let rows = persistent(&pool, 2).await.unwrap();
        assert_eq!(rows.len(), 1, "only the repeat offender qualifies");
        assert_eq!(rows[0].source_path, "wikis/a/two.md");
        assert_eq!(rows[0].consecutive, 2);
    }

    #[tokio::test]
    async fn ledger_increments_per_failure_and_resets_on_clean() {
        let (_dir, pool) = pool().await;
        let path = "wikis/alice/index.md";

        assert_eq!(record_failure(&pool, path, "boom 1").await.unwrap(), 1);
        assert_eq!(record_failure(&pool, path, "boom 2").await.unwrap(), 2);
        assert_eq!(record_failure(&pool, path, "boom 3").await.unwrap(), 3);
        let row = get(&pool, path).await.unwrap().expect("row");
        assert_eq!(row.consecutive, 3);
        assert_eq!(row.last_error, "boom 3", "last_error tracks the freshest");

        // A clean rewrite ends the streak; the next failure starts a NEW one.
        reset(&pool, path).await.unwrap();
        assert!(get(&pool, path).await.unwrap().is_none(), "streak cleared");
        assert_eq!(
            record_failure(&pool, path, "boom again").await.unwrap(),
            1,
            "a post-reset failure starts a fresh streak"
        );

        // Reset is idempotent for pages that never failed.
        reset(&pool, "wikis/alice/never_failed.md").await.unwrap();
    }
}
