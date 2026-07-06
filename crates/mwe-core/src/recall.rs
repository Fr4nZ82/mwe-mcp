// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recall helpers + multi-modal recall pipeline.
//!
//! Two layers in one module:
//!
//! - **Pure helpers** (top of file): `ngrams`, `jaccard_*`,
//!   `cosine_similarity`. Side-effect free, deterministic, used both
//!   here (vector ranking) and by [`crate::capture`] (dedup at
//!   capture time).
//! - **Async orchestrators** (bottom of file):
//!   `_internal.wiki_search`, `wiki_facts_for`, `wiki_recall`. They
//!   wrap the DB layer
//!   ([`crate::fact_index`]), embed query text via the supplied
//!   [`crate::embedder::Embedder`], apply the
//!   [`crate::acl::can_read`] filter post-fetch, and bump recall
//!   counters in `fact_index` for every row they return.
//!
//! ### Why character 6-grams
//!
//! The legacy MWE plugin had to deduplicate short bursts ("manca il
//! latte" vs "manca il pane") and word-level tokenization tanked
//! recall on Italian compound forms. Character 6-grams give a robust
//! substring fingerprint that survives minor reword and typos while
//! staying cheap to compute.
//!
//! ### Why brute-force cosine
//!
//! Recall scores embeddings via a flat O(N) loop. The target
//! workdir size (low thousands of active regions) keeps a full scan
//! in single-digit milliseconds with bge-m3 1024-d vectors. A
//! vector-index integration (`sqlite-vec`) is deferred work — until
//! profiling shows we need it.

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::SqlitePool;
use thiserror::Error;

use crate::acl::can_read;
use crate::capture_buffer::{self, BufferedCapture, CaptureBufferError};
use crate::embedder::{Embedder, EmbedderError};
use crate::fact_index::{self, FactIndexError, FactIndexRow};
use crate::types::{Acl, FactId, Principal};

/// Default size of the n-gram window used across the dedup pipeline.
/// 6 characters is the legacy-MWE default that proved its keep against
/// `manca il latte` / `manca il pane` collisions.
pub const DEFAULT_NGRAM: usize = 6;

/// Default threshold above which `wiki_capture` treats a new capture as
/// a near-duplicate of an existing fact (and short-circuits with
/// `CaptureAction::Skipped`).
///
/// Empirical: 0.85 is the value the legacy plugin landed on; tweakable
/// per-call via `CaptureRequest::dedup_threshold` when the type-specific
/// `on_dedup_match` policy needs a different sensitivity.
pub const DEFAULT_DEDUP_THRESHOLD: f32 = 0.85;

/// Cap on buffered captures embedded per recall turn for the mid-range
/// "fresh" slot (see [`recall_fresh_captures`]). The light dream drains the
/// buffer at its backlog threshold, so the pending set is normally a handful;
/// this bounds the worst case. PROVISIONAL: the optimisation (embed once at
/// capture time, store the vector — option C) is a tracked follow-up.
const FRESH_CANDIDATE_CAP: i64 = 32;

/// Tokenize `text` into a set of character n-grams (window = `n`).
///
/// Behaviour:
/// - Inputs are case-folded with `to_lowercase` before windowing so
///   "Pasta" and "pasta" dedup against each other.
/// - Runs of ASCII whitespace are collapsed to a single space (so the
///   number of newlines and tabs does not perturb the fingerprint).
/// - When `text.chars().count() < n`, the function returns a single
///   n-gram equal to the (normalised) text padded on the right with
///   `\u{0}` so a very short input still has *some* fingerprint to
///   compare against.
///
/// Returns the set of n-grams as `String`s (one allocation per
/// distinct gram). The size of the working set is bounded by the
/// number of code points in `text`.
#[must_use]
pub fn ngrams(text: &str, n: usize) -> HashSet<String> {
    let normalised: String = normalise(text);
    let chars: Vec<char> = normalised.chars().collect();
    let mut out: HashSet<String> = HashSet::new();
    if chars.is_empty() {
        return out;
    }
    if chars.len() < n {
        // Pad to length `n` with NUL so short inputs still produce a
        // single, stable n-gram. NUL is chosen because it cannot
        // appear in the normalised input (we strip control chars in
        // `normalise`).
        let mut padded = String::with_capacity(n);
        padded.extend(chars.iter());
        for _ in chars.len()..n {
            padded.push('\u{0}');
        }
        out.insert(padded);
        return out;
    }
    for window in chars.windows(n) {
        out.insert(window.iter().collect());
    }
    out
}

/// Jaccard similarity of `a`'s and `b`'s character n-gram sets:
/// `|A ∩ B| / |A ∪ B|`, in `[0.0, 1.0]`.
///
/// Returns `1.0` when both inputs are empty (the empty set is its own
/// duplicate). Returns `0.0` when exactly one side is empty.
#[must_use]
pub fn jaccard_ngram(a: &str, b: &str, n: usize) -> f32 {
    let sa = ngrams(a, n);
    let sb = ngrams(b, n);
    jaccard_sets(&sa, &sb)
}

/// Convenience: [`jaccard_ngram`] with the default window size
/// ([`DEFAULT_NGRAM`]).
#[must_use]
pub fn jaccard_6gram(a: &str, b: &str) -> f32 {
    jaccard_ngram(a, b, DEFAULT_NGRAM)
}

/// Jaccard similarity of two pre-computed n-gram sets. Hoisted out
/// so the capture loop can compute the candidate set once and reuse
/// it across every comparison.
///
/// `usize → f32` casts use [`u32_to_f32_lossy`] because the working
/// sets bounded by document length never overflow `u32`, but the cast
/// itself can lose mantissa precision at the high end — accepted: the
/// dedup threshold lives at 0.85, not at 1e6-precision.
#[must_use]
pub fn jaccard_sets<S: std::hash::BuildHasher>(
    a: &HashSet<String, S>,
    b: &HashSet<String, S>,
) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        return 1.0;
    }
    u32_to_f32_lossy(inter) / u32_to_f32_lossy(union)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn u32_to_f32_lossy(n: usize) -> f32 {
    let clamped = u32::try_from(n).unwrap_or(u32::MAX);
    clamped as f32
}

// ---------- Validity as a ranking signal ----------

/// Multiplier applied to a hit's similarity score when its validity
/// window is **closed** at query time (`valid_to` in the past).
///
/// The temporal-validity model's recall half: a closed window — the
/// bought item, the watched film, the past appointment, the retracted
/// project — **down-ranks, never hides** (the deviating fact is often
/// the gold, and a closed fact may be exactly what a dated question
/// asks about). Multiplicative, so ordering *within* the closed set is
/// preserved. An explicitly dated query uses
/// [`fact_index::FactFilters::valid_at`] instead — there, selecting the
/// facts true at the asked date is the point.
pub const CLOSED_WINDOW_DOWNRANK: f32 = 0.8;

/// `true` when `valid_to` is set and not after `now` — the window has
/// closed. A missing or unparseable bound counts as **open** (the
/// conservative side: never down-rank on bad data).
fn window_closed_at(valid_to: Option<&str>, now: &chrono::DateTime<chrono::Utc>) -> bool {
    valid_to
        .and_then(|vt| chrono::DateTime::parse_from_rfc3339(vt).ok())
        .is_some_and(|vt| vt <= *now)
}

// ---------- Cosine ----------

/// Cosine similarity of two embedding vectors, in `[-1.0, 1.0]`.
///
/// Returns `0.0` for empty or mismatched-length inputs (instead of
/// panicking — embedding dim drift across a model upgrade is a real
/// failure mode we want the caller to see as "no signal" rather than
/// a crash). Returns `0.0` when either vector has zero magnitude.
///
/// The function is pure and deterministic; the capture and recall
/// pipelines call it from a hot loop, so it intentionally avoids any
/// allocation.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na2 = 0.0_f32;
    let mut nb2 = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na2 += x * x;
        nb2 += y * y;
    }
    let denom = na2.sqrt() * nb2.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

