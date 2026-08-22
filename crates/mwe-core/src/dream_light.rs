// SPDX-License-Identifier: AGPL-3.0-or-later
//! The **light dream** — turn waiting claims into memories.
//!
//! The light dream is the frequent, cheap half of the "two dream" cadence (the
//! other being the nightly REM full reorg, [`crate::rem`]). It drains the
//! [captures buffer](crate::capture_buffer), and it does so in **two halves
//! with the compilation plan between them** — because a claim becomes a fact
//! only once somebody has decided which page it goes on (founder, 2026-08-18:
//! *«io credo sia giusto scrivere l'entry nel db dei fatti quando si è già
//! deciso dove mettere il fatto in attesa, parallelamente alla scrittura sulla
//! prosa»*).
//!
//! ```text
//! screen_queue → build_wiki_plan → materialise → compile_dirty_pages
//!  (dedup)        (which page?)     (the rows)      (the prose)
//! ```
//!
//! **[`screen_queue`] — what needs no destination.** One question only: is this
//! claim already remembered? The same jaccard 6-gram scan a live
//! [`crate::capture::wiki_capture`] runs (same-subject scope across the whole
//! forest, channel-page boundary, embed-set guard, the same `dedup_threshold`),
//! plus the same comparison against the other claims in this queue — none of
//! which is a row yet, so the DB scan cannot see them. A duplicate resolves to
//! its survivor (`skipped_dup`) and never reaches the plan. Parity is the point:
//! a claim that waited must get exactly the dedup it would have gotten written
//! live. Sub-threshold paraphrases remain the REM night's semantic dedup job.
//! No LLM — the scan is pure CPU.
//!
//! What survives is projected as a [`crate::planner::FactForPage`] with **no
//! page**, and judged by the placement stage beside the facts already on pages.
//!
//! **[`materialise`] — the moment a claim becomes a memory.** For each claim
//! the plan placed: embed (normally already staged at buffer time), insert into
//! `fact_index` addressed to `wikis/<wiki>/<page>`, stamp a closure reason
//! staged while it waited, apply the classifier's `supersede_hint`, and stamp
//! the buffer row `promoted`. A claim the plan could not place keeps waiting —
//! it is not a fact nobody renders.
//!
//! The offsets stay `NULL` until the compile writes the page moments later and
//! `repoint_facts` stamps them: the same *pending render* state a live capture
//! passes through between its insert and its page write. Recall serves such a
//! fact straight from `fact_index.text`.
//!
//! **[`drain_deterministically`]** does all three with no model at all, for a
//! deployment with no prose writer configured and for tests.
//!
//! ## Idempotency & crash-safety
//!
//! The light dream only ever advances `buffered` rows, the insert is
//! `insert_if_absent`, `mark_superseded` no-ops on an already-superseded row,
//! and the status updates are guarded on `status = 'buffered'`. So a crash
//! mid-cycle simply re-runs idempotently — the stable `capture_id == fact_id`
//! is what makes that safe.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture_buffer::{self, BufferedCapture, CaptureBufferError};
use crate::embedder::{Embedder, EmbedderError};
use crate::fact_index::{self, FactIndexError, NewFact};
use crate::types::Principal;
use crate::wiki::{WikiError, WikiTree};

