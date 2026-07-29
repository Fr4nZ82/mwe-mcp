// SPDX-License-Identifier: AGPL-3.0-or-later
//! Capture / supersede / forget / link — first write-side flow.
//!
//! This module is the orchestration layer that ties the three
//! floors (parser, filesystem surface, `fact_index`) into the four atomic
//! internal APIs the `wiki_ingest_message` LLM and the dashboard call:
//!
//! - `_internal.wiki_capture` — append a new region to a page, embed
//!   the body, dedup against existing facts in the same wiki, insert
//!   the index row.
//! - `_internal.wiki_supersede` — perform a capture **and** flag a
//!   previous fact as superseded by the new id.
//! - `_internal.wiki_forget` — tombstone an index row with a reason.
//!   The filesystem stays untouched (the marker becomes orphaned in
//!   the file; the re-index leaves it alone since `deleted_at` is the
//!   authoritative tombstone). A future `wiki_lint` can flag the
//!   orphan for operator cleanup.
//! - `_internal.wiki_link` — append a `[[wiki_id/page]]` link to a
//!   page. No `fact_index` touch (links are not facts).
//!
//! ## Why the rest of the spec is *not* here
//!
//! - **Applicative WAL wrapping.** The capture itself does not need a
//!   WAL: the `fact_index` insert is its **commit point** (DB row
//!   first, page render second — a failed page write compensates by
//!   tombstoning the row, and a crash in between leaves a pending
//!   render the next compile re-emits; see
//!   capture & dedup).
//!   Multi-step structural writes elsewhere (the REM proposal kinds)
//!   keep their `proposal_ops_log` journaling.
//! - **Cross-user attribution constraints.** Per
//!   [the memory model](../../../docs/concepts/memory-model.md),
//!   when `sender != owner` the sender must have read access to the
//!   owner's wiki. The check is a later tightening — the agent
//!   composing the call today is the only writer surface, and it is
//!   trusted.
//!
//! The four functions here are the floor; later milestones layer
//! policy on top.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

use crate::embedder::{Embedder, EmbedderError};
use crate::fact_index::{self, FactIndexError, FactIndexRow, NewFact};
use crate::recall::{self, DEFAULT_DEDUP_THRESHOLD};
use crate::types::{CatalogId, FactId, FactIdParseError, Principal, WikiId};
use crate::wiki::{WikiError, WikiTree, atomic_write, is_safe_page_path};

