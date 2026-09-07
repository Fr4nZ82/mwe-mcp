// SPDX-License-Identifier: AGPL-3.0-or-later
//! Structure-proposal lifecycle: list + apply / auto-apply chassis.
//!
//! Wraps the `structure_proposals` table and ships a 3-state lifecycle:
//!
//! ```text
//!  pending ── apply (manual, apply_mode='manual') ──────────────────► applied
//!     │
//!     ├── auto_apply (sweep on timeout_at, apply_mode='auto') ──────► applied
//!     │
//!     └── auto-apply sweep failed past grace (LLM down, validation broken) ──► expired
//! ```
//!
//! A structural change is never undone. The memory reorganises itself
//! and the reorganisation stands: the user steers it by talking to the
//! agent, not by rolling a change back.
//!
//! ## Scope
//!
//! [`apply_proposal`] keeps the manual path (`pending → applied`,
//! `apply_mode='manual'`); [`auto_apply_proposal`] drives the sweep path
//! (same destination, `apply_mode='auto'`) and its wiring is
//! [`auto_apply_overdue_proposals`]. The per-kind handlers
//! ([`crate::promote`], [`crate::dedup`]) are reused verbatim by both —
//! the difference between manual and auto-apply is in the state-flip
//! helper that runs after the handler succeeds, not in the handler
//! itself.
//!
//! ## Act-first structural changes
//!
//! The two structural rungs (`wiki_promote` `paragraph_to_file` /
//! `pages_to_new_wiki`) never enter `pending`: REM applies them directly
//! and records a **born-applied** receipt via [`emit_applied_proposal`]
//! (status `applied` at insert). The receipt is a record of what the
//! engine did on its own, not an offer to undo it. The pending lifecycle
//! above is for the kinds that really are a question: `dedup_merge` and
//! `fact_forget`.
//!
//! ## MCP exposure
//!
//! None. The whole `structure_proposal_*` family is off the MCP surface:
//! consumers learn about applied structural changes from the
//! `structure_applied` event, and the write entry point
//! ([`apply_proposal`]) is consumed exclusively by the built-in
//! dashboard, which calls it directly.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

use crate::wiki::WikiTree;

// ---------- canonical kind names ----------

/// Canonical `structure_proposals.kind` string constants.
///
/// The chassis rejects any other value as `unknown_kind`.
pub mod kind {
    /// Promote facts paragraph→page, or a group of pages into a wiki of their own.
    pub const WIKI_PROMOTE: &str = "wiki_promote";
    /// Merge two near-duplicate facts after REM semantic dedup.
    pub const DEDUP_MERGE: &str = "dedup_merge";
    /// A non-sender subject's request to forget one fact, put to the fact's
    /// audience as a propose-first vote ([`crate::votes`]).
    ///
    /// The proposal opens `pending` (the fact stays active); a NO-majority
    /// within the window rejects it, and silence (or an all-voted quorum with no
    /// NO-majority) applies it — tombstoning the fact.
    pub const FACT_FORGET: &str = "fact_forget";
    /// **Receipt only** — the planner minted a page nobody asked for.
    ///
    /// A concept page the Cartografo proposed for facts that fitted no
    /// existing one. Emitted **born-applied**
    /// ([`super::emit_applied_proposal`]): the nightly pass cannot stop and
    /// wait for an answer, but the operator must be able to *see* what the
    /// machine invented.
    ///
    /// Without it a page the engine invented is a silent write: the memory
    /// gains a subject nobody named, and there is no line anywhere for the
    /// owner to read (founder, 2026-08-04).
    pub const PAGE_CREATE: &str = "page_create";

    /// **Receipt only** — the REM decided a page should link somewhere.
    ///
    /// The link is a decision, not prose: the plan carries it and the next
    /// rewrite of that page is required to write it. Emitted **born-applied**
    /// for the same reason as [`PAGE_CREATE`] — the nightly pass cannot stop
    /// and wait — and for the same purpose: what the engine decided about the
    /// shape of the memory must be readable by the person whose memory it is.
    pub const RAIL_ADD: &str = "rail_add";

    /// Every canonical kind.
    pub const ALL: &[&str] = &[
        WIKI_PROMOTE,
        DEDUP_MERGE,
        FACT_FORGET,
        PAGE_CREATE,
        RAIL_ADD,
    ];

    /// `true` when `s` matches one of the canonical kinds.
    #[must_use]
    pub fn is_canonical(s: &str) -> bool {
        ALL.contains(&s)
    }
}

// ---------- Errors ----------

/// Errors raised by the read side ([`list`]).
#[derive(Debug, Error)]
pub enum ProposalsError {
    /// Underlying SQL failure.
    #[error("proposals db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON deserialisation failure for the `questions` / `context` /
    /// `spec` columns (shape drift).
    #[error("proposals json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Errors raised by [`apply_proposal`] and the state-flip helper
/// [`mark_applied`]. The variants are what the dashboard renders when an
/// operator applies a proposal; nothing here reaches the MCP surface.
#[derive(Debug, Error)]
pub enum ApplyError {
    /// Underlying SQL failure.
    #[error("proposals db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON (de)serialisation failure.
    #[error("proposals json: {0}")]
    Json(#[from] serde_json::Error),
    /// No row with the requested `proposal_id`.
    #[error("proposal not found: {0}")]
    NotFound(String),
    /// The proposal is not in `pending` status — already applied,
    /// expired, or raced with another writer.
    #[error("proposal {proposal_id} not pending (current status: {status})")]
    NotPending {
        /// Identifier of the proposal.
        proposal_id: String,
        /// Status observed at the time of the failed transition.
        status: String,
    },
    /// `kind` is one of the four canonical names but its handler
    /// is not yet shipped.
    #[error("kind {0} handler not yet implemented")]
    KindNotYetImplemented(String),
    /// `kind` is not one of the four canonical names — DB drift or
    /// an emit path writing an unknown kind.
    #[error("unknown proposal kind: {0}")]
    UnknownKind(String),
    /// Kind handler rejected the request shape — context / answers
    /// malformed, missing field, invalid path, etc. Maps to MCP
    /// `invalid_input`.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    /// Kind handler hit a filesystem error during apply (read/write,
    /// permission denied, `atomic_write` failure). Maps to MCP
    /// `internal_error`.
    #[error("handler io: {0}")]
    HandlerIo(String),
    /// Kind handler hit a data-consistency error during apply (fact
    /// missing from index, marker not present on disk, etc.). Maps to
    /// MCP `internal_error` — the wire signal is "the operation could
    /// not proceed because the world is not in the state the handler
    /// expected".
    #[error("handler data: {0}")]
    HandlerData(String),
    /// The caller is neither the proposal's `recipient_id` (addressee)
    /// nor an admin. Surfaced when a non-admin tries to apply a
    /// proposal addressed to a different user. Maps to MCP `forbidden`.
    #[error("caller {caller} not authorized to apply {proposal_id}")]
    NotAuthorized {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `sender_id` of the caller that was refused.
        caller: String,
    },
}

/// Result alias for the read side.
pub type Result<T> = std::result::Result<T, ProposalsError>;

// ---------- Status enum ----------

/// Lifecycle status of a structure proposal row (3-state model).
///
/// ```text
/// pending ── user apply (manual) ─────────────────────► applied
///    │
///    ├── 24h timeout, sweep auto-apply ──────────────► applied
///    │
///    └── auto-apply sweep failed (LLM down, validation broken) ──► expired
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Awaiting an answer from the user.
    Pending,
    /// Applied — either manually by the user via the dashboard
    /// (`apply_mode = 'manual'`) or by the timeout sweep with the
    /// `recommended` answers (`apply_mode = 'auto'`).
    Applied,
    /// Past `timeout_at` without a successful apply (e.g. auto-apply sweep
    /// failed because the LLM was down or validation rejected the
    /// `recommended` answers).
    Expired,
}

impl ProposalStatus {
    /// Wire string matching the `status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Expired => "expired",
        }
    }
}