fn normalise(s: &str) -> String {
    let lowered = s.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_was_space = false;
    for c in lowered.chars() {
        if c.is_control() || c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// ---------- Recall orchestration types ----------

/// Errors raised by the multi-modal recall pipeline.
#[derive(Debug, Error)]
pub enum RecallError {
    /// Underlying fact-index DB error.
    #[error("recall fact_index: {0}")]
    FactIndex(#[from] FactIndexError),

    /// Underlying embedder error.
    #[error("recall embedder: {0}")]
    Embedder(#[from] EmbedderError),

    /// Underlying `SQLite` error (used by the recall-hit bump path
    /// which goes through `sqlx` directly).
    #[error("recall db: {0}")]
    Db(#[from] sqlx::Error),

    /// Underlying capture-buffer error — the mid-range "fresh" slot reads
    /// the un-promoted buffer via [`recall_fresh_captures`].
    #[error("recall capture_buffer: {0}")]
    CaptureBuffer(#[from] CaptureBufferError),
}

/// Result alias for the recall pipeline.
pub type RecallResult<T> = std::result::Result<T, RecallError>;

/// The requesting principal's identity + group membership, used to
/// project ACL visibility onto candidate facts.
///
/// `sender_id` is the bare user id (no `user:` prefix — that prefix is
/// part of the principal wire format, while ACL evaluation compares
/// against the raw id). For "global" / anonymous queries pass an
/// empty `sender_id` and an empty group list; only regions naming the
/// builtin `global` group will pass.
#[derive(Debug, Clone)]
pub struct SenderContext {
    /// Bare user id (`"alice"`, not `"user:alice"`).
    pub sender_id: String,
    /// Group ids the sender belongs to (`"famiglia"`, not
    /// `"group:famiglia"`).
    pub sender_groups: Vec<String>,
}

impl SenderContext {
    /// Convenience constructor for the common "single user, no group"
    /// case (mostly tests + dashboard sessions).
    #[must_use]
    pub fn user(id: impl Into<String>) -> Self {
        Self {
            sender_id: id.into(),
            sender_groups: Vec::new(),
        }
    }

    /// Convenience constructor for the "anonymous / global only" case.
    #[must_use]
    pub const fn anonymous() -> Self {
        Self {
            sender_id: String::new(),
            sender_groups: Vec::new(),
        }
    }
}

/// One row returned by a recall call.
///
/// Mirrors the subset of [`FactIndexRow`] a downstream caller (LLM
/// ingest, dashboard) actually needs to consume; the full embedding
/// is *not* included (it stays in the index for the next call).
#[derive(Debug, Clone)]
pub struct RecallHit {
    /// Fact id.
    pub fact_id: FactId,
    /// Containing wiki.
    pub wiki_id: String,
    /// Source file path, relative to workdir.
    pub source_path: String,
    /// Byte offsets of the region marker (None if pre-offset row).
    pub region_start: Option<i64>,
    /// One past the closing marker.
    pub region_end: Option<i64>,
    /// Region body text (no markers).
    pub text: String,
    /// Owner principal (the fact's SUBJECT).
    pub owner_id: Principal,
    /// Read-extension list (the visibility axis, additive to owner+sender).
    /// Surfaced so the classifier can SEE a recalled fact's current
    /// audience and faithfully reproduce it on a REPLACE-semantics
    /// `acl_change` (and so a supersede can inherit it).
    pub allow_ids: Vec<Principal>,
    /// Optional cross-user attribution.
    pub sender_id: Option<Principal>,
    /// Optional fact type tag.
    pub fact_type: Option<String>,
    /// Wall-clock of creation.
    pub created_at: String,
    /// End of the validity interval (ISO 8601) when known; `None` = OPEN.
    /// Carried so the due-soon slot can render *when* the fact fires.
    pub valid_to: Option<String>,
    /// Score the recall mechanism assigned this row (cosine for
    /// vector ranking, 1.0 for pure SQL queries).
    pub score: f32,
    /// `true` when this hit is an **un-promoted** buffered capture surfaced
    /// by [`recall_fresh_captures`] (the mid-range "fresh" slot), not a
    /// durable `fact_index` fact. Fresh hits carry no published-page region,
    /// so `region_start`/`region_end` are `None`.
    pub fresh: bool,
}

impl RecallHit {
    fn from_row(row: FactIndexRow, score: f32) -> Self {
        Self {
            fact_id: row.fact_id,
            wiki_id: row.wiki_id,
            source_path: row.source_path,
            region_start: row.region_start,
            region_end: row.region_end,
            text: row.text,
            owner_id: row.owner_id,
            allow_ids: row.allow_ids,
            sender_id: row.sender_id,
            fact_type: row.fact_type,
            created_at: row.created_at,
            valid_to: row.valid_to,
            score,
            fresh: false,
        }
    }

    /// Build a hit from an un-promoted buffered capture (the mid-range
    /// "fresh" slot). No published-page region exists yet, so the offsets
    /// are `None` and `source_path` points at the wiki's capture journal.
    fn from_buffered(cap: BufferedCapture, score: f32) -> Self {
        let journal = format!("{}/_captures.md", cap.wiki_id.as_str());
        Self {
            fact_id: cap.capture_id,
            wiki_id: cap.wiki_id.as_str().to_owned(),
            source_path: journal,
            region_start: None,
            region_end: None,
            text: cap.body,
            owner_id: cap.owner,
            allow_ids: cap.allow,
            sender_id: cap.sender,
            fact_type: cap.fact_type,
            created_at: cap.captured_at,
            valid_to: cap.valid_to,
            score,
            fresh: true,
        }
    }
}

// ---------- ACL projection ----------

fn row_visible_to(row: &FactIndexRow, sender: &SenderContext) -> bool {
    let acl = Acl {
        owner: Some(row.owner_id.clone()),
        allow: row.allow_ids.clone(),
    };
    can_read(
        &acl,
        &sender.sender_id,
        &sender.sender_groups,
        row.sender_id.as_ref(),
    )
}

fn buffered_visible_to(cap: &BufferedCapture, sender: &SenderContext) -> bool {
    let acl = Acl {
        owner: Some(cap.owner.clone()),
        allow: cap.allow.clone(),
    };
    can_read(
        &acl,
        &sender.sender_id,
        &sender.sender_groups,
        cap.sender.as_ref(),
    )
}

// ---------- wiki_search ----------

/// `_internal.wiki_search` — vector top-K against active facts.
///
/// Steps:
/// 1. Embed `query` via the supplied [`Embedder`].
/// 2. Fetch candidates via [`fact_index::find_by_filters`] (so
///    structured filters narrow the working set before scoring).
/// 3. Compute cosine vs every candidate's stored embedding.
/// 4. Drop rows the sender cannot read ([`can_read`] post-filter).
/// 5. Sort by score descending, take `top_k`.
/// 6. Bump `last_recall_at` / `recall_count_30d` on every returned id.
///
/// Returns hits in score-descending order. Empty input is valid (a
/// zero-length working set returns an empty result without erroring).
///
/// # Errors
///
/// See [`RecallError`].
pub async fn wiki_search(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    filters: fact_index::FactFilters,
    sender: &SenderContext,
) -> RecallResult<Vec<RecallHit>> {
    search_inner(pool, embedder, query, top_k, filters, sender, true).await
}

/// [`wiki_search`] without step 6 (the recall-counter bump) — for
/// measurement paths (the recall eval harness) whose synthetic queries
/// must not inflate the recency signal of the corpus under test.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn wiki_search_unrecorded(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    filters: fact_index::FactFilters,
    sender: &SenderContext,
) -> RecallResult<Vec<RecallHit>> {
    search_inner(pool, embedder, query, top_k, filters, sender, false).await
}

async fn search_inner(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    filters: fact_index::FactFilters,
    sender: &SenderContext,
    bump: bool,
) -> RecallResult<Vec<RecallHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let q_emb = embedder.embed(query).await?;
    tracing::debug!(
        query_len = query.len(),
        embed_dim = q_emb.len(),
        wiki_id = filters.wiki_id.as_deref(),
        "recall: wiki_search embedded query"
    );
    let candidates = fact_index::find_by_filters(pool, &filters).await?;
    let scored = score_and_filter(&q_emb, candidates, sender, top_k);
    if bump {
        bump_recall_hits_from(pool, &scored).await?;
    }
    tracing::info!(
        wiki_id = filters.wiki_id.as_deref(),
        sender_id = sender.sender_id,
        candidates_after_acl = scored.len(),
        top_k,
        "recall: wiki_search done"
    );
    Ok(scored)
}

fn score_and_filter(
    query_embedding: &[f32],
    candidates: Vec<FactIndexRow>,
    sender: &SenderContext,
    top_k: usize,
) -> Vec<RecallHit> {
    // The down-rank anchors on the engine wall-clock. (A backlog replay
    // re-living turns via `occurred_at` ranks against the present —
    // harmless: at replay time the corpus' closures are themselves only
    // as far along as the replayed turn.)
    let now = chrono::Utc::now();
    let mut scored: Vec<(f32, FactIndexRow)> = candidates
        .into_iter()
        .filter(|row| row_visible_to(row, sender))
        .map(|row| {
            let mut s = cosine_similarity(query_embedding, &row.embedding);
            // Validity as a ranking SIGNAL, never a filter: a closed
            // window down-ranks the hit but can still surface.
            if window_closed_at(row.valid_to.as_deref(), &now) {
                s *= CLOSED_WINDOW_DOWNRANK;
            }
            (s, row)
        })
        .collect();
    // Sort by score descending. NaN treated as "lowest" so it never
    // wins the top-K (a NaN here would mean the embedding had a
    // zero magnitude, which the dot product surfaces as 0.0 / 0.0).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Less));
    scored
        .into_iter()
        .take(top_k)
        .map(|(s, row)| RecallHit::from_row(row, s))
        .collect()
}

async fn bump_recall_hits_from(pool: &SqlitePool, hits: &[RecallHit]) -> RecallResult<()> {
    if hits.is_empty() {
        return Ok(());
    }
    let ids: Vec<FactId> = hits.iter().map(|h| h.fact_id.clone()).collect();
    fact_index::bump_recall_hits(pool, &ids).await?;
    Ok(())
}

// ---------- wiki_facts_for ----------

/// `_internal.wiki_facts_for` — structured SQL query, no embeddings.
///
/// Returns rows the sender can read, in `created_at` descending order
/// (most recent first). Score is always `1.0` because there is no
/// ranking dimension to project — the caller decides ordering by the
/// filter it passes.
///
/// Does NOT bump recall counters: this is for audit / dashboard /
/// list views where treating a list-page render as "the operator
/// recalled this fact" would inflate the recency signal.
///
/// # Errors
///
/// As [`fact_index::find_by_filters`].
pub async fn wiki_facts_for(
    pool: &SqlitePool,
    filters: fact_index::FactFilters,
    sender: &SenderContext,
) -> RecallResult<Vec<RecallHit>> {
    // Consumer-facing list: never reveal (ACL gate always applies).
    let rows = wiki_facts_full_for(pool, &filters, sender, false).await?;
    Ok(rows
        .into_iter()
        .map(|r| RecallHit::from_row(r, 1.0))
        .collect())
}

/// Like [`wiki_facts_for`], but returns the **full** `fact_index` rows.
///
/// ACL filtered, with the same `FactFilters` (including the dashboard's `sort`
/// / `include_inactive`) and the same per-row ACL gate as [`wiki_facts_for`].
/// The dashboard facts browser needs every column (validity, salience, recall
/// counters, provenance, lifecycle), and the slim [`RecallHit`] projection is
/// shared with the recall path, so bloating it would be the wrong trade —
/// this is its own entry point instead.
///
/// When `reveal` is `true` the per-row ACL gate is **skipped** and every
/// matching row is returned — the dashboard's admin-reveal lens. The caller
/// is responsible for authorising the bypass (the dashboard only passes
/// `true` for an admin with the reveal cookie set); never pass `true` from a
/// consumer-facing path.
///
/// # Errors
///
/// As [`fact_index::find_by_filters`].
pub async fn wiki_facts_full_for(
    pool: &SqlitePool,
    filters: &fact_index::FactFilters,
    sender: &SenderContext,
    reveal: bool,
) -> RecallResult<Vec<fact_index::FactIndexRow>> {
    let rows = fact_index::find_by_filters(pool, filters).await?;
    let visible: Vec<fact_index::FactIndexRow> = rows
        .into_iter()
        .filter(|r| reveal || row_visible_to(r, sender))
        .collect();
    tracing::info!(
        wiki_id = filters.wiki_id.as_deref(),
        sender_id = sender.sender_id,
        reveal,
        returned = visible.len(),
        "recall: wiki_facts_full_for done"
    );
    Ok(visible)
}

/// List the **un-promoted buffered captures** for the dashboard facts view —
/// the "fresh / consolidating" slot.
///
/// Sibling of [`wiki_facts_full_for`], but reads `capture_buffer` instead of
/// `fact_index`. A freshly-ingested claim sits in the buffer until the light
/// dream promotes it (≈ one cadence later); the facts table reads only
/// `fact_index`, so without this it lags the agent's own recall — which
/// already sees those captures via [`recall_fresh_captures`]. Surfacing them
/// here keeps the operator view in step with what the consumer can already
/// recall.
///
/// Unlike [`recall_fresh_captures`] this does **no** semantic ranking (it is a
/// list, not a search, so it needs no embedder): it returns every visible
/// buffered capture, ACL-filtered for `sender` and honouring the `wiki_id`,
/// `owner_id`, `fact_type`, `topics_any`, and `created_*` fields of `filters`.
/// The dashboard renders these with a `fresh` flag of its own.
///
/// `reveal` has the same meaning as in [`wiki_facts_full_for`]: when `true`
/// the per-capture ACL gate is skipped (the admin-reveal lens, caller-gated).
///
/// # Errors
///
/// As [`capture_buffer::find_buffered_in_wiki`] / [`capture_buffer::find_all_buffered`].
pub async fn wiki_buffered_full_for(
    pool: &SqlitePool,
    filters: &fact_index::FactFilters,
    sender: &SenderContext,
    reveal: bool,
) -> RecallResult<Vec<BufferedCapture>> {
    let candidates = match filters.wiki_id.as_deref() {
        Some(wiki_id) => capture_buffer::find_buffered_in_wiki(pool, wiki_id).await?,
        None => capture_buffer::find_all_buffered(pool, FRESH_CANDIDATE_CAP).await?,
    };
    let visible: Vec<BufferedCapture> = candidates
        .into_iter()
        .filter(|cap| reveal || buffered_visible_to(cap, sender))
        .filter(|cap| capture_matches_filters(cap, filters))
        .collect();
    tracing::info!(
        wiki_id = filters.wiki_id.as_deref(),
        sender_id = sender.sender_id,
        reveal,
        returned = visible.len(),
        "recall: wiki_buffered_full_for done"
    );
    Ok(visible)
}

/// In-process mirror of the `fact_index` SQL filters for the un-promoted
/// buffer, which has no equivalent query helper. `wiki_id` is already applied
/// at fetch time; the rest are matched here. `valid_at` and `limit` are
/// deliberately ignored — a buffered capture is alive by definition (no closed
/// window yet) and the candidate set is already capped.
fn capture_matches_filters(cap: &BufferedCapture, filters: &fact_index::FactFilters) -> bool {
    if let Some(owner) = filters.owner_id.as_ref()
        && &cap.owner != owner
    {
        return false;
    }
    if let Some(fact_type) = filters.fact_type.as_deref()
        && cap.fact_type.as_deref() != Some(fact_type)
    {
        return false;
    }
    if !filters.topics_any.is_empty()
        && !filters
            .topics_any
            .iter()
            .any(|wanted| cap.topics.iter().any(|t| t == wanted))
    {
        return false;
    }
    if let Some(after) = filters.created_after.as_deref()
        && cap.captured_at.as_str() < after
    {
        return false;
    }
    if let Some(before) = filters.created_before.as_deref()
        && cap.captured_at.as_str() >= before
    {
        return false;
    }
    true
}

// ---------- wiki_recall ----------

/// `_internal.wiki_recall` — hybrid recall used by the LLM ingest.
///
/// Today this is a thin wrapper over [`wiki_search`] — semantic top-K
/// against active facts in the requested scope, ACL filtered, with
/// the recall-counter side effect. The `recent_messages` slice is
/// accepted but ignored at this stage; a later revision will use it to
/// weight the score against very-recent conversational context.
///
/// Kept as a separate entry point so the call site in `wiki_ingest_message`
/// is stable from day one, even while the body grows policy.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn wiki_recall(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    _recent_messages: &[String],
    top_k: usize,
    filters: fact_index::FactFilters,
    sender: &SenderContext,
) -> RecallResult<Vec<RecallHit>> {
    wiki_search(pool, embedder, query, top_k, filters, sender).await
}

// ---------- mid-range bridge: the "fresh" (un-promoted) slot ----------

/// Recall over the **un-promoted capture buffer** — the mid-range bridge.
///
/// [`wiki_search`] only sees promoted facts (`fact_index`). Material a
/// consumer captured but the light dream has not promoted yet lives in
/// `capture_buffer`, invisible to topic recall — the "mid-range gap" (a
/// claim said N turns ago, already out of the recent window but not yet a
/// durable fact). This surfaces those captures in their own "fresh /
/// unconsolidated" slot, ranked semantically against `query` and
/// ACL-filtered for `sender`.
///
/// PROVISIONAL — revisit after the recall-strategy review. It
/// re-embeds each pending capture at recall time; the buffer drains at the
/// light-dream backlog threshold so the candidate set is small, capped at
/// [`FRESH_CANDIDATE_CAP`]. The optimisation (embed once at capture time,
/// store the vector — option C) is the tracked follow-up.
///
/// Does NOT bump recall counters: buffered captures are not `fact_index`
/// rows yet, so there is no recency signal to advance.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn recall_fresh_captures(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    query: &str,
    sender: &SenderContext,
    fresh_top_k: usize,
) -> RecallResult<Vec<RecallHit>> {
    if fresh_top_k == 0 {
        return Ok(Vec::new());
    }
    let candidates = capture_buffer::find_all_buffered(pool, FRESH_CANDIDATE_CAP).await?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let q_emb = embedder.embed(query).await?;
    let mut scored: Vec<(f32, BufferedCapture)> = Vec::new();
    for cap in candidates {
        if !buffered_visible_to(&cap, sender) {
            continue;
        }
        // The fresh slot re-embeds buffered bodies per turn; strip the
        // embed markers like every other similarity surface.
        let emb = embedder
            .embed(&crate::parser::strip_embed_markers(&cap.body))
            .await?;
        let mut s = cosine_similarity(&q_emb, &emb);
        // Same validity down-rank as `score_and_filter`: a buffered
        // capture can already carry a closed window (a same-day closure
        // landed on it, or a dated commitment whose time passed).
        if window_closed_at(cap.valid_to.as_deref(), &chrono::Utc::now()) {
            s *= CLOSED_WINDOW_DOWNRANK;
        }
        scored.push((s, cap));
    }
    // Sort by score descending; NaN sinks to the bottom (mirrors `score_and_filter`).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Less));
    Ok(scored
        .into_iter()
        .take(fresh_top_k)
        .map(|(s, cap)| RecallHit::from_buffered(cap, s))
        .collect())
}

// ---------- the due-soon slot ----------

/// Recall the facts whose validity window is **about to close or fire** —
/// the recall block's due-soon slot (imminent appointments, near deadlines).
///
/// The pull is **time-driven, not query-driven**: closeness to `now` decides
/// (facts with `valid_to` inside `[now, now + horizon]`, most imminent
/// first), independent of what the turn is talking about — that is the whole
/// point of the slot, a dated commitment must surface even when nothing in
/// the conversation resembles it. ACL-filtered like every other slot.
///
/// `now` is supplied by the caller (testability + one clock per turn); the
/// look-ahead `horizon` is an operator setting surfaced with the recall
/// settings panel. The window reads `valid_to` — when a distinct
/// `remind_at` firing timestamp lands (cross-consumer reminder delivery),
/// the slot widens to it.
///
/// Does NOT bump recall counters: the pull is mechanical (every turn inside
/// the horizon would re-surface the same rows), so counting it would
/// inflate the recency signal without any semantic re-use behind it.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn recall_due_soon(
    pool: &SqlitePool,
    sender: &SenderContext,
    now: chrono::DateTime<chrono::Utc>,
    horizon: chrono::Duration,
    top_k: usize,
) -> RecallResult<Vec<RecallHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let fmt = |t: chrono::DateTime<chrono::Utc>| {
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, /* use_z */ true)
    };
    let rows = fact_index::find_due_between(pool, &fmt(now), &fmt(now + horizon), 0).await?;
    let hits: Vec<RecallHit> = rows
        .into_iter()
        .filter(|row| row_visible_to(row, sender))
        .take(top_k)
        .map(|row| RecallHit::from_row(row, 1.0))
        .collect();
    tracing::debug!(
        sender_id = sender.sender_id,
        hits = hits.len(),
        "recall: due-soon slot pulled"
    );
    Ok(hits)
}