/// Errors raised by the capture orchestration.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// Underlying filesystem layer error.
    #[error("capture wiki io: {0}")]
    Wiki(#[from] WikiError),

    /// Underlying fact-index error.
    #[error("capture fact_index: {0}")]
    FactIndex(#[from] FactIndexError),

    /// Underlying embedder error.
    #[error("capture embedder: {0}")]
    Embedder(#[from] EmbedderError),

    /// Underlying `SQLite` error (used by `wiki_forget` / `wiki_link`
    /// which do not go through `fact_index::insert`).
    #[error("capture db: {0}")]
    Db(#[from] sqlx::Error),

    /// Underlying IO error from a low-level filesystem call.
    #[error("capture io: {0}")]
    Io(#[from] std::io::Error),

    /// Body submitted to `wiki_capture` is empty or whitespace-only.
    /// We refuse: a region with no content cannot be embedded usefully
    /// and would skew dedup scoring.
    #[error("capture body is empty")]
    EmptyBody,

    /// Body contains a literal `{{` or `}}` — the user-facing API
    /// accepts plain prose, not pre-formatted markers, so we reject
    /// rather than risk producing nested or malformed regions.
    #[error("capture body must not contain literal {{{{ or }}}} (markers are managed by mwe-mcp)")]
    BodyContainsMarker,

    /// Page path failed [`is_safe_page_path`]. Wraps the offending
    /// path so the caller can echo it back to the agent.
    #[error("capture page path {path:?} is not safe inside a wiki")]
    UnsafePagePath {
        /// The offending path the caller supplied.
        path: PathBuf,
    },

    /// Creating this page would collide with an existing page or a
    /// reserved filename on a case-insensitive filesystem (the smart
    /// consumer's local mirror), or its `.md` extension is not spelled
    /// in the byte-exact lowercase form the link grammar strips and
    /// re-appends. Refused at create time so the server and the mirror
    /// can never disagree about which file it is.
    #[error("capture page path {path:?}: {reason}")]
    PageCaseConflict {
        /// The offending path the caller supplied.
        path: PathBuf,
        /// What it collides with, in caller-echoable prose.
        reason: String,
    },

    /// Fact id supplied by [`wiki_supersede`] did not resolve to a row
    /// in `fact_index`.
    #[error("capture: previous fact_id {0} not found in fact_index")]
    PreviousFactNotFound(FactId),

    /// Generated `fact_id` failed the type's parse check (vanishingly
    /// rare — surfaces only if `uuid` ever stops producing canonical
    /// v7s). Carried as its own variant so the test for it is explicit.
    #[error("capture: generated UUIDv7 failed validation: {0}")]
    GeneratedFactIdInvalid(#[from] FactIdParseError),

    /// Cross-user attribution invariant violation
    /// (see capture & dedup):
    /// `sender` was supplied and already appears in `allow` (redundant —
    /// the [`can_read`] algorithm auto-grants read to `sender`, so
    /// listing it again is a code-smell that usually signals copy/paste).
    #[error(
        "capture: sender {0} is already in allow list (redundant — sender already grants read)"
    )]
    SenderRedundantInAllow(Principal),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, CaptureError>;

/// Tombstone reason stamped on a fact whose page write failed after the
/// `fact_index` row had already committed (the compensation path of the
/// DB-row-first write order).
pub const REASON_FILE_WRITE_FAILED: &str = "capture_file_write_failed";

// ---------- Request / Outcome types ----------

/// Input to [`wiki_capture`].
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// Target wiki.
    pub wiki_id: WikiId,
    /// Page within the wiki (relative to the wiki directory).
    pub page: PathBuf,
    /// Prose body of the new region. No markers — capture wraps it
    /// with the bare runtime form `{{f=…}}body{{/}}` (the ACL goes into
    /// the `fact_index` columns, not the marker).
    pub body: String,
    /// Owning principal.
    pub owner: Principal,
    /// Additional principals granted read access via `allow=`.
    pub allow: Vec<Principal>,
    /// Cross-user attribution (who *captured* the fact, orthogonal to
    /// `owner`). `None` on input is materialized to `owner` by
    /// [`normalize_sender_attribution`] — `sender` is always stored as a
    /// distinct, explicit field, never collapsed into `owner`.
    pub sender: Option<Principal>,
    /// Optional fact taxonomy hint (`bio`, `preference`, `episode`, …).
    pub fact_type: Option<String>,
    /// Optional topic tags, denormalised into the index for SQL filter
    /// queries.
    pub topics: Vec<String>,
    /// Jaccard threshold above which the capture short-circuits as a
    /// duplicate. `None` ⇒ [`recall::DEFAULT_DEDUP_THRESHOLD`].
    pub dedup_threshold: Option<f32>,
    /// Per-fact validity interval, threaded from the ingest
    /// extraction into [`fact_index::NewFact`]. `valid_from` = when the fact
    /// starts holding (`None` = open-start); `valid_to` = when it stops
    /// (`None` = OPEN, "true now, no known end"). The classifier resolves both
    /// against `current_time`; here they are an opaque pass-through to storage.
    pub valid_from: Option<String>,
    /// End of the validity interval; `None` = OPEN. See [`Self::valid_from`].
    pub valid_to: Option<String>,
    /// Per-page placement hints the classifier deduced,
    /// forwarded from the ingest extraction so the write/compile
    /// path can place the fact on the right subject page. `style` =
    /// `prosa` | `prosa-tecnica` | `lista`; the dominant writing register of the
    /// target page. Inert pass-through for now — no consumer until later
    /// stages wire it through buffer→promote→compile.
    pub style: Option<String>,
    /// Per-page "what goes here" one-liner that aids future placement. See
    /// [`Self::style`]. Inert pass-through for now.
    pub page_description: Option<String>,
    /// Per-fact salience the ingest classifier deduced,
    /// threaded into [`fact_index::NewFact`] (direct path) and onto the buffer
    /// (standard-wiki path). `high | normal | low`; `None` = unspecified. Opaque
    /// pass-through to storage here; the promote step routes `high` facts to
    /// `index.md`.
    pub salience: Option<String>,
    /// Project-wiki pages this fact's turn authored, as plain
    /// `[[wiki_id/page]]` wikilinks ([`fact_index::NewFact::authored_refs`]).
    /// Threaded from `wiki_ingest_message`'s `metadata.authored_refs` so
    /// consolidation links to the project page instead of duplicating its
    /// body (roadmap group 17). Empty for a pure-standard capture.
    pub authored_refs: Vec<String>,
}

/// Outcome of [`wiki_capture`] / [`wiki_supersede`].
#[derive(Debug, Clone)]
pub struct CaptureOutcome {
    /// `fact_id` of the newly captured row (every action — including
    /// `Skipped` — assigns a fresh id so the caller can correlate
    /// audit log entries).
    pub fact_id: FactId,
    /// What the orchestrator decided to do.
    pub action: CaptureAction,
}

/// One of three terminal states for a capture call.
#[derive(Debug, Clone)]
pub enum CaptureAction {
    /// New region appended to the page, embedding inserted into
    /// `fact_index`, no dedup hit.
    Captured {
        /// Path of the page the region was appended to, relative to
        /// the workdir.
        source_path: String,
        /// Byte offset of the opening `{{f=…}}` marker.
        region_start: i64,
        /// Byte offset one past the closing `{{/}}` marker.
        region_end: i64,
    },
    /// Dedup short-circuit: the request matched an existing active
    /// fact above the threshold; the filesystem and `fact_index` were
    /// not touched.
    ///
    /// The `matched_fact_id` is the existing row the new request
    /// collided with; `similarity` is its jaccard score.
    Skipped {
        /// Fact id of the active row the request collided with.
        matched_fact_id: FactId,
        /// Jaccard similarity that triggered the skip.
        similarity: f32,
    },
    /// `wiki_supersede`-only: a fresh capture was performed *and* an
    /// existing fact was flagged as superseded by the new id.
    Superseded {
        /// Path of the page the new region was appended to.
        source_path: String,
        /// Byte offset of the opening `{{f=…}}` marker.
        region_start: i64,
        /// Byte offset one past the closing `{{/}}` marker.
        region_end: i64,
        /// `fact_id` of the row that just got superseded.
        previous_fact_id: FactId,
    },
}

/// Outcome of [`wiki_forget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetOutcome {
    /// True when the row transitioned from active to tombstoned.
    /// Idempotent calls (already-deleted, or never existed) return false.
    pub tombstoned: bool,
}

/// Outcome of [`wiki_link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOutcome {
    /// Path of the page that received the new link, relative to the
    /// workdir.
    pub source_path: String,
    /// Byte offset of the appended link's first character.
    pub link_start: i64,
    /// Byte offset one past the appended link.
    pub link_end: i64,
}

// ---------- dedup candidate scan (shared with the light dream) ----------

/// Scan `candidates` for the best jaccard 6-gram match against `body`
/// under the capture dedup discipline — shared by the direct write path
/// and the light-dream promotion, so a buffered capture gets exactly
/// the dedup a live write would have gotten:
///
/// - **Same owner only.** A fact dedups only against facts owned by the
///   same principal. Different owner ⇒ different fact — two senders
///   adding to a shared `group:` page collapse to one item (same
///   owner), but per-user facts that merely share a wiki (an agent's
///   behaviour rules, each owned by the user who dictated it) never
///   collide. Preserves per-fragment ownership.
/// - **Never across the channel-page boundary** ([`crate::wiki::is_channel_page`]:
///   both sides on a reserved channel page, or neither): a new behaviour
///   rule must not be skipped as a duplicate of an ordinary fact that
///   happens to restate it (the rule would then never reach `rules.md`, so
///   the behaviour-rules channel would never serve it), and the same holds
///   for a project signpost on `projects.md`. Rule-vs-rule and
///   signpost-vs-signpost still dedup.
/// - `exclude` skips one fact id — a light-dream retry after a partial
///   promotion must not dedup a capture against its own fact.
///
/// Embed markers are stripped from both sides before comparison (a
/// catalog id is a key, not prose). The EMBED-SET guard — two different
/// photos with near-identical captions are two facts, not a duplicate —
/// stays with the caller, which decides what a hit means.
pub(crate) fn best_dedup_candidate<'a>(
    candidates: &'a [FactIndexRow],
    owner: &Principal,
    on_channel_page: bool,
    body: &str,
    exclude: Option<&FactId>,
) -> Option<(&'a FactIndexRow, f32)> {
    let needle = recall::ngrams(
        &crate::parser::strip_embed_markers(body),
        recall::DEFAULT_NGRAM,
    );
    let mut best: Option<(&FactIndexRow, f32)> = None;
    for row in candidates {
        if exclude.is_some_and(|id| id == &row.fact_id) {
            continue;
        }
        if &row.owner_id != owner {
            continue;
        }
        if crate::wiki::is_channel_page(&row.source_path) != on_channel_page {
            continue;
        }
        let hay = recall::ngrams(
            &crate::parser::strip_embed_markers(&row.text),
            recall::DEFAULT_NGRAM,
        );
        let score = recall::jaccard_sets(&needle, &hay);
        if score > best.as_ref().map_or(-1.0, |(_, s)| *s) {
            best = Some((row, score));
        }
    }
    best
}

// ---------- wiki_capture ----------

/// `_internal.wiki_capture` — append a new region to a page, embed the
/// body, dedup, persist to `fact_index`.
///
/// Steps (in order — see module docs for the WAL-wrap deferral note):
///
/// 1. Validate the request (body non-empty, no markers in body, safe
///    page path).
/// 2. Locate the target wiki (errors if the wiki id is unknown).
/// 3. Embed the body via the supplied [`Embedder`].
/// 4. Fetch every active fact in the wiki, compute jaccard 6-gram on
///    the body, take the max score.
/// 5. If score ≥ `dedup_threshold` → `CaptureAction::Skipped`.
/// 6. Otherwise: render the marker region and `insert` the index row
///    (offsets NULL — the commit point), then `atomic_write` the page
///    and stamp the rendered offsets. A failed page write compensates
///    by tombstoning the row ([`REASON_FILE_WRITE_FAILED`]).
///
/// # Errors
///
/// See [`CaptureError`].
pub async fn wiki_capture(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    req: CaptureRequest,
) -> Result<CaptureOutcome> {
    wiki_capture_with_source(tree, pool, embedder, req, None).await
}

