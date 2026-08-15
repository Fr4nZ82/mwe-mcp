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
//! ## Captures-journal invariant
//!
//! The durable source of truth for a buffered capture is the per-wiki on-disk
//! journal `<wiki_dir>/_captures.md` ([`crate::wiki::CAPTURES_FILENAME`]). The
//! `capture_buffer` DB table is a *rebuildable cache*: `rm engine.db` followed
//! by [`reindex_capture_journal`] (invoked from [`crate::reindex::reindex_full`])
//! regenerates every row. The journal is excluded from `list_pages` and the
//! marker reindex sweep so its entries are never mistaken for published facts.
//!
//! ## Id stability
//!
//! Each capture is minted a `UUIDv7` `capture_id` at buffer time and that id is
//! reused verbatim as the `fact_id` when the light dream promotes it. A claim
//! therefore keeps one stable id across buffer → fact → compiled-page — the
//! correctness hinge for incremental compilation (fingerprints key on
//! `fact_id`s).

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture::CaptureRequest;
use crate::embedder::Embedder;
use crate::types::{
    FactId, FactIdParseError, Principal, PrincipalParseError, WikiId, WikiIdParseError,
};
use crate::wiki::{
    CAPTURES_FILENAME, WikiError, WikiTree, atomic_write, workdir_relative_source_path,
};

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
    /// Body submitted was empty or whitespace-only.
    #[error("capture body is empty")]
    EmptyBody,
    /// Body contained a reserved sequence. Captured claims are plain prose; the
    /// `{{…}}` marker grammar and the journal's HTML-comment delimiters are
    /// managed by mwe-mcp, so a body may not contain them.
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

    /// Decode from the DB/journal string; unknown values fall back to `Buffered`.
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
// `PartialEq` only: the staged embedding is `Vec<f32>`, and floats have no
// total equality. Nothing compares captures for `Eq` — the derive was free
// until a vector landed on the struct.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferedCapture {
    /// `UUIDv7`; reused verbatim as the `fact_id` on promotion.
    pub capture_id: FactId,
    /// Wiki the capture belongs to (its journal lives at `<wiki>/_captures.md`).
    pub wiki_id: WikiId,
    /// Page the classifier proposed (a compiler hint, not a hard target).
    pub target_page: PathBuf,
    /// Captured claim prose, verbatim, no markers.
    pub body: String,
    /// Owning principal.
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
    /// together with `valid_to`; promotion stamps it onto the fact. DB-only
    /// post-capture mutation — never written to the journal, same durability
    /// class as `status`/`processed_at`.
    pub decay_reason: Option<String>,
    /// The ingest classifier's proposed page writing style (closed palette
    /// `prosa` | `prosa-tecnica` | `lista`), staged here so
    /// [`promote_one`](crate::dream_light) copies it onto the fact
    /// (`fact_index.style`), where the light cadence seeds a new page's
    /// testata from it. Travels with [`Self::target_page`] — one placement axis,
    /// sibling of the validity axis above. `None` = the classifier proposed
    /// nothing. (Whitespace-free enum → rides the journal codec as a bare attr.)
    pub style: Option<String>,
    /// The classifier's proposed "cosa ci va dentro" one-liner that seeds
    /// the page's testata description. `None` = unproposed. Free text, so the
    /// whitespace-delimited journal codec percent-escapes it. See [`Self::style`].
    pub page_description: Option<String>,
    /// Per-fact salience the classifier deduced, staged here so
    /// [`promote_one`](crate::dream_light) copies it onto the fact
    /// (`fact_index.salience`), where `high` facts are routed to the
    /// actor-wiki `index.md`. `high | normal | low`; `None` = unspecified.
    /// Whitespace-free enum → rides the journal codec as the bare `sal=` attr,
    /// like vf=/vt=.
    pub salience: Option<String>,
    /// Project-wiki pages this turn authored, as plain `[[wiki_id/page]]`
    /// wikilinks ([`crate::fact_index::NewFact::authored_refs`]), staged here
    /// so [`promote_one`](crate::dream_light) copies them onto the fact and
    /// consolidation links instead of duplicating (roadmap group 17). Rides
    /// the journal codec as the comma-joined `aref=` attr, like `topics=`.
    /// Empty for a pure-standard capture.
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
    /// `None` is a first-class state, not a fault to repair eagerly. A row
    /// recovered by reindexing the journal has no vector (a blob has no place
    /// in a human-readable journal, and this one is derivable from the body),
    /// and a transient embedder fault at buffer time must not cost the
    /// capture. Both readers fall back to computing it — which is exactly the
    /// pre-existing behaviour. DB-only, like `status` / `processed_at` /
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
    /// buffer must not become a second transcript. It **is** mirrored in the
    /// journal (`omsg=`) — unlike the vector it cannot be recomputed from
    /// anything the row holds, so a reindex would lose it for good.
    pub origin_message_hash: Option<String>,
}

