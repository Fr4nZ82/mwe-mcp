// SPDX-License-Identifier: AGPL-3.0-or-later
//! REM Briefing-processor (sub-job 10).
//!
//! The REM is the conceptual maintainer of non-smart wikis
//! (the identity wikis and every emerged sub-wiki). On
//! smart-wikis the smart consumer drains
//! [`wiki_briefing_items`] at `smart_bootstrap` and closes them with
//! `mark_processed` on the next `wiki_admin_push`; on non-smart
//! wikis no smart consumer is wired, so rows would otherwise pile up
//! with `processed_at IS NULL` forever. This module fills the gap with
//! one stateless entry point:
//!
//! ```ignore
//! pub async fn process_briefing_item(
//!     pool: &SqlitePool,
//!     tree: &WikiTree,
//!     bi_id: i64,
//! ) -> Result<ProcessOutcome>
//! ```
//!
//! The same function is invoked **(a)** in batch by [`super::run_cycle`]
//! (sub-job 10) over every pending non-smart row past the grace
//! period and **(b)** synchronously by the dashboard endpoint
//! `POST /dashboard/wiki/:id/briefing-items/:bi_id/process` when the
//! operator clicks the "Submit" button on a non-smart wiki view
//! and does not want to wait for the next REM tick.
//!
//! ## Policy: mark-passive (MVP default)
//!
//! Two operational behaviours were considered:
//!
//! - **mark-passive** — read the `target_cite` context (when present)
//!   pro-forma, then stamp `processed_at = NOW()`. No semantic action
//!   on the target fact, no supersede / promote / archive. Equivalent
//!   to "REM took note".
//! - **action-taking** — recall the context, feed it to a workhorse
//!   LLM slot, decide a structural action, apply it through the
//!   proposal pipeline, then stamp processed.
//!
//! The MVP ships **mark-passive** because it is the simplest +
//! safest behaviour to validate the loop end-to-end before adding
//! semantic teeth. Action-taking remains a deferred follow-up tracked
//! in the roadmap; the public [`ProcessOutcome`] exposes
//! `context_loaded` so the operator can see how often the processor
//! was given a citable target to read versus how often it was a free
//! comment with no anchor.

use sqlx::SqlitePool;
use thiserror::Error;

use crate::briefing;
use crate::types::WikiId;
use crate::wiki::{self, WikiError, WikiTree};

/// Outcome of one [`process_briefing_item`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// Row was eligible and stamped `processed_at = NOW()`. The
    /// `target_cite` value reflects what was on the row (or `None` for
    /// a comment that wasn't anchored). `context_loaded` is `true`
    /// when a parseable `target_cite` resolved to a readable page in
    /// the memory-wiki tree (mark-passive policy reads the target
    /// pro-forma so the report can show how often REM actually "took
    /// note" of a citable context).
    Processed {
        /// The `bi_id` that was processed.
        bi_id: i64,
        /// Verbatim `target_cite` string from the row, if any.
        target_cite: Option<String>,
        /// `true` when the citation parsed AND the page was readable.
        /// `false` when there was no citation, the citation was
        /// malformed, the wiki had been deleted, or the page no
        /// longer existed. Mark-passive policy stamps `processed_at`
        /// either way — the flag is informational, not a gate.
        context_loaded: bool,
    },
    /// Row was already `processed_at IS NOT NULL`. No-op, idempotent.
    AlreadyProcessed {
        /// The `bi_id` the caller asked about.
        bi_id: i64,
    },
    /// Row was eligible but its `wiki_id` no longer resolves to a
    /// known wiki on disk (a wiki was deleted with rows still in the
    /// inbox). The row is left untouched — the caller can decide to
    /// quarantine or skip on the next cycle. The batch loop in
    /// [`super::run_cycle`] counts these as errors so the operator
    /// notices.
    WikiNotFound {
        /// The `bi_id` whose wiki could not be located.
        bi_id: i64,
        /// The `wiki_id` string from the row (verbatim, may not be a
        /// valid [`WikiId`] anymore).
        wiki_id: String,
    },
}

