// SPDX-License-Identifier: AGPL-3.0-or-later
//! Document / long-form ingest — the async pipeline behind
//! `wiki_ingest_external` (engineering wiki:
//! [`wiki/design-notes/document-ingest.md`](../../../wiki/design-notes/document-ingest.md)).
//!
//! A document is a **unit by default**: the disposition dial decides whether
//! it stays consultable (`consult` — a document page + the catalog blob,
//! nothing scattered), keeps its identity plus a selective extraction
//! (`dossier` — only facts that transcend the document leave it), or
//! dissolves entirely into routed facts (`dissolve` — the long-voice-note
//! case). The code provides mechanism (segmentation, caps, the checkpointed
//! job lifecycle); the LLM instructed by the bundled prompts decides content
//! (the disposition, what transcends the document, where facts belong).
//!
//! Pipeline phases, each a checkpoint on the `document_jobs` row so a
//! crashed worker resumes instead of re-running:
//! classify → segment → anchor → extract (map, per segment) →
//! conciliate (reduce) → file (capture buffer) → notice.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::capture::{self, CaptureRequest};
use crate::capture_buffer;
use crate::config::{LlmConfig, LlmFunction};
use crate::embedder::Embedder;
use crate::events::{self, EventKind};
use crate::ingest::{available_wikis, normalize_capture_page, parse_first_json};
use crate::llm::{CompletionRequest, LlmBackend, LlmError};
use crate::prompts;
use crate::types::{CatalogId, Principal, WikiId};
use crate::wiki::WikiTree;

/// `capture_buffer.source_kind` value stamped on facts this pipeline files.
pub const SOURCE_KIND_DOCUMENT: &str = "document";

/// Bundled default for the `document-classify` system prompt
/// (registered in [`crate::prompts::BUNDLED`]).
pub const BUNDLED_DOCUMENT_CLASSIFY_MD: &str = include_str!("../prompts/document-classify.md");
/// Bundled default for the `document-extract` system prompt.
pub const BUNDLED_DOCUMENT_EXTRACT_MD: &str = include_str!("../prompts/document-extract.md");
/// Bundled default for the `document-merge` system prompt.
pub const BUNDLED_DOCUMENT_MERGE_MD: &str = include_str!("../prompts/document-merge.md");

/// Errors raised by the document-ingest pipeline.
#[derive(Debug, Error)]
pub enum DocumentError {
    /// Underlying SQL failure.
    #[error("document db: {0}")]
    Db(#[from] sqlx::Error),
    /// Wiki tree failure (unknown wiki, IO).
    #[error("document wiki: {0}")]
    Wiki(#[from] crate::wiki::WikiError),
    /// Failure inside the shared ingest helpers (wiki enumeration).
    #[error("document ingest: {0}")]
    Ingest(#[from] crate::ingest::IngestError),
    /// Direct capture failure (the anchor fact).
    #[error("document capture: {0}")]
    Capture(#[from] crate::capture::CaptureError),
    /// Buffered capture failure (extracted facts).
    #[error("document buffer: {0}")]
    Buffer(#[from] crate::capture_buffer::CaptureBufferError),
    /// LLM failure (classify / extract / merge).
    #[error("document llm: {0}")]
    Llm(#[from] LlmError),
    /// Embedding failure (the reduce prefilter).
    #[error("document embed: {0}")]
    Embed(#[from] crate::embedder::EmbedderError),
    /// Prompt loading failure.
    #[error("document prompt: {0}")]
    Prompt(#[from] crate::prompts::PromptError),
    /// Notice emission failure.
    #[error("document events: {0}")]
    Events(#[from] crate::events::EventsError),
    /// Invalid input (empty text, oversized document, bad enum, …).
    #[error("document invalid: {0}")]
    Invalid(String),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, DocumentError>;

/// True when the failure is transient (transport / rate-limit / backend
/// 5xx): the job stays `running` and the next poll retries; everything
/// else marks the job `failed`.
const fn is_retriable(err: &DocumentError) -> bool {
    matches!(
        err,
        DocumentError::Llm(LlmError::Transport(_) | LlmError::RateLimit(_) | LlmError::Backend(_))
    )
}

// ---------- The disposition dial ----------

/// How the memory holds a document — the dial decided in the 13a design
/// session: a document is a unit by default; scattering is the judged
/// exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Full identity: a document page (testata + summary + `{{embed=…}}`),
    /// nothing extracted. The appliance-manual case.
    Consult,
    /// The document page **plus** a selective extraction of what
    /// transcends the document. The meeting / phone-call case.
    Dossier,
    /// No document identity: full extraction, facts routed to the right
    /// pages. The long-voice-note case.
    Dissolve,
}

impl Disposition {
    /// Wire-stable lowercase token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Consult => "consult",
            Self::Dossier => "dossier",
            Self::Dissolve => "dissolve",
        }
    }

    /// Parse the wire token. `None` for an unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "consult" => Some(Self::Consult),
            "dossier" => Some(Self::Dossier),
            "dissolve" => Some(Self::Dissolve),
            _ => None,
        }
    }
}

/// The document's textual shape, driving segmentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    /// Running prose / markdown: cut on headings + paragraph packing.
    Prose,
    /// A conversation transcript: cut on utterance blocks; per-block
    /// timestamps (when the transcript carries them) flow to per-fact
    /// `occurred_at`.
    Dialogue,
}

impl DocFormat {
    /// Wire-stable lowercase token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Dialogue => "dialogue",
        }
    }

    /// Parse the wire token. `None` for an unknown value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prose" => Some(Self::Prose),
            "dialogue" => Some(Self::Dialogue),
            _ => None,
        }
    }
}

// ---------- Policy ----------

/// Resource knobs of the pipeline. Caps and cadences only — never a
/// semantic gate (the LLM decides dispositions and extractions).
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentPolicy {
    /// Worker poll cadence, seconds.
    pub poll_secs: u64,
    /// Segment packing target, characters.
    pub segment_target_chars: usize,
    /// Hard per-segment cap; a single oversized paragraph is split here.
    pub segment_max_chars: usize,
    /// Enqueue refuses a document that segments beyond this (no silent
    /// truncation).
    pub max_segments: usize,
    /// Extraction output cap per segment (excess dropped with a warn log).
    pub max_facts_per_segment: usize,
    /// Document prefix the disposition classifier sees, characters.
    pub classify_sample_chars: usize,
    /// Embedding cosine at/above which two candidate facts cluster for the
    /// reduce merge.
    pub merge_threshold: f32,
    /// Hard input cap at enqueue, characters.
    pub max_document_chars: usize,
}

impl Default for DocumentPolicy {
    fn default() -> Self {
        Self {
            poll_secs: 10,
            segment_target_chars: 3_000,
            segment_max_chars: 4_500,
            max_segments: 400,
            max_facts_per_segment: 12,
            classify_sample_chars: 6_000,
            merge_threshold: 0.90,
            max_document_chars: 1_500_000,
        }
    }
}

// ---------- Enqueue ----------

/// Input to [`enqueue`]. The server layer resolves the source to text
/// (blob read / trusted `text` seam) and the effective ACL before calling.
#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    /// `media` | `inline` | `url` — the wire `source.type`.
    pub source_kind: String,
    /// Catalog id / url; `None` for inline.
    pub source_ref: Option<String>,
    /// The resolved document text.
    pub text: String,
    /// Caller-supplied title hint (e.g. the original filename).
    pub title_hint: Option<String>,
    /// Caller-forced disposition; `None` = the classifier decides.
    pub disposition: Option<Disposition>,
    /// Caller-forced format; `None` = the classifier decides.
    pub format: Option<DocFormat>,
    /// The document's semantic clock (ISO-8601); relative dates inside the
    /// document resolve against it.
    pub occurred_at: Option<String>,
    /// Owning principal of everything the job writes.
    pub owner: Principal,
    /// `allow=` extension list (inherited from the source catalog row).
    pub allow: Vec<Principal>,
    /// Cross-user attribution (who captured it). `None` on input is
    /// materialized to `owner` at enqueue — always stored as a distinct,
    /// explicit field, never collapsed into `owner`.
    pub sender: Option<Principal>,
    /// Bypass the (text sha256, owner) idempotency check.
    pub force: bool,
}