// ---------- Multi-hop link resolution ----------

/// Hard cap on the number of hops [`wiki_multi_hop_facts`] will follow.
///
/// Matches the [memory model](../../../docs/concepts/memory-model.md)
/// — prevents pathological wiki graphs from sending the recall pipeline
/// into a long-running scan.
pub const MULTI_HOP_HARD_LIMIT: usize = 10;

/// Walk the wiki link graph and accumulate every active fact reachable
/// in at most `max_hops` hops.
///
/// Each hop is one wiki: hop 0 is the starting wiki itself, hop 1 are
/// the wikis it links to via `[[…]]`, and so on. `max_hops` is clamped
/// to [`MULTI_HOP_HARD_LIMIT`].
///
/// Discovery is breadth-first and visits each wiki at most once. The
/// resulting `RecallHit`s are ACL-filtered against `sender`, score
/// always 1.0 (the hop graph is structural, not similarity-driven).
///
/// Returns the visited wiki ids alongside the accumulated hits so the
/// caller can display the navigation breadcrumb.
///
/// # Errors
///
/// As [`fact_index::find_active_in_wiki`].
pub async fn wiki_multi_hop_facts(
    pool: &SqlitePool,
    start_wiki_id: &str,
    max_hops: usize,
    sender: &SenderContext,
) -> RecallResult<MultiHopOutcome> {
    let cap = max_hops.min(MULTI_HOP_HARD_LIMIT);
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = vec![start_wiki_id.to_owned()];
    let mut visit_order: Vec<String> = Vec::new();
    let mut hits: Vec<RecallHit> = Vec::new();
    let mut hops = 0usize;
    while !frontier.is_empty() && hops <= cap {
        let mut next: Vec<String> = Vec::new();
        for wiki_id in &frontier {
            if !visited.insert(wiki_id.clone()) {
                continue;
            }
            visit_order.push(wiki_id.clone());
            let rows = fact_index::find_active_in_wiki(pool, wiki_id).await?;
            for row in rows {
                // Harvest outgoing wikilinks from the fact body first
                // so the next frontier can grow even when the row is
                // ACL-redacted for `sender`.
                for target in extract_wikilink_wiki_ids(&row.text) {
                    if !visited.contains(&target) {
                        next.push(target);
                    }
                }
                if row_visible_to(&row, sender) {
                    hits.push(RecallHit::from_row(row, 1.0));
                }
            }
        }
        frontier = next;
        hops += 1;
    }
    tracing::info!(
        sender_id = sender.sender_id,
        start_wiki_id,
        visited = visit_order.len(),
        hits = hits.len(),
        max_hops = cap,
        "recall: wiki_multi_hop_facts done"
    );
    Ok(MultiHopOutcome {
        hits,
        visited: visit_order,
    })
}

