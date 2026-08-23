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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlx::SqlitePool;
use thiserror::Error;

use crate::acl::can_read;
use crate::capture_buffer::{self, BufferedCapture, CaptureBufferError};
use crate::embedder::{Embedder, EmbedderError};
use crate::enrollment::{self, EnrolledUserLite};
use crate::fact_index::{self, FactIndexError, FactIndexRow};
use crate::sections::{self, SectionError};
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

/// How many buffered rows the fresh slot **scans** before the ACL filter runs.
///
/// Wider than [`FRESH_CANDIDATE_CAP`] on purpose. The cap bounds how many
/// captures get *embedded*; this bounds how many get *looked at*, and the two
/// must not be the same number, because the ACL filter runs in Rust
/// ([`can_read`] is the single judge of who may read what, and duplicating it
/// in SQL is how the two drift apart). With one number, a cut of 32 taken
/// before the filter let one busy sender's backlog empty another reader's
/// bridge entirely — they never reached the filter to be rejected, they simply
/// used up the window. The scan is newest-first, so what a wide window buys is
/// the *recent* rows of everyone rather than all of one.
///
/// It still bites in principle, and says so in the log when it does.
const FRESH_SCAN_CAP: i64 = 256;

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
///
/// The one predicate for "is this fact still standing", shared with the
/// ingest reconciliation stage's candidate rendering
/// (`ingest::reconcile_candidate_line`): a horizon still ahead of us is an
/// **open** fact with a deadline, and anything that decides on the field's
/// presence alone gets every dated commitment wrong.
pub(crate) fn window_closed_at(
    valid_to: Option<&str>,
    now: &chrono::DateTime<chrono::Utc>,
) -> bool {
    valid_to
        .and_then(|vt| chrono::DateTime::parse_from_rfc3339(vt).ok())
        .is_some_and(|vt| vt <= *now)
}

// ---------- Subject coverage as a ranking signal ----------

/// Proportional uplift a hit earns for **each** of the turn's subjects it
/// covers beyond the first (planning card 65).
///
/// A question naming two people should be answered by a fact about both,
/// and cosine alone cannot say so: measured on a live corpus, the fact
/// that named both people, the right topic and the right occasion ranked
/// **8th at 0.458**, below a birth date at 0.484. `subject_id`/`allow_ids`
/// decided only *whether* a reader may see a fact, never what it was worth.
///
/// **Multiplicative, and that is the whole design.** The first version of
/// this signal added a flat amount, and the founder's objection killed it:
/// *«le cose vanno bilanciate — se cerco una cosa in comune tra 2 utenti,
/// cercare fatti con entrambi gli utenti ha lo stesso peso dell'argomento
/// di cui si parla»*. Measured, a flat bonus is not a weight at all: dense
/// embeddings compress this corpus' similarities into ~0.10 between the
/// 1st and 10th hit, so a flat 0.10 was **97 % of the entire usable range**
/// — every covering fact overtook every non-covering one whatever the turn
/// was about. That is a veto, not a balance.
///
/// Scaling instead of adding restores the trade: a fact that covers both
/// subjects **and** is on topic gains more than one that covers both and is
/// off topic, and ordering *within* each coverage class is preserved. It is
/// the same reason [`CLOSED_WINDOW_DOWNRANK`] beside it is multiplicative.
///
/// **Set at 0.15 by the founder, 2026-08-03**, over a measurement that
/// argued for 0.20 — the deliberately conservative end of the range, and
/// his call to make. What it buys and what it costs, on the labelled turns:
/// the narrated question's answer goes 8th → **2nd** (0.20 put it 1st), and
/// the turn that actually failed in production goes 17th → **7th** rather
/// than 3rd — so at the then-current cut of 5 it still missed a five-fact
/// block. That is the half this weight could not close on its own, and it
/// closed on 2026-08-05 from the other side: `recall_top_k` is now 10
/// ([`crate::ingest::IngestPolicy::recall_top_k`]), so 7th is inside the
/// block. Everything topical stays in the block at either value; the flat
/// form pushed it out.
///
/// A ranking **signal, never a filter**: nothing becomes unreachable and
/// everything that surfaced before still surfaces.
pub const SUBJECT_COVERAGE_UPLIFT: f32 = 0.15;

/// First-person forms that put the SPEAKER among the turn's subjects.
///
/// Deliberately narrow, and the exclusions are the point. «**mi** ricordi
/// che macchina ha Bob?» uses `mi` as the *addressee* — the answer is
/// about Bob alone — so the unstressed clitics are absent and only the
/// forms that place the speaker IN the question are listed. Admitting them
/// would invite the asker's unrelated facts into questions about somebody
/// else, which is the failure this whole signal exists to avoid.
const FIRST_PERSON: &[&str] = &[
    // Italian: subject pronoun, tonic object, possessives.
    "io", "me", "mio", "mia", "miei", "mie", // English.
    "my", "mine", "myself",
];

/// The one first-person form that must be matched **with its case**.
///
/// English `I` and Italian `i` are the same token once lowercased, and `i`
/// is the Italian masculine plural article — *«quali sono **i** piatti
/// preferiti di bob?»*. Listed among the lowercase forms it put the
/// SPEAKER among the subjects of ordinary Italian turns, which is the one
/// thing [`FIRST_PERSON`]'s exclusions exist to prevent: it armed the
/// two-subject path, ran the per-row mention scan, and up-ranked every fact
/// naming the asker in a question about somebody else.
///
/// The trade is deliberate and one-sided: a lowercase English *«i like
/// tea»* stops putting the speaker in, which costs a ranking nudge on a
/// turn that is already about them, while the article fires on a large
/// share of Italian turns.
const FIRST_PERSON_CASED: &str = "I";

/// Lowercased word tokens — the unit every name match works on, so none
/// ever fires on a substring (`bobby` must not answer for `bob`).
fn word_set(s: &str) -> std::collections::BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Every name a person answers to, lowercased.
fn names_of(user: &EnrolledUserLite) -> Vec<String> {
    let mut v = vec![user.user_id.to_lowercase()];
    v.extend(user.aliases.iter().map(|a| a.to_lowercase()));
    v
}