impl FromStr for ProposalStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "applied" => Ok(Self::Applied),
            "expired" => Ok(Self::Expired),
            other => Err(other.to_owned()),
        }
    }
}

// ---------- List filters and rows ----------

/// Filters for [`list`]. Every field optional + AND-combined.
#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    /// Defaults to `Pending` in the dispatcher; `None` lifts the
    /// filter and returns rows in every state.
    pub status: Option<ProposalStatus>,
    /// Optional kind filter (e.g. `"wiki_promote"`).
    pub kind: Option<String>,
    /// Optional recipient scope. When `Some(principal)` — a
    /// `Principal` wire string like `"user:frodo"` — the listing is
    /// narrowed to rows addressed to that principal **or** unaddressed
    /// (`recipient_id IS NULL`, the admin-fallback bucket). `None` lifts
    /// the scope (admin view: every recipient).
    pub recipient: Option<String>,
    /// Page size; defaults to [`DEFAULT_LIST_TOP_K`].
    pub top_k: Option<i64>,
}

/// Default page size for [`list`].
pub const DEFAULT_LIST_TOP_K: i64 = 20;
/// Max page size for [`list`].
pub const MAX_LIST_TOP_K: i64 = 50;
/// Distinguishes the two paths that produce an `applied` row.
///
/// Stamped in the `apply_mode` column and surfaced in
/// [`ApplyOutcome`] / [`AutoApplyOutcome`] / list payloads so callers
/// can tell whether the user explicitly approved or the sweep
/// auto-applied on their behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    /// User answered the questionnaire in the dashboard within the
    /// 24 h pending window.
    Manual,
    /// Sweep auto-applied with the `recommended` answers after
    /// `timeout_at` elapsed (path stays `apply_mode='auto'` even after
    /// a subsequent confirm).
    Auto,
}

impl ApplyMode {
    /// Wire string matching the `apply_mode` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// One row in the listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalRow {
    /// Opaque proposal id.
    pub proposal_id: String,
    /// Discriminator kind (one of [`kind::ALL`]).
    pub kind: String,
    /// Decoded JSON context (`{ intent, sample_block, source_wiki_id, … }`).
    pub context: Value,
    /// Decoded JSON questions (`[{ id, text, options:[…] }]`).
    pub questions: Value,
    /// ISO 8601 timestamp the row was emitted.
    pub emitted_at: String,
    /// ISO 8601 auto-apply deadline (`proposed_at + 24h` by default).
    pub expires_at: String,
    /// Current lifecycle status.
    pub status: ProposalStatus,
    /// ISO 8601 timestamp of the apply transition, if any. `None` for
    /// `pending` rows.
    pub applied_at: Option<String>,
    /// `sender_id` that applied, or `None` for auto-apply at timeout.
    pub applied_by: Option<String>,
    /// Addressee of the proposal: a `Principal` wire string like
    /// `"user:frodo"`, or `None` for unaddressed / admin-fallback rows.
    pub recipient_id: Option<String>,
}

type ListTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Enumerate structure proposals matching `filters`. Rows are returned
/// `proposed_at DESC` (newest first).
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
/// - [`ProposalsError::Json`] when a stored `questions` / `context`
///   blob is no longer valid JSON.
pub async fn list(pool: &SqlitePool, filters: &ListFilters) -> Result<Vec<ProposalRow>> {
    let limit = filters
        .top_k
        .unwrap_or(DEFAULT_LIST_TOP_K)
        .clamp(1, MAX_LIST_TOP_K);

    let mut sql = String::from(
        "SELECT proposal_id, kind, context, questions, proposed_at, timeout_at, status,
                applied_at, applied_by, recipient_id
           FROM structure_proposals",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    if filters.status.is_some() {
        clauses.push("status = ?");
    }
    if filters.kind.is_some() {
        clauses.push("kind = ?");
    }
    if filters.recipient.is_some() {
        // 0032: addressed to me OR unaddressed (admin-fallback bucket).
        clauses.push("(recipient_id = ? OR recipient_id IS NULL)");
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY proposed_at DESC, proposal_id DESC LIMIT ?");

    let mut q = sqlx::query_as::<_, ListTuple>(&sql);
    if let Some(s) = filters.status {
        q = q.bind(s.as_str());
    }
    if let Some(k) = &filters.kind {
        q = q.bind(k);
    }
    if let Some(r) = &filters.recipient {
        q = q.bind(r);
    }
    let rows: Vec<ListTuple> = q.bind(limit).fetch_all(pool).await?;

    let mut out = Vec::with_capacity(rows.len());
    for (
        proposal_id,
        kind,
        context,
        questions,
        proposed_at,
        timeout_at,
        status,
        applied_at,
        applied_by,
        recipient_id,
    ) in rows
    {
        let context: Value = serde_json::from_str(&context)?;
        let questions: Value = serde_json::from_str(&questions)?;
        out.push(ProposalRow {
            proposal_id,
            kind,
            context,
            questions,
            emitted_at: proposed_at,
            expires_at: timeout_at,
            status: parse_status_lenient(&status),
            applied_at,
            applied_by,
            recipient_id,
        });
    }
    Ok(out)
}

fn parse_status_lenient(s: &str) -> ProposalStatus {
    ProposalStatus::from_str(s).unwrap_or(ProposalStatus::Pending)
}

// ---------- In-flight count ----------

/// Count the `pending` rows — the proposals still waiting on somebody.
///
/// The only rows anybody can still act on: once a proposal is applied
/// the change stands. Scoped by `recipient`: pass
/// `Some("user:<id>")` to count only the rows addressed to that user
/// plus the unaddressed/admin-fallback ones, or `None` for the
/// deployment-wide count (admin view).
///
/// Used by the dashboard in-flight badge.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
pub async fn count_pending(pool: &SqlitePool, recipient: Option<&str>) -> Result<i64> {
    let mut sql = String::from("SELECT COUNT(*) FROM structure_proposals WHERE status = 'pending'");
    if recipient.is_some() {
        // 0032: scope to the caller — addressed to me OR unaddressed.
        sql.push_str(" AND (recipient_id = ? OR recipient_id IS NULL)");
    }
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    if let Some(r) = recipient {
        q = q.bind(r);
    }
    Ok(q.fetch_one(pool).await?)
}

// ---------- Recipient (addressee) derivation + authorization ----------

/// Derive the recipient (addressee) of a proposal from the fact that
/// triggered it.
///
/// The human who actually said it (`sender_id`) wins; otherwise the
/// owning user; otherwise `None` (a group/global subject with no sender →
/// unaddressed / admin-fallback). The returned string, when `Some`, is a
/// `Principal` wire string like `"user:frodo"`, matching `subject_id` /
/// `sender_id` on the fact and the `recipient_id` column.
///
/// This is the single policy knob for "who gets notified about a
/// proposal" — change it here and every emitter follows.
#[must_use]
pub fn recipient_from_fact(
    subject_id: &crate::types::Principal,
    sender_id: Option<&crate::types::Principal>,
) -> Option<String> {
    use crate::types::Principal;
    if let Some(Principal::User(id)) = sender_id {
        return Some(format!("user:{id}"));
    }
    if let Principal::User(id) = subject_id {
        return Some(format!("user:{id}"));
    }
    None
}

/// Whether `caller_sender_id` may apply a proposal whose addressee is
/// `recipient_id`.
///
/// Admins always may. An unaddressed proposal (`recipient_id == None` —
/// the admin-fallback bucket) stays actionable by anyone, preserving the
/// pre-0032 single-operator behaviour. Otherwise only the addressed user
/// may act. `recipient_id`, when `Some`, is a `Principal` wire string
/// (`"user:<id>"`); `caller_sender_id` is the bare session id.
#[must_use]
pub fn recipient_can_act(
    recipient_id: Option<&str>,
    caller_sender_id: &str,
    is_admin: bool,
) -> bool {
    if is_admin {
        return true;
    }
    recipient_id.is_none_or(|r| {
        r.strip_prefix("user:")
            .is_some_and(|u| u == caller_sender_id)
    })
}

// ---------- Apply / AutoApply outcomes ----------

/// Successful outcome of [`apply_proposal`] — the manual path
/// `pending → applied`.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// Identifier of the applied proposal.
    pub proposal_id: String,
    /// Kind that was applied (one of [`kind::ALL`]).
    pub kind: String,
    /// ISO 8601 timestamp of the flip (server clock).
    pub applied_at: String,
    /// `sender_id` who applied. Always `Some` for the manual path.
    pub applied_by: Option<String>,
    /// Always [`ApplyMode::Manual`] for this outcome.
    ///
    /// Surfaced so a uniform wire shape can carry both paths without a
    /// discriminator.
    pub apply_mode: ApplyMode,
}

