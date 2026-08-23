// SPDX-License-Identifier: AGPL-3.0-or-later
//! The captures buffer — the pre-compilation staging area for the
//! **standard** path.
//!
//! ## Why this exists
//!
//! Earlier, `wiki_ingest_message` wrote each classified claim straight
//! into the published `.md` page (via [`crate::capture::wiki_capture`]). The
//! result was a raw log of `{{owner=…}}…{{/}}`
//! markers stacked per subject — no synthesis, no dedup, no topic organisation.
//! The root cause was the *Sam-as-Author* assumption — an
//! agent writing finished prose in the turn — which is incoherent with mwe-mcp
//! being agent-agnostic. The standard-wiki path restores the old engine's pipeline:
//!
//! ```text
//! message → archive → classifier → BUFFER(captures) → facts → wiki(.md compiled)
//! ```
//!
//! For a **standard** wiki (one whose `_meta` smart flag is `false`)
//! the ingest router now writes the classified [`CaptureRequest`] *here*, never
//! into the published `.md`. The light dream promotes buffered captures
//! into `fact_index`; the nightly Cronista compiles facts into prose. The
//! published `.md` is the compiler's OUTPUT.
//!
//! Smart wikis (`_meta` smart flag `true`, smart-consumer-owned) do **not** use
//! this module — their pages are hand-authored via `wiki_admin_*`. The
//! perimeter is standard families only.
//!
//! ## Where a pending capture lives
//!
//! In this table, and nowhere else. `capture_buffer` is the **source of
//! truth** for a claim between the turn that captured it and the compile that
//! writes it onto a page, exactly as the product's principle says it should be
//! (*authority follows the author*: engine-curated memory is DB-authoritative,
//! the pages are its render — see the memory model). Durability is the
//! workdir snapshot's job ([`crate::backup`]), which takes the DB image and
//! the file tree together because neither reconstructs the other.
//!
//! **There is no on-disk capture journal, and nothing may coin one.** A file
//! every capture reads and rewrites whole, that nothing prunes, and whose
//! per-entry `status=` never moves off `buffered`, is a second description of
//! this table that goes stale on the first light dream — and a safety-net
//! reindex then re-parses a wiki's entire history to insert rows that already
//! exist.
//!
//! ## Id stability
//!
//! Each capture is minted a `UUIDv7` `capture_id` at buffer time and that id is
//! reused verbatim as the `fact_id` when the light dream promotes it. A claim
//! therefore keeps one stable id across buffer → fact → compiled-page — the
//! correctness hinge for incremental compilation (fingerprints key on
//! `fact_id`s).

use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture::CaptureRequest;
use crate::embedder::Embedder;
use crate::types::{FactId, FactIdParseError, Principal, PrincipalParseError, WikiIdParseError};
use crate::wiki::WikiError;

