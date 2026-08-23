//! What changed since the last nightly run — the perimeter the REM works on.
//!
//! Every sub-job of the cycle scans the whole corpus and stops at a cap. That
//! is the wrong shape twice over: it re-reads material nothing has touched in
//! months, and the cap then decides which of *this* day's changes get looked
//! at, on an ordering nobody chose. What a night should re-examine is what the
//! day did.
//!
//! Four things say what the day did, and all four were already being written:
//!
//! - **`dream_runs`** — when the last full cycle finished, which is the
//!   perimeter's own boundary;
//! - **`page_create` receipts** in `structure_proposals` — every page the
//!   engine invented, with the wiki it landed in;
//! - **`fact_index.created_at`** — the facts written since;
//! - **`capture_buffer.last_attempt_at`** — the claims a pass read and left
//!   waiting.
//!
//! **A first run has no boundary and no perimeter.** With no previous full
//! cycle recorded, [`DayPerimeter::since`] is `None` and
//! [`DayPerimeter::covers_everything`] is true — which the callers read as
//! *look at all of it*, because on a corpus nobody has ever reorganised the
//! whole corpus is what the day did.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

/// What the day changed, as the REM sees it.
#[derive(Debug, Default, Clone)]
pub struct DayPerimeter {
    /// When the last full cycle finished. `None` = there has never been one.
    pub since: Option<DateTime<Utc>>,
    /// Plan slugs of the pages the engine invented since then.
    pub pages_born: BTreeSet<String>,
    /// `fact_id`s written since then.
    pub facts_written: BTreeSet<String>,
    /// How many claims a pass read since then and left waiting. A count, not
    /// a list: what to do with them is the closing pass's job
    /// ([`crate::dream::run_closing_pass`]), and this is only here so a night
    /// can say whether the day left any.
    pub claims_left_waiting: usize,
}

impl DayPerimeter {
    /// Whether there is no boundary, so everything counts as touched.
    #[must_use]
    pub const fn covers_everything(&self) -> bool {
        self.since.is_none()
    }

    /// Whether `fact` is inside the perimeter.
    ///
    /// A fact written since the boundary, or **any** fact when there is no
    /// boundary. Callers use it to order their candidates, never to drop
    /// one: a fact outside the perimeter is not out of scope, it is only
    /// further down the queue.
    #[must_use]
    pub fn touched_fact(&self, fact_id: &str) -> bool {
        self.covers_everything() || self.facts_written.contains(fact_id)
    }

    /// Whether `slug` names a page the day put there.
    #[must_use]
    pub fn touched_page(&self, slug: &str) -> bool {
        self.covers_everything() || self.pages_born.contains(slug)
    }

    /// Whether anything at all happened.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.covers_everything()
            && self.pages_born.is_empty()
            && self.facts_written.is_empty()
            && self.claims_left_waiting == 0
    }
}