/// Successful outcome of [`auto_apply_proposal`] — the sweep path
/// `pending → applied`.
#[derive(Debug, Clone)]
pub struct AutoApplyOutcome {
    /// Identifier of the auto-applied proposal.
    pub proposal_id: String,
    /// Kind that was auto-applied (one of [`kind::ALL`]).
    pub kind: String,
    /// ISO 8601 timestamp of the flip (server clock).
    pub applied_at: String,
    /// Always [`ApplyMode::Auto`] for this outcome.
    pub apply_mode: ApplyMode,
}

// ---------- Apply path ----------

/// Apply a pending proposal. Atomic single-row state machine:
///
/// 1. Load the row, fail with [`ApplyError::NotFound`] if missing,
///    [`ApplyError::NotPending`] if the status is not `pending`.
/// 2. Validate the kind name against [`kind::ALL`]
///    ([`ApplyError::UnknownKind`] otherwise).
/// 3. Dispatch to the per-kind handler. Kinds whose handler is not yet
///    shipped return [`ApplyError::KindNotYetImplemented`].
/// 4. Race-safe flip via `UPDATE … WHERE status = 'pending'`. If 0 rows
///    are touched (another writer raced us between step 1 and step 4),
///    return [`ApplyError::NotPending`] with `status = "race"`.
///
/// `applied_by` is recorded verbatim — pass the sender for user-driven
/// apply, `None` for auto-apply at `timeout_at`.
///
/// # Errors
///
/// See [`ApplyError`].
pub async fn apply_proposal(
    pool: &SqlitePool,
    tree: &WikiTree,
    proposal_id: &str,
    answers: &Value,
    applied_by: Option<&str>,
    is_admin: bool,
) -> std::result::Result<ApplyOutcome, ApplyError> {
    let row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT kind, status, context, recipient_id FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    let (kind, status, context_raw, recipient_id) =
        row.ok_or_else(|| ApplyError::NotFound(proposal_id.to_owned()))?;
    if status != ProposalStatus::Pending.as_str() {
        return Err(ApplyError::NotPending {
            proposal_id: proposal_id.to_owned(),
            status,
        });
    }
    // 0032: only the addressee or an admin may apply.
    let caller = applied_by.unwrap_or("");
    if !recipient_can_act(recipient_id.as_deref(), caller, is_admin) {
        return Err(ApplyError::NotAuthorized {
            proposal_id: proposal_id.to_owned(),
            caller: caller.to_owned(),
        });
    }
    if !kind::is_canonical(&kind) {
        return Err(ApplyError::UnknownKind(kind));
    }
    let context: Value = serde_json::from_str(&context_raw)?;

    let spec = dispatch_apply_kind(pool, tree, &kind, &context, answers).await?;

    mark_applied(pool, proposal_id, applied_by, answers, spec.as_ref()).await
}

/// Dispatch table for per-kind apply handlers.
///
/// Returns the consolidated `spec` JSON the handler produced (or `None`
/// for handlers that have no post-apply spec). Kinds whose handler is
/// not yet shipped return [`ApplyError::KindNotYetImplemented`].
async fn dispatch_apply_kind(
    pool: &SqlitePool,
    tree: &WikiTree,
    kind: &str,
    context: &Value,
    answers: &Value,
) -> std::result::Result<Option<Value>, ApplyError> {
    match kind {
        kind::WIKI_PROMOTE => {
            let spec = crate::promote::apply_wiki_promote(pool, tree, context, answers).await?;
            Ok(Some(spec))
        },
        kind::DEDUP_MERGE => {
            let spec = crate::dedup::apply_dedup_merge(pool, tree, context, answers).await?;
            Ok(Some(spec))
        },
        kind::FACT_FORGET => {
            let spec = apply_fact_forget(pool, context).await?;
            Ok(Some(spec))
        },
        // `PAGE_CREATE` is never reachable: it is emitted born-applied, so
        // it is never `pending` and this dispatcher never sees it — a
        // receipt, not a missing handler.
        kind::PAGE_CREATE => Err(ApplyError::KindNotYetImplemented(kind.to_owned())),
        other => Err(ApplyError::UnknownKind(other.to_owned())),
    }
}

/// Apply a `fact_forget` proposal: tombstone the fact named in its `context`.
///
/// The terminal step of a non-sender subject's forget vote ([`crate::votes`]): the
/// audience's silence (or an all-voted quorum) consented, so the fact is
/// forgotten via [`crate::fact_index::mark_forgotten`] (reason
/// `"fact_forget_vote"`). Returns a small spec recording the tombstoned id (so
/// the row's `spec` column is non-NULL for the listing / audit). A
/// vote-resolved deletion is final: the only way back is re-stating the fact.
///
/// DB half only: the apply chassis carries no tree/embedder, so the retired
/// region's on-disk bytes are **not** excised here. The all-voted path in
/// [`crate::votes::cast_vote`] strips act-time right after this apply; the
/// silent-deadline sweep path leaves the residue (fail-closed-redacted by the
/// active ACL map) for the light-dream hygiene sweep
/// ([`crate::reindex::sweep_retired_regions`]).
async fn apply_fact_forget(
    pool: &SqlitePool,
    context: &Value,
) -> std::result::Result<Value, ApplyError> {
    let fact_id_str = context
        .get("fact_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApplyError::InvalidPayload("fact_forget context missing fact_id".into()))?;
    let fact_id = crate::types::FactId::parse(fact_id_str).map_err(|e| {
        ApplyError::InvalidPayload(format!("fact_forget fact_id {fact_id_str:?}: {e}"))
    })?;
    let touched = crate::fact_index::mark_forgotten(pool, &fact_id, "fact_forget_vote")
        .await
        .map_err(|e| ApplyError::HandlerData(format!("fact_forget tombstone: {e}")))?;
    Ok(serde_json::json!({
        "variant": "fact_forget",
        "fact_id": fact_id_str,
        "tombstoned": touched,
    }))
}

/// Internal state-flip for the manual apply path: bump
/// `pending → applied`, stamp `applied_at`, `applied_by`, `answers`,
/// optional `spec`, `apply_mode='manual'`. Returns the [`ApplyOutcome`]
/// on success.
///
/// Exposed at `pub(crate)` so the test module can exercise the flip
/// without going through [`dispatch_apply_kind`]. Production callers
/// always reach it via [`apply_proposal`].
///
/// # Errors
///
/// - [`ApplyError::NotPending`] if the row was modified concurrently
///   and the conditional `UPDATE` matched zero rows.
/// - [`ApplyError::Db`] / [`ApplyError::Json`] for sqlx / serde failures.
pub(crate) async fn mark_applied(
    pool: &SqlitePool,
    proposal_id: &str,
    applied_by: Option<&str>,
    answers: &Value,
    spec: Option<&Value>,
) -> std::result::Result<ApplyOutcome, ApplyError> {
    let applied_at = chrono::Utc::now();
    let applied_at_str = applied_at.to_rfc3339();
    let answers_json = serde_json::to_string(answers)?;
    let spec_json = spec.map(serde_json::to_string).transpose()?;

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied',
                apply_mode = 'manual',
                applied_at = ?,
                applied_by = ?,
                answers = ?,
                spec = ?
          WHERE proposal_id = ? AND status = 'pending'",
    )
    .bind(&applied_at_str)
    .bind(applied_by)
    .bind(&answers_json)
    .bind(spec_json.as_deref())
    .bind(proposal_id)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(ApplyError::NotPending {
            proposal_id: proposal_id.to_owned(),
            status: "race".to_owned(),
        });
    }

    let (kind,): (String,) =
        sqlx::query_as("SELECT kind FROM structure_proposals WHERE proposal_id = ?")
            .bind(proposal_id)
            .fetch_one(pool)
            .await?;

    tracing::info!(
        proposal_id,
        kind = %kind,
        applied_by = applied_by.unwrap_or("<auto>"),
        apply_mode = ApplyMode::Manual.as_str(),
        "proposals: applied",
    );

    Ok(ApplyOutcome {
        proposal_id: proposal_id.to_owned(),
        kind,
        applied_at: applied_at_str,
        applied_by: applied_by.map(str::to_owned),
        apply_mode: ApplyMode::Manual,
    })
}