/// Outcome of [`enqueue`].
#[derive(Debug, Clone)]
pub struct EnqueueOutcome {
    /// The job id (fresh, or the prior job's on an idempotency hit).
    pub job_id: String,
    /// True when an existing non-failed job for the same (text, owner)
    /// absorbed the call.
    pub existing: bool,
    /// Resolved document size, characters.
    pub size_chars: usize,
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn new_job_id() -> String {
    uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Enqueue a document job. Idempotent by (text sha256, owner) across
/// non-failed jobs unless `force`; the receipt is immediate, the work is
/// the worker loop's.
///
/// # Errors
///
/// [`DocumentError::Invalid`] on empty/oversized text; [`DocumentError::Db`].
pub async fn enqueue(
    pool: &SqlitePool,
    policy: &DocumentPolicy,
    req: EnqueueRequest,
) -> Result<EnqueueOutcome> {
    if req.text.trim().is_empty() {
        return Err(DocumentError::Invalid("document text is empty".into()));
    }
    if req.text.chars().count() > policy.max_document_chars {
        return Err(DocumentError::Invalid(format!(
            "document exceeds max_document_chars ({})",
            policy.max_document_chars
        )));
    }
    let sha = sha256_hex(&req.text);
    let owner = req.owner.to_string();
    if !req.force {
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT job_id FROM document_jobs
              WHERE text_sha256 = ? AND owner_id = ? AND status != 'failed'
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&sha)
        .bind(&owner)
        .fetch_optional(pool)
        .await?;
        if let Some((job_id,)) = existing {
            return Ok(EnqueueOutcome {
                job_id,
                existing: true,
                size_chars: req.text.chars().count(),
            });
        }
    }
    let job_id = new_job_id();
    let allow_json = serde_json::to_string(
        &req.allow
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )
    .map_err(|e| DocumentError::Invalid(format!("allow_ids: {e}")))?;
    // Mirror the capture-path invariant: sender is always materialized
    // (= owner when absent) and kept distinct from owner, so a later
    // ownership change never rebinds the original provenance.
    let sender = req.sender.clone().or_else(|| Some(req.owner.clone()));
    let ts = now();
    sqlx::query(
        r#"INSERT INTO document_jobs (
            job_id, source_kind, source_ref, text_sha256, "text", title_hint,
            disposition_requested, format_requested, occurred_at,
            owner_id, allow_ids, sender_id, status, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?)"#,
    )
    .bind(&job_id)
    .bind(&req.source_kind)
    .bind(&req.source_ref)
    .bind(&sha)
    .bind(&req.text)
    .bind(&req.title_hint)
    .bind(req.disposition.map(Disposition::as_str))
    .bind(req.format.map(DocFormat::as_str))
    .bind(&req.occurred_at)
    .bind(&owner)
    .bind(&allow_json)
    .bind(sender.map(|s| s.to_string()))
    .bind(&ts)
    .bind(&ts)
    .execute(pool)
    .await?;
    tracing::info!(
        job_id,
        source_kind = req.source_kind,
        "document: job enqueued"
    );
    Ok(EnqueueOutcome {
        job_id,
        existing: false,
        size_chars: req.text.chars().count(),
    })
}

// ---------- Job row ----------

/// One `document_jobs` row — the checkpointed lifecycle of an ingested
/// document.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DocumentJob {
    /// `UUIDv7` job id.
    pub job_id: String,
    /// `media` | `inline` | `url`.
    pub source_kind: String,
    /// Catalog id / url; `None` for inline.
    pub source_ref: Option<String>,
    /// Idempotency key over the resolved text.
    pub text_sha256: String,
    /// The resolved document text.
    pub text: String,
    /// Caller-supplied title hint.
    pub title_hint: Option<String>,
    /// Caller-forced disposition token.
    pub disposition_requested: Option<String>,
    /// Caller-forced format token.
    pub format_requested: Option<String>,
    /// The document's semantic clock.
    pub occurred_at: Option<String>,
    /// Owning principal (string form).
    pub owner_id: String,
    /// JSON array of allow principals.
    pub allow_ids: Option<String>,
    /// Cross-user attribution. Materialized (= owner) at enqueue; `None`
    /// survives only as the degenerate scrubbed state.
    pub sender_id: Option<String>,
    /// `queued` | `running` | `done` | `failed`.
    pub status: String,
    /// Classify-phase checkpoint.
    pub resolved_disposition: Option<String>,
    /// Classify-phase checkpoint.
    pub resolved_format: Option<String>,
    /// Classify-phase checkpoint.
    pub resolved_title: Option<String>,
    /// Classify-phase checkpoint.
    pub target_wiki_id: Option<String>,
    /// Anchor page slug (consult / dossier).
    pub document_page: Option<String>,
    /// Anchor-phase checkpoint.
    pub anchor_fact_id: Option<String>,
    /// Classify-phase summary (the anchor body).
    pub summary: Option<String>,
    /// Reduce-phase checkpoint (post-merge candidate facts, JSON).
    pub reduced_json: Option<String>,
    /// Segment-phase checkpoint.
    pub total_segments: Option<i64>,
    /// Extraction progress.
    pub done_segments: i64,
    /// Candidate facts produced by the map phase.
    pub facts_extracted: i64,
    /// File-phase progress cursor.
    pub facts_buffered: i64,
    /// Last error (kept across retries for the operator).
    pub error: Option<String>,
    /// ISO-8601 timestamps.
    pub created_at: String,
    /// Last lifecycle touch.
    pub updated_at: String,
    /// Set when the job reaches `done` / `failed`.
    pub finished_at: Option<String>,
}

const SELECT_JOB: &str = r#"
    SELECT job_id, source_kind, source_ref, text_sha256, "text", title_hint,
           disposition_requested, format_requested, occurred_at,
           owner_id, allow_ids, sender_id, status,
           resolved_disposition, resolved_format, resolved_title,
           target_wiki_id, document_page, anchor_fact_id, summary,
           reduced_json, total_segments, done_segments, facts_extracted,
           facts_buffered, error, created_at, updated_at, finished_at
      FROM document_jobs
"#;

/// Fetch one job by id.
///
/// # Errors
///
/// [`DocumentError::Db`].
pub async fn find_job(pool: &SqlitePool, job_id: &str) -> Result<Option<DocumentJob>> {
    let sql = format!("{SELECT_JOB} WHERE job_id = ?");
    Ok(sqlx::query_as::<_, DocumentJob>(&sql)
        .bind(job_id)
        .fetch_optional(pool)
        .await?)
}

/// The oldest runnable job: `queued`, or `running` (a crashed worker's
/// leftover — the checkpoints make re-entry safe).
async fn next_runnable_job(pool: &SqlitePool) -> Result<Option<DocumentJob>> {
    let sql = format!(
        "{SELECT_JOB} WHERE status IN ('queued', 'running') ORDER BY created_at ASC LIMIT 1"
    );
    Ok(sqlx::query_as::<_, DocumentJob>(&sql)
        .fetch_optional(pool)
        .await?)
}

async fn touch_job(pool: &SqlitePool, job_id: &str, sets: &str, binds: &[&str]) -> Result<()> {
    let sql = format!("UPDATE document_jobs SET {sets}, updated_at = ? WHERE job_id = ?");
    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.bind(now()).bind(job_id).execute(pool).await?;
    Ok(())
}

// ---------- Segmentation (deterministic, code-owned) ----------

/// One segment of the document — the unit of the extraction map phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Heading chain (prose) or a block hint (dialogue); prompt context.
    pub heading: Option<String>,
    /// Segment text.
    pub content: String,
    /// Per-utterance clock (dialogue), when the transcript carries one.
    pub occurred_at: Option<String>,
}

/// Cut the document into segments.
///
/// Deterministic and code-owned: the model judges content, never where
/// to cut. Prose cuts on markdown headings + paragraph packing; dialogue
/// cuts on blank-line blocks with per-block timestamp detection.
#[must_use]
pub fn segment_document(
    text: &str,
    format: DocFormat,
    base_date: Option<&str>,
    policy: &DocumentPolicy,
) -> Vec<Segment> {
    match format {
        DocFormat::Prose => segment_prose(text, policy),
        DocFormat::Dialogue => segment_dialogue(text, base_date, policy),
    }
}

/// Split an oversized chunk at `max` characters on the nearest char
/// boundary (a pathological single paragraph must not blow the prompt).
fn hard_split(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        current.push(ch);
        if current.chars().count() >= max {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn segment_prose(text: &str, policy: &DocumentPolicy) -> Vec<Segment> {
    // Heading chain: a stack of (level, title) — `## B` under `# A`
    // renders as "A › B" in the segment context.
    let mut chain: Vec<(usize, String)> = Vec::new();
    let mut out: Vec<Segment> = Vec::new();
    let mut buf = String::new();
    let mut buf_heading: Option<String> = None;

    let flush = |out: &mut Vec<Segment>, buf: &mut String, heading: &Option<String>| {
        let body = buf.trim();
        if !body.is_empty() {
            out.push(Segment {
                heading: heading.clone(),
                content: body.to_owned(),
                occurred_at: None,
            });
        }
        buf.clear();
    };

    for para in text.split("\n\n") {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.lines().next().unwrap_or("");
        let hashes = first.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && first.chars().nth(hashes) == Some(' ') {
            // A heading paragraph closes the current segment and pushes the
            // chain.
            flush(&mut out, &mut buf, &buf_heading);
            let title = first[hashes + 1..].trim().to_owned();
            chain.retain(|(lvl, _)| *lvl < hashes);
            chain.push((hashes, title));
            buf_heading = Some(
                chain
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join(" › "),
            );
            // Any prose lines following the heading inside the same
            // paragraph stay with it.
            let rest: String = trimmed.lines().skip(1).collect::<Vec<_>>().join("\n");
            if !rest.trim().is_empty() {
                buf.push_str(rest.trim());
                buf.push_str("\n\n");
            }
            continue;
        }
        for piece in hard_split(trimmed, policy.segment_max_chars) {
            if !buf.is_empty()
                && buf.chars().count() + piece.chars().count() > policy.segment_target_chars
            {
                flush(&mut out, &mut buf, &buf_heading);
            }
            buf.push_str(&piece);
            buf.push_str("\n\n");
        }
    }
    flush(&mut out, &mut buf, &buf_heading);
    out
}

/// Best-effort timestamp of a dialogue block: a full ISO-8601 instant
/// anywhere in the first line, or a leading `[HH:MM]` / `[HH:MM:SS]`
/// combined with the document date.
fn block_timestamp(first_line: &str, base_date: Option<&str>) -> Option<String> {
    // Full ISO instant: scan word tokens.
    for tok in first_line.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '-');
        if t.len() >= 16
            && t.as_bytes().get(4) == Some(&b'-')
            && t.as_bytes().get(7) == Some(&b'-')
            && (t.as_bytes().get(10) == Some(&b'T') || t.as_bytes().get(10) == Some(&b' '))
            && chrono::DateTime::parse_from_rfc3339(t).is_ok()
        {
            return Some(t.to_owned());
        }
    }
    // Bracketed wall-clock: needs the document date to become an instant.
    let date = base_date?.get(..10)?;
    let inner = first_line.trim_start().strip_prefix('[')?;
    let end = inner.find(']')?;
    let clock = &inner[..end];
    let parts: Vec<&str> = clock.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let s: u32 = if parts.len() == 3 {
        parts[2].parse().ok()?
    } else {
        0
    };
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(format!("{date}T{h:02}:{m:02}:{s:02}Z"))
}

