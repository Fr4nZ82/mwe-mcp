// SPDX-License-Identifier: AGPL-3.0-or-later
//! Narrative comment application (action-taking).
//!
//! A dashboard comment on a compiled standard page is parked as an unprocessed
//! `wiki_briefing_items` row — there is no submit button, the operator just
//! leaves it. The REM full cycle (the batched, nightly-or-admin-triggered dream)
//! reads **every** pending comment for a wiki *together* and turns each into a
//! fact-level edit against the facts on the page the comment is anchored to:
//! `correct` a claim in place, `remove` it, `add` a new one, or `move` one of
//! the page's facts to a destination the operator named (another page of this
//! wiki, or another wiki entirely). The compile pass that runs after the cycle
//! then recompiles only the page(s) whose fact set or content changed — the
//! [content-aware fingerprint](planner) makes an in-place correction dirty
//! exactly its page.
//!
//! Two invariants hold this module to the maintainer's cost rule ("a memory edit
//! must never re-scan the whole wiki"):
//!
//! - **Containment.** A comment only ever acts on the facts of the page it is
//!   anchored to. `correct` / `remove` / `move` are refused for any `fact_id`
//!   not on that page (a cross-page / hallucinated id never mutates a stranger's
//!   fact). `move` relocates such a fact *out* of the page, but only to a
//!   destination drawn from a bounded list (the wiki owner's other non-smart
//!   wikis + this wiki's other pages) — never an invented or cross-owner one.
//! - **A fact is a fact.** A `correct` preserves the existing fact's
//!   owner/`allow`/sender verbatim (it touches claim text only); an `add` carries
//!   its OWN ACL under the same rules as a captured message — the LLM decides
//!   the subject (`owner`) and audience (`allow`) from the comment, the page's
//!   wiki scope, and the commenter's group scopes, defaulting to the commenter /
//!   `[]`, with `sender` = the human who left the comment (`author_sender_id`).
//!   It never copies an arbitrary existing fact's (possibly broader) owner. A
//!   `move` keeps the fact's ACL and refuses a destination wiki with a different
//!   owner.
//!
//! Unlike `correct` / `remove` / `add` (which apply bare), a `move` is
//! **born-applied + revertible** — the `promote::*_direct` wrappers mint a
//! receipt, because a relocation (especially cross-wiki) must be undoable.
//!
//! The interpreter runs on the strong **ingest** tier — turning a free-text
//! correction into precise fact ops is the same class of judgment as ingesting a
//! message, and reusing that slot keeps operator config unchanged.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::briefing;
use crate::embedder::Embedder;
use crate::fact_index::{self, NewFact, RegionUpdate};
use crate::llm::{CompletionRequest, LlmBackend};
use crate::promote::{self, ParagraphToFileHints};
use crate::prompts;
use crate::proposals;
use crate::types::{FactId, Principal, WikiId};
use crate::wiki::{WikiError, WikiHandle, WikiTree, workdir_relative_source_path};

/// Bundled default for the comment-apply prompt.
pub const BUNDLED_COMMENT_APPLY_MD: &str = include_str!("../prompts/comment-apply.md");

/// Tombstone reason stamped on a fact a comment asks to forget.
const REASON_COMMENT_REMOVE: &str = "dashboard_comment";

/// Landing page for a cross-wiki `move`: a fact refiled into another wiki
/// always lands on that wiki's `index.md`. The compilation plan keys pages by
/// bare slug forest-wide, so a named cross-wiki page would collide; the
/// destination wiki's own compile pass then re-homes the fact onto the right
/// page. Same contract as the REM cross-wiki refile sweep.
const CROSS_WIKI_DEST_PAGE: &str = "index.md";

/// Cap on the comment excerpt woven into a move receipt's `reason` so the
/// audit string stays a single readable line.
const MOVE_REASON_EXCERPT_CHARS: usize = 160;

/// What [`apply_comments`] did for one standard wiki.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CommentApplyReport {
    /// Comments stamped `processed_at` (drained).
    pub comments_processed: usize,
    /// Existing facts corrected in place.
    pub facts_corrected: usize,
    /// New facts added on the anchored page.
    pub facts_added: usize,
    /// `add` ops skipped by the write-time dedup (the text was a
    /// near-duplicate of an existing same-owner fact — nothing inserted).
    pub facts_deduped: usize,
    /// Facts tombstoned at a comment's request.
    pub facts_removed: usize,
    /// Facts relocated at a comment's request — to another page of this wiki,
    /// or cross-wiki onto the destination wiki's `index.md`. Unlike the other
    /// ops these are born-applied + revertible (the `_direct` wrappers mint a
    /// receipt), because a move — especially cross-wiki — must be undoable.
    pub facts_moved: usize,
    /// Per-page soft errors (an unparseable / failed page is left for the next
    /// cycle; its comments stay unprocessed). Never aborts the other pages.
    pub errors: Vec<String>,
}