/// Outcome of [`wiki_multi_hop_facts`].
#[derive(Debug, Clone)]
pub struct MultiHopOutcome {
    /// Accumulated ACL-filtered facts.
    pub hits: Vec<RecallHit>,
    /// Wiki ids visited, in breadth-first order.
    pub visited: Vec<String>,
}

/// One parsed wikilink target, per the canonical link grammar.
///
/// The grammar
/// (recall-pipeline.md §Link grammar):
/// `[[wiki_id]]` is a wiki hop, `[[wiki_id/page-slug]]` a page hop
/// (the slug may itself contain `/` for a nested page), and an
/// optional `|display` alias is presentation only — it is stripped
/// before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Target wiki id (the first `/`-segment of the link body).
    pub wiki_id: String,
    /// Page slug after the first `/` — no `.md` extension, may contain
    /// further `/` separators. `None` for a bare wiki hop.
    pub page: Option<String>,
}

/// Parse every `[[wiki_id]]` / `[[wiki_id/page]]` / `[[target|display]]`
/// out of `body` into structured [`WikiLink`]s.
///
/// Lightweight scan — does not handle escapes or nested brackets,
/// mirroring the Obsidian flavour the rest of the codebase emits via
/// `wiki_link`. The `|display` alias is stripped (resolution never sees
/// it); the first `/` splits wiki id from page slug. Consumed by the
/// recall navigator ([`crate::recall_nav`]) to offer both the linked
/// wiki and — for a page hop — the linked page as candidates.
#[must_use]
pub fn extract_wikilinks(body: &str) -> Vec<WikiLink> {
    let bytes = body.as_bytes();
    let mut out: Vec<WikiLink> = Vec::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find the closing `]]`.
            let body_start = i + 2;
            let mut j = body_start;
            while j + 1 < bytes.len() {
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    break;
                }
                j += 1;
            }
            if j + 1 >= bytes.len() {
                break;
            }
            let inner = &body[body_start..j];
            // Strip `|display` alias — presentation only.
            let head = inner.split('|').next().unwrap_or(inner);
            // First `/` splits wiki id from the page slug.
            let (wiki_id, page) = match head.split_once('/') {
                Some((w, p)) => (w.trim(), {
                    let p = p.trim();
                    (!p.is_empty()).then(|| p.to_owned())
                }),
                None => (head.trim(), None),
            };
            if !wiki_id.is_empty() {
                out.push(WikiLink {
                    wiki_id: wiki_id.to_owned(),
                    page,
                });
            }
            i = j + 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Parse `[[wiki_id]]` / `[[wiki_id/page]]` / `[[wiki_id|display]]`