fn segment_dialogue(text: &str, base_date: Option<&str>, policy: &DocumentPolicy) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut buf = String::new();
    let mut buf_ts: Option<String> = None;

    let flush = |out: &mut Vec<Segment>, buf: &mut String, ts: &mut Option<String>| {
        let body = buf.trim();
        if !body.is_empty() {
            out.push(Segment {
                heading: None,
                content: body.to_owned(),
                occurred_at: ts.take(),
            });
        }
        buf.clear();
    };

    for block in text.split("\n\n") {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ts = block_timestamp(trimmed.lines().next().unwrap_or(""), base_date);
        for piece in hard_split(trimmed, policy.segment_max_chars) {
            if !buf.is_empty()
                && buf.chars().count() + piece.chars().count() > policy.segment_target_chars
            {
                flush(&mut out, &mut buf, &mut buf_ts);
            }
            if buf.is_empty() {
                buf_ts.clone_from(&ts);
            }
            buf.push_str(&piece);
            buf.push_str("\n\n");
        }
    }
    flush(&mut out, &mut buf, &mut buf_ts);
    out
}

// ---------- Classify (disposition + identity) ----------

/// The classifier's JSON reply — tolerant deserialization, validated by
/// [`classify_document`].
#[derive(Debug, Clone, Default, Deserialize)]
struct LlmDocumentPlan {
    #[serde(default)]
    disposition: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    page_slug: Option<String>,
    #[serde(default)]
    target_wiki_id: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    page_description: Option<String>,
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
}

/// Input to [`classify_document`].
#[derive(Debug, Clone)]
pub struct ClassifyInput<'a> {
    /// Document text (the classifier reads a policy-capped prefix).
    pub text: &'a str,
    /// Caller-supplied title hint.
    pub title_hint: Option<&'a str>,
    /// `media` | `inline` | `url`.
    pub source_kind: &'a str,
    /// The document's semantic clock.
    pub occurred_at: Option<&'a str>,
    /// Caller-forced disposition (overrides the proposal).
    pub forced_disposition: Option<Disposition>,
    /// Caller-forced format (overrides the proposal).
    pub forced_format: Option<DocFormat>,
    /// Owning principal — its identity wiki is the routing fallback.
    pub owner: &'a Principal,
}

/// The validated classify outcome — everything the anchor + extraction
/// phases need.
#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    /// The disposition the job will run.
    pub disposition: Disposition,
    /// The segmentation shape.
    pub format: DocFormat,
    /// Document title (the anchor page's subject).
    pub title: String,
    /// Anchor page path (normalized, safe).
    pub page: PathBuf,
    /// Wiki the anchor lands in.
    pub target_wiki_id: String,
    /// Anchor body prose.
    pub summary: String,
    /// Testata seed.
    pub page_description: Option<String>,
    /// Testata seed (`prosa` | `prosa-tecnica` | `lista`).
    pub style: Option<String>,
    /// Topic tags for the anchor fact.
    pub topics: Vec<String>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

/// Render the standard-wiki routing window the document prompts share with
/// the conversational classifier. Each entry carries the wiki's `scope` prose
/// (the category description) so the extractor can read it as an audience +
/// placement signal, exactly as the message classifier does in
/// `ingest::build_prompt`'s `available_wikis` section.
fn wikis_block(tree: &WikiTree) -> Result<String> {
    let mut out = String::from("available_wikis:\n");
    let mut any = false;
    for d in tree.walk()? {
        if d.meta.smart {
            continue;
        }
        any = true;
        out.push_str("  - wiki_id: ");
        out.push_str(d.meta.wiki_id.as_str());
        out.push_str("\n    title: ");
        out.push_str(&d.meta.title);
        out.push_str("\n    type: ");
        out.push_str(&d.meta.wiki_type);
        out.push_str("\n    scope: ");
        match d
            .meta
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => out.push_str(s),
            None => out.push_str("(no scope configured)"),
        }
        out.push('\n');
    }
    if !any {
        out.push_str("  (none)\n");
    }
    Ok(out)
}

/// Render the sender's groups (id + operator-set `scope` prose) for the
/// extraction prompt's `sender_groups` section — the audience signal the
/// extractor reads when deciding a fact's `allow_ids`, mirroring
/// `ingest::build_prompt`. An empty list renders `(none)`.
fn sender_groups_block(groups: &[(String, Option<String>)]) -> String {
    let mut out = String::from("sender_groups:\n");
    if groups.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    for (id, scope) in groups {
        out.push_str("  - id: ");
        out.push_str(id);
        out.push_str("\n    scope: ");
        match scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => out.push_str(s),
            None => out.push_str("(no scope configured)"),
        }
        out.push('\n');
    }
    out
}

/// Render the `known_users` roster the extractor resolves a named subject
/// against — the enrolment gate that stops `owner_id` minting a `user:<id>`
/// for a person who is not in the system. Mirrors `ingest::build_prompt`'s
/// `known_users` section (id + aliases); an empty roster renders `(none)`.
fn known_users_block(users: &[crate::enrollment::EnrolledUserLite]) -> String {
    let mut out = String::from("known_users:\n");
    if users.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    for u in users {
        out.push_str("  - id: ");
        out.push_str(&u.user_id);
        if !u.aliases.is_empty() {
            out.push_str("\n    aliases: ");
            out.push_str(&u.aliases.join(", "));
        }
        out.push('\n');
    }
    out
}

fn wiki_exists_standard(tree: &WikiTree, wiki_id: &str) -> bool {
    available_wikis(tree, usize::MAX)
        .is_ok_and(|ws| ws.iter().any(|w| !w.smart && w.wiki_id == wiki_id))
}

/// Run the disposition classifier over the document and validate its
/// proposal into a [`ResolvedPlan`].
///
/// Caller-forced disposition/format win over the proposal; an
/// unparseable proposal degrades to the conservative `consult` (nothing
/// scatters on a bad day).
///
/// # Errors
///
/// [`DocumentError::Llm`] on transport failure, [`DocumentError::Prompt`],
/// [`DocumentError::Invalid`] when no standard wiki exists to route to.
pub async fn classify_document(
    llm: &dyn LlmBackend,
    tree: &WikiTree,
    workdir: &Path,
    policy: &DocumentPolicy,
    input: &ClassifyInput<'_>,
) -> Result<ResolvedPlan> {
    let system = prompts::load("document-classify", workdir, BUNDLED_DOCUMENT_CLASSIFY_MD)?;
    let sample = truncate_chars(input.text, policy.classify_sample_chars);
    let mut user = String::new();
    user.push_str("source_kind: ");
    user.push_str(input.source_kind);
    user.push('\n');
    if let Some(t) = input.title_hint {
        user.push_str("title_hint: ");
        user.push_str(t);
        user.push('\n');
    }
    if let Some(at) = input.occurred_at {
        user.push_str("document_time: ");
        user.push_str(at);
        user.push('\n');
    }
    user.push_str(&wikis_block(tree)?);
    user.push_str("\ndocument_sample:\n");
    user.push_str(&sample);

    let resp = llm
        .complete(
            CompletionRequest::new(user)
                .with_system(system)
                .with_temperature(0.1)
                .with_max_tokens(2048),
        )
        .await?;
    let plan: LlmDocumentPlan = parse_first_json(&resp.text).unwrap_or_default();

    let disposition = input
        .forced_disposition
        .or_else(|| plan.disposition.as_deref().and_then(Disposition::parse))
        // Conservative fail-safe: an unparseable proposal keeps the
        // document whole instead of scattering it.
        .unwrap_or(Disposition::Consult);
    let format = input
        .forced_format
        .or_else(|| plan.format.as_deref().and_then(DocFormat::parse))
        .unwrap_or(DocFormat::Prose);

    let title = plan
        .title
        .filter(|t| !t.trim().is_empty())
        .map(|t| truncate_chars(t.trim(), 120))
        .or_else(|| input.title_hint.map(|t| truncate_chars(t.trim(), 120)))
        .unwrap_or_else(|| {
            let first = input.text.lines().find(|l| !l.trim().is_empty());
            truncate_chars(
                first.unwrap_or("documento").trim_start_matches('#').trim(),
                80,
            )
        });

    // Routing: the proposal, else the owner's identity wiki, else the
    // first standard wiki — same anti-hallucination posture as ingest.
    let target_wiki_id = match plan.target_wiki_id {
        Some(w) if wiki_exists_standard(tree, &w) => w,
        _ => {
            let owner_wiki = match input.owner {
                Principal::User(id) => id.clone(),
                // A group owner (incl. the builtin global group) has no user
                // identity wiki — fall through to the first standard wiki.
                Principal::Group(_) => String::new(),
            };
            if !owner_wiki.is_empty() && wiki_exists_standard(tree, &owner_wiki) {
                owner_wiki
            } else {
                available_wikis(tree, usize::MAX)?
                    .into_iter()
                    .find(|w| !w.smart)
                    .map(|w| w.wiki_id)
                    .ok_or_else(|| {
                        DocumentError::Invalid("no standard wiki to route the document to".into())
                    })?
            }
        },
    };

    // Page slug: the proposal (normalized through the same safety funnel
    // as ingest), else a code-side slug of the title.
    let fallback = crate::slug::derive_slug(&title).map_or_else(
        |_| PathBuf::from("documento.md"),
        |s| PathBuf::from(format!("{s}.md")),
    );
    let page = normalize_capture_page(plan.page_slug.as_deref(), &fallback);

    let summary = plan
        .summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| truncate_chars(input.text.trim(), 400));

    Ok(ResolvedPlan {
        disposition,
        format,
        title,
        page,
        target_wiki_id,
        summary,
        page_description: plan.page_description.filter(|s| !s.trim().is_empty()),
        style: plan.style.filter(|s| !s.trim().is_empty()),
        topics: plan.topics,
    })
}