// ---------- fact_forget vote resolution (shared apply / expire) ----------

/// Apply a pending `fact_forget` proposal **now** as the resolution of its vote
/// (the audience consented — an all-voted quorum in [`crate::votes::cast_vote`],
/// or silence past the deadline in the sweep).
///
/// Runs the manual apply path (`pending → applied`, tombstoning the fact via
/// [`dispatch_apply_kind`]) on system authority (`is_admin = true`, no
/// `applied_by`). `answers` is irrelevant for this kind (the handler reads
/// `fact_id` from the context), so an empty object is passed.
///
/// # Errors
///
/// [`ApplyError`] from the apply path (missing row, not pending, handler
/// failure) or the post-apply token-clear UPDATE.
pub(crate) async fn apply_fact_forget_now(
    pool: &SqlitePool,
    tree: &WikiTree,
    proposal_id: &str,
) -> std::result::Result<(), ApplyError> {
    let empty_answers = Value::Object(serde_json::Map::default());
    apply_proposal(pool, tree, proposal_id, &empty_answers, None, true).await?;
    Ok(())
}

/// Resolve a single overdue pending `fact_forget` at its voting deadline
/// (silence = consent).
///
/// Reads the `eligible_voters` from the row's `context`, tallies the NO votes,
/// and either **applies** the forget (`Ok(Some(()))` — no NO-majority, the
/// audience let it stand) via [`apply_fact_forget_now`], or **expires** it
/// (`Ok(None)` — a NO-majority is on record, the fact stays). The all-yes early
/// resolution is handled live in [`crate::votes::cast_vote`]; this is the
/// silent-deadline path the sweep drives.
///
/// # Errors
///
/// [`ApplyError`] from the context decode, the tally query, or the apply path.
async fn resolve_overdue_fact_forget(
    pool: &SqlitePool,
    tree: &WikiTree,
    proposal_id: &str,
) -> std::result::Result<Option<()>, ApplyError> {
    let context_raw: String =
        sqlx::query_scalar("SELECT context FROM structure_proposals WHERE proposal_id = ?")
            .bind(proposal_id)
            .fetch_one(pool)
            .await?;
    let context: Value = serde_json::from_str(&context_raw)?;
    let eligible = context
        .get("eligible_voters")
        .and_then(Value::as_array)
        .map_or(0usize, Vec::len);
    let no_votes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM structure_proposal_votes WHERE proposal_id = ? AND vote = 'no'",
    )
    .bind(proposal_id)
    .fetch_one(pool)
    .await?;
    let eligible_n = i64::try_from(eligible).unwrap_or(i64::MAX);
    if no_votes * 2 > eligible_n {
        // A NO-majority is recorded — silence does NOT override it; close the
        // request without applying (defensive: cast_vote normally already did).
        expire_pending_proposal(pool, proposal_id).await?;
        return Ok(None);
    }
    apply_fact_forget_now(pool, tree, proposal_id).await?;
    Ok(Some(()))
}

/// Flip a pending proposal to `expired` (race-safe on `status = 'pending'`).
///
/// The terminal "never applied" transition a blocked `fact_forget` takes when a
/// NO-majority of its audience rejects the request ([`crate::votes::cast_vote`]):
/// the fact stays active, the proposal is closed. Returns the rows affected
/// (0 if a concurrent writer moved the row first).
///
/// # Errors
///
/// [`ApplyError::Db`] for the UPDATE.
pub(crate) async fn expire_pending_proposal(
    pool: &SqlitePool,
    proposal_id: &str,
) -> std::result::Result<u64, ApplyError> {
    let rows = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'expired'
          WHERE proposal_id = ? AND status = 'pending'",
    )
    .bind(proposal_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

// ---------- Auto-apply path (sweep) ----------

/// Auto-apply a pending proposal with `recommended` answers.
///
/// Mirrors [`apply_proposal`] step-for-step; the only difference is
/// `apply_mode`, stamped `'auto'` instead of `'manual'`.
///
/// Production callers go through the auto-apply sweep; the synchronous
/// surface exists so tests and the dashboard "force auto-apply" debug
/// affordance can drive the path directly.
///
/// # Errors
///
/// Same shape as [`apply_proposal`]: [`ApplyError::NotFound`],
/// [`ApplyError::NotPending`], [`ApplyError::UnknownKind`],
/// [`ApplyError::KindNotYetImplemented`], handler / DB / JSON errors.
pub async fn auto_apply_proposal(
    pool: &SqlitePool,
    tree: &WikiTree,
    proposal_id: &str,
    answers: &Value,
) -> std::result::Result<AutoApplyOutcome, ApplyError> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT kind, status, context FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    let (kind, status, context_raw) =
        row.ok_or_else(|| ApplyError::NotFound(proposal_id.to_owned()))?;
    if status != ProposalStatus::Pending.as_str() {
        return Err(ApplyError::NotPending {
            proposal_id: proposal_id.to_owned(),
            status,
        });
    }
    if !kind::is_canonical(&kind) {
        return Err(ApplyError::UnknownKind(kind));
    }
    let context: Value = serde_json::from_str(&context_raw)?;

    let spec = dispatch_apply_kind(pool, tree, &kind, &context, answers).await?;

    mark_auto_applied(pool, proposal_id, answers, spec.as_ref()).await
}

/// Internal state-flip for the auto-apply path: bump
/// `pending → applied`, stamp `apply_mode='auto'`, `applied_at`,
/// `applied_by=NULL`, `answers` and the optional `spec`.
///
/// # Errors
///
/// - [`ApplyError::NotPending`] if the row was modified concurrently.
/// - [`ApplyError::Db`] / [`ApplyError::Json`] for sqlx / serde failures.
pub(crate) async fn mark_auto_applied(
    pool: &SqlitePool,
    proposal_id: &str,
    answers: &Value,
    spec: Option<&Value>,
) -> std::result::Result<AutoApplyOutcome, ApplyError> {
    let applied_at = chrono::Utc::now();
    let applied_at_str = applied_at.to_rfc3339();
    let answers_json = serde_json::to_string(answers)?;
    let spec_json = spec.map(serde_json::to_string).transpose()?;

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied',
                apply_mode = 'auto',
                applied_at = ?,
                applied_by = NULL,
                answers = ?,
                spec = ?
          WHERE proposal_id = ? AND status = 'pending'",
    )
    .bind(&applied_at_str)
    .bind(&answers_json)
    .bind(spec_json.as_deref())
    .bind(proposal_id)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(ApplyError::NotPending {
            proposal_id: proposal_id.to_owned(),
            status: "race".to_owned(),
        });
    }

    let (kind,): (String,) =
        sqlx::query_as("SELECT kind FROM structure_proposals WHERE proposal_id = ?")
            .bind(proposal_id)
            .fetch_one(pool)
            .await?;

    tracing::info!(
        proposal_id,
        kind = %kind,
        apply_mode = ApplyMode::Auto.as_str(),
        "proposals: auto-applied",
    );

    Ok(AutoApplyOutcome {
        proposal_id: proposal_id.to_owned(),
        kind,
        applied_at: applied_at_str,
        apply_mode: ApplyMode::Auto,
    })
}

// ---------- Confirm path ----------

// ---------- Emit path ----------