/// Errors raised by [`process_briefing_item`]. Maps to `400` / `404` /
/// `500` at the dashboard boundary.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// No row with the requested `bi_id`.
    #[error("briefing item bi_{0} not found")]
    NotFound(i64),
    /// Underlying SQL failure (sqlx surface).
    #[error("briefing_processor db: {0}")]
    Db(#[from] sqlx::Error),
    /// Filesystem-side failure surfaced while resolving the target
    /// page through [`WikiTree`].
    #[error("briefing_processor wiki: {0}")]
    Wiki(#[from] WikiError),
    /// `wiki_id` column held a value that no longer parses against the
    /// canonical id charset (corrupt row left behind by a prior
    /// migration). Should not happen on rows the rest of the codebase
    /// wrote.
    #[error("briefing_processor invalid wiki_id in row bi_{bi_id}: {detail}")]
    InvalidWikiId {
        /// Offending row.
        bi_id: i64,
        /// Free-form detail.
        detail: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, ProcessError>;

/// Process one briefing-item row.
///
/// Steps (mark-passive policy):
///
/// 1. Load `wiki_id`, `target_cite`, `processed_at` for the row. Row
///    missing ⇒ [`ProcessError::NotFound`].
/// 2. If `processed_at IS NOT NULL` ⇒ [`ProcessOutcome::AlreadyProcessed`].
/// 3. Parse `wiki_id` and locate it via [`WikiTree::locate`]. Missing
///    wiki ⇒ [`ProcessOutcome::WikiNotFound`] (no DB mutation; the
///    caller decides what to do).
/// 4. If `target_cite` is set, parse it via [`briefing::parse_cite`]
///    and attempt to read the referenced page (pro-forma read, so the
///    `context_loaded` flag in the outcome reflects whether REM
///    actually had something to "look at"). Parse / read failures
///    leave `context_loaded = false` but do **not** abort — comments
///    without a usable anchor are still legitimately drainable.
/// 5. `UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ?`
///    with the current UTC timestamp. Return [`ProcessOutcome::Processed`].
///
/// # Errors
///
/// See [`ProcessError`].
pub async fn process_briefing_item(
    pool: &SqlitePool,
    tree: &WikiTree,
    bi_id: i64,
) -> Result<ProcessOutcome> {
    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT wiki_id, target_cite, processed_at FROM wiki_briefing_items WHERE id = ?",
    )
    .bind(bi_id)
    .fetch_optional(pool)
    .await?;
    let Some((wiki_id_raw, target_cite, processed_at)) = row else {
        return Err(ProcessError::NotFound(bi_id));
    };
    if processed_at.is_some() {
        return Ok(ProcessOutcome::AlreadyProcessed { bi_id });
    }

    let wiki_id = match WikiId::parse(&wiki_id_raw) {
        Ok(id) => id,
        Err(e) => {
            return Err(ProcessError::InvalidWikiId {
                bi_id,
                detail: format!("{e}"),
            });
        },
    };
    let handle = match tree.locate(&wiki_id) {
        Ok(h) => h,
        Err(WikiError::WikiNotFound { .. }) => {
            return Ok(ProcessOutcome::WikiNotFound {
                bi_id,
                wiki_id: wiki_id_raw,
            });
        },
        Err(e) => return Err(ProcessError::Wiki(e)),
    };

    // Pro-forma context read (mark-passive policy). A malformed
    // citation, a wiki whose target page was renamed, or a citation
    // whose parsed `wiki_id` differs from the row's `wiki_id` all
    // collapse onto `context_loaded = false` — the row is still
    // drained because the operator's intent (the comment) is what
    // mattered, not whether the anchor is still resolvable.
    let context_loaded = target_cite
        .as_deref()
        .is_some_and(|cite| load_context_silently(tree, &handle, &wiki_id, cite));

    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ?")
        .bind(&now)
        .bind(bi_id)
        .execute(pool)
        .await?;

    Ok(ProcessOutcome::Processed {
        bi_id,
        target_cite,
        context_loaded,
    })
}

/// Try to read the page referenced by `cite` for the pro-forma context
/// load step. Returns `true` only when the citation parses AND its
/// wiki segment matches the row's wiki AND the page reads cleanly.
///
/// Errors are absorbed silently — `target_cite` may legitimately
/// point at a page that no longer exists (the operator commented on a
/// section that was later removed; nothing in the spec promises
/// citations stay live forever), and the processor still wants to
/// stamp the row. The boolean we return drives a single counter in
/// the report.
fn load_context_silently(
    tree: &WikiTree,
    handle: &wiki::WikiHandle,
    row_wiki: &WikiId,
    cite: &str,
) -> bool {
    let Ok(parsed) = briefing::parse_cite(cite) else {
        return false;
    };
    if parsed.wiki_id.as_str() != row_wiki.as_str() {
        // Citation points at a different wiki than the row — refuse
        // to cross the boundary. `wiki_briefing_items.wiki_id` is the
        // authoritative scope; the row stays drainable.
        let _ = tree; // tree unused on this branch; keep the parameter shape uniform
        return false;
    }
    let page = std::path::PathBuf::from(&parsed.path);
    handle.read_page(&page).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use sqlx::SqlitePool;
    use std::path::Path;
    use tempfile::TempDir;

    /// Spin up an isolated workdir with a fresh `engine.db` + an empty
    /// `wikis/` tree. Mirrors the helper used by the parent `rem`
    /// tests so the two modules behave identically on the test floor.
    async fn setup_workdir() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("open tree");
        (dir, tree, pool)
    }

    fn write_standard_wiki(tree: &WikiTree, slug: &str, owner: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{owner}'\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(
            dir.join("index.md"),
            "# placeholder\n## first-heading\n\nbody.\n",
        )
        .unwrap();
    }

    async fn insert_briefing_row(
        pool: &SqlitePool,
        wiki_id: &str,
        target_cite: Option<&str>,
        processed_at: Option<&str>,
    ) -> i64 {
        let ts = chrono::Utc::now().to_rfc3339();
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO wiki_briefing_items
                (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, \
                 author_sender_id, processed_at)
             VALUES (?, 'dashboard_comment', 'dashboard:alice', 'topic', 'body', 'external', \
                     ?, ?, 'alice', ?)
             RETURNING id",
        )
        .bind(wiki_id)
        .bind(&ts)
        .bind(target_cite)
        .bind(processed_at)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    #[tokio::test]
    async fn returns_processed_with_context_loaded_when_cite_resolves() {
        let (dir, tree, pool) = setup_workdir().await;
        write_standard_wiki(&tree, "alice", "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        let bi_id = insert_briefing_row(
            &pool,
            "alice",
            Some("wiki://alice/index.md#first-heading"),
            None,
        )
        .await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process_briefing_item");

        match outcome {
            ProcessOutcome::Processed {
                bi_id: out_id,
                target_cite,
                context_loaded,
            } => {
                assert_eq!(out_id, bi_id);
                assert_eq!(
                    target_cite.as_deref(),
                    Some("wiki://alice/index.md#first-heading"),
                );
                assert!(
                    context_loaded,
                    "target_cite resolves to an existing page ⇒ context_loaded must be true"
                );
            },
            other => panic!("expected Processed, got {other:?}"),
        }

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_some(),
            "processed_at must be stamped after a Processed outcome"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn returns_processed_without_cite() {
        let (dir, tree, pool) = setup_workdir().await;
        write_standard_wiki(&tree, "alice", "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        let bi_id = insert_briefing_row(&pool, "alice", None, None).await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process");

        match outcome {
            ProcessOutcome::Processed {
                target_cite,
                context_loaded,
                ..
            } => {
                assert!(target_cite.is_none());
                assert!(!context_loaded, "no cite ⇒ context_loaded must be false");
            },
            other => panic!("expected Processed, got {other:?}"),
        }
        drop(dir);
    }

    #[tokio::test]
    async fn returns_already_processed_when_processed_at_is_set() {
        let (dir, tree, pool) = setup_workdir().await;
        write_standard_wiki(&tree, "alice", "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        let earlier = chrono::Utc::now().to_rfc3339();
        let bi_id = insert_briefing_row(&pool, "alice", None, Some(&earlier)).await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process");

        match outcome {
            ProcessOutcome::AlreadyProcessed { bi_id: out_id } => {
                assert_eq!(out_id, bi_id);
            },
            other => panic!("expected AlreadyProcessed, got {other:?}"),
        }

        // processed_at must not have been overwritten — re-read it.
        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(processed.as_deref(), Some(earlier.as_str()));
        drop(dir);
    }

    #[tokio::test]
    async fn returns_wiki_not_found_when_wiki_id_does_not_resolve() {
        let (dir, tree, pool) = setup_workdir().await;
        // No on-disk wiki for `ghost` — only the row exists.
        let bi_id = insert_briefing_row(&pool, "ghost", None, None).await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process");

        match outcome {
            ProcessOutcome::WikiNotFound {
                bi_id: out_id,
                wiki_id,
            } => {
                assert_eq!(out_id, bi_id);
                assert_eq!(wiki_id, "ghost");
            },
            other => panic!("expected WikiNotFound, got {other:?}"),
        }

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_none(),
            "WikiNotFound must not stamp processed_at"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn returns_processed_with_context_loaded_false_when_page_missing() {
        let (dir, tree, pool) = setup_workdir().await;
        write_standard_wiki(&tree, "alice", "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        let bi_id = insert_briefing_row(
            &pool,
            "alice",
            Some("wiki://alice/does-not-exist.md#anywhere"),
            None,
        )
        .await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process");

        match outcome {
            ProcessOutcome::Processed { context_loaded, .. } => {
                assert!(
                    !context_loaded,
                    "missing page ⇒ context_loaded must be false (but still Processed)"
                );
            },
            other => panic!("expected Processed, got {other:?}"),
        }

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_some(),
            "mark-passive ⇒ processed_at stamped even when context fails to load"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn returns_not_found_for_missing_row() {
        let (_dir, tree, pool) = setup_workdir().await;
        let err = process_briefing_item(&pool, &tree, 999_999)
            .await
            .expect_err("expected error");
        match err {
            ProcessError::NotFound(id) => assert_eq!(id, 999_999),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_cite_keeps_row_drainable() {
        // Verifies the silent absorption of `parse_cite` failures: a
        // junk citation must not abort the processor — the row still
        // ends up drained, just with `context_loaded = false`.
        let (dir, tree, pool) = setup_workdir().await;
        write_standard_wiki(&tree, "alice", "alice");
        let tree = WikiTree::open(dir.path()).unwrap();
        let bi_id = insert_briefing_row(&pool, "alice", Some("not-a-cite-at-all"), None).await;

        let outcome = process_briefing_item(&pool, &tree, bi_id)
            .await
            .expect("process");

        match outcome {
            ProcessOutcome::Processed {
                target_cite,
                context_loaded,
                ..
            } => {
                assert_eq!(target_cite.as_deref(), Some("not-a-cite-at-all"));
                assert!(!context_loaded);
            },
            other => panic!("expected Processed, got {other:?}"),
        }
        // sanity: ensure no stray page read materialised somewhere.
        assert!(Path::new(&tree.wikis_dir().join("alice/index.md")).exists());
        drop(dir);
    }
}