// ---------- Extraction (map) ----------

/// One candidate fact the map phase extracted — the reduce input and the
/// file-phase payload (serialized on the job/segment rows as JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateFact {
    /// Atomic prose claim.
    pub body: String,
    /// Routing target (validated against the standard-wiki window).
    #[serde(default)]
    pub target_wiki_id: Option<String>,
    /// Placement hint.
    #[serde(default)]
    pub target_page: Option<String>,
    /// The fact's SUBJECT principal (`user:<id>` | `group:<id>` | `global`),
    /// decided by the extractor under the ingest rules — independent of the
    /// audience. `None` (absent) defaults to `user:<sender>` (the uploader) at
    /// the file phase, the same default the `ingest` path uses.
    #[serde(default)]
    pub owner_id: Option<String>,
    /// The fact's AUDIENCE — extra read principals beyond owner+sender,
    /// decided by the extractor from the group/wiki `scope` signals and the
    /// document's own cues. Empty (the default) keeps the fact to owner+sender.
    #[serde(default)]
    pub allow_ids: Vec<String>,
    /// Taxonomy hint.
    #[serde(default)]
    pub fact_type: Option<String>,
    /// Topic tags.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Per-fact validity interval.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// See [`Self::valid_from`].
    #[serde(default)]
    pub valid_to: Option<String>,
    /// `high | normal | low`.
    #[serde(default)]
    pub salience: Option<String>,
    /// Testata seed for a fresh page.
    #[serde(default)]
    pub style: Option<String>,
    /// Testata seed for a fresh page.
    #[serde(default)]
    pub page_description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LlmSegmentFacts {
    #[serde(default)]
    facts: Vec<CandidateFact>,
}

/// The two selectivity postures of the extraction prompt. Code picks the
/// instruction; the judgment stays the model's.
const SELECTIVITY_DOSSIER: &str = "Extract ONLY facts that transcend the document itself: \
commitments, decisions, dates, personal facts, relationships, preferences. The document keeps \
its own page — its internal content (procedures, technical details, the discussion itself) \
must NOT become facts. When in doubt, leave it in the document. An empty facts array is a \
perfectly good answer for a segment with nothing that outlives the document.";
const SELECTIVITY_DISSOLVE: &str = "Extract EVERY atomic fact worth remembering: the document \
has no identity of its own — your extractions are the only trace it leaves in memory. Skip \
filler and pleasantries; keep facts, preferences, episodes, commitments, decisions, dates.";

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the ingest prompt's input assembly"
)]
async fn extract_segment(
    llm: &dyn LlmBackend,
    tree: &WikiTree,
    workdir: &Path,
    policy: &DocumentPolicy,
    job: &DocumentJob,
    plan_disposition: Disposition,
    sender_groups: &[(String, Option<String>)],
    known_users: &[crate::enrollment::EnrolledUserLite],
    segment: &Segment,
    position: (i64, i64),
) -> Result<Vec<CandidateFact>> {
    use std::fmt::Write as _;

    let seg_heading = segment.heading.as_deref();
    let seg_occurred_at = segment.occurred_at.as_deref();
    let seg_content = segment.content.as_str();
    let selectivity = match plan_disposition {
        Disposition::Dossier => SELECTIVITY_DOSSIER,
        Disposition::Dissolve => SELECTIVITY_DISSOLVE,
        Disposition::Consult => return Ok(Vec::new()),
    };
    let system = prompts::render(
        "document-extract",
        workdir,
        BUNDLED_DOCUMENT_EXTRACT_MD,
        &[("selectivity", selectivity)],
    )?;
    let current_time = seg_occurred_at
        .or(job.occurred_at.as_deref())
        .unwrap_or(job.created_at.as_str());
    let mut user = String::new();
    user.push_str("document_title: ");
    user.push_str(job.resolved_title.as_deref().unwrap_or("(untitled)"));
    user.push('\n');
    if let Some(s) = &job.summary {
        user.push_str("document_summary: ");
        user.push_str(&truncate_chars(s, 600));
        user.push('\n');
    }
    user.push_str("current_time: ");
    user.push_str(current_time);
    user.push('\n');
    // The document's sender is the uploader (`sender_id`, materialized to the
    // owner when absent) — the principal `owner_id`/`allow_ids` default against.
    user.push_str("sender_id: ");
    user.push_str(job.sender_id.as_deref().unwrap_or(&job.owner_id));
    user.push('\n');
    user.push('\n');
    user.push_str(&known_users_block(known_users));
    user.push_str(&sender_groups_block(sender_groups));
    if let Some(h) = seg_heading {
        user.push_str("segment_heading: ");
        user.push_str(h);
        user.push('\n');
    }
    let _ = writeln!(user, "segment_position: {} of {}", position.0, position.1);
    user.push('\n');
    user.push_str(&wikis_block(tree)?);
    user.push_str("\nsegment:\n");
    user.push_str(seg_content);

    let resp = llm
        .complete(
            CompletionRequest::new(user)
                .with_system(system)
                .with_temperature(0.1)
                .with_max_tokens(8_192),
        )
        .await?;
    let parsed: LlmSegmentFacts = parse_first_json(&resp.text).unwrap_or_default();
    let mut out = Vec::new();
    for f in parsed.facts {
        if f.body.trim().is_empty() {
            continue;
        }
        if out.len() >= policy.max_facts_per_segment {
            tracing::warn!(
                job_id = job.job_id,
                cap = policy.max_facts_per_segment,
                "document: per-segment fact cap hit, excess dropped"
            );
            break;
        }
        // Anti-hallucination: an unknown routing target drops to the job's
        // anchor wiki rather than inventing one.
        let wiki_ok = f
            .target_wiki_id
            .as_deref()
            .is_some_and(|w| wiki_exists_standard(tree, w));
        let mut f = f;
        if !wiki_ok {
            f.target_wiki_id.clone_from(&job.target_wiki_id);
        }
        out.push(f);
    }
    Ok(out)
}

// ---------- Reduce (conciliate) ----------

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// Greedy single-link clustering by embedding cosine — the deterministic
/// prefilter; only multi-member clusters spend an LLM merge call.
fn cluster_by_similarity(embeddings: &[Vec<f32>], threshold: f32) -> Vec<Vec<usize>> {
    let mut assigned = vec![false; embeddings.len()];
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for i in 0..embeddings.len() {
        if assigned[i] {
            continue;
        }
        assigned[i] = true;
        let mut cluster = vec![i];
        for (j, done) in assigned.iter_mut().enumerate().skip(i + 1) {
            if !*done && cosine(&embeddings[i], &embeddings[j]) >= threshold {
                *done = true;
                cluster.push(j);
            }
        }
        clusters.push(cluster);
    }
    clusters
}