/// Errors raised by the light dream. Infrastructure failures (DB, embedder)
/// bubble; per-capture soft failures are collected into the report instead.
#[derive(Debug, Error)]
pub enum LightError {
    /// Captures buffer access failed.
    #[error("light dream capture_buffer: {0}")]
    Buffer(#[from] CaptureBufferError),
    /// `fact_index` access failed.
    #[error("light dream fact_index: {0}")]
    FactIndex(#[from] FactIndexError),
    /// Embedding a claim body failed. Raised inside [`write_placed`], where it
    /// is caught per-claim (the claim keeps waiting) rather than aborting the
    /// whole cycle.
    #[error("light dream embedder: {0}")]
    Embedder(#[from] EmbedderError),
    /// Walking the wiki tree failed.
    #[error("light dream wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Building the compilation plan failed — only
    /// [`drain_deterministically`], which builds one itself.
    #[error("light dream planner: {0}")]
    Planner(#[from] crate::planner::PlannerError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, LightError>;

/// Operator-tunable knobs for one light cycle.
#[derive(Debug, Clone)]
pub struct LightPolicy {
    /// Maximum captures promoted in one cycle (cost guard on the embedder).
    /// Excess captures stay `buffered` and are picked up next cycle.
    pub max_promotions_per_cycle: i64,
    /// Jaccard 6-gram similarity at or above which a buffered capture
    /// folds into an existing active fact instead of promoting — the
    /// same knob (and default) as the direct path's
    /// `CaptureRequest::dedup_threshold`.
    pub dedup_threshold: f32,
}

impl Default for LightPolicy {
    fn default() -> Self {
        Self {
            max_promotions_per_cycle: 500,
            dedup_threshold: crate::recall::DEFAULT_DEDUP_THRESHOLD,
        }
    }
}

/// Outcome of one [`run_light_cycle`].
#[derive(Debug, Default, Clone)]
pub struct LightCycleReport {
    /// Buffered captures examined this cycle.
    pub scanned: usize,
    /// Captures promoted into a fresh `fact_index` row.
    pub promoted: usize,
    /// Captures skipped as exact duplicates of an existing fact.
    pub skipped_dup: usize,
    /// Supersede hints applied (a prior fact marked superseded).
    pub superseded: usize,
    /// Captures the placement pass read and gave no page: they keep their
    /// buffer row and are offered again next pass. Not an error — since
    /// 2026-08-22 there is no page meaning "unsorted", so waiting IS the
    /// outcome for a claim nothing fits yet.
    pub left_waiting: usize,
    /// Per-capture soft errors (`"<capture_id>: <error>"`); the cycle continues.
    pub errors: Vec<String>,
}

/// The queue as the compile is about to see it.
///
/// Produced by [`screen_queue`], consumed by [`materialise`]. `for_plan` is
/// what the placement stage judges; `rows` are the buffer rows behind them,
/// kept so materialisation does not re-read the table between the two.
pub struct WaitingQueue {
    /// The surviving claims, projected for [`crate::planner::build_wiki_plan`].
    pub for_plan: Vec<crate::planner::FactForPage>,
    rows: Vec<BufferedCapture>,
    /// What the screening did. Filled in further by [`materialise`].
    pub report: LightCycleReport,
}

/// Screen the queue: drop the duplicates, project the rest for the plan.
///
/// **Nothing becomes a fact here.** A claim waiting in the buffer is waiting to
/// be *sorted*, and this pass only answers the question that needs no
/// destination — *is this already remembered?* — so the placement stage right
/// after judges a queue with no duplicates in it. The claims that survive
/// become `fact_index` rows in [`materialise`], once the plan says which page
/// each one goes on (founder, 2026-08-18: *«io credo sia giusto scrivere
/// l'entry nel db dei fatti quando si è già deciso dove mettere il fatto in
/// attesa, parallelamente alla scrittura sulla pagina»*).
///
/// A claim whose subject has no wiki on disk is left in the queue with a soft
/// error rather than projected: with nowhere for a page of its own to be born,
/// the placement has no answer to give.
///
/// # Errors
///
/// Surfaces infrastructure failures (DB / tree walk). Per-claim failures are
/// collected into `report.errors` and the claim is left buffered.
pub async fn screen_queue(
    pool: &SqlitePool,
    tree: &WikiTree,
    policy: &LightPolicy,
) -> Result<WaitingQueue> {
    let mut report = LightCycleReport::default();
    let captures = capture_buffer::find_all_buffered(pool, policy.max_promotions_per_cycle).await?;
    report.scanned = captures.len();
    if captures.is_empty() {
        return Ok(WaitingQueue {
            for_plan: Vec::new(),
            rows: Vec::new(),
            report,
        });
    }
    let on_disk = wikis_on_disk(tree)?;
    let mut for_plan = Vec::with_capacity(captures.len());
    let mut rows: Vec<BufferedCapture> = Vec::with_capacity(captures.len());
    for cap in captures {
        // Against the claims already accepted in THIS pass, first. None of them
        // is a `fact_index` row yet — they become rows only after the plan —
        // so the DB scan inside `screen_one` cannot see them, and two identical
        // claims arriving in one interval would both be written. Same
        // comparison the DB scan uses (jaccard 6-gram over the
        // marker-stripped body), same audience test, and the embed sets must
        // match for the same reason.
        if let Some(twin) = rows.iter().find(|prev| {
            same_audience(prev, &cap)
                && crate::recall::jaccard_6gram(
                    &crate::parser::strip_embed_markers(&prev.body),
                    &crate::parser::strip_embed_markers(&cap.body),
                ) >= policy.dedup_threshold
                && crate::parser::collect_embeds(&prev.body)
                    == crate::parser::collect_embeds(&cap.body)
        }) {
            tracing::info!(
                capture_id = %cap.capture_id,
                matched_capture_id = %twin.capture_id,
                "light dream: SKIPPED (duplicate of another claim in the same queue)"
            );
            capture_buffer::mark_skipped_dup(pool, &cap.capture_id, &twin.capture_id, &now())
                .await?;
            report.skipped_dup += 1;
            continue;
        }
        match screen_one(pool, &cap, &on_disk, policy, &mut report).await {
            Ok(Some(f)) => {
                for_plan.push(f);
                rows.push(cap);
            },
            Ok(None) => {},
            Err(e) => {
                tracing::warn!(capture_id = %cap.capture_id, error = %e, "light dream: claim failed screening, left buffered");
                report.errors.push(format!("{}: {e}", cap.capture_id));
            },
        }
    }
    tracing::info!(
        scanned = report.scanned,
        waiting = for_plan.len(),
        skipped_dup = report.skipped_dup,
        errors = report.errors.len(),
        "light dream: queue screened"
    );
    Ok(WaitingQueue {
        for_plan,
        rows,
        report,
    })
}

/// Write the screened claims into `fact_index`, each on the page the plan
/// gave it — the moment a claim becomes a memory.
///
/// Runs between the plan and the compile, so a row is born knowing its page
/// and the prose that renders it is written moments later from the same plan.
/// A claim the plan could not place (no home page at all) stays in the queue:
/// it waits, rather than becoming a fact nobody renders.
///
/// # Errors
///
/// Surfaces infrastructure failures. Per-claim failures are collected into
/// `queue.report.errors` and the claim is left buffered.
pub async fn materialise(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    queue: &mut WaitingQueue,
    plan: &crate::planner::CompilationPlan,
    now: &str,
) -> Result<()> {
    if queue.rows.is_empty() {
        return Ok(());
    }
    // Where the plan put each claim: page path and wiki, by fact id.
    let mut placed: HashMap<&str, (&str, &str)> = HashMap::new();
    for page in plan.pages.values() {
        for f in &page.primary_facts {
            placed.insert(
                f.fact_id.as_str(),
                (page.wiki_id.as_str(), page.page_path.as_str()),
            );
        }
    }
    for cap in &queue.rows {
        let Some((wiki_id, page_path)) = placed.get(cap.capture_id.as_str()).copied() else {
            // Not an error and not a loss: a claim nobody could place keeps
            // its buffer row and is offered to the next pass, and to the
            // nightly one that reads a whole wiki at once. The counter is
            // what separates *never looked at* from *looked at and declined*
            // — the difference the buffer used to carry by existing.
            if let Err(e) =
                capture_buffer::mark_placement_attempted(pool, &cap.capture_id, now).await
            {
                tracing::warn!(capture_id = %cap.capture_id, error = %e,
                    "light dream: placement attempt not recorded");
            }
            queue.report.left_waiting += 1;
            continue;
        };
        if let Err(e) = write_placed(
            pool,
            tree,
            embedder,
            cap,
            wiki_id,
            page_path,
            &mut queue.report,
        )
        .await
        {
            tracing::warn!(capture_id = %cap.capture_id, error = %e, "light dream: claim not written, left buffered");
            queue.report.errors.push(format!("{}: {e}", cap.capture_id));
        }
    }
    tracing::info!(
        scanned = queue.report.scanned,
        promoted = queue.report.promoted,
        left_waiting = queue.report.left_waiting,
        skipped_dup = queue.report.skipped_dup,
        superseded = queue.report.superseded,
        errors = queue.report.errors.len(),
        "light dream: cycle complete"
    );
    Ok(())
}

/// The promotion-time restated-known-fact miss check: load the buffered
/// capture's turn linkage, look back at what that turn surfaced, and
/// record a [`crate::recall_log`] miss when the deduped-into fact was
/// absent. Split out so the caller can treat any failure as ignorable
/// telemetry.
async fn miss_check(
    pool: &SqlitePool,
    cap: &BufferedCapture,
    dup: &crate::fact_index::FactIndexRow,
    score: f32,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(log_id) = capture_buffer::recall_log_id(pool, &cap.capture_id).await? else {
        return Ok(());
    };
    let Some(log) = crate::recall_log::find_log(pool, log_id).await? else {
        return Ok(()); // pruned past retention — nothing to compare against
    };
    if log.surfaced(dup.fact_id.as_str(), &dup.source_path) {
        return Ok(());
    }
    crate::recall_log::record_miss(
        pool,
        &crate::recall_log::NewMiss {
            created_at: &now(),
            sender_id: &log.sender_id,
            fact_id: dup.fact_id.as_str(),
            wiki_id: &dup.wiki_id,
            source_path: &dup.source_path,
            surface: crate::recall_log::MissSurface::Promotion,
            similarity: score,
            restated_text: &cap.body,
            log_id: Some(log_id),
            seed_topics: &log.topics,
        },
    )
    .await?;
    Ok(())
}

/// Drain the queue with **no model at all**: screen, place deterministically,
/// write the rows.
///
/// The placement is [`crate::planner::NewFactPlacement::Ingest`] — every page
/// the user's own turn named, and the identity fallback for the rest — so nothing
/// here needs a Cartografo or a Cronista. Two callers:
///
/// * a deployment with no prose writer configured, where the alternative is a
///   queue that grows for ever while the recall fresh slot (a ranked top-K)
///   quietly stops offering the older half of it;
/// * a test that wants a buffered claim readable as a fact, by the path the
///   product actually takes rather than by reaching into the buffer.
///
/// The pages are not written: each row lands addressed to its page with NULL
/// offsets — the same *pending render* state a live capture passes through —
/// and the first compile with a writer renders them.
///
/// # Errors
///
/// Surfaces infrastructure failures (DB / tree walk / planner).
pub async fn drain_deterministically(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    policy: &LightPolicy,
    now: &str,
) -> Result<LightCycleReport> {
    let mut queue = screen_queue(pool, tree, policy).await?;
    if queue.for_plan.is_empty() {
        return Ok(queue.report);
    }
    let plan = crate::planner::build_wiki_plan(
        pool,
        tree,
        crate::planner::NewFactPlacement::Ingest,
        None,
        now,
        &queue.for_plan,
    )
    .await?;
    materialise(pool, tree, embedder, &mut queue, &plan, now).await?;
    Ok(queue.report)
}

/// Do two waiting claims have the same audience — same subject, same author,
/// same `allow` set?
///
/// The intra-queue half of the rule the DB scan applies row-side
/// ([`crate::capture::Audience`]): two people saying the same thing with
/// different reach are two facts, not one (founder, 2026-08-18: *«i duplicati
/// possono esistere … se due utenti hanno detto la stessa cosa ma con acl
/// diversa»*).
fn same_audience(a: &BufferedCapture, b: &BufferedCapture) -> bool {
    a.subject == b.subject
        && a.sender == b.sender
        && a.allow.len() == b.allow.len()
        && a.allow.iter().all(|p| b.allow.contains(p))
        && b.allow.iter().all(|p| a.allow.contains(p))
}

/// Screen one claim: is it already remembered, and is there a wiki for it?
///
/// Returns the plan projection when the claim survives, `None` when it was
/// folded into an existing fact or has nowhere a page of its own could be born.
async fn screen_one(
    pool: &SqlitePool,
    cap: &BufferedCapture,
    on_disk: &HashMap<String, bool>,
    policy: &LightPolicy,
    report: &mut LightCycleReport,
) -> Result<Option<crate::planner::FactForPage>> {
    // Dedup — the direct path's own scan, deferred to here
    // ([`crate::capture::best_dedup_candidate`]: same-subject scope,
    // channel-page boundary, jaccard 6-gram vs the facts about this subject).
    // Exclude self so a retry after a partial run does not fold a claim into
    // its own fact. The embed SETS must also match: two photos with
    // near-identical captions share the words while linking different media —
    // dropping the second would orphan it.
    //
    // Scoped on the SUBJECT, across the whole forest
    // ([`fact_index::find_active_by_subject`]): the subject is the one thing
    // about a fact that never moves, so it is the only honest scope for a
    // question about sameness.
    let active = fact_index::find_active_by_subject(pool, &cap.subject).await?;
    // A waiting claim is never on a channel page: the three of them
    // (`@rules.md`, `@projects.md`, `@projects_diary.md`) are written by their
    // own deterministic paths, inside the turn, and never queue here. So the
    // fence below reads "compare me against ordinary prose only", which also
    // keeps a rule from being deduped away by a claim that merely sounds like
    // it ([`crate::wiki::is_channel_page`]).
    let on_channel_page = false;
    let audience = crate::capture::Audience {
        subject: &cap.subject,
        allow: &cap.allow,
        sender: cap.sender.as_ref(),
    };
    if let Some((dup, score)) = crate::capture::best_dedup_candidate(
        &active,
        &audience,
        on_channel_page,
        &cap.body,
        Some(&cap.capture_id),
    ) && score >= policy.dedup_threshold
        && crate::parser::collect_embeds(&dup.text) == crate::parser::collect_embeds(&cap.body)
    {
        tracing::info!(
            capture_id = %cap.capture_id,
            matched_fact_id = dup.fact_id.as_str(),
            similarity = score,
            threshold = policy.dedup_threshold,
            "light dream: SKIPPED (dedup hit)"
        );
        capture_buffer::mark_skipped_dup(pool, &cap.capture_id, &dup.fact_id, &now()).await?;
        report.skipped_dup += 1;
        // The judge-free restated-known-fact miss signal
        // ([`crate::recall_log`]): the user restated a fact memory already
        // held — did the ORIGINAL turn's recall surface it? The buffer row
        // carries the turn linkage; a row without one is skipped, and the whole
        // check is best-effort telemetry — a failure never touches the queue.
        // Channel-page facts are out of scope: a rule is channel-delivered and
        // a signpost is owned end-to-end by its writer, so neither is a recall
        // miss the repair loop could act on.
        if !crate::wiki::is_channel_page(&dup.source_path) {
            match miss_check(pool, cap, dup, score).await {
                Ok(()) => {},
                Err(e) => {
                    tracing::warn!(error = %e, "light dream: recall-miss check failed (ignored)");
                },
            }
        }
        return Ok(None);
    }

    let Some(home) = home_wiki(cap, on_disk) else {
        report.errors.push(format!(
            "{}: no wiki on disk for subject {} — left waiting",
            cap.capture_id, cap.subject
        ));
        return Ok(None);
    };
    Ok(Some(crate::planner::FactForPage {
        fact_id: cap.capture_id.clone(),
        text: cap.body.clone(),
        fact_type: cap.fact_type.clone(),
        subject: cap.subject.clone(),
        allow: cap.allow.clone(),
        sender: cap.sender.clone(),
        source_wiki_id: home,
        valid_from: cap.valid_from.clone(),
        valid_to: cap.valid_to.clone(),
        decay_reason: cap.decay_reason.clone(),
        successor_fact_id: None,
        // No page: nobody has placed this claim, which is exactly why it is
        // in front of the placement stage.
        target_page: None,
        style: cap.style,
        salience: cap.salience.clone(),
        authored_refs: cap.authored_refs.clone(),
    }))
}

/// Write one screened claim into `fact_index`, on the page the plan gave it.
async fn write_placed(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    cap: &BufferedCapture,
    wiki_id: &str,
    page_path: &str,
    report: &mut LightCycleReport,
) -> Result<()> {
    // A real file's address: `wikis/<wiki>/<page>`. The offsets stay NULL until
    // the compile writes the page moments later and `repoint_facts` stamps
    // them — the same "pending render" state a live capture passes through
    // between its insert and its page write.
    let source_path = format!("{}/{wiki_id}/{page_path}", crate::wiki::WIKIS_DIR);

    // Normally there is nothing to embed: `buffer_capture` already computed
    // this exact vector, over this exact text, when the claim was staged. The
    // call below is the fallback for a row that has none — one whose staging
    // hit a transient embedder fault. Same text either way, so a fact is
    // ranked identically whichever route its vector took.
    let embedding = match cap.embedding.clone() {
        Some(v) => v,
        None => {
            embedder
                .embed(&crate::parser::strip_embed_markers(&cap.body))
                .await?
        },
    };
    let new = NewFact {
        fact_id: cap.capture_id.clone(),
        wiki_id: wiki_id.to_owned(),
        source_path,
        region_start: None,
        region_end: None,
        text: cap.body.clone(),
        embedding,
        subject_id: cap.subject.clone(),
        allow_ids: cap.allow.clone(),
        sender_id: cap.sender.clone(),
        fact_type: cap.fact_type.clone(),
        topics: cap.topics.clone(),
        // The validity interval the classifier deduced and the buffer staged.
        valid_from: cap.valid_from.clone(),
        valid_to: cap.valid_to.clone(),
        // The page is where the plan just put it, so the carried placement IS
        // that page: a later build reads it back instead of re-judging.
        target_page: Some(page_path.to_owned()),
        style: cap.style,
        // Per-fact salience the classifier deduced.
        salience: cap.salience.clone(),
        // Source-document provenance (the document-ingest path) — audit and
        // citation, never prose.
        source_ref: cap.source_ref.clone(),
        // Group-17 provenance breadcrumbs, so consolidation links to the
        // project page instead of duplicating its body.
        authored_refs: cap.authored_refs.clone(),
    };
    fact_index::insert_if_absent(pool, &new).await?;

    // A closure gesture that landed while the claim was still waiting staged
    // its decay reason on the buffer (the closing `valid_to` already rode the
    // validity copy above); stamp the WHY onto the fresh fact. The insert
    // itself keeps its fresh-fact invariant.
    if let Some(reason) = &cap.decay_reason {
        fact_index::stamp_decay_reason(pool, &cap.capture_id, reason).await?;
    }

    // Apply the classifier's supersede hint, if the target is still active.
    if let Some(old) = &cap.supersede_hint
        && let Some(row) = fact_index::find_by_id(pool, old).await?
        && row.superseded_at.is_none()
        && row.deleted_at.is_none()
        && fact_index::mark_superseded(pool, old, &cap.capture_id).await? > 0
    {
        report.superseded += 1;
        // Disk half of the supersede (same pattern as
        // `capture::wiki_supersede`): excise the retired region's bytes from
        // its page. Best-effort — the tombstone already retired the fact, the
        // active ACL map redacts any residue fail-closed, and the hygiene
        // sweep picks up leftovers.
        if let Err(e) = crate::reindex::strip_fact_region(pool, tree, embedder.clone(), old).await {
            tracing::warn!(
                previous_fact_id = old.as_str(),
                error = %e,
                "light dream: supersede page-strip failed (redaction still applies)"
            );
        }
    }

    capture_buffer::mark_promoted(pool, &cap.capture_id, &now()).await?;
    report.promoted += 1;
    Ok(())
}

/// The wiki ids that actually exist on disk right now — the guard against
/// promoting a capture whose wiki was deleted while it waited.
/// The wikis a promoted fact may be parked in, by id — standard wikis only,
/// each flagged with whether it is an agent's own (`is_agent`).
///
/// Smart wikis are excluded outright: the engine does not write them, and a
/// fact parked in one would be compiled into a page a consumer owns.
fn wikis_on_disk(tree: &WikiTree) -> Result<HashMap<String, bool>> {
    Ok(tree
        .walk()?
        .into_iter()
        .filter(|d| !d.meta.smart)
        .map(|d| (d.meta.wiki_id.as_str().to_owned(), d.meta.is_agent))
        .collect())
}

/// The wiki a **new page** for this claim would be born in.
///
/// Not where the claim belongs: the placement stage is offered the whole
/// forest and may put it on any existing page, in any wiki. This answers only
/// the narrower question the plan cannot answer for itself — when a claim needs
/// a page that does not exist yet, whose wiki does that page join? (The plan
/// takes a new page's wiki from the first fact assigned to it.)
///
/// Derived, never remembered: the buffer carries no destination at all
/// (migration `0071_capture_buffer_no_destination`). The answer comes from the
/// one thing about a fact that never moves, its **subject** — an identity
/// wiki's id IS its principal's id, so the subject's own wiki is a lookup and
/// not a guess, and when the subject is an agent its own wiki is where a fact
/// about it belongs anyway.
///
/// The fallback is whoever said it, and it refuses an agent's wiki: a claim
/// about someone else must never land in the assistant's autobiography. A
/// subject with no wiki at all (a group named before it was enrolled) leaves
/// the claim waiting — better than a page in an arbitrary place.
fn home_wiki(cap: &BufferedCapture, on_disk: &HashMap<String, bool>) -> Option<String> {
    let id_of = |p: &Principal| match p {
        Principal::User(id) | Principal::Group(id) => id.as_str().to_owned(),
    };
    let home = id_of(&cap.subject);
    if on_disk.contains_key(&home) {
        return Some(home);
    }
    let sender = id_of(cap.sender.as_ref()?);
    (on_disk.get(&sender) == Some(&false)).then_some(sender)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureRequest;
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::types::{Principal, WikiId};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        write_wiki(&wikis, "alice");
        // Bob's too: promotion parks a fact in the SUBJECT's own wiki, so a
        // test with a fact about Bob needs one to exist.
        write_wiki(&wikis, "bob");
        let tree = WikiTree::open(dir.path()).expect("tree");
        // Both are ENROLLED, which is what gives their wikis an identity card.
        // Since 2026-08-22 the card is the only placement that needs no model:
        // without it a deterministic drain places nothing, which is correct
        // and would leave every test below asserting on an empty corpus.
        for u in ["alice", "bob"] {
            sqlx::query(
                "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES (?,'[]',0)",
            )
            .bind(u)
            .execute(&pool)
            .await
            .unwrap();
        }
        (dir, tree, pool)
    }

    fn write_wiki(wikis_dir: &Path, slug: &str) {
        let d = wikis_dir.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        let fm = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
        );
        std::fs::write(d.join("_meta.md"), fm).unwrap();
        std::fs::write(d.join("cucina.md"), "# index\n").unwrap();
    }

    /// A capture the deterministic drain can actually place.
    ///
    /// `salience: high` is the load-bearing field: since 2026-08-22 the only
    /// placement that needs no model is the identity one, so a claim without
    /// it stays in the buffer when no Cartografo runs — which is correct, and
    /// would make every test below assert on an empty corpus.
    fn cap_req(body: &str) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: "user:alice".parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("preference".to_owned()),
            topics: vec!["food".to_owned()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: Some("high".to_owned()),
        }
    }

    /// A fixed instant for the deterministic drains below — the plan is keyed
    /// on it, and a test that varies it varies nothing it is about.
    const NOW: &str = "2026-08-18T00:00:00Z";

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake",
            vec![0.1, 0.2, 0.3, 0.4],
        ))
    }

    #[tokio::test]
    async fn promotes_buffered_capture_to_fact() {
        let (_dir, tree, pool) = setup().await;
        let buffered = capture_buffer::buffer_capture(&pool, cap_req("Alice loves pasta."), None)
            .await
            .unwrap();

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.promoted, 1);
        assert_eq!(report.skipped_dup, 0);

        // The fact exists with fact_id == capture_id, and carries the "no page
        // yet" address until the compile places it.
        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.text, "Alice loves pasta.");
        // Born with a REAL page: the plan placed it before the row existed. No
        // model was involved — the deterministic placement had no page named
        // for this claim, so the identity fallback gave it the wiki's parking
        // page, which is a page like any other.
        assert_eq!(
            row.source_path,
            format!("wikis/alice/{}", crate::wiki::PROFILE_FILENAME)
        );
        assert!(
            row.region_start.is_none(),
            "offsets wait for the compile to write the page (pending render)"
        );
        assert_eq!(row.subject_id, "user:alice".parse::<Principal>().unwrap());

        // The buffer row is now promoted, not pending.
        assert_eq!(capture_buffer::count_buffered(&pool).await.unwrap(), 0);
    }

    /// **The invariant of the whole module: a fact is never born without a
    /// page.** Before 2026-08-18 a promoted claim carried a made-up address —
    /// `wikis/<wiki>/_pending.md`, a file nothing ever wrote — for the window
    /// between the promotion and the compile. Now the plan runs first, so every
    /// row's address is a real page's from the moment it exists.
    #[tokio::test]
    async fn no_fact_is_born_without_a_page() {
        let (_dir, tree, pool) = setup().await;
        for body in [
            "Alice ama la pasta.",
            "Alice nuota il martedì.",
            "Bob ha una barca.",
        ] {
            let mut req = cap_req(body);
            if body.starts_with("Bob") {
                req.subject = "user:bob".parse::<Principal>().unwrap();
            }
            capture_buffer::buffer_capture(&pool, req, None)
                .await
                .unwrap();
        }

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.promoted, 3, "{report:?}");

        for wiki in ["alice", "bob"] {
            for row in fact_index::find_active_in_wiki(&pool, wiki).await.unwrap() {
                let name = std::path::Path::new(&row.source_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                assert!(
                    !name.is_empty() && !name.starts_with('_'),
                    "a fact must be born on a real page, got `{}`",
                    row.source_path
                );
                assert!(
                    row.source_path.starts_with(&format!("wikis/{wiki}/")),
                    "and inside its own wiki: {}",
                    row.source_path
                );
            }
        }
    }

    #[tokio::test]
    async fn promotion_carries_validity_into_fact_index() {
        // The validity interval the classifier deduced (carried on the
        // CaptureRequest) must survive buffer → journal → promote and land in
        // fact_index — the gap on the standard-wiki path.
        let (_dir, tree, pool) = setup().await;
        let mut req = cap_req("Sono a Berlino questa settimana.");
        req.valid_from = Some("2026-06-06T00:00:00+00:00".to_owned());
        req.valid_to = Some("2026-06-13T00:00:00+00:00".to_owned());
        let buffered = capture_buffer::buffer_capture(&pool, req, None)
            .await
            .unwrap();

        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.valid_from.as_deref(), Some("2026-06-06T00:00:00+00:00"));
        assert_eq!(row.valid_to.as_deref(), Some("2026-06-13T00:00:00+00:00"));
    }

    /// A claim carries how it should READ into the queue, and comes out with
    /// the page the plan gave it.
    ///
    /// `style` says what shape the material has — list, prose, technical prose
    /// — and survives the wait onto the fact, where a page takes its style from
    /// the majority of its facts. The page the turn happened to name does NOT
    /// survive: the buffer holds no destination, and placing the claim is the
    /// dream's call at the moment it reads the queue (founder, 2026-08-18).
    #[tokio::test]
    async fn a_claim_carries_its_style_and_gets_its_page_from_the_plan() {
        let (_dir, tree, pool) = setup().await;
        let mut req = cap_req("Alice preferisce il tè.");
        // The live route's field. The buffer drops it; asserted below.
        req.page = Some(PathBuf::from("preferenze.md"));
        req.style = Some(crate::wiki::PageStyle::ProsaTecnica);
        req.page_description = Some("Le preferenze di Alice".to_owned());
        let buffered = capture_buffer::buffer_capture(&pool, req, None)
            .await
            .unwrap();

        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(
            row.target_page.as_deref(),
            Some(crate::wiki::PROFILE_FILENAME),
            "the page is the plan's answer, not the turn's proposal"
        );
        assert_eq!(
            row.source_path,
            format!("wikis/alice/{}", crate::wiki::PROFILE_FILENAME),
            "and it is a real page in the subject's own wiki"
        );
        assert_eq!(
            row.style,
            Some(crate::wiki::PageStyle::ProsaTecnica),
            "how the material READS survives the wait; where it goes does not"
        );
    }

    /// The parking spot is the SUBJECT's wiki, never the one the classifier
    /// happened to name: the buffer names none, and the subject is the one
    /// thing about a fact that never moves.
    #[tokio::test]
    async fn promotion_parks_the_fact_in_the_subjects_own_wiki() {
        let (_dir, tree, pool) = setup().await;
        let mut req = cap_req("Bob ha una barca.");
        req.subject = "user:bob".parse::<Principal>().unwrap();
        // Alice is who said it, and the wiki the plan had in hand.
        req.sender = Some("user:alice".parse::<Principal>().unwrap());
        req.wiki_id = WikiId::parse("alice").unwrap();
        let buffered = capture_buffer::buffer_capture(&pool, req, None)
            .await
            .unwrap();

        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.wiki_id, "bob");
        assert_eq!(
            row.sender_id.as_ref().map(ToString::to_string).as_deref(),
            Some("user:alice"),
            "and who said it is untouched"
        );
    }

    /// A subject with no wiki of its own is parked with whoever said it —
    /// the claim is not lost, and the placement pass moves it wherever it
    /// reads best.
    #[tokio::test]
    async fn promotion_falls_back_to_the_senders_wiki() {
        let (_dir, tree, pool) = setup().await;
        let mut req = cap_req("Il nuovo vicino si chiama Dario.");
        req.subject = "user:dario".parse::<Principal>().unwrap();
        req.sender = Some("user:alice".parse::<Principal>().unwrap());
        let buffered = capture_buffer::buffer_capture(&pool, req, None)
            .await
            .unwrap();

        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.wiki_id, "alice");
    }

    #[tokio::test]
    async fn exact_duplicate_is_skipped_not_double_inserted() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(&pool, cap_req("Buy milk"), None)
            .await
            .unwrap();
        // Same claim, different whitespace/case → exact-dup after normalisation.
        capture_buffer::buffer_capture(&pool, cap_req("buy   MILK"), None)
            .await
            .unwrap();

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.promoted, 1);
        assert_eq!(report.skipped_dup, 1);
        assert_eq!(
            fact_index::find_active_in_wiki(&pool, "alice")
                .await
                .unwrap()
                .len(),
            1,
            "only one fact for two exact-duplicate captures"
        );
    }

    /// The promotion half of the judge-free miss signal: the buffered
    /// restatement dedups into an existing fact, and its logged turn had
    /// NOT surfaced that fact → one `recall_miss` (surface `promotion`),
    /// linked to the turn. When a later logged turn DID surface it, the
    /// dedup fold records nothing.
    #[tokio::test]
    async fn promotion_dedup_records_a_recall_miss_only_when_the_turn_missed_it() {
        let (_dir, tree, pool) = setup().await;
        // The fact memory already holds.
        let first = capture_buffer::buffer_capture(&pool, cap_req("Buy milk"), None)
            .await
            .unwrap();
        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        // A restatement from a turn whose recall surfaced nothing.
        let blind = capture_buffer::buffer_capture(&pool, cap_req("buy   MILK"), None)
            .await
            .unwrap();
        // Relative to now, never a pinned date: `record_turn` prunes on write
        // against `Utc::now() - RECALL_LOG_RETENTION_DAYS`, so an absolute
        // fixture stops being a live row on a calendar day and takes the test
        // with it (it did, on 2026-08-04).
        let blind_log = crate::recall_log::record_turn(
            &pool,
            "alice",
            &(chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
        capture_buffer::stamp_recall_log(&pool, std::slice::from_ref(&blind.capture_id), blind_log)
            .await
            .unwrap();
        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.skipped_dup, 1);
        let misses = crate::recall_log::recent_misses(&pool, 10).await.unwrap();
        assert_eq!(misses.len(), 1, "{misses:?}");
        assert_eq!(misses[0].fact_id, first.capture_id.as_str());
        assert_eq!(misses[0].surface, "promotion");
        assert_eq!(misses[0].sender_id, "alice");
        assert_eq!(misses[0].log_id, Some(blind_log));

        // A restatement from a turn that DID surface the fact → no miss.
        let seen = capture_buffer::buffer_capture(&pool, cap_req("BUY milk"), None)
            .await
            .unwrap();
        let seen_log = crate::recall_log::record_turn(
            &pool,
            "alice",
            &chrono::Utc::now().to_rfc3339(),
            &[first.capture_id.as_str().to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();
        capture_buffer::stamp_recall_log(&pool, std::slice::from_ref(&seen.capture_id), seen_log)
            .await
            .unwrap();
        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.skipped_dup, 1);
        assert_eq!(
            crate::recall_log::recent_misses(&pool, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a surfaced fact is a plain dedup, not a second miss"
        );
    }

    #[tokio::test]
    async fn supersede_hint_retires_the_prior_fact() {
        let (_dir, tree, pool) = setup().await;
        // First capture, promoted to a fact.
        let first = capture_buffer::buffer_capture(&pool, cap_req("Alice prefers coffee."), None)
            .await
            .unwrap();
        drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();

        // Second capture supersedes the first.
        capture_buffer::buffer_capture(
            &pool,
            cap_req("Alice now prefers tea."),
            Some(first.capture_id.clone()),
        )
        .await
        .unwrap();
        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.promoted, 1);
        assert_eq!(report.superseded, 1);

        let old = fact_index::find_by_id(&pool, &first.capture_id)
            .await
            .unwrap()
            .unwrap();
        assert!(old.superseded_at.is_some(), "old fact must be superseded");
    }

    /// When the superseded fact has a **rendered** on-disk region (it went
    /// through the direct-write path before the buffered supersede), the
    /// light dream's supersede hint also excises those bytes — the same
    /// disk half `capture::wiki_supersede` performs.
    #[tokio::test]
    async fn supersede_hint_strips_the_prior_facts_rendered_region() {
        let (dir, tree, pool) = setup().await;
        // A rendered fact with a real `{{f=…}}` region on cucina.md.
        let outcome = crate::capture::wiki_capture(
            &tree,
            &pool,
            embedder(),
            cap_req("Alice prefers coffee."),
        )
        .await
        .unwrap();
        let old = outcome.fact_id;
        let page_abs = dir.path().join("wikis/alice/cucina.md");
        assert!(
            std::fs::read_to_string(&page_abs)
                .unwrap()
                .contains(old.as_str()),
            "precondition: the old fact is rendered on disk"
        );

        capture_buffer::buffer_capture(&pool, cap_req("Alice now prefers tea."), Some(old.clone()))
            .await
            .unwrap();
        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.superseded, 1);

        let page = std::fs::read_to_string(&page_abs).unwrap();
        assert!(
            !page.contains(old.as_str()),
            "superseded region must be excised from the page: {page}"
        );
    }

    #[tokio::test]
    async fn cycle_is_idempotent() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(&pool, cap_req("Only claim."), None)
            .await
            .unwrap();
        let r1 = drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();
        assert_eq!(r1.promoted, 1);
        // Nothing left buffered → a second cycle is a no-op.
        let r2 = drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
            .await
            .unwrap();
        assert_eq!(r2.scanned, 0);
        assert_eq!(r2.promoted, 0);
        assert_eq!(
            fact_index::find_active_in_wiki(&pool, "alice")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn promotion_carries_a_buffered_closure_onto_the_fact() {
        // The same-day flow: a closure gesture lands while its target is
        // still buffered ("compra il latte" → "comprato" before the light
        // dream). The buffer staged valid_to + decay_reason; the promoted
        // fact must carry both.
        let (_dir, tree, pool) = setup().await;
        let buffered = capture_buffer::buffer_capture(&pool, cap_req("Serve il latte."), None)
            .await
            .unwrap();
        capture_buffer::close_validity(
            &pool,
            &buffered.capture_id,
            "2026-06-11T18:00:00Z",
            fact_index::decay::COMPLETED,
        )
        .await
        .unwrap()
        .expect("buffered row closed");

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.promoted, 1);
        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact");
        assert_eq!(row.valid_to.as_deref(), Some("2026-06-11T18:00:00Z"));
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
    }

    /// Promotion parity with the direct path: a NEAR-duplicate above the
    /// jaccard threshold folds into the existing fact — the buffered path
    /// no longer needs the REM night for a paraphrase a live write would
    /// have skipped on the spot.
    #[tokio::test]
    async fn near_duplicate_above_threshold_folds_at_promotion() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(
            &pool,
            cap_req("Alice must pick up Matteo at the summer camp at 13:15."),
            None,
        )
        .await
        .unwrap();
        // Same claim, trailing punctuation delta — high jaccard, not exact.
        capture_buffer::buffer_capture(
            &pool,
            cap_req("Alice must pick up Matteo at the summer camp at 13:15!!"),
            None,
        )
        .await
        .unwrap();

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.promoted, 1);
        assert_eq!(report.skipped_dup, 1);
        assert_eq!(
            fact_index::find_active_in_wiki(&pool, "alice")
                .await
                .unwrap()
                .len(),
            1,
            "one fact survives for two near-duplicate captures"
        );
    }

    /// Subject discipline at promotion (the direct path's same-subject
    /// scope, ported): the SAME text under two different subjects is two
    /// facts — one fragment's subject is never folded into another principal's.
    #[tokio::test]
    async fn same_text_different_subject_is_not_folded() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(&pool, cap_req("The cat is called Felix."), None)
            .await
            .unwrap();
        let mut other = cap_req("The cat is called Felix.");
        other.subject = "user:bob".parse::<Principal>().unwrap();
        capture_buffer::buffer_capture(&pool, other, None)
            .await
            .unwrap();

        let report =
            drain_deterministically(&pool, &tree, &embedder(), &LightPolicy::default(), NOW)
                .await
                .unwrap();
        assert_eq!(report.promoted, 2, "cross-subject pair must not dedup");
        assert_eq!(report.skipped_dup, 0);
    }
}