/// Default timeout for newly emitted proposals: 24 h from `proposed_at`.
pub const DEFAULT_EMIT_TIMEOUT: chrono::Duration = chrono::Duration::hours(24);

/// Parameters for [`emit_proposal`].
///
/// Per-kind emitters in [`crate::dedup`], [`crate::promote`]
/// build the `context` + `questions` blobs and forward
/// to this function. The chassis keeps the INSERT atomic and the
/// `proposal_id` minting policy in one place (`UUIDv4` — opaque, no
/// order information leaked).
#[derive(Debug, Clone)]
pub struct EmitParams {
    /// Canonical kind name (one of [`kind::ALL`]).
    pub kind: &'static str,
    /// JSON context the handler will read at apply time.
    pub context: Value,
    /// JSON questionnaire the dashboard renders. Each question carries
    /// an `options[]` array with exactly one `recommended: true` entry —
    /// that is what the auto-apply path will pick on timeout.
    pub questions: Value,
    /// Window before auto-apply fires (default
    /// [`DEFAULT_EMIT_TIMEOUT`]).
    pub timeout: chrono::Duration,
    /// Addressee of the proposal: a `Principal` wire string like
    /// `"user:frodo"`, or `None` for unaddressed / admin-fallback.
    /// Emitters derive it with [`recipient_from_fact`].
    pub recipient: Option<String>,
}

impl EmitParams {
    /// Convenience for emitters that don't override the clock.
    #[must_use]
    pub const fn new(kind: &'static str, context: Value, questions: Value) -> Self {
        Self {
            kind,
            context,
            questions,
            timeout: DEFAULT_EMIT_TIMEOUT,
            recipient: None,
        }
    }

    /// Set the recipient (addressee) of the proposal.
    #[must_use]
    pub fn with_recipient(mut self, recipient: Option<String>) -> Self {
        self.recipient = recipient;
        self
    }