/// The people a turn is **about**: the speaker when the first person puts
/// them in the question, plus every enrolled person the turn names —
/// **in the order the turn names them**.
///
/// A match against the roster, not a judgement — the enrolled identities
/// are a short known list, so this costs a set lookup and **no model
/// call**. The speaker's own identity is deterministic and free: the
/// engine has it before it reads the turn.
///
/// The order is not cosmetic. This list is **cut** where the identity cards
/// are served ([`IngestPolicy::max_mentioned_cards`], 2), so an alphabetical
/// order — a `BTreeSet`, say — would serve a turn naming three people the two
/// whose ids sort first: *«cosa preparo per Carol e Bob?»* drops whoever loses
/// the alphabet, on every turn, forever. Mention order says which
/// person the question is built around, and where a list is cut the order IS
/// the selection (founder, 2026-08-09).
#[must_use]
pub fn turn_subjects(query: &str, sender_id: &str, roster: &[EnrolledUserLite]) -> Vec<String> {
    let raw: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let lower: Vec<String> = raw.iter().map(|w| w.to_lowercase()).collect();
    let at = |name: &str| lower.iter().position(|w| w == name);
    let mut found: Vec<(usize, String)> = Vec::new();
    let first_person = raw
        .iter()
        .position(|w| *w == FIRST_PERSON_CASED)
        .into_iter()
        .chain(FIRST_PERSON.iter().filter_map(|p| at(p)))
        .min();
    if let Some(pos) = first_person {
        found.push((pos, sender_id.to_lowercase()));
    }
    for u in roster {
        if let Some(pos) = names_of(u).iter().filter_map(|n| at(n)).min() {
            found.push((pos, u.user_id.to_lowercase()));
        }
    }
    // Earliest mention first; a subject named twice, or named and also the
    // speaker, keeps its earliest position and appears once.
    found.sort_by_key(|(pos, _)| *pos);
    let mut out: Vec<String> = Vec::with_capacity(found.len());
    for (_, id) in found {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// The people a fact is **about** — governance and content together,
/// because neither alone is aboutness: `subject`/`allow` say who may READ
/// it, the text and topics say who it NAMES. The measured example needs
/// both at once — the answering fact is owned by one person and names the
/// other only in its topics and its prose.
fn fact_mentions(
    row: &FactIndexRow,
    roster: &[EnrolledUserLite],
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut push = |p: &Principal| {
        if let Principal::User(u) = p {
            out.insert(u.to_lowercase());
        }
    };
    push(&row.subject_id);
    for a in &row.allow_ids {
        push(a);
    }
    if let Some(s) = row.sender_id.as_ref() {
        push(s);
    }
    let mut hay = word_set(&row.text);
    for t in &row.topics {
        hay.extend(word_set(t));
    }
    for u in roster {
        if names_of(u).iter().any(|n| hay.contains(n)) {
            out.insert(u.user_id.to_lowercase());
        }
    }
    out
}

/// The multiplier a row earns against this turn's subjects — `1.0` when it
/// earns nothing.
///
/// `1.0` — and free, because the per-row scan never runs — unless the turn
/// carries **at least two** subjects. That guard is what makes the signal
/// cost nothing on ordinary traffic: measured on 141 real turns, 2 of them
/// name two people, and the other 139 pay one comparison.
fn coverage_multiplier(
    row: &FactIndexRow,
    subjects: &[String],
    roster: &[EnrolledUserLite],
) -> f32 {
    if subjects.len() < 2 {
        return 1.0;
    }
    // Order carries no meaning here — this is the ranking half, and the
    // subject list is roster-bounded, so a scan is a set lookup.
    let covered = fact_mentions(row, roster)
        .iter()
        .filter(|m| subjects.contains(m))
        .count();
    // The count is bounded by the enrolled roster, so the cast is exact.
    #[allow(clippy::cast_precision_loss, reason = "roster-bounded, far below 2^23")]
    let extra = covered.saturating_sub(1) as f32;
    SUBJECT_COVERAGE_UPLIFT.mul_add(extra, 1.0)
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

    /// Underlying capture buffer error — the mid-range "fresh" slot reads
    /// the un-promoted buffer via [`recall_fresh_captures`].
    #[error("recall capture_buffer: {0}")]
    CaptureBuffer(#[from] CaptureBufferError),

    /// Underlying section-index error — the smart-wiki document corpus
    /// read by [`search_sections`].
    #[error("recall wiki_sections: {0}")]
    Sections(#[from] SectionError),
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
    /// The fact's SUBJECT principal.
    pub subject_id: Principal,
    /// Read-extension list (the visibility axis, additive to subject+sender).
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
    /// Start of the validity interval (ISO 8601) when known; `None` =
    /// unknown / open-start. Carried for the same reason as `valid_to`: a
    /// caller that shows a recalled fact to a model must be able to say
    /// whether it is history, in force, or still to come.
    pub valid_from: Option<String>,
    /// End of the validity interval (ISO 8601) when known; `None` = OPEN.
    /// Carried so the due-soon slot can render *when* the fact fires, and so
    /// the ingest prompt can mark a closed window as history rather than
    /// letting the model read a spent fact as the present.
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
    pub(crate) fn from_row(row: FactIndexRow, score: f32) -> Self {
        Self {
            fact_id: row.fact_id,
            wiki_id: row.wiki_id,
            source_path: row.source_path,
            region_start: row.region_start,
            region_end: row.region_end,
            text: row.text,
            subject_id: row.subject_id,
            allow_ids: row.allow_ids,
            sender_id: row.sender_id,
            fact_type: row.fact_type,
            created_at: row.created_at,
            valid_from: row.valid_from,
            valid_to: row.valid_to,
            score,
            fresh: false,
        }
    }

    /// Build a hit from an un-promoted buffered capture (the mid-range
    /// "fresh" slot).
    ///
    /// **No address of any kind**, and both halves are empty strings because
    /// there is genuinely nothing to name: a waiting claim has no page (nothing
    /// is written yet) and no wiki (the buffer names none — where it goes is
    /// settled when the light dream sorts the queue). The navigator never
    /// follows a fresh hit ([`crate::recall_nav`]), which is why there is
    /// nothing to point it at.
    fn from_buffered(cap: BufferedCapture, score: f32) -> Self {
        Self {
            fact_id: cap.capture_id,
            wiki_id: String::new(),
            source_path: String::new(),
            region_start: None,
            region_end: None,
            text: cap.body,
            subject_id: cap.subject,
            allow_ids: cap.allow,
            sender_id: cap.sender,
            fact_type: cap.fact_type,
            created_at: cap.captured_at,
            valid_from: cap.valid_from,
            valid_to: cap.valid_to,
            score,
            fresh: true,
        }
    }
}

// ---------- ACL projection ----------

fn row_visible_to(row: &FactIndexRow, sender: &SenderContext) -> bool {
    let acl = Acl {
        subject: Some(row.subject_id.clone()),
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
        subject: Some(cap.subject.clone()),
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
    mut filters: fact_index::FactFilters,
    sender: &SenderContext,
    bump: bool,
) -> RecallResult<Vec<RecallHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    // The ACL goes into the QUERY, not just the loop below. Scoring is a
    // cosine against every candidate's stored embedding, so a candidate the
    // sender may not read costs a vector read, a transfer and a decode
    // before `score_and_filter` throws it away — on a path that runs once
    // per conversational turn over the whole active corpus. Narrowing in SQL
    // is the same move the section corpus already makes (`search_sections`:
    // ACL before both scans, so an unreadable wiki's bytes never leave the
    // store).
    //
    // `row_visible_to` below is deliberately KEPT. The predicate is a
    // narrowing optimisation and the row check stays the authority: two
    // spellings of one rule drift, and the failure mode of this particular
    // drift is showing somebody something they were never told, so the
    // stricter of the two must always get the last word.
    filters.readable_by = Some(crate::acl::reader_principals(
        &sender.sender_id,
        &sender.sender_groups,
    ));
    let q_emb = embedder.embed(query).await?;
    tracing::debug!(
        query_len = query.len(),
        embed_dim = q_emb.len(),
        wiki_id = filters.wiki_id.as_deref(),
        "recall: wiki_search embedded query"
    );
    // Who the turn is about. One read of a table that holds one row per
    // enrolled person, and the scan it feeds is skipped entirely below
    // unless the turn names two of them.
    let roster: Vec<EnrolledUserLite> = enrollment::list_users(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| !u.is_agent)
        .collect();
    let subjects = turn_subjects(query, &sender.sender_id, &roster);
    // The second key: the clause each `[[wikilink]]` was written inside,
    // standing in for the facts beside it. A question phrased in none of a
    // fact's own words can reach it through the sentence that says why its
    // page points somewhere ([`crate::link_key`]). Never an error — without
    // the keys a fact is found by its own words, which is the behaviour this
    // path had before they existed.
    let link_scores = match crate::link_key::all_embedded(pool).await {
        Ok(keys) => crate::link_key::best_scores(&keys, &q_emb),
        Err(e) => {
            tracing::warn!(error = %e, "recall: link keys unread — own words only");
            HashMap::new()
        },
    };
    let candidates = fact_index::find_by_filters(pool, &filters).await?;
    let scored = score_and_filter(
        &q_emb,
        candidates,
        sender,
        top_k,
        &subjects,
        &roster,
        &link_scores,
    );
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

/// Score, filter and cut the candidate facts.
///
/// `link_scores` is the second key ([`crate::link_key`]), and it enters the
/// score as a **max**, never as a bonus added on top: a fact is worth the
/// better of what its own words earn and what the clause beside it earns.
/// The additive form was measured to cost 5 points of precision for the same
/// reach. The multipliers below then apply to whichever won, because
/// validity and subject coverage are properties of the fact and do not care
/// how it was found.
///
/// **A key can never widen what a reader sees**: the visibility filter runs
/// first, so a key only ever moves a fact the sender was already allowed to
/// read.
fn score_and_filter(
    query_embedding: &[f32],
    candidates: Vec<FactIndexRow>,
    sender: &SenderContext,
    top_k: usize,
    subjects: &[String],
    roster: &[EnrolledUserLite],
    link_scores: &HashMap<String, f32>,
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
            let own = cosine_similarity(query_embedding, &row.embedding);
            let mut s = link_scores
                .get(row.fact_id.as_str())
                .map_or(own, |k| own.max(*k));
            // Validity as a ranking SIGNAL, never a filter: a closed
            // window down-ranks the hit but can still surface.
            if window_closed_at(row.valid_to.as_deref(), &now) {
                s *= CLOSED_WINDOW_DOWNRANK;
            }
            // Subject coverage, the same family of signal and the same
            // shape: a fact about BOTH people a two-person question named
            // is worth proportionally more than one about neither. Scaling
            // rather than adding is what keeps it a weight instead of an
            // override — see the constant.
            s *= coverage_multiplier(&row, subjects, roster);
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

// ---------- the section corpus ----------

/// One hit from the smart-wiki section index (`wiki_sections`).
///
/// Deliberately **not** a [`RecallHit`]: a section is a chunk of a
/// document a smart consumer authored, not a governed fact. It has no
/// subject, no sender, no validity window and no lifecycle, so the ingest
/// verbs that act on facts (supersede, forget, validity edit, ACL edit)
/// have nothing to bite on. Keeping the two types apart is what stops a
/// fact-shaped code path from silently operating on documentation — the
/// failure mode that shipped when both lived in one table.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionHit {
    /// Containing smart wiki.
    pub wiki_id: String,
    /// Workdir-relative page path.
    pub source_path: String,
    /// Position of the section on its page — half of its identity.
    pub section_ord: i64,
    /// Heading chain, when the section sits under one.
    pub heading_path: Option<String>,
    /// The indexed text (heading chain + body).
    pub text: String,
    /// Cosine similarity against the query.
    pub score: f32,
}

impl SectionHit {
    /// Stable `"<source_path>#<section_ord>"` handle.
    #[must_use]
    pub fn handle(&self) -> String {
        format!("{}#{}", self.source_path, self.section_ord)
    }
}

/// A hit from either corpus, for the two consumer surfaces that search
/// **everything** the sender can see (`wiki_search`, `wiki_navigate`).
///
/// Every other caller takes the fact corpus alone and keeps working with
/// [`RecallHit`] — which is the point: a path that never asked for
/// documentation cannot accidentally receive it.
#[derive(Debug, Clone)]
pub enum SearchHit {
    /// A governed fact from a standard wiki.
    Fact(Box<RecallHit>),
    /// A document section from a smart wiki.
    Section(SectionHit),
}

impl SearchHit {
    /// Ranking score, for the merge.
    #[must_use]
    pub fn score(&self) -> f32 {
        match self {
            Self::Fact(h) => h.score,
            Self::Section(s) => s.score,
        }
    }

    /// Containing wiki.
    #[must_use]
    pub fn wiki_id(&self) -> &str {
        match self {
            Self::Fact(h) => &h.wiki_id,
            Self::Section(s) => &s.wiki_id,
        }
    }

    /// The hit's body text.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Fact(h) => &h.text,
            Self::Section(s) => &s.text,
        }
    }

    /// The stable handle a caller echoes back: a `fact_id` for a fact, a
    /// `"<source_path>#<ord>"` for a section.
    #[must_use]
    pub fn handle(&self) -> String {
        match self {
            Self::Fact(h) => h.fact_id.as_str().to_owned(),
            Self::Section(s) => s.handle(),
        }
    }
}

/// The smart wikis `sender` may read, resolved **once per wiki** from the
/// `smart_wikis` registry.
///
/// This is the whole point of moving the ACL off the rows: read access to
/// a smart wiki is one decision about one wiki, not one decision per
/// indexed section. The effective set is `owner ∪ shared_with`, exactly what
/// a per-row check would compute — one decision, same visibility.
async fn readable_smart_wikis(
    pool: &SqlitePool,
    sender: &SenderContext,
) -> RecallResult<Vec<String>> {
    let registry = sections::list_smart_wikis(pool).await?;
    Ok(registry
        .into_iter()
        .filter(|w| {
            let acl = Acl {
                subject: Some(w.owner_id.clone()),
                allow: w.shared_with.clone(),
            };
            can_read(&acl, &sender.sender_id, &sender.sender_groups, None)
        })
        .map(|w| w.wiki_id)
        .collect())
}

// ---------- rank fusion ----------

/// How deep the lexical list is consulted.
///
/// Past this the reciprocal weight is already smaller than the gap
/// between adjacent vector ranks, so a deeper list changes no order and
/// only widens the query. Both corpora are over-fetched relative to the
/// `top_k` any caller asks for.
const LEXICAL_DEPTH: usize = 50;

/// Reciprocal-rank-fusion constant. 60 is the value the original RRF
/// paper measured and the one every implementation since has kept: large
/// enough that rank 1 does not swamp rank 5, small enough that the tail
/// stops mattering.
const RRF_K: f32 = 60.0;

/// The bonus a section gets for being *titled* with the query rather than
/// merely containing it. Any value above `2/RRF_K` (the largest sum two
/// rank-0 placements can produce) makes the tier absolute; `1.0` is that
/// with room to read.
const DEFINITION_TIER: f32 = 1.0;

/// Both lexical signals for one query, in one place: the ranked list every
/// section entry point fuses with, and the set of sections the query
/// *names* (see [`fuse_by_lexical_rank`]). Two index lookups, no
/// embeddings, sub-millisecond on the reference store.
///
/// # Errors
///
/// Bubbles the section-index reads.
async fn lexical_signals(
    pool: &SqlitePool,
    wikis: &[String],
    query: &str,
) -> RecallResult<(Vec<(String, i64)>, HashSet<String>)> {
    let lexical = sections::search_lexical(pool, wikis, query, LEXICAL_DEPTH).await?;
    let defining: HashSet<String> =
        sections::search_lexical_headings(pool, wikis, query, LEXICAL_DEPTH)
            .await?
            .into_iter()
            .map(|(source_path, ord)| format!("{source_path}#{ord}"))
            .collect();
    Ok((lexical, defining))
}

/// Reorder a **cosine-sorted** list by fusing its own ranking with a
/// lexical one — `score` is not touched.
///
/// ## Why fusion, and why it may not touch the score
///
/// The two rankings are not commensurable: a cosine is a distance in
/// `[-1, 1]`, `bm25` is an unbounded corpus-relative weight, and no
/// constant converts one into the other. Reciprocal rank fusion sidesteps
/// that by discarding both magnitudes and keeping only *positions* —
/// `Σ 1/(k + rank)` over the lists an item appears in. An item ranked
/// well by both wins; an item ranked first by one and absent from the
/// other still places high, which is the entire point (an identifier has
/// no semantics to embed, a paraphrase shares no words).
///
/// What it must **not** do is write the fused number into
/// [`SectionHit::score`]. That field is a cosine and three callers read
/// it as one: [`DEFAULT_SIGNPOST_FLOOR`] is a cosine threshold applied to
/// it, [`search_all`] merges the two corpora by comparing it against a
/// *fact's* cosine, and `wiki_search` serializes it to the consumer in
/// the same `score` field a fact hit uses. A fused number would fail all
/// three silently — no error, no failing test, just a gate that never
/// opens again and a ranking that compares two different units. So fusion
/// changes the **order** and nothing else.
///
/// ## The definition tier
///
/// Rank fusion alone cannot separate a section *titled* `D-006` from one
/// that merely *cites* `D-006` in its body, because the citing section is
/// in **both** lists too. Measured on the production corpus after 1.5.4
/// shipped: `D-001` (which quotes the string) held rank 0 by cosine and
/// rank 2 lexically — `1/60 + 1/62` — against the defining section's
/// `1/78 + 1/60`. Neither a smaller `RRF_K` nor a heavier lexical term
/// flips that; both are monotone in a rank gap of two.
///
/// So `defining` — the sections whose *heading* carries every term of the
/// query ([`sections::search_lexical_headings`]) — is a **tier**, not a
/// weight: a bonus larger than any achievable RRF sum, so a definition
/// outranks every citation and definitions keep their own fused order
/// among themselves. On a prose query no heading carries all the terms,
/// the set is empty, and this is inert.
///
/// `items` must already be in cosine order: its index *is* the vector
/// rank. Ties break on that original rank, so the result is deterministic
/// and a lexically-unknown list comes back untouched.
fn fuse_by_lexical_rank<T, F>(
    items: &mut Vec<T>,
    lexical: &[(String, i64)],
    defining: &HashSet<String>,
    handle: F,
) where
    F: Fn(&T) -> String,
{
    if lexical.is_empty() && defining.is_empty() {
        return;
    }
    let lex_rank: std::collections::HashMap<String, usize> = lexical
        .iter()
        .enumerate()
        .map(|(rank, (source_path, ord))| (format!("{source_path}#{ord}"), rank))
        .collect();
    let mut keyed: Vec<(f32, usize, T)> = std::mem::take(items)
        .into_iter()
        .enumerate()
        .map(|(vec_rank, item)| {
            let key = handle(&item);
            let mut rrf = reciprocal(vec_rank);
            if let Some(&lex) = lex_rank.get(&key) {
                rrf += reciprocal(lex);
            }
            if defining.contains(&key) {
                rrf += DEFINITION_TIER;
            }
            (rrf, vec_rank, item)
        })
        .collect();
    keyed.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    items.extend(keyed.into_iter().map(|(_, _, item)| item));
}

/// One list's contribution to an item's fused score: `1 / (k + rank)`.
fn reciprocal(rank: usize) -> f32 {
    #[allow(clippy::cast_precision_loss)] // ranks are bounded by LEXICAL_DEPTH / top_k
    let rank = rank as f32;
    1.0 / (RRF_K + rank)
}

/// Top-K over the **section** corpus — smart-wiki documentation — ranked
/// by vector similarity fused with exact-term search.
///
/// ACL is applied *before* both scans: only the readable wikis' sections
/// are loaded, so an unreadable wiki's bytes never leave the DB. (The
/// per-row predecessor had to read every row before it could discard
/// any.)
///
/// Both passes always run. A gate that decided per query whether the
/// lexical pass is "needed" would spend a judgement to guard a
/// sub-millisecond index lookup, and would drop the hit silently whenever
/// it answered wrongly; the ranking decides instead of a switch.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn search_sections(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    sender: &SenderContext,
) -> RecallResult<Vec<SectionHit>> {
    let readable = readable_smart_wikis(pool, sender).await?;
    search_sections_in(pool, embedder, query, top_k, sender, &readable).await
}

/// [`search_sections`] restricted to an explicit set of smart wikis.
///
/// Split out so a caller that has already decided *which* projects this turn
/// may read — [`search_all`] behind the signpost funnel — pays for those
/// wikis' vectors only. `wikis` is trusted to be ACL-filtered by the caller
/// (both entry points derive it from [`readable_smart_wikis`]); an empty
/// slice short-circuits without touching the store.
///
/// # Errors
///
/// As [`search_sections`].
pub async fn search_sections_in(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    sender: &SenderContext,
    wikis: &[String],
) -> RecallResult<Vec<SectionHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let readable = wikis;
    if readable.is_empty() {
        return Ok(Vec::new());
    }
    let q_emb = embedder.embed(query).await?;
    let candidates = sections::find_candidates_in_wikis(pool, readable).await?;
    let mut scored: Vec<SectionHit> = candidates
        .into_iter()
        .map(|row| SectionHit {
            score: cosine_similarity(&q_emb, &row.embedding),
            wiki_id: row.wiki_id,
            source_path: row.source_path,
            section_ord: row.section_ord,
            heading_path: row.heading_path,
            text: row.text,
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Less)
    });
    let (lexical, defining) = lexical_signals(pool, readable, query).await?;
    fuse_by_lexical_rank(&mut scored, &lexical, &defining, SectionHit::handle);
    scored.truncate(top_k);

    let bumps: Vec<(String, i64)> = scored
        .iter()
        .map(|h| (h.source_path.clone(), h.section_ord))
        .collect();
    sections::bump_recall_hits(pool, &bumps).await?;

    tracing::info!(
        sender_id = sender.sender_id,
        readable_wikis = readable.len(),
        lexical_hits = lexical.len(),
        hits = scored.len(),
        top_k,
        "recall: search_sections done"
    );
    Ok(scored)
}

// ---------- the named-project trigger ----------

/// Split `text` into lowercase alphanumeric tokens.
///
/// Both the message and a wiki's slug go through this, so the two are
/// compared on the same footing: `"mwe-mcp"` and `"mwe mcp"` both become
/// `["mwe", "mcp"]`, and punctuation around a name ("`AcmeSigns`?") falls
/// away.
fn name_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Shortest slug (joined, without separators) that may trigger the
/// project-docs lookup. Guards against a one- or two-letter slug firing
/// on ordinary text.
const MIN_SLUG_MATCH_LEN: usize = 4;