/// Read the perimeter.
///
/// **Never an error.** A perimeter that could not be read degrades to the
/// empty one, which `covers_everything()` reports as false and every caller
/// reads as "order by whatever else you have" — the behaviour they had before
/// a perimeter existed. A night must not fail because a `SELECT` did.
pub async fn perimeter(pool: &SqlitePool) -> DayPerimeter {
    let mut out = DayPerimeter::default();

    // The boundary: the last full cycle that finished. A failed run is not a
    // boundary — what it did not finish, the next night still has to look at.
    let boundary: Option<String> = sqlx::query_scalar(
        "SELECT finished_at FROM dream_runs
          WHERE kind = 'full' AND ok = 1
          ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "rem: day boundary unread — the night orders itself");
        None
    });
    let Some(boundary) = boundary else {
        return out;
    };
    let Ok(since) = DateTime::parse_from_rfc3339(&boundary) else {
        tracing::warn!(
            boundary,
            "rem: day boundary unparseable — the night orders itself"
        );
        return out;
    };
    out.since = Some(since.with_timezone(&Utc));

    match sqlx::query_scalar::<_, String>(
        "SELECT context FROM structure_proposals
          WHERE kind = 'page_create' AND proposed_at >= ?",
    )
    .bind(&boundary)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            for ctx in rows {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ctx)
                    && let Some(slug) = v.get("slug").and_then(serde_json::Value::as_str)
                {
                    out.pages_born.insert(slug.to_owned());
                }
            }
        },
        Err(e) => tracing::warn!(error = %e, "rem: pages born today unread"),
    }

    match sqlx::query_scalar::<_, String>(
        "SELECT fact_id FROM fact_index
          WHERE created_at >= ? AND deleted_at IS NULL AND superseded_at IS NULL",
    )
    .bind(&boundary)
    .fetch_all(pool)
    .await
    {
        Ok(ids) => out.facts_written = ids.into_iter().collect(),
        Err(e) => tracing::warn!(error = %e, "rem: facts written today unread"),
    }

    match sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM capture_buffer
          WHERE status = 'buffered' AND last_attempt_at >= ?",
    )
    .bind(&boundary)
    .fetch_one(pool)
    .await
    {
        Ok(n) => out.claims_left_waiting = usize::try_from(n).unwrap_or(usize::MAX),
        Err(e) => tracing::warn!(error = %e, "rem: claims left waiting unread"),
    }

    tracing::info!(
        since = %boundary,
        pages_born = out.pages_born.len(),
        facts_written = out.facts_written.len(),
        claims_left_waiting = out.claims_left_waiting,
        "rem: the day's perimeter"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    async fn record_full_run(pool: &SqlitePool, finished_at: &str, ok: bool) {
        sqlx::query(
            "INSERT INTO dream_runs
               (kind, trigger_source, ok, summary, log_text, started_at, finished_at)
             VALUES ('full','scheduled',?,'s','l',?,?)",
        )
        .bind(i64::from(ok))
        .bind(finished_at)
        .bind(finished_at)
        .execute(pool)
        .await
        .expect("run row");
    }

    /// A corpus nobody has ever reorganised has no boundary, and everything
    /// counts as touched — which is what the callers need, because on the
    /// first night the whole corpus *is* what the day did.
    #[tokio::test]
    async fn with_no_previous_cycle_the_perimeter_is_everything() {
        let (_wd, pool) = db().await;
        let p = perimeter(&pool).await;
        assert!(p.covers_everything());
        assert!(p.touched_fact("anything"));
        assert!(p.touched_page("anything"));
        assert!(!p.is_empty(), "everything is not nothing");
    }

    /// A run that failed is not a boundary: whatever it did not finish, the
    /// next night still has to look at.
    #[tokio::test]
    async fn a_failed_run_does_not_close_the_day() {
        let (_wd, pool) = db().await;
        record_full_run(&pool, "2026-08-22T02:00:00Z", false).await;
        assert!(perimeter(&pool).await.covers_everything());

        record_full_run(&pool, "2026-08-23T02:00:00Z", true).await;
        assert!(!perimeter(&pool).await.covers_everything());
    }

    /// The four sources, read off the rows they were already being written to.
    #[tokio::test]
    async fn the_perimeter_is_what_happened_after_the_last_cycle() {
        let (_wd, pool) = db().await;
        record_full_run(&pool, "2026-08-22T02:00:00Z", true).await;

        for (slug, at) in [
            ("prima", "2026-08-22T01:00:00Z"),
            ("dopo", "2026-08-22T09:00:00Z"),
        ] {
            sqlx::query(
                "INSERT INTO structure_proposals
                   (proposal_id, kind, context, questions, proposed_at, timeout_at, status)
                 VALUES (?, 'page_create', ?, '[]', ?, ?, 'applied')",
            )
            .bind(format!("p-{slug}"))
            .bind(format!("{{\"slug\":\"{slug}\"}}"))
            .bind(at)
            .bind(at)
            .execute(&pool)
            .await
            .expect("receipt");
        }

        for (tail, at) in [
            ("aa", "2026-08-22T01:00:00Z"),
            ("bb", "2026-08-22T09:00:00Z"),
        ] {
            sqlx::query(
                "INSERT INTO fact_index
                   (fact_id, wiki_id, source_path, text, embedding, embedding_dim, subject_id,
                    allow_ids, topics, created_at, updated_at)
                 VALUES (?, 'alice', 'wikis/alice/x.md', 't', X'00', 0, 'user:alice',
                         '[]', '[]', ?, ?)",
            )
            .bind(format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{tail}"))
            .bind(at)
            .bind(at)
            .execute(&pool)
            .await
            .expect("fact");
        }

        sqlx::query(
            "INSERT INTO capture_buffer
               (capture_id, body, subject_id, status, captured_at, last_attempt_at)
             VALUES ('0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dcc','x','user:alice','buffered',
                     '2026-08-22T09:00:00Z','2026-08-22T09:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("capture");

        let p = perimeter(&pool).await;
        assert!(!p.covers_everything());
        assert_eq!(
            p.pages_born,
            std::iter::once("dopo".to_owned()).collect::<BTreeSet<_>>(),
            "a page born before the boundary is not the day's"
        );
        assert!(p.touched_fact("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dbb"));
        assert!(!p.touched_fact("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5daa"));
        assert!(p.touched_page("dopo"));
        assert!(!p.touched_page("prima"));
        assert_eq!(p.claims_left_waiting, 1);
        assert!(!p.is_empty());
    }

    /// A night on which nothing happened has a boundary and an empty
    /// perimeter — which is a different thing from having no boundary, and
    /// the callers must be able to tell them apart.
    #[tokio::test]
    async fn a_quiet_day_is_not_the_same_as_a_first_night() {
        let (_wd, pool) = db().await;
        record_full_run(&pool, "2026-08-22T02:00:00Z", true).await;
        let p = perimeter(&pool).await;
        assert!(!p.covers_everything());
        assert!(p.is_empty());
        assert!(!p.touched_fact("anything"));
    }
}