/// Errors raised by [`apply_comments`].
#[derive(Debug, Error)]
pub enum CommentApplyError {
    /// DB access failed.
    #[error("comment_apply db: {0}")]
    Db(#[from] sqlx::Error),
    /// `fact_index` mutation failed.
    #[error("comment_apply fact_index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),
    /// Embedding a corrected / added claim failed.
    #[error("comment_apply embedder: {0}")]
    Embedder(#[from] crate::embedder::EmbedderError),
    /// LLM interpretation failed (transient — the comments stay unprocessed and
    /// retry next cycle).
    #[error("comment_apply llm: {0}")]
    Llm(#[from] crate::llm::LlmError),
    /// Filesystem / tree resolution failed.
    #[error("comment_apply wiki: {0}")]
    Wiki(#[from] WikiError),
    /// A planner/compiler prompt override is broken.
    #[error("comment_apply prompt: {0}")]
    Prompt(#[from] prompts::PromptError),
    /// The LLM returned text with no parseable `{ "ops": [...] }` object.
    #[error("comment_apply: unparseable interpreter output")]
    Unparseable,
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, CommentApplyError>;

/// The interpreter's structured output: a flat list of ops over the page facts.
#[derive(Debug, Deserialize)]
struct InterpretedOps {
    #[serde(default)]
    ops: Vec<RawOp>,
}

/// One interpreted op. Flat (rather than a tagged enum) for robustness to LLM
/// output variance; the `action` string is validated when applied.
#[derive(Debug, Deserialize)]
struct RawOp {
    action: String,
    #[serde(default)]
    fact_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// `add` only: the new fact's SUBJECT principal, decided by the LLM under
    /// the ingest rules. Absent/malformed → `user:<commenter>` (the default a
    /// captured message fact takes for its author).
    #[serde(default)]
    owner_id: Option<String>,
    /// `add` only: the new fact's AUDIENCE (extra read principals beyond
    /// owner+sender), decided by the LLM from the page/group scope signals.
    /// Empty (the default) keeps the fact to owner+commenter.
    #[serde(default)]
    allow_ids: Vec<String>,
    /// `move` only: the destination wiki chosen from `{destinations}`. Absent
    /// (or equal to the source wiki) means a same-wiki page move.
    #[serde(default)]
    dest_wiki_id: Option<String>,
    /// `move` only: the destination page. For a same-wiki move it is a page of
    /// this wiki; for a cross-wiki move it is ignored (the fact always lands on
    /// the destination wiki's `index.md` — the compilation plan keys pages by
    /// bare slug forest-wide, so a named cross-wiki page would collide).
    #[serde(default)]
    dest_page: Option<String>,
}

/// Interpret and apply every pending comment in `bi_ids` for one standard wiki.
///
/// `bi_ids` are the candidate `wiki_briefing_items` ids the REM cycle already
/// filtered (non-smart, past the grace period). Each is loaded, grouped by
/// the page its `target_cite` anchors to, interpreted by `llm`, and applied. A
/// comment with no resolvable anchor is drained with no action (there is nothing
/// safe to target).
///
/// # Errors
///
/// Infrastructure failures (DB, tree). Per-page interpreter/apply failures are
/// captured in [`CommentApplyReport::errors`], never raised — so one bad page
/// never blocks the rest.
pub async fn apply_comments(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    wiki_id: &WikiId,
    bi_ids: &[i64],
    now: &str,
) -> Result<CommentApplyReport> {
    let mut report = CommentApplyReport::default();
    if bi_ids.is_empty() {
        return Ok(report);
    }
    let handle = match tree.locate(wiki_id) {
        Ok(h) => h,
        // The wiki vanished between the candidate scan and now — leave the rows
        // for the cycle's `WikiNotFound` accounting, do nothing here.
        Err(WikiError::WikiNotFound { .. }) => return Ok(report),
        Err(e) => return Err(e.into()),
    };
    // The wiki's scope principal (derived from topology). Two roles: it bounds a
    // `move` destination to a same-owner wiki, and it is the LAST-RESORT owner an
    // `add` falls back to when the LLM emits no `owner_id` AND the comment has no
    // recorded author — never an arbitrary existing fact's (possibly broader)
    // owner. With an author and/or an LLM `owner_id` the `add` follows the
    // captured-message rules instead (see `apply_add`).
    let add_owner = tree.resolve_scope_principal(handle.meta())?;
    // The language every claim this pass writes comes out in. The operator may
    // comment in any language; the page keeps the one its wiki declares, so a
    // correction never leaves a page speaking two languages. Resolved once per
    // wiki off the principal already in hand.
    let language_directive = crate::locale::render_memory_language_directive(
        crate::enrollment::locale_for_principal(pool, &add_owner)
            .await
            .unwrap_or_default()
            .as_deref(),
    );

    // Group the pending comments by the page their citation anchors to.
    let mut by_page: std::collections::BTreeMap<String, Vec<(i64, String, Option<String>)>> =
        std::collections::BTreeMap::new();
    for &bi in bi_ids {
        let Some((cite, body, author)) = load_comment(pool, bi).await? else {
            continue; // already drained by a racing path
        };
        if let Some(src) = resolve_source_path(tree, &handle, wiki_id, cite.as_deref()) {
            by_page.entry(src).or_default().push((bi, body, author));
        } else {
            // Unanchored / cross-wiki / orphaned citation: nothing safe to
            // target. Drain it so it does not pile up forever.
            stamp_processed(pool, bi, now).await?;
            report.comments_processed += 1;
        }
    }

    for (source_path, comments) in by_page {
        if let Err(e) = apply_page(
            pool,
            tree,
            embedder,
            llm,
            wiki_id,
            &add_owner,
            &language_directive,
            &source_path,
            &comments,
            now,
            &mut report,
        )
        .await
        {
            report.errors.push(format!("page {source_path}: {e}"));
        }
    }
    Ok(report)
}

/// Apply the comments anchored on one page.
///
/// Gathers the page's active facts, interprets the comments against them, and
/// applies the resulting ops. Stamps the comments `processed_at` only once the
/// LLM call parsed — a transient LLM/parse failure leaves them for next cycle.
#[allow(
    clippy::too_many_arguments,
    reason = "threads the shared cycle handles"
)]
async fn apply_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    wiki_id: &WikiId,
    add_owner: &Principal,
    language_directive: &str,
    source_path: &str,
    comments: &[(i64, String, Option<String>)],
    now: &str,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let facts = fact_index::find_active_by_source_path(pool, source_path).await?;
    if facts.is_empty() {
        // The anchor resolved to a page with no live facts (all forgotten, or a
        // hub). Nothing to act on — drain the comments.
        for (bi, _, _) in comments {
            stamp_processed(pool, *bi, now).await?;
            report.comments_processed += 1;
        }
        return Ok(());
    }

    // The commenter whose `add` facts are owned/sent: the single distinct author
    // among this page's pending comments. The dashboard records every comment's
    // author (`author_sender_id`), so — like a captured message — an `add`'s
    // `sender` is that human and the owner defaults to them. When a page mixes
    // authors (rare: comments are interpreted together but an `add` op carries
    // no back-reference to one comment), the most recent author represents them;
    // the LLM may still attribute a fact to a named subject via `owner_id`.
    let commenter = representative_commenter(comments);
    let scope_ctx = describe_scope(tree, wiki_id, commenter.as_ref(), pool).await;

    let facts_desc = describe_facts(&facts);
    let comments_desc = describe_comments(comments);
    // Destination context for the `move` op: the candidate wikis the source
    // wiki's owner may write into (cross-wiki moves) plus this wiki's other
    // pages (same-wiki page moves). The LLM resolves "salute" → a concrete
    // wiki/page from this list; it can only pick what is offered here.
    let destinations = describe_destinations(tree, wiki_id, source_path, add_owner);
    let system = prompts::render(
        "comment-apply",
        tree.workdir(),
        BUNDLED_COMMENT_APPLY_MD,
        &[
            ("locale", language_directive),
            ("facts", facts_desc.as_str()),
            ("comments", comments_desc.as_str()),
            ("scope", scope_ctx.as_str()),
            ("destinations", destinations.as_str()),
        ],
    )?;
    let resp = llm
        .complete(
            CompletionRequest::new("Return the JSON ops object.")
                .with_system(system)
                .with_temperature(0.1)
                .with_max_tokens(4_000),
        )
        .await?;
    let Some(parsed) = parse_json::<InterpretedOps>(&resp.text) else {
        return Err(CommentApplyError::Unparseable);
    };

    let known: HashSet<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
    let reason = move_reason(comments);

    // Fact ids a `remove` op in THIS batch targets. Ops apply in emission
    // order, and the LLM may emit a corrected `add` before its paired
    // `remove` — an `add` must never dedup against a fact the same batch
    // tombstones, or the correction would be skipped and the original removed
    // (net information loss). `apply_add` excludes these from its candidates.
    let batch_removals: HashSet<&str> = parsed
        .ops
        .iter()
        .filter(|o| o.action == "remove")
        .filter_map(|o| o.fact_id.as_deref())
        .collect();

    for op in &parsed.ops {
        match op.action.as_str() {
            "correct" => {
                apply_correct(pool, embedder.as_ref(), &facts, &known, op, report).await?;
            },
            "remove" => apply_remove(pool, tree, embedder, &known, op, report).await?,
            "add" => {
                apply_add(
                    pool,
                    embedder.as_ref(),
                    wiki_id,
                    source_path,
                    commenter.as_ref(),
                    add_owner,
                    &batch_removals,
                    op,
                    report,
                )
                .await?;
            },
            "move" => {
                apply_move(
                    pool,
                    tree,
                    &facts,
                    &known,
                    wiki_id,
                    source_path,
                    add_owner,
                    op,
                    &reason,
                    report,
                )
                .await?;
            },
            other => {
                report
                    .errors
                    .push(format!("ignored unknown op action {other:?}"));
            },
        }
    }

    for (bi, _, _) in comments {
        stamp_processed(pool, *bi, now).await?;
        report.comments_processed += 1;
    }
    Ok(())
}

/// Correct an existing fact's claim in place, preserving its offsets + ACL.
async fn apply_correct(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    facts: &[fact_index::FactIndexRow],
    known: &HashSet<&str>,
    op: &RawOp,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let (Some(fid_str), Some(text)) = (op.fact_id.as_deref(), op.text.as_deref()) else {
        report
            .errors
            .push("correct missing fact_id or text".to_owned());
        return Ok(());
    };
    if !known.contains(fid_str) {
        // Cross-page / hallucinated id — refuse (containment guard).
        report
            .errors
            .push(format!("correct refused: fact {fid_str} not on this page"));
        return Ok(());
    }
    let Ok(fid) = FactId::parse(fid_str) else {
        report
            .errors
            .push(format!("correct refused: malformed fact_id {fid_str}"));
        return Ok(());
    };
    let row = facts
        .iter()
        .find(|f| f.fact_id.as_str() == fid_str)
        .expect("fid is in `known`, built from `facts`");
    let embedding = embedder
        .embed(&crate::parser::strip_embed_markers(text))
        .await?;
    // `RegionUpdate` carries no ACL fields — the correction touches the
    // claim text only, the row's ACL columns stay untouched by design.
    let update = RegionUpdate {
        region_start: row.region_start,
        region_end: row.region_end,
        text: text.to_owned(),
        embedding,
    };
    if fact_index::update_region(pool, &fid, &update).await? > 0 {
        report.facts_corrected += 1;
    }
    Ok(())
}

/// Tombstone an existing fact a comment asks to forget, and excise its
/// on-disk region ([`crate::capture::wiki_forget`] owns both halves; the
/// strip inside it is best-effort and never fails the removal).
async fn apply_remove(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    known: &HashSet<&str>,
    op: &RawOp,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let Some(fid_str) = op.fact_id.as_deref() else {
        report.errors.push("remove missing fact_id".to_owned());
        return Ok(());
    };
    if !known.contains(fid_str) {
        report
            .errors
            .push(format!("remove refused: fact {fid_str} not on this page"));
        return Ok(());
    }
    let Ok(fid) = FactId::parse(fid_str) else {
        report
            .errors
            .push(format!("remove refused: malformed fact_id {fid_str}"));
        return Ok(());
    };
    let outcome =
        crate::capture::wiki_forget(tree, pool, embedder.clone(), &fid, REASON_COMMENT_REMOVE)
            .await
            .map_err(|e| match e {
                crate::capture::CaptureError::FactIndex(err) => CommentApplyError::FactIndex(err),
                other => {
                    CommentApplyError::Wiki(WikiError::Io(std::io::Error::other(other.to_string())))
                },
            })?;
    if outcome.tombstoned {
        report.facts_removed += 1;
    }
    Ok(())
}

/// Insert a new fact on the anchored page. A fact is a fact: its subject and
/// audience follow the same rules as a captured message — the LLM decides
/// `owner`/`allow` (subject + audience from the comment, the page's wiki scope,
/// and the commenter's group scopes), and the `sender` is the human who left
/// the comment (`commenter`, recorded as `author_sender_id`).
///
/// Defaults mirror the message path: an absent/malformed `owner_id` falls back
/// to the commenter (the fact's author), and if even the commenter is unknown
/// to `fallback_owner` (the wiki's resolved scope principal) so the fact is
/// never ownerless. Malformed `allow_ids` entries are dropped — never widen on
/// a parse slip.
///
/// Before the insert the add passes the same write-time dedup the capture path
/// applies everywhere else ([`crate::capture::best_dedup_candidate`]): an add
/// that is really a rephrase of an existing same-owner fact is skipped —
/// capture-`Skipped` semantics, nothing is inserted or merged — counted in
/// `facts_deduped` (`facts_added` is not bumped). Facts a `remove` op in the
/// same batch targets (`batch_removals`) never count as dedup candidates: the
/// add may be the corrected replacement of a fact the batch is about to
/// tombstone.
#[allow(
    clippy::too_many_arguments,
    reason = "the add threads the wiki/page, the commenter, the fallback owner, the batch removals, the op, and the report"
)]
async fn apply_add(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    wiki_id: &WikiId,
    source_path: &str,
    commenter: Option<&Principal>,
    fallback_owner: &Principal,
    batch_removals: &HashSet<&str>,
    op: &RawOp,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let Some(text) = op.text.as_deref() else {
        report.errors.push("add missing text".to_owned());
        return Ok(());
    };
    let owner_id = op
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<Principal>().ok())
        .or_else(|| commenter.cloned())
        .unwrap_or_else(|| fallback_owner.clone());
    let allow_ids: Vec<Principal> = op
        .allow_ids
        .iter()
        .filter_map(|s| s.trim().parse::<Principal>().ok())
        .collect();