/// [`wiki_capture`] variant stamping a source-document provenance
/// (`fact_index.source_ref`) onto the fact.
///
/// The document-ingest anchor path
/// (document ingest);
/// conversational captures use the plain [`wiki_capture`] (no provenance).
///
/// # Errors
///
/// See [`CaptureError`].
#[allow(clippy::too_many_lines)] // orchestrator reads top-to-bottom, splitting hides the flow
pub async fn wiki_capture_with_source(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    mut req: CaptureRequest,
    source_ref: Option<String>,
) -> Result<CaptureOutcome> {
    validate_body(&req.body)?;
    if !is_safe_page_path(&req.page) {
        return Err(CaptureError::UnsafePagePath {
            path: req.page.clone(),
        });
    }
    normalize_sender_attribution(&mut req)?;
    let handle = tree.locate(&req.wiki_id)?;
    // When the capture would CREATE the page file, refuse names that a
    // case-insensitive mirror would collapse onto an existing entry or
    // a reserved file, and `.md` spelled in a case the index ignores.
    // Appends to an existing byte-exact page carry no such risk.
    if !crate::wiki::page_exists_byte_exact(handle.abs_dir(), &req.page)
        && let Some(reason) = crate::wiki::page_path_case_hazard(&req.page)
            .or_else(|| crate::wiki::page_case_conflict(handle.abs_dir(), &req.page))
    {
        return Err(CaptureError::PageCaseConflict {
            path: req.page.clone(),
            reason,
        });
    }
    let wiki_id_str = req.wiki_id.as_str().to_owned();
    tracing::debug!(
        wiki_id = %wiki_id_str,
        page = %req.page.display(),
        owner = %req.owner,
        body_len = req.body.len(),
        "capture: validated request"
    );

    // Similarity surfaces (embedding + n-gram dedup) compare the WORDS:
    // an embed marker is a key, not prose — two different photos with
    // near-identical captions must not be told apart (or collide) by
    // their catalog ids. The stored body keeps the markers.
    let similarity_body = crate::parser::strip_embed_markers(&req.body);

    // Embed first so a remote-embedder failure short-circuits before
    // we touch anything durable.
    let embedding = embedder.embed(&similarity_body).await?;
    tracing::debug!(
        wiki_id = %wiki_id_str,
        model = embedder.model_id(),
        dim = embedding.len(),
        "capture: embedded body"
    );

    // Dedup against active facts in the same wiki.
    // Floor at 0; no ceiling — callers may pass >1.0 to mean "off"
    // (wiki_supersede relies on this).
    let threshold = req
        .dedup_threshold
        .unwrap_or(DEFAULT_DEDUP_THRESHOLD)
        .max(0.0);
    let candidates = fact_index::find_active_in_wiki(pool, &wiki_id_str).await?;
    let on_channel_page = crate::wiki::is_channel_page(&req.page.to_string_lossy());
    let best = best_dedup_candidate(&candidates, &req.owner, on_channel_page, &req.body, None);
    tracing::debug!(
        wiki_id = %wiki_id_str,
        candidates = candidates.len(),
        threshold,
        best_score = best.as_ref().map(|(_, s)| *s),
        best_id = best.as_ref().map(|(row, _)| row.fact_id.as_str().to_owned()),
        "capture: dedup scoring done"
    );
    if let Some((matched_row, similarity)) = &best
        && *similarity >= threshold
    {
        // Embed-set guard: words alone may match while the media links
        // differ — two different photos with near-identical captions
        // are two facts, not a duplicate. Equal embed sets (the same
        // photo re-sent) still dedup away.
        let matched_embeds = crate::parser::collect_embeds(&matched_row.text);
        if matched_embeds == crate::parser::collect_embeds(&req.body) {
            // We still mint a fresh id so the caller's audit row has an
            // anchor it can log — but no filesystem or index write.
            let fresh = new_fact_id()?;
            tracing::info!(
                wiki_id = %wiki_id_str,
                page = %req.page.display(),
                matched_fact_id = matched_row.fact_id.as_str(),
                similarity,
                threshold,
                "capture: SKIPPED (dedup hit)"
            );
            return Ok(CaptureOutcome {
                fact_id: fresh,
                action: CaptureAction::Skipped {
                    matched_fact_id: matched_row.fact_id.clone(),
                    similarity: *similarity,
                },
            });
        }
        tracing::info!(
            wiki_id = %wiki_id_str,
            matched_fact_id = matched_row.fact_id.as_str(),
            similarity,
            "capture: dedup hit overridden — embed sets differ (different media, distinct facts)"
        );
    }

    // Generate id, render the bare runtime marker, compute the new page
    // contents + region offsets. The ACL goes into the fact_index row
    // below — the DB column is the source of truth, the marker carries
    // the key only.
    let fact_id = new_fact_id()?;
    let marker = render_marker(&fact_id, &req.body);
    let abs_page = handle.abs_dir().join(&req.page);
    let (new_contents, region_start, region_end) = append_region(&abs_page, &marker)?;

    // DB row FIRST, file second: the insert is the capture's commit
    // point — the authoritative record (ACL, claim text, validity) is
    // complete before any render exists, so a crash can never resurrect
    // a fact with a degraded ACL from the marker alone. Offsets stay
    // NULL on insert: offsets mean "rendered on disk", and the marker is
    // not on disk yet — the reindex existence sweep exempts offset-less
    // rows as pending renders.
    let source_path = crate::wiki::workdir_relative_source_path(tree.workdir(), &abs_page);
    let row = NewFact {
        fact_id: fact_id.clone(),
        wiki_id: wiki_id_str.clone(),
        source_path: source_path.clone(),
        region_start: None,
        region_end: None,
        text: req.body,
        embedding,
        owner_id: req.owner,
        allow_ids: req.allow,
        sender_id: req.sender,
        fact_type: req.fact_type,
        topics: req.topics,
        // Validity threaded from the ingest extraction (via
        // CaptureRequest). `valid_to: None` = open horizon. `decay_reason` stays
        // NULL at insert — a fresh fact is alive; it is closed only later.
        valid_from: req.valid_from,
        valid_to: req.valid_to,
        // The ingest placement axis. On the direct path the
        // fact is already homed (real source_path), so this is the proposal the
        // classifier made — carried for parity with the standard-wiki path and for
        // the live-creation path that seeds a page's testata from it.
        target_page: Some(req.page.to_string_lossy().into_owned()),
        style: req.style,
        page_description: req.page_description,
        // Per-fact salience, opaque pass-through onto the
        // fact (the promote step routes `high` facts to the actor-wiki index.md).
        salience: req.salience,
        source_ref,
        // Group-17 provenance breadcrumbs threaded from the ingest turn.
        authored_refs: req.authored_refs,
    };
    fact_index::insert(pool, &row).await?;

    if let Err(e) = atomic_write(&abs_page, new_contents.as_bytes()) {
        // The consumer is about to receive an error, so the committed
        // row must not stay alive. Best-effort: if the tombstone fails
        // too, the row survives as a pending render the next compile
        // re-emits — an at-least-once residue, never an ACL leak.
        if let Err(tomb) =
            fact_index::mark_forgotten(pool, &fact_id, REASON_FILE_WRITE_FAILED).await
        {
            tracing::error!(
                wiki_id = %wiki_id_str,
                fact_id = fact_id.as_str(),
                error = %tomb,
                "capture: page write failed AND compensation tombstone failed — row left as pending render"
            );
        }
        return Err(e.into());
    }

    // Stamp the rendered offsets. Best-effort: the capture is already
    // durable (row + page), so an offsets hiccup must not fail it — the
    // reindex offset repair or the next compile repoint heals it.
    if let Err(e) = fact_index::move_region(
        pool,
        &fact_id,
        &source_path,
        Some(i64::try_from(region_start).unwrap_or(i64::MAX)),
        Some(i64::try_from(region_end).unwrap_or(i64::MAX)),
    )
    .await
    {
        tracing::warn!(
            wiki_id = %wiki_id_str,
            fact_id = fact_id.as_str(),
            error = %e,
            "capture: rendered-offsets stamp failed (row stays a pending render until repaired)"
        );
    }
    tracing::info!(
        wiki_id = %wiki_id_str,
        fact_id = fact_id.as_str(),
        source_path,
        region_start,
        region_end,
        "capture: CAPTURED"
    );

    Ok(CaptureOutcome {
        fact_id,
        action: CaptureAction::Captured {
            source_path,
            region_start: i64::try_from(region_start).unwrap_or(i64::MAX),
            region_end: i64::try_from(region_end).unwrap_or(i64::MAX),
        },
    })
}