    /// Override the auto-apply window.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: chrono::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Insert a new `pending` proposal. Returns the freshly minted
/// `proposal_id`.
///
/// A `pending` row is a question somebody has to answer, and the one caller
/// that asks one is the forget vote in [`crate::votes`]: a fact's audience is
/// consulted before somebody who is not its author takes it away. What the
/// nightly cycle decides goes through [`emit_applied_proposal`] instead —
/// born applied, a receipt the owner reads rather than a question nobody is
/// awake to answer.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
/// - [`ProposalsError::Json`] when `context` / `questions` cannot be
///   serialised (would only happen for non-finite floats or recursive
///   data — the caller controls the shape).
pub async fn emit_proposal(pool: &SqlitePool, params: EmitParams) -> Result<String> {
    if !kind::is_canonical(params.kind) {
        return Err(ProposalsError::Db(sqlx::Error::Protocol(format!(
            "emit_proposal: unknown kind {:?}",
            params.kind
        ))));
    }
    let proposal_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let timeout_at = now + params.timeout;
    let context_json = serde_json::to_string(&params.context)?;
    let questions_json = serde_json::to_string(&params.questions)?;

    sqlx::query(
        "INSERT INTO structure_proposals \
         (proposal_id, kind, context, questions, proposed_at, timeout_at, status, recipient_id) \
         VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(&proposal_id)
    .bind(params.kind)
    .bind(&context_json)
    .bind(&questions_json)
    .bind(now.to_rfc3339())
    .bind(timeout_at.to_rfc3339())
    .bind(params.recipient.as_deref())
    .execute(pool)
    .await?;

    tracing::info!(
        proposal_id,
        kind = params.kind,
        timeout_at = %timeout_at.to_rfc3339(),
        recipient = ?params.recipient,
        "proposals: emitted"
    );

    Ok(proposal_id)
}

// ---------- Born-applied emit (act-first) ----------

/// Relative dashboard path a consumer agent surfaces as a clickable link so the
/// originating user can review or modify a proposed change to the structure.
///
/// Points at the real per-proposal **open-in-chat** primer
/// (`GET /dashboard/proposals/:id/open-in-chat`): it lands the user inside the
/// dashboard's agentic chat with the proposal already summarised, where they
/// can ask to modify it. (There is deliberately no per-proposal detail page
/// and no tray: reading a proposal and acting on it are the same
/// conversation, which is what this primer opens.) Relative on purpose: mwe-mcp has
/// no notion of a public base URL, so the consumer prepends whatever base it
/// knows the operator serves the dashboard from.
#[must_use]
pub fn proposal_dashboard_path(proposal_id: &str) -> String {
    format!("/dashboard/proposals/{proposal_id}/open-in-chat")
}

/// Outcome of [`emit_applied_proposal`].
#[derive(Debug, Clone)]
pub struct AppliedEmit {
    /// Freshly minted proposal id.
    pub proposal_id: String,
}

/// Emit a **born-applied** proposal (act-first).
///
/// Unlike [`emit_proposal`], which inserts a `pending` row the sweep later
/// applies, this inserts a row already in `applied`. It is the inverse order
/// the structured-wiki emergence needs: the ingest router has *already*
/// created the typed wiki and written the fact, and this records what
/// happened. The row never passes through `pending`, so the auto-apply sweep
/// never touches it, and `applied` is its final state.
///
/// `spec` is the JSON record of what the operation touched (for
/// `structured_emerge`: the wiki id, the source path of the page, and the
/// originating fact ids). `applied_by` is the originating sender's raw id.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
/// - [`ProposalsError::Json`] when `context` / `questions` / `spec` cannot be
///   serialised (the caller controls the shape, so practically unreachable).
pub async fn emit_applied_proposal(
    pool: &SqlitePool,
    params: EmitParams,
    spec: Value,
    applied_by: Option<&str>,
) -> Result<AppliedEmit> {
    if !kind::is_canonical(params.kind) {
        return Err(ProposalsError::Db(sqlx::Error::Protocol(format!(
            "emit_applied_proposal: unknown kind {:?}",
            params.kind
        ))));
    }
    let proposal_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let context_json = serde_json::to_string(&params.context)?;
    let questions_json = serde_json::to_string(&params.questions)?;
    let spec_json = serde_json::to_string(&spec)?;

    sqlx::query(
        "INSERT INTO structure_proposals \
         (proposal_id, kind, context, questions, proposed_at, timeout_at, status, \
          applied_at, applied_by, apply_mode, spec, recipient_id) \
         VALUES (?, ?, ?, ?, ?, ?, 'applied', ?, ?, 'auto', ?, ?)",
    )
    .bind(&proposal_id)
    .bind(params.kind)
    .bind(&context_json)
    .bind(&questions_json)
    .bind(now.to_rfc3339())
    // timeout_at is NOT NULL but irrelevant for a born-applied row (no sweep
    // selects it); stamp it at `now` so the column is satisfied.
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .bind(applied_by)
    .bind(&spec_json)
    .bind(params.recipient.as_deref())
    .execute(pool)
    .await?;

    tracing::info!(
        proposal_id,
        kind = params.kind,
        recipient = ?params.recipient,
        "proposals: emitted (born applied — act-first)"
    );

    Ok(AppliedEmit { proposal_id })
}

// ---------- Auto-apply sweep + expire fallback ----------

/// Grace period before the expire sweep flips a `pending` row past
/// `timeout_at` to `expired`.
///
/// The auto-apply sweep ([`auto_apply_overdue_proposals`]) gets several
/// retries within this window to recover from transient failures (LLM
/// down, embedding endpoint unreachable). Only after
/// `timeout_at + EXPIRE_GRACE_PERIOD` does the chassis give up on the
/// row and flip it to `expired`.
pub const EXPIRE_GRACE_PERIOD: chrono::Duration = chrono::Duration::hours(24);

/// Summary of one call to [`auto_apply_overdue_proposals`].
#[derive(Debug, Clone, Default)]
pub struct AutoApplySweepReport {
    /// Rows the sweep loaded from `structure_proposals`.
    pub candidates_examined: usize,
    /// `(proposal_id, kind)` of the rows the sweep moved from `pending` to
    /// `applied`.
    pub auto_applied: Vec<(String, String)>,
    /// `(proposal_id, error_message)` for proposals the chassis or the
    /// handler refused. Soft errors only — the sweep keeps going and
    /// the row stays `pending` for the next sweep (or until the
    /// expire-grace sweep mops it up).
    pub errors: Vec<(String, String)>,
}

/// Summary of one call to [`expire_overdue_proposals`].
#[derive(Debug, Clone, Default)]
pub struct ExpireReport {
    /// Number of rows flipped from `pending` to `expired`.
    pub expired: u64,
}

/// Try to auto-apply every `pending` proposal past `timeout_at`.
///
/// Derives answers via [`build_recommended_answers`].
///
/// **`fact_forget` is the one exception to the two-window auto-apply.** Its
/// `timeout_at` is the *voting* deadline, not a 24 h auto-apply timeout, and the
/// audience's silence past it is already consent — so an overdue, un-blocked
/// `fact_forget` is resolved via the internal `apply_fact_forget_now` (the
/// fact is tombstoned) rather than through the questionnaire handler.
/// A `fact_forget` carrying a NO-majority at its deadline (which
/// [`crate::votes::cast_vote`] should already have rejected) is expired instead,
/// never applied — silence is consent, a recorded NO-majority is not.
///
/// Production callers wire this into a recurring sweep (REM scheduler
/// today, separate cron tomorrow); the synchronous surface exists so
/// tests can drive the sweep without spinning up a scheduler.
///
/// Per-row failures are collected in the report; only infrastructure
/// errors (SQL, events emission) bubble up.
///
/// # Errors
///
/// - [`ApplyError::Db`] for sqlx failures during candidate loading.
pub async fn auto_apply_overdue_proposals(
    pool: &SqlitePool,
    tree: &WikiTree,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<AutoApplySweepReport, ApplyError> {
    let mut report = AutoApplySweepReport::default();
    let now_iso = now.to_rfc3339();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT proposal_id, kind, questions
           FROM structure_proposals
          WHERE status = 'pending' AND timeout_at < ?
          ORDER BY proposed_at ASC",
    )
    .bind(&now_iso)
    .fetch_all(pool)
    .await?;
    report.candidates_examined = rows.len();

    for (proposal_id, kind_str, questions_raw) in rows {
        // fact_forget resolves on its OWN lifecycle (silence = consent → apply,
        // straight to `applied`), not the generic two-window auto-apply.
        if kind_str == kind::FACT_FORGET {
            match resolve_overdue_fact_forget(pool, tree, &proposal_id).await {
                Ok(Some(())) => report
                    .auto_applied
                    .push((proposal_id, kind::FACT_FORGET.to_owned())),
                Ok(None) => {}, // expired (NO-majority) — not an error, not applied
                Err(e) => report.errors.push((proposal_id, format!("{e}"))),
            }
            continue;
        }
        let answers = match build_recommended_answers(&questions_raw) {
            Ok(a) => a,
            Err(e) => {
                report
                    .errors
                    .push((proposal_id.clone(), format!("recommended_answers: {e}")));
                continue;
            },
        };
        match auto_apply_proposal(pool, tree, &proposal_id, &answers).await {
            Ok(outcome) => {
                report
                    .auto_applied
                    .push((outcome.proposal_id, outcome.kind));
            },
            Err(e) => {
                report.errors.push((proposal_id, format!("{e}")));
            },
        }
    }
    Ok(report)
}

/// Build the `answers` JSON the chassis expects from a questionnaire
/// shape `[{ id, text, options: [{id, value?, recommended}] }]`.
///
/// The answer for `question.id` is the option's `value` (if present)
/// or its `id` (otherwise). Multi-question proposals produce a flat
/// object `{ <question_id>: <chosen> }`. Single-question proposals
/// retain that shape too — handlers that need a specific field (e.g.
/// `wiki_promote::target_page`) accept it from the answers object
/// directly.
///
/// # Errors
///
/// String describing what went wrong (missing `id`, missing
/// `options`, no `recommended: true` option, etc.). Surfaced as a
/// per-row soft error by [`auto_apply_overdue_proposals`].
pub fn build_recommended_answers(questions_raw: &str) -> std::result::Result<Value, String> {
    let questions: Value =
        serde_json::from_str(questions_raw).map_err(|e| format!("questions json: {e}"))?;
    let arr = questions
        .as_array()
        .ok_or_else(|| "questions must be a JSON array".to_owned())?;
    let mut answers = serde_json::Map::new();
    for q in arr {
        let qid = q
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "question without id".to_owned())?;
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("question {qid:?} has no options array"))?;
        let recommended = options
            .iter()
            .find(|o| {
                o.get("recommended")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("question {qid:?} has no recommended option"))?;
        let chosen = recommended
            .get("value")
            .or_else(|| recommended.get("id"))
            .cloned()
            .ok_or_else(|| format!("recommended option for {qid:?} missing id/value"))?;
        answers.insert(qid.to_owned(), chosen);
    }
    Ok(Value::Object(answers))
}

// ---------- Expire sweep ----------

/// Flip overdue `pending` proposals past the grace period to `expired`.
///
/// A row qualifies once `timeout_at + EXPIRE_GRACE_PERIOD < now`. The
/// grace period lets [`auto_apply_overdue_proposals`] retry transient
/// failures (LLM down, embedding endpoint reachable later) before the
/// chassis gives up. The REM cycle runs the two sweeps back to back and
/// hands both the same `now`, so a row the auto-apply sweep keeps
/// failing stops being retried once the grace window closes.
///
/// Single-statement sweep with no per-row handler: the row lands on
/// `expired` and nothing is emitted.
///
/// # Errors
///
/// - [`ApplyError::Db`] for sqlx failures.
pub async fn expire_overdue_proposals(
    pool: &SqlitePool,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<ExpireReport, ApplyError> {
    let cutoff = (now - EXPIRE_GRACE_PERIOD).to_rfc3339();
    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'expired'
          WHERE status = 'pending' AND timeout_at < ?",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    if rows_affected > 0 {
        tracing::info!(
            expired = rows_affected,
            cutoff = %cutoff,
            "proposals: expired sweep (past grace period)",
        );
    }
    Ok(ExpireReport {
        expired: rows_affected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn fresh_pool() -> (crate::test_db::TestWorkdir, SqlitePool) {
        crate::test_db::TestWorkdir::with_db().await
    }

    /// As [`fresh_pool`], but also returns a [`WikiTree`] rooted at the
    /// same tempdir. The tree's `wikis/` directory is pre-created so
    /// [`WikiTree::open`] can canonicalise it.
    async fn fresh_pool_and_tree() -> (crate::test_db::TestWorkdir, SqlitePool, WikiTree) {
        crate::test_db::TestWorkdir::with_db_and_tree().await
    }

    /// Seed helper. `timeout_offset_secs` is added to `proposed_at` to
    /// build `timeout_at` (negative = already past).
    async fn seed(
        pool: &SqlitePool,
        proposal_id: &str,
        kind: &str,
        status: &str,
        timeout_offset_secs: i64,
    ) {
        let now = chrono::Utc::now();
        let timeout = now + chrono::Duration::seconds(timeout_offset_secs);
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(proposal_id)
        .bind(kind)
        .bind(r#"{"intent":"test"}"#)
        .bind(r#"[{"id":"q1","text":"do it?","options":[]}]"#)
        .bind(now.to_rfc3339())
        .bind(timeout.to_rfc3339())
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    // ---- ProposalStatus enum (3-state) ----

    #[test]
    fn proposal_status_wire_strings_are_stable() {
        assert_eq!(ProposalStatus::Pending.as_str(), "pending");
        assert_eq!(ProposalStatus::Applied.as_str(), "applied");
        assert_eq!(ProposalStatus::Expired.as_str(), "expired");
    }

    #[test]
    fn proposal_status_parse_round_trips_every_variant() {
        for s in [
            ProposalStatus::Pending,
            ProposalStatus::Applied,
            ProposalStatus::Expired,
        ] {
            assert_eq!(ProposalStatus::from_str(s.as_str()), Ok(s));
        }
    }

    #[test]
    fn proposal_status_parse_unknown_value_returns_error_with_input() {
        let bad = ProposalStatus::from_str("nope");
        assert_eq!(bad, Err("nope".to_owned()));
    }

    // ---- kind constants ----

    #[test]
    fn kind_constants_are_the_five_the_engine_emits() {
        assert_eq!(kind::WIKI_PROMOTE, "wiki_promote");
        assert_eq!(kind::DEDUP_MERGE, "dedup_merge");
        assert_eq!(kind::FACT_FORGET, "fact_forget");
        assert_eq!(kind::PAGE_CREATE, "page_create");
        assert_eq!(kind::RAIL_ADD, "rail_add");
        // Two questionnaire kinds, the fact-forget vote, and two receipt-only
        // kinds — never `pending`, emitted born-applied so what the engine
        // decided about the shape of the memory (a page it invented, a link it
        // required) leaves a record the owner can read.
        assert_eq!(kind::ALL.len(), 5);
        assert!(kind::is_canonical("wiki_promote"));
        assert!(kind::is_canonical("fact_forget"));
        assert!(kind::is_canonical("page_create"));
        assert!(kind::is_canonical("rail_add"));
        // Plausible names that are not kinds: the list above is the whole
        // list, and a canonical check that quietly accepted one of these
        // would let a proposal through with nothing to apply it.
        assert!(!kind::is_canonical("wiki_type_forge"));
        assert!(!kind::is_canonical("structured_emerge"));
        assert!(!kind::is_canonical("promote_stage3"));
        assert!(!kind::is_canonical(""));
    }

    // ---- recipient derivation + authorization ----

    #[test]
    fn recipient_from_fact_prefers_sender_then_subject_else_none() {
        use crate::types::Principal;
        // The human who actually said it (sender) wins, even on a group fact.
        assert_eq!(
            recipient_from_fact(
                &Principal::Group("famiglia".into()),
                Some(&Principal::User("frodo".into())),
            ),
            Some("user:frodo".to_owned()),
        );
        // No sender → the owning user.
        assert_eq!(
            recipient_from_fact(&Principal::User("galadriel".into()), None),
            Some("user:galadriel".to_owned()),
        );
        // Group / global subject with no sender → unaddressed (admin-fallback).
        assert_eq!(
            recipient_from_fact(&Principal::Group("famiglia".into()), None),
            None,
        );
        assert_eq!(recipient_from_fact(&Principal::global(), None), None);
    }

    #[test]
    fn recipient_can_act_admin_addressee_and_null() {
        // Admin always may, even for someone else's proposal.
        assert!(recipient_can_act(Some("user:frodo"), "galadriel", true));
        // The addressee may act on their own.
        assert!(recipient_can_act(Some("user:frodo"), "frodo", false));
        // A non-addressee non-admin may not.
        assert!(!recipient_can_act(Some("user:frodo"), "galadriel", false));
        // Unaddressed (NULL) stays actionable by anyone (pre-0032 behaviour).
        assert!(recipient_can_act(None, "anyone", false));
        // A non-user (group) recipient is not actionable by an arbitrary non-admin.
        assert!(!recipient_can_act(Some("group:famiglia"), "frodo", false));
    }

    // ---- list (existing coverage, retained with the canonical kind names) ----

    #[tokio::test]
    async fn list_defaults_to_pending() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(&pool, "p-2", kind::WIKI_PROMOTE, "applied", 86_400).await;
        let rows = list(
            &pool,
            &ListFilters {
                status: Some(ProposalStatus::Pending),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].proposal_id, "p-1");
        assert_eq!(rows[0].status, ProposalStatus::Pending);
    }

    #[tokio::test]
    async fn list_filters_by_kind() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(&pool, "p-2", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let rows = list(
            &pool,
            &ListFilters {
                kind: Some(kind::WIKI_PROMOTE.to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, kind::WIKI_PROMOTE);
    }

    #[tokio::test]
    async fn list_no_status_returns_every_row() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(&pool, "p-2", kind::DEDUP_MERGE, "applied", 86_400).await;
        seed(&pool, "p-3", kind::DEDUP_MERGE, "expired", 86_400).await;
        let rows = list(&pool, &ListFilters::default()).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    // ---- apply_proposal dispatch ----

    #[tokio::test]
    async fn apply_proposal_not_found() {
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        let err = apply_proposal(&pool, &tree, "p-missing", &json!({}), Some("frodo"), true)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::NotFound(ref id) if id == "p-missing"));
    }

    #[tokio::test]
    async fn apply_proposal_rejects_non_pending() {
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "applied", 86_400).await;
        let err = apply_proposal(&pool, &tree, "p-1", &json!({}), Some("frodo"), true)
            .await
            .unwrap_err();
        match err {
            ApplyError::NotPending {
                proposal_id,
                status,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(status, "applied");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_proposal_rejects_unknown_kind() {
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", "forge_type", "pending", 86_400).await;
        let err = apply_proposal(&pool, &tree, "p-1", &json!({}), Some("frodo"), true)
            .await
            .unwrap_err();
        match err {
            ApplyError::UnknownKind(k) => assert_eq!(k, "forge_type"),
            other => panic!("unexpected: {other:?}"),
        }
        // Row stays pending.
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "pending");
    }

    #[tokio::test]
    async fn apply_proposal_returns_kind_not_implemented_for_unshipped_kinds() {
        // wiki_promote is shipped (see crate::promote tests); `page_create`
        // is a born-applied receipt, so reaching the chassis with it pending
        // is a caller bug and surfaces KindNotYetImplemented. Driving an
        // end-to-end happy path through the chassis is the promote module's
        // job.
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        let unshipped = [kind::PAGE_CREATE];
        for (i, k) in unshipped.iter().enumerate() {
            let id = format!("p-{i}");
            seed(&pool, &id, k, "pending", 86_400).await;
            let err = apply_proposal(&pool, &tree, &id, &json!({}), Some("frodo"), true)
                .await
                .unwrap_err();
            match err {
                ApplyError::KindNotYetImplemented(reported) => assert_eq!(&reported, *k),
                other => panic!("unexpected for kind {k}: {other:?}"),
            }
            // Row stays pending — handler never reached the state flip.
            let s: String =
                sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                    .bind(&id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(s, "pending", "row for kind {k} must stay pending");
        }
    }

    #[tokio::test]
    async fn apply_proposal_wiki_promote_with_bad_context_surfaces_invalid_payload() {
        // wiki_promote is now shipped; with the bogus default seed
        // context (`{"intent":"test"}`) it surfaces InvalidPayload because
        // the handler cannot deserialise the required fields. Confirms
        // the chassis is reaching the handler and the handler is gating
        // on its own input contract.
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let err = apply_proposal(&pool, &tree, "p-1", &json!({}), Some("frodo"), true)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::InvalidPayload(_)), "{err:?}");
        // Row stays pending.
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "pending");
    }

    // ---- mark_applied state-flip (tested directly, bypassing dispatch) ----

    #[tokio::test]
    async fn mark_applied_flips_pending_and_records_the_answers() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", Some("frodo"), &json!({"q1":"yes"}), None)
            .await
            .unwrap();
        assert_eq!(out.proposal_id, "p-1");
        assert_eq!(out.kind, kind::WIKI_PROMOTE);
        assert_eq!(out.applied_by.as_deref(), Some("frodo"));
        // Stored values match the outcome.
        let (status, applied_by, answers): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, applied_by, answers
               FROM structure_proposals WHERE proposal_id = ?",
            )
            .bind("p-1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "applied");
        assert_eq!(applied_by.as_deref(), Some("frodo"));
        assert_eq!(answers.as_deref(), Some(r#"{"q1":"yes"}"#));
    }

    #[tokio::test]
    async fn mark_applied_is_no_op_on_non_pending() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "expired", 86_400).await;
        let err = mark_applied(&pool, "p-1", Some("frodo"), &json!({}), None)
            .await
            .unwrap_err();
        match err {
            ApplyError::NotPending {
                proposal_id,
                status,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(status, "race");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mark_applied_stores_spec_when_provided() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let spec = json!({"target_wiki_id":"alice/lavoro","facts_moved":3});
        mark_applied(&pool, "p-1", None, &json!({}), Some(&spec))
            .await
            .unwrap();
        let stored: Option<String> =
            sqlx::query_scalar("SELECT spec FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let parsed: Value = serde_json::from_str(&stored.unwrap()).unwrap();
        assert_eq!(parsed, spec);
    }

    // ---- auto_apply_proposal / mark_auto_applied ----

    type AutoAppliedSnapshot = (String, Option<String>, Option<String>, Option<String>);

    #[tokio::test]
    async fn mark_auto_applied_flips_straight_to_applied() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_auto_applied(&pool, "p-1", &json!({"q1": "rec"}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        assert_eq!(out.proposal_id, "p-1");
        assert_eq!(out.apply_mode, ApplyMode::Auto);
        let (status, apply_mode, applied_by, answers): AutoAppliedSnapshot = sqlx::query_as(
            "SELECT status, apply_mode, applied_by, answers
                       FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "applied",
            "the sweep applies; there is no confirmation step to wait for"
        );
        assert_eq!(apply_mode.as_deref(), Some("auto"));
        assert_eq!(
            applied_by, None,
            "sweep auto-apply must leave applied_by NULL"
        );
        assert_eq!(answers.as_deref(), Some(r#"{"q1":"rec"}"#));
    }

    #[tokio::test]
    async fn mark_auto_applied_is_no_op_on_non_pending() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "applied", 86_400).await;
        let err = mark_auto_applied(&pool, "p-1", &json!({}), None)
            .await
            .unwrap_err();
        match err {
            ApplyError::NotPending {
                proposal_id,
                status,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(status, "race");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- build_recommended_answers (moved from rem.rs) ----

    #[test]
    fn build_recommended_answers_picks_recommended_value_or_id() {
        let q = r#"[
            {"id": "target_page", "text": "where?", "options": [
                {"id": "rec", "value": "a.md", "recommended": true},
                {"id": "alt", "value": "b.md"}
            ]},
            {"id": "confirm", "text": "ok?", "options": [
                {"id": "yes", "recommended": true}
            ]}
        ]"#;
        let answers = build_recommended_answers(q).unwrap();
        assert_eq!(answers["target_page"], "a.md");
        assert_eq!(answers["confirm"], "yes");
    }

    #[test]
    fn build_recommended_answers_rejects_missing_recommended() {
        let q = r#"[{"id": "x", "options": [{"id": "a"}]}]"#;
        let err = build_recommended_answers(q).expect_err("must reject");
        assert!(err.contains("recommended"));
    }

    // ---- auto_apply_overdue_proposals sweep ----

    /// Seed a `pending` `wiki_promote` proposal with a recommended
    /// answer for `target_page` so the chassis would reach the kind
    /// handler. The handler itself rejects (`InvalidPayload`) because
    /// the context is `{"intent":"test"}` and the promote handler
    /// expects a richer shape — that's fine for sweep-level assertions
    /// (we want to know what the *sweep* does, not what the handler
    /// produces).
    async fn seed_with_recommended(pool: &SqlitePool, id: &str, timeout_offset_secs: i64) {
        seed(pool, id, kind::WIKI_PROMOTE, "pending", timeout_offset_secs).await;
        sqlx::query("UPDATE structure_proposals SET questions = ? WHERE proposal_id = ?")
            .bind(
                r#"[{"id":"target_page","text":"where?","options":[
                    {"id":"x","value":"a.md","recommended":true}
                ]}]"#,
            )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn auto_apply_sweep_skips_pending_within_timeout() {
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed_with_recommended(&pool, "p-future", 86_400).await;
        let now = chrono::Utc::now();
        let report = auto_apply_overdue_proposals(&pool, &tree, now)
            .await
            .unwrap();
        assert_eq!(report.candidates_examined, 0);
        assert!(report.auto_applied.is_empty());
        assert!(report.errors.is_empty());
    }

    #[tokio::test]
    async fn auto_apply_sweep_records_handler_failure_as_soft_error() {
        // The promote handler rejects the test seed context with
        // InvalidPayload; the sweep collects it per-row and the row
        // stays pending so the next sweep can retry.
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed_with_recommended(&pool, "p-past", -3600).await;
        let now = chrono::Utc::now();
        let report = auto_apply_overdue_proposals(&pool, &tree, now)
            .await
            .unwrap();
        assert_eq!(report.candidates_examined, 1);
        assert!(report.auto_applied.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].0, "p-past");
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-past")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "pending", "row stays pending so the next sweep retries");
    }

    #[tokio::test]
    async fn auto_apply_sweep_records_missing_recommended_as_soft_error() {
        let (_workdir, pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", -3600).await;
        // Default seed questions has empty options — no recommended.
        let report = auto_apply_overdue_proposals(&pool, &tree, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(report.candidates_examined, 1);
        assert!(report.auto_applied.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(
            report.errors[0].1.starts_with("recommended_answers:"),
            "got: {:?}",
            report.errors[0].1
        );
    }

    #[tokio::test]
    async fn expire_grace_period_only_flips_past_grace_window() {
        // EXPIRE_GRACE_PERIOD is 24h. A row 1h past timeout_at must not
        // flip; a row 48h past timeout_at must.
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-recent", kind::WIKI_PROMOTE, "pending", -3600).await;
        seed(
            &pool,
            "p-stale",
            kind::WIKI_PROMOTE,
            "pending",
            -(48 * 3600),
        )
        .await;
        seed(&pool, "p-future", kind::WIKI_PROMOTE, "pending", 86_400).await;

        let now = chrono::Utc::now();
        let report = expire_overdue_proposals(&pool, now).await.unwrap();
        assert_eq!(report.expired, 1, "only the stale row crosses the grace");

        let statuses: Vec<(String, String)> = sqlx::query_as(
            "SELECT proposal_id, status FROM structure_proposals ORDER BY proposal_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let map: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(map.get("p-recent").map(String::as_str), Some("pending"));
        assert_eq!(map.get("p-stale").map(String::as_str), Some("expired"));
        assert_eq!(map.get("p-future").map(String::as_str), Some("pending"));
    }

    // ---- expire_overdue_proposals ----

    #[tokio::test]
    async fn expire_sweep_flips_past_grace_only() {
        // Expire requires timeout_at + EXPIRE_GRACE_PERIOD
        // (24h) before the row is given up. A row 1 min past timeout
        // does NOT expire — the auto-apply sweep still owns it.
        let (_workdir, pool) = fresh_pool().await;
        seed(
            &pool,
            "p-past-but-in-grace",
            kind::WIKI_PROMOTE,
            "pending",
            -60,
        )
        .await;
        seed(
            &pool,
            "p-past-grace",
            kind::WIKI_PROMOTE,
            "pending",
            -(48 * 3600),
        )
        .await;
        seed(&pool, "p-future", kind::WIKI_PROMOTE, "pending", 86_400).await;
        seed(
            &pool,
            "p-applied",
            kind::WIKI_PROMOTE,
            "applied",
            -(48 * 3600),
        )
        .await;

        let report = expire_overdue_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(report.expired, 1);

        let statuses: Vec<(String, String)> = sqlx::query_as(
            "SELECT proposal_id, status FROM structure_proposals ORDER BY proposal_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let map: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(
            map.get("p-past-but-in-grace").map(String::as_str),
            Some("pending"),
            "row within grace must stay pending so the next sweep can retry auto-apply"
        );
        assert_eq!(map.get("p-past-grace").map(String::as_str), Some("expired"));
        assert_eq!(map.get("p-future").map(String::as_str), Some("pending"));
        assert_eq!(map.get("p-applied").map(String::as_str), Some("applied"));
    }

    #[tokio::test]
    async fn expire_sweep_is_idempotent() {
        let (_workdir, pool) = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", -(48 * 3600)).await;
        let first = expire_overdue_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        let second = expire_overdue_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(first.expired, 1);
        assert_eq!(second.expired, 0);
    }
}