    // Write-time dedup, exactly as the capture path applies at every other
    // write site: an `add` that merely rephrases an existing fact is skipped
    // rather than minting a near-duplicate row. Same discipline as
    // `capture::wiki_capture` — same-owner scope, the channel-page boundary,
    // jaccard 6-gram against the wiki's active facts, and the embed-set guard
    // (two distinct media with near-identical captions stay two facts). Dedup
    // first so a hit short-circuits before the (possibly remote) embed call.
    // Facts a `remove` op in the same batch targets are not candidates: ops
    // apply in emission order, so the doomed fact may still be active while
    // an earlier `add` carries its corrected replacement.
    let mut candidates = fact_index::find_active_in_wiki(pool, wiki_id.as_str()).await?;
    candidates.retain(|c| !batch_removals.contains(c.fact_id.as_str()));
    let on_channel_page = crate::wiki::is_channel_page(source_path);
    if let Some((dup, score)) =
        crate::capture::best_dedup_candidate(&candidates, &owner_id, on_channel_page, text, None)
        && score >= crate::recall::DEFAULT_DEDUP_THRESHOLD
    {
        if crate::parser::collect_embeds(&dup.text) == crate::parser::collect_embeds(text) {
            tracing::info!(
                wiki_id = wiki_id.as_str(),
                matched_fact_id = dup.fact_id.as_str(),
                similarity = score,
                "comment-apply add: SKIPPED (dedup hit)"
            );
            report.facts_deduped += 1;
            return Ok(());
        }
        tracing::info!(
            wiki_id = wiki_id.as_str(),
            matched_fact_id = dup.fact_id.as_str(),
            similarity = score,
            "comment-apply add: dedup hit overridden — embed sets differ (different media, distinct facts)"
        );
    }