async fn reduce_candidates(
    llm: &dyn LlmBackend,
    embedder: &Arc<dyn Embedder>,
    workdir: &Path,
    policy: &DocumentPolicy,
    candidates: Vec<CandidateFact>,
) -> Result<Vec<CandidateFact>> {
    if candidates.len() < 2 {
        return Ok(candidates);
    }
    let bodies: Vec<String> = candidates.iter().map(|c| c.body.clone()).collect();
    let embeddings = embedder.embed_batch(&bodies).await?;
    let clusters = cluster_by_similarity(&embeddings, policy.merge_threshold);
    let mut out = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        if cluster.len() == 1 {
            out.push(candidates[cluster[0]].clone());
            continue;
        }
        let members: Vec<&CandidateFact> = cluster.iter().map(|&i| &candidates[i]).collect();
        let system = prompts::load("document-merge", workdir, BUNDLED_DOCUMENT_MERGE_MD)?;
        let mut user = String::from("candidates:\n");
        for (n, m) in members.iter().enumerate() {
            use std::fmt::Write as _;
            let _ = writeln!(user, "  {}. {}", n + 1, m.body);
        }
        let merged: Option<CandidateFact> = match llm
            .complete(
                CompletionRequest::new(user)
                    .with_system(system)
                    .with_temperature(0.1)
                    .with_max_tokens(4_096),
            )
            .await
        {
            Ok(resp) => parse_first_json(&resp.text),
            Err(e) if !matches!(e, LlmError::Invalid(_)) => return Err(e.into()),
            Err(_) => None,
        };
        match merged {
            Some(mut m) if !m.body.trim().is_empty() => {
                // The merge rewrites the BODY; every other field is re-stamped
                // unconditionally from the first cluster member. The merge
                // model sees no scope context (it must never re-route or
                // re-scope) and returns only {"body":…}; any metadata it
                // emits alongside (a fact_type, topics) is discarded here —
                // otherwise fields would zero out or drift on every merge (the
                // parse-failure fallback below clones the whole member; this
                // path must match it, not silently drop metadata).
                let first = members[0];
                m.target_wiki_id.clone_from(&first.target_wiki_id);
                m.target_page.clone_from(&first.target_page);
                m.owner_id.clone_from(&first.owner_id);
                m.allow_ids.clone_from(&first.allow_ids);
                m.fact_type.clone_from(&first.fact_type);
                m.topics.clone_from(&first.topics);
                m.valid_from.clone_from(&first.valid_from);
                m.valid_to.clone_from(&first.valid_to);
                m.salience.clone_from(&first.salience);
                m.style.clone_from(&first.style);
                m.page_description.clone_from(&first.page_description);
                out.push(m);
            },
            _ => out.push(members[0].clone()),
        }
    }
    Ok(out)
}

// ---------- The job processor ----------

fn parse_principal(s: &str) -> Result<Principal> {
    s.parse::<Principal>()
        .map_err(|e| DocumentError::Invalid(format!("principal `{s}`: {e}")))
}

/// Resolve an extracted fact's `owner_id` / `allow_ids` (the strings the
/// extractor returned) into engine principals under the ingest rules.
///
/// A fact's subject/audience follow the SAME rules regardless of source: the
/// extractor decides them just as the message classifier does. The defaults
/// mirror that path — an absent/malformed `owner_id` falls back to the
/// uploader (`fallback_owner`, the job's materialized sender) so a fact is
/// never ownerless; malformed `allow_ids` entries are dropped (never widen on a
/// parse slip). The `sender` stays the uploader, stamped separately by the
/// caller.
fn candidate_acl(cand: &CandidateFact, fallback_owner: &Principal) -> (Principal, Vec<Principal>) {
    let owner = cand
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<Principal>().ok())
        .unwrap_or_else(|| fallback_owner.clone());
    let allow = cand
        .allow_ids
        .iter()
        .filter_map(|s| s.trim().parse::<Principal>().ok())
        .collect();
    (owner, allow)
}

fn job_acl(job: &DocumentJob) -> Result<(Principal, Vec<Principal>, Option<Principal>)> {
    let owner = parse_principal(&job.owner_id)?;
    let allow: Vec<Principal> = match job.allow_ids.as_deref() {
        None | Some("") => Vec::new(),
        Some(json) => serde_json::from_str::<Vec<String>>(json)
            .map_err(|e| DocumentError::Invalid(format!("allow_ids: {e}")))?
            .iter()
            .map(|s| parse_principal(s))
            .collect::<Result<Vec<_>>>()?,
    };
    let sender = job.sender_id.as_deref().map(parse_principal).transpose()?;
    Ok((owner, allow, sender))
}

