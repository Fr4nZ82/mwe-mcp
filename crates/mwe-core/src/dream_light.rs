// SPDX-License-Identifier: AGPL-3.0-or-later
//! The **light dream** — promote buffered captures into `fact_index`.
//!
//! The light dream is the frequent, cheap half of the "two dream"
//! cadence (the other being the nightly REM full reorg, [`crate::rem`]). It
//! drains the [captures buffer](crate::capture_buffer): each `buffered` capture
//! is embedded and inserted into `fact_index` as a permanent fact, after which
//! it is recallable. This is what closes the gap where a standard-wiki
//! capture was durably buffered but not yet a fact.
//!
//! Per buffered capture, in order:
//!
//! 1. **Dedup skip — the direct path's own check, deferred.** The same
//!    jaccard 6-gram scan a live [`crate::capture::wiki_capture`] runs
//!    (same-owner scope, rules-page boundary, embed-set guard, the same
//!    `dedup_threshold` default): when an active fact in the wiki already
//!    carries the claim at ≥ threshold, the capture resolves to that
//!    survivor (`skipped_dup`) and no new fact is created. Parity is the
//!    point — a buffered capture must get exactly the dedup it would have
//!    gotten written live; without it the buffered path had NO similarity
//!    dedup anywhere in its lifecycle (promotion was exact-string only,
//!    and the REM revisor deliberately skips pairs above the threshold
//!    on the assumption capture already folded them). Sub-threshold
//!    paraphrases remain the REM night's semantic dedup job. Still no
//!    LLM here — the scan is pure CPU.
//! 2. **Embed + insert.** Otherwise the body is embedded and inserted as a fact
//!    whose `fact_id` **is** the `capture_id` (id stability — see
//!    [`crate::capture_buffer`]). The insert is idempotent
//!    ([`fact_index::insert_if_absent`]).
//! 3. **Supersede.** If the classifier flagged a `supersede_hint` and that fact
//!    is still active, it is marked superseded by the new one. The decision was
//!    the classifier's; the light dream just applies it deterministically (no
//!    LLM judgement here).
//! 4. **Resolve.** The `capture_buffer` row is stamped `promoted` /
//!    `skipped_dup` with its `resolved_fact_id`.
//!
//! ## Where the promoted fact lives (pre-compilation)
//!
//! A promoted-but-not-yet-compiled fact has no published page yet — the Cronista
//! writes pages later. Its `source_path` therefore points at the wiki's
//! `_captures.md` journal and its region offsets are `NULL`. Because the journal
//! is excluded from the reindex marker sweep ([`crate::reindex`]), these facts
//! are never orphaned by a page reindex; recall serves them straight from
//! `fact_index.text`. When the Cronista compiles a fact into a page it repoints
//! `source_path` + offsets and the journal entry is compacted.
//!
//! ## Idempotency & crash-safety
//!
//! The light dream only ever advances `buffered` rows, the insert is
//! `insert_if_absent`, `mark_superseded` no-ops on an already-superseded row,
//! and the status updates are guarded on `status = 'buffered'`. So a crash
//! mid-cycle, or a re-run after `rm engine.db` (which rebuilds the buffer rows
//! as `buffered` from the journal), simply re-promotes idempotently — the stable
//! `capture_id == fact_id` is what makes that safe.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture_buffer::{self, BufferedCapture, CaptureBufferError};
use crate::embedder::{Embedder, EmbedderError};
use crate::fact_index::{self, FactIndexError, NewFact};
use crate::wiki::{CAPTURES_FILENAME, WikiError, WikiTree, workdir_relative_source_path};

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
    /// Embedding a capture body failed. Raised inside `promote_one`, where it is
    /// caught per-capture (the capture stays buffered for the next cycle) rather
    /// than aborting the whole cycle.
    #[error("light dream embedder: {0}")]
    Embedder(#[from] EmbedderError),
    /// Walking the wiki tree to map journals failed.
    #[error("light dream wiki: {0}")]
    Wiki(#[from] WikiError),
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
    /// Per-capture soft errors (`"<capture_id>: <error>"`); the cycle continues.
    pub errors: Vec<String>,
}

/// Run one light cycle: drain up to `policy.max_promotions_per_cycle` buffered
/// captures into `fact_index`.
///
/// # Errors
///
/// Surfaces infrastructure failures (DB / tree walk). Per-capture failures
/// (e.g. a transient embedder error) are collected into `report.errors` and the
/// capture is left `buffered` for the next cycle.
pub async fn run_light_cycle(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    policy: &LightPolicy,
) -> Result<LightCycleReport> {
    let mut report = LightCycleReport::default();
    let captures = capture_buffer::find_all_buffered(pool, policy.max_promotions_per_cycle).await?;
    report.scanned = captures.len();
    if captures.is_empty() {
        return Ok(report);
    }
    let journal_paths = build_journal_path_map(tree)?;
    for cap in captures {
        if let Err(e) = promote_one(
            pool,
            tree,
            &embedder,
            &cap,
            &journal_paths,
            policy,
            &mut report,
        )
        .await
        {
            tracing::warn!(capture_id = %cap.capture_id, error = %e, "light dream: capture failed, left buffered");
            report.errors.push(format!("{}: {e}", cap.capture_id));
        }
    }
    tracing::info!(
        scanned = report.scanned,
        promoted = report.promoted,
        skipped_dup = report.skipped_dup,
        superseded = report.superseded,
        errors = report.errors.len(),
        "light dream: cycle complete"
    );
    Ok(report)
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

async fn promote_one(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    cap: &BufferedCapture,
    journal_paths: &HashMap<String, String>,
    policy: &LightPolicy,
    report: &mut LightCycleReport,
) -> Result<()> {
    let wiki = cap.wiki_id.as_str();
    let Some(source_path) = journal_paths.get(wiki).cloned() else {
        // Wiki vanished between buffering and promotion: leave the capture
        // buffered and surface it as a soft error rather than minting an
        // orphan fact whose source_path points nowhere.
        report
            .errors
            .push(format!("{}: wiki {wiki} not found on disk", cap.capture_id));
        return Ok(());
    };

    // Dedup skip — the direct path's own scan, deferred to promotion
    // ([`crate::capture::best_dedup_candidate`]: same-owner scope,
    // channel-page boundary, jaccard 6-gram vs the wiki's active facts).
    // Exclude self so a retry after a partial promotion does not skip
    // the capture against its own fact. The embed SETS must also match:
    // two photos with near-identical captions share the words while
    // linking different media — dropping the second would orphan it.
    let active = fact_index::find_active_in_wiki(pool, wiki).await?;
    let on_channel_page = crate::wiki::is_channel_page(&cap.target_page.to_string_lossy());
    if let Some((dup, score)) = crate::capture::best_dedup_candidate(
        &active,
        &cap.owner,
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
            "light dream: SKIPPED (dedup hit at promotion)"
        );
        capture_buffer::mark_skipped_dup(pool, &cap.capture_id, &dup.fact_id, &now()).await?;
        report.skipped_dup += 1;
        // The promotion half of the judge-free restated-known-fact miss
        // signal ([`crate::recall_log`]): the user restated a fact memory
        // already held — did the ORIGINAL turn's recall surface it? The
        // buffer row carries the turn linkage; a row without one (legacy,
        // journal-recovered) is skipped, and the whole check is best-effort
        // telemetry — a failure never touches the promotion. Channel-page
        // facts are out of scope: a rule is channel-delivered, and a
        // signpost is owned end-to-end by its writer, so neither is a
        // recall miss the repair loop could act on.
        if !crate::wiki::is_channel_page(&dup.source_path) {
            match miss_check(pool, cap, dup, score).await {
                Ok(()) => {},
                Err(e) => {
                    tracing::warn!(error = %e, "light dream: recall-miss check failed (ignored)");
                },
            }
        }
        return Ok(());
    }

    // Embed + insert (idempotent). The fact_id IS the capture_id. A failed
    // embed surfaces as a per-capture soft error (caught by run_light_cycle);
    // the capture stays buffered for the next cycle. The embedding reads
    // the marker-stripped text (the marker is semantic noise).
    let embedding = embedder
        .embed(&crate::parser::strip_embed_markers(&cap.body))
        .await?;
    let new = NewFact {
        fact_id: cap.capture_id.clone(),
        wiki_id: wiki.to_owned(),
        source_path,
        region_start: None,
        region_end: None,
        text: cap.body.clone(),
        embedding,
        owner_id: cap.owner.clone(),
        allow_ids: cap.allow.clone(),
        sender_id: cap.sender.clone(),
        fact_type: cap.fact_type.clone(),
        topics: cap.topics.clone(),
        // Carry the validity interval the buffer staged (the classifier deduced
        // it; the buffer→journal path now preserves it) into fact_index —
        // closing the gap on the standard-wiki path and unblocking the validity
        // render in compiled prose.
        valid_from: cap.valid_from.clone(),
        valid_to: cap.valid_to.clone(),
        // Carry the ingest PLACEMENT onto the fact so the light cadence can
        // settle it on its ingest page without re-running the Cartografo.
        // `target_page` rode the buffer all along; we stop dropping it at
        // promotion. A hint — the plan/REM stays authoritative.
        target_page: Some(cap.target_page.to_string_lossy().into_owned()),
        style: cap.style.clone(),
        page_description: cap.page_description.clone(),
        // Carry the per-fact salience the buffer staged (the classifier deduced
        // it) onto the fact, so recall can route a `high`-salience fact to the
        // actor-wiki index.md base context.
        salience: cap.salience.clone(),
        // Carry the source-document provenance the buffer staged (the
        // document-ingest path) onto the fact — audit/citation, never prose.
        source_ref: cap.source_ref.clone(),
        // Carry the group-17 provenance breadcrumbs the buffer staged onto the
        // fact, so consolidation links to the project page instead of
        // duplicating its body.
        authored_refs: cap.authored_refs.clone(),
    };
    fact_index::insert_if_absent(pool, &new).await?;

    // A closure gesture that landed while the capture was still buffered
    // staged its decay reason on the buffer (the closing valid_to already
    // rode the validity copy above); stamp the WHY onto the freshly
    // promoted fact. The insert itself keeps its fresh-fact invariant.
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
        // Disk half of the supersede (same pattern as `capture::wiki_supersede`):
        // excise the retired region's bytes from its page. Best-effort — the
        // tombstone already retired the fact, the active ACL map redacts any
        // residue fail-closed, and the hygiene sweep picks up leftovers.
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

/// Map every wiki id to the workdir-relative path of its `_captures.md` journal.
/// A promoted-but-uncompiled fact takes this as its `source_path`.
fn build_journal_path_map(tree: &WikiTree) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for d in tree.walk()? {
        let journal = d.abs_dir.join(CAPTURES_FILENAME);
        let rel = workdir_relative_source_path(tree.workdir(), &journal);
        map.insert(d.meta.wiki_id.as_str().to_owned(), rel);
    }
    Ok(map)
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
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    fn write_wiki(wikis_dir: &Path, slug: &str) {
        let d = wikis_dir.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        let fm = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
        );
        std::fs::write(d.join("_meta.md"), fm).unwrap();
        std::fs::write(d.join("index.md"), "# index\n").unwrap();
    }

    fn cap_req(body: &str) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("preference".to_owned()),
            topics: vec!["food".to_owned()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        }
    }

    fn embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake",
            vec![0.1, 0.2, 0.3, 0.4],
        ))
    }

    #[tokio::test]
    async fn promotes_buffered_capture_to_fact() {
        let (_dir, tree, pool) = setup().await;
        let buffered =
            capture_buffer::buffer_capture(&tree, &pool, cap_req("Alice loves pasta."), None)
                .await
                .unwrap();

        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.promoted, 1);
        assert_eq!(report.skipped_dup, 0);

        // The fact exists with fact_id == capture_id and the journal source_path.
        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.text, "Alice loves pasta.");
        assert!(row.source_path.ends_with("_captures.md"));
        assert!(row.region_start.is_none());
        assert_eq!(row.owner_id, "user:alice".parse::<Principal>().unwrap());

        // The buffer row is now promoted, not pending.
        assert_eq!(capture_buffer::count_buffered(&pool).await.unwrap(), 0);
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
        let buffered = capture_buffer::buffer_capture(&tree, &pool, req, None)
            .await
            .unwrap();

        run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.valid_from.as_deref(), Some("2026-06-06T00:00:00+00:00"));
        assert_eq!(row.valid_to.as_deref(), Some("2026-06-13T00:00:00+00:00"));
    }

    #[tokio::test]
    async fn promotion_carries_placement_into_fact_index() {
        // The ingest PLACEMENT (target_page + style + page_description) must
        // survive buffer → journal → promote and land on the fact, so the
        // light cadence can settle it without re-running the Cartografo. (Still
        // inert here — no consumer reads these columns yet.)
        let (_dir, tree, pool) = setup().await;
        let mut req = cap_req("Alice preferisce il tè.");
        req.page = PathBuf::from("preferenze.md");
        req.style = Some("prosa-tecnica".to_owned());
        req.page_description = Some("Le preferenze di Alice".to_owned());
        let buffered = capture_buffer::buffer_capture(&tree, &pool, req, None)
            .await
            .unwrap();

        run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();

        let row = fact_index::find_by_id(&pool, &buffered.capture_id)
            .await
            .unwrap()
            .expect("promoted fact exists");
        assert_eq!(row.target_page.as_deref(), Some("preferenze.md"));
        assert_eq!(row.style.as_deref(), Some("prosa-tecnica"));
        assert_eq!(
            row.page_description.as_deref(),
            Some("Le preferenze di Alice")
        );
    }

    #[tokio::test]
    async fn exact_duplicate_is_skipped_not_double_inserted() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(&tree, &pool, cap_req("Buy milk"), None)
            .await
            .unwrap();
        // Same claim, different whitespace/case → exact-dup after normalisation.
        capture_buffer::buffer_capture(&tree, &pool, cap_req("buy   MILK"), None)
            .await
            .unwrap();

        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        let first = capture_buffer::buffer_capture(&tree, &pool, cap_req("Buy milk"), None)
            .await
            .unwrap();
        run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();

        // A restatement from a turn whose recall surfaced nothing.
        let blind = capture_buffer::buffer_capture(&tree, &pool, cap_req("buy   MILK"), None)
            .await
            .unwrap();
        let blind_log = crate::recall_log::record_turn(
            &pool,
            "alice",
            "2026-07-05T10:00:00+00:00",
            &[],
            &[],
            &[],
        )
        .await
        .unwrap();
        capture_buffer::stamp_recall_log(&pool, std::slice::from_ref(&blind.capture_id), blind_log)
            .await
            .unwrap();
        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        let seen = capture_buffer::buffer_capture(&tree, &pool, cap_req("BUY milk"), None)
            .await
            .unwrap();
        let seen_log = crate::recall_log::record_turn(
            &pool,
            "alice",
            "2026-07-05T11:00:00+00:00",
            &[first.capture_id.as_str().to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();
        capture_buffer::stamp_recall_log(&pool, std::slice::from_ref(&seen.capture_id), seen_log)
            .await
            .unwrap();
        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        let first =
            capture_buffer::buffer_capture(&tree, &pool, cap_req("Alice prefers coffee."), None)
                .await
                .unwrap();
        run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();

        // Second capture supersedes the first.
        capture_buffer::buffer_capture(
            &tree,
            &pool,
            cap_req("Alice now prefers tea."),
            Some(first.capture_id.clone()),
        )
        .await
        .unwrap();
        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        // A rendered fact with a real `{{f=…}}` region on index.md.
        let outcome = crate::capture::wiki_capture(
            &tree,
            &pool,
            embedder(),
            cap_req("Alice prefers coffee."),
        )
        .await
        .unwrap();
        let old = outcome.fact_id;
        let page_abs = dir.path().join("wikis/alice/index.md");
        assert!(
            std::fs::read_to_string(&page_abs)
                .unwrap()
                .contains(old.as_str()),
            "precondition: the old fact is rendered on disk"
        );

        capture_buffer::buffer_capture(
            &tree,
            &pool,
            cap_req("Alice now prefers tea."),
            Some(old.clone()),
        )
        .await
        .unwrap();
        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        capture_buffer::buffer_capture(&tree, &pool, cap_req("Only claim."), None)
            .await
            .unwrap();
        let r1 = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();
        assert_eq!(r1.promoted, 1);
        // Nothing left buffered → a second cycle is a no-op.
        let r2 = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
        let buffered =
            capture_buffer::buffer_capture(&tree, &pool, cap_req("Serve il latte."), None)
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

        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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
            &tree,
            &pool,
            cap_req("Alice must pick up Matteo at the summer camp at 13:15."),
            None,
        )
        .await
        .unwrap();
        // Same claim, trailing punctuation delta — high jaccard, not exact.
        capture_buffer::buffer_capture(
            &tree,
            &pool,
            cap_req("Alice must pick up Matteo at the summer camp at 13:15!!"),
            None,
        )
        .await
        .unwrap();

        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
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

    /// Ownership discipline at promotion (the direct path's same-owner
    /// scope, ported): the SAME text under two different owners is two
    /// facts — per-fragment ownership is never folded across principals.
    #[tokio::test]
    async fn same_text_different_owner_is_not_folded() {
        let (_dir, tree, pool) = setup().await;
        capture_buffer::buffer_capture(&tree, &pool, cap_req("The cat is called Felix."), None)
            .await
            .unwrap();
        let mut other = cap_req("The cat is called Felix.");
        other.owner = "user:bob".parse::<Principal>().unwrap();
        capture_buffer::buffer_capture(&tree, &pool, other, None)
            .await
            .unwrap();

        let report = run_light_cycle(&pool, &tree, embedder(), &LightPolicy::default())
            .await
            .unwrap();
        assert_eq!(report.promoted, 2, "cross-owner pair must not dedup");
        assert_eq!(report.skipped_dup, 0);
    }
}