    let raw = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
    let fid = FactId::parse(&raw.to_string()).expect("uuid v7 is a valid FactId");
    let embedding = embedder
        .embed(&crate::parser::strip_embed_markers(text))
        .await?;
    let new = NewFact {
        fact_id: fid,
        wiki_id: wiki_id.as_str().to_owned(),
        source_path: source_path.to_owned(),
        region_start: None,
        region_end: None,
        text: text.to_owned(),
        embedding,
        owner_id,
        allow_ids,
        sender_id: commenter.cloned(),
        fact_type: None,
        topics: Vec::new(),
        valid_from: None,
        valid_to: None,
        // Inert: re-derived/non-ingest fact — no classifier placement
        // proposal to carry.
        target_page: None,
        style: None,
        page_description: None,
        salience: None,
        source_ref: None,
        // Dashboard comment-apply is not a smart-consumer authoring turn.
        authored_refs: Vec::new(),
    };
    fact_index::insert(pool, &new).await?;
    report.facts_added += 1;
    Ok(())
}

/// Relocate one of THIS page's facts, following the operator's commented
/// intent. Two shapes, both reusing the engine REM already owns:
///
/// - **cross-wiki** (`op.dest_wiki_id` set and ≠ this wiki): the fact is
///   refiled onto the destination wiki's `index.md` via
///   [`promote::apply_fact_refile_direct`]. The destination must locate, be
///   **owned by the same owner** as the source (no cross-owner move), and be
///   **standard** (a smart wiki is the consumer's — refused).
/// - **same-wiki page move** (`dest_wiki_id` absent / == this wiki, with a
///   `dest_page` that differs from the source page): the fact moves to that
///   page via [`promote::apply_paragraph_to_file_direct`].
///
/// Containment holds exactly as for `correct` / `remove`: the `fact_id` MUST be
/// on this page (`known`), else the op is refused. A `dest_wiki_id` that does
/// not locate is a no-op refusal (anti-hallucination), and a move with neither
/// a different page nor a different wiki is a no-op. On success the move is
/// **born-applied + revertible** — the `_direct` wrappers mint the receipt.
#[allow(
    clippy::too_many_arguments,
    reason = "the contained move threads the page facts, containment set, source wiki/page, owner, the op, and the receipt reason"
)]
async fn apply_move(
    pool: &SqlitePool,
    tree: &WikiTree,
    facts: &[fact_index::FactIndexRow],
    known: &HashSet<&str>,
    wiki_id: &WikiId,
    source_path: &str,
    add_owner: &Principal,
    op: &RawOp,
    reason: &str,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let Some(fid_str) = op.fact_id.as_deref() else {
        report.errors.push("move missing fact_id".to_owned());
        return Ok(());
    };
    if !known.contains(fid_str) {
        // Cross-page / hallucinated id — refuse (containment guard).
        report
            .errors
            .push(format!("move refused: fact {fid_str} not on this page"));
        return Ok(());
    }
    let Ok(fid) = FactId::parse(fid_str) else {
        report
            .errors
            .push(format!("move refused: malformed fact_id {fid_str}"));
        return Ok(());
    };
    let row = facts
        .iter()
        .find(|f| f.fact_id.as_str() == fid_str)
        .expect("fid is in `known`, built from `facts`");
    let recipient = proposals::recipient_from_fact(&row.owner_id, row.sender_id.as_ref());

    // Is this a cross-wiki move? Only when a dest wiki is named AND it differs
    // from the source wiki; an absent or same-wiki dest_wiki_id is a same-wiki
    // page move.
    let dest_wiki_id = op
        .dest_wiki_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != wiki_id.as_str());

    if let Some(dest_wiki_id) = dest_wiki_id {
        apply_move_cross_wiki(
            pool,
            tree,
            &fid,
            wiki_id,
            source_path,
            dest_wiki_id,
            add_owner,
            recipient,
            reason,
            report,
        )
        .await
    } else {
        apply_move_same_wiki(
            pool,
            tree,
            &fid,
            wiki_id,
            source_path,
            op,
            recipient,
            reason,
            report,
        )
        .await
    }
}

/// Cross-wiki branch of [`apply_move`]: validate the destination wiki, then
/// refile the fact onto its `index.md`.
#[allow(
    clippy::too_many_arguments,
    reason = "the cross-wiki branch carries the fact, both wiki endpoints, source page, owner, recipient, and reason"
)]
async fn apply_move_cross_wiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    fid: &FactId,
    wiki_id: &WikiId,
    source_path: &str,
    dest_wiki_id: &str,
    add_owner: &Principal,
    recipient: Option<String>,
    reason: &str,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let Ok(dest_id) = WikiId::parse(dest_wiki_id) else {
        report.errors.push(format!(
            "move refused: malformed dest_wiki_id {dest_wiki_id}"
        ));
        return Ok(());
    };
    // The destination must locate (anti-hallucination), be standard (a smart
    // wiki is the consumer's), and be owned by the SAME owner as the source
    // (the page's resolved acl_default) — never a cross-owner move.
    let Ok(dest_handle) = tree.locate(&dest_id) else {
        report
            .errors
            .push(format!("move refused: dest wiki {dest_wiki_id} not found"));
        return Ok(());
    };
    if dest_handle.meta().smart {
        report.errors.push(format!(
            "move refused: dest wiki {dest_wiki_id} is smart (consumer-owned)"
        ));
        return Ok(());
    }
    let dest_owner = tree.resolve_scope_principal(dest_handle.meta())?;
    if &dest_owner != add_owner {
        report.errors.push(format!(
            "move refused: dest wiki {dest_wiki_id} owner {dest_owner} differs from source owner {add_owner}"
        ));
        return Ok(());
    }

    let source_handle = tree.locate(wiki_id)?;
    let source_page = page_wiki_relative(&source_handle, source_path);
    match promote::apply_fact_refile_direct(
        pool,
        tree,
        fid,
        wiki_id.as_str(),
        &source_page,
        dest_wiki_id,
        CROSS_WIKI_DEST_PAGE,
        Some(reason),
        recipient,
    )
    .await
    {
        Ok(_) => report.facts_moved += 1,
        // An engine refusal (the fact moved since the comment was parked, a bad
        // path) is recorded and left for the next cycle's accounting — it never
        // aborts the page's other ops.
        Err(promote::DirectPromoteError::Apply(e)) => report
            .errors
            .push(format!("move refused: cross-wiki refile of {fid}: {e}")),
        Err(promote::DirectPromoteError::Receipt(e)) => report.errors.push(format!(
            "move applied but receipt not written for {fid}: {e}"
        )),
    }
    Ok(())
}