/// out of `body`, wiki-granular: the page suffix and the `|display`
/// alias are stripped; only the wiki id is returned.
///
/// Thin projection of [`extract_wikilinks`]. Used by
/// [`wiki_multi_hop_facts`] (the hop graph is wiki-level) and by
/// `rem::run_recall` for back-pressure ranking; re-used by the Backlink
/// reciprocity detector in [`crate::rem`] to identify standard-wiki →
/// smart-wiki wikilinks that lack a reciprocal back-link.
#[must_use]
pub fn extract_wikilink_wiki_ids(body: &str) -> Vec<String> {
    extract_wikilinks(body)
        .into_iter()
        .map(|l| l.wiki_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- normalise ----------

    #[test]
    fn normalise_collapses_whitespace_and_case() {
        assert_eq!(normalise("Foo  Bar\n\tBaz"), "foo bar baz");
        assert_eq!(normalise("  leading"), " leading");
        assert_eq!(normalise("trailing  "), "trailing");
    }

    // ---------- ngrams ----------

    #[test]
    fn ngrams_short_input_pads_to_single_gram() {
        let set = ngrams("hi", 6);
        assert_eq!(set.len(), 1);
        let only = set.iter().next().unwrap();
        assert!(only.starts_with("hi"));
        assert_eq!(only.chars().count(), 6);
    }

    #[test]
    fn ngrams_exact_length_returns_one_gram() {
        let set = ngrams("abcdef", 6);
        assert_eq!(set.len(), 1);
        assert!(set.contains("abcdef"));
    }

    #[test]
    fn ngrams_overlapping_windows() {
        // "abcdefg" → "abcdef", "bcdefg"
        let set = ngrams("abcdefg", 6);
        assert_eq!(set.len(), 2);
        assert!(set.contains("abcdef"));
        assert!(set.contains("bcdefg"));
    }

    #[test]
    fn ngrams_dedups_repeated_windows() {
        let set = ngrams("aaaaaaa", 6);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn ngrams_empty_input_returns_empty_set() {
        assert!(ngrams("", 6).is_empty());
    }

    // ---------- jaccard ----------

    #[test]
    fn jaccard_identical_strings_is_one() {
        assert!((jaccard_6gram("manca il latte", "manca il latte") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_disjoint_strings_is_zero() {
        assert!(jaccard_6gram("aaaaaaa", "bbbbbbb").abs() < 1e-6);
    }

    #[test]
    fn jaccard_close_paraphrase_above_threshold() {
        let a = "manca il latte";
        let b = "Manca il latte.";
        let s = jaccard_6gram(a, b);
        assert!(
            s >= DEFAULT_DEDUP_THRESHOLD,
            "expected ≥{DEFAULT_DEDUP_THRESHOLD}, got {s}"
        );
    }

    #[test]
    fn jaccard_different_groceries_below_threshold() {
        // The classic legacy-MWE motivating case: two short list items
        // about *different* groceries must NOT dedup.
        let s = jaccard_6gram("manca il latte", "manca il pane");
        assert!(
            s < DEFAULT_DEDUP_THRESHOLD,
            "expected <{DEFAULT_DEDUP_THRESHOLD}, got {s}"
        );
    }

    #[test]
    fn jaccard_empty_vs_empty_is_one() {
        assert!((jaccard_6gram("", "") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_empty_vs_nonempty_is_zero() {
        assert!(jaccard_6gram("", "anything").abs() < 1e-6);
        assert!(jaccard_6gram("anything", "").abs() < 1e-6);
    }

    #[test]
    fn jaccard_whitespace_variation_does_not_matter() {
        let a = "I love italian pasta";
        let b = "I  love\nitalian\tpasta";
        let s = jaccard_6gram(a, b);
        assert!((s - 1.0).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn jaccard_sets_reusable_for_loop() {
        let needle = ngrams("manca il latte", 6);
        let hay1 = ngrams("Manca il latte.", 6);
        let hay2 = ngrams("compra il pane", 6);
        let s1 = jaccard_sets(&needle, &hay1);
        let s2 = jaccard_sets(&needle, &hay2);
        assert!(s1 > s2, "{s1} should beat {s2}");
    }

    // ---------- cosine ----------

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![0.1, 0.2, 0.3];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_vectors_is_minus_one() {
        let a = vec![1.0, 1.0];
        let b = vec![-1.0, -1.0];
        assert!((cosine_similarity(&a, &b) - -1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        let zero = vec![0.0_f32; 4];
        let v = vec![1.0_f32, 2.0, 3.0, 4.0];
        assert!(cosine_similarity(&zero, &v).abs() < 1e-6);
        assert!(cosine_similarity(&v, &zero).abs() < 1e-6);
    }

    #[test]
    fn cosine_mismatched_dim_returns_zero() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_empty_vectors_returns_zero() {
        let e: Vec<f32> = Vec::new();
        assert!(cosine_similarity(&e, &e).abs() < 1e-6);
    }

    // ---------- ACL projection ----------

    fn sample_row(id_str: &str, owner: &str, sender: Option<&str>, text: &str) -> FactIndexRow {
        FactIndexRow {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id_str).unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/intro.md".to_owned(),
            region_start: Some(0),
            region_end: Some(32),
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            owner_id: owner.parse().unwrap(),
            allow_ids: vec![],
            sender_id: sender.map(|s| s.parse().unwrap()),
            fact_type: None,
            topics: vec![],
            created_at: "2026-05-18T00:00:00Z".to_owned(),
            updated_at: "2026-05-18T00:00:00Z".to_owned(),
            superseded_at: None,
            superseded_by: None,
            successor_fact_id: None,
            deleted_at: None,
            deleted_reason: None,
            last_recall_at: None,
            recall_count_30d: 0,
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            // Inert: re-derived/non-ingest fact — no
            // classifier placement proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        }
    }

    #[test]
    fn row_visible_to_owner_user() {
        let row = sample_row(
            "018f1234-5678-7abc-9def-0123456789ab",
            "user:alice",
            None,
            "x",
        );
        assert!(row_visible_to(&row, &SenderContext::user("alice")));
        assert!(!row_visible_to(&row, &SenderContext::user("bob")));
    }

    #[test]
    fn row_visible_to_cross_user_attribution() {
        // Owner=alice, sender_of_region=bob — bob must be able to
        // read the region he himself authored on alice's wiki
        // (cross-user attribution invariant, see
        // [memory model](../../../docs/concepts/memory-model.md)).
        let row = sample_row(
            "018f1234-5678-7abc-9def-0123456789ab",
            "user:alice",
            Some("user:bob"),
            "x",
        );
        assert!(row_visible_to(&row, &SenderContext::user("bob")));
        assert!(!row_visible_to(&row, &SenderContext::user("charlie")));
    }

    #[test]
    fn row_visible_to_group_member() {
        let mut row = sample_row(
            "018f1234-5678-7abc-9def-0123456789ab",
            "group:famiglia",
            None,
            "x",
        );
        row.allow_ids = vec![];
        let alice = SenderContext {
            sender_id: "alice".into(),
            sender_groups: vec!["famiglia".into()],
        };
        assert!(row_visible_to(&row, &alice));
        let bob = SenderContext::user("bob");
        assert!(!row_visible_to(&row, &bob));
    }

    #[test]
    fn row_visible_to_global_owner_lets_anyone() {
        let row = sample_row("018f1234-5678-7abc-9def-0123456789ab", "global", None, "x");
        assert!(row_visible_to(&row, &SenderContext::anonymous()));
        assert!(row_visible_to(&row, &SenderContext::user("bob")));
    }

    // ---------- recall_fresh_captures (mid-range bridge) ----------

    #[tokio::test]
    async fn recall_fresh_surfaces_unpromoted_captures_acl_scoped() {
        use crate::capture::CaptureRequest;
        use crate::capture_buffer::buffer_capture;
        use crate::types::WikiId;
        use crate::wiki::WikiTree;
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        for slug in ["alice", "bob"] {
            let d = wikis.join(slug);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("_meta.md"),
                format!(
                    "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
                ),
            )
            .unwrap();
            std::fs::write(d.join("index.md"), "# index\n").unwrap();
        }
        let tree = WikiTree::open(dir.path()).expect("tree");

        let mk = |wiki: &str, body: &str, owner: &str| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: owner.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: vec![],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };

        // Two un-promoted captures: one owned by alice, one by bob.
        buffer_capture(
            &tree,
            &pool,
            mk(
                "alice",
                "Alice joined Virgin Active on Via Roma.",
                "user:alice",
            ),
            None,
        )
        .await
        .expect("buffer alice");
        buffer_capture(
            &tree,
            &pool,
            mk("bob", "Bob's private note.", "user:bob"),
            None,
        )
        .await
        .expect("buffer bob");

        let embedder = embedder_default();
        let hits = recall_fresh_captures(
            &pool,
            embedder.as_ref(),
            "where is my gym",
            &SenderContext::user("alice"),
            5,
        )
        .await
        .expect("fresh recall");

        // Alice sees only her own un-promoted capture, flagged fresh; bob's is
        // ACL-filtered out. Exactly the mid-range gap the bridge closes.
        assert_eq!(hits.len(), 1, "alice must see only her buffered capture");
        assert!(hits[0].fresh, "buffered hit must be flagged fresh");
        assert!(hits[0].text.contains("Virgin Active"));
        assert_eq!(hits[0].owner_id, "user:alice".parse::<Principal>().unwrap());
        assert!(
            hits[0].region_start.is_none(),
            "fresh hit has no published-page region"
        );
    }

    // ---------- wiki_buffered_full_for (dashboard fresh slot) ----------

    #[tokio::test]
    async fn wiki_buffered_full_for_lists_unpromoted_acl_scoped_and_filtered() {
        use crate::capture::CaptureRequest;
        use crate::capture_buffer::buffer_capture;
        use crate::types::WikiId;
        use crate::wiki::WikiTree;
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        for slug in ["alice", "bob"] {
            let d = wikis.join(slug);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("_meta.md"),
                format!(
                    "---\nwiki_id: {slug}\nwiki_type: wiki-user\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
                ),
            )
            .unwrap();
            std::fs::write(d.join("index.md"), "# index\n").unwrap();
        }
        let tree = WikiTree::open(dir.path()).expect("tree");

        let mk = |wiki: &str, body: &str, owner: &str, fact_type: Option<&str>| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: owner.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: fact_type.map(str::to_owned),
            topics: vec![],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };

        // alice owns two captures (one `plan`, one `bio`); bob owns one.
        buffer_capture(
            &tree,
            &pool,
            mk(
                "alice",
                "Alice must move a washing machine.",
                "user:alice",
                Some("plan"),
            ),
            None,
        )
        .await
        .expect("buffer alice plan");
        buffer_capture(
            &tree,
            &pool,
            mk(
                "alice",
                "Alice was born in 1982.",
                "user:alice",
                Some("bio"),
            ),
            None,
        )
        .await
        .expect("buffer alice bio");
        buffer_capture(
            &tree,
            &pool,
            mk("bob", "Bob's private note.", "user:bob", None),
            None,
        )
        .await
        .expect("buffer bob");

        let alice = SenderContext::user("alice");
        let alice_owner = "user:alice".parse::<Principal>().unwrap();

        // No filter: alice sees both her buffered captures; bob's is
        // ACL-filtered out — no embedder involved.
        let all = wiki_buffered_full_for(&pool, &fact_index::FactFilters::default(), &alice, false)
            .await
            .expect("buffered list");
        assert_eq!(all.len(), 2, "alice sees only her two buffered captures");
        assert!(
            all.iter().all(|c| c.owner == alice_owner),
            "every capture is owned by alice"
        );

        // The `fact_type` filter applies in-process, just like the promoted path.
        let plans = wiki_buffered_full_for(
            &pool,
            &fact_index::FactFilters {
                fact_type: Some("plan".to_owned()),
                ..Default::default()
            },
            &alice,
            false,
        )
        .await
        .expect("buffered plan list");
        assert_eq!(plans.len(), 1, "only the plan capture matches the filter");
        assert!(plans[0].body.contains("washing machine"));
    }

    // ---------- score_and_filter ----------

    #[test]
    fn score_and_filter_ranks_descending_and_truncates() {
        let query = vec![1.0, 0.0];
        let mut row1 = sample_row("018f1234-5678-7abc-9def-0123456789ab", "global", None, "a");
        row1.embedding = vec![1.0, 0.0]; // cos=1
        let mut row2 = sample_row("018f1234-5678-7abc-9def-0123456789ac", "global", None, "b");
        row2.embedding = vec![0.9, 0.1]; // cos≈0.994
        let mut row3 = sample_row("018f1234-5678-7abc-9def-0123456789ad", "global", None, "c");
        row3.embedding = vec![0.0, 1.0]; // cos=0
        let hits = score_and_filter(
            &query,
            vec![row3, row1.clone(), row2.clone()],
            &SenderContext::anonymous(),
            2,
        );
        assert_eq!(hits.len(), 2);
        // First must be the perfect match, then row2.
        assert_eq!(hits[0].fact_id, row1.fact_id);
        assert_eq!(hits[1].fact_id, row2.fact_id);
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn score_and_filter_applies_acl_post_fetch() {
        let query = vec![1.0, 0.0];
        let mut row_public = sample_row(
            "018f1234-5678-7abc-9def-0123456789ab",
            "global",
            None,
            "public",
        );
        row_public.embedding = vec![1.0, 0.0];
        let mut row_private = sample_row(
            "018f1234-5678-7abc-9def-0123456789ac",
            "user:alice",
            None,
            "private",
        );
        row_private.embedding = vec![1.0, 0.0];
        let bob = SenderContext::user("bob");
        let hits = score_and_filter(&query, vec![row_public.clone(), row_private], &bob, 10);
        assert_eq!(hits.len(), 1, "private row must drop out");
        assert_eq!(hits[0].fact_id, row_public.fact_id);
    }

    #[test]
    fn score_and_filter_empty_input_returns_empty() {
        let q = vec![1.0_f32, 0.0];
        let out = score_and_filter(&q, Vec::new(), &SenderContext::anonymous(), 5);
        assert!(out.is_empty());
    }

    // ---------- async orchestrators ----------

    use crate::embedder::FakeEmbedder;
    use crate::fact_index::{FactFilters, NewFact};
    use sqlx::sqlite::SqlitePoolOptions;

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

    fn embedder_fixed(vec: Vec<f32>) -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding("fake", vec))
    }

    fn embedder_default() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::new("fake", 4))
    }

    fn insert_row(
        pool_setup: &mut Vec<NewFact>,
        id_str: &str,
        wiki: &str,
        owner: &str,
        text: &str,
        embedding: Vec<f32>,
    ) {
        pool_setup.push(NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id_str).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: format!("wikis/{wiki}/intro.md"),
            region_start: Some(0),
            region_end: Some(32),
            text: text.to_owned(),
            embedding,
            owner_id: owner.parse().unwrap(),
            allow_ids: vec![],
            sender_id: None,
            fact_type: None,
            topics: vec![],
            valid_from: None,
            valid_to: None,
            // Inert: re-derived/non-ingest fact — no
            // classifier placement proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        });
    }

    async fn populate(pool: &SqlitePool, rows: Vec<NewFact>) {
        for r in rows {
            fact_index::insert(pool, &r).await.expect("insert");
        }
    }

    // -- find_by_filters --

    #[tokio::test]
    async fn find_by_filters_scopes_by_wiki_id() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "user:alice",
            "alice fact",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "bob",
            "user:bob",
            "bob fact",
            vec![0.2; 4],
        );
        populate(&pool, rows).await;

        let only_alice = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                wiki_id: Some("alice".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(only_alice.len(), 1);
        assert_eq!(only_alice[0].wiki_id, "alice");
    }

    #[tokio::test]
    async fn find_by_filters_combines_owner_and_fact_type() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        let r1 = NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap(),
            wiki_id: "alice".into(),
            source_path: "wikis/alice/intro.md".into(),
            region_start: Some(0),
            region_end: Some(32),
            text: "preference".into(),
            embedding: vec![0.1; 4],
            owner_id: "user:alice".parse().unwrap(),
            allow_ids: vec![],
            sender_id: None,
            fact_type: Some("preference".into()),
            topics: vec![],
            valid_from: None,
            valid_to: None,
            // Inert: re-derived/non-ingest fact — no
            // classifier placement proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        let mut r2 = r1.clone();
        r2.fact_id = FactId::parse("018f1234-5678-7abc-9def-0123456789ac").unwrap();
        r2.fact_type = Some("bio".into());
        rows.push(r1.clone());
        rows.push(r2.clone());
        populate(&pool, rows).await;

        let prefs = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                wiki_id: Some("alice".into()),
                owner_id: Some("user:alice".parse().unwrap()),
                fact_type: Some("preference".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].fact_type.as_deref(), Some("preference"));
    }

    #[tokio::test]
    async fn find_by_filters_topics_any_uses_json_each() {
        let pool = make_pool().await;
        let r1 = NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap(),
            wiki_id: "alice".into(),
            source_path: "wikis/alice/intro.md".into(),
            region_start: Some(0),
            region_end: Some(32),
            text: "x".into(),
            embedding: vec![0.1; 4],
            owner_id: "user:alice".parse().unwrap(),
            allow_ids: vec![],
            sender_id: None,
            fact_type: None,
            topics: vec!["food".into(), "italian".into()],
            valid_from: None,
            valid_to: None,
            // Inert: re-derived/non-ingest fact — no
            // classifier placement proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        let mut r2 = r1.clone();
        r2.fact_id = FactId::parse("018f1234-5678-7abc-9def-0123456789ac").unwrap();
        r2.topics = vec!["sports".into()];
        let mut r3 = r1.clone();
        r3.fact_id = FactId::parse("018f1234-5678-7abc-9def-0123456789ad").unwrap();
        r3.topics = vec![];
        populate(&pool, vec![r1, r2, r3]).await;

        let any_food = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                topics_any: vec!["food".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(any_food.len(), 1);

        let any_food_or_sports = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                topics_any: vec!["food".into(), "sports".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(any_food_or_sports.len(), 2);
    }

    // -- wiki_search --

    #[tokio::test]
    async fn wiki_search_returns_top_k_by_cosine() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "perfect match",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "alice",
            "global",
            "decent match",
            vec![0.9, 0.1, 0.0, 0.0],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ad",
            "alice",
            "global",
            "no match",
            vec![0.0, 0.0, 1.0, 0.0],
        );
        populate(&pool, rows).await;

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_search(
            &pool,
            embedder,
            "perfect match query",
            2,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].fact_id.as_str(),
            "018f1234-5678-7abc-9def-0123456789ab"
        );
        assert!(hits[0].score >= hits[1].score);
    }

    #[tokio::test]
    async fn wiki_search_downranks_a_closed_window_but_never_hides_it() {
        // Validity as a ranking SIGNAL: two equally similar facts — the
        // one whose window closed ranks below the open one, with exactly
        // the multiplicative down-rank, and still surfaces.
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "open item",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "alice",
            "global",
            "closed item",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let closed_id = FactId::parse("018f1234-5678-7abc-9def-0123456789ac").unwrap();
        fact_index::close_validity(&pool, &closed_id, "2026-06-10T00:00:00Z", "completed", None)
            .await
            .unwrap()
            .expect("closed");

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_search(
            &pool,
            embedder,
            "item",
            10,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 2, "down-rank is never a filter");
        assert_eq!(
            hits[0].fact_id.as_str(),
            "018f1234-5678-7abc-9def-0123456789ab",
            "the open fact outranks the closed one"
        );
        assert!((hits[0].score - 1.0).abs() < 1e-5);
        assert!((hits[1].score - CLOSED_WINDOW_DOWNRANK).abs() < 1e-5);
    }

    #[tokio::test]
    async fn wiki_search_leaves_a_future_window_unranked_down() {
        // A valid_to still in the future is an OPEN commitment (an
        // appointment to come) — no down-rank until it passes.
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "future appointment",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        sqlx::query("UPDATE fact_index SET valid_to = '2099-01-01T00:00:00Z'")
            .execute(&pool)
            .await
            .unwrap();

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_search(
            &pool,
            embedder,
            "appointment",
            10,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 1e-5, "no down-rank yet");
    }

    #[tokio::test]
    async fn find_by_filters_valid_at_selects_facts_true_at_the_instant() {
        // The dated-query selector: only facts whose window contains the
        // asked instant survive (this one IS a filter, by design).
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "durable open fact",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "alice",
            "global",
            "window closed on june 10",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ad",
            "alice",
            "global",
            "starts in july",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;
        // Mixed suffix formats on purpose: datetime() must normalize.
        sqlx::query(
            "UPDATE fact_index SET valid_to = '2026-06-10T00:00:00+00:00'
              WHERE fact_id = '018f1234-5678-7abc-9def-0123456789ac'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE fact_index SET valid_from = '2026-07-01T00:00:00Z'
              WHERE fact_id = '018f1234-5678-7abc-9def-0123456789ad'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let at_june_11 = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                valid_at: Some("2026-06-11T12:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            at_june_11.len(),
            1,
            "only the durable fact holds on the 11th"
        );
        assert_eq!(at_june_11[0].text, "durable open fact");

        let at_june_9 = fact_index::find_by_filters(
            &pool,
            &FactFilters {
                valid_at: Some("2026-06-09T12:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            at_june_9.len(),
            2,
            "the june-10 window was still open on the 9th"
        );
    }

    #[tokio::test]
    async fn wiki_search_drops_rows_invisible_to_sender() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "user:alice",
            "private to alice",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "alice",
            "global",
            "public",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_search(
            &pool,
            embedder,
            "query",
            10,
            FactFilters::default(),
            &SenderContext::user("bob"),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].fact_id.as_str(),
            "018f1234-5678-7abc-9def-0123456789ac"
        );
    }

    #[tokio::test]
    async fn wiki_search_bumps_recall_counters_on_returned_rows() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "x",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_search(
            &pool,
            embedder,
            "q",
            5,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        let row = fact_index::find_by_id(&pool, &hits[0].fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.recall_count_30d, 1);
        assert!(row.last_recall_at.is_some());
    }

    #[tokio::test]
    async fn wiki_search_top_k_zero_returns_empty() {
        let pool = make_pool().await;
        let embedder = embedder_default();
        let hits = wiki_search(
            &pool,
            embedder,
            "q",
            0,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert!(hits.is_empty());
    }

    // -- wiki_facts_for --

    #[tokio::test]
    async fn wiki_facts_for_returns_filtered_rows_without_bumping_recall() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "a",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ac",
            "alice",
            "user:alice",
            "b",
            vec![0.2; 4],
        );
        populate(&pool, rows).await;

        let hits = wiki_facts_for(
            &pool,
            FactFilters {
                wiki_id: Some("alice".into()),
                ..Default::default()
            },
            &SenderContext::user("alice"),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| (h.score - 1.0).abs() < 1e-6));
        // Recall counter not bumped — wiki_facts_for is for audit / list
        // views.
        let row = fact_index::find_by_id(&pool, &hits[0].fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.recall_count_30d, 0);
    }

    #[tokio::test]
    async fn wiki_facts_for_applies_acl_filter() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "user:alice",
            "a",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;

        let hits = wiki_facts_for(&pool, FactFilters::default(), &SenderContext::user("bob"))
            .await
            .unwrap();
        assert!(hits.is_empty(), "bob must not see alice's private row");
    }

    // -- wiki_recall --

    #[tokio::test]
    async fn wiki_recall_delegates_to_search_today() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0123456789ab",
            "alice",
            "global",
            "x",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;

        let embedder = embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]);
        let hits = wiki_recall(
            &pool,
            embedder,
            "q",
            &["earlier message".into()],
            5,
            FactFilters::default(),
            &SenderContext::anonymous(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ---------- multi-hop link resolution ----------

    #[test]
    fn extract_wikilink_strips_page_suffix_and_alias() {
        assert_eq!(
            extract_wikilink_wiki_ids("see [[alice]] for more"),
            vec!["alice".to_owned()],
        );
        assert_eq!(
            extract_wikilink_wiki_ids("nested [[alice/lavoro]] reference"),
            vec!["alice".to_owned()],
        );
        assert_eq!(
            extract_wikilink_wiki_ids("aliased [[alice|Alice the Great]]"),
            vec!["alice".to_owned()],
        );
        assert_eq!(
            extract_wikilink_wiki_ids("two [[alice]] then [[bob/intro|B]]"),
            vec!["alice".to_owned(), "bob".to_owned()],
        );
        assert!(extract_wikilink_wiki_ids("no links here").is_empty());
        assert!(extract_wikilink_wiki_ids("[[ ]] only whitespace").is_empty());
    }

    #[test]
    fn extract_wikilinks_returns_page_hops_and_strips_aliases() {
        // Bare wiki hop.
        assert_eq!(
            extract_wikilinks("see [[alice]] for more"),
            vec![WikiLink {
                wiki_id: "alice".to_owned(),
                page: None,
            }],
        );
        // Page hop — slug preserved, no `.md`.
        assert_eq!(
            extract_wikilinks("nested [[alice/lavoro]] reference"),
            vec![WikiLink {
                wiki_id: "alice".to_owned(),
                page: Some("lavoro".to_owned()),
            }],
        );
        // Alias stripped on both forms — resolution never sees the label.
        assert_eq!(
            extract_wikilinks("aliased [[alice|Alice the Great]]"),
            vec![WikiLink {
                wiki_id: "alice".to_owned(),
                page: None,
            }],
        );
        assert_eq!(
            extract_wikilinks("aliased page [[bob/intro|B]]"),
            vec![WikiLink {
                wiki_id: "bob".to_owned(),
                page: Some("intro".to_owned()),
            }],
        );
        // Nested page slug keeps its own `/` separators.
        assert_eq!(
            extract_wikilinks("[[acme/modules/session]]"),
            vec![WikiLink {
                wiki_id: "acme".to_owned(),
                page: Some("modules/session".to_owned()),
            }],
        );
        // A trailing slash is a bare wiki hop, not an empty page.
        assert_eq!(
            extract_wikilinks("[[alice/]]"),
            vec![WikiLink {
                wiki_id: "alice".to_owned(),
                page: None,
            }],
        );
        assert!(extract_wikilinks("no links").is_empty());
        assert!(extract_wikilinks("[[ ]] whitespace only").is_empty());
    }

    #[tokio::test]
    async fn multi_hop_walks_link_graph_until_hard_limit() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        // alice → bob → carol
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000a0001",
            "alice",
            "user:alice",
            "alice fact references [[bob]]",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000b0001",
            "bob",
            "user:alice",
            "bob fact references [[carol]]",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000c0001",
            "carol",
            "user:alice",
            "carol fact is terminal",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;

        let out = wiki_multi_hop_facts(&pool, "alice", 10, &SenderContext::user("alice"))
            .await
            .unwrap();
        assert_eq!(out.visited, vec!["alice", "bob", "carol"]);
        assert_eq!(out.hits.len(), 3);
    }

    #[tokio::test]
    async fn multi_hop_respects_zero_hop_limit() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000a0011",
            "alice",
            "user:alice",
            "alice fact references [[bob]]",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000b0011",
            "bob",
            "user:alice",
            "bob fact",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;
        let out = wiki_multi_hop_facts(&pool, "alice", 0, &SenderContext::user("alice"))
            .await
            .unwrap();
        assert_eq!(out.visited, vec!["alice"]);
        assert_eq!(out.hits.len(), 1);
    }

    #[tokio::test]
    async fn multi_hop_acl_filters_intermediate_facts() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000a0021",
            "alice",
            "user:alice",
            "alice fact mentions [[bob]]",
            vec![0.1; 4],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000b0021",
            "bob",
            "user:bob",
            "private bob fact",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;
        // alice walks the graph: bob's fact is not visible, but the
        // wiki was still visited so the breadcrumb is honest.
        let out = wiki_multi_hop_facts(&pool, "alice", 10, &SenderContext::user("alice"))
            .await
            .unwrap();
        assert_eq!(out.visited, vec!["alice", "bob"]);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].wiki_id, "alice");
    }

    // -- recall_due_soon (the due-soon slot) --

    fn insert_due(
        pool_setup: &mut Vec<NewFact>,
        id_str: &str,
        owner: &str,
        text: &str,
        valid_to: Option<&str>,
    ) {
        let wiki = owner.split(':').next_back().unwrap_or(owner);
        insert_row(pool_setup, id_str, wiki, owner, text, vec![0.1; 4]);
        pool_setup.last_mut().unwrap().valid_to = valid_to.map(str::to_owned);
    }

    #[tokio::test]
    async fn recall_due_soon_pulls_imminent_windows_most_imminent_first() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        // In-horizon, ordered by imminence (now = 2026-06-10, horizon 7d).
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0001",
            "user:alice",
            "dentist thursday",
            Some("2026-06-12T17:00:00Z"),
        );
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0002",
            "user:alice",
            "conference tomorrow",
            Some("2026-06-11T09:00:00Z"),
        );
        // Excluded: already past / beyond the horizon / open horizon /
        // another user's private window.
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0003",
            "user:alice",
            "yesterday's deadline",
            Some("2026-06-09T12:00:00Z"),
        );
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0004",
            "user:alice",
            "next month trip",
            Some("2026-07-10T00:00:00Z"),
        );
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0005",
            "user:alice",
            "lives in lisbon",
            None,
        );
        insert_due(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000d0006",
            "user:bob",
            "bob's private exam",
            Some("2026-06-11T08:00:00Z"),
        );
        populate(&pool, rows).await;

        let now = chrono::DateTime::parse_from_rfc3339("2026-06-10T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let hits = recall_due_soon(
            &pool,
            &SenderContext::user("alice"),
            now,
            chrono::Duration::days(7),
            10,
        )
        .await
        .unwrap();

        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["conference tomorrow", "dentist thursday"],
            "in-horizon only, ACL-filtered, most imminent first"
        );

        // top_k caps after the ACL filter; 0 short-circuits.
        let one = recall_due_soon(
            &pool,
            &SenderContext::user("alice"),
            now,
            chrono::Duration::days(7),
            1,
        )
        .await
        .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].text, "conference tomorrow");
        assert!(
            recall_due_soon(
                &pool,
                &SenderContext::user("alice"),
                now,
                chrono::Duration::days(7),
                0
            )
            .await
            .unwrap()
            .is_empty()
        );
    }
}