// ---------- wiki_supersede ----------

/// `_internal.wiki_supersede` — capture a new region and flag the
/// previous fact as retired.
///
/// Algorithm:
/// 1. Validate that `old_fact_id` exists in `fact_index` (errors with
///    `PreviousFactNotFound` otherwise).
/// 2. Delegate to [`wiki_capture`] with `dedup_threshold = 1.01` so
///    the dedup check is disabled — supersede is explicit intent.
/// 3. If the inner capture returned `Captured`, call
///    `fact_index::mark_superseded(old, new)` and return `Superseded`.
///    Other outcomes are surfaced verbatim (in practice only
///    `Captured` is possible since dedup is off).
///
/// # Errors
///
/// As [`wiki_capture`] plus [`CaptureError::PreviousFactNotFound`].
pub async fn wiki_supersede(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    old_fact_id: &FactId,
    req: CaptureRequest,
) -> Result<CaptureOutcome> {
    if fact_index::find_by_id(pool, old_fact_id).await?.is_none() {
        return Err(CaptureError::PreviousFactNotFound(old_fact_id.clone()));
    }
    // Force dedup off — supersede is intentional, never accidental.
    let req_no_dedup = CaptureRequest {
        dedup_threshold: Some(1.01),
        ..req
    };
    let outcome = wiki_capture(tree, pool, embedder.clone(), req_no_dedup).await?;
    match outcome.action {
        CaptureAction::Captured {
            source_path,
            region_start,
            region_end,
        } => {
            fact_index::mark_superseded(pool, old_fact_id, &outcome.fact_id).await?;
            // Disk half of the supersede: strip the old region from its page
            // so the raw text recall-by-navigation reads no longer carries
            // the retired (and often contradictory) fact. Best-effort — the
            // DB tombstone already excludes it from recall and
            // `page_acl_map_active` redacts any residue, so a strip failure
            // must not fail the supersede. This is the one path that also
            // cleans `rules.md`, which the narrative compiler never rewrites.
            if let Err(e) =
                crate::reindex::strip_fact_region(pool, tree, embedder.clone(), old_fact_id).await
            {
                tracing::warn!(
                    previous_fact_id = old_fact_id.as_str(),
                    error = %e,
                    "capture: supersede page-strip failed (redaction still applies)"
                );
            }
            tracing::info!(
                previous_fact_id = old_fact_id.as_str(),
                new_fact_id = outcome.fact_id.as_str(),
                source_path,
                "capture: SUPERSEDED"
            );
            Ok(CaptureOutcome {
                fact_id: outcome.fact_id,
                action: CaptureAction::Superseded {
                    source_path,
                    region_start,
                    region_end,
                    previous_fact_id: old_fact_id.clone(),
                },
            })
        },
        other => Ok(CaptureOutcome {
            fact_id: outcome.fact_id,
            action: other,
        }),
    }
}

// ---------- wiki_forget ----------

/// `_internal.wiki_forget` — tombstone a fact row, then excise its
/// on-disk region.
///
/// The DB tombstone (`deleted_at`) is the authoritative half and lands
/// first; the disk half then strips the region's bytes from the page via
/// [`crate::reindex::strip_fact_region`] — the same pattern as
/// [`wiki_supersede`], and the one cleanup that also reaches `rules.md`,
/// which the narrative compiler never rewrites. The strip is
/// **best-effort**: a failure is logged and never fails the forget (the
/// tombstone already excludes the fact from recall, and the active ACL
/// map redacts any residue fail-closed; leftover bytes are picked up by
/// the light-dream hygiene sweep).
///
/// `reason` is free-form but the spec recommends one of
/// `user_request`, `filesystem_removed`, `gdpr_erasure`.
///
/// # Errors
///
/// As [`sqlx::Error`].
pub async fn wiki_forget(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    fact_id: &FactId,
    reason: &str,
) -> Result<ForgetOutcome> {
    let touched = fact_index::mark_forgotten(pool, fact_id, reason).await?;
    let tombstoned = touched > 0;
    if tombstoned {
        // Disk half of the forget. Only strips a retired row, so a racing
        // restore can never lose live prose.
        if let Err(e) = crate::reindex::strip_fact_region(pool, tree, embedder, fact_id).await {
            tracing::warn!(
                fact_id = fact_id.as_str(),
                error = %e,
                "capture: forget page-strip failed (redaction still applies)"
            );
        }
        tracing::info!(fact_id = fact_id.as_str(), reason, "capture: FORGOTTEN");
    } else {
        tracing::debug!(
            fact_id = fact_id.as_str(),
            reason,
            "capture: forget no-op (unknown id or already tombstoned)"
        );
    }
    Ok(ForgetOutcome { tombstoned })
}

// ---------- wiki_link ----------