/// Same-wiki branch of [`apply_move`]: move the fact to another page of this
/// wiki. A no-op when no `dest_page` is given or it equals the source page.
/// Closed-world: `dest_page` MUST be an existing page of this wiki (the same
/// list `describe_destinations` offers) — a move never creates a page, so a
/// hallucinated destination is refused rather than conjured into existence.
#[allow(
    clippy::too_many_arguments,
    reason = "the same-wiki branch carries the fact, wiki, source page, the op, recipient, and reason"
)]
async fn apply_move_same_wiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    fid: &FactId,
    wiki_id: &WikiId,
    source_path: &str,
    op: &RawOp,
    recipient: Option<String>,
    reason: &str,
    report: &mut CommentApplyReport,
) -> Result<()> {
    let source_handle = tree.locate(wiki_id)?;
    let source_page = page_wiki_relative(&source_handle, source_path);
    let dest_page = op.dest_page.as_deref().map_or("", str::trim);
    if dest_page.is_empty() || dest_page == source_page {
        // Neither a different wiki nor a different page — nothing to do.
        report.errors.push(format!(
            "move refused: fact {fid} has no different destination page or wiki"
        ));
        return Ok(());
    }

    // Closed-world guard: the destination must be an EXISTING page of this
    // wiki — the same bounded list `describe_destinations` offers the LLM. A
    // move relocates a fact onto a page the operator can already see; it never
    // CREATES a page, or a hallucinated `dest_page` could invent one out of
    // thin air. Refuse anything not enumerated.
    match source_handle.list_pages() {
        Ok(pages)
            if pages
                .iter()
                .any(|p| p.rel_path.to_string_lossy().replace('\\', "/") == dest_page) => {},
        Ok(_) => {
            report.errors.push(format!(
                "move refused: dest page {dest_page} is not a page of this wiki"
            ));
            return Ok(());
        },
        // A TRANSIENT enumeration failure must not consume the comment (a
        // drained refusal would): propagate, so the page's comments stay
        // unprocessed and retry next cycle. The retry re-applies safely —
        // adds dedup at write time, corrects/removes are idempotent.
        Err(e) => return Err(e.into()),
    }

    let hints = ParagraphToFileHints {
        reason: Some(reason.to_owned()),
        ..ParagraphToFileHints::default()
    };
    match promote::apply_paragraph_to_file_direct(
        pool,
        tree,
        wiki_id.as_str(),
        &source_page,
        std::slice::from_ref(fid),
        dest_page,
        &hints,
        recipient,
    )
    .await
    {
        Ok(_) => report.facts_moved += 1,
        Err(promote::DirectPromoteError::Apply(e)) => report
            .errors
            .push(format!("move refused: same-wiki move of {fid}: {e}")),
        Err(promote::DirectPromoteError::Receipt(e)) => report.errors.push(format!(
            "move applied but receipt not written for {fid}: {e}"
        )),
    }
    Ok(())
}

// ---------- helpers ----------

/// A page path relative to its wiki directory (what the `paragraph_to_file` /
/// `fact_refile` primitives expect for `source_page`), derived from the
/// workdir-relative `source_path` by stripping the wiki's `rel_dir` prefix.
/// The handlers join it back onto the wiki's `abs_dir`, so a workdir-relative
/// path would double the prefix.
fn page_wiki_relative(handle: &WikiHandle, source_path: &str) -> String {
    let rel_dir = handle.rel_dir().to_string_lossy().replace('\\', "/");
    let prefix = format!("{rel_dir}/");
    source_path
        .strip_prefix(&prefix)
        .unwrap_or(source_path)
        .to_owned()
}

/// Describe the destinations a `move` op may target, for the prompt's
/// `{destinations}` placeholder. Two bounded lists for the source wiki's
/// **owner** (`owner` is the page's resolved `acl_default`):
///
/// - **other wikis** the owner can write — every **non-smart** wiki whose
///   resolved `acl_default` equals `owner`, except the source wiki itself
///   (cross-wiki moves; a fact always lands on the dest wiki's `index.md`);
/// - **this wiki's other pages** (same-wiki page moves), the source page
///   excluded.
///
/// Bounded by a person's wiki tree (`tree.walk()` is the same scan `locate`
/// uses); a `walk`/`list_pages` error degrades to an empty section rather than
/// failing the whole sub-job (the LLM then simply emits no move).
fn describe_destinations(
    tree: &WikiTree,
    wiki_id: &WikiId,
    source_path: &str,
    owner: &Principal,
) -> String {
    let mut wikis: Vec<String> = Vec::new();
    if let Ok(discovered) = tree.walk() {
        for d in discovered {
            if &d.meta.wiki_id == wiki_id || d.meta.smart {
                continue;
            }
            // Only wikis the same owner controls — never a cross-owner target.
            if tree
                .resolve_scope_principal(&d.meta)
                .is_ok_and(|p| &p == owner)
            {
                wikis.push(format!("{} · {}", d.meta.wiki_id.as_str(), d.meta.title));
            }
        }
    }
    wikis.sort();

    let mut pages: Vec<String> = Vec::new();
    if let Ok(handle) = tree.locate(wiki_id) {
        let current = page_wiki_relative(&handle, source_path);
        if let Ok(page_infos) = handle.list_pages() {
            for p in page_infos {
                let rel = p.rel_path.to_string_lossy().replace('\\', "/");
                if rel != current {
                    pages.push(rel);
                }
            }
        }
    }
    pages.sort();

    let wikis_block = if wikis.is_empty() {
        "(none)".to_owned()
    } else {
        wikis.join("\n")
    };
    let pages_block = if pages.is_empty() {
        "(none)".to_owned()
    } else {
        pages.join("\n")
    };
    format!(
        "OTHER WIKIS (move the fact into one of these — it will land on that wiki's main page):\n{wikis_block}\n\nOTHER PAGES OF THIS WIKI (move the fact to one of these pages):\n{pages_block}"
    )
}

/// Build the one-line `reason` woven into a move receipt from the page's
/// pending comments. A bounded excerpt keeps the audit string readable.
fn move_reason(comments: &[(i64, String, Option<String>)]) -> String {
    let joined = comments
        .iter()
        .map(|(_, body, _)| body.trim())
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let excerpt: String = joined.chars().take(MOVE_REASON_EXCERPT_CHARS).collect();
    if excerpt.is_empty() {
        "dashboard comment".to_owned()
    } else {
        format!("dashboard comment: {excerpt}")
    }
}

/// One pending-comment row, as loaded by [`load_comment`].
#[derive(sqlx::FromRow)]
struct CommentRow {
    target_cite: Option<String>,
    body: String,
    author_sender_id: Option<String>,
    processed_at: Option<String>,
}

/// Load `(target_cite, body, author_sender_id)` for a pending comment, or
/// `None` if the row is gone or already processed (a racing manual drain).
/// `author_sender_id` is the bare id of the signed-in user who left the comment
/// (the dashboard stamps it on every parked comment) — the `sender`/owner
/// default an `add` op uses.
async fn load_comment(
    pool: &SqlitePool,
    bi: i64,
) -> Result<Option<(Option<String>, String, Option<String>)>> {
    let row: Option<CommentRow> = sqlx::query_as(
        "SELECT target_cite, body, author_sender_id, processed_at FROM wiki_briefing_items WHERE id = ?",
    )
    .bind(bi)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        if r.processed_at.is_some() {
            None
        } else {
            Some((r.target_cite, r.body, r.author_sender_id))
        }
    }))
}