/// Drive one job through its remaining phases. Re-entrant: every phase
/// checkpoints on the row, so a crash resumes where it left off.
#[allow(clippy::too_many_lines)] // the phase ladder reads top-to-bottom
async fn process_job(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    workdir: &Path,
    policy: &DocumentPolicy,
    mut job: DocumentJob,
) -> Result<()> {
    if job.status == "queued" {
        touch_job(pool, &job.job_id, "status = ?", &["running"]).await?;
        job.status = "running".into();
    }
    let (owner, allow, sender) = job_acl(&job)?;

    // Phase: classify. The plan's testata seeds (topics / style /
    // page_description) ride the anchor capture below; they are held in memory
    // only (not checkpointed on the job row), so a post-restart resume that
    // skips this block anchors with the defaults from the `else` arm.
    let (mut anchor_topics, mut anchor_style, mut anchor_page_description) =
        if job.resolved_disposition.is_none() {
            let input = ClassifyInput {
                text: &job.text,
                title_hint: job.title_hint.as_deref(),
                source_kind: &job.source_kind,
                occurred_at: job.occurred_at.as_deref(),
                forced_disposition: job
                    .disposition_requested
                    .as_deref()
                    .and_then(Disposition::parse),
                forced_format: job.format_requested.as_deref().and_then(DocFormat::parse),
                owner: &owner,
            };
            let plan = classify_document(llm, tree, workdir, policy, &input).await?;
            touch_job(
                pool,
                &job.job_id,
                "resolved_disposition = ?, resolved_format = ?, resolved_title = ?, \
             target_wiki_id = ?, document_page = ?, summary = ?",
                &[
                    plan.disposition.as_str(),
                    plan.format.as_str(),
                    &plan.title,
                    &plan.target_wiki_id,
                    &plan.page.to_string_lossy(),
                    &plan.summary,
                ],
            )
            .await?;
            // Normalize an empty `reduced_json` checkpoint to NULL so the reduce
            // phase's "not yet reduced" test (IS NULL) stays unambiguous.
            sqlx::query(
            "UPDATE document_jobs SET reduced_json = NULL WHERE job_id = ? AND reduced_json = ''",
        )
        .bind(&job.job_id)
        .execute(pool)
        .await?;
            job = find_job(pool, &job.job_id)
                .await?
                .ok_or_else(|| DocumentError::Invalid("job vanished mid-flight".into()))?;
            tracing::info!(
                job_id = job.job_id,
                disposition = job.resolved_disposition.as_deref().unwrap_or("?"),
                wiki = job.target_wiki_id.as_deref().unwrap_or("?"),
                "document: classified"
            );
            (plan.topics, plan.style, plan.page_description)
        } else {
            (Vec::new(), None, None)
        };
    let disposition = job
        .resolved_disposition
        .as_deref()
        .and_then(Disposition::parse)
        .unwrap_or(Disposition::Consult);
    let format = job
        .resolved_format
        .as_deref()
        .and_then(DocFormat::parse)
        .unwrap_or(DocFormat::Prose);
    let target_wiki = job
        .target_wiki_id
        .clone()
        .ok_or_else(|| DocumentError::Invalid("classified job missing target wiki".into()))?;

    // Phase: segment.
    if job.total_segments.is_none() {
        let segments = segment_document(&job.text, format, job.occurred_at.as_deref(), policy);
        if segments.len() > policy.max_segments {
            return Err(DocumentError::Invalid(format!(
                "document segments to {} > max_segments {} — raise the cap or split the document",
                segments.len(),
                policy.max_segments
            )));
        }
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM document_job_segments WHERE job_id = ?")
            .bind(&job.job_id)
            .execute(&mut *tx)
            .await?;
        for (i, seg) in segments.iter().enumerate() {
            sqlx::query(
                "INSERT INTO document_job_segments (job_id, seq, heading, content, occurred_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&job.job_id)
            .bind(i64::try_from(i).unwrap_or(i64::MAX))
            .bind(&seg.heading)
            .bind(&seg.content)
            .bind(&seg.occurred_at)
            .execute(&mut *tx)
            .await?;
        }
        let total = i64::try_from(segments.len()).unwrap_or(i64::MAX);
        sqlx::query("UPDATE document_jobs SET total_segments = ?, updated_at = ? WHERE job_id = ?")
            .bind(total)
            .bind(now())
            .bind(&job.job_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        job.total_segments = Some(total);
    }
    let total_segments = job.total_segments.unwrap_or(0);

    // Phase: anchor (consult / dossier keep the document's identity).
    if matches!(disposition, Disposition::Consult | Disposition::Dossier)
        && job.anchor_fact_id.is_none()
    {
        let mut body = job
            .summary
            .clone()
            .unwrap_or_else(|| job.resolved_title.clone().unwrap_or_default());
        if job.source_kind == "media"
            && let Some(sref) = &job.source_ref
            && let Ok(cid) = CatalogId::parse(sref)
        {
            body.push(' ');
            body.push_str(&capture::render_embed_marker(&cid));
        }
        let wiki_id = WikiId::parse(&target_wiki)
            .map_err(|e| DocumentError::Invalid(format!("target wiki id: {e}")))?;
        // The anchor is the document's own identity fact (subject = the
        // uploader/owner), so its audience is the explicit `allow` that rode the
        // job — no placement-derived widening. A fact's ACL is the fact's, never
        // inherited from where it lands.
        let outcome = capture::wiki_capture_with_source(
            tree,
            pool,
            Arc::clone(embedder),
            CaptureRequest {
                wiki_id,
                page: PathBuf::from(
                    job.document_page
                        .clone()
                        .unwrap_or_else(|| "documento.md".into()),
                ),
                body,
                owner: owner.clone(),
                allow: allow.clone(),
                sender: sender.clone(),
                fact_type: Some("document".into()),
                // Seed the document page's testata from the classify plan.
                topics: std::mem::take(&mut anchor_topics),
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: anchor_style.take(),
                page_description: anchor_page_description.take(),
                salience: None,
                // The anchor IS the document page — it points at nothing;
                // its provenance rides `source_ref` alone. Extracted dossier
                // facts are the ones that carry the page in `authored_refs`.
                authored_refs: Vec::new(),
            },
            job.source_ref.clone(),
        )
        .await?;
        let anchor_id = outcome.fact_id.as_str().to_owned();
        touch_job(pool, &job.job_id, "anchor_fact_id = ?", &[&anchor_id]).await?;
        job.anchor_fact_id = Some(anchor_id);
        // The blob's read set widens to the anchor's — monotone, soft-fail,
        // mirroring the conversational claim path.
        if job.source_kind == "media"
            && let Some(sref) = &job.source_ref
            && let Ok(cid) = CatalogId::parse(sref)
            && let Err(e) =
                crate::media::widen_acl(pool, &cid, &owner, &allow, sender.as_ref()).await
        {
            tracing::warn!(job_id = job.job_id, error = %e, "document: media ACL widening failed (soft)");
        }
    }

    // Phase: extract (map) — consult skips straight to done.
    if !matches!(disposition, Disposition::Consult) {
        // The uploader's groups (id + scope prose) — the audience signal the
        // extractor reads when deciding each fact's `allow_ids`, the same
        // assembly the message classifier uses. A `group:<scope>` device-channel
        // sender (no enrollment row) simply yields no groups.
        let sender_id_str = job.sender_id.as_deref().unwrap_or(&job.owner_id);
        let sender_groups = crate::enrollment::groups_with_scope_for(pool, sender_id_str)
            .await
            .unwrap_or_default();
        // The enrolled roster the extractor resolves a named subject against —
        // the gate that stops `owner_id` minting a `user:<id>` for a person who
        // is not in the system (a relative, a pet). The same roster the message
        // classifier injects; the message path had it, the document path did not
        // — which is how unenrolled `user:<X>` owners were coined.
        let known_users = crate::enrollment::list_users(pool)
            .await
            .unwrap_or_default();
        loop {
            let pending: Option<(i64, Option<String>, String, Option<String>)> = sqlx::query_as(
                "SELECT seq, heading, content, occurred_at FROM document_job_segments
                  WHERE job_id = ? AND status = 'pending' ORDER BY seq ASC LIMIT 1",
            )
            .bind(&job.job_id)
            .fetch_optional(pool)
            .await?;
            let Some((seq, heading, content, seg_at)) = pending else {
                break;
            };
            let segment = Segment {
                heading,
                content,
                occurred_at: seg_at.clone(),
            };
            let facts = extract_segment(
                llm,
                tree,
                workdir,
                policy,
                &job,
                disposition,
                &sender_groups,
                &known_users,
                &segment,
                (seq + 1, total_segments),
            )
            .await;
            match facts {
                Ok(mut facts) => {
                    // Dialogue per-utterance clock: a fact without its own
                    // validity inherits the segment's instant as valid_from.
                    if let Some(at) = &seg_at {
                        for f in &mut facts {
                            if f.valid_from.is_none() {
                                f.valid_from = Some(at.clone());
                            }
                        }
                    }
                    let n = i64::try_from(facts.len()).unwrap_or(0);
                    let json = serde_json::to_string(&facts)
                        .map_err(|e| DocumentError::Invalid(format!("facts_json: {e}")))?;
                    let mut tx = pool.begin().await?;
                    sqlx::query(
                        "UPDATE document_job_segments SET status = 'done', facts_json = ?
                          WHERE job_id = ? AND seq = ?",
                    )
                    .bind(&json)
                    .bind(&job.job_id)
                    .bind(seq)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(
                        "UPDATE document_jobs
                            SET done_segments = done_segments + 1,
                                facts_extracted = facts_extracted + ?,
                                updated_at = ?
                          WHERE job_id = ?",
                    )
                    .bind(n)
                    .bind(now())
                    .bind(&job.job_id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                },
                Err(e) if is_retriable(&e) => return Err(e),
                Err(e) => {
                    // A terminal per-segment failure (parse refusal, …) must
                    // not sink the whole document: record and move on.
                    tracing::warn!(job_id = job.job_id, seq, error = %e, "document: segment failed");
                    sqlx::query(
                        "UPDATE document_job_segments SET status = 'failed', error = ?
                          WHERE job_id = ? AND seq = ?",
                    )
                    .bind(e.to_string())
                    .bind(&job.job_id)
                    .bind(seq)
                    .execute(pool)
                    .await?;
                    sqlx::query(
                        "UPDATE document_jobs SET done_segments = done_segments + 1, updated_at = ? WHERE job_id = ?",
                    )
                    .bind(now())
                    .bind(&job.job_id)
                    .execute(pool)
                    .await?;
                },
            }
        }

        // Phase: reduce.
        if job.reduced_json.is_none() {
            let rows: Vec<(Option<String>,)> = sqlx::query_as(
                "SELECT facts_json FROM document_job_segments
                  WHERE job_id = ? AND status = 'done' ORDER BY seq ASC",
            )
            .bind(&job.job_id)
            .fetch_all(pool)
            .await?;
            let mut candidates: Vec<CandidateFact> = Vec::new();
            for (json,) in rows {
                if let Some(json) = json
                    && let Ok(mut facts) = serde_json::from_str::<Vec<CandidateFact>>(&json)
                {
                    candidates.append(&mut facts);
                }
            }
            let reduced = reduce_candidates(llm, embedder, workdir, policy, candidates).await?;
            let json = serde_json::to_string(&reduced)
                .map_err(|e| DocumentError::Invalid(format!("reduced_json: {e}")))?;
            touch_job(pool, &job.job_id, "reduced_json = ?", &[&json]).await?;
            job.reduced_json = Some(json);
        }

        // Phase: file — buffer each reduced fact from the progress cursor
        // (re-entrant after a crash, no double-buffering).
        let reduced: Vec<CandidateFact> = job
            .reduced_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default();
        let anchor_link = job.document_page.as_deref().map(|p| {
            let stem = p.strip_suffix(".md").unwrap_or(p);
            format!("[[{target_wiki}/{stem}]]")
        });
        let start = usize::try_from(job.facts_buffered).unwrap_or(0);
        for cand in reduced.iter().skip(start) {
            let wiki_str = cand
                .target_wiki_id
                .clone()
                .unwrap_or_else(|| target_wiki.clone());
            let wiki_id = WikiId::parse(&wiki_str)
                .map_err(|e| DocumentError::Invalid(format!("fact wiki id: {e}")))?;
            // A fact is a fact: its subject/audience follow the ingest rules,
            // decided per-fact by the extractor — not derived from where it
            // lands. The owner defaults to the uploader; the audience is the
            // extractor's `allow_ids` (group/wiki scope + document cues).
            let fact_owner_fallback = sender.clone().unwrap_or_else(|| owner.clone());
            let (fact_owner, fact_allow) = candidate_acl(cand, &fact_owner_fallback);
            let page = normalize_capture_page(cand.target_page.as_deref(), Path::new("index.md"));
            let body = cand.body.trim().to_owned();
            // Dossier provenance rides `authored_refs` — the code-built
            // wikilink to the document page (the model never writes links,
            // and the claim text stays clean: no inline `[[…]]` suffix
            // polluting embeddings, dedup, and the Cronista's prose). The
            // compiler projects the ref as a provenance breadcrumb the
            // Cronista weaves as a terse reference; `source_ref` below
            // stays the per-fact audit column.
            let authored_refs = if matches!(disposition, Disposition::Dossier) {
                anchor_link.clone().into_iter().collect()
            } else {
                Vec::new()
            };
            capture_buffer::buffer_capture_with_source(
                tree,
                pool,
                CaptureRequest {
                    wiki_id,
                    page,
                    body,
                    owner: fact_owner,
                    allow: fact_allow,
                    sender: sender.clone(),
                    fact_type: cand.fact_type.clone(),
                    topics: cand.topics.clone(),
                    dedup_threshold: None,
                    valid_from: cand.valid_from.clone(),
                    valid_to: cand.valid_to.clone(),
                    style: cand.style.clone(),
                    page_description: cand.page_description.clone(),
                    salience: cand.salience.clone(),
                    authored_refs,
                },
                None,
                SOURCE_KIND_DOCUMENT,
                job.source_ref
                    .clone()
                    .or_else(|| Some(format!("document-job:{}", job.job_id))),
            )
            .await?;
            sqlx::query(
                "UPDATE document_jobs SET facts_buffered = facts_buffered + 1, updated_at = ? WHERE job_id = ?",
            )
            .bind(now())
            .bind(&job.job_id)
            .execute(pool)
            .await?;
        }
        job.facts_buffered = i64::try_from(reduced.len()).unwrap_or(job.facts_buffered);
    }

    // Phase: done + notice.
    let ts = now();
    sqlx::query(
        "UPDATE document_jobs SET status = 'done', error = NULL, finished_at = ?, updated_at = ? WHERE job_id = ?",
    )
    .bind(&ts)
    .bind(&ts)
    .bind(&job.job_id)
    .execute(pool)
    .await?;
    let payload = serde_json::json!({
        "job_id": job.job_id,
        "disposition": disposition.as_str(),
        "title": job.resolved_title,
        "document_page": matches!(disposition, Disposition::Consult | Disposition::Dossier)
            .then(|| job.document_page.clone())
            .flatten(),
        "facts_buffered": job.facts_buffered,
        "source_ref": job.source_ref,
        "recipient_id": job.owner_id,
    });
    events::insert_event(
        pool,
        EventKind::DocumentIngested,
        Some(&target_wiki),
        job.anchor_fact_id.as_deref(),
        &payload,
    )
    .await?;
    tracing::info!(
        job_id = job.job_id,
        disposition = disposition.as_str(),
        facts = job.facts_buffered,
        "document: job done"
    );
    Ok(())
}

/// One worker tick: pick the oldest runnable job and drive it. Returns
/// `true` when a job was processed (the caller may tick again immediately).
///
/// # Errors
///
/// [`DocumentError::Db`] on queue access; per-job failures are absorbed
/// into the job row (`failed`, or left `running` for a transient error).
pub async fn run_one_job(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    workdir: &Path,
    policy: &DocumentPolicy,
) -> Result<bool> {
    let Some(job) = next_runnable_job(pool).await? else {
        return Ok(false);
    };
    let job_id = job.job_id.clone();
    match process_job(pool, tree, embedder, llm, workdir, policy, job).await {
        Ok(()) => Ok(true),
        Err(e) if is_retriable(&e) => {
            tracing::warn!(job_id, error = %e, "document: transient failure, job stays runnable");
            let _ = touch_job(pool, &job_id, "error = ?", &[&e.to_string()]).await;
            Ok(true)
        },
        Err(e) => {
            tracing::warn!(job_id, error = %e, "document: job FAILED");
            let ts = now();
            sqlx::query(
                "UPDATE document_jobs SET status = 'failed', error = ?, finished_at = ?, updated_at = ? WHERE job_id = ?",
            )
            .bind(e.to_string())
            .bind(&ts)
            .bind(&ts)
            .bind(&job_id)
            .execute(pool)
            .await?;
            Ok(true)
        },
    }
}

/// The document worker loop.
///
/// Polls for runnable jobs every `policy.poll_secs`, drives them serially
/// (one job in flight per deployment — document jobs are rare and heavy),
/// exits on `shutdown`. The LLM backend is built per tick from the
/// `ingest` slot; a missing slot idles the loop (enqueue already refuses
/// without it, so the queue only holds jobs accepted while a slot
/// existed).
pub async fn run_worker_loop<S>(
    pool: SqlitePool,
    tree: WikiTree,
    embedder: Arc<dyn Embedder>,
    llm_config: LlmConfig,
    workdir: PathBuf,
    policy: DocumentPolicy,
    shutdown: S,
) where
    S: std::future::Future<Output = ()> + Send,
{
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(policy.poll_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!("document worker: shutdown");
                return;
            }
            _ = ticker.tick() => {
                let Some(slot) = llm_config.slot(LlmFunction::Ingest) else {
                    continue;
                };
                let llm = match slot.build_backend(LlmFunction::Ingest) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "document worker: ingest slot unbuildable");
                        continue;
                    },
                };
                loop {
                    match run_one_job(&pool, &tree, &embedder, llm.as_ref(), &workdir, &policy).await {
                        Ok(true) => {}, // drain the queue
                        Ok(false) => break, // idle
                        Err(e) => {
                            tracing::warn!(error = %e, "document worker: queue access failed");
                            break;
                        },
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn policy() -> DocumentPolicy {
        DocumentPolicy::default()
    }

    #[test]
    fn disposition_tokens_roundtrip() {
        for d in [
            Disposition::Consult,
            Disposition::Dossier,
            Disposition::Dissolve,
        ] {
            assert_eq!(Disposition::parse(d.as_str()), Some(d));
        }
        assert_eq!(Disposition::parse("shred"), None);
        for f in [DocFormat::Prose, DocFormat::Dialogue] {
            assert_eq!(DocFormat::parse(f.as_str()), Some(f));
        }
    }

    #[test]
    fn prose_segmentation_respects_headings_and_target() {
        let text = "# Manual\n\nIntro paragraph.\n\n## Cleaning\n\nStep one.\n\nStep two.\n";
        let segs = segment_prose(text, &policy());
        assert_eq!(segs.len(), 2, "one segment per section: {segs:?}");
        assert_eq!(segs[0].heading.as_deref(), Some("Manual"));
        assert!(segs[0].content.contains("Intro paragraph."));
        assert_eq!(segs[1].heading.as_deref(), Some("Manual › Cleaning"));
        assert!(segs[1].content.contains("Step two."));
    }

    #[test]
    fn prose_segmentation_packs_to_target() {
        let mut p = policy();
        p.segment_target_chars = 40;
        p.segment_max_chars = 60;
        let text = "Alpha paragraph one.\n\nBeta paragraph two.\n\nGamma paragraph three.";
        let segs = segment_prose(text, &p);
        assert!(segs.len() >= 2, "packing must split: {segs:?}");
        let rejoined: String = segs
            .iter()
            .map(|s| s.content.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rejoined.contains("Alpha") && rejoined.contains("Gamma"));
    }

    #[test]
    fn oversized_paragraph_hard_splits() {
        let mut p = policy();
        p.segment_target_chars = 30;
        p.segment_max_chars = 30;
        let text = "x".repeat(100);
        let segs = segment_prose(&text, &p);
        assert!(segs.len() >= 3, "hard split expected: {}", segs.len());
        assert!(segs.iter().all(|s| s.content.chars().count() <= 30));
    }

    #[test]
    fn dialogue_blocks_carry_bracket_timestamps() {
        let text = "[18:02] frodo: si parte domani.\n\n[18:05] gimli: prenoto io.";
        let mut p = policy();
        p.segment_target_chars = 10; // force one segment per block
        let segs = segment_dialogue(text, Some("2026-06-12T00:00:00Z"), &p);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].occurred_at.as_deref(), Some("2026-06-12T18:02:00Z"));
        assert_eq!(segs[1].occurred_at.as_deref(), Some("2026-06-12T18:05:00Z"));
    }

    #[test]
    fn dialogue_iso_timestamp_detected() {
        let line = "2026-06-12T18:02:00Z frodo: ciao";
        assert_eq!(
            block_timestamp(line, None).as_deref(),
            Some("2026-06-12T18:02:00Z")
        );
        assert_eq!(block_timestamp("frodo: ciao", None), None);
    }

    #[test]
    fn clustering_groups_identical_vectors() {
        let e = vec![vec![1.0, 0.0], vec![1.0, 0.0], vec![0.0, 1.0]];
        let clusters = cluster_by_similarity(&e, 0.9);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0], vec![0, 1]);
        assert_eq!(clusters[1], vec![2]);
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_per_owner_and_text() {
        let pool = make_pool().await;
        let req = EnqueueRequest {
            source_kind: "inline".into(),
            source_ref: None,
            text: "Il manuale della stufa a pellet.".into(),
            title_hint: Some("manuale stufa".into()),
            disposition: None,
            format: None,
            occurred_at: None,
            owner: "user:frodo".parse().unwrap(),
            allow: Vec::new(),
            sender: None,
            force: false,
        };
        let first = enqueue(&pool, &policy(), req.clone()).await.expect("first");
        assert!(!first.existing);
        let second = enqueue(&pool, &policy(), req.clone())
            .await
            .expect("second");
        assert!(second.existing);
        assert_eq!(first.job_id, second.job_id);
        // Same text, different owner → a fresh job.
        let mut other = req.clone();
        other.owner = "user:gimli".parse().unwrap();
        let third = enqueue(&pool, &policy(), other).await.expect("third");
        assert!(!third.existing);
        // force bypasses the idempotency hit.
        let mut forced = req;
        forced.force = true;
        let fourth = enqueue(&pool, &policy(), forced).await.expect("fourth");
        assert!(!fourth.existing);
        assert_ne!(fourth.job_id, first.job_id);
    }

    // A backend popping scripted complete() responses in order — the
    // pipeline calls the slot N times per job (classify + per-segment
    // extract + per-cluster merge).
    struct ScriptedLlm(std::sync::Mutex<std::collections::VecDeque<String>>);
    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self(std::sync::Mutex::new(
                responses.iter().map(|s| (*s).to_owned()).collect(),
            ))
        }
    }
    #[async_trait::async_trait]
    impl LlmBackend for ScriptedLlm {
        fn model_id(&self) -> &'static str {
            "scripted"
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> std::result::Result<crate::llm::CompletionResponse, LlmError> {
            let next = self
                .0
                .lock()
                .expect("script mutex")
                .pop_front()
                .expect("LLM script exhausted — unexpected extra call");
            Ok(crate::llm::CompletionResponse {
                text: next,
                finish_reason: crate::llm::FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage {
                    prompt_tokens: None,
                    completion_tokens: None,
                },
            })
        }
    }

    fn write_wiki(wikis_dir: &std::path::Path, slug: &str, title: &str, wiki_type: &str) {
        let dir = wikis_dir.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: {wiki_type}\nslug: {slug}\ntitle: {title}\nacl_default: 'user:{slug}'\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), &frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# index\n").unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn dossier_job_runs_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        write_wiki(&wikis, "alice", "Alice", "wiki-user");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(
            crate::embedder::FakeEmbedder::with_fixed_embedding("fake", vec![0.1, 0.2, 0.3, 0.4]),
        );
        let llm = ScriptedLlm::new(&[
            // classify
            r#"{"disposition":"dossier","format":"prose","title":"Meeting X","page_slug":"meeting_x.md","target_wiki_id":"alice","summary":"Riunione sul viaggio in Norvegia.","page_description":"dossier del meeting","style":"prosa","topics":["meeting"]}"#,
            // extract (one segment — short document). The extractor decides the
            // fact's subject (`owner_id`) and audience (`allow_ids`) under the
            // ingest rules — here a fact ABOUT Gimli, shared with the team.
            r#"{"facts":[{"body":"Gimli prenota il viaggio in Norvegia entro venerdì 19 giugno 2026.","target_wiki_id":"alice","target_page":"viaggio_norvegia.md","owner_id":"user:gimli","allow_ids":["group:team"],"fact_type":"commitment","topics":["viaggio"]}]}"#,
        ]);

        let outcome = enqueue(
            &pool,
            &policy(),
            EnqueueRequest {
                source_kind: "inline".into(),
                source_ref: None,
                text: "Meeting di oggi: si è deciso che Gimli prenota il viaggio entro venerdì."
                    .into(),
                title_hint: None,
                disposition: None,
                format: None,
                occurred_at: Some("2026-06-12T10:00:00Z".into()),
                owner: "user:alice".parse().unwrap(),
                allow: Vec::new(),
                sender: None,
                force: false,
            },
        )
        .await
        .expect("enqueue");

        let ran = run_one_job(&pool, &tree, &embedder, &llm, dir.path(), &policy())
            .await
            .expect("run");
        assert!(ran);

        let job = find_job(&pool, &outcome.job_id)
            .await
            .expect("find")
            .expect("job exists");
        assert_eq!(job.status, "done", "error: {:?}", job.error);
        assert_eq!(job.resolved_disposition.as_deref(), Some("dossier"));
        assert_eq!(job.document_page.as_deref(), Some("meeting_x.md"));
        assert_eq!(job.facts_buffered, 1);
        let anchor = job.anchor_fact_id.as_deref().expect("anchor fact");

        // The anchor is a live fact on the document page.
        let row =
            crate::fact_index::find_by_id(&pool, &crate::types::FactId::parse(anchor).unwrap())
                .await
                .expect("anchor row")
                .expect("anchor exists");
        assert_eq!(row.wiki_id, "alice");
        assert!(row.source_path.ends_with("meeting_x.md"));
        // The document page's testata is seeded from the classify plan: the
        // anchor fact carries the plan's style / page_description / topics
        // (they ride the anchor capture, not just the job-row checkpoint).
        assert_eq!(row.style.as_deref(), Some("prosa"));
        assert_eq!(row.page_description.as_deref(), Some("dossier del meeting"));
        assert_eq!(row.topics, vec!["meeting".to_owned()]);
        let page = std::fs::read_to_string(wikis.join("alice").join("meeting_x.md")).unwrap();
        assert!(page.contains("Riunione sul viaggio in Norvegia."));

        // The extracted fact sits in the buffer with document provenance:
        // the claim text stays clean (no trailing `([[…]])` link suffix) and
        // the pointer to the dossier page rides `authored_refs` instead.
        let buffered = capture_buffer::find_buffered_in_wiki(&pool, "alice")
            .await
            .expect("buffered");
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].source_kind, SOURCE_KIND_DOCUMENT);
        assert_eq!(
            buffered[0].source_ref.as_deref(),
            Some(format!("document-job:{}", job.job_id).as_str())
        );
        assert_eq!(
            buffered[0].body, "Gimli prenota il viaggio in Norvegia entro venerdì 19 giugno 2026.",
            "the claim text carries no source-link suffix"
        );
        assert!(
            !buffered[0].body.contains("[["),
            "no wikilink inside the claim text: {}",
            buffered[0].body
        );
        assert_eq!(
            buffered[0].authored_refs,
            vec!["[[alice/meeting_x]]".to_owned()],
            "the dossier-page pointer rides authored_refs"
        );
        // A fact is a fact: the extracted fact carries the extractor's
        // owner/allow (subject = Gimli, audience = the team), NOT the job
        // owner (alice) nor a placement-derived default. The sender stays the
        // uploader (alice).
        assert_eq!(
            buffered[0].owner,
            "user:gimli".parse::<Principal>().unwrap(),
            "extracted fact's owner is the LLM-decided subject, not the job owner"
        );
        assert_eq!(
            buffered[0].allow,
            vec!["group:team".parse::<Principal>().unwrap()],
            "extracted fact's audience is the LLM-decided allow"
        );
        assert_eq!(
            buffered[0].sender,
            Some("user:alice".parse::<Principal>().unwrap()),
            "sender stays the uploader"
        );

        // Completion notice emitted.
        let (kind,): (String,) =
            sqlx::query_as("SELECT kind FROM wiki_events ORDER BY id DESC LIMIT 1")
                .fetch_one(&pool)
                .await
                .expect("event");
        assert_eq!(kind, "document_ingested");
        drop(dir);
    }

    #[tokio::test]
    async fn merge_preserves_first_member_metadata() {
        // Two near-duplicate candidates cluster (the fake embedder returns one
        // fixed vector, so cosine == 1), and the merge model must only rewrite
        // the body. Every non-body field must survive as the first member's —
        // routing/ACL, taxonomy, AND validity/salience/testata seeds — even
        // when the model hallucinates a different fact_type/topics pair
        // alongside the body (the contract is {"body":…} only; anything else
        // is discarded).
        let dir = tempfile::tempdir().unwrap();
        let embedder: Arc<dyn Embedder> = Arc::new(
            crate::embedder::FakeEmbedder::with_fixed_embedding("fake", vec![0.1, 0.2, 0.3, 0.4]),
        );
        let llm = ScriptedLlm::new(&[
            r#"{"body":"Gimli prenota il viaggio in Norvegia.","fact_type":"preference","topics":["cucina"]}"#,
        ]);
        let first = CandidateFact {
            body: "Gimli prenota il viaggio entro venerdì.".into(),
            target_wiki_id: Some("alice".into()),
            target_page: Some("viaggio_norvegia.md".into()),
            owner_id: Some("user:gimli".into()),
            allow_ids: vec!["group:team".into()],
            fact_type: Some("commitment".into()),
            topics: vec!["viaggio".into()],
            valid_from: Some("2026-06-12T00:00:00Z".into()),
            valid_to: Some("2026-06-19T00:00:00Z".into()),
            salience: Some("high".into()),
            style: Some("lista".into()),
            page_description: Some("il viaggio in Norvegia".into()),
        };
        let second = CandidateFact {
            body: "Gimli si occupa della prenotazione del viaggio.".into(),
            ..first.clone()
        };
        let out = reduce_candidates(&llm, &embedder, dir.path(), &policy(), vec![first, second])
            .await
            .expect("reduce");
        assert_eq!(out.len(), 1, "the cluster folds to one fact");
        let m = &out[0];
        assert_eq!(m.body, "Gimli prenota il viaggio in Norvegia.");
        // Validity / salience / testata seeds preserved (the regression).
        assert_eq!(m.valid_from.as_deref(), Some("2026-06-12T00:00:00Z"));
        assert_eq!(m.valid_to.as_deref(), Some("2026-06-19T00:00:00Z"));
        assert_eq!(m.salience.as_deref(), Some("high"));
        assert_eq!(m.style.as_deref(), Some("lista"));
        assert_eq!(
            m.page_description.as_deref(),
            Some("il viaggio in Norvegia")
        );
        // Routing / ACL still the first member's.
        assert_eq!(m.target_wiki_id.as_deref(), Some("alice"));
        assert_eq!(m.target_page.as_deref(), Some("viaggio_norvegia.md"));
        assert_eq!(m.owner_id.as_deref(), Some("user:gimli"));
        assert_eq!(m.allow_ids, vec!["group:team".to_owned()]);
        // Taxonomy: the first member's wins over the hallucinated
        // "preference"/"cucina" pair the scripted reply smuggled in.
        assert_eq!(m.fact_type.as_deref(), Some("commitment"));
        assert_eq!(m.topics, vec!["viaggio".to_owned()]);
    }

    #[tokio::test]
    async fn enqueue_refuses_empty_and_oversized() {
        let pool = make_pool().await;
        let mut p = policy();
        p.max_document_chars = 10;
        let base = EnqueueRequest {
            source_kind: "inline".into(),
            source_ref: None,
            text: "   ".into(),
            title_hint: None,
            disposition: None,
            format: None,
            occurred_at: None,
            owner: "user:frodo".parse().unwrap(),
            allow: Vec::new(),
            sender: None,
            force: false,
        };
        assert!(enqueue(&pool, &p, base.clone()).await.is_err());
        let mut big = base;
        big.text = "x".repeat(50);
        assert!(enqueue(&pool, &p, big).await.is_err());
    }
}