/// Whether `message_tokens` names the wiki whose slug is `slug`.
///
/// The slug is matched as a **contiguous token sequence**, never as a
/// substring and never token-by-token. That is what makes the trigger
/// safe on a compound slug: `cc-pc-lavoro` fires only on the whole
/// "cc pc lavoro", so an ordinary message about *lavoro* does not drag
/// in a project's documentation. Likewise `acmesigns` never fires from a
/// longer word that merely contains it.
fn message_names_wiki(message_tokens: &[String], slug: &str) -> bool {
    let needle = name_tokens(slug);
    if needle.is_empty() || needle.iter().map(String::len).sum::<usize>() < MIN_SLUG_MATCH_LEN {
        return false;
    }
    message_tokens
        .windows(needle.len())
        .any(|window| window == needle.as_slice())
}

/// The readable smart wikis whose **name appears in `message`**.
///
/// The deterministic half of the project-docs trigger: no LLM, no
/// embedding, just a token match against the registry's slugs. Only
/// wikis the sender may read are considered, so the trigger can never
/// reveal that a project exists.
async fn smart_wikis_named_in(
    pool: &SqlitePool,
    message: &str,
    sender: &SenderContext,
) -> RecallResult<Vec<String>> {
    let tokens = name_tokens(message);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let registry = sections::list_smart_wikis(pool).await?;
    Ok(registry
        .into_iter()
        .filter(|w| {
            let acl = Acl {
                subject: Some(w.owner_id.clone()),
                allow: w.shared_with.clone(),
            };
            can_read(&acl, &sender.sender_id, &sender.sender_groups, None)
                && message_names_wiki(&tokens, &w.slug)
        })
        .map(|w| w.wiki_id)
        .collect())
}

/// The readable projects a **signpost surfaced this turn** points at.
///
/// The other half of the project-docs trigger, roadmap group 48. A
/// signpost is a short fact on its subject's reserved page
/// ([`crate::signposts`]) saying that a project exists; when one comes
/// back in the turn's ordinary fact recall, the project it names becomes
/// a candidate — that is how a turn reaches project documentation
/// *without naming the project*.
///
/// Costs one query, and only when a signpost actually surfaced. Read
/// access is re-checked against the smart-wiki registry rather than
/// inferred from the signpost's own ACL: the registry is the authority on
/// who may read a project, and the two are written independently.
async fn projects_signposted_in(
    pool: &SqlitePool,
    surfaced: &[RecallHit],
    sender: &SenderContext,
) -> RecallResult<Vec<String>> {
    let signpost_pages: std::collections::BTreeSet<&str> = surfaced
        .iter()
        .filter(|h| crate::wiki::is_signpost_page(&h.source_path))
        .map(|h| h.source_path.as_str())
        .collect();
    if signpost_pages.is_empty() {
        return Ok(Vec::new());
    }
    let surfaced_ids: std::collections::BTreeSet<&str> =
        surfaced.iter().map(|h| h.fact_id.as_str()).collect();
    let mut named: Vec<String> = Vec::new();
    for page in signpost_pages {
        for row in fact_index::find_active_by_source_path(pool, page).await? {
            if !surfaced_ids.contains(row.fact_id.as_str()) {
                continue;
            }
            if let Some(project) = crate::signposts::project_of(&row)
                && !named.contains(&project)
            {
                named.push(project);
            }
        }
    }
    if named.is_empty() {
        return Ok(Vec::new());
    }
    let registry = sections::list_smart_wikis(pool).await?;
    Ok(registry
        .into_iter()
        .filter(|w| {
            named.contains(&w.wiki_id) && {
                let acl = Acl {
                    subject: Some(w.owner_id.clone()),
                    allow: w.shared_with.clone(),
                };
                can_read(&acl, &sender.sender_id, &sender.sender_groups, None)
            }
        })
        .map(|w| w.wiki_id)
        .collect())
}

/// Similarity floor a section must clear to be pulled in by a
/// **signpost**, as opposed to by an explicit name.
///
/// The two triggers deserve different treatment. Naming a project is an
/// instruction — the turn asked for it, so its documentation is offered
/// whatever the cosine. A signpost is an inference: the memory noticed
/// the project exists and is guessing that this turn is about it. That
/// guess must be able to come back empty.
///
/// ## What the value is, and what it can actually separate
///
/// **Measured**, not chosen: `bge-m3` embeddings of real turns against
/// the real `AcmeSigns` corpus (2 112 sections at the current chunk
/// policy), best cosine per turn:
///
/// The turns below are translated; they were measured as the operator typed
/// them, in the deployment's own language, and the scores are from those runs.
///
/// | turn | best section |
/// |---|---|
/// | «we are eating at eight at my sister's tonight» | 0.427 |
/// | «what did I do for work this week?» | 0.494 |
/// | «tomorrow at 17:00 I have to go to this customer whose display is not working» | 0.608 |
/// | «a customer called to say the content has been stuck for 10 days» | 0.602 |
/// | «how does acmesigns send the content to the displays?» | 0.651 |
///
/// So the floor sits in the empty band between a turn that has nothing to
/// do with the project and a turn in its semantic neighbourhood. That is
/// what it enforces, and it enforces it with room to spare.
///
/// **It does not separate the last three from each other.** The founder's
/// two cases — an appointment that merely mentions a display (must not
/// dig) and a symptom report that never names the project (must dig) —
/// come out at 0.608 and 0.602: indistinguishable, and in the wrong
/// order. A margin-based gate (best section minus best personal fact)
/// was measured too and splits them no better: +0.082 against +0.085.
/// The two sentences *are* the same sentence to an embedding model —
/// customer, display, malfunction. What separates them is the speech act,
/// which similarity does not encode.
///
/// Digging on the appointment turn therefore still happens. The cost is
/// bounded: a labelled reference slot, capped at
/// [`crate::ingest::IngestPolicy::project_docs_char_budget`], that the
/// ingest prompt forbids filing as fact. The distinction the founder
/// wants needs the turn's *intent*, which the classifier knows but only
/// **after** recall has run — a second pass, not a threshold. Left open.
///
/// Corpus- and model-specific: re-measure before trusting this number on
/// a different embedder or a very different corpus. Overridable per
/// deployment through [`crate::ingest::IngestPolicy`].
pub const DEFAULT_SIGNPOST_FLOOR: f32 = 0.55;

/// Similarity a project's **signpost description** must reach before the
/// merged view ([`search_all`]) is allowed to read that project's sections
/// at all.
///
/// ## Why the funnel is the description, and not the sections
///
/// The section corpus is the largest thing in the store — on the reference
/// deployment 4 907 rows and 4.7 MB of text against 1 086 facts and 185 KB,
/// so **96 % of the indexed characters are project documentation**. A
/// merged ranking that reads all of it on every turn spends its budget
/// there, and the founder's contract is the opposite one: *the project
/// corpus stays out of recall unless the turn is explicitly about a
/// project*, with the per-project description ([`crate::signposts`]) as the
/// funnel that says a project exists and what it is about.
///
/// So the decision is taken **against one short authored line per project**
/// — a handful of dot products — instead of against thousands of sections.
/// A project whose description the reader cannot see, or that has none, is
/// not admitted: the only other way in is naming it
/// ([`smart_wikis_named_in`], floor 0), which is an instruction rather than
/// a guess.
///
/// ## Where the number comes from
///
/// Measured on the production corpus (2026-08-01) over 24 probes — 16
/// ordinary personal turns that must open nothing, 8 project turns that
/// must open something:
///
/// | rule | personal turns wrongly opened | project turns caught |
/// |---|---|---|
/// | name only | 0 / 16 | 2 / 8 |
/// | description ≥ 0.35 | **7** / 16 | 7 / 8 |
/// | description ≥ 0.40 | 3 / 16 | 4 / 8 |
/// | **description ≥ 0.45** | **0** / 16 | 3 / 8 |
/// | description ≥ 0.50 | 0 / 16 | 3 / 8 |
///
/// No threshold separates the two groups cleanly — the same shape the
/// project-docs bench found for [`DEFAULT_SIGNPOST_FLOOR`]. Given that,
/// the default is set where the **contract** points: precision first, at
/// `0.45`, the highest-recall value that opens nothing on an ordinary
/// personal turn.
///
/// The project turns it misses are exactly those phrased in a project's
/// *internal* vocabulary (`il player Tizen non parte`, `il testo non
/// scorre da una cornice all'altra`) while the description is written in
/// end-user language. That is a property of the description, not of the
/// threshold: a description that names what the project is about raises
/// those turns above the floor. The funnel is only as wide as the line
/// somebody wrote.
///
/// Corpus- and model-specific; re-measure on a different embedder.
/// Overridable per deployment through
/// [`crate::ingest::IngestPolicy::smart_corpus_floor`].
pub const DEFAULT_SMART_CORPUS_FLOOR: f32 = 0.45;

/// Similarity the turn's **best promoted hit** must clear before the recall
/// block's `RELEVANT MEMORY` slot renders any promoted hit at all.
///
/// Turn-level, never per-hit: measured on the live corpus, the right answer
/// on one turn (0.4813) and the noise on another (0.4306) sit in the same
/// band, so no per-hit threshold separates them, while their *best* hits
/// (0.5474 vs 0.4306) do.
///
/// ## Ships OFF — `0.0` — and why
///
/// The mechanism is built and tested; the **number is not earned**. Its only
/// labelled failure was a turn (`«il volume»`) that was an incomplete
/// utterance and should never have reached recall at all — the founder's
/// point, 2026-08-01: *«l'esempio dove io dico soltanto "il volume" è proprio
/// quello che non deve essere cercato perché non ha senso»*. Calibrating a
/// downstream relevance gate on damage caused by an upstream one that let an
/// unsearchable turn through is the wrong instrument on the wrong evidence.
///
/// So this defaults to **disabled**. The upstream fix is the classifier's
/// `skip` rule (planning card 61 §16); once that lands, a floor gets a number
/// only if a *complete* turn is measured doing harm — from the gold set
/// growing on confirmed misses, not from a sweep of unlabelled turns.
/// `recall.relevance_floor` on the operator panel turns it on; the value the
/// distribution suggested, if one is ever wanted, was `0.45`.
pub const DEFAULT_RELEVANCE_FLOOR: f32 = 0.0;

/// How much of the project-docs slot one pass may spend.
///
/// The slot is shared by the two entry points and consumed in order, so
/// the second pass is handed what the first left rather than its own
/// fresh allowance.
#[derive(Debug, Clone, Copy)]
pub struct SlotBudget {
    /// Maximum sections this pass may keep.
    pub top_k: usize,
    /// Maximum characters of body text this pass may gather.
    pub char_budget: usize,
}

impl SlotBudget {
    /// The slot as configured, before anything has been spent.
    #[must_use]
    pub const fn new(top_k: usize, char_budget: usize) -> Self {
        Self { top_k, char_budget }
    }

    /// What is left after `hits` were already admitted.
    #[must_use]
    pub fn remaining(self, hits: &[SectionHit]) -> Self {
        let spent: usize = hits.iter().map(|h| h.text.len()).sum();
        Self {
            top_k: self.top_k.saturating_sub(hits.len()),
            char_budget: self.char_budget.saturating_sub(spent),
        }
    }
}

/// Rank the sections of `wikis` against `message` and keep what fits.
///
/// The shared core of the two project-docs entry points. `floor` drops
/// hits below a cosine; the budget admits **whole** sections only, and
/// always admits the first one — an empty slot is worse than one hit
/// that overruns, and the index-time section cap
/// ([`crate::document::SECTION_MAX_CHARS`]) is what keeps that first hit
/// from starving the others.
///
/// ## The floor is applied before the fusion, on purpose
///
/// Order here decides whether the signpost path can regress. The lexical
/// pass matches on `OR`, so on an ordinary sentence *something* in the
/// corpus shares a word with it — "has a lexical match" is not evidence,
/// only a high lexical rank is. Were the floor waived for lexically
/// ranked sections, a turn like «stasera ceniamo da mia sorella» (best
/// cosine 0.427, floor 0.55, digs nothing today) would start dragging in
/// documentation because one page happens to contain "sorella". So the
/// floor keeps deciding **whether** to dig, exactly as measured, and the
/// fusion only decides **what surfaces** among the sections that already
/// cleared it.
///
/// Which is also why the founder's case works: a turn that *names* the
/// project comes through [`recall_named_project_docs`] with `floor = 0.0`,
/// so an identifier the embedding cannot represent — `D-006` — is ranked
/// by the pass that can read it literally.
async fn rank_project_sections(
    pool: &SqlitePool,
    embedder: &Arc<dyn Embedder>,
    message: &str,
    wikis: &[String],
    budget: SlotBudget,
    floor: f32,
) -> RecallResult<Vec<SectionHit>> {
    let (top_k, char_budget) = (budget.top_k, budget.char_budget);
    let q_emb = embedder.embed(message).await?;
    let candidates = sections::find_candidates_in_wikis(pool, wikis).await?;
    let mut scored: Vec<SectionHit> = candidates
        .into_iter()
        .map(|row| SectionHit {
            score: cosine_similarity(&q_emb, &row.embedding),
            wiki_id: row.wiki_id,
            source_path: row.source_path,
            section_ord: row.section_ord,
            heading_path: row.heading_path,
            text: row.text,
        })
        .filter(|hit| hit.score >= floor)
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Less)
    });
    let (lexical, defining) = lexical_signals(pool, wikis, message).await?;
    fuse_by_lexical_rank(&mut scored, &lexical, &defining, SectionHit::handle);

    let mut kept: Vec<SectionHit> = Vec::with_capacity(top_k);
    let mut spent = 0_usize;
    for hit in scored {
        if kept.len() == top_k {
            break;
        }
        if spent + hit.text.len() > char_budget && !kept.is_empty() {
            break;
        }
        spent += hit.text.len();
        kept.push(hit);
    }

    let bumps: Vec<(String, i64)> = kept
        .iter()
        .map(|h| (h.source_path.clone(), h.section_ord))
        .collect();
    sections::bump_recall_hits(pool, &bumps).await?;
    Ok(kept)
}

/// Documentation from the projects the message **named** — the first of
/// the two project-docs entry points, and the one that needs no
/// judgement.
///
/// Naming a project is an instruction: the turn declared its own scope,
/// so its sections are ranked and offered whatever the cosine. This runs
/// **before** the classifier, so the docs are in front of it when it
/// decides the turn's intent ("the user is asking about the project" is a
/// `recall`, not a `capture`).
///
/// A message that names nothing pays nothing — not even a query
/// embedding, because the name match runs first.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn recall_named_project_docs(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    message: &str,
    budget: SlotBudget,
    sender: &SenderContext,
) -> RecallResult<Vec<SectionHit>> {
    if budget.top_k == 0 || budget.char_budget == 0 {
        return Ok(Vec::new());
    }
    let named = smart_wikis_named_in(pool, message, sender).await?;
    if named.is_empty() {
        return Ok(Vec::new());
    }
    // Floor 0: the turn named the project, so its docs are offered
    // whatever the cosine.
    let kept = rank_project_sections(pool, &embedder, message, &named, budget, 0.0).await?;
    tracing::info!(
        sender_id = sender.sender_id,
        named_wikis = ?named,
        hits = kept.len(),
        "recall: project docs pulled by name"
    );
    Ok(kept)
}