/// `_internal.wiki_link` — append a cross-wiki link to a page.
///
/// The link is rendered as a plain Obsidian-compatible wikilink:
/// `[[target_wiki/target_page]]` (or `[[target_wiki]]` when
/// `target_page` is `None`). Returns the byte offsets of the new link
/// so a caller that wants to drop a fact-id marker around it can do
/// so in a follow-up call to [`wiki_capture`].
///
/// `wiki_link` is *not* a fact: it does not produce a `fact_index`
/// row. The watcher / re-index treats the link as ordinary markdown.
///
/// # Errors
///
/// See [`CaptureError`].
pub fn wiki_link(
    tree: &WikiTree,
    in_wiki: &WikiId,
    in_page: &Path,
    target_wiki: &WikiId,
    target_page: Option<&Path>,
) -> Result<LinkOutcome> {
    if !is_safe_page_path(in_page) {
        return Err(CaptureError::UnsafePagePath {
            path: in_page.to_path_buf(),
        });
    }
    if let Some(p) = target_page
        && !is_safe_page_path(p)
    {
        return Err(CaptureError::UnsafePagePath {
            path: p.to_path_buf(),
        });
    }

    let handle = tree.locate(in_wiki)?;
    let abs_page = handle.abs_dir().join(in_page);
    // Same create-time guard as `wiki_capture`: a link append may forge
    // the source page, and a case-colliding name must never reach disk.
    if !crate::wiki::page_exists_byte_exact(handle.abs_dir(), in_page)
        && let Some(reason) = crate::wiki::page_path_case_hazard(in_page)
            .or_else(|| crate::wiki::page_case_conflict(handle.abs_dir(), in_page))
    {
        return Err(CaptureError::PageCaseConflict {
            path: in_page.to_path_buf(),
            reason,
        });
    }
    let link_target = target_page.map_or_else(
        || format!("[[{target_wiki}]]"),
        |p| {
            let p_str = p.to_string_lossy().replace('\\', "/");
            // Strip the trailing .md so the wikilink reads naturally
            // in Obsidian.
            let trimmed = p_str.strip_suffix(".md").unwrap_or(&p_str);
            format!("[[{target_wiki}/{trimmed}]]")
        },
    );
    let (new_contents, start, end) = append_text(&abs_page, &link_target)?;
    atomic_write(&abs_page, new_contents.as_bytes())?;

    let source_path = crate::wiki::workdir_relative_source_path(tree.workdir(), &abs_page);
    tracing::info!(
        in_wiki = in_wiki.as_str(),
        source_path,
        target = link_target,
        "capture: LINKED"
    );
    Ok(LinkOutcome {
        source_path,
        link_start: i64::try_from(start).unwrap_or(i64::MAX),
        link_end: i64::try_from(end).unwrap_or(i64::MAX),
    })
}

// ---------- Helpers ----------

fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Err(CaptureError::EmptyBody);
    }
    // The only marker syntax a body may carry is well-formed self-closing
    // `{{embed=…}}` markers (rendered by code, never by the model — see
    // media pipeline).
    // Region markers, stray braces and malformed fragments stay rejected.
    if (body.contains("{{") || body.contains("}}"))
        && crate::parser::embed_only_markers(body).is_none()
    {
        return Err(CaptureError::BodyContainsMarker);
    }
    Ok(())
}

/// Enforce the cross-user attribution invariants on a fresh
/// [`CaptureRequest`]
/// (see capture & dedup):
///
/// 1. When `req.sender` is absent, materialize it to `req.owner`. The
///    capturer is the owner (the "user talks about themself" case);
///    `sender` and `owner` are always kept as two separate, materialized
///    fields so a later owner change never silently rebinds the original
///    provenance. `sender_id = NULL` survives only as the degenerate
///    "scrubbed" state (e.g. a deleted user) that falls back to owner at
///    read time — see
///    marker-grammar §5.
/// 2. When `req.sender` is also listed in `req.allow`, refuse. The
///    [`can_read`](crate::acl::can_read) algorithm already grants read
///    to `sender_of_region`; duplicating it under `allow=` is
///    redundant and usually points to caller confusion (the dashboard,
///    chat router, or a downstream emitter has copy-pasted the same
///    principal into both fields).
///
/// Note: at this layer we cannot validate "the implicit sender of the
/// MCP call ≠ owner" — the JWT identity is consumed at the MCP
/// dispatcher level, and capture only sees the materialised
/// `CaptureRequest`. The dispatcher is responsible for translating its
/// own bearer-token identity into `req.sender` when capturing on
/// someone else's wiki; the validation here closes the loop once that
/// translation has happened.
fn normalize_sender_attribution(req: &mut CaptureRequest) -> Result<()> {
    let Some(sender) = req.sender.clone() else {
        // Absent attribution → materialize sender = owner. Provenance is
        // frozen at birth as a distinct field, never collapsed to NULL.
        req.sender = Some(req.owner.clone());
        return Ok(());
    };
    if sender == req.owner {
        // Already explicit and consistent — keep it materialized.
        return Ok(());
    }
    if req.allow.iter().any(|p| p == &sender) {
        return Err(CaptureError::SenderRedundantInAllow(sender));
    }
    Ok(())
}

pub(crate) fn new_fact_id() -> Result<FactId> {
    let raw = Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
    // `Uuid::new_v7` always produces the canonical RFC 4122 layout we
    // require, but go through `FactId::parse` so a future refactor of
    // the type's invariants still fails closed.
    Ok(FactId::parse(&raw.to_string())?)
}

/// The **runtime** marker: bare region key only — `{{f=<uuid>}}body{{/}}`.
///
/// The ACL lives in the `fact_index` columns (the DB is the
/// authoritative source — see
/// redaction policy);
/// no write path puts ACL attributes into a marker. Full markers are an
/// accepted *input* (legacy pages, imported archives) and the *export*
/// format ([`render_full_marker`]).
pub(crate) fn render_marker(fact_id: &FactId, body: &str) -> String {
    format!("{{{{f={fact_id}}}}}{body}{{{{/}}}}")
}

/// The self-closing media embed marker `{{embed=<catalog_id>}}`.
///
/// Like the region markers above, embed markers are **rendered by code,
/// never by the model**: ingest receives the claimed catalog ids as a
/// structured field and appends the markers itself (see
/// media pipeline).
#[must_use]
pub fn render_embed_marker(catalog_id: &CatalogId) -> String {
    format!("{{{{embed={catalog_id}}}}}")
}

/// The **export/interchange** marker: the full self-describing form
/// `{{owner=… allow=… sender=… f=…}}body{{/}}`.
///
/// Never written at runtime — used when producing a portable archive
/// where each fragment must carry its own ACL without the engine DB
/// next to it. The parser accepts this form as input, so an exported
/// page re-imports losslessly.
#[must_use]
pub fn render_full_marker(
    fact_id: &FactId,
    owner: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
    body: &str,
) -> String {
    let mut attrs = Vec::with_capacity(4);
    attrs.push(format!("owner={owner}"));
    if !allow.is_empty() {
        let joined: Vec<String> = allow.iter().map(ToString::to_string).collect();
        attrs.push(format!("allow={}", joined.join(",")));
    }
    if let Some(s) = sender {
        attrs.push(format!("sender={s}"));
    }
    attrs.push(format!("f={fact_id}"));
    format!("{{{{{}}}}}{}{{{{/}}}}", attrs.join(" "), body)
}

/// Append `region` at the end of the page body, returning the new
/// contents + the byte offsets of the appended slice.
///
/// Reads the page from disk if it exists; treats a missing page as an
/// empty body. The page's frontmatter (if any) is preserved verbatim.
fn append_region(abs_page: &Path, region: &str) -> Result<(String, usize, usize)> {
    let raw = read_page_or_empty(abs_page)?;
    let needs_newline = !raw.is_empty() && !raw.ends_with('\n');
    let mut out = raw;
    if needs_newline {
        out.push('\n');
    }
    let start = out.len();
    out.push_str(region);
    let end = out.len();
    // Always finish with a trailing newline so subsequent appends land
    // on their own line.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, start, end))
}