/// Errors raised by the captures buffer.
#[derive(Debug, Error)]
pub enum CaptureBufferError {
    /// Underlying filesystem-tree error (e.g. unknown wiki id).
    #[error("capture_buffer wiki io: {0}")]
    Wiki(#[from] WikiError),
    /// Underlying `SQLite` error.
    #[error("capture_buffer db: {0}")]
    Db(#[from] sqlx::Error),
    /// Low-level filesystem IO error.
    #[error("capture_buffer io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialisation of the `allow_ids` / `topics` columns failed.
    #[error("capture_buffer json: {0}")]
    Json(#[from] serde_json::Error),
    /// A `fact_id` / `capture_id` failed to parse.
    #[error("capture_buffer fact id: {0}")]
    FactId(#[from] FactIdParseError),
    /// A persisted `wiki_id` failed to parse.
    #[error("capture_buffer wiki id: {0}")]
    WikiId(#[from] WikiIdParseError),
    /// A persisted principal failed to parse.
    #[error("capture_buffer principal: {0}")]
    Principal(#[from] PrincipalParseError),
    /// Reading back the `fact_index` row a re-buffer is un-writing failed.
    #[error("capture_buffer fact index: {0}")]
    FactIndex(#[from] crate::fact_index::FactIndexError),
    /// Body submitted was empty or whitespace-only.
    #[error("capture body is empty")]
    EmptyBody,
    /// Body contained a reserved sequence. Captured claims are plain prose; the
    /// `{{…}}` marker grammar is managed by mwe-mcp, so a body may not
    /// contain it, and `<!--` stays reserved for the same class of reason.
    #[error(
        "capture body must not contain literal {{{{ or }}}} or an HTML comment (managed by mwe-mcp)"
    )]
    BodyContainsReserved,
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, CaptureBufferError>;

/// Lifecycle state of a buffered capture (the `status` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStatus {
    /// Written by ingest, awaiting the light dream.
    Buffered,
    /// The light dream inserted a `fact_index` row (`fact_id == capture_id`).
    Promoted,
    /// The light dream found an exact/near duplicate; the capture resolved to an
    /// existing fact instead of a new one.
    SkippedDup,
}

impl CaptureStatus {
    /// Stable wire/DB string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buffered => "buffered",
            Self::Promoted => "promoted",
            Self::SkippedDup => "skipped_dup",
        }
    }

    /// Decode from the stored string; unknown values fall back to `Buffered`.
    #[must_use]
    pub fn from_db(s: &str) -> Self {
        match s {
            "promoted" => Self::Promoted,
            "skipped_dup" => Self::SkippedDup,
            _ => Self::Buffered,
        }
    }
}

/// One buffered capture — the classifier's output for a single claim, staged
/// for the light dream. Mirrors `fact_index`'s classifier/ACL columns so
/// promotion is a straight copy.
///
/// **It says nothing about where the claim will be written**, and that is the
/// point of the buffer: this is the queue of claims waiting to be *sorted* and
/// then written as prose, and the sorting is the light dream's, taken against
/// the memory as it stands when it reads the queue (founder, 2026-08-18 —
/// migration `0071_capture_buffer_no_destination`). Everything here describes
/// the CLAIM; the wiki a promoted fact needs for its "no page yet" address is
/// derived at promotion from the subject, which never moves.
// `PartialEq` only: the staged embedding is `Vec<f32>`, and floats have no
// total equality. Nothing compares captures for `Eq` — the derive was free
// until a vector landed on the struct.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferedCapture {
    /// `UUIDv7`; reused verbatim as the `fact_id` on promotion.
    pub capture_id: FactId,
    /// Captured claim prose, verbatim, no markers.
    pub body: String,
    /// The fact's **subject** — who or what it is *about* (not its author
    /// `sender`, not its audience `allow`).
    pub subject: Principal,
    /// Extra principals granted read access via `allow=`.
    pub allow: Vec<Principal>,
    /// Cross-user attribution (who captured the fact). Always materialized
    /// (= subject when absent) and kept distinct from `subject`; `None` survives
    /// only as the degenerate scrubbed state that falls back to subject.
    pub sender: Option<Principal>,
    /// Optional fact taxonomy hint (`bio`, `preference`, …).
    pub fact_type: Option<String>,
    /// Optional topic tags.
    pub topics: Vec<String>,
    /// Optional `fact_id` the classifier flagged this capture as superseding.
    pub supersede_hint: Option<FactId>,
    /// Lifecycle state.
    pub status: CaptureStatus,
    /// ISO-8601 capture timestamp.
    pub captured_at: String,
    /// ISO-8601, set when the light dream resolves the row.
    pub processed_at: Option<String>,
    /// The fact this capture became (`== capture_id`) or deduped into.
    pub resolved_fact_id: Option<FactId>,
    /// `ingest` | `shadow_diff` | `dashboard`.
    pub source_kind: String,
    /// Optional provenance reference.
    pub source_ref: Option<String>,
    /// The per-fact validity interval the classifier deduced, staged here so the
    /// light dream copies it into `fact_index` on promotion (closing the gap on
    /// the standard-wiki path). `valid_from` = when the fact starts holding
    /// (`None` = open-start); `None` `valid_to` = OPEN ("true now, no horizon").
    pub valid_from: Option<String>,
    /// End of the validity interval; `None` = OPEN. See [`Self::valid_from`].
    pub valid_to: Option<String>,
    /// Why the staged validity window was closed — `None` at buffer time (a
    /// fresh capture is alive); set only when a **closure gesture lands while
    /// the capture is still buffered** (the same-day flow: the item is bought
    /// before the light dream promotes it). [`close_validity`] stamps it
    /// together with `valid_to`; promotion stamps it onto the fact.
    pub decay_reason: Option<String>,
    /// The ingest classifier's proposed page writing style (closed palette
    /// `prosa` | `prosa-tecnica` | `lista`), staged here so
    /// [`promote_one`](crate::dream_light) copies it onto the fact
    /// (`fact_index.style`), where the light cadence seeds a new page's
    /// testata from it. A *rendering* hint, not a destination: it says how the
    /// page that ends up holding this claim should read, never which page that
    /// is. `None` = the classifier proposed nothing.
    pub style: Option<crate::wiki::PageStyle>,
    /// Per-fact salience the classifier deduced, staged here so
    /// [`promote_one`](crate::dream_light) copies it onto the fact
    /// (`fact_index.salience`), where `high` facts are routed to the
    /// subject's identity card. `high | normal | low`; `None` = unspecified.
    pub salience: Option<String>,
    /// How many placement passes have read this claim and given it no page.
    ///
    /// `0` = never offered to one. A claim nobody could place has nowhere to
    /// go — no page means it simply keeps waiting (founder, 2026-08-22) — and
    /// without this counter *never looked at* and *looked at and declined*
    /// would be the same row. Observational: nothing gates on it. It is what
    /// lets the nightly strong pass, and an operator reading the buffer, tell
    /// a claim the cheap tier has declined four times from one that arrived
    /// five minutes ago.
    pub placement_attempts: i64,
    /// When the most recent placement pass read this claim without placing it.
    pub last_attempt_at: Option<String>,
    /// Project-wiki pages this turn authored, as plain `[[wiki_id/page]]`
    /// wikilinks ([`crate::fact_index::NewFact::authored_refs`]), staged here
    /// so [`promote_one`](crate::dream_light) copies them onto the fact and
    /// consolidation links instead of duplicating. Empty
    /// for a pure-standard capture.
    pub authored_refs: Vec<String>,
    /// The capture's embedding, computed **once** at buffer time over the
    /// marker-stripped body — the same text, by the same rule, that
    /// [`promote_one`](crate::dream_light) would embed at promotion and that
    /// [`crate::recall::recall_fresh_captures`] would embed to rank it.
    ///
    /// Both of those read this instead now. It is not an optimisation stacked
    /// on top of the old cost: it is one of two identical computations kept
    /// and the other deleted — the read path's, which ran once per turn per
    /// pending capture, and was the expensive one.
    ///
    /// `None` is a first-class state, not a fault to repair eagerly: a
    /// transient embedder fault at buffer time must not cost the capture, and
    /// a row written before this column existed has none. Both readers fall
    /// back to computing it — which is exactly the pre-existing behaviour.
    /// Like `status` / `processed_at` /
    /// `decay_reason`.
    pub embedding: Option<Vec<f32>>,
    /// Fingerprint of the conversational turn this capture was extracted from
    /// ([`origin_fingerprint`]); `None` when there is no single originating
    /// message (a document job, a dashboard write, a row older than the
    /// column).
    ///
    /// The fresh recall slot uses it to avoid putting the same thing twice in
    /// one recall block: the turn already carries the raw messages the
    /// consumer holds plus the cross-consumer recent window, so a fresh
    /// capture derived from a message the agent is being shown anyway adds
    /// characters and nothing else. Comparing origins is exact; comparing ages
    /// is not, because that window is bounded by entry count and by characters
    /// as well as by TTL.
    ///
    /// A hash rather than the text: an origin may be a long paste, and the
    /// buffer must not become a second transcript. Unlike the vector beside
    /// it, it cannot be recomputed from anything the row holds.
    pub origin_message_hash: Option<String>,
}

/// Fingerprint of an originating message, for
/// [`BufferedCapture::origin_message_hash`].
///
/// SHA-256 of the trimmed text, hex: stable across processes and releases
/// (which `DefaultHasher` is not) and fixed-width whatever the message.
///
/// Trimming is the whole of the normalisation, deliberately. The two sides
/// ever compared are the *same string* arriving by two routes — the message
/// the consumer sent us, and the message we recorded for the cross-consumer
/// window — so anything cleverer (case folding, whitespace collapsing) would
/// only widen the match into territory where two genuinely different turns
/// start colliding, and a collision here silently drops a fact from recall.
#[must_use]
pub fn origin_fingerprint(message: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(message.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Material computed for a capture at buffer time, beside the claim itself.
///
/// Both fields are optional and both default to absent, so a caller with
/// neither an embedder nor an originating message stages nothing and the row
/// behaves exactly as it did before these columns existed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BufferStaging {
    /// See [`BufferedCapture::embedding`].
    pub embedding: Option<Vec<f32>>,
    /// See [`BufferedCapture::origin_message_hash`].
    pub origin_message_hash: Option<String>,
}

impl BufferStaging {
    /// Compute the staging for a capture about to be buffered.
    ///
    /// The one place that turns a claim into a vector for the buffer, so the
    /// "which text gets embedded" rule cannot drift from the one promotion and
    /// the fresh slot apply: the **marker-stripped** body, because a catalog
    /// id is a key and not prose.
    ///
    /// Soft on the embedder. A capture is durable memory and an embedding is
    /// derivable, so a transient fault leaves the vector `None` with a warning
    /// and the claim is buffered anyway — the readers recompute. The opposite
    /// trade (fail the capture to guarantee the column) would lose the one
    /// thing that cannot be reconstructed.
    pub async fn build(embedder: &dyn Embedder, body: &str, origin_message: Option<&str>) -> Self {
        let embedding = match embedder
            .embed(&crate::parser::strip_embed_markers(body))
            .await
        {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "capture_buffer: embedding at buffer time failed — staged without a vector \
                     (recall and promotion will compute it)"
                );
                None
            },
        };
        Self {
            embedding,
            origin_message_hash: origin_message.map(origin_fingerprint),
        }
    }
}

/// Outcome of [`buffer_capture`].
#[derive(Debug, Clone)]
pub struct BufferOutcome {
    /// Id of the buffered capture (anchors the consumer's audit row).
    pub capture_id: FactId,
}

// ---------- write path ----------

/// Buffer a classified capture for a **standard** wiki.
///
/// Inserts it into the `capture_buffer` table, which is where a pending
/// capture lives — there and nowhere else. The published `.md` is left
/// untouched: the compiler produces it, from the fact this becomes.
///
/// `supersede_hint` carries the classifier's proposed supersede target (if any);
/// the actual supersede happens at promotion time, not here.
///
/// The caller is responsible for having decided the target wiki is a standard wiki
/// (a wiki whose `_meta` smart flag is `false`); this function does not
/// re-check the family.
///
/// # Errors
///
/// See [`CaptureBufferError`].
pub async fn buffer_capture(
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
) -> Result<BufferOutcome> {
    buffer_capture_with_source(
        pool,
        req,
        supersede_hint,
        "ingest",
        None,
        BufferStaging::default(),
    )
    .await
}

/// [`buffer_capture`] with the turn's staged vector and origin fingerprint —
/// the conversational entry point.
///
/// Kept separate from the bare [`buffer_capture`] so a caller that has neither
/// (a test, a path with no embedder in hand) is not forced to invent them, and
/// so the staged columns stay visibly optional: they are an optimisation and a
/// de-duplication hint, never part of what makes a capture valid.
///
/// # Errors
///
/// See [`CaptureBufferError`].
pub async fn buffer_capture_staged(
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
    staging: BufferStaging,
) -> Result<BufferOutcome> {
    buffer_capture_with_source(pool, req, supersede_hint, "ingest", None, staging).await
}

/// [`buffer_capture`] variant stamping the capture's source.
///
/// `source_kind` names the producer (`ingest` | `document` | …) and
/// `source_ref` carries the source-document provenance (catalog id / url)
/// that promotion copies onto `fact_index.source_ref`
/// (document ingest).
///
/// # Errors
///
/// See [`CaptureBufferError`].
pub async fn buffer_capture_with_source(
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
    source_kind: &str,
    source_ref: Option<String>,
    staging: BufferStaging,
) -> Result<BufferOutcome> {
    let CaptureRequest {
        // The plan's destination stops here. It is the live route's — the
        // `lista` item and the container the user asked for this turn, both of
        // which are written to their page inside the turn. A claim that waits
        // is a claim nobody has placed yet, and the light dream places it when
        // it reads the queue (see [`BufferedCapture`]).
        wiki_id: _,
        page: _,
        // Describes the PAGE, so it belongs to the page: the live route writes
        // it on the testata of the page it creates (`capture::seed_page_card`).
        // A claim that waits has no page to describe.
        page_description: _,
        body,
        subject,
        allow,
        sender,
        fact_type,
        topics,
        dedup_threshold: _,
        // Thread the per-fact validity interval through the standard-wiki
        // buffer→promote path (closing the gap). Staged on the buffer here,
        // copied into fact_index by promote_one (dream_light.rs).
        valid_from,
        valid_to,
        // How the claim should READ (never where it goes):
        // `page` field above); `style`/`page_description` are now staged
        // alongside it so promote_one copies the whole placement onto the fact
        // and the light cadence can settle it without re-running the Cartografo.
        style,
        // Per-fact salience, staged on the buffer so promote_one copies it onto
        // the fact.
        salience,
        // Group-17 provenance breadcrumbs threaded from the ingest turn.
        authored_refs,
    } = req;
    validate_buffer_body(&body)?;
    // Mirror capture.rs: sender is always materialized (= subject when
    // absent) and kept distinct from subject, so a later subject change never
    // rebinds the original provenance. NULL survives only as the degenerate
    // scrubbed state (e.g. a deleted user) that falls back to subject at read.
    let sender = sender.or_else(|| Some(subject.clone()));
    let capture_id = new_capture_id()?;
    let cap = BufferedCapture {
        capture_id: capture_id.clone(),
        body,
        subject,
        allow,
        sender,
        fact_type,
        topics,
        supersede_hint,
        status: CaptureStatus::Buffered,
        captured_at: chrono::Utc::now().to_rfc3339(),
        processed_at: None,
        resolved_fact_id: None,
        source_kind: source_kind.to_owned(),
        source_ref,
        valid_from,
        valid_to,
        decay_reason: None,
        style,
        salience,
        authored_refs,
        embedding: staging.embedding,
        origin_message_hash: staging.origin_message_hash,
        // A fresh capture has never been offered to a placement pass.
        placement_attempts: 0,
        last_attempt_at: None,
    };

    insert_row(pool, &cap).await?;
    tracing::info!(
        capture_id = %cap.capture_id,
        subject = %cap.subject,
        "capture_buffer: BUFFERED"
    );
    Ok(BufferOutcome { capture_id })
}

fn validate_buffer_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Err(CaptureBufferError::EmptyBody);
    }
    // `<!--` stays reserved unconditionally; braces are admitted only as
    // well-formed self-closing `{{embed=…}}` markers, mirroring
    // `capture::validate_body` (see media pipeline).
    if body.contains("<!--") {
        return Err(CaptureBufferError::BodyContainsReserved);
    }
    if (body.contains("{{") || body.contains("}}"))
        && crate::parser::embed_only_markers(body).is_none()
    {
        return Err(CaptureBufferError::BodyContainsReserved);
    }
    Ok(())
}