/// Documentation from a project a **signpost** pointed at — the second
/// entry point, and the one that is a *guess*.
///
/// Runs **after** the classifier, gated by the judgement it returned
/// (`needs_project_docs`), because the decision is "would reading the
/// docs help answer this?" and that is a judgement, not a distance. It
/// was measured as a distance first, on a 17-sentence bench against the
/// production corpus, and no similarity signal separated the two cases
/// the founder cared about — an appointment that merely mentions a
/// screen scored *above* a malfunction report that needed the docs
/// (0.608 vs 0.602 on the raw turn; distilling the claim first made it
/// worse, 0.622 vs 0.586). Two sentences about a client and a screen sit
/// at the same distance from the corpus whether they concern a payment
/// or a fault. So the model decides, per
/// `[[feedback-no-hardcoded-gates-llm-decides]]`, and it costs nothing:
/// the classifier already runs and already returns JSON.
///
/// `floor` stays as a cheap backstop under that judgement — an unrelated
/// turn scores far below it — not as the discriminator.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn recall_signposted_project_docs(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    message: &str,
    surfaced: &[RecallHit],
    exclude_wikis: &[String],
    budget: SlotBudget,
    floor: f32,
    sender: &SenderContext,
) -> RecallResult<Vec<SectionHit>> {
    if budget.top_k == 0 || budget.char_budget == 0 {
        return Ok(Vec::new());
    }
    let signposted: Vec<String> = projects_signposted_in(pool, surfaced, sender)
        .await?
        .into_iter()
        .filter(|w| !exclude_wikis.iter().any(|e| e == w))
        .collect();
    if signposted.is_empty() {
        return Ok(Vec::new());
    }
    let kept = rank_project_sections(pool, &embedder, message, &signposted, budget, floor).await?;
    tracing::info!(
        sender_id = sender.sender_id,
        signposted_wikis = ?signposted,
        floor,
        hits = kept.len(),
        "recall: project docs pulled by signpost"
    );
    Ok(kept)
}

/// The smart wikis this query is allowed to reach, behind the signpost
/// funnel — the whole-corpus counterpart of the ingest turn's project-docs
/// slot.
///
/// Two ways in, and nothing else:
///
/// 1. **The turn names the project** — same contiguous-token slug rule as
///    [`smart_wikis_named_in`], no floor. The turn declared its own scope.
/// 2. **The project's signpost description clears `floor`** — one cosine
///    against one short authored line per project (see
///    [`DEFAULT_SMART_CORPUS_FLOOR`] for the measurement behind the number).
///
/// A project with **no description the reader can see is not admitted**, so
/// this fails closed: an unwritten signpost keeps its project out of the
/// merged view rather than letting it in unexamined. That is deliberate —
/// it is what makes the description load-bearing rather than decorative —
/// and naming the project still reaches it.
///
/// The description is an ordinary `fact_index` row on the subject's reserved
/// `@projects.md`, so its **stored** embedding is reused (no per-turn
/// re-embed of the funnel) and its visibility is the ordinary per-fragment
/// ACL: a reader who cannot see a project's signpost cannot open its docs,
/// and cannot learn the project exists.
///
/// # Errors
///
/// Surfaces registry, `fact_index` and embedder failures.
pub async fn admitted_smart_wikis(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    floor: f32,
    sender: &SenderContext,
) -> RecallResult<Vec<String>> {
    let readable = readable_smart_wikis(pool, sender).await?;
    if readable.is_empty() {
        return Ok(Vec::new());
    }
    // `floor <= 0` means the funnel is OFF, not "a threshold nothing can
    // fail": an operator who zeroes it is asking for the pre-funnel
    // behaviour, and a project with no description would otherwise stay shut
    // at any floor — a surprising way for a disable switch to behave.
    if floor <= 0.0 {
        return Ok(readable);
    }
    let named = smart_wikis_named_in(pool, query, sender).await?;

    // One query for every description signpost, then the per-fragment ACL.
    let signposts = fact_index::find_by_filters(
        pool,
        &fact_index::FactFilters {
            topics_any: vec![crate::signposts::TOPIC_DESCRIPTION.to_owned()],
            ..Default::default()
        },
    )
    .await?;
    let visible: Vec<&fact_index::FactIndexRow> = signposts
        .iter()
        .filter(|row| row_visible_to(row, sender))
        .collect();

    let mut admitted = named;
    if !visible.is_empty() {
        let q_emb = embedder.embed(query).await?;
        for row in visible {
            let Some(project) = crate::signposts::project_of(row) else {
                continue;
            };
            if admitted.contains(&project) || !readable.contains(&project) {
                continue;
            }
            if cosine_similarity(&q_emb, &row.embedding) >= floor {
                admitted.push(project);
            }
        }
    }
    admitted.retain(|w| readable.contains(w));
    tracing::info!(
        sender_id = sender.sender_id,
        readable = readable.len(),
        admitted = admitted.len(),
        floor,
        "recall: smart-corpus funnel"
    );
    Ok(admitted)
}