/// Fingerprint of an originating message, for
/// [`BufferedCapture::origin_message_hash`].
///
/// SHA-256 of the trimmed text, hex: stable across processes and releases
/// (which `DefaultHasher` is not), whitespace-free so it rides the journal's
/// attribute list as a bare token, and fixed-width whatever the message.
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
    /// Workdir-relative path of the journal the entry landed in.
    pub journal_path: String,
}

// ---------- write path ----------

/// Buffer a classified capture for a **standard** wiki.
///
/// Appends it to the per-wiki `_captures.md` journal (the durable SSOT) and
/// indexes it in the `capture_buffer` table. The published `.md` is left
/// untouched — the nightly compiler produces it.
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
    tree: &WikiTree,
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
) -> Result<BufferOutcome> {
    buffer_capture_with_source(
        tree,
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
    tree: &WikiTree,
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
    staging: BufferStaging,
) -> Result<BufferOutcome> {
    buffer_capture_with_source(tree, pool, req, supersede_hint, "ingest", None, staging).await
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
    tree: &WikiTree,
    pool: &SqlitePool,
    req: CaptureRequest,
    supersede_hint: Option<FactId>,
    source_kind: &str,
    source_ref: Option<String>,
    staging: BufferStaging,
) -> Result<BufferOutcome> {
    let CaptureRequest {
        wiki_id,
        page,
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
        // The placement/style axis. `target_page` already rides the buffer (the
        // `page` field above); `style`/`page_description` are now staged
        // alongside it so promote_one copies the whole placement onto the fact
        // and the light cadence can settle it without re-running the Cartografo.
        // page_description's free text is percent-escaped by the journal codec;
        // style is a whitespace-free enum.
        style,
        page_description,
        // Per-fact salience, staged on the buffer so promote_one copies it onto
        // the fact. Whitespace-free enum, rides the journal codec as the `sal=`
        // attr.
        salience,
        // Group-17 provenance breadcrumbs threaded from the ingest turn.
        authored_refs,
    } = req;
    validate_buffer_body(&body)?;
    let handle = tree.locate(&wiki_id)?;
    // Mirror capture.rs: sender is always materialized (= subject when
    // absent) and kept distinct from subject, so a later subject change never
    // rebinds the original provenance. NULL survives only as the degenerate
    // scrubbed state (e.g. a deleted user) that falls back to subject at read.
    let sender = sender.or_else(|| Some(subject.clone()));
    let capture_id = new_capture_id()?;
    let cap = BufferedCapture {
        capture_id: capture_id.clone(),
        wiki_id,
        target_page: page,
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
        page_description,
        salience,
        authored_refs,
        embedding: staging.embedding,
        origin_message_hash: staging.origin_message_hash,
    };

    let journal_abs = handle.abs_dir().join(CAPTURES_FILENAME);
    append_entry(&journal_abs, &cap)?;
    insert_row(pool, &cap).await?;
    let journal_path = workdir_relative_source_path(tree.workdir(), &journal_abs);
    tracing::info!(
        wiki_id = %cap.wiki_id,
        capture_id = %cap.capture_id,
        page = %cap.target_page.display(),
        subject = %cap.subject,
        "capture_buffer: BUFFERED"
    );
    Ok(BufferOutcome {
        capture_id,
        journal_path,
    })
}

fn validate_buffer_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Err(CaptureBufferError::EmptyBody);
    }
    // `<!--` stays reserved unconditionally (the journal's entry
    // delimiters); braces are admitted only as well-formed self-closing
    // `{{embed=…}}` markers, mirroring `capture::validate_body` (see
    // media pipeline).
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

/// Every buffered (not-yet-promoted) capture for a wiki, oldest first. The
/// light dream's drain query.
///
/// # Errors
///
/// DB or decode errors.
pub async fn find_buffered_in_wiki(
    pool: &SqlitePool,
    wiki_id: &str,
) -> Result<Vec<BufferedCapture>> {
    let rows: Vec<BufferRow> = sqlx::query_as(&format!(
        "{SELECT_COLS} WHERE wiki_id = ? AND status = 'buffered' ORDER BY captured_at, capture_id"
    ))
    .bind(wiki_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode).collect()
}

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

/// Stamp the turn's recall-log row onto freshly buffered captures.
///
/// The linkage the promotion-time miss detector reads
/// ([`crate::recall_log`]). DB-only, best-effort telemetry: the column
/// never rides the journal codec, so a journal-recovered row simply has
/// no linkage and detection skips it.
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
/// logged (`None`: pre-feature row, journal-recovered row, or unknown id).
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
/// Returns the previous staged values for the receipt's revert payload,
/// or `None` when `capture_id` has no buffered row.
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