fn new_capture_id() -> Result<FactId> {
    let raw = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
    Ok(FactId::parse(&raw.to_string())?)
}

// ---------- read path ----------

/// Count of pending (buffered) captures across all wikis — backlog signal for
/// the light-dream threshold trigger.
///
/// # Errors
///
/// DB errors.
pub async fn count_buffered(pool: &SqlitePool) -> Result<i64> {
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM capture_buffer WHERE status = 'buffered'")
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// Every buffered capture across all wikis, **oldest first** (capped). The
/// light dream's global drain query — one pass per cycle rather than per wiki.
///
/// Oldest-first is the *drain's* order and only the drain's: a queue is served
/// from the front. Anything that **reads** the buffer to answer a question
/// about now wants [`find_recent_buffered`] instead — see the note there.
///
/// # Errors
///
/// DB or decode errors.
pub async fn find_all_buffered(pool: &SqlitePool, limit: i64) -> Result<Vec<BufferedCapture>> {
    let rows: Vec<BufferRow> = sqlx::query_as(&format!(
        "{SELECT_COLS} WHERE status = 'buffered' ORDER BY captured_at, capture_id LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode).collect()
}

/// Every buffered capture across all wikis, **newest first** (capped) — the
/// read side's selection query.
///
/// The twin of [`find_all_buffered`], and the difference is the whole point.
/// Where a cut list is shown to a model or an operator, **the order IS the
/// selection** (founder, 2026-08-09), so it has to be the axis the reader
/// cares about. Everything that reads this table reads it to answer *what was
/// just said and is not on a page yet* — the recall bridge, the reconciliation
/// stage, the dashboard's consolidating list. Serving those from the drain's
/// oldest-first order threw away exactly the rows they exist for, and only
/// once the buffer grew past the cap: invisible on a quiet deployment, wrong
/// on a busy one, and wrong hardest when the light dream is lagging — which is
/// precisely when the buffer matters most.
///
/// # Errors
///
/// DB or decode errors.
pub async fn find_recent_buffered(pool: &SqlitePool, limit: i64) -> Result<Vec<BufferedCapture>> {
    let rows: Vec<BufferRow> = sqlx::query_as(&format!(
        "{SELECT_COLS} WHERE status = 'buffered' ORDER BY captured_at DESC, capture_id DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode).collect()
}

// ---------- light-dream resolution ----------

/// Mark a buffered capture as **promoted**.
///
/// The light dream inserted a `fact_index` row whose `fact_id == capture_id`.
/// Idempotent — re-running over an already-promoted row is a harmless no-op (it
/// only advances `buffered` rows). `now` is the ISO-8601 `processed_at` stamp.
///
/// # Errors
///
/// DB errors.
pub async fn mark_promoted(pool: &SqlitePool, capture_id: &FactId, now: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET status = 'promoted', processed_at = ?, resolved_fact_id = capture_id
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(now)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Record that a placement pass read this claim and gave it no page.
///
/// The claim stays `buffered` — that is the whole point. A claim nobody placed
/// is on no page at all (founder, 2026-08-22: *«i fatti senza destinazione si
/// accumuleranno nella tabella buffer … se il giro orario non trova la
/// collocazione lo lascia lì, marcandolo come "già provato a collocare"»*), so
/// it waits for the next pass: the next hour's, then tonight's, which reads a
/// whole wiki at once with the strong model — and finally the closing pass,
/// which has to give it a page whatever the pile looks like.
///
/// Observational, and deliberately not a gate: nothing refuses a claim for
/// having been declined often. What the counter buys is the difference between
/// *never looked at* and *looked at and declined*, which without it would be
/// the same row.
///
/// # Errors
///
/// Underlying `sqlx` errors.
pub async fn mark_placement_attempted(
    pool: &SqlitePool,
    capture_id: &FactId,
    now: &str,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET placement_attempts = placement_attempts + 1, last_attempt_at = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(now)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Put an already-written fact back in the queue: copy it into the buffer
/// under its own id and drop its `fact_index` row.
///
/// What happens to a fact whose page goes away. The alternative — pick
/// another page for it here — is a placement decision, and placement is the
/// light dream's job against the memory as it stands, not a caller's guess at
/// delete time (founder, 2026-08-22: *«i fatti rimossi dalle pagine tornano
/// nella tabella in attesa di essere ridistribuiti»*).
///
/// The `fact_index` row is **hard-deleted**, not tombstoned: the buffer reuses
/// the fact's id as its `capture_id`, so promotion re-inserts under the same
/// primary key and a surviving tombstone would collide. The claim is not
/// forgotten — it is un-written, and the id it comes back under is the one it
/// left with, so every reference to it still resolves once it lands again.
///
/// The row's embedding rides along, so the queue does not pay to compute it a
/// second time. Returns `false` when `fact_id` names no live row (already
/// tombstoned, already re-buffered, never existed).
///
/// # Errors
///
/// DB errors, or a persisted column that no longer parses.
pub async fn rebuffer_fact(pool: &SqlitePool, fact_id: &FactId, now: &str) -> Result<bool> {
    let Some(row) = crate::fact_index::find_by_id(pool, fact_id).await? else {
        return Ok(false);
    };
    if row.deleted_at.is_some() {
        return Ok(false);
    }
    let allow_json = serde_json::to_string(
        &row.allow_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )?;
    let topics_json = serde_json::to_string(&row.topics)?;
    let authored_refs_json = serde_json::to_string(&row.authored_refs)?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO capture_buffer
            (capture_id, body, subject_id, allow_ids, sender_id, fact_type,
             topics, status, captured_at, source_kind, source_ref,
             valid_from, valid_to, decay_reason, style, salience,
             authored_refs, embedding, embedding_dim)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'buffered', ?, 'rebuffer', ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(capture_id) DO UPDATE SET
             status = 'buffered', processed_at = NULL, resolved_fact_id = NULL",
    )
    .bind(row.fact_id.as_str())
    .bind(&row.text)
    .bind(row.subject_id.to_string())
    .bind(allow_json)
    .bind(row.sender_id.as_ref().map(ToString::to_string))
    .bind(row.fact_type.clone())
    .bind(topics_json)
    .bind(now)
    .bind(row.source_ref.clone())
    .bind(row.valid_from.clone())
    .bind(row.valid_to.clone())
    .bind(row.decay_reason.clone())
    .bind(row.style.map(crate::wiki::PageStyle::as_str))
    .bind(row.salience.clone())
    .bind(authored_refs_json)
    .bind(crate::fact_index::encode_embedding(&row.embedding))
    .bind(i64::try_from(row.embedding.len()).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM fact_index WHERE fact_id = ?")
        .bind(fact_id.as_str())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(
        fact_id = fact_id.as_str(),
        subject = %row.subject_id,
        "capture_buffer: fact returned to the queue"
    );
    Ok(true)
}

/// Stamp the turn's recall-log row onto freshly buffered captures.
///
/// The linkage the promotion-time miss detector reads
/// ([`crate::recall_log`]). Best-effort telemetry: a row without a linkage
/// simply has none and detection skips it.
///
/// # Errors
///
/// DB errors.
pub async fn stamp_recall_log(
    pool: &SqlitePool,
    capture_ids: &[FactId],
    log_id: i64,
) -> Result<u64> {
    let mut stamped = 0;
    for id in capture_ids {
        let res = sqlx::query("UPDATE capture_buffer SET recall_log_id = ? WHERE capture_id = ?")
            .bind(log_id)
            .bind(id.as_str())
            .execute(pool)
            .await?;
        stamped += res.rows_affected();
    }
    Ok(stamped)
}

/// The recall-log linkage of one buffered capture, when its turn was
/// logged (`None`: pre-feature row, or unknown id).
///
/// # Errors
///
/// DB errors.
pub async fn recall_log_id(pool: &SqlitePool, capture_id: &FactId) -> Result<Option<i64>> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT recall_log_id FROM capture_buffer WHERE capture_id = ?")
            .bind(capture_id.as_str())
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(id,)| id))
}

/// Mark a buffered capture as **skipped (duplicate)**.
///
/// The light dream found an existing active fact carrying the same claim, so no
/// new fact was created and `resolved_fact_id` points at the survivor.
/// Idempotent.
///
/// # Errors
///
/// DB errors.
pub async fn mark_skipped_dup(
    pool: &SqlitePool,
    capture_id: &FactId,
    matched_fact_id: &FactId,
    now: &str,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET status = 'skipped_dup', processed_at = ?, resolved_fact_id = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(now)
    .bind(matched_fact_id.as_str())
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Close the staged validity window of a still-**buffered** capture:
/// stamp `valid_to` + `decay_reason` on the buffer row.
///
/// The buffered half of the closure verb — a closure gesture whose target
/// has not been promoted yet (the same-day flow: "buy the milk" →
/// "I bought the milk" within one buffer window). Promotion then
/// carries both onto the fact. Rows already `promoted`/`skipped_dup` are
/// left alone (their fact row is the closure target — the id is stable
/// across promotion, so the fact-side verb hits first).
///
/// Returns the previous staged values for the receipt's record of what
/// changed, or `None` when `capture_id` has no buffered row.
///
/// # Errors
///
/// DB errors.
pub async fn close_validity(
    pool: &SqlitePool,
    capture_id: &FactId,
    valid_to: &str,
    reason: &str,
) -> Result<Option<crate::fact_index::ClosedValidity>> {
    let prev: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT valid_to, decay_reason FROM capture_buffer
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(capture_id.as_str())
    .fetch_optional(pool)
    .await?;
    let Some((prev_valid_to, prev_decay_reason)) = prev else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE capture_buffer
            SET valid_to = ?, decay_reason = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(valid_to)
    .bind(reason)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(crate::fact_index::ClosedValidity {
        prev_valid_to,
        prev_decay_reason,
        // The buffer stages no successor pointer — closures land it on the
        // fact row only (the id is stable across promotion).
        prev_successor_fact_id: None,
    }))
}

/// Correct the staged validity *interval* of a still-**buffered** capture:
/// set `valid_from` and/or `valid_to` on the buffer row, **leaving
/// `decay_reason` untouched**.
///
/// The buffered half of the validity-edit verb — the fact-side
/// [`crate::fact_index::set_validity`] probes first; this catches a target
/// whose capture has not been promoted yet (the same-day flow). A
/// `Some(value)` SETS that bound, a `None` LEAVES it (COALESCE-in-Rust),
/// exactly like the fact-side write.
///
/// Returns the previous staged interval for the receipt's record of what
/// changed, or `None` when `capture_id` has no buffered row.
///
/// # Errors
///
/// DB errors.
pub async fn set_validity(
    pool: &SqlitePool,
    capture_id: &FactId,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> Result<Option<crate::fact_index::PrevValidity>> {
    let prev: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT valid_from, valid_to FROM capture_buffer
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(capture_id.as_str())
    .fetch_optional(pool)
    .await?;
    let Some((prev_valid_from, prev_valid_to)) = prev else {
        return Ok(None);
    };
    let new_from = valid_from.map_or_else(|| prev_valid_from.clone(), |v| Some(v.to_owned()));
    let new_to = valid_to.map_or_else(|| prev_valid_to.clone(), |v| Some(v.to_owned()));
    sqlx::query(
        "UPDATE capture_buffer
            SET valid_from = ?, valid_to = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(&new_from)
    .bind(&new_to)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(crate::fact_index::PrevValidity {
        prev_valid_from,
        prev_valid_to,
    }))
}

/// Replace the ACL columns of a still-**buffered** capture: set
/// `subject_id`, `allow_ids`, and `sender_id` on the buffer row.
///
/// The buffered half of the acl-change verb — the fact-side
/// [`crate::fact_index::set_acl`] probes first; this catches a target
/// whose capture has not been promoted yet. The buffer's `subject_id` is
/// NOT NULL and `allow_ids` defaults to `'[]'`, so both always carry a
/// value.
///
/// Returns the previous ACL for the receipt's record of what changed, or
/// `None` when `capture_id` has no buffered row.
///
/// # Errors
///
/// DB errors + JSON serialization failures on `allow_ids`.
pub async fn set_acl(
    pool: &SqlitePool,
    capture_id: &FactId,
    subject: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) -> Result<Option<crate::fact_index::PrevAcl>> {
    let prev: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT subject_id, allow_ids, sender_id FROM capture_buffer
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(capture_id.as_str())
    .fetch_optional(pool)
    .await?;
    let Some((prev_subject, prev_allow, prev_sender)) = prev else {
        return Ok(None);
    };
    let prev_subject_id = prev_subject.parse::<Principal>()?;
    let prev_allow_ids = principals_from_json(&prev_allow);
    let prev_sender_id = prev_sender
        .as_deref()
        .map(str::parse::<Principal>)
        .transpose()?;
    let allow_json = crate::fact_index::principals_to_json(allow)?;
    sqlx::query(
        "UPDATE capture_buffer
            SET subject_id = ?, allow_ids = ?, sender_id = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(subject.to_string())
    .bind(&allow_json)
    .bind(sender.map(ToString::to_string))
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(Some(crate::fact_index::PrevAcl {
        prev_subject_id,
        prev_allow_ids,
        prev_sender_id,
    }))
}

/// Replace **only** a buffered capture's `allow_ids`.
///
/// `subject_id` and `sender_id` are left untouched — the buffered twin of
/// [`crate::fact_index::inherit_allow`], for a successor the promoter has not
/// moved into the fact store yet. The capture id is stable across promotion,
/// so correcting the buffer row is correcting the fact.
///
/// Returns `false` when the capture is unknown or no longer `buffered`.
///
/// # Errors
///
/// `sqlx::Error` + JSON serialization failures on `allow_ids`.
pub async fn inherit_allow(
    pool: &SqlitePool,
    capture_id: &FactId,
    allow: &[Principal],
) -> Result<bool> {
    let allow_json = crate::fact_index::principals_to_json(allow)?;
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET allow_ids = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(&allow_json)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

// ---------- DB layer ----------

const SELECT_COLS: &str = "SELECT capture_id, body, subject_id, allow_ids, \
     sender_id, fact_type, topics, supersede_hint, status, captured_at, processed_at, \
     resolved_fact_id, source_kind, source_ref, valid_from, valid_to, decay_reason, style, \
     salience, authored_refs, embedding, origin_message_hash, placement_attempts, \
     last_attempt_at \
     FROM capture_buffer";

#[derive(sqlx::FromRow)]
struct BufferRow {
    capture_id: String,
    body: String,
    subject_id: String,
    allow_ids: String,
    sender_id: Option<String>,
    fact_type: Option<String>,
    topics: String,
    supersede_hint: Option<String>,
    status: String,
    captured_at: String,
    processed_at: Option<String>,
    resolved_fact_id: Option<String>,
    source_kind: String,
    source_ref: Option<String>,
    valid_from: Option<String>,
    valid_to: Option<String>,
    decay_reason: Option<String>,
    style: Option<String>,
    salience: Option<String>,
    authored_refs: String,
    embedding: Option<Vec<u8>>,
    origin_message_hash: Option<String>,
    placement_attempts: i64,
    last_attempt_at: Option<String>,
}

fn decode(r: BufferRow) -> Result<BufferedCapture> {
    let supersede_hint = r.supersede_hint.as_deref().map(FactId::parse).transpose()?;
    let resolved_fact_id = r
        .resolved_fact_id
        .as_deref()
        .map(FactId::parse)
        .transpose()?;
    let sender = r
        .sender_id
        .as_deref()
        .map(str::parse::<Principal>)
        .transpose()?;
    Ok(BufferedCapture {
        capture_id: FactId::parse(&r.capture_id)?,
        body: r.body,
        subject: r.subject_id.parse::<Principal>()?,
        allow: principals_from_json(&r.allow_ids),
        sender,
        fact_type: r.fact_type,
        topics: serde_json::from_str(&r.topics).unwrap_or_default(),
        supersede_hint,
        status: CaptureStatus::from_db(&r.status),
        captured_at: r.captured_at,
        processed_at: r.processed_at,
        resolved_fact_id,
        source_kind: r.source_kind,
        source_ref: r.source_ref,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
        decay_reason: r.decay_reason,
        style: crate::wiki::PageStyle::parse_lenient(r.style.as_deref()),
        salience: r.salience,
        authored_refs: serde_json::from_str(&r.authored_refs).unwrap_or_default(),
        // A blob that does not decode is treated as absent, not as an error:
        // the vector is derivable, both readers recompute when it is `None`,
        // and refusing to read the row would strand a capture over a column
        // that exists only to save work.
        embedding: r.embedding.as_deref().and_then(|b| {
            crate::fact_index::decode_embedding(b)
                .inspect_err(|e| {
                    tracing::warn!(error = %e, "capture_buffer: unreadable staged embedding — recomputing");
                })
                .ok()
        }),
        origin_message_hash: r.origin_message_hash,
        placement_attempts: r.placement_attempts,
        last_attempt_at: r.last_attempt_at,
    })
}

async fn insert_row(pool: &SqlitePool, cap: &BufferedCapture) -> Result<u64> {
    let allow_json = serde_json::to_string(
        &cap.allow
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )?;
    let topics_json = serde_json::to_string(&cap.topics)?;
    let authored_refs_json = serde_json::to_string(&cap.authored_refs)?;
    let res = sqlx::query(
        "INSERT INTO capture_buffer
            (capture_id, body, subject_id, allow_ids, sender_id, fact_type,
             topics, supersede_hint, status, captured_at, processed_at, resolved_fact_id,
             source_kind, source_ref, valid_from, valid_to, decay_reason, style,
             salience, authored_refs, embedding, embedding_dim,
             origin_message_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(capture_id) DO NOTHING",
    )
    .bind(cap.capture_id.as_str())
    .bind(&cap.body)
    .bind(cap.subject.to_string())
    .bind(allow_json)
    .bind(cap.sender.as_ref().map(ToString::to_string))
    .bind(cap.fact_type.clone())
    .bind(topics_json)
    .bind(cap.supersede_hint.as_ref().map(|f| f.as_str().to_owned()))
    .bind(cap.status.as_str())
    .bind(&cap.captured_at)
    .bind(cap.processed_at.clone())
    .bind(cap.resolved_fact_id.as_ref().map(|f| f.as_str().to_owned()))
    .bind(&cap.source_kind)
    .bind(cap.source_ref.clone())
    .bind(cap.valid_from.clone())
    .bind(cap.valid_to.clone())
    .bind(cap.decay_reason.clone())
    .bind(cap.style.map(crate::wiki::PageStyle::as_str))
    .bind(cap.salience.clone())
    .bind(authored_refs_json)
    // `embedding_dim` rides alongside the blob purely as the same
    // self-description `fact_index` carries: the blob's own length already
    // gives the dimension, and nothing reads the column, but a stored vector
    // that cannot say how long it is has bitten this codebase before.
    .bind(
        cap.embedding
            .as_deref()
            .map(crate::fact_index::encode_embedding),
    )
    .bind(
        cap.embedding
            .as_ref()
            .map(|v| i64::try_from(v.len()).unwrap_or(i64::MAX)),
    )
    .bind(cap.origin_message_hash.clone())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

fn principals_from_json(s: &str) -> Vec<Principal> {
    serde_json::from_str::<Vec<String>>(s)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.parse::<Principal>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::types::WikiId;
    use sqlx::SqlitePool;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        write_wiki(&wikis, "alice", "wiki-user");
        (dir, pool)
    }

    fn write_wiki(wikis_dir: &Path, slug: &str, wiki_type: &str) {
        let d = wikis_dir.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        let fm = format!(
            "---\nwiki_id: {slug}\nwiki_type: {wiki_type}\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
        );
        std::fs::write(d.join("_meta.md"), fm).unwrap();
        std::fs::write(d.join("cucina.md"), "# index\n").unwrap();
    }

    /// A capture request. `wiki_id`/`page` are the LIVE route's fields (the
    /// struct is shared with [`crate::capture::capture_fact`]); the buffer
    /// drops them, which is what `no_destination_reaches_the_row` checks.
    fn req(wiki: &str, body: &str, subject: &str) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: body.to_owned(),
            subject: subject.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("bio".to_owned()),
            topics: vec!["a".to_owned(), "b".to_owned()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        }
    }

    /// A capture lands in the table, and **writes no file**: the published
    /// `.md` is the compiler's output, and it is the only file a claim ever
    /// reaches (2026-08-18).
    #[tokio::test]
    async fn buffer_capture_writes_the_row_and_no_file() {
        let (dir, pool) = setup().await;
        let before: Vec<_> = std::fs::read_dir(dir.path().join("wikis/alice"))
            .expect("read wiki dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        let out = buffer_capture(
            &pool,
            req("alice", "Alice loves pasta.", "user:alice"),
            None,
        )
        .await
        .expect("buffer");

        // Nothing new on disk — not the retired journal, not anything else.
        let after: Vec<_> = std::fs::read_dir(dir.path().join("wikis/alice"))
            .expect("read wiki dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            before, after,
            "buffering must write no file at all — the row IS the capture"
        );

        // The row is the capture.
        let buffered = find_all_buffered(&pool, 100).await.expect("find");
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].capture_id, out.capture_id);
        assert_eq!(buffered[0].body, "Alice loves pasta.");
        assert_eq!(
            buffered[0].subject,
            "user:alice".parse::<Principal>().unwrap()
        );
        assert_eq!(buffered[0].fact_type.as_deref(), Some("bio"));
        assert_eq!(buffered[0].topics, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(count_buffered(&pool).await.unwrap(), 1);
    }

    /// **A claim waiting in the buffer says nothing about where it will go.**
    /// The request the plan hands over still carries a wiki and a page — it is
    /// the struct the live route shares — and both stop at this boundary:
    /// where a claim goes is decided when the light dream reads the queue
    /// (founder, 2026-08-18). Written against the columns, because the field
    /// being gone from the struct is exactly what a stale row could contradict.
    #[tokio::test]
    async fn no_destination_reaches_the_row() {
        let (_dir, pool) = setup().await;
        buffer_capture(
            &pool,
            req("alice", "Alice loves pasta.", "user:alice"),
            None,
        )
        .await
        .expect("buffer");
        let cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('capture_buffer')")
                .fetch_all(&pool)
                .await
                .expect("pragma");
        for gone in ["wiki_id", "target_page"] {
            assert!(
                !cols.iter().any(|c| c == gone),
                "`{gone}` must not exist on capture_buffer — a waiting claim has no destination; columns: {cols:?}"
            );
        }
    }

    #[test]
    fn origin_fingerprint_ignores_surrounding_whitespace_only() {
        // The two sides ever compared are the same string arriving by two
        // routes, so trimming is the whole of the normalisation.
        let a = origin_fingerprint("  Ho comprato il latte.\n");
        assert_eq!(a, origin_fingerprint("Ho comprato il latte."));
        // Anything beyond that stays a different message: widening the match
        // would silently drop a fact from recall on a collision.
        assert_ne!(a, origin_fingerprint("ho comprato il latte."));
        assert_ne!(a, origin_fingerprint("Ho  comprato il latte."));
        assert_ne!(a, origin_fingerprint("Ho comprato il pane."));
    }

    #[tokio::test]
    async fn rejects_marker_and_comment_bodies() {
        let (_dir, pool) = setup().await;
        assert!(matches!(
            buffer_capture(&pool, req("alice", "has {{marker}}", "user:alice"), None).await,
            Err(CaptureBufferError::BodyContainsReserved)
        ));
        assert!(matches!(
            buffer_capture(&pool, req("alice", "has <!-- comment", "user:alice"), None).await,
            Err(CaptureBufferError::BodyContainsReserved)
        ));
        assert!(matches!(
            buffer_capture(&pool, req("alice", "   ", "user:alice"), None).await,
            Err(CaptureBufferError::EmptyBody)
        ));
    }

    #[tokio::test]
    async fn sender_equal_subject_is_materialized() {
        let (_dir, pool) = setup().await;
        let mut r = req("alice", "self note", "user:alice");
        r.sender = Some("user:alice".parse::<Principal>().unwrap());
        buffer_capture(&pool, r, None).await.unwrap();
        let buffered = find_all_buffered(&pool, 100).await.unwrap();
        assert_eq!(
            buffered[0].sender,
            Some("user:alice".parse::<Principal>().unwrap()),
            "sender must stay materialized (= subject), never collapsed to None"
        );
    }
}