/// Top-K over **both** corpora, merged into one ranking.
///
/// For the consumer surfaces whose contract is "search everything I can
/// see". The fact corpus is always read; the section corpus is read only
/// for the projects [`admitted_smart_wikis`] lets through, so an ordinary
/// personal turn merges one corpus and pays for one.
///
/// Both halves are over-fetched to `top_k` and the union re-ranked by
/// cosine, so the merge cannot lose a hit that would have made the combined
/// top-K. On top of that order only the **definition tier** is applied —
/// the sections whose *heading chain* carries every term of the query, so
/// that a user typing an identifier still gets the page that defines it
/// rather than one that quotes it. The ranking half of the lexical signal
/// stays inside [`search_sections_in`], where both candidates can be in it;
/// see the comment at the call site for what happens when it does not.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn search_all(
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    filters: fact_index::FactFilters,
    smart_floor: f32,
    sender: &SenderContext,
) -> RecallResult<Vec<SearchHit>> {
    let facts = wiki_search(pool, Arc::clone(&embedder), query, top_k, filters, sender).await?;
    // The funnel decides which projects this turn may read *before* their
    // vectors are loaded, so an ordinary personal turn never scans the
    // documentation corpus at all.
    let admitted =
        admitted_smart_wikis(pool, Arc::clone(&embedder), query, smart_floor, sender).await?;
    let secs = search_sections_in(pool, embedder, query, top_k, sender, &admitted).await?;

    let mut merged: Vec<SearchHit> = facts
        .into_iter()
        .map(|h| SearchHit::Fact(Box::new(h)))
        .chain(secs.into_iter().map(SearchHit::Section))
        .collect();
    merged.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Less)
    });
    // ONLY the definition tier crosses the corpus boundary. The ranking
    // half of the lexical signal (`search_lexical`, OR over every term) is
    // deliberately withheld here, because a fact's handle is a `fact_id`
    // and can never appear in a list keyed by `source_path#ord` — so on the
    // merged list that bonus is reachable by one corpus only. Its magnitude
    // is larger than the entire span of the vector-rank term (`1/(60+r)`
    // over a list of at most `2·top_k`), so for every realistic `top_k` a
    // section sharing *any* token with the query outranked **every** fact
    // whatever the cosines were: measured on the production corpus, 11 of
    // 14 probe queries returned sections at ranks the score order put last,
    // one of them at cosine 0.25 above a fact at 0.63.
    //
    // `defining` (`search_lexical_headings`, AND over the heading chain) is
    // the signal that actually means "the query NAMES this section", which
    // is the guarantee the tier was built for; it is empty on prose (13 of
    // those same 14 probes) so it cannot re-open the same hole.
    let (_, defining) = lexical_signals(pool, &admitted, query).await?;
    fuse_by_lexical_rank(&mut merged, &[], &defining, SearchHit::handle);
    merged.truncate(top_k);
    Ok(merged)
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
/// buffered capture, ACL-filtered for `sender` and honouring the `subject_id`,
/// `fact_type`, `topics_any`, and `created_*` fields of `filters`.
///
/// A `wiki_id` filter is **ignored here, and cannot be otherwise**: a buffered
/// capture is in no wiki (the buffer names no destination — see
/// [`capture_buffer::BufferedCapture`]), so there is nothing to match it
/// against. The consolidating list is the same list on every wiki's page.
/// The dashboard renders these with a `fresh` flag of its own.
///
/// `reveal` has the same meaning as in [`wiki_facts_full_for`]: when `true`
/// the per-capture ACL gate is skipped (the admin-reveal lens, caller-gated).
///
/// # Errors
///
/// As [`capture_buffer::find_recent_buffered`].
pub async fn wiki_buffered_full_for(
    pool: &SqlitePool,
    filters: &fact_index::FactFilters,
    sender: &SenderContext,
    reveal: bool,
) -> RecallResult<Vec<BufferedCapture>> {
    // Newest first: this is the "consolidating" list, and if it is cut the
    // operator wants what just landed, not the oldest stuck rows.
    let candidates = capture_buffer::find_recent_buffered(pool, FRESH_SCAN_CAP).await?;
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
    if let Some(subject) = filters.subject_id.as_ref()
        && &cap.subject != subject
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
/// `already_in_context` is the set of [`capture_buffer::origin_fingerprint`]s
/// of the messages the consumer is **already being shown this turn** — its own
/// replayed transcript, plus the cross-consumer recent window this turn serves
/// back. A capture extracted from one of those is skipped: its content is in
/// front of the agent in the message that produced it, and repeating it as an
/// extracted fact is the same information twice inside one recall block.
///
/// The suppression is by ORIGIN, not by age, because age does not decide it.
/// The recent window is bounded by a TTL *and* an entry count *and* a
/// character budget, so on a talkative surface it drops a message long before
/// the TTL — "recent enough to still be shown" is not a function of time.
/// Matching the message a capture came from answers the actual question. It
/// also leaves every capture made by SOMEBODY ELSE untouched, which is the
/// case the slot is really for: another user's claim was never in this
/// sender's window by any route, so no fingerprint of theirs can match it.
///
/// Empty set = suppress nothing, the historical behaviour.
///
/// Does NOT bump recall counters: buffered captures are not `fact_index`
/// rows yet, so there is no recency signal to advance.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn recall_fresh_captures<S: std::hash::BuildHasher + Sync>(
    pool: &SqlitePool,
    embedder: &dyn Embedder,
    query: &str,
    sender: &SenderContext,
    fresh_top_k: usize,
    already_in_context: &HashSet<String, S>,
) -> RecallResult<Vec<RecallHit>> {
    if fresh_top_k == 0 {
        return Ok(Vec::new());
    }
    // Newest first, and a scan window wider than the embed cap — this slot's
    // whole job is the claim just made and not yet on a page. See
    // [`FRESH_SCAN_CAP`] for why the two numbers differ.
    let candidates = capture_buffer::find_recent_buffered(pool, FRESH_SCAN_CAP).await?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    if i64::try_from(candidates.len()).unwrap_or(i64::MAX) >= FRESH_SCAN_CAP {
        tracing::warn!(
            scan_cap = FRESH_SCAN_CAP,
            "recall: fresh-capture scan window is full — buffered captures older than the \
             newest {FRESH_SCAN_CAP} are not offered this turn"
        );
    }
    let q_emb = embedder.embed(query).await?;
    let mut scored: Vec<(f32, BufferedCapture)> = Vec::new();
    let mut suppressed = 0_usize;
    for cap in candidates {
        if scored.len() >= usize::try_from(FRESH_CANDIDATE_CAP).unwrap_or(usize::MAX) {
            break;
        }
        if !buffered_visible_to(&cap, sender) {
            continue;
        }
        if cap
            .origin_message_hash
            .as_deref()
            .is_some_and(|h| already_in_context.contains(h))
        {
            suppressed += 1;
            continue;
        }
        // The staged vector, computed once when the claim was buffered over
        // this same marker-stripped text. The fallback embeds a row that has
        // none — journal-recovered, or staged while the embedder was down —
        // and it is a fallback precisely because embedding every candidate on
        // every turn is the cost this staging exists to avoid.
        let emb = match cap.embedding.clone() {
            Some(v) => v,
            None => {
                embedder
                    .embed(&crate::parser::strip_embed_markers(&cap.body))
                    .await?
            },
        };
        let mut s = cosine_similarity(&q_emb, &emb);
        // Same validity down-rank as `score_and_filter`: a buffered
        // capture can already carry a closed window (a same-day closure
        // landed on it, or a dated commitment whose time passed).
        if window_closed_at(cap.valid_to.as_deref(), &chrono::Utc::now()) {
            s *= CLOSED_WINDOW_DOWNRANK;
        }
        scored.push((s, cap));
    }
    if suppressed > 0 {
        tracing::debug!(
            sender_id = sender.sender_id,
            suppressed,
            "recall: fresh captures already in the turn's context, not repeated"
        );
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
/// settings panel. The window reads `valid_to`, and that stays the only
/// stored firing time: a distinct `remind_at` column was considered for
/// cross-consumer reminder delivery and **declined**, because 87 % of dated
/// facts store a `valid_to` on a day boundary (a date, no hour), so a second
/// column would have been empty for exactly the commitments that need to
/// fire. An hour, when nobody stated one, is a delivery-side policy — not a
/// datum to store per fact.
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

/// Every active fact living on the pages a turn actually opened.
///
/// The third leg of the reconciliation stage's candidate set, and the reason
/// that stage sits after the navigator rather than beside the classifier: a
/// slot may reconcile against a set it is shown COMPLETE, never against a
/// sample. The flat recall hands the turn a top-K sample of the store, so a
/// judgement about a fact that ALREADY EXISTS ("is this the one the message
/// closes?") is answered out of a window that may simply not contain it. The
/// pages the navigator opened are different in kind: for each one this returns
/// **all** of its readable facts, so within that page nothing is hidden by
/// ranking.
///
/// Cheap by construction, not by luck: `NavigatedFragment` already carries the
/// page it came from and `fact_index` is indexed on `(source_path,
/// region_start)` (`idx_fact_path`), so this is one indexed read per opened
/// page — never a second search.
///
/// ACL-filtered by the same [`row_visible_to`] every other slot uses, so a
/// fact the reader may not see can neither be shown nor closed. Score is
/// always `1.0`: membership of a page is structural, not similarity.
///
/// `cap` is a resource bound on the whole set. When it bites, the pages are
/// consumed in the order given (the navigator's own order, best first) and the
/// truncation is **logged** — a candidate silently dropped here is a fact that
/// quietly cannot be closed.
///
/// # Errors
///
/// See [`RecallError`].
pub async fn facts_on_pages(
    pool: &SqlitePool,
    paths: &[String],
    sender: &SenderContext,
    cap: usize,
) -> RecallResult<Vec<RecallHit>> {
    if paths.is_empty() || cap == 0 {
        return Ok(Vec::new());
    }
    let mut hits: Vec<RecallHit> = Vec::new();
    let mut seen_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for path in paths {
        if !seen_paths.insert(path.as_str()) {
            continue;
        }
        let rows = fact_index::find_active_by_source_path(pool, path).await?;
        hits.extend(
            rows.into_iter()
                .filter(|row| row_visible_to(row, sender))
                .map(|row| RecallHit::from_row(row, 1.0)),
        );
    }
    // NEWEST FIRST, across all the pages together — the cap decides what the
    // stage never sees, so it has to drop the least likely candidate, not the
    // nearest one to hand.
    //
    // The store's query returns `created_at ASC` (a shared query: `comment_apply`
    // and `signposts` rely on that order, which is why the reversal lives here
    // and not in the SQL), and collecting page by page would put the second
    // page's recent facts behind the first page's ancient ones. On a family
    // shopping page a year old, "ho comprato il latte" targets something
    // written days ago: fill the cap oldest-first and the one candidate that
    // would have matched is the one never shown, the model answers honestly
    // that nothing matches, and the item stays open forever.
    //
    // `created_at` is ISO-8601 UTC, so the lexical compare IS the chronological
    // one. Ties keep their relative order (`sort_by` is stable), which leaves
    // the navigator's page order as the tie-break.
    hits.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    if hits.len() > cap {
        tracing::warn!(
            cap,
            total = hits.len(),
            pages = seen_paths.len(),
            "recall: page-scoped candidate set hit its cap — the oldest facts on \
             the opened pages cannot be reconciled this turn"
        );
        hits.truncate(cap);
    }
    tracing::debug!(
        sender_id = sender.sender_id,
        pages = seen_paths.len(),
        hits = hits.len(),
        "recall: facts on the navigated pages pulled"
    );
    Ok(hits)
}

// ---------- The link grammar, and the walk's depth cap ----------

/// Hard cap on how many hops the recall navigator may take in one turn.
///
/// The operator's depth dial ([`crate::recall_nav::NavigatorPolicy::max_hops`])
/// is clamped to it, so a mis-set knob cannot send one turn walking the
/// memory for as long as it has links to follow.
pub const MULTI_HOP_HARD_LIMIT: usize = 10;

/// One parsed wikilink target, per the canonical link grammar.
///
/// `[[wiki_id]]` names a wiki, `[[wiki_id/page-slug]]` a page (the slug may
/// itself contain `/` for a nested page), and an optional `|display` alias is
/// presentation only — it is stripped before resolution.
///
/// Only the second form is a link the engine mints or teaches: a link names a
/// **page**, and a wiki is not one, so `[[wiki_id]]` resolves nowhere. The
/// parser still returns it, because it has to read what a model wrote in order
/// for the callers to reject it.
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
/// Thin projection of [`extract_wikilinks`], for the one caller that asks a
/// wiki-level question: the backlink reciprocity detector in [`crate::rem`],
/// which looks for a standard wiki linking into a smart one without a link
/// coming back. Which page inside the smart wiki carries the link does not
/// change the answer, so the page half is dropped.
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

    fn sample_row(id_str: &str, subject: &str, sender: Option<&str>, text: &str) -> FactIndexRow {
        FactIndexRow {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id_str).unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/intro.md".to_owned(),
            region_start: Some(0),
            region_end: Some(32),
            text: text.to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            subject_id: subject.parse().unwrap(),
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
            salience: None,
            source_ref: None,
        }
    }

    #[test]
    fn row_visible_to_subject_user() {
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
        // subject=alice, sender_of_region=bob — bob must be able to
        // read the region he himself authored on alice's wiki
        // (cross-user attribution invariant, see
        // memory model).
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
    fn row_visible_to_global_subject_lets_anyone() {
        let row = sample_row("018f1234-5678-7abc-9def-0123456789ab", "global", None, "x");
        assert!(row_visible_to(&row, &SenderContext::anonymous()));
        assert!(row_visible_to(&row, &SenderContext::user("bob")));
    }

    // ---------- recall_fresh_captures (mid-range bridge) ----------

    /// The bridge offers the claim just made, not the oldest one still queued.
    ///
    /// The slot exists for the fact captured minutes ago that no page carries
    /// yet. It reused the light dream's **drain** query — oldest first — so
    /// once the buffer held more rows than the cap, the newest capture was the
    /// one thrown away: invisible while the light dream keeps up, wrong exactly
    /// when it is lagging, which is when the buffer matters most.
    #[tokio::test]
    async fn the_fresh_slot_offers_the_newest_captures_not_the_oldest() {
        use crate::capture::CaptureRequest;
        use crate::capture_buffer::buffer_capture;
        use crate::types::WikiId;
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        let d = wikis.join("alice");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(d.join("cucina.md"), "# index\n").unwrap();

        let mk = |body: String| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body,
            subject: "user:alice".parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("episode".into()),
            topics: vec!["varie".into()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        // More pending rows than the embed cap, oldest written first.
        let total = usize::try_from(FRESH_CANDIDATE_CAP).unwrap() + 4;
        for n in 0..total {
            buffer_capture(&pool, mk(format!("nota numero {n}")), None)
                .await
                .expect("buffer");
        }

        let hits = recall_fresh_captures(
            &pool,
            embedder_default().as_ref(),
            "nota",
            &SenderContext::user("alice"),
            usize::try_from(FRESH_CANDIDATE_CAP).unwrap(),
            &HashSet::new(),
        )
        .await
        .expect("fresh");
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        let last = format!("nota numero {}", total - 1);
        assert!(
            texts.iter().any(|t| t.contains(&last)),
            "the most recent capture is offered: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("nota numero 0")),
            "and the oldest is the one the cap drops, not the newest: {texts:?}"
        );
        drop(dir);
    }

    /// The fresh slot must not restate, as an extracted fact, something the
    /// agent is already reading in the message that produced it — and must
    /// keep serving everything else, which in practice means everybody else's
    /// captures, since another person's message was never in this sender's
    /// context by any route.
    ///
    /// The suppression keys on the ORIGIN of the capture, not on its age,
    /// because age cannot answer the question: the window that carries those
    /// messages is bounded by an entry count and a character budget as well as
    /// by a TTL, so "still being shown" is not a function of time.
    #[tokio::test]
    async fn recall_fresh_skips_captures_whose_origin_message_is_already_in_context() {
        use crate::capture::CaptureRequest;
        use crate::capture_buffer::{BufferStaging, buffer_capture_staged, origin_fingerprint};
        use crate::types::WikiId;
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        let d = wikis.join("alice");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(d.join("cucina.md"), "# index\n").unwrap();

        let mk = |body: &str| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: "user:alice".parse::<Principal>().unwrap(),
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

        let shown = "Mi sono iscritta in palestra in Via Roma.";
        let unshown = "Il corso di nuoto comincia a ottobre.";
        for (body, origin) in [
            ("Alice si è iscritta in palestra.", shown),
            ("Il corso di nuoto di Alice comincia a ottobre.", unshown),
        ] {
            buffer_capture_staged(
                &pool,
                mk(body),
                None,
                BufferStaging {
                    embedding: None,
                    origin_message_hash: Some(origin_fingerprint(origin)),
                },
            )
            .await
            .expect("buffer");
        }

        let embedder = embedder_default();
        let mut in_context = HashSet::new();
        in_context.insert(origin_fingerprint(shown));

        let hits = recall_fresh_captures(
            &pool,
            embedder.as_ref(),
            "palestra e nuoto",
            &SenderContext::user("alice"),
            10,
            &in_context,
        )
        .await
        .expect("fresh");
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.contains("palestra")),
            "the agent is already reading the message this came from: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("nuoto")),
            "everything whose origin is NOT in context still surfaces: {texts:?}"
        );

        // With nothing in context — a consumer that carries no window — the
        // slot behaves exactly as it did before the suppression existed.
        let all = recall_fresh_captures(
            &pool,
            embedder.as_ref(),
            "palestra e nuoto",
            &SenderContext::user("alice"),
            10,
            &HashSet::new(),
        )
        .await
        .expect("fresh");
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn recall_fresh_surfaces_unpromoted_captures_acl_scoped() {
        use crate::capture::CaptureRequest;
        use crate::capture_buffer::buffer_capture;
        use crate::types::WikiId;
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
            std::fs::write(d.join("cucina.md"), "# index\n").unwrap();
        }

        let mk = |wiki: &str, body: &str, subject: &str| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: subject.parse::<Principal>().unwrap(),
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
        buffer_capture(&pool, mk("bob", "Bob's private note.", "user:bob"), None)
            .await
            .expect("buffer bob");

        let embedder = embedder_default();
        let hits = recall_fresh_captures(
            &pool,
            embedder.as_ref(),
            "where is my gym",
            &SenderContext::user("alice"),
            5,
            &HashSet::new(),
        )
        .await
        .expect("fresh recall");

        // Alice sees only her own un-promoted capture, flagged fresh; bob's is
        // ACL-filtered out. Exactly the mid-range gap the bridge closes.
        assert_eq!(hits.len(), 1, "alice must see only her buffered capture");
        assert!(hits[0].fresh, "buffered hit must be flagged fresh");
        assert!(hits[0].text.contains("Virgin Active"));
        assert_eq!(
            hits[0].subject_id,
            "user:alice".parse::<Principal>().unwrap()
        );
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
            std::fs::write(d.join("cucina.md"), "# index\n").unwrap();
        }

        let mk = |wiki: &str, body: &str, subject: &str, fact_type: Option<&str>| CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: subject.parse::<Principal>().unwrap(),
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
            &pool,
            mk("bob", "Bob's private note.", "user:bob", None),
            None,
        )
        .await
        .expect("buffer bob");

        let alice = SenderContext::user("alice");
        let alice_subject = "user:alice".parse::<Principal>().unwrap();

        // No filter: alice sees both her buffered captures; bob's is
        // ACL-filtered out — no embedder involved.
        let all = wiki_buffered_full_for(&pool, &fact_index::FactFilters::default(), &alice, false)
            .await
            .expect("buffered list");
        assert_eq!(all.len(), 2, "alice sees only her two buffered captures");
        assert!(
            all.iter().all(|c| c.subject == alice_subject),
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
            &[],
            &[],
            &HashMap::new(),
        );
        assert_eq!(hits.len(), 2);
        // First must be the perfect match, then row2.
        assert_eq!(hits[0].fact_id, row1.fact_id);
        assert_eq!(hits[1].fact_id, row2.fact_id);
        assert!(hits[0].score >= hits[1].score);
    }

    // ---------- subject coverage (card 65) ----------

    fn person(id: &str, aliases: &[&str]) -> EnrolledUserLite {
        EnrolledUserLite {
            user_id: id.to_owned(),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            is_agent: false,
        }
    }

    fn roster() -> Vec<EnrolledUserLite> {
        vec![person("alice", &[]), person("bob", &["bobby"])]
    }

    #[test]
    fn turn_subjects_puts_the_speaker_in_when_the_first_person_does() {
        let s = turn_subjects("what music do bob and I like?", "alice", &roster());
        assert_eq!(
            s,
            vec!["bob", "alice"],
            "in the order the turn names them: {s:?}"
        );
    }

    /// The exclusion that keeps this signal honest: an unstressed clitic
    /// makes the speaker the ADDRESSEE, not a subject, so their unrelated
    /// facts must not be invited into a question about somebody else.
    #[test]
    fn turn_subjects_leaves_the_speaker_out_when_they_are_only_addressed() {
        let s = turn_subjects("mi ricordi che macchina ha bob?", "alice", &roster());
        assert_eq!(s, vec!["bob"], "{s:?}");
    }

    /// Mention order, because this list is cut at two where the cards are
    /// served: an alphabetical order would drop the same person on every
    /// turn, whichever one the question was built around.
    #[test]
    fn turn_subjects_come_back_in_the_order_the_turn_names_them() {
        let roster = vec![person("alice", &[]), person("bob", &[]), person("zoe", &[])];
        assert_eq!(
            turn_subjects("cosa preparo per zoe e bob?", "carla", &roster),
            vec!["zoe", "bob"],
            "the question is built around zoe"
        );
        assert_eq!(
            turn_subjects("cosa preparo per bob e zoe?", "carla", &roster),
            vec!["bob", "zoe"],
            "and the same names in the other order come back in that order"
        );
    }

    /// `i` is the Italian plural article, not the English pronoun.
    ///
    /// Lowercased together they were the same token, so *«quali sono i piatti
    /// preferiti di bob?»* put the asker among the subjects of a question
    /// about somebody else — arming the two-subject path and up-ranking every
    /// fact that names them.
    #[test]
    fn the_italian_article_i_does_not_make_the_asker_a_subject() {
        let roster = vec![person("alice", &[]), person("bob", &[])];
        assert_eq!(
            turn_subjects("quali sono i piatti preferiti di bob?", "alice", &roster),
            vec!["bob"],
            "the article is not the speaker"
        );
        assert_eq!(
            turn_subjects("what do bob and I cook?", "alice", &roster),
            vec!["bob", "alice"],
            "the capitalised English pronoun still does"
        );
    }

    #[test]
    fn turn_subjects_matches_an_alias_but_never_a_substring() {
        let by_alias = turn_subjects("is bobby coming?", "alice", &roster());
        assert_eq!(by_alias, vec!["bob"], "{by_alias:?}");
        let substring = turn_subjects("bobsleigh practice", "alice", &roster());
        assert!(substring.is_empty(), "{substring:?}");
    }

    #[test]
    fn coverage_is_free_below_two_subjects() {
        let mut row = sample_row(
            "018f1234-5678-7abc-9def-00000000c001",
            "user:alice",
            None,
            "bob and alice went out",
        );
        row.topics = vec!["bob".to_owned()];
        let one = vec!["bob".to_owned()];
        assert!((coverage_multiplier(&row, &one, &roster()) - 1.0).abs() < f32::EPSILON);
        assert!((coverage_multiplier(&row, &[], &roster()) - 1.0).abs() < f32::EPSILON);
    }

    /// The measured case in miniature: a fact naming BOTH people of a
    /// two-person question overtakes a closer-scoring one naming a single
    /// person. On the live corpus the gap to close was 0.484 → 0.458.
    #[test]
    fn a_fact_covering_both_subjects_overtakes_a_closer_one_covering_one() {
        let query = vec![1.0, 0.0];
        let mut single = sample_row(
            "018f1234-5678-7abc-9def-00000000c002",
            "global",
            None,
            "bob was born in October",
        );
        single.embedding = vec![0.99, 0.14]; // the better cosine
        let mut both = sample_row(
            "018f1234-5678-7abc-9def-00000000c003",
            "global",
            None,
            "alice is going to the concert with bob",
        );
        both.embedding = vec![0.95, 0.31]; // the worse cosine
        both.topics = vec!["bob".to_owned()];

        let subjects = vec!["alice".to_owned(), "bob".to_owned()];
        let sender = SenderContext {
            sender_id: "alice".to_owned(),
            sender_groups: vec![],
        };
        // Both rows are globally readable, so the ACL decides nothing here
        // and the ordering is the signal under test and nothing else.
        let base = score_and_filter(
            &query,
            vec![single.clone(), both.clone()],
            &sender,
            2,
            &[],
            &roster(),
            &HashMap::new(),
        );
        assert_eq!(
            base[0].fact_id, single.fact_id,
            "without subjects, cosine alone decides"
        );

        let with = score_and_filter(
            &query,
            vec![single.clone(), both.clone()],
            &sender,
            2,
            &subjects,
            &roster(),
            &HashMap::new(),
        );
        assert_eq!(
            with[0].fact_id, both.fact_id,
            "the fact about both must lead"
        );
        assert_eq!(with[1].fact_id, single.fact_id);
    }

    /// §5's invariant: a ranking signal, never a gate. Covering nothing
    /// costs a fact its position, never its presence.
    #[test]
    fn coverage_reorders_and_never_removes() {
        let query = vec![1.0, 0.0];
        let mut uncovered = sample_row(
            "018f1234-5678-7abc-9def-00000000c004",
            "global",
            None,
            "an unrelated note",
        );
        uncovered.embedding = vec![0.2, 0.98];
        let mut covered = sample_row(
            "018f1234-5678-7abc-9def-00000000c005",
            "global",
            None,
            "alice and bob together",
        );
        covered.embedding = vec![0.9, 0.44];
        let subjects = vec!["alice".to_owned(), "bob".to_owned()];
        let out = score_and_filter(
            &query,
            vec![uncovered, covered],
            &SenderContext::anonymous(),
            10,
            &subjects,
            &roster(),
            &HashMap::new(),
        );
        assert_eq!(out.len(), 2, "both still served: {out:?}");
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
        let hits = score_and_filter(
            &query,
            vec![row_public.clone(), row_private],
            &bob,
            10,
            &[],
            &[],
            &HashMap::new(),
        );
        assert_eq!(hits.len(), 1, "private row must drop out");
        assert_eq!(hits[0].fact_id, row_public.fact_id);
    }

    #[test]
    fn score_and_filter_empty_input_returns_empty() {
        let q = vec![1.0_f32, 0.0];
        let out = score_and_filter(
            &q,
            Vec::new(),
            &SenderContext::anonymous(),
            5,
            &[],
            &[],
            &HashMap::new(),
        );
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
        subject: &str,
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
            subject_id: subject.parse().unwrap(),
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
            salience: None,
            source_ref: None,
        });
    }

    /// **A fact is reachable through the sentence beside it**, not only
    /// through its own words.
    ///
    /// The case the second key exists for: a question that shares not one word
    /// with the fact, but shares its subject with the clause the page's link
    /// was written inside. Without the key the fact is last; with it, first.
    /// The lab measured exactly this shape — a coeliac diagnosis moving from
    /// rank 83 to rank 7 on a cooking question saying neither "coeliac" nor
    /// "gluten".
    #[tokio::test]
    async fn a_clause_key_reaches_a_fact_the_question_shares_no_words_with() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        // `pasta` sits on the query's own axis; `celiachia` is orthogonal to
        // it and would never be found by the words of the question.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000aa01",
            "alice",
            "user:alice",
            "alice cooks pasta on sundays",
            vec![1.0, 0.0],
        );
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000bb01",
            "alice",
            "user:alice",
            "alice was diagnosed coeliac in 2019",
            vec![0.0, 1.0],
        );
        populate(&pool, rows).await;

        let query = vec![1.0_f32, 0.0];
        let embedder: Arc<dyn Embedder> = Arc::new(
            crate::embedder::FakeEmbedder::with_fixed_embedding("fake", query.clone()),
        );
        let sender = SenderContext::user("alice");

        let ranked = |hits: &[RecallHit]| -> Vec<String> {
            hits.iter().map(|h| h.fact_id.as_str().to_owned()).collect()
        };
        let before = wiki_search_unrecorded(
            &pool,
            Arc::clone(&embedder),
            "what can she cook",
            2,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await
        .expect("search");
        assert_eq!(
            ranked(&before)[0],
            "018f1234-5678-7abc-9def-00000000aa01",
            "on its own words the coeliac fact is behind: {before:?}"
        );

        // The page's prose links the diet page, and the clause it sits in is
        // about cooking — which is what the question is about too.
        crate::link_key::replace_for_page(
            &pool,
            "wikis/alice/intro.md",
            "alice",
            &[(
                crate::link_key::Clause {
                    target: "alice/cucina".to_owned(),
                    text: "le abitudini e le ricette che ne derivano sono raccolte altrove"
                        .to_owned(),
                    at: 0,
                },
                vec!["018f1234-5678-7abc-9def-00000000bb01".to_owned()],
                Some(query.clone()),
            )],
            "t",
        )
        .await
        .expect("keys");

        let after = wiki_search_unrecorded(
            &pool,
            embedder,
            "what can she cook",
            2,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await
        .expect("search");
        assert_eq!(
            ranked(&after)[0],
            "018f1234-5678-7abc-9def-00000000bb01",
            "the clause beside it carries it to the front: {after:?}"
        );
        assert_eq!(after.len(), 2, "and nothing is dropped: {after:?}");
    }

    /// A key never widens what a reader may see: the visibility filter runs
    /// before scoring, so a key on somebody else's fact moves nothing.
    #[tokio::test]
    async fn a_clause_key_cannot_surface_a_fact_the_reader_may_not_read() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000cc01",
            "bob",
            "user:bob",
            "bob keeps a private diary",
            vec![0.0, 1.0],
        );
        populate(&pool, rows).await;

        let query = vec![1.0_f32, 0.0];
        crate::link_key::replace_for_page(
            &pool,
            "wikis/bob/intro.md",
            "bob",
            &[(
                crate::link_key::Clause {
                    target: "bob/qualcosa".to_owned(),
                    text: "una frase che somiglia moltissimo alla domanda".to_owned(),
                    at: 0,
                },
                vec!["018f1234-5678-7abc-9def-00000000cc01".to_owned()],
                Some(query.clone()),
            )],
            "t",
        )
        .await
        .expect("keys");

        let embedder: Arc<dyn Embedder> = Arc::new(
            crate::embedder::FakeEmbedder::with_fixed_embedding("fake", query),
        );
        let hits = wiki_search_unrecorded(
            &pool,
            embedder,
            "anything",
            5,
            fact_index::FactFilters::default(),
            &SenderContext::user("alice"),
        )
        .await
        .expect("search");
        assert!(
            hits.is_empty(),
            "a perfect key on a fact alice may not read shows her nothing: {hits:?}"
        );
    }

    async fn populate(pool: &SqlitePool, rows: Vec<NewFact>) {
        for r in rows {
            fact_index::insert(pool, &r).await.expect("insert");
        }
    }

    // -- the section corpus --

    async fn seed_smart_wiki(
        pool: &SqlitePool,
        wiki_id: &str,
        owner: &str,
        shared_with: Vec<Principal>,
    ) {
        sections::upsert_smart_wiki(
            pool,
            &sections::SmartWikiRow {
                wiki_id: wiki_id.to_owned(),
                slug: wiki_id.rsplit('-').next().unwrap_or(wiki_id).to_owned(),
                owner_id: owner.parse().unwrap(),
                shared_with,
                project_id: None,
                wiki_type: "project".to_owned(),
                description: None,
            },
        )
        .await
        .expect("register smart wiki");
    }

    /// Like [`seed_section`], with a heading — the column that decides
    /// whether a section *is* the query or merely mentions it.
    async fn seed_section_with_heading(
        pool: &SqlitePool,
        wiki_id: &str,
        ord: i64,
        heading: &str,
        text: &str,
        embedding: Vec<f32>,
    ) {
        let source_path = format!("wikis/{wiki_id}/doc.md");
        let mut desired: Vec<sections::NewSection> =
            sections::find_page_sections(pool, &source_path)
                .await
                .expect("read")
                .into_iter()
                .map(|r| sections::NewSection {
                    wiki_id: r.wiki_id,
                    source_path: r.source_path,
                    section_ord: r.section_ord,
                    heading_path: r.heading_path,
                    text: r.text,
                    embedding: r.embedding,
                })
                .collect();
        desired.push(sections::NewSection {
            wiki_id: wiki_id.to_owned(),
            source_path: source_path.clone(),
            section_ord: ord,
            heading_path: Some(heading.to_owned()),
            text: text.to_owned(),
            embedding,
        });
        sections::replace_page_sections(pool, &source_path, &desired)
            .await
            .expect("seed section");
    }

    async fn seed_section(
        pool: &SqlitePool,
        wiki_id: &str,
        ord: i64,
        text: &str,
        embedding: Vec<f32>,
    ) {
        let source_path = format!("wikis/{wiki_id}/doc.md");
        let mut desired: Vec<sections::NewSection> =
            sections::find_page_sections(pool, &source_path)
                .await
                .expect("read")
                .into_iter()
                .map(|r| sections::NewSection {
                    wiki_id: r.wiki_id,
                    source_path: r.source_path,
                    section_ord: r.section_ord,
                    heading_path: r.heading_path,
                    text: r.text,
                    embedding: r.embedding,
                })
                .collect();
        desired.push(sections::NewSection {
            wiki_id: wiki_id.to_owned(),
            source_path: source_path.clone(),
            section_ord: ord,
            heading_path: None,
            text: text.to_owned(),
            embedding,
        });
        sections::replace_page_sections(pool, &source_path, &desired)
            .await
            .expect("seed section");
    }

    #[tokio::test]
    async fn search_sections_ranks_by_cosine_and_bumps_recall() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(&pool, "alice-proj", 0, "near", vec![1.0, 0.0, 0.0, 0.0]).await;
        seed_section(&pool, "alice-proj", 1, "far", vec![0.0, 1.0, 0.0, 0.0]).await;

        let sender = SenderContext::user("alice");
        let hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            &sender,
        )
        .await
        .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "near");
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[0].handle(), "wikis/alice-proj/doc.md#0");

        // Telemetry advanced — the signal REM's recall-hot finding reads.
        let rows = sections::find_page_sections(&pool, "wikis/alice-proj/doc.md")
            .await
            .unwrap();
        assert!(rows.iter().all(|r| r.recall_count_30d == 1));
    }

    // -- rank fusion: the identifier the embedding cannot represent --

    /// Seed one wiki with a section the query matches **semantically** and
    /// one it matches **literally**, arranged so the two disagree: the
    /// decoy wins on cosine (it is the query vector), the identifier
    /// section scores zero against it.
    async fn seed_disagreeing_corpus(pool: &SqlitePool) {
        seed_smart_wiki(pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(
            pool,
            "alice-proj",
            0,
            "the retry policy, discussed at length",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        seed_section(
            pool,
            "alice-proj",
            1,
            "D-006. Retry with backoff, then dead-letter.",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
    }

    #[tokio::test]
    async fn search_sections_lifts_an_exact_term_over_a_better_cosine() {
        let pool = make_pool().await;
        seed_disagreeing_corpus(&pool).await;
        let sender = SenderContext::user("alice");

        let hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "D-006",
            10,
            &sender,
        )
        .await
        .expect("search");

        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].text.starts_with("D-006"),
            "the section that carries the identifier must lead, got {:?}",
            hits[0].text
        );
        // The order changed; the score did not. It is still the cosine,
        // which is what DEFAULT_SIGNPOST_FLOOR and the REM cycle read.
        assert!(
            (hits[0].score - 0.0).abs() < 1e-6,
            "score must stay a cosine, got {}",
            hits[0].score
        );
        assert!((hits[1].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn a_query_with_no_lexical_match_leaves_the_cosine_order_alone() {
        let pool = make_pool().await;
        seed_disagreeing_corpus(&pool).await;
        let sender = SenderContext::user("alice");

        let hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "quantum barnacles",
            10,
            &sender,
        )
        .await
        .expect("search");
        assert_eq!(hits.len(), 2);
        assert!(hits[0].text.starts_with("the retry policy"));
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn naming_the_project_ranks_its_identifier_first() {
        // The founder's case end to end: «cosa abbiamo deciso in D-006 di
        // proj» names the project, so the docs are pulled with floor 0,
        // and the fusion decides what surfaces.
        let pool = make_pool().await;
        seed_disagreeing_corpus(&pool).await;
        let sender = SenderContext::user("alice");

        let hits = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "cosa abbiamo deciso in D-006 di proj?",
            SlotBudget::new(1, 4_000),
            &sender,
        )
        .await
        .expect("named docs");

        assert_eq!(hits.len(), 1, "the slot holds one section");
        assert!(
            hits[0].text.starts_with("D-006"),
            "got {:?} — the single slot went to the decoy",
            hits[0].text
        );
    }

    #[tokio::test]
    async fn a_lexical_match_does_not_lift_a_section_over_the_signpost_floor() {
        // The floor decides *whether* to dig and runs first; the fusion
        // decides only what surfaces among what already cleared it.
        // Otherwise an unrelated turn would start dragging in docs
        // because one page happens to share a word with it.
        let pool = make_pool().await;
        seed_disagreeing_corpus(&pool).await;
        let wikis = vec!["alice-proj".to_owned()];

        let kept = rank_project_sections(
            &pool,
            &embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "D-006",
            &wikis,
            SlotBudget::new(10, 4_000),
            DEFAULT_SIGNPOST_FLOOR,
        )
        .await
        .expect("ranked");

        assert_eq!(kept.len(), 1, "only the section above the floor survives");
        assert!(kept[0].text.starts_with("the retry policy"));
    }

    /// Seed a project's **description signpost** — the funnel's own input:
    /// an ordinary fact on the subject's reserved `@projects.md`, carrying the
    /// topics [`crate::signposts`] writes.
    fn insert_signpost(
        pool_setup: &mut Vec<NewFact>,
        id_str: &str,
        owner_wiki: &str,
        subject: &str,
        project_wiki_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) {
        pool_setup.push(NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(id_str).unwrap(),
            wiki_id: owner_wiki.to_owned(),
            source_path: format!("wikis/{owner_wiki}/@projects.md"),
            region_start: Some(0),
            region_end: Some(32),
            text: text.to_owned(),
            embedding,
            subject_id: subject.parse().unwrap(),
            allow_ids: vec![],
            sender_id: None,
            fact_type: Some(crate::signposts::SIGNPOST_FACT_TYPE.to_owned()),
            topics: vec![
                crate::signposts::TOPIC_SIGNPOST.to_owned(),
                format!("signpost-wiki:{project_wiki_id}"),
                crate::signposts::TOPIC_DESCRIPTION.to_owned(),
            ],
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        });
    }

    /// The funnel's default: an ordinary personal turn never reaches a
    /// project's documentation, however large that documentation is.
    #[tokio::test]
    async fn the_funnel_keeps_a_project_shut_when_its_description_does_not_match() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-proj",
            0,
            "a doc section",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let mut rows = Vec::new();
        // The description points somewhere else entirely (orthogonal).
        insert_signpost(
            &mut rows,
            "019f0000-0000-7000-8000-00000000000a",
            "alice",
            "user:alice",
            "alice-proj",
            "A project about industrial label printing.",
            vec![0.0, 1.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let sender = SenderContext::user("alice");

        let admitted = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "cosa mangiamo stasera",
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("funnel");
        assert!(admitted.is_empty(), "got {admitted:?}");

        let hits = search_all(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "cosa mangiamo stasera",
            10,
            fact_index::FactFilters::default(),
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("search all");
        assert!(
            !hits.iter().any(|h| matches!(h, SearchHit::Section(_))),
            "a perfect-cosine section still must not surface: {hits:?}"
        );
    }

    /// …and opens it when the description does match, or when the turn
    /// names the project outright.
    #[tokio::test]
    async fn the_funnel_opens_a_project_on_its_description_or_its_name() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-signage", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-signage",
            0,
            "a doc section",
            vec![0.0, 0.0, 1.0, 0.0],
        )
        .await;
        let mut rows = Vec::new();
        insert_signpost(
            &mut rows,
            "019f0000-0000-7000-8000-00000000000b",
            "alice",
            "user:alice",
            "alice-signage",
            "Screens in shops that show adverts.",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let sender = SenderContext::user("alice");

        // (a) the description is the query's own direction → admitted
        let by_desc = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "how do the shop screens get their adverts",
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("funnel");
        assert_eq!(by_desc, vec!["alice-signage".to_owned()]);

        // (b) orthogonal query, but the turn NAMES the project → admitted
        //     regardless of the floor: naming is an instruction.
        let by_name = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![0.0, 1.0, 0.0, 0.0]),
            "come funziona signage per i clienti",
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("funnel");
        assert_eq!(by_name, vec!["alice-signage".to_owned()]);
    }

    /// A project with no description stays shut — the funnel fails closed,
    /// which is what makes the description load-bearing — and a reader who
    /// cannot see another user's signpost cannot open their project either.
    #[tokio::test]
    async fn the_funnel_fails_closed_without_a_readable_description() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-proj",
            0,
            "a doc section",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let sender = SenderContext::user("alice");

        // No signpost at all.
        let none = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "anything at all",
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("funnel");
        assert!(none.is_empty(), "no description ⇒ no admission: {none:?}");

        // …but a floor of 0 is an explicit "funnel off" switch.
        let off = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "anything at all",
            0.0,
            &sender,
        )
        .await
        .expect("funnel");
        assert_eq!(off, vec!["alice-proj".to_owned()]);

        // A description owned by someone else, shared with nobody, is
        // invisible — so it can neither admit the project nor reveal it.
        let mut rows = Vec::new();
        insert_signpost(
            &mut rows,
            "019f0000-0000-7000-8000-00000000000c",
            "bob",
            "user:bob",
            "alice-proj",
            "anything at all",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let still_shut = admitted_smart_wikis(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "anything at all",
            DEFAULT_SMART_CORPUS_FLOOR,
            &sender,
        )
        .await
        .expect("funnel");
        assert!(still_shut.is_empty(), "got {still_shut:?}");
    }

    /// A section the query **names** still beats a perfect-cosine fact.
    ///
    /// The guarantee the definition tier exists for, expressed through the
    /// signal that actually means "names": every query term present in the
    /// section's `heading_path`, which is how the indexer writes a real row
    /// (`wiki_sections."text"` opens with the heading chain). Its sibling
    /// below pins the other half — that merely *containing* a token is not
    /// enough.
    #[tokio::test]
    async fn a_section_the_query_names_outranks_a_perfect_cosine_fact() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section_with_heading(
            &pool,
            "alice-proj",
            0,
            "Decision log > D-006 - the retry policy",
            "Retry with backoff, then dead-letter.",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        // A fact that is a perfect cosine match — without the definition
        // tier it would sit on top of the section the query names.
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "019f0000-0000-7000-8000-000000000001",
            "alice-notes",
            "user:alice",
            "a perfectly on-topic fact",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let sender = SenderContext::user("alice");

        let hits = search_all(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "D-006",
            10,
            fact_index::FactFilters::default(),
            0.0, // funnel off: this test is about the cross-corpus ranking
            &sender,
        )
        .await
        .expect("search all");

        assert_eq!(hits.len(), 2);
        assert!(
            matches!(hits[0], SearchHit::Section(_)),
            "the NAMED section must lead; got {:?}",
            hits[0].text()
        );
        assert!(
            matches!(hits[1], SearchHit::Fact(_)),
            "got {:?}",
            hits[1].text()
        );
        // …and it led *despite* the fact scoring higher: the definition
        // tier, not the cosine, is what put it there.
        assert!(
            hits[1].score() > hits[0].score(),
            "{} vs {}",
            hits[1].score(),
            hits[0].score()
        );
    }

    /// The regression this fix exists for: a section that merely **mentions**
    /// a query token must not evict a better-scoring personal fact.
    ///
    /// Before the fix, `search_all` re-applied the *ranking* half of the
    /// lexical signal (`search_lexical`, OR over every term) to the merged
    /// list. A fact's handle is a `fact_id` and can never key into a list of
    /// `source_path#ord`, so only sections could collect that bonus — and
    /// `1/(60 + lexical_rank)` is larger than the whole span of `1/(60 +
    /// vector_rank)` across a list of `2·top_k`. Any section sharing any
    /// token therefore outranked every fact, whatever the cosines were. On
    /// the production corpus that inverted 11 of 14 probe queries.
    #[tokio::test]
    async fn a_section_that_merely_mentions_a_term_does_not_evict_a_better_fact() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        // Shares the word, is nowhere near the query by cosine, and its
        // heading does not name it — the shape of a documentation page that
        // happens to contain an ordinary word.
        seed_section_with_heading(
            &pool,
            "alice-proj",
            0,
            "Player runtime > update channel",
            "The playlist is fetched by the player on every sync.",
            vec![0.0, 0.0, 1.0, 0.0],
        )
        .await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "019f0000-0000-7000-8000-000000000002",
            "alice",
            "user:alice",
            "Alice likes the playlist she made with her son.",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        let sender = SenderContext::user("alice");

        let hits = search_all(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "playlist",
            10,
            fact_index::FactFilters::default(),
            0.0, // funnel off, so the ranking alone is under test
            &sender,
        )
        .await
        .expect("search all");

        assert_eq!(hits.len(), 2, "both corpora are in the merge");
        assert!(
            matches!(hits[0], SearchHit::Fact(_)),
            "the better-scoring fact must lead; got {:?}",
            hits[0].text()
        );
        assert!(
            hits[0].score() > hits[1].score(),
            "the merged list is in score order when nothing is NAMED: {} vs {}",
            hits[0].score(),
            hits[1].score()
        );
    }

    /// The shape that beat rank fusion on the production corpus after
    /// 1.5.4 shipped: the section that *cites* the identifier leads the
    /// vector list **and** is in the lexical list, two positions behind
    /// the one that defines it. RRF cannot recover from that — the
    /// definition tier can.
    #[tokio::test]
    async fn a_section_titled_with_the_query_outranks_one_that_cites_it() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section_with_heading(
            &pool,
            "alice-proj",
            0,
            "Decision log > D-001 - adopt the item / content split",
            "A picture is two things, not one (S-019, D-006): the preview and the original.",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        seed_section_with_heading(
            &pool,
            "alice-proj",
            1,
            "Decision log > D-006 - a picture on screen is a preview, not the file",
            "Adopted whole, including the escape hatch.",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;

        let hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "D-006",
            10,
            &SenderContext::user("alice"),
        )
        .await
        .expect("search");

        assert_eq!(hits.len(), 2);
        assert!(
            hits[0]
                .heading_path
                .as_deref()
                .is_some_and(|h| h.contains("D-006")),
            "the section titled with the identifier must lead, got {:?}",
            hits[0].heading_path
        );
        // The tier reorders; it must not rewrite the cosine the signpost
        // floor and the merge both read.
        assert!(hits[0].score < hits[1].score);
    }

    #[tokio::test]
    async fn the_definition_tier_is_inert_on_a_prose_query() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section_with_heading(
            &pool,
            "alice-proj",
            0,
            "Pictures",
            "the preview is what the page draws",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        seed_section_with_heading(
            &pool,
            "alice-proj",
            1,
            "Pictures > storage",
            "the original lives on the server",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;

        // No heading carries *every* term, so nothing is promoted and the
        // cosine order stands.
        let hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "how are pictures stored on the server",
            10,
            &SenderContext::user("alice"),
        )
        .await
        .expect("search");
        assert_eq!(hits[0].heading_path.as_deref(), Some("Pictures"));
    }

    #[tokio::test]
    async fn search_sections_honours_the_wiki_level_acl() {
        let pool = make_pool().await;
        // Alice's private project, and one she shared with a group Bob is in.
        seed_smart_wiki(&pool, "alice-private", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-private",
            0,
            "private note",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        seed_smart_wiki(
            &pool,
            "alice-shared",
            "user:alice",
            vec![Principal::Group("devs".to_owned())],
        )
        .await;
        seed_section(
            &pool,
            "alice-shared",
            0,
            "shared note",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let alice = SenderContext::user("alice");
        let owner_hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            &alice,
        )
        .await
        .unwrap();
        assert_eq!(owner_hits.len(), 2, "the wiki's owner reads both");

        let bob = SenderContext {
            sender_id: "bob".to_owned(),
            sender_groups: vec!["devs".to_owned()],
        };
        let grantee_hits = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            &bob,
        )
        .await
        .unwrap();
        assert_eq!(
            grantee_hits.len(),
            1,
            "a group grantee reads only the share"
        );
        assert_eq!(grantee_hits[0].text, "shared note");

        let stranger = SenderContext::user("carol");
        let none = search_sections(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            &stranger,
        )
        .await
        .unwrap();
        assert!(none.is_empty(), "a stranger reads neither");
    }

    #[tokio::test]
    async fn a_revoke_closes_the_section_read_window_with_one_row_write() {
        let pool = make_pool().await;
        seed_smart_wiki(
            &pool,
            "alice-proj",
            "user:alice",
            vec![Principal::User("bob".to_owned())],
        )
        .await;
        seed_section(&pool, "alice-proj", 0, "shared", vec![1.0, 0.0, 0.0, 0.0]).await;

        let bob = SenderContext::user("bob");
        assert_eq!(
            search_sections(
                &pool,
                embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
                "q",
                10,
                &bob
            )
            .await
            .unwrap()
            .len(),
            1
        );

        // The revoke touches the registry row only — no per-section rewrite.
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        assert!(
            search_sections(
                &pool,
                embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
                "q",
                10,
                &bob
            )
            .await
            .unwrap()
            .is_empty(),
            "the revoke closes the read window immediately"
        );
    }

    // -- the named-project trigger --

    #[test]
    fn a_named_slug_matches_however_the_operator_writes_it() {
        let tokens = name_tokens("Come funziona questa cosa di AcmeSigns?");
        assert!(message_names_wiki(&tokens, "acmesigns"));
        // Case and punctuation are irrelevant on both sides.
        assert!(message_names_wiki(
            &name_tokens("parliamo di LNPrint."),
            "lnprint"
        ));
        // A hyphenated slug matches the spaced or hyphenated spelling.
        assert!(message_names_wiki(
            &name_tokens("il mwe-mcp è lento"),
            "mwe-mcp"
        ));
        assert!(message_names_wiki(
            &name_tokens("il mwe mcp è lento"),
            "mwe-mcp"
        ));
    }

    #[test]
    fn a_compound_slug_never_fires_on_one_of_its_words() {
        // `cc-pc-lavoro` must not turn every message about *lavoro* into a
        // project-docs lookup. The slug matches as a whole sequence only.
        let tokens = name_tokens("domani ho tanto lavoro");
        assert!(!message_names_wiki(&tokens, "cc-pc-lavoro"));
        assert!(message_names_wiki(
            &name_tokens("sul cc pc lavoro"),
            "cc-pc-lavoro"
        ));
    }

    #[test]
    fn a_slug_inside_a_longer_word_does_not_count() {
        // Substring matching would fire here; token matching does not.
        assert!(!message_names_wiki(&name_tokens("acmesignss"), "acmesigns"));
        assert!(!message_names_wiki(
            &name_tokens("superacmesigns"),
            "acmesigns"
        ));
    }

    #[test]
    fn a_too_short_slug_never_triggers() {
        assert!(!message_names_wiki(&name_tokens("va bene ok"), "ok"));
        assert!(!message_names_wiki(&name_tokens("a b c"), "abc"));
    }

    #[tokio::test]
    async fn project_docs_fire_only_when_the_message_names_the_project() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        seed_section(
            &pool,
            "franz-acmesigns",
            0,
            "Content is pushed to each display over a websocket channel.",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let sender = SenderContext::user("franz");

        let hit = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "come fa acmesigns a inviare i contenuti ai display?",
            SlotBudget::new(3, 2_000),
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(hit.len(), 1);
        assert!(hit[0].text.contains("websocket"));

        // Same corpus, same embedder — but nothing is named and no
        // signpost surfaced, so the slot stays empty and the turn pays
        // nothing.
        let unnamed = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "stasera ceniamo alle otto",
            SlotBudget::new(3, 2_000),
            &sender,
        )
        .await
        .unwrap();
        assert!(unnamed.is_empty());
    }

    /// Plant a description signpost for `project` on `subject`'s reserved
    /// page and return it as a hit, the way the turn's fact recall would
    /// have surfaced it.
    async fn surfaced_signpost(
        pool: &SqlitePool,
        owner_wiki: &str,
        subject: &str,
        project: &str,
        text: &str,
    ) -> RecallHit {
        let fact_id = crate::capture::new_fact_id().unwrap();
        let row = NewFact {
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: owner_wiki.to_owned(),
            source_path: format!("wikis/{owner_wiki}/@projects.md"),
            region_start: Some(0),
            region_end: Some(32),
            text: text.to_owned(),
            embedding: vec![0.0, 0.0, 1.0, 0.0],
            subject_id: subject.parse().unwrap(),
            allow_ids: vec![],
            sender_id: None,
            fact_type: Some(crate::signposts::SIGNPOST_FACT_TYPE.to_owned()),
            topics: crate::signposts::description_topics(project),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        };
        fact_index::insert(pool, &row).await.expect("insert");
        let stored = fact_index::find_by_id(pool, &fact_id)
            .await
            .expect("read")
            .expect("row");
        RecallHit::from_row(stored, 0.9)
    }

    #[tokio::test]
    async fn a_surfaced_signpost_opens_its_project_without_the_name() {
        // A symptom of what the product does, with AcmeSigns never named.
        // The signpost is what gets the turn to the door; the similarity
        // is what opens it.
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        seed_section(
            &pool,
            "franz-acmesigns",
            0,
            "Content is pushed to each display over a websocket channel; a stalled feed means the channel dropped.",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let signpost = surfaced_signpost(
            &pool,
            "franz",
            "user:franz",
            "franz-acmesigns",
            "AcmeSigns — sistema che manda i contenuti ai cartelli digitali dei negozi",
        )
        .await;
        let sender = SenderContext::user("franz");

        let hits = recall_signposted_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "mi ha chiamato un cliente che dice che i contenuti sono fermi da 10 giorni",
            std::slice::from_ref(&signpost),
            &[],
            SlotBudget::new(3, 2_000),
            DEFAULT_SIGNPOST_FLOOR,
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1, "the signpost must open the project");
        assert!(hits[0].text.contains("websocket"));
    }

    #[tokio::test]
    async fn a_marginally_related_turn_reaches_the_project_and_keeps_nothing() {
        // The gate mechanism: the signpost still fires the lookup — that
        // is cheap — but nothing clears the floor, so the slot stays
        // empty. (Mechanism only. On the real corpus the floor separates
        // an unrelated turn from a project-neighbourhood one; it does NOT
        // separate two neighbourhood turns — see
        // `DEFAULT_SIGNPOST_FLOOR`.)
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        seed_section(
            &pool,
            "franz-acmesigns",
            0,
            "Content is pushed to each display over a websocket channel.",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let signpost = surfaced_signpost(
            &pool,
            "franz",
            "user:franz",
            "franz-acmesigns",
            "AcmeSigns — sistema che manda i contenuti ai cartelli digitali",
        )
        .await;
        let sender = SenderContext::user("franz");
        // Orthogonal to the section: cosine 0, far below the floor.
        let unrelated = embedder_fixed(vec![0.0, 1.0, 0.0, 0.0]);

        let hits = recall_signposted_project_docs(
            &pool,
            Arc::clone(&unrelated),
            "domani alle 17:00 devo andare da questo cliente che ha il display che non funziona",
            std::slice::from_ref(&signpost),
            &[],
            SlotBudget::new(3, 2_000),
            DEFAULT_SIGNPOST_FLOOR,
            &sender,
        )
        .await
        .unwrap();
        assert!(
            hits.is_empty(),
            "a marginal mention must not drag in the project docs"
        );

        // Naming the project is an instruction, not a guess: the same
        // weak similarity is served, because the turn asked for it.
        let named = recall_named_project_docs(
            &pool,
            unrelated,
            "cosa dice la documentazione di acmesigns?",
            SlotBudget::new(3, 2_000),
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(
            named.len(),
            1,
            "the floor does not apply to a named project"
        );
    }

    #[tokio::test]
    async fn a_signpost_never_opens_a_project_its_reader_cannot_see() {
        // Belt and braces: the ACL is re-checked against the registry,
        // not inferred from the signpost that surfaced.
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        seed_section(
            &pool,
            "franz-acmesigns",
            0,
            "internal architecture note",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let signpost = surfaced_signpost(
            &pool,
            "franz",
            "user:franz",
            "franz-acmesigns",
            "AcmeSigns — un sistema",
        )
        .await;

        let hits = recall_signposted_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "i contenuti sono fermi",
            std::slice::from_ref(&signpost),
            &[],
            SlotBudget::new(3, 2_000),
            DEFAULT_SIGNPOST_FLOOR,
            &SenderContext::user("carol"),
        )
        .await
        .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn naming_a_project_you_cannot_read_yields_nothing() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        seed_section(
            &pool,
            "franz-acmesigns",
            0,
            "internal architecture note",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let stranger = SenderContext::user("carol");
        let hits = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "come funziona acmesigns?",
            SlotBudget::new(3, 2_000),
            &stranger,
        )
        .await
        .unwrap();
        assert!(
            hits.is_empty(),
            "naming a project must not reveal it to someone who cannot read it"
        );
    }

    #[tokio::test]
    async fn the_char_budget_bounds_the_slot_with_whole_sections() {
        let pool = make_pool().await;
        seed_smart_wiki(&pool, "franz-acmesigns", "user:franz", Vec::new()).await;
        let long = "x".repeat(300);
        seed_section(&pool, "franz-acmesigns", 0, &long, vec![1.0, 0.0, 0.0, 0.0]).await;
        seed_section(&pool, "franz-acmesigns", 1, &long, vec![0.9, 0.1, 0.0, 0.0]).await;
        seed_section(&pool, "franz-acmesigns", 2, &long, vec![0.8, 0.2, 0.0, 0.0]).await;
        let sender = SenderContext::user("franz");

        let hits = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "acmesigns",
            SlotBudget::new(10, 700),
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "a third whole section would overrun 700 chars"
        );
        assert!(
            hits.iter().all(|h| h.text.len() == 300),
            "sections are kept whole, never truncated"
        );

        // A budget smaller than the first hit still returns it — one whole
        // section beats an empty slot on a question that named the project.
        let tiny = recall_named_project_docs(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "acmesigns",
            SlotBudget::new(10, 10),
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(tiny.len(), 1);

        // `top_k = 0` disables the slot outright.
        assert!(
            recall_named_project_docs(
                &pool,
                embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
                "acmesigns",
                SlotBudget::new(0, 2_000),
                &sender,
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn search_all_merges_both_corpora_and_wiki_search_stays_facts_only() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000000f1",
            "alice",
            "user:alice",
            "a personal fact",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-proj",
            0,
            "a doc section",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let sender = SenderContext::user("alice");

        // The fact corpus alone — this is what the ingest turn recalls, so
        // documentation can no longer crowd out personal memory there.
        let facts = wiki_search(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            fact_index::FactFilters::default(),
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].text, "a personal fact");

        // Both, for the consumer surfaces that ask for everything.
        let all = search_all(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            10,
            fact_index::FactFilters::default(),
            0.0, // funnel off: this test is about the merge, not the gate
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|h| matches!(h, SearchHit::Fact(_))));
        assert!(all.iter().any(|h| matches!(h, SearchHit::Section(_))));
        // Ranked as one list.
        assert!(all[0].score() >= all[1].score());
    }

    #[tokio::test]
    async fn search_all_truncates_to_top_k_across_the_merge() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000000f2",
            "alice",
            "user:alice",
            "fact",
            vec![0.0, 1.0, 0.0, 0.0],
        );
        populate(&pool, rows).await;
        seed_smart_wiki(&pool, "alice-proj", "user:alice", Vec::new()).await;
        seed_section(
            &pool,
            "alice-proj",
            0,
            "closer section",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let sender = SenderContext::user("alice");
        let all = search_all(
            &pool,
            embedder_fixed(vec![1.0, 0.0, 0.0, 0.0]),
            "q",
            1,
            fact_index::FactFilters::default(),
            0.0, // funnel off: this test is about top_k across the merge
            &sender,
        )
        .await
        .unwrap();
        assert_eq!(all.len(), 1, "top_k is honoured across the merged ranking");
        assert!(
            matches!(all[0], SearchHit::Section(_)),
            "the closer hit wins"
        );
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
    async fn find_by_filters_combines_subject_and_fact_type() {
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
            subject_id: "user:alice".parse().unwrap(),
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
                subject_id: Some("user:alice".parse().unwrap()),
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
            subject_id: "user:alice".parse().unwrap(),
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

    /// The ACL now narrows the QUERY as well as the result rows, and the two
    /// must agree. This is the test that keeps them agreeing: read access is
    /// `subject ∪ allow ∪ sender`, none of the three sufficient alone, so a
    /// predicate that forgot one would silently hide facts a reader is
    /// entitled to — and nothing else would notice, because the row check
    /// downstream can only ever remove rows, never restore one the query
    /// never fetched.
    #[tokio::test]
    async fn acl_predicate_admits_every_axis_the_row_check_admits() {
        let pool = make_pool().await;
        let emb = vec![1.0, 0.0, 0.0, 0.0];
        let mut rows = Vec::new();
        // Readable through the SUBJECT axis.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a001",
            "w",
            "user:alice",
            "own",
            emb.clone(),
        );
        // Through the universal group — stored as the bare `global`.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a002",
            "w",
            "global",
            "public",
            emb.clone(),
        );
        // Through the ALLOW axis: owned by somebody else, shared with alice.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a003",
            "w",
            "user:bob",
            "shared with alice",
            emb.clone(),
        );
        rows[2].allow_ids = vec!["user:alice".parse().unwrap()];
        // Through the ALLOW axis by GROUP membership.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a004",
            "w",
            "user:bob",
            "shared with the family",
            emb.clone(),
        );
        rows[3].allow_ids = vec!["group:famiglia".parse().unwrap()];
        // Through the SENDER axis: alice captured it, about somebody else.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a005",
            "w",
            "user:bob",
            "alice said this about bob",
            emb.clone(),
        );
        rows[4].sender_id = Some("user:alice".parse().unwrap());
        // Readable through none of the three.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-00000000a006",
            "w",
            "user:bob",
            "bob's own business",
            emb.clone(),
        );
        populate(&pool, rows).await;

        let sender = SenderContext {
            sender_id: "alice".to_owned(),
            sender_groups: vec!["famiglia".to_owned()],
        };
        let hits = wiki_search(
            &pool,
            embedder_fixed(emb),
            "query",
            50,
            FactFilters::default(),
            &sender,
        )
        .await
        .unwrap();
        let texts: std::collections::BTreeSet<&str> =
            hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "own",
                "public",
                "shared with alice",
                "shared with the family",
                "alice said this about bob",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<&str>>(),
            "every axis that grants read must survive the predicate"
        );
        assert!(
            !texts.contains("bob's own business"),
            "and nothing else may"
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

    // ---------- the link grammar ----------

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

    // -- recall_due_soon (the due-soon slot) --

    fn insert_due(
        pool_setup: &mut Vec<NewFact>,
        id_str: &str,
        subject: &str,
        text: &str,
        valid_to: Option<&str>,
    ) {
        let wiki = subject.split(':').next_back().unwrap_or(subject);
        insert_row(pool_setup, id_str, wiki, subject, text, vec![0.1; 4]);
        pool_setup.last_mut().unwrap().valid_to = valid_to.map(str::to_owned);
    }

    /// The candidate leg that makes reconciliation honest: for a page the
    /// turn actually opened, **every** readable fact on it comes back — not a
    /// top-K sample of it — while a fact the reader may not see is absent, and
    /// a page nobody opened contributes nothing.
    #[tokio::test]
    async fn facts_on_pages_returns_the_whole_page_and_only_what_the_reader_may_see() {
        let pool = make_pool().await;
        let mut rows = Vec::new();
        // Three facts on ONE page, all alice's — a similarity top-K would
        // have shown at most some of them.
        for (n, text) in [
            (1u8, "greenhouse frame ordered"),
            (2, "greenhouse glass in may"),
            (3, "greenhouse abandoned"),
        ] {
            insert_row(
                &mut rows,
                &format!("018f1234-5678-7abc-9def-0000000e000{n}"),
                "alice",
                "user:alice",
                text,
                vec![0.1; 4],
            );
        }
        // Bob's private fact, on the SAME page — must never come back for alice.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000e0009",
            "alice",
            "user:bob",
            "bob's private note",
            vec![0.1; 4],
        );
        // A fact on a page the navigator did not open.
        insert_row(
            &mut rows,
            "018f1234-5678-7abc-9def-0000000e000a",
            "carol",
            "user:alice",
            "unrelated page",
            vec![0.1; 4],
        );
        populate(&pool, rows).await;

        let hits = facts_on_pages(
            &pool,
            &["wikis/alice/intro.md".to_owned()],
            &SenderContext::user("alice"),
            32,
        )
        .await
        .expect("page-scoped candidates");

        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(
            texts.len(),
            3,
            "every readable fact on the opened page, not a ranked sample: {texts:?}"
        );
        assert!(texts.contains(&"greenhouse abandoned"));
        assert!(
            !texts.iter().any(|t| t.contains("bob")),
            "a fact the reader may not see can neither be shown nor closed"
        );
        assert!(
            !texts.iter().any(|t| t.contains("unrelated")),
            "a page nobody opened contributes nothing"
        );

        // Newest first, so the cap drops the least likely candidate. A
        // closure almost always targets something written recently; filling
        // the cap oldest-first hides exactly the fact that would have matched.
        let cheap = facts_on_pages(
            &pool,
            &["wikis/alice/intro.md".to_owned()],
            &SenderContext::user("alice"),
            1,
        )
        .await
        .expect("capped");
        assert_eq!(cheap.len(), 1);
        assert_eq!(
            cheap[0].text, "greenhouse abandoned",
            "the cap kept the most recent fact, not the alphabetically or \
             chronologically first one"
        );

        // No pages, or a zero cap, is not an error — it is an empty leg.
        assert!(
            facts_on_pages(&pool, &[], &SenderContext::user("alice"), 32)
                .await
                .expect("no pages")
                .is_empty()
        );
        assert!(
            facts_on_pages(
                &pool,
                &["wikis/alice/intro.md".to_owned()],
                &SenderContext::user("alice"),
                0
            )
            .await
            .expect("zero cap")
            .is_empty()
        );
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