/// Restore a buffered capture's staged validity from a closure snapshot —
/// the revert half of [`close_validity`].
///
/// Returns the number of rows touched (0 when the row was promoted in the
/// meantime — the fact-side restore covers it, the id being stable).
///
/// # Errors
///
/// DB errors.
pub async fn restore_validity(
    pool: &SqlitePool,
    capture_id: &FactId,
    prev_valid_to: Option<&str>,
    prev_decay_reason: Option<&str>,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET valid_to = ?, decay_reason = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(prev_valid_to)
    .bind(prev_decay_reason)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
/// Returns the previous staged interval for the receipt's revert payload,
/// or `None` when `capture_id` has no buffered row.
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

/// Restore a buffered capture's staged validity *interval* from a
/// [`crate::fact_index::PrevValidity`] snapshot — the revert half of
/// [`set_validity`]. Sets BOTH bounds back.
///
/// Returns the number of rows touched (0 when the row was promoted in the
/// meantime — the fact-side restore covers it, the id being stable).
///
/// # Errors
///
/// DB errors.
pub async fn restore_validity_interval(
    pool: &SqlitePool,
    capture_id: &FactId,
    prev_valid_from: Option<&str>,
    prev_valid_to: Option<&str>,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE capture_buffer
            SET valid_from = ?, valid_to = ?
          WHERE capture_id = ? AND status = 'buffered'",
    )
    .bind(prev_valid_from)
    .bind(prev_valid_to)
    .bind(capture_id.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
/// Returns the previous ACL for the receipt's revert payload, or `None`
/// when `capture_id` has no buffered row.
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

/// Restore a buffered capture's ACL columns from a
/// [`crate::fact_index::PrevAcl`] snapshot — the revert half of
/// [`set_acl`].
///
/// Returns the number of rows touched (0 when the row was promoted in the
/// meantime — the fact-side restore covers it, the id being stable).
///
/// # Errors
///
/// DB errors + JSON serialization failures on `allow_ids`.
pub async fn restore_acl(
    pool: &SqlitePool,
    capture_id: &FactId,
    subject: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) -> Result<u64> {
    let allow_json = crate::fact_index::principals_to_json(allow)?;
    let res = sqlx::query(
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
    Ok(res.rows_affected())
}

// ---------- captures-journal rebuild ----------

/// Rebuild the `capture_buffer` index for one wiki from its durable journal.
///
/// Reads `<wiki>/_captures.md` and upserts each entry. Idempotent
/// (`ON CONFLICT(capture_id) DO NOTHING`), so a journal entry whose row already
/// exists is left untouched. Returns the number of rows freshly inserted; a
/// missing journal is a no-op.
///
/// # Errors
///
/// IO or DB errors. Malformed individual entries are skipped, not fatal.
pub async fn reindex_capture_journal(
    pool: &SqlitePool,
    wiki_id: &WikiId,
    wiki_abs_dir: &Path,
) -> Result<usize> {
    let journal = wiki_abs_dir.join(CAPTURES_FILENAME);
    let raw = match std::fs::read_to_string(&journal) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut inserted = 0usize;
    for cap in parse_journal(&raw, wiki_id) {
        if insert_row(pool, &cap).await? > 0 {
            inserted += 1;
        }
    }
    Ok(inserted)
}

// ---------- DB layer ----------

const SELECT_COLS: &str = "SELECT capture_id, wiki_id, target_page, body, subject_id, allow_ids, \
     sender_id, fact_type, topics, supersede_hint, status, captured_at, processed_at, \
     resolved_fact_id, source_kind, source_ref, valid_from, valid_to, decay_reason, style, \
     page_description, salience, authored_refs, embedding, origin_message_hash \
     FROM capture_buffer";

#[derive(sqlx::FromRow)]
struct BufferRow {
    capture_id: String,
    wiki_id: String,
    target_page: String,
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
    page_description: Option<String>,
    salience: Option<String>,
    authored_refs: String,
    embedding: Option<Vec<u8>>,
    origin_message_hash: Option<String>,
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
        wiki_id: WikiId::parse(&r.wiki_id)?,
        target_page: PathBuf::from(r.target_page),
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
        style: r.style,
        page_description: r.page_description,
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
            (capture_id, wiki_id, target_page, body, subject_id, allow_ids, sender_id, fact_type,
             topics, supersede_hint, status, captured_at, processed_at, resolved_fact_id,
             source_kind, source_ref, valid_from, valid_to, decay_reason, style,
             page_description, salience, authored_refs, embedding, embedding_dim,
             origin_message_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(capture_id) DO NOTHING",
    )
    .bind(cap.capture_id.as_str())
    .bind(cap.wiki_id.as_str())
    .bind(cap.target_page.to_string_lossy().as_ref())
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
    .bind(cap.style.clone())
    .bind(cap.page_description.clone())
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

// ---------- journal codec ----------

fn journal_header(wiki_id: &WikiId) -> String {
    format!("---\nkind: capture_journal\nwiki_id: {wiki_id}\n---\n")
}

fn append_entry(journal_abs: &Path, cap: &BufferedCapture) -> Result<()> {
    let mut out = match std::fs::read_to_string(journal_abs) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => journal_header(&cap.wiki_id),
        Err(e) => return Err(e.into()),
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&render_entry(cap));
    atomic_write(journal_abs, out.as_bytes())?;
    Ok(())
}

fn render_entry(cap: &BufferedCapture) -> String {
    let allow_csv = cap
        .allow
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let topics_csv = cap.topics.join(",");
    let sender = cap
        .sender
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let sup = cap
        .supersede_hint
        .as_ref()
        .map(|f| f.as_str().to_owned())
        .unwrap_or_default();
    let ft = cap.fact_type.clone().unwrap_or_default();
    let page = cap.target_page.to_string_lossy();
    // Per-fact validity. ISO-8601 is whitespace-free so it is safe in this
    // whitespace-delimited attribute list; empty = open/unknown.
    let vf = cap.valid_from.clone().unwrap_or_default();
    let vt = cap.valid_to.clone().unwrap_or_default();
    // The placement style axis. `style` is a whitespace-free enum (bare attr,
    // like vf/vt); `desc` is free text, percent-escaped so it stays a single
    // token in this whitespace-delimited list.
    let style = cap.style.clone().unwrap_or_default();
    let desc = cap
        .page_description
        .as_deref()
        .map(enc_attr)
        .unwrap_or_default();
    // Per-fact salience. Whitespace-free enum (bare attr, like vf/vt/style);
    // empty = unspecified.
    let sal = cap.salience.clone().unwrap_or_default();
    // Capture source. `src` is a bare producer token (`ingest` | `document`);
    // `sref` is the source-document provenance (catalog id / url) —
    // percent-escaped like `desc` since a url may carry arbitrary characters.
    let src = cap.source_kind.clone();
    let sref = cap.source_ref.as_deref().map(enc_attr).unwrap_or_default();
    // Group-17 provenance breadcrumbs. Comma-joined like `topics` — each ref
    // is a whitespace-free `[[wiki_id/page]]` wikilink with no comma, so it
    // stays one token in this whitespace-delimited attr list.
    let aref_csv = cap.authored_refs.join(",");
    // The origin fingerprint. Hex, so a bare attr like vf/vt/style/sal. It
    // MUST ride the journal: unlike the staged embedding beside it — omitted
    // on purpose, being a binary blob and recomputable from the body — this
    // one cannot be derived from anything the entry holds, so a `rm engine.db`
    // + reindex would lose it permanently.
    let omsg = cap.origin_message_hash.clone().unwrap_or_default();
    format!(
        "<!-- mwe-capture id={id} ts={ts} page={page} type={ft} status={status} \
         subject={subject} allow={allow_csv} sender={sender} sup={sup} topics={topics_csv} \
         vf={vf} vt={vt} style={style} desc={desc} sal={sal} src={src} sref={sref} \
         aref={aref_csv} omsg={omsg} -->\n\
         {body}\n\
         <!-- /mwe-capture -->\n",
        id = cap.capture_id,
        ts = cap.captured_at,
        status = cap.status.as_str(),
        subject = cap.subject,
        body = cap.body,
    )
}

/// Percent-escape a free-text attribute value (`page_description`) so
/// it survives the whitespace-delimited journal attr list as a single token.
/// Only `%` and ASCII whitespace are escaped — enough to keep it one token while
/// staying human-readable; UTF-8 letters pass through untouched. [`dec_attr`]
/// reverses it.
fn enc_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

/// Inverse of [`enc_attr`]: decode `%XX` (XX = ASCII hex) back to its byte. Only
/// the ASCII escapes `enc_attr` emits occur, so `byte as char` is well-defined;
/// a malformed escape is kept verbatim.
fn dec_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.clone().take(2).collect();
            if hex.len() == 2
                && let Ok(byte) = u8::from_str_radix(&hex, 16)
            {
                out.push(byte as char);
                chars.next();
                chars.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn parse_journal(text: &str, wiki_id: &WikiId) -> Vec<BufferedCapture> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("<!-- mwe-capture ") else {
            continue;
        };
        let Some(attrs) = rest.strip_suffix(" -->") else {
            continue;
        };
        let mut body_lines: Vec<&str> = Vec::new();
        let mut closed = false;
        for bl in lines.by_ref() {
            if bl.trim() == "<!-- /mwe-capture -->" {
                closed = true;
                break;
            }
            body_lines.push(bl);
        }
        if !closed {
            break;
        }
        if let Some(cap) = parse_entry(attrs, &body_lines.join("\n"), wiki_id) {
            out.push(cap);
        }
    }
    out
}

fn parse_entry(attrs: &str, body: &str, wiki_id: &WikiId) -> Option<BufferedCapture> {
    let mut map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for tok in attrs.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            map.insert(k, v);
        }
    }
    let capture_id = FactId::parse(map.get("id").copied()?).ok()?;
    // `owner=` is the pre-rename spelling, read forever and never written.
    // This key is required — the `?` drops the WHOLE entry when it is missing,
    // silently, with no warning and no counter — and this journal is the
    // durable source of truth a `rm engine.db` rebuild replays, holding entries
    // from every version the deployment has ever run. Dropping the alias would
    // not raise an error, it would report a clean rebuild that lost facts.
    let subject = map
        .get("subject")
        .or_else(|| map.get("owner"))
        .copied()?
        .parse::<Principal>()
        .ok()?;
    let allow = map
        .get("allow")
        .copied()
        .map(split_principals)
        .unwrap_or_default();
    let sender = match map.get("sender").copied() {
        Some(s) if !s.is_empty() => Some(s.parse::<Principal>().ok()?),
        _ => None,
    };
    let supersede_hint = match map.get("sup").copied() {
        Some(s) if !s.is_empty() => Some(FactId::parse(s).ok()?),
        _ => None,
    };
    // Every optional attribute reads the same way — present and non-empty, or
    // absent — so it is read the same way here. An attribute missing entirely
    // is an OLDER journal, one written before that attribute existed, and the
    // codec must keep parsing those: the journal is the durable source of
    // truth a `rm engine.db` rebuilds from, and it holds entries from every
    // version the deployment has ever run.
    let attr = |k: &str| map.get(k).copied().filter(|s| !s.is_empty());
    let fact_type = attr("type").map(str::to_owned);
    let csv = |k: &str| {
        attr(k)
            .map(|s| s.split(',').map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let topics = csv("topics");
    // Per-fact validity (vf/vt): absent or empty → None (open/unknown).
    let valid_from = attr("vf").map(str::to_owned);
    let valid_to = attr("vt").map(str::to_owned);
    // Placement style axis. `style` is a bare enum token; `desc` is
    // percent-escaped free text → decode it.
    let style = attr("style").map(str::to_owned);
    let page_description = attr("desc").map(dec_attr);
    // Per-fact salience (sal). Bare enum token; absent → None (unspecified).
    let salience = attr("sal").map(str::to_owned);
    // Capture source (src/sref). Absent → the historical default `ingest` with
    // no provenance; `sref` is percent-escaped.
    let source_kind = attr("src").unwrap_or("ingest").to_owned();
    let source_ref = attr("sref").map(dec_attr);
    // Group-17 provenance breadcrumbs (aref), comma-split like `topics`.
    let authored_refs = csv("aref");
    // Origin fingerprint (omsg). Absent → None: the capture simply never gets
    // suppressed as already-in-context, which is the pre-existing behaviour and
    // the safe direction — showing a fact twice costs characters, hiding one
    // costs the fact.
    let origin_message_hash = attr("omsg").map(str::to_owned);
    Some(BufferedCapture {
        capture_id,
        wiki_id: wiki_id.clone(),
        target_page: PathBuf::from(map.get("page").copied().unwrap_or("index.md")),
        body: body.to_owned(),
        subject,
        allow,
        sender,
        fact_type,
        topics,
        supersede_hint,
        status: CaptureStatus::from_db(map.get("status").copied().unwrap_or("buffered")),
        captured_at: map.get("ts").copied().unwrap_or("").to_owned(),
        processed_at: None,
        resolved_fact_id: None,
        source_kind,
        source_ref,
        valid_from,
        valid_to,
        // Post-capture mutation, never journalled (see the field doc) — a
        // rebuilt row starts alive; ON CONFLICT keeps an existing closed row.
        decay_reason: None,
        style,
        page_description,
        salience,
        authored_refs,
        // Derivable from the body, so deliberately absent from the journal —
        // a rebuilt row pays one embedding at its next read or at promotion,
        // exactly as every row did before the column existed.
        embedding: None,
        origin_message_hash,
    })
}

fn split_principals(s: &str) -> Vec<Principal> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse::<Principal>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use sqlx::SqlitePool;
    use std::path::Path;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        write_wiki(&wikis, "alice", "wiki-user");
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    fn write_wiki(wikis_dir: &Path, slug: &str, wiki_type: &str) {
        let d = wikis_dir.join(slug);
        std::fs::create_dir_all(&d).unwrap();
        let fm = format!(
            "---\nwiki_id: {slug}\nwiki_type: {wiki_type}\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:{slug}'\n---\n"
        );
        std::fs::write(d.join("_meta.md"), fm).unwrap();
        std::fs::write(d.join("index.md"), "# index\n").unwrap();
    }

    fn req(wiki: &str, body: &str, subject: &str) -> CaptureRequest {
        CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from("index.md"),
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

    #[tokio::test]
    async fn buffer_capture_writes_journal_and_index() {
        let (dir, tree, pool) = setup().await;
        let out = buffer_capture(
            &tree,
            &pool,
            req("alice", "Alice loves pasta.", "user:alice"),
            None,
        )
        .await
        .expect("buffer");

        // Journal exists on disk with the entry.
        let journal = dir.path().join("wikis/alice/_captures.md");
        let raw = std::fs::read_to_string(&journal).expect("journal");
        assert!(raw.contains("kind: capture_journal"));
        assert!(raw.contains("Alice loves pasta."));
        assert!(raw.contains(out.capture_id.as_str()));

        // DB index has the buffered row.
        let buffered = find_buffered_in_wiki(&pool, "alice").await.expect("find");
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

    #[tokio::test]
    async fn journal_round_trips_through_codec() {
        let cap = BufferedCapture {
            authored_refs: Vec::new(),
            // The vector is deliberately NOT journalled, so a round-trip
            // through the codec must come back without it; the fingerprint is,
            // so it must come back intact.
            embedding: Some(vec![0.25, -0.5]),
            origin_message_hash: Some(origin_fingerprint("  Cena con i Brandibuck venerdì?  ")),
            capture_id: new_capture_id().unwrap(),
            wiki_id: WikiId::parse("famiglia").unwrap(),
            target_page: PathBuf::from("recipes/pasta.md"),
            body: "Cena con i Brandibuck venerdì.\nSeconda riga.".to_owned(),
            subject: "group:famiglia".parse::<Principal>().unwrap(),
            allow: vec!["user:bob".parse::<Principal>().unwrap()],
            sender: Some("user:alice".parse::<Principal>().unwrap()),
            fact_type: Some("plan".to_owned()),
            topics: vec!["dinner".to_owned()],
            supersede_hint: Some(new_capture_id().unwrap()),
            status: CaptureStatus::Buffered,
            captured_at: "2026-05-31T10:00:00+00:00".to_owned(),
            processed_at: None,
            resolved_fact_id: None,
            source_kind: "ingest".to_owned(),
            source_ref: None,
            // A finite horizon must survive the journal codec.
            valid_from: Some("2026-05-31T10:00:00+00:00".to_owned()),
            valid_to: Some("2026-06-05T23:59:59+00:00".to_owned()),
            decay_reason: None,
            // The placement style axis. `desc` has spaces + a literal
            // '%' so it exercises the percent-escaping; `style` is a bare enum.
            style: Some("prosa-tecnica".to_owned()),
            page_description: Some("Cene coi Brandibuck (100% prosa)".to_owned()),
            salience: None,
        };
        let rendered = render_entry(&cap);
        let parsed = parse_journal(&rendered, &WikiId::parse("famiglia").unwrap());
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.capture_id, cap.capture_id);
        assert_eq!(p.target_page, cap.target_page);
        assert_eq!(p.body, cap.body);
        assert_eq!(p.subject, cap.subject);
        assert_eq!(p.allow, cap.allow);
        assert_eq!(p.sender, cap.sender);
        assert_eq!(p.fact_type, cap.fact_type);
        assert_eq!(p.topics, cap.topics);
        assert_eq!(p.supersede_hint, cap.supersede_hint);
        assert_eq!(p.valid_from, cap.valid_from);
        assert_eq!(p.valid_to, cap.valid_to);
        assert_eq!(p.style, cap.style);
        assert_eq!(p.page_description, cap.page_description);
        // The origin fingerprint MUST survive: nothing else on the entry can
        // reconstruct it, so a reindex that lost it would lose it for good.
        assert_eq!(p.origin_message_hash, cap.origin_message_hash);
        // The vector must NOT: it is derivable from the body, and a binary
        // blob has no place in a human-readable journal. A rebuilt row pays
        // one embedding at its next read or at promotion.
        assert_eq!(p.embedding, None);
    }

    /// The rebuild path, end to end, from a journal file written by an OLDER
    /// version — planted on disk verbatim, not rendered by the code under test.
    ///
    /// `reindex_rebuilds_index_from_journal_after_db_wipe` above cannot catch a
    /// codec break: it writes with the current writer and reads with the current
    /// reader, so both halves move together and it stays green while every file
    /// already on disk stops loading. This test supplies the half that does not
    /// move.
    ///
    /// The entry below is the pre-rename spelling (`owner=`) with none of the
    /// attributes added since. If the read alias were ever dropped, `parse_entry`
    /// would return `None` and this rebuild would report a clean **zero** —
    /// which is what makes the loss silent rather than loud.
    #[tokio::test]
    async fn a_rebuild_replays_a_journal_written_by_an_older_version() {
        let (dir, _tree, pool) = setup().await;
        let abs = dir.path().join("wikis/alice");
        std::fs::create_dir_all(&abs).unwrap();
        std::fs::write(
            abs.join(crate::wiki::CAPTURES_FILENAME),
            "<!-- mwe-capture id=018f1234-5678-7abc-9def-0123456789ab \
             ts=2026-05-01T10:00:00+00:00 page=index.md type=bio status=buffered \
             owner=user:alice allow=user:bob sender=user:carol sup= topics=peso -->\n\
             Alice pesa 72 kg.\n\
             <!-- /mwe-capture -->\n",
        )
        .unwrap();

        let n = reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        assert_eq!(
            n, 1,
            "a journal from an older version must replay, not vanish"
        );

        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].body, "Alice pesa 72 kg.");
        assert_eq!(
            buffered[0].subject,
            Principal::User("alice".into()),
            "the pre-rename key names the subject"
        );
        assert_eq!(buffered[0].sender, Some(Principal::User("carol".into())));
    }

    #[test]
    fn a_legacy_journal_entry_still_parses() {
        // The journal is the durable source of truth a `rm engine.db` rebuild
        // replays. It is append-only with no compaction path, so it holds
        // entries written by every version the deployment has ever run.
        //
        // The round-trip test above cannot catch a codec change: it renders and
        // re-parses with the same code, so both halves move together and it
        // stays green while every entry already on disk stops loading. This
        // test exists to be the half that does not move — the line below is
        // verbatim, written the way an older version wrote it, and it must keep
        // parsing forever.
        //
        // What makes it worth a test of its own: `parse_entry` reads the
        // subject key with `?` inside an Option-returning function, so a key it
        // fails to find drops the WHOLE entry — no warning, no counter, no
        // error. The rebuild then reports success having quietly lost the fact.
        //
        // The `owner=` below is the point of the test, not a leftover to sweep.
        let legacy = "<!-- mwe-capture id=018f1234-5678-7abc-9def-0123456789ab \
             ts=2026-05-01T10:00:00+00:00 page=persone/alice.md type=bio \
             status=buffered owner=user:alice allow=user:bob sender=user:carol \
             sup= topics=peso,salute -->\n\
             Alice pesa 72 kg.\n\
             <!-- /mwe-capture -->\n";

        let parsed = parse_journal(legacy, &WikiId::parse("famiglia").unwrap());
        assert_eq!(parsed.len(), 1, "a legacy entry must not be dropped");
        let p = &parsed[0];
        assert_eq!(p.subject, Principal::User("alice".into()));
        assert_eq!(p.allow, vec![Principal::User("bob".into())]);
        assert_eq!(p.sender, Some(Principal::User("carol".into())));
        assert_eq!(p.body, "Alice pesa 72 kg.");
        // Attributes that did not yet exist when this entry was written come
        // back as their absent value, not as a parse failure.
        assert_eq!(p.valid_from, None);
        assert_eq!(p.origin_message_hash, None);
        assert_eq!(p.source_kind, "ingest");
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
    async fn reindex_rebuilds_index_from_journal_after_db_wipe() {
        let (dir, tree, pool) = setup().await;
        buffer_capture(
            &tree,
            &pool,
            req("alice", "First claim.", "user:alice"),
            None,
        )
        .await
        .unwrap();
        buffer_capture(
            &tree,
            &pool,
            req("alice", "Second claim.", "user:alice"),
            None,
        )
        .await
        .unwrap();

        // Simulate `rm engine.db`: drop every row from the index.
        sqlx::query("DELETE FROM capture_buffer")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(count_buffered(&pool).await.unwrap(), 0);

        // Rebuild from the durable journal.
        let abs = dir.path().join("wikis/alice");
        let n = reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        assert_eq!(n, 2);
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 2);
        let bodies: Vec<&str> = buffered.iter().map(|c| c.body.as_str()).collect();
        assert!(bodies.contains(&"First claim."));
        assert!(bodies.contains(&"Second claim."));
    }

    #[tokio::test]
    async fn validity_survives_journal_reindex_round_trip() {
        // Captures-journal invariant: the validity interval must reach the durable
        // journal and recovered by reindex after a DB wipe — never DB-only.
        let (dir, tree, pool) = setup().await;
        let mut r = req("alice", "A Berlino questa settimana.", "user:alice");
        r.valid_from = Some("2026-06-06T00:00:00+00:00".to_owned());
        r.valid_to = Some("2026-06-13T00:00:00+00:00".to_owned());
        buffer_capture(&tree, &pool, r, None).await.unwrap();

        // The durable journal carries the interval verbatim (vf=/vt= attributes).
        let journal = dir.path().join("wikis/alice/_captures.md");
        let raw = std::fs::read_to_string(&journal).unwrap();
        assert!(raw.contains("vf=2026-06-06T00:00:00+00:00"));
        assert!(raw.contains("vt=2026-06-13T00:00:00+00:00"));

        // Simulate `rm engine.db`, then rebuild from the journal.
        sqlx::query("DELETE FROM capture_buffer")
            .execute(&pool)
            .await
            .unwrap();
        let abs = dir.path().join("wikis/alice");
        reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(
            buffered[0].valid_from.as_deref(),
            Some("2026-06-06T00:00:00+00:00")
        );
        assert_eq!(
            buffered[0].valid_to.as_deref(),
            Some("2026-06-13T00:00:00+00:00")
        );
    }

    #[tokio::test]
    async fn salience_survives_journal_reindex_round_trip() {
        // Captures-journal invariant: the per-fact salience must reach the durable
        // journal (the whitespace-free `sal=` attr) and be recovered by reindex
        // after a DB wipe — never DB-only, so the index-routing signal survives.
        let (dir, tree, pool) = setup().await;
        let mut r = req("alice", "Alice è celiaca.", "user:alice");
        r.salience = Some("high".to_owned());
        buffer_capture(&tree, &pool, r, None).await.unwrap();

        // The durable journal carries the bare enum token.
        let journal = dir.path().join("wikis/alice/_captures.md");
        let raw = std::fs::read_to_string(&journal).unwrap();
        assert!(raw.contains("sal=high"));

        // Simulate `rm engine.db`, then rebuild from the journal.
        sqlx::query("DELETE FROM capture_buffer")
            .execute(&pool)
            .await
            .unwrap();
        let abs = dir.path().join("wikis/alice");
        reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].salience.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn authored_refs_survive_journal_reindex_round_trip() {
        // Group 17: the provenance breadcrumbs must reach the durable journal
        // (the comma-joined `aref=` attr) and be recovered by reindex after a
        // DB wipe — so a journal-rebuilt fact still links instead of duplicating.
        let (dir, tree, pool) = setup().await;
        let mut r = req("alice", "Ho rifatto il login del progetto.", "user:alice");
        r.authored_refs = vec![
            "[[acme/auth]]".to_owned(),
            "[[acme/modules/session]]".to_owned(),
        ];
        buffer_capture(&tree, &pool, r, None).await.unwrap();

        // The durable journal carries the comma-joined wikilinks as one token.
        let journal = dir.path().join("wikis/alice/_captures.md");
        let raw = std::fs::read_to_string(&journal).unwrap();
        assert!(raw.contains("aref=[[acme/auth]],[[acme/modules/session]]"));

        // Simulate `rm engine.db`, then rebuild from the journal.
        sqlx::query("DELETE FROM capture_buffer")
            .execute(&pool)
            .await
            .unwrap();
        let abs = dir.path().join("wikis/alice");
        reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(
            buffered[0].authored_refs,
            vec![
                "[[acme/auth]]".to_owned(),
                "[[acme/modules/session]]".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn placement_survives_journal_reindex_round_trip() {
        // Captures-journal invariant: the ingest placement style axis (style +
        // page_description) must reach the durable journal and be recovered by
        // reindex after a DB wipe — never DB-only. page_description has spaces +
        // a literal '%', so this also pins the percent-escaping of the codec.
        let (dir, tree, pool) = setup().await;
        let mut r = req("alice", "Alice ama la pasta.", "user:alice");
        r.style = Some("prosa-tecnica".to_owned());
        r.page_description = Some("Cosa piace ad Alice (100% gusti)".to_owned());
        buffer_capture(&tree, &pool, r, None).await.unwrap();

        // The durable journal carries style verbatim and the description escaped.
        let journal = dir.path().join("wikis/alice/_captures.md");
        let raw = std::fs::read_to_string(&journal).unwrap();
        assert!(raw.contains("style=prosa-tecnica"));
        assert!(raw.contains("desc=Cosa%20piace%20ad%20Alice%20(100%25%20gusti)"));

        // Simulate `rm engine.db`, then rebuild from the journal.
        sqlx::query("DELETE FROM capture_buffer")
            .execute(&pool)
            .await
            .unwrap();
        let abs = dir.path().join("wikis/alice");
        reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].style.as_deref(), Some("prosa-tecnica"));
        assert_eq!(
            buffered[0].page_description.as_deref(),
            Some("Cosa piace ad Alice (100% gusti)")
        );
    }

    #[tokio::test]
    async fn reindex_is_idempotent() {
        let (dir, tree, pool) = setup().await;
        buffer_capture(
            &tree,
            &pool,
            req("alice", "Only claim.", "user:alice"),
            None,
        )
        .await
        .unwrap();
        let abs = dir.path().join("wikis/alice");
        // Row already exists from buffer_capture → rebuild inserts nothing.
        let n = reindex_capture_journal(&pool, &WikiId::parse("alice").unwrap(), &abs)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(count_buffered(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn rejects_marker_and_comment_bodies() {
        let (_dir, tree, pool) = setup().await;
        assert!(matches!(
            buffer_capture(
                &tree,
                &pool,
                req("alice", "has {{marker}}", "user:alice"),
                None
            )
            .await,
            Err(CaptureBufferError::BodyContainsReserved)
        ));
        assert!(matches!(
            buffer_capture(
                &tree,
                &pool,
                req("alice", "has <!-- comment", "user:alice"),
                None
            )
            .await,
            Err(CaptureBufferError::BodyContainsReserved)
        ));
        assert!(matches!(
            buffer_capture(&tree, &pool, req("alice", "   ", "user:alice"), None).await,
            Err(CaptureBufferError::EmptyBody)
        ));
    }

    #[tokio::test]
    async fn sender_equal_subject_is_materialized() {
        let (_dir, tree, pool) = setup().await;
        let mut r = req("alice", "self note", "user:alice");
        r.sender = Some("user:alice".parse::<Principal>().unwrap());
        buffer_capture(&tree, &pool, r, None).await.unwrap();
        let buffered = find_buffered_in_wiki(&pool, "alice").await.unwrap();
        assert_eq!(
            buffered[0].sender,
            Some("user:alice".parse::<Principal>().unwrap()),
            "sender must stay materialized (= subject), never collapsed to None"
        );
    }
}