/// Stamp a comment `processed_at`, idempotently (a no-op if already drained).
async fn stamp_processed(pool: &SqlitePool, bi: i64, now: &str) -> Result<()> {
    sqlx::query(
        "UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ? AND processed_at IS NULL",
    )
    .bind(now)
    .bind(bi)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve a comment's `target_cite` to the workdir-relative `source_path` of
/// the anchored page, matching the format the compiler stamps on `fact_index`.
/// `None` when the citation is absent, malformed, or points at another wiki.
fn resolve_source_path(
    tree: &WikiTree,
    handle: &WikiHandle,
    wiki_id: &WikiId,
    cite: Option<&str>,
) -> Option<String> {
    let parsed = briefing::parse_cite(cite?).ok()?;
    if parsed.wiki_id.as_str() != wiki_id.as_str() {
        return None;
    }
    let abs = handle.abs_dir().join(&parsed.path);
    Some(workdir_relative_source_path(tree.workdir(), &abs))
}

fn describe_facts(facts: &[fact_index::FactIndexRow]) -> String {
    facts
        .iter()
        .map(|f| format!("{}: {}", f.fact_id.as_str(), f.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_comments(comments: &[(i64, String, Option<String>)]) -> String {
    comments
        .iter()
        .enumerate()
        .map(|(i, (_, body, _))| format!("{}. {}", i + 1, body))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The commenter principal an `add` op is owned/sent by: the single distinct
/// non-empty `author_sender_id` among the page's pending comments, taken as
/// `user:<id>`. When the page mixes authors the last (most recent) one
/// represents them — a minimal, faithful choice given an `add` op carries no
/// back-reference to one comment. `None` only for legacy comments with no
/// recorded author (pre-`author_sender_id` rows) — then `apply_add` falls back
/// to the wiki's scope principal, never inventing a sender.
fn representative_commenter(comments: &[(i64, String, Option<String>)]) -> Option<Principal> {
    comments
        .iter()
        .filter_map(|(_, _, author)| author.as_deref())
        .map(str::trim)
        .rfind(|a| !a.is_empty())
        .map(|id| Principal::User(id.to_owned()))
}

/// Build the `{scope}` context block for the prompt: the commenter's id, this
/// page's wiki `scope` prose, and the commenter's group scopes — the same
/// audience signals `ingest::build_prompt` surfaces for the `owner_id`/
/// `allow_ids` decision. Best-effort: a lookup failure degrades a section to a
/// note rather than failing the page (the LLM then simply keeps the default
/// `[]` audience).
async fn describe_scope(
    tree: &WikiTree,
    wiki_id: &WikiId,
    commenter: Option<&Principal>,
    pool: &SqlitePool,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    match commenter {
        Some(c) => {
            let _ = writeln!(out, "commenter: {c}");
        },
        None => out.push_str("commenter: (unknown — legacy comment with no recorded author)\n"),
    }

    let wiki_scope = tree
        .locate(wiki_id)
        .ok()
        .and_then(|h| h.meta().scope.clone())
        .filter(|s| !s.trim().is_empty());
    match wiki_scope {
        Some(s) => {
            let _ = writeln!(out, "this page's wiki scope: {}", s.trim());
        },
        None => out.push_str("this page's wiki scope: (none configured)\n"),
    }

    out.push_str("commenter's group scopes:\n");
    let groups = match commenter {
        Some(Principal::User(id)) => crate::enrollment::groups_with_scope_for(pool, id)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if groups.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for (id, scope) in &groups {
            let _ = write!(out, "  - id: {id}\n    scope: ");
            match scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(s) => out.push_str(s),
                None => out.push_str("(no scope configured)"),
            }
            out.push('\n');
        }
    }
    out
}

/// Extract the first balanced-ish `{ … }` span and deserialize it.
fn parse_json<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<T>(&raw[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;
    use std::sync::Arc;

    async fn setup() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(wikis.join("alice/index.md"), "# Alice\n\n## bio\n\nbody.\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    fn fid_str(seed: u8) -> String {
        format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{seed:02x}")
    }

    async fn insert_fact(pool: &SqlitePool, id: &str, text: &str) {
        insert_fact_owned(pool, id, text, "user:alice").await;
    }

    async fn insert_fact_owned(pool: &SqlitePool, id: &str, text: &str, owner: &str) {
        fact_index::insert(
            pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: FactId::parse(id).unwrap(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/index.md".to_owned(),
                region_start: Some(10),
                region_end: Some(40),
                text: text.to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: owner.parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no classifier placement
                // proposal to carry.
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    async fn insert_comment(pool: &SqlitePool, cite: Option<&str>, body: &str) -> i64 {
        let ts = "2026-05-31T00:00:00Z";
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO wiki_briefing_items \
             (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, author_sender_id, processed_at) \
             VALUES ('alice','dashboard_comment','dashboard:alice','t', ?, 'external', ?, ?, 'alice', NULL) RETURNING id",
        )
        .bind(body)
        .bind(ts)
        .bind(cite)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    #[tokio::test]
    async fn correct_updates_claim_in_place_and_drains_comment() {
        let (dir, tree, pool) = setup().await;
        let id = fid_str(0x11);
        insert_fact(&pool, &id, "Alice was born in 1985").await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "the birth year is wrong, she was born in 1986",
        )
        .await;

        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"correct\",\"fact_id\":\"{id}\",\"text\":\"Alice was born in 1986\"}}]}}"
            ),
        );
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_corrected, 1);
        assert_eq!(report.comments_processed, 1);
        let row = fact_index::find_by_id(&pool, &FactId::parse(&id).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.text, "Alice was born in 1986");
        assert_eq!(row.owner_id, "user:alice".parse::<Principal>().unwrap());
        // Comment drained.
        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(processed.is_some());
        drop(dir);
    }

    #[tokio::test]
    async fn correct_for_a_fact_off_the_page_is_refused() {
        let (dir, tree, pool) = setup().await;
        let on_page = fid_str(0x21);
        insert_fact(&pool, &on_page, "Alice likes tea").await;
        let stranger = fid_str(0x99); // never inserted on this page
        let bi = insert_comment(&pool, Some("wiki://alice/index.md#bio"), "fix it").await;

        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"correct\",\"fact_id\":\"{stranger}\",\"text\":\"hijacked\"}}]}}"
            ),
        );
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(
            report.facts_corrected, 0,
            "stranger fact must not be touched"
        );
        assert!(!report.errors.is_empty(), "refusal recorded");
        // The on-page fact is untouched.
        let row = fact_index::find_by_id(&pool, &FactId::parse(&on_page).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.text, "Alice likes tea");
        // Comment still drained (we read it, there was just nothing valid to do).
        assert_eq!(report.comments_processed, 1);
        drop(dir);
    }

    #[tokio::test]
    async fn unanchored_comment_is_drained_without_action() {
        let (dir, tree, pool) = setup().await;
        insert_fact(&pool, &fid_str(0x31), "Alice likes tea").await;
        let bi = insert_comment(&pool, None, "nice page!").await;

        let llm = FakeLlmBackend::new("fake", "{\"ops\":[]}");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_corrected, 0);
        assert_eq!(report.facts_added, 0);
        assert_eq!(report.facts_removed, 0);
        assert_eq!(
            report.comments_processed, 1,
            "drained even without an anchor"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn add_defaults_owner_to_commenter_and_stamps_them_as_sender() {
        // The page mixes owners: facts[0] is `global` — the over-grant trap a
        // previous design would copy. With no LLM `owner_id`, the new fact now
        // defaults to the COMMENTER (its author, `author_sender_id=alice`) and
        // is SENT by them — never the arbitrary global facts[0], never the wiki
        // default for a fact whose author is known.
        let (dir, tree, pool) = setup().await;
        insert_fact_owned(&pool, &fid_str(0x41), "Alice's project is public", "global").await;
        insert_fact_owned(&pool, &fid_str(0x42), "Alice prefers tea", "user:alice").await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "also note she has a cat",
        )
        .await;

        // No owner_id / allow_ids on the add → the documented defaults apply.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"ops\":[{\"action\":\"add\",\"text\":\"Alice has a cat\"}]}",
        );
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_added, 1);
        let facts = fact_index::find_active_by_source_path(&pool, "wikis/alice/index.md")
            .await
            .unwrap();
        let added = facts
            .iter()
            .find(|f| f.text == "Alice has a cat")
            .expect("the added fact is present");
        assert_eq!(
            added.owner_id,
            "user:alice".parse::<Principal>().unwrap(),
            "add defaults its owner to the commenter, never the arbitrary global facts[0]"
        );
        assert!(
            added.allow_ids.is_empty(),
            "no allow_ids on the op → owner+commenter only"
        );
        assert_eq!(
            added.sender_id,
            Some("user:alice".parse::<Principal>().unwrap()),
            "the add's sender is the human who left the comment"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn add_uses_llm_owner_and_allow_under_the_ingest_rules() {
        // A fact is a fact: when the comment is ABOUT someone else and shared
        // with a group, the LLM's `owner_id`/`allow_ids` are honoured verbatim
        // (subject = Bob, audience = the family) while `sender` stays the human
        // commenter (alice).
        let (dir, tree, pool) = setup().await;
        insert_fact(&pool, &fid_str(0x51), "Alice likes tea").await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "Bob changed jobs — the family should know",
        )
        .await;

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"ops\":[{\"action\":\"add\",\"text\":\"Bob changed jobs.\",\"owner_id\":\"user:bob\",\"allow_ids\":[\"group:famiglia\"]}]}",
        );
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_added, 1);
        let facts = fact_index::find_active_by_source_path(&pool, "wikis/alice/index.md")
            .await
            .unwrap();
        let added = facts
            .iter()
            .find(|f| f.text == "Bob changed jobs.")
            .expect("the added fact is present");
        assert_eq!(
            added.owner_id,
            "user:bob".parse::<Principal>().unwrap(),
            "owner is the LLM-decided subject"
        );
        assert_eq!(
            added.allow_ids,
            vec!["group:famiglia".parse::<Principal>().unwrap()],
            "audience is the LLM-decided allow"
        );
        assert_eq!(
            added.sender_id,
            Some("user:alice".parse::<Principal>().unwrap()),
            "sender stays the human commenter"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn add_that_rephrases_an_existing_fact_is_deduped_at_write_time() {
        // An `add` op that merely restates a fact already on the wiki must be
        // skipped — the same write-time dedup the capture path applies — rather
        // than mint a near-duplicate second row.
        let (dir, tree, pool) = setup().await;
        insert_fact(
            &pool,
            &fid_str(0x61),
            "Alice adopted a black cat named Felix in 2019",
        )
        .await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "note that she has a cat",
        )
        .await;

        // The LLM over-eagerly emits an `add` restating the existing fact.
        // Owner defaults to the commenter (user:alice) — the same owner as the
        // existing fact — so same-owner dedup engages and skips it.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"ops\":[{\"action\":\"add\",\"text\":\"Alice adopted a black cat named Felix in 2019\"}]}",
        );
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(
            report.facts_added, 0,
            "a verbatim restatement is deduped at write time, not inserted"
        );
        assert_eq!(report.facts_deduped, 1, "the dedup skip is counted");
        let facts = fact_index::find_active_by_source_path(&pool, "wikis/alice/index.md")
            .await
            .unwrap();
        assert_eq!(facts.len(), 1, "no duplicate row was inserted");
        assert_eq!(report.comments_processed, 1);
        drop(dir);
    }

    #[tokio::test]
    async fn add_before_paired_remove_is_not_deduped_against_the_removed_fact() {
        // The interpreter may emit a corrected `add` BEFORE its paired
        // `remove` (ops apply in emission order). The add is a near-duplicate
        // of the fact the same batch removes; deduping it against that doomed
        // fact would skip the correction and then tombstone the original —
        // net information loss. The batch's remove targets are excluded from
        // the add's dedup candidates, so the corrected fact must be inserted
        // AND the old fact tombstoned.
        let (dir, tree, pool) = setup().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let old_text = "Alice adopted a black cat named Felix from the shelter in 2019";
        let new_text = "Alice adopted a black cat named Felix from the shelter in 2021";
        // Premise guard: the corrected text must trip the write-time dedup
        // (same owner, same wiki, similarity at/above the threshold) — the
        // test would not exercise the dedup path otherwise.
        let sim = crate::recall::jaccard_6gram(old_text, new_text);
        assert!(
            sim >= crate::recall::DEFAULT_DEDUP_THRESHOLD,
            "premise: texts must be near-duplicates (got {sim})"
        );
        let fid = capture_marker(&tree, &pool, embedder.clone(), "index.md", old_text).await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "the adoption year is wrong, it was 2021",
        )
        .await;

        // add first, remove second — the emission order the fix must survive.
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"add\",\"text\":\"{new_text}\"}},{{\"action\":\"remove\",\"fact_id\":\"{fid}\"}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(
            report.facts_added, 1,
            "the corrected add must be inserted, not deduped against the fact the batch removes: {:?}",
            report.errors
        );
        assert_eq!(
            report.facts_deduped, 0,
            "no dedup skip on the corrected add"
        );
        assert_eq!(report.facts_removed, 1, "the old fact is tombstoned");
        assert_eq!(report.comments_processed, 1);
        let facts = fact_index::find_active_by_source_path(&pool, "wikis/alice/index.md")
            .await
            .unwrap();
        assert_eq!(facts.len(), 1, "exactly the corrected fact survives");
        assert_eq!(facts[0].text, new_text);
        drop(dir);
    }

    // ---------- move ----------

    /// `setup()` + two destination wikis under the same workdir, both
    /// children of `alice` so the scope-principal derivation makes them
    /// `user:alice` (the SAME owner as `alice`):
    /// - `salute` (standard — a cross-wiki move into it is allowed);
    /// - `proj` (a SMART wiki — a cross-wiki move into it must be refused
    ///   because it is the consumer's container).
    async fn setup_with_dests() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        // `setup()`'s tree is re-opened below after the dest metas are written.
        let (dir, _tree, pool) = setup().await;
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("salute")).unwrap();
        std::fs::write(
            wikis.join("salute/_meta.md"),
            "---\nwiki_id: salute\nwiki_type: wiki-tech\nslug: salute\ntitle: Salute\nparent_wiki_id: alice\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(wikis.join("proj")).unwrap();
        std::fs::write(
            wikis.join("proj/_meta.md"),
            "---\nwiki_id: proj\nwiki_type: wiki-tech\nslug: proj\ntitle: Project\nparent_wiki_id: alice\nsmart: true\n---\n",
        )
        .unwrap();
        // Re-open so the freshly written destination metas are visible.
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    /// Capture a fact through the real `wiki_capture` path so it lands as a
    /// **marker on disk** (the `paragraph_to_file` / `fact_refile` engines
    /// locate the region by marker, unlike the bare `insert_fact` helper).
    async fn capture_marker(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        page: &str,
        body: &str,
    ) -> FactId {
        use crate::capture::{CaptureAction, CaptureRequest, wiki_capture};
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: std::path::PathBuf::from(page),
            body: body.to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
            allow: vec![],
            sender: None,
            fact_type: None,
            topics: vec![],
            dedup_threshold: Some(1.01),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let outcome = wiki_capture(tree, pool, embedder, req).await.unwrap();
        match outcome.action {
            CaptureAction::Captured { .. } => outcome.fact_id,
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn move_relocates_fact_cross_wiki_onto_dest_index_and_mints_receipt() {
        let (dir, tree, pool) = setup_with_dests().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let fid = capture_marker(
            &tree,
            &pool,
            embedder.clone(),
            "index.md",
            "Alice takes lisinopril for blood pressure",
        )
        .await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "this belongs on the salute wiki",
        )
        .await;

        // The model resolves "salute" → dest_wiki_id (dest_page null → index).
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"move\",\"fact_id\":\"{fid}\",\"dest_wiki_id\":\"salute\",\"dest_page\":null}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_moved, 1, "{:?}", report.errors);
        assert_eq!(report.comments_processed, 1);

        // The fact moved cross-wiki onto salute's index.md.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.wiki_id, "salute");
        assert_eq!(row.source_path, "wikis/salute/index.md");

        // A born-applied wiki_promote receipt (revertible) was minted.
        let receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote' AND status = 'applied'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(receipts, 1, "one born-applied move receipt");
        drop(dir);
    }

    #[tokio::test]
    async fn move_relocates_fact_same_wiki_to_another_page() {
        let (dir, tree, pool) = setup_with_dests().await;
        // The destination page must already exist — a same-wiki move relocates
        // onto an offered page, it never conjures one (closed-world rule).
        std::fs::write(
            dir.path().join("wikis/alice/work.md"),
            "# Work\n\n## standup\n\nbody.\n",
        )
        .unwrap();
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let fid = capture_marker(
            &tree,
            &pool,
            embedder.clone(),
            "index.md",
            "Alice has a standup every morning at the office",
        )
        .await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "move this to the work page",
        )
        .await;

        // Same-wiki page move: dest_wiki_id null, dest_page set.
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"move\",\"fact_id\":\"{fid}\",\"dest_wiki_id\":null,\"dest_page\":\"work.md\"}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_moved, 1, "{:?}", report.errors);
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.wiki_id, "alice", "same wiki");
        assert_eq!(row.source_path, "wikis/alice/work.md", "moved to work.md");
        drop(dir);
    }

    #[tokio::test]
    async fn move_to_a_nonexistent_same_wiki_page_is_refused() {
        // Closed-world: a same-wiki move targets an EXISTING page (one
        // `describe_destinations` offers). A `dest_page` that names no page of
        // this wiki must be refused, never conjured into existence.
        let (dir, tree, pool) = setup_with_dests().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let fid = capture_marker(
            &tree,
            &pool,
            embedder.clone(),
            "index.md",
            "Alice has a standup every morning at the office",
        )
        .await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "move this to the work page",
        )
        .await;

        // The model hallucinates `nowhere.md`, which does not exist on disk.
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"move\",\"fact_id\":\"{fid}\",\"dest_wiki_id\":null,\"dest_page\":\"nowhere.md\"}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(
            report.facts_moved, 0,
            "a move to a non-existent page must be refused (closed-world)"
        );
        assert!(
            report.errors.iter().any(|e| e.contains("not a page")),
            "refusal must cite the missing page: {:?}",
            report.errors
        );
        // The fact stayed on index.md, and the page was not created.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/index.md", "fact untouched");
        assert!(
            !dir.path().join("wikis/alice/nowhere.md").exists(),
            "no page was conjured"
        );
        let receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(receipts, 0, "no receipt on a refused move");
        drop(dir);
    }

    #[tokio::test]
    async fn move_for_a_fact_off_the_page_is_refused() {
        let (dir, tree, pool) = setup_with_dests().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        // A real on-page fact (so the page is non-empty and interpretation runs)
        // plus a stranger id never on this page.
        let _on_page = capture_marker(
            &tree,
            &pool,
            embedder.clone(),
            "index.md",
            "Alice likes tea",
        )
        .await;
        let stranger = fid_str(0x99);
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "move it to salute",
        )
        .await;

        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"move\",\"fact_id\":\"{stranger}\",\"dest_wiki_id\":\"salute\",\"dest_page\":null}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(
            report.facts_moved, 0,
            "a fact not on this page must not move (containment)"
        );
        assert!(!report.errors.is_empty(), "refusal recorded");
        let receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(receipts, 0, "no receipt on a refused move");
        drop(dir);
    }

    #[tokio::test]
    async fn move_to_a_smart_dest_wiki_is_refused() {
        let (dir, tree, pool) = setup_with_dests().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 2));
        let fid = capture_marker(
            &tree,
            &pool,
            embedder.clone(),
            "index.md",
            "Alice uses Rust on the backend",
        )
        .await;
        let bi = insert_comment(
            &pool,
            Some("wiki://alice/index.md#bio"),
            "this is really a project note",
        )
        .await;

        // The model (wrongly) targets the SMART wiki `proj`; the engine refuses.
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"ops\":[{{\"action\":\"move\",\"fact_id\":\"{fid}\",\"dest_wiki_id\":\"proj\",\"dest_page\":null}}]}}"
            ),
        );
        let wiki_id = WikiId::parse("alice").unwrap();

        let report = apply_comments(
            &pool,
            &tree,
            &embedder,
            &llm,
            &wiki_id,
            &[bi],
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("apply");

        assert_eq!(report.facts_moved, 0, "a smart dest must be refused");
        assert!(
            report.errors.iter().any(|e| e.contains("smart")),
            "refusal must cite the smart dest: {:?}",
            report.errors
        );
        // The fact stayed in alice.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.wiki_id, "alice", "fact untouched");
        drop(dir);
    }
}