/// Append `text` to the page (no marker wrapping). Used by
/// [`wiki_link`]. Returns the offsets of the appended slice.
fn append_text(abs_page: &Path, text: &str) -> Result<(String, usize, usize)> {
    let raw = read_page_or_empty(abs_page)?;
    let needs_newline = !raw.is_empty() && !raw.ends_with('\n');
    let mut out = raw;
    if needs_newline {
        out.push('\n');
    }
    let start = out.len();
    out.push_str(text);
    let end = out.len();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, start, end))
}

fn read_page_or_empty(abs_page: &Path) -> Result<String> {
    match std::fs::read_to_string(abs_page) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(CaptureError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    fn seed_alice(tree: &WikiTree) {
        let dir = tree.wikis_dir().join("alice");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = "---\n\
                    wiki_id: alice\n\
                    wiki_type: wiki-user\n\
                    parent_wiki_id: null\n\
                    slug: alice\n\
                    title: Alice\n\
                    acl_default: 'user:alice'\n\
                    ---\n";
        std::fs::write(dir.join("_meta.md"), meta).unwrap();
    }

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("fake-bge-m3", 4))
    }

    fn sample_request(body: &str) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: PathBuf::from("intro.md"),
            body: body.to_owned(),
            owner: "user:alice".parse().unwrap(),
            allow: vec![],
            sender: None,
            fact_type: Some("preference".to_owned()),
            topics: vec![],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        }
    }

    // ---------- validate_body ----------

    #[test]
    fn validate_body_rejects_empty_and_marker_strings() {
        assert!(matches!(validate_body(""), Err(CaptureError::EmptyBody)));
        assert!(matches!(validate_body("   "), Err(CaptureError::EmptyBody)));
        assert!(matches!(
            validate_body("hello {{owner=user:a}}foo{{/}}"),
            Err(CaptureError::BodyContainsMarker)
        ));
        assert!(matches!(
            validate_body("trailing }} bracket"),
            Err(CaptureError::BodyContainsMarker)
        ));
        assert!(validate_body("plain prose").is_ok());
    }

    #[test]
    fn validate_body_admits_well_formed_embeds_only() {
        assert!(validate_body("sunset {{embed=c-2026-06-12-photo-001.jpg}}").is_ok());
        assert!(matches!(
            validate_body("bad {{embed=garbage}}"),
            Err(CaptureError::BodyContainsMarker)
        ));
        assert!(matches!(
            validate_body("mixed {{embed=c-2026-06-12-photo-001.jpg}} and {{f=x}}y{{/}}"),
            Err(CaptureError::BodyContainsMarker)
        ));
    }

    // ---------- render_embed_marker ----------

    #[test]
    fn render_embed_marker_round_trips_through_the_parser() {
        let cid = CatalogId::parse("c-2026-06-12-photo-001.jpg").unwrap();
        let marker = render_embed_marker(&cid);
        assert_eq!(marker, "{{embed=c-2026-06-12-photo-001.jpg}}");
        let embeds = crate::parser::embed_only_markers(&marker).expect("valid");
        assert_eq!(embeds, vec![cid]);
    }

    // ---------- normalize_sender_attribution ----------

    fn req_with(owner: &str, sender: Option<&str>, allow: Vec<&str>) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: PathBuf::from("index.md"),
            body: "x".into(),
            owner: owner.parse().unwrap(),
            allow: allow.into_iter().map(|s| s.parse().unwrap()).collect(),
            sender: sender.map(|s| s.parse().unwrap()),
            fact_type: None,
            topics: vec![],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        }
    }

    #[test]
    fn normalize_sender_keeps_materialized_when_equal_to_owner() {
        let mut req = req_with("user:alice", Some("user:alice"), vec![]);
        normalize_sender_attribution(&mut req).unwrap();
        assert_eq!(
            req.sender.as_ref().map(ToString::to_string).as_deref(),
            Some("user:alice"),
            "sender must stay materialized (= owner), never collapsed to None"
        );
    }

    #[test]
    fn normalize_sender_keeps_when_different_from_owner() {
        let mut req = req_with("user:bob", Some("user:alice"), vec![]);
        normalize_sender_attribution(&mut req).unwrap();
        assert_eq!(
            req.sender.as_ref().map(ToString::to_string).as_deref(),
            Some("user:alice")
        );
    }

    #[test]
    fn normalize_sender_rejects_when_redundant_in_allow() {
        let mut req = req_with("user:bob", Some("user:alice"), vec!["user:alice"]);
        let err = normalize_sender_attribution(&mut req).expect_err("must reject");
        assert!(matches!(err, CaptureError::SenderRedundantInAllow(_)));
    }

    #[test]
    fn normalize_sender_keeps_when_allow_unrelated() {
        let mut req = req_with("user:bob", Some("user:alice"), vec!["group:family"]);
        normalize_sender_attribution(&mut req).unwrap();
        assert!(req.sender.is_some());
    }

    #[test]
    fn normalize_sender_materializes_to_owner_when_none() {
        let mut req = req_with("user:bob", None, vec!["user:alice"]);
        normalize_sender_attribution(&mut req).unwrap();
        assert_eq!(
            req.sender.as_ref().map(ToString::to_string).as_deref(),
            Some("user:bob"),
            "absent sender must be materialized to the owner"
        );
        assert_eq!(req.allow.len(), 1);
    }

    // ---------- render_marker ----------

    #[test]
    fn render_marker_emits_the_bare_runtime_form() {
        let fid = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let rendered = render_marker(&fid, "I love pasta");
        assert_eq!(
            rendered,
            "{{f=018f1234-5678-7abc-9def-0123456789ab}}I love pasta{{/}}"
        );
    }

    #[test]
    fn render_full_marker_emits_canonical_export_form() {
        let fid = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let owner: Principal = "user:alice".parse().unwrap();
        let allow: Vec<Principal> =
            vec!["group:family".parse().unwrap(), "user:bob".parse().unwrap()];
        let sender: Principal = "user:bob".parse().unwrap();
        let rendered = render_full_marker(&fid, &owner, &allow, Some(&sender), "I love pasta");
        let expected = "{{owner=user:alice allow=group:family,user:bob sender=user:bob \
                        f=018f1234-5678-7abc-9def-0123456789ab}}I love pasta{{/}}";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_full_marker_omits_empty_allow_and_absent_sender() {
        let fid = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let owner: Principal = "user:alice".parse().unwrap();
        let rendered = render_full_marker(&fid, &owner, &[], None, "body");
        let expected = "{{owner=user:alice f=018f1234-5678-7abc-9def-0123456789ab}}body{{/}}";
        assert_eq!(rendered, expected);
    }

    // ---------- wiki_capture happy path ----------

    #[tokio::test]
    async fn capture_creates_page_with_marker_and_index_row() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        let req = sample_request("I love pasta");
        let outcome = wiki_capture(&tree, &pool, embedder(), req)
            .await
            .expect("capture");
        let (region_start, region_end) = match outcome.action {
            CaptureAction::Captured {
                region_start,
                region_end,
                ..
            } => (region_start, region_end),
            other => panic!("expected Captured, got {other:?}"),
        };
        assert!(region_end > region_start);

        // Page exists and contains the bare runtime marker — no ACL
        // attributes on disk, the ACL lives in the fact_index row.
        let intro = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(intro.contains("I love pasta"));
        assert!(intro.contains("{{f="));
        assert!(!intro.contains("owner="));
        assert!(intro.contains("{{/}}"));

        // fact_index row written.
        let row = fact_index::find_by_id(&pool, &outcome.fact_id)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.wiki_id, "alice");
        assert_eq!(row.text, "I love pasta");
        assert_eq!(row.embedding.len(), 4);
        assert_eq!(row.source_path, "wikis/alice/intro.md");
        // Rendered offsets stamped after the page write (a row left at
        // NULL would read as a pending render to the reindex sweep).
        assert_eq!(row.region_start, Some(region_start));
        assert_eq!(row.region_end, Some(region_end));
    }

    #[tokio::test]
    async fn capture_refuses_case_colliding_new_page() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        // Seed `intro.md`, then try to create its case twin: one file on
        // a case-insensitive mirror, so the capture must refuse — and it
        // must refuse identically on a filesystem that folds case, where
        // the twin would land *inside* the existing page.
        wiki_capture(&tree, &pool, embedder(), sample_request("I love pasta"))
            .await
            .expect("seed capture");
        let mut twin = sample_request("Pizza is fine too");
        twin.page = PathBuf::from("Intro.md");
        let err = wiki_capture(&tree, &pool, embedder(), twin)
            .await
            .expect_err("case twin must refuse");
        assert!(
            matches!(err, CaptureError::PageCaseConflict { .. }),
            "{err:?}"
        );

        // A reserved-filename case variant is refused even on empty disk.
        let mut meta_variant = sample_request("sneaky");
        meta_variant.page = PathBuf::from("_Meta.md");
        let err = wiki_capture(&tree, &pool, embedder(), meta_variant)
            .await
            .expect_err("reserved case variant must refuse");
        assert!(
            matches!(err, CaptureError::PageCaseConflict { .. }),
            "{err:?}"
        );

        // A fresh uppercase page with no twin lands byte-faithfully.
        let mut fresh = sample_request("Notes on the Big Rewrite");
        fresh.page = PathBuf::from("Rewrite-Notes.md");
        wiki_capture(&tree, &pool, embedder(), fresh)
            .await
            .expect("fresh uppercase page");
        assert!(dir.path().join("wikis/alice/Rewrite-Notes.md").is_file());
    }

    /// DB-row-first write order: when the page write fails after the
    /// row committed, the capture errors AND compensates by tombstoning
    /// the row — the consumer's error response and the store agree.
    #[cfg(unix)]
    #[tokio::test]
    async fn capture_compensates_row_when_page_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        // Make the wiki dir read-only so atomic_write cannot create its
        // temp file (the page itself does not exist yet — append_region
        // treats that as an empty body and succeeds).
        let wiki_dir = dir.path().join("wikis/alice");
        std::fs::set_permissions(&wiki_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let req = sample_request("doomed body");
        let err = wiki_capture(&tree, &pool, embedder(), req)
            .await
            .expect_err("page write must fail");
        assert!(
            matches!(err, CaptureError::Io(_) | CaptureError::Wiki(_)),
            "got {err:?}"
        );

        // Restore permissions so tempdir cleanup works.
        std::fs::set_permissions(&wiki_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The committed row was compensated, with the dedicated reason.
        let rows = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert!(rows.is_empty(), "no live row may survive a failed capture");
        let all: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT fact_id, deleted_reason FROM fact_index")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.as_deref(), Some(REASON_FILE_WRITE_FAILED));
    }

    /// The per-fact validity interval the classifier deduced
    /// (carried on `CaptureRequest`) must reach the `fact_index` row, and
    /// `decay_reason` must stay NULL at insert (a fresh fact is alive). An unset
    /// validity persists as the open horizon (`None`/`None`).
    #[tokio::test]
    async fn capture_threads_validity_into_fact_index() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        // A dated commitment: finite validity threaded through to storage.
        let mut dated = sample_request("dentist appointment Thursday 17:00");
        dated.valid_from = Some("2026-06-06T00:00:00Z".to_owned());
        dated.valid_to = Some("2026-06-11T17:00:00Z".to_owned());
        let o1 = wiki_capture(&tree, &pool, embedder(), dated)
            .await
            .expect("capture");
        let r1 = fact_index::find_by_id(&pool, &o1.fact_id)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(r1.valid_from.as_deref(), Some("2026-06-06T00:00:00Z"));
        assert_eq!(r1.valid_to.as_deref(), Some("2026-06-11T17:00:00Z"));
        assert!(r1.decay_reason.is_none(), "a fresh fact must be alive");

        // A durable fact: unset validity persists as the open horizon.
        let open = sample_request("Inception is a cult film");
        let o2 = wiki_capture(&tree, &pool, embedder(), open)
            .await
            .expect("capture");
        let r2 = fact_index::find_by_id(&pool, &o2.fact_id)
            .await
            .unwrap()
            .expect("row");
        assert!(r2.valid_from.is_none());
        assert!(r2.valid_to.is_none(), "open horizon when unset");
        assert!(r2.decay_reason.is_none());
    }

    #[tokio::test]
    async fn two_distinct_captures_both_land_and_are_appended() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        let r1 = sample_request("manca il latte");
        let r2 = sample_request("compra il pane");

        let o1 = wiki_capture(&tree, &pool, embedder(), r1).await.unwrap();
        let o2 = wiki_capture(&tree, &pool, embedder(), r2).await.unwrap();

        assert!(matches!(o1.action, CaptureAction::Captured { .. }));
        assert!(matches!(o2.action, CaptureAction::Captured { .. }));

        let intro = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(intro.contains("manca il latte"));
        assert!(intro.contains("compra il pane"));

        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            2
        );
    }

    // ---------- dedup ----------

    #[tokio::test]
    async fn capture_dedups_near_paraphrase() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        let r1 = sample_request("manca il latte");
        let first = wiki_capture(&tree, &pool, embedder(), r1).await.unwrap();

        let r2 = sample_request("Manca il latte.");
        let second = wiki_capture(&tree, &pool, embedder(), r2).await.unwrap();
        match second.action {
            CaptureAction::Skipped {
                matched_fact_id,
                similarity,
            } => {
                assert_eq!(matched_fact_id, first.fact_id);
                assert!(similarity >= DEFAULT_DEDUP_THRESHOLD);
            },
            other => panic!("expected Skipped, got {other:?}"),
        }

        // Filesystem unchanged (one region only).
        let intro = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert_eq!(intro.matches("{{f=").count(), 1);
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn capture_dedup_threshold_can_be_overridden_per_call() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        wiki_capture(&tree, &pool, embedder(), sample_request("hello world"))
            .await
            .unwrap();

        // Threshold 0.0 → every capture dedups, even unrelated ones.
        let mut harsh = sample_request("totally different text now");
        harsh.dedup_threshold = Some(0.0);
        let outcome = wiki_capture(&tree, &pool, embedder(), harsh).await.unwrap();
        assert!(matches!(outcome.action, CaptureAction::Skipped { .. }));

        // Threshold 1.01 (effectively off) → captures even on a perfect
        // duplicate.
        let mut permissive = sample_request("hello world");
        permissive.dedup_threshold = Some(1.01);
        let outcome = wiki_capture(&tree, &pool, embedder(), permissive)
            .await
            .unwrap();
        assert!(matches!(outcome.action, CaptureAction::Captured { .. }));
    }

    /// Dedup never crosses the rules-page boundary (both-or-neither): a new
    /// behaviour rule on `rules.md` is NOT skipped as a duplicate of an
    /// ordinary same-owner fact that restates it — a skip would keep the rule
    /// off `rules.md` and out of the behaviour-rules channel — while
    /// rule-vs-rule on the page still dedups.
    #[tokio::test]
    async fn capture_dedup_never_crosses_the_rules_page_boundary() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        // An ordinary fact restating the directive, on a content page.
        wiki_capture(
            &tree,
            &pool,
            embedder(),
            sample_request("rispondi sempre in modo conciso"),
        )
        .await
        .unwrap();

        // The same words as a behaviour rule on `rules.md`: with a
        // threshold-0.0 probe (everything same-page would match) the
        // cross-page pair must still NOT pair — the rule captures.
        let mut rule = sample_request("Rispondi sempre in modo conciso.");
        rule.page = PathBuf::from(crate::wiki::RULES_FILENAME);
        rule.dedup_threshold = Some(0.0);
        let first_rule = wiki_capture(&tree, &pool, embedder(), rule).await.unwrap();
        assert!(
            matches!(first_rule.action, CaptureAction::Captured { .. }),
            "a rule never dedups against a non-rules fact, got {:?}",
            first_rule.action
        );

        // Rule-vs-rule on the page still dedups (the user repeating a
        // standing directive folds into the existing rule).
        let mut repeat = sample_request("Rispondi sempre in modo conciso.");
        repeat.page = PathBuf::from(crate::wiki::RULES_FILENAME);
        let outcome = wiki_capture(&tree, &pool, embedder(), repeat)
            .await
            .unwrap();
        match outcome.action {
            CaptureAction::Skipped {
                matched_fact_id, ..
            } => assert_eq!(matched_fact_id, first_rule.fact_id),
            other => panic!("expected rule-vs-rule Skipped, got {other:?}"),
        }
    }

    // ---------- supersede ----------

    #[tokio::test]
    async fn supersede_appends_new_region_and_retires_old_one() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;

        let first = wiki_capture(&tree, &pool, embedder(), sample_request("I weigh 72 kg"))
            .await
            .unwrap();
        let new_req = sample_request("I weigh 70 kg");
        let outcome = wiki_supersede(&tree, &pool, embedder(), &first.fact_id, new_req)
            .await
            .unwrap();
        match outcome.action {
            CaptureAction::Superseded {
                previous_fact_id, ..
            } => assert_eq!(previous_fact_id, first.fact_id),
            other => panic!("expected Superseded, got {other:?}"),
        }

        // Old row tombstoned, new row active.
        let old = fact_index::find_by_id(&pool, &first.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert!(old.superseded_at.is_some());
        assert_eq!(old.superseded_by, Some(outcome.fact_id.clone()));
        let active = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].fact_id, outcome.fact_id);
    }

    #[tokio::test]
    async fn supersede_errors_when_previous_fact_unknown() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;
        let phantom = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let err = wiki_supersede(&tree, &pool, embedder(), &phantom, sample_request("x"))
            .await
            .expect_err("must error");
        assert!(matches!(err, CaptureError::PreviousFactNotFound(_)));
    }

    // ---------- forget ----------

    #[tokio::test]
    async fn forget_tombstones_fact_and_strips_its_region_from_disk() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;
        let cap = wiki_capture(&tree, &pool, embedder(), sample_request("temporary"))
            .await
            .unwrap();
        let intro_before =
            std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(intro_before.contains(cap.fact_id.as_str()));

        let outcome = wiki_forget(&tree, &pool, embedder(), &cap.fact_id, "user_request")
            .await
            .unwrap();
        assert!(outcome.tombstoned);

        // Disk half: the retired region's bytes left the page.
        let intro_after = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(
            !intro_after.contains(cap.fact_id.as_str()),
            "forgotten fact's marker must be excised: {intro_after}"
        );
        assert!(!intro_after.contains("temporary"));

        // Tombstone applied; the settled row carries no offsets.
        let row = fact_index::find_by_id(&pool, &cap.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert!(row.deleted_at.is_some());
        assert_eq!(row.deleted_reason, Some("user_request".to_owned()));
        assert!(row.region_start.is_none() && row.region_end.is_none());
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn forget_survives_a_missing_page_file() {
        // The strip is best-effort: a page already gone from disk must not
        // fail the tombstone (the DB half is what forgetting means).
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let pool = make_pool().await;
        let cap = wiki_capture(&tree, &pool, embedder(), sample_request("temporary"))
            .await
            .unwrap();
        std::fs::remove_file(dir.path().join("wikis/alice/intro.md")).unwrap();

        let outcome = wiki_forget(&tree, &pool, embedder(), &cap.fact_id, "user_request")
            .await
            .unwrap();
        assert!(outcome.tombstoned, "forget must succeed without the page");
    }

    #[tokio::test]
    async fn forget_unknown_id_returns_not_tombstoned() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let phantom = FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap();
        let out = wiki_forget(&tree, &pool, embedder(), &phantom, "user_request")
            .await
            .unwrap();
        assert!(!out.tombstoned);
    }

    // ---------- link ----------

    #[test]
    fn link_appends_wikilink_to_page() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);

        let out = wiki_link(
            &tree,
            &WikiId::parse("alice").unwrap(),
            Path::new("intro.md"),
            &WikiId::parse("alice-acmecorp").unwrap(),
            Some(Path::new("widget-pro.md")),
        )
        .unwrap();
        assert!(out.link_end > out.link_start);

        let intro = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(intro.contains("[[alice-acmecorp/widget-pro]]"));
    }

    #[test]
    fn link_without_page_renders_wiki_only_target() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);

        wiki_link(
            &tree,
            &WikiId::parse("alice").unwrap(),
            Path::new("intro.md"),
            &WikiId::parse("alice-acmecorp").unwrap(),
            None,
        )
        .unwrap();

        let intro = std::fs::read_to_string(dir.path().join("wikis/alice/intro.md")).unwrap();
        assert!(intro.contains("[[alice-acmecorp]]"));
        assert!(!intro.contains("alice-acmecorp/"));
    }

    #[test]
    fn link_rejects_unsafe_paths() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        seed_alice(&tree);
        let bad = Path::new("../escape.md");
        let err = wiki_link(
            &tree,
            &WikiId::parse("alice").unwrap(),
            bad,
            &WikiId::parse("alice-acmecorp").unwrap(),
            None,
        )
        .expect_err("must reject");
        assert!(matches!(err, CaptureError::UnsafePagePath { .. }));
    }
}
