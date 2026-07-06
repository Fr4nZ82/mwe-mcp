// SPDX-License-Identifier: AGPL-3.0-or-later
//! Structure-proposal lifecycle: list + apply / auto-apply / confirm /
//! revert chassis.
//!
//! Wraps the `structure_proposals` table
//! (engine DB and migrations)
//! and ships the 5-state lifecycle described in
//! the proposal apply engine
//! (5 states; the `confirm_window` policy:
//! silence = consent, not rejection):
//!
//! ```text
//!  pending ── apply (manual, apply_mode='manual') ────────────────────► applied
//!     │                                                                   │
//!     │                                                                   ├── revert(Token) within 7gg ──► reverted (revert_triggered_by='user')
//!     │                                                                   └── 7gg silent ────────────────► applied (permanent)
//!     │
//!     ├── auto_apply (sweep on timeout_at, apply_mode='auto') ──► applied_pending_confirm
//!     │                                                                   │
//!     │                                                                   ├── confirm ────────────────────► applied  (apply_mode stays 'auto', revert_token minted)
//!     │                                                                   ├── revert(Caller) ──────────────► reverted (revert_triggered_by='user')
//!     │                                                                   └── sweep on confirm_deadline ──► applied  (silence = consent, locked, NO revert_token)
//!     │
//!     └── auto-apply sweep failed past grace (LLM down, validation broken) ──► expired
//! ```
//!
//! ## Scope
//!
//! [`apply_proposal`] keeps the manual path (`pending → applied`,
//! `apply_mode='manual'`); [`auto_apply_proposal`] drives the sweep
//! path (`pending → applied_pending_confirm`, `apply_mode='auto'`);
//! [`confirm_proposal`] promotes `applied_pending_confirm → applied`
//! and mints the `revert_token`; [`revert_proposal`] accepts either a
//! [`RevertAuth::Token`] (post-`applied`) or a [`RevertAuth::Caller`]
//! (post-`applied_pending_confirm`). The auto-apply sweep wiring is
//! [`auto_apply_overdue_proposals`]; the finalize sweep is
//! [`auto_finalize_unconfirmed_proposals`] — silence past
//! `confirm_window` flips silently to `applied` without minting a token
//! and without emitting any event.
//!
//! The per-kind handlers ([`crate::promote`],
//! [`crate::dedup`]) are reused verbatim by both
//! `apply_proposal` and `auto_apply_proposal` — the difference between
//! manual and auto-apply is in the state-flip helper that runs after
//! the handler succeeds, not in the handler itself. `confirm_proposal`
//! and the finalize sweep do NOT re-invoke the kind handler (the
//! modifications are already on disk from the auto-apply step).
//!
//! ## Act-first structural changes
//!
//! Since the apply-and-notice conversion, the two structural rungs
//! (`wiki_promote` `paragraph_to_file` / `file_to_subwiki`) never enter
//! `pending`: REM applies them directly and records a **born-applied**
//! receipt via [`emit_applied_proposal`] (status `applied`,
//! `revert_token` + `revert_deadline` minted at insert). The pending
//! lifecycle above remains for the questionnaire kinds (`dedup_merge`
//! today).
//!
//! ## MCP exposure
//!
//! None. The whole `structure_proposal_*` family is off the MCP
//! surface: consumers learn about applied structural changes from the
//! `structure_applied` event (which names the affected user and
//! carries the undo `dashboard_path`); the write entry points
//! ([`apply_proposal`], [`confirm_proposal`], [`revert_proposal`]) are
//! consumed exclusively by the built-in dashboard, which calls these
//! functions directly. See
//! [the tool reference](../../../docs/protocol/tool-reference.md)
//! for the rationale.

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
/// Pinned by the proposal apply engine;
/// the chassis rejects any other value as `unknown_kind`.
pub mod kind {
    /// Promote facts paragraph→file or file→sub-wiki (3-stage auto-promotion building block).
    pub const WIKI_PROMOTE: &str = "wiki_promote";
    /// Merge two near-duplicate facts after REM semantic dedup.
    pub const DEDUP_MERGE: &str = "dedup_merge";
    /// Bundle multiple ops into one transactional apply.
    pub const BUNDLE: &str = "bundle";
    /// A non-sender owner's request to forget one fact, put to the fact's
    /// audience as a propose-first vote ([`crate::votes`]).
    ///
    /// Part of the write-authority model
    /// ([identity and ACL](../../../docs/concepts/identity-and-acl.md)).
    /// The proposal opens `pending` (the fact stays active); a NO-majority
    /// within the window rejects it, and silence (or an all-voted quorum with no
    /// NO-majority) applies it — tombstoning the fact.
    pub const FACT_FORGET: &str = "fact_forget";

    /// Every canonical kind — the questionnaire kinds.
    pub const ALL: &[&str] = &[WIKI_PROMOTE, DEDUP_MERGE, BUNDLE, FACT_FORGET];

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
/// [`mark_applied`]. The variants double as the wire error classes
/// surfaced by the MCP dispatcher for `structure_proposal_apply`.
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
    /// reverted, expired, or raced with another writer.
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
    /// nor an admin (0032). Surfaced when a non-admin tries to apply a
    /// proposal addressed to a different user. Maps to MCP `forbidden`.
    #[error("caller {caller} not authorized to apply {proposal_id}")]
    NotAuthorized {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `sender_id` of the caller that was refused.
        caller: String,
    },
}

/// Errors raised by [`revert_proposal`] and the state-flip helper
/// [`mark_reverted`].
#[derive(Debug, Error)]
pub enum RevertError {
    /// Underlying SQL failure.
    #[error("proposals db: {0}")]
    Db(#[from] sqlx::Error),
    /// No row with the requested `proposal_id`.
    #[error("proposal not found: {0}")]
    NotFound(String),
    /// The proposal is not in a revertable status. Revertable statuses
    /// are `applied` (with [`RevertAuth::Token`]) and
    /// `applied_pending_confirm` (with [`RevertAuth::Caller`]); anything
    /// else (`pending`, `reverted`, `expired`) is refused here.
    #[error("proposal {proposal_id} not revertable (current status: {status})")]
    NotRevertable {
        /// Identifier of the proposal.
        proposal_id: String,
        /// Status observed at the time of the failed transition.
        status: String,
    },
    /// The supplied token does not match the row's `revert_token` (or
    /// the row has no token — would happen if the caller used
    /// [`RevertAuth::Token`] on an `applied_pending_confirm` row that
    /// hasn't been confirmed yet).
    #[error("invalid revert token for {0}")]
    InvalidRevertToken(String),
    /// The 7-day revert window for an `applied` proposal has closed.
    #[error("revert window closed for {proposal_id} (deadline {deadline} < now)")]
    RevertWindowClosed {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `revert_deadline` value as stored on the row.
        deadline: String,
    },
    /// The `confirm_window` for an `applied_pending_confirm` row has
    /// elapsed. Practically this only surfaces if the caller races the
    /// finalize sweep ([`auto_finalize_unconfirmed_proposals`]):
    /// the row will already have been flipped to `applied` (silently,
    /// no `revert_token` minted) by the time the next request lands,
    /// but during the race window the chassis refuses the revert
    /// explicitly rather than silently widening the window.
    #[error("confirm window expired for {proposal_id} (deadline {deadline} < now)")]
    ConfirmWindowExpired {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `confirm_deadline` value as stored on the row.
        deadline: String,
    },
    /// [`RevertAuth::Caller`] check failed.
    ///
    /// The caller is not the sender of the originating proposal nor
    /// an admin. Reserved for the stricter auth policy planned
    /// alongside the proposal-emitter column; the current dispatcher
    /// accepts any non-empty `caller_id` and relies on the dashboard
    /// surface to filter who can see what.
    #[error("caller {caller} not authorized to revert {proposal_id}")]
    RevertNotAuthorized {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `sender_id` of the caller that was refused.
        caller: String,
    },
    /// `kind` is canonical but the inverse handler is not yet shipped.
    #[error("kind {0} inverse not yet implemented")]
    KindNotYetImplemented(String),
    /// `kind` is not one of the four canonical names.
    #[error("unknown proposal kind: {0}")]
    UnknownKind(String),
    /// Inverse handler rejected the stored `spec` shape.
    /// Maps to MCP `invalid_input`.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    /// Inverse handler hit a filesystem error.
    /// Maps to MCP `internal_error`.
    #[error("handler io: {0}")]
    HandlerIo(String),
    /// Inverse handler hit a data-consistency error
    /// (e.g. marker disappeared from the target page).
    /// Maps to MCP `internal_error`.
    #[error("handler data: {0}")]
    HandlerData(String),
}

/// Errors raised by [`confirm_proposal`] and [`mark_confirmed`].
///
/// Confirm is the path `applied_pending_confirm → applied` the user
/// takes after the sweep has auto-applied a proposal on their behalf.
#[derive(Debug, Error)]
pub enum ConfirmError {
    /// Underlying SQL failure.
    #[error("proposals db: {0}")]
    Db(#[from] sqlx::Error),
    /// No row with the requested `proposal_id`.
    #[error("proposal not found: {0}")]
    NotFound(String),
    /// The proposal is not in `applied_pending_confirm` status — either
    /// still `pending`, already confirmed (`applied`), reverted, or
    /// expired.
    #[error("proposal {proposal_id} not in applied_pending_confirm (current status: {status})")]
    NotPendingConfirm {
        /// Identifier of the proposal.
        proposal_id: String,
        /// Status observed at the time of the failed transition.
        status: String,
    },
    /// The `confirm_window` has elapsed; the auto-revert sweep should
    /// be picking the row up next. The chassis surfaces this as a hard
    /// refusal rather than silently widening the window.
    #[error("confirm window expired for {proposal_id} (deadline {deadline} < now)")]
    ConfirmWindowExpired {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `confirm_deadline` value as stored on the row.
        deadline: String,
    },
    /// The caller is neither the proposal's `recipient_id` (addressee)
    /// nor an admin (0032). Surfaced when a non-admin tries to confirm a
    /// proposal addressed to a different user. Maps to MCP `forbidden`.
    #[error("caller {caller} not authorized to confirm {proposal_id}")]
    NotAuthorized {
        /// Identifier of the proposal.
        proposal_id: String,
        /// `sender_id` of the caller that was refused.
        caller: String,
    },
}

/// Caller-side authorization payload for [`revert_proposal`].
///
/// The chassis picks the right code path based on the row's `status`:
/// `applied` requires a [`Token`] match, `applied_pending_confirm`
/// requires a non-empty [`Caller`].
///
/// [`Token`]: RevertAuth::Token
/// [`Caller`]: RevertAuth::Caller
#[derive(Debug, Clone, Copy)]
pub enum RevertAuth<'a> {
    /// Path post-`applied`: classic revert with the `revert_token`
    /// minted at apply / confirm time. Must match the row's stored
    /// token byte-for-byte.
    Token(&'a str),
    /// Path post-`applied_pending_confirm`: no token (the proposal
    /// hasn't been confirmed yet, so no token has been minted). The
    /// caller's `sender_id` is recorded as the `revert_triggered_by`
    /// audit trail and checked against the proposal's `recipient_id`
    /// (0032): only the addressee or an admin may revert.
    Caller {
        /// `sender_id` of the caller.
        sender: &'a str,
        /// Whether the caller holds the admin role (bypasses the
        /// recipient check).
        is_admin: bool,
    },
}

/// Result alias for the read side.
pub type Result<T> = std::result::Result<T, ProposalsError>;

// ---------- Status enum ----------

/// Lifecycle status of a structure proposal row (5-state model).
///
/// ```text
/// pending ── user apply (manual) ──────────────► applied
///    │                                               │
///    │                                               ├── revert (within 7gg, with token) ──► reverted
///    │                                               └── 7gg silent ─────────────────────► applied (permanent)
///    │
///    ├── 24h timeout, sweep auto-apply ──► applied_pending_confirm
///    │                                          │
///    │                                          ├── user confirm ─────────► applied
///    │                                          ├── user revert ──────────► reverted
///    │                                          └── confirm_window timeout, sweep auto-revert ──► reverted
///    │
///    └── auto-apply sweep failed (LLM down, validation broken) ──► expired
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Awaiting an answer from the user.
    Pending,
    /// Auto-applied by the timeout sweep with `recommended` answers, waiting
    /// for the user to confirm (`structure_proposal_confirm`) or revert. If
    /// silent past `confirm_deadline`, the auto-revert sweep flips to
    /// `reverted`.
    AppliedPendingConfirm,
    /// Applied — either manually by the user via the dashboard (`apply_mode
    /// = 'manual'`) or post-confirmation of a previous `AppliedPendingConfirm`.
    Applied,
    /// Reverted (user-triggered with token, user-triggered without token on
    /// `AppliedPendingConfirm`, or sweep-triggered after `confirm_deadline`).
    Reverted,
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
            Self::AppliedPendingConfirm => "applied_pending_confirm",
            Self::Applied => "applied",
            Self::Reverted => "reverted",
            Self::Expired => "expired",
        }
    }
}

impl FromStr for ProposalStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "applied_pending_confirm" => Ok(Self::AppliedPendingConfirm),
            "applied" => Ok(Self::Applied),
            "reverted" => Ok(Self::Reverted),
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
    /// Optional recipient scope (0032). When `Some(principal)` — a
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
/// Revert window length: 7 days.
///
/// Counts from `applied_at` (manual path) or `confirmed_at` (proposals
/// that landed via the auto-apply path then got confirmed).
pub const REVERT_WINDOW: chrono::Duration = chrono::Duration::days(7);
/// Confirm window length: 7 days from `applied_at`.
///
/// Applies to `applied_pending_confirm` rows. Silence within this
/// window triggers auto-revert by the
/// `auto_finalize_unconfirmed_proposals` sweep.
pub const CONFIRM_WINDOW: chrono::Duration = chrono::Duration::days(7);

/// Distinguishes the two paths that produce an `applied` row.
///
/// Stamped in the `apply_mode` column and surfaced in
/// [`ApplyOutcome`] / [`ConfirmOutcome`] / list payloads so callers
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
    /// `applied_at + 7d` for `applied` rows; `None` otherwise.
    pub revert_deadline: Option<String>,
    /// Addressee of the proposal (0032): a `Principal` wire string like
    /// `"user:frodo"`, or `None` for unaddressed / admin-fallback rows
    /// (every pre-0032 row reads as `None`).
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
                applied_at, applied_by, revert_deadline, recipient_id
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
        revert_deadline,
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
            revert_deadline,
            recipient_id,
        });
    }
    Ok(out)
}

fn parse_status_lenient(s: &str) -> ProposalStatus {
    ProposalStatus::from_str(s).unwrap_or(ProposalStatus::Pending)
}

// ---------- In-flight counts ----------

/// Snapshot of how many structure proposals are currently "in flight" —
/// i.e. in a state where a subsequent capture/recall turn could grow
/// the wiki state in ways that make the eventual revert harder.
///
/// Scoped by `recipient` (0032): pass `Some("user:<id>")` to count only
/// the rows addressed to that user plus the unaddressed/admin-fallback
/// ones, or `None` for the deployment-wide count (admin view). This is
/// the column the deferred notes called `emitted_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InFlightCounts {
    /// Rows where `status = 'pending'`.
    pub pending: i64,
    /// Rows where `status = 'applied_pending_confirm'` (auto-apply sweep
    /// happened, user has not yet confirmed nor reverted).
    pub applied_pending_confirm: i64,
    /// Rows where `status = 'applied'` whose revert window is still open
    /// (`revert_deadline IS NOT NULL AND revert_deadline > now`) — the
    /// born-applied structured-wiki emergences the user can still
    /// undo. Excludes confirmed/expired-window/`reverted` rows.
    pub revertable_applied: i64,
}

impl InFlightCounts {
    /// `pending + applied_pending_confirm + revertable_applied` — every
    /// row the user can still act on (review, confirm/revert, or undo).
    /// This is what the dashboard in-flight badge surfaces.
    #[must_use]
    pub const fn total(self) -> i64 {
        self.pending + self.applied_pending_confirm + self.revertable_applied
    }
}

/// Count `structure_proposals` rows in flight — `pending`,
/// `applied_pending_confirm`, and `applied`-with-an-open-revert-window —
/// in a single query.
///
/// `now` is the instant the revert window is measured against (an
/// explicit parameter rather than `datetime('now')` so callers and
/// tests control time): an `applied` row counts toward
/// `revertable_applied` only while `revert_deadline > now`. Stored
/// deadlines are rfc3339 UTC strings (see [`apply_proposal`] /
/// [`confirm_proposal`]), so the comparison is a lexical string compare
/// against `now.to_rfc3339()` — correct because both sides share the
/// same `+00:00` offset.
///
/// Used by the dashboard in-flight badge (every actionable row) and by
/// [`super::ingest::wiki_ingest_message`] callers (e.g. the MCP
/// dispatcher), which read the `pending` / `applied_pending_confirm`
/// fields to attach a structured `pending_attention` warning so the
/// consumer agent can nudge the user toward the dashboard before piling
/// more state on top of an unconfirmed change.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
pub async fn count_in_flight(
    pool: &SqlitePool,
    recipient: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<InFlightCounts> {
    let now_str = now.to_rfc3339();
    let mut sql = String::from(
        "SELECT \
            COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending, \
            COALESCE(SUM(CASE WHEN status = 'applied_pending_confirm' THEN 1 ELSE 0 END), 0) \
                AS applied_pending_confirm, \
            COALESCE(SUM(CASE WHEN status = 'applied' \
                AND revert_deadline IS NOT NULL AND revert_deadline > ? \
                THEN 1 ELSE 0 END), 0) AS revertable_applied \
         FROM structure_proposals",
    );
    if recipient.is_some() {
        // 0032: scope to the caller — addressed to me OR unaddressed.
        sql.push_str(" WHERE (recipient_id = ? OR recipient_id IS NULL)");
    }
    let mut q = sqlx::query_as::<_, (i64, i64, i64)>(&sql).bind(&now_str);
    if let Some(r) = recipient {
        q = q.bind(r);
    }
    let row: (i64, i64, i64) = q.fetch_one(pool).await?;
    Ok(InFlightCounts {
        pending: row.0,
        applied_pending_confirm: row.1,
        revertable_applied: row.2,
    })
}

// ---------- Recipient (addressee) derivation + authorization (0032) ----------

/// Derive the recipient (addressee) of a proposal from the fact that
/// triggered it.
///
/// The human who actually said it (`sender_id`) wins; otherwise the
/// owning user; otherwise `None` (a group/global owner with no sender →
/// unaddressed / admin-fallback). The returned string, when `Some`, is a
/// `Principal` wire string like `"user:frodo"`, matching `owner_id` /
/// `sender_id` on the fact and the `recipient_id` column.
///
/// This is the single policy knob for "who gets notified about a
/// proposal" — change it here and every emitter follows.
#[must_use]
pub fn recipient_from_fact(
    owner_id: &crate::types::Principal,
    sender_id: Option<&crate::types::Principal>,
) -> Option<String> {
    use crate::types::Principal;
    if let Some(Principal::User(id)) = sender_id {
        return Some(format!("user:{id}"));
    }
    if let Principal::User(id) = owner_id {
        return Some(format!("user:{id}"));
    }
    None
}

/// Whether `caller_sender_id` may apply / confirm / revert a proposal
/// whose addressee is `recipient_id`.
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

// ---------- Apply / AutoApply / Confirm / Revert outcomes ----------

/// Successful outcome of [`apply_proposal`] — the manual path
/// `pending → applied`. The `revert_token` is minted at this point and
/// valid for the next [`REVERT_WINDOW`].
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
    /// `UUIDv4` the caller must echo back to [`revert_proposal`] within
    /// [`REVERT_WINDOW`].
    pub revert_token: String,
    /// ISO 8601 `applied_at + 7d`.
    pub revert_deadline: String,
    /// Always [`ApplyMode::Manual`] for this outcome.
    ///
    /// Surfaced so a uniform wire shape can carry both paths without a
    /// discriminator.
    pub apply_mode: ApplyMode,
}

/// Successful outcome of [`auto_apply_proposal`] — the sweep path
/// `pending → applied_pending_confirm`.
///
/// No `revert_token` is minted at this point (the user reverts via
/// [`RevertAuth::Caller`], not a token); a token is minted later by
/// [`confirm_proposal`] if the user confirms.
#[derive(Debug, Clone)]
pub struct AutoApplyOutcome {
    /// Identifier of the auto-applied proposal.
    pub proposal_id: String,
    /// Kind that was auto-applied (one of [`kind::ALL`]).
    pub kind: String,
    /// ISO 8601 timestamp of the flip (server clock).
    pub applied_at: String,
    /// ISO 8601 `applied_at + CONFIRM_WINDOW`.
    ///
    /// The user must confirm or revert before this deadline; silence
    /// triggers `auto_finalize_unconfirmed_proposals`.
    pub confirm_deadline: String,
    /// Always [`ApplyMode::Auto`] for this outcome.
    pub apply_mode: ApplyMode,
}

/// Successful outcome of [`confirm_proposal`] — the confirm path
/// `applied_pending_confirm → applied`.
///
/// The `revert_token` is minted at this point (no token existed during
/// `applied_pending_confirm`) and is valid for the next
/// [`REVERT_WINDOW`] from `confirmed_at`.
#[derive(Debug, Clone)]
pub struct ConfirmOutcome {
    /// Identifier of the confirmed proposal.
    pub proposal_id: String,
    /// Kind that was confirmed (one of [`kind::ALL`]).
    pub kind: String,
    /// ISO 8601 timestamp of the apply step that the confirm completes
    /// (preserved from when the sweep auto-applied).
    pub applied_at: String,
    /// ISO 8601 timestamp of the confirmation flip (server clock).
    pub confirmed_at: String,
    /// `sender_id` of the confirming user (always populated — the path
    /// requires an authenticated caller).
    pub confirmed_by: String,
    /// `UUIDv4` minted at confirmation; must be echoed back to
    /// [`revert_proposal`] via [`RevertAuth::Token`] within
    /// [`REVERT_WINDOW`] of `confirmed_at`.
    pub revert_token: String,
    /// ISO 8601 `confirmed_at + 7d`.
    pub revert_deadline: String,
    /// Always [`ApplyMode::Auto`] for this outcome.
    ///
    /// Confirm does not rewrite the original path label, it only
    /// finalises it.
    pub apply_mode: ApplyMode,
}

/// Successful outcome of [`revert_proposal`].
#[derive(Debug, Clone)]
pub struct RevertOutcome {
    /// Identifier of the reverted proposal.
    pub proposal_id: String,
    /// Kind that was reverted.
    pub kind: String,
    /// ISO 8601 timestamp of the flip.
    pub reverted_at: String,
    /// Whichever revertable status the row was in before the flip.
    ///
    /// Either [`ProposalStatus::Applied`] or
    /// [`ProposalStatus::AppliedPendingConfirm`]. Lets the caller
    /// render "we cancelled your manual apply" vs "we cancelled the
    /// auto-apply we did on your behalf".
    pub prior_status: ProposalStatus,
    /// `"user"` for revert calls (both auth paths) or `"sweep"` for
    /// rows the auto-revert sweep flipped.
    ///
    /// Mirrors the `revert_triggered_by` column. The auto-revert sweep
    /// is `auto_finalize_unconfirmed_proposals`.
    pub revert_triggered_by: String,
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
        kind::BUNDLE => Err(ApplyError::KindNotYetImplemented(kind.to_owned())),
        other => Err(ApplyError::UnknownKind(other.to_owned())),
    }
}

/// Apply a `fact_forget` proposal: tombstone the fact named in its `context`.
///
/// The terminal step of a non-sender owner's forget vote ([`crate::votes`]): the
/// audience's silence (or an all-voted quorum) consented, so the fact is
/// forgotten via [`crate::fact_index::mark_forgotten`] (reason
/// `"fact_forget_vote"`). Returns a small spec recording the tombstoned id (so
/// the row's `spec` column is non-NULL for the listing / audit); the kind has no
/// revert inverse — a vote-resolved deletion is final, undone only by re-stating
/// the fact, never by [`revert_proposal`].
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
/// optional `spec`, `apply_mode='manual'`, mint `revert_token` and
/// `revert_deadline`. Returns the [`ApplyOutcome`] on success.
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
    let revert_deadline = applied_at + REVERT_WINDOW;
    let revert_token = Uuid::new_v4().to_string();
    let applied_at_str = applied_at.to_rfc3339();
    let revert_deadline_str = revert_deadline.to_rfc3339();
    let answers_json = serde_json::to_string(answers)?;
    let spec_json = spec.map(serde_json::to_string).transpose()?;

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied',
                apply_mode = 'manual',
                applied_at = ?,
                applied_by = ?,
                answers = ?,
                spec = ?,
                revert_token = ?,
                revert_deadline = ?
          WHERE proposal_id = ? AND status = 'pending'",
    )
    .bind(&applied_at_str)
    .bind(applied_by)
    .bind(&answers_json)
    .bind(spec_json.as_deref())
    .bind(&revert_token)
    .bind(&revert_deadline_str)
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
        revert_token,
        revert_deadline: revert_deadline_str,
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
/// `applied_by`), then **clears the revert token + deadline**: a vote-resolved
/// forget is final, so the row carries no undo lever and never counts as
/// `revertable_applied`. `answers` is irrelevant for this kind (the handler
/// reads `fact_id` from the context), so an empty object is passed.
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
    // A vote-resolved forget is final — strip the undo lever `mark_applied`
    // minted so the dashboard never offers a (non-existent) revert.
    sqlx::query(
        "UPDATE structure_proposals
            SET revert_token = NULL, revert_deadline = NULL
          WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .execute(pool)
    .await?;
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
/// Mirrors [`apply_proposal`] step-for-step, but the post-handler flip
/// lands on `applied_pending_confirm` instead of `applied`, and
/// `apply_mode` is stamped `'auto'`. The user then has
/// [`CONFIRM_WINDOW`] to either [`confirm_proposal`] or
/// [`revert_proposal`] with [`RevertAuth::Caller`]; silence triggers
/// `auto_finalize_unconfirmed_proposals`.
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
/// `pending → applied_pending_confirm`, stamp `apply_mode='auto'`,
/// `applied_at`, `applied_by=NULL`, `answers`, optional `spec`, and
/// `confirm_deadline = applied_at + CONFIRM_WINDOW`. No `revert_token`
/// is minted at this stage — the user reverts via
/// [`RevertAuth::Caller`] without a token until they confirm.
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
    let confirm_deadline = applied_at + CONFIRM_WINDOW;
    let applied_at_str = applied_at.to_rfc3339();
    let confirm_deadline_str = confirm_deadline.to_rfc3339();
    let answers_json = serde_json::to_string(answers)?;
    let spec_json = spec.map(serde_json::to_string).transpose()?;

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied_pending_confirm',
                apply_mode = 'auto',
                applied_at = ?,
                applied_by = NULL,
                answers = ?,
                spec = ?,
                confirm_deadline = ?
          WHERE proposal_id = ? AND status = 'pending'",
    )
    .bind(&applied_at_str)
    .bind(&answers_json)
    .bind(spec_json.as_deref())
    .bind(&confirm_deadline_str)
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
        confirm_deadline = %confirm_deadline_str,
        "proposals: auto-applied",
    );

    Ok(AutoApplyOutcome {
        proposal_id: proposal_id.to_owned(),
        kind,
        applied_at: applied_at_str,
        confirm_deadline: confirm_deadline_str,
        apply_mode: ApplyMode::Auto,
    })
}

// ---------- Confirm path ----------

/// Promote an `applied_pending_confirm` proposal to `applied`.
///
/// The kind handler is not invoked here — the changes were already
/// applied by the sweep that flipped the row to
/// `applied_pending_confirm`. This call only mints the `revert_token`
/// + `revert_deadline` and stamps `confirmed_at` / `confirmed_by`.
///
/// # Errors
///
/// - [`ConfirmError::NotFound`] when no row matches `proposal_id`.
/// - [`ConfirmError::NotPendingConfirm`] when the row is in any other
///   status (race with auto-revert sweep, double confirm, etc.).
/// - [`ConfirmError::ConfirmWindowExpired`] when the `confirm_deadline`
///   has already elapsed.
/// - [`ConfirmError::Db`] for sqlx failures.
pub async fn confirm_proposal(
    pool: &SqlitePool,
    proposal_id: &str,
    confirmed_by: &str,
    is_admin: bool,
) -> std::result::Result<ConfirmOutcome, ConfirmError> {
    type ConfirmRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<ConfirmRow> = sqlx::query_as(
        "SELECT kind, status, confirm_deadline, applied_at, recipient_id
               FROM structure_proposals
              WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    let (kind, status, stored_deadline, stored_applied_at, recipient_id) =
        row.ok_or_else(|| ConfirmError::NotFound(proposal_id.to_owned()))?;
    if status != ProposalStatus::AppliedPendingConfirm.as_str() {
        return Err(ConfirmError::NotPendingConfirm {
            proposal_id: proposal_id.to_owned(),
            status,
        });
    }
    // 0032: only the addressee or an admin may confirm.
    if !recipient_can_act(recipient_id.as_deref(), confirmed_by, is_admin) {
        return Err(ConfirmError::NotAuthorized {
            proposal_id: proposal_id.to_owned(),
            caller: confirmed_by.to_owned(),
        });
    }
    let deadline_ok = stored_deadline.as_deref().is_some_and(|d| {
        chrono::DateTime::parse_from_rfc3339(d)
            .ok()
            .is_some_and(|deadline| deadline > chrono::Utc::now())
    });
    if !deadline_ok {
        return Err(ConfirmError::ConfirmWindowExpired {
            proposal_id: proposal_id.to_owned(),
            deadline: stored_deadline.unwrap_or_default(),
        });
    }

    mark_confirmed(pool, proposal_id, confirmed_by, &kind, stored_applied_at).await
}

/// Internal state-flip for the confirm path: bump
/// `applied_pending_confirm → applied`, stamp `confirmed_at`,
/// `confirmed_by`, mint `revert_token`, set
/// `revert_deadline = confirmed_at + REVERT_WINDOW`. Race-safe via
/// `WHERE status = 'applied_pending_confirm'`.
///
/// # Errors
///
/// - [`ConfirmError::NotPendingConfirm`] with `status="race"` if the
///   conditional UPDATE matched zero rows.
/// - [`ConfirmError::Db`] for sqlx failures.
pub(crate) async fn mark_confirmed(
    pool: &SqlitePool,
    proposal_id: &str,
    confirmed_by: &str,
    kind: &str,
    applied_at: Option<String>,
) -> std::result::Result<ConfirmOutcome, ConfirmError> {
    let confirmed_at = chrono::Utc::now();
    let revert_deadline = confirmed_at + REVERT_WINDOW;
    let revert_token = Uuid::new_v4().to_string();
    let confirmed_at_str = confirmed_at.to_rfc3339();
    let revert_deadline_str = revert_deadline.to_rfc3339();

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied',
                confirmed_at = ?,
                confirmed_by = ?,
                revert_token = ?,
                revert_deadline = ?
          WHERE proposal_id = ? AND status = 'applied_pending_confirm'",
    )
    .bind(&confirmed_at_str)
    .bind(confirmed_by)
    .bind(&revert_token)
    .bind(&revert_deadline_str)
    .bind(proposal_id)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(ConfirmError::NotPendingConfirm {
            proposal_id: proposal_id.to_owned(),
            status: "race".to_owned(),
        });
    }

    tracing::info!(
        proposal_id,
        kind,
        confirmed_by,
        apply_mode = ApplyMode::Auto.as_str(),
        "proposals: confirmed",
    );

    Ok(ConfirmOutcome {
        proposal_id: proposal_id.to_owned(),
        kind: kind.to_owned(),
        applied_at: applied_at.unwrap_or_default(),
        confirmed_at: confirmed_at_str,
        confirmed_by: confirmed_by.to_owned(),
        revert_token,
        revert_deadline: revert_deadline_str,
        apply_mode: ApplyMode::Auto,
    })
}

// ---------- Revert path ----------

/// Revert a revertable proposal. Atomic single-row state machine,
/// dispatched on the row's status:
///
/// **`applied` path** ([`RevertAuth::Token`]):
/// 1. Load the row, fail with [`RevertError::NotFound`] if missing.
/// 2. Validate `status == 'applied'`
///    ([`RevertError::NotRevertable`] otherwise).
/// 3. Validate `token == stored_revert_token`
///    ([`RevertError::InvalidRevertToken`] otherwise).
/// 4. Validate `now < revert_deadline`
///    ([`RevertError::RevertWindowClosed`] otherwise).
/// 5. Dispatch to the per-kind inverse handler, then race-safe flip via
///    `UPDATE … WHERE status='applied' AND revert_token=?`.
///
/// **`applied_pending_confirm` path** ([`RevertAuth::Caller`]):
/// 1. Load the row, fail with [`RevertError::NotFound`] if missing.
/// 2. Validate `status == 'applied_pending_confirm'`
///    ([`RevertError::NotRevertable`] otherwise).
/// 3. Validate `now < confirm_deadline`
///    ([`RevertError::ConfirmWindowExpired`] otherwise — the auto-revert
///    sweep should be picking it up next).
/// 4. Validate `caller` is non-empty
///    ([`RevertError::RevertNotAuthorized`] otherwise). Finer-grained
///    "sender of proposal or admin" auth waits for a per-proposal
///    `emitted_by` column.
/// 5. Dispatch to the per-kind inverse handler, then race-safe flip via
///    `UPDATE … WHERE status='applied_pending_confirm'`.
///
/// Both paths stamp `revert_triggered_by='user'` and emit the same
/// [`RevertOutcome`] shape (differing only in `prior_status`).
///
/// # Errors
///
/// See [`RevertError`].
pub async fn revert_proposal(
    pool: &SqlitePool,
    tree: &WikiTree,
    proposal_id: &str,
    auth: RevertAuth<'_>,
) -> std::result::Result<RevertOutcome, RevertError> {
    type RevertRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<RevertRow> = sqlx::query_as(
        "SELECT kind, status, revert_token, revert_deadline, confirm_deadline, spec, recipient_id
               FROM structure_proposals
              WHERE proposal_id = ?",
    )
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    let (
        kind,
        status,
        stored_token,
        stored_revert_deadline,
        stored_confirm_deadline,
        stored_spec,
        recipient_id,
    ) = row.ok_or_else(|| RevertError::NotFound(proposal_id.to_owned()))?;

    let prior_status =
        ProposalStatus::from_str(&status).map_err(|_| RevertError::NotRevertable {
            proposal_id: proposal_id.to_owned(),
            status: status.clone(),
        })?;

    match (prior_status, auth) {
        (ProposalStatus::Applied, RevertAuth::Token(token)) => {
            let token_ok = stored_token.as_deref().is_some_and(|t| t == token);
            if !token_ok {
                return Err(RevertError::InvalidRevertToken(proposal_id.to_owned()));
            }
            let deadline_ok = stored_revert_deadline.as_deref().is_some_and(|d| {
                chrono::DateTime::parse_from_rfc3339(d)
                    .ok()
                    .is_some_and(|deadline| deadline > chrono::Utc::now())
            });
            if !deadline_ok {
                return Err(RevertError::RevertWindowClosed {
                    proposal_id: proposal_id.to_owned(),
                    deadline: stored_revert_deadline.unwrap_or_default(),
                });
            }
        },
        (ProposalStatus::AppliedPendingConfirm, RevertAuth::Caller { sender, is_admin }) => {
            // 0032: reject an empty caller, and gate to the addressee or
            // an admin — the auto-applied row carries no token.
            if sender.trim().is_empty()
                || !recipient_can_act(recipient_id.as_deref(), sender, is_admin)
            {
                return Err(RevertError::RevertNotAuthorized {
                    proposal_id: proposal_id.to_owned(),
                    caller: sender.to_owned(),
                });
            }
            let deadline_ok = stored_confirm_deadline.as_deref().is_some_and(|d| {
                chrono::DateTime::parse_from_rfc3339(d)
                    .ok()
                    .is_some_and(|deadline| deadline > chrono::Utc::now())
            });
            if !deadline_ok {
                return Err(RevertError::ConfirmWindowExpired {
                    proposal_id: proposal_id.to_owned(),
                    deadline: stored_confirm_deadline.unwrap_or_default(),
                });
            }
        },
        (ProposalStatus::Applied, RevertAuth::Caller { .. })
        | (ProposalStatus::AppliedPendingConfirm, RevertAuth::Token(_)) => {
            return Err(RevertError::InvalidRevertToken(proposal_id.to_owned()));
        },
        _ => {
            return Err(RevertError::NotRevertable {
                proposal_id: proposal_id.to_owned(),
                status,
            });
        },
    }

    if !kind::is_canonical(&kind) {
        return Err(RevertError::UnknownKind(kind));
    }
    let spec: Value = match stored_spec.as_deref() {
        Some(s) => serde_json::from_str(s)
            .map_err(|e| RevertError::InvalidPayload(format!("spec column not valid JSON: {e}")))?,
        None => {
            return Err(RevertError::InvalidPayload(
                "spec column is NULL — proposal was applied without a kind handler spec".into(),
            ));
        },
    };

    dispatch_revert_kind(pool, tree, &kind, &spec).await?;

    mark_reverted(pool, proposal_id, prior_status, auth, "user").await
}

/// Dispatch table for per-kind revert handlers. Kinds whose handler is
/// not yet shipped return [`RevertError::KindNotYetImplemented`].
async fn dispatch_revert_kind(
    pool: &SqlitePool,
    tree: &WikiTree,
    kind: &str,
    spec: &Value,
) -> std::result::Result<(), RevertError> {
    match kind {
        kind::WIKI_PROMOTE => crate::promote::revert_wiki_promote(pool, tree, spec).await,
        kind::DEDUP_MERGE => crate::dedup::revert_dedup_merge(pool, tree, spec).await,
        kind::BUNDLE => crate::bundle::revert_bundle(pool, tree, spec).await,
        // A vote-resolved forget is FINAL: its audience consented, so there is
        // no undo lever (the apply path clears the revert token, so this branch
        // is unreachable in practice — kept as a hard refusal for defence).
        kind::FACT_FORGET => Err(RevertError::KindNotYetImplemented(kind.to_owned())),
        other => Err(RevertError::UnknownKind(other.to_owned())),
    }
}

/// Internal state-flip: bump `applied | applied_pending_confirm →
/// reverted` race-safely. Stamps `reverted_at` and
/// `revert_triggered_by`. The conditional `WHERE` includes the prior
/// status (and, on the `applied` path, the `revert_token`) so a
/// concurrent flip is detected.
///
/// # Errors
///
/// - [`RevertError::InvalidRevertToken`] if the `applied`-path UPDATE
///   matched zero rows (token mismatch or somebody already reverted).
/// - [`RevertError::NotRevertable`] with `status="race"` if the
///   `applied_pending_confirm`-path UPDATE matched zero rows.
/// - [`RevertError::Db`] for sqlx failures.
pub(crate) async fn mark_reverted(
    pool: &SqlitePool,
    proposal_id: &str,
    prior_status: ProposalStatus,
    auth: RevertAuth<'_>,
    triggered_by: &str,
) -> std::result::Result<RevertOutcome, RevertError> {
    let reverted_at = chrono::Utc::now().to_rfc3339();
    let rows_affected = match (prior_status, auth) {
        (ProposalStatus::Applied, RevertAuth::Token(token)) => sqlx::query(
            "UPDATE structure_proposals
                    SET status = 'reverted',
                        reverted_at = ?,
                        revert_triggered_by = ?
                  WHERE proposal_id = ? AND status = 'applied' AND revert_token = ?",
        )
        .bind(&reverted_at)
        .bind(triggered_by)
        .bind(proposal_id)
        .bind(token)
        .execute(pool)
        .await?
        .rows_affected(),
        (ProposalStatus::AppliedPendingConfirm, _) => sqlx::query(
            "UPDATE structure_proposals
                    SET status = 'reverted',
                        reverted_at = ?,
                        revert_triggered_by = ?
                  WHERE proposal_id = ? AND status = 'applied_pending_confirm'",
        )
        .bind(&reverted_at)
        .bind(triggered_by)
        .bind(proposal_id)
        .execute(pool)
        .await?
        .rows_affected(),
        _ => {
            return Err(RevertError::NotRevertable {
                proposal_id: proposal_id.to_owned(),
                status: prior_status.as_str().to_owned(),
            });
        },
    };
    if rows_affected == 0 {
        return Err(match prior_status {
            ProposalStatus::Applied => RevertError::InvalidRevertToken(proposal_id.to_owned()),
            _ => RevertError::NotRevertable {
                proposal_id: proposal_id.to_owned(),
                status: "race".to_owned(),
            },
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
        prior_status = prior_status.as_str(),
        triggered_by,
        "proposals: reverted",
    );

    Ok(RevertOutcome {
        proposal_id: proposal_id.to_owned(),
        kind,
        reverted_at,
        prior_status,
        revert_triggered_by: triggered_by.to_owned(),
    })
}

// ---------- Emit path ----------

/// Default timeout for newly emitted proposals
/// (the proposal apply engine):
/// 24 h from `proposed_at`.
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
    /// Override the `proposed_at` clock — tests only.
    pub now: Option<chrono::DateTime<chrono::Utc>>,
    /// Addressee of the proposal (0032): a `Principal` wire string like
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
            now: None,
            recipient: None,
        }
    }

    /// Set the recipient (addressee) of the proposal (0032).
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

    /// Override the clock — tests only.
    #[must_use]
    pub const fn with_now(mut self, now: chrono::DateTime<chrono::Utc>) -> Self {
        self.now = Some(now);
        self
    }
}

/// Insert a new `pending` proposal. Returns the freshly minted
/// `proposal_id`.
///
/// Production callers go through the per-kind emitters in
/// [`crate::dedup`] / [`crate::promote`] so the
/// `context` shape lives next to the handler that reads it.
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
    let now = params.now.unwrap_or_else(chrono::Utc::now);
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

/// Default revert window for a born-applied proposal: 7 days, matching the
/// manual-apply revert window the dashboard grants.
pub const DEFAULT_REVERT_WINDOW: chrono::Duration = chrono::Duration::days(7);

/// Relative dashboard path a consumer agent surfaces as a clickable link so the
/// originating user can review / modify / revert an emerged structure.
///
/// Points at the real per-proposal **open-in-chat** primer
/// (`GET /dashboard/proposals/:id/open-in-chat`): it lands the user inside the
/// dashboard's agentic chat with the proposal already summarised, where they
/// can ask to modify or revert it. (There is deliberately no per-proposal
/// detail page — the list at `/dashboard/proposals` carries the one-click
/// revert button, and this primer carries the conversational path.) Relative
/// for the same reason as [`PENDING_CONFIRMS_DASHBOARD_PATH`] (mwe-mcp has no
/// public base URL).
#[must_use]
pub fn proposal_dashboard_path(proposal_id: &str) -> String {
    format!("/dashboard/proposals/{proposal_id}/open-in-chat")
}

/// Outcome of [`emit_applied_proposal`].
#[derive(Debug, Clone)]
pub struct AppliedEmit {
    /// Freshly minted proposal id.
    pub proposal_id: String,
    /// Opaque revert token (the dashboard backend reads it from the row to
    /// authorise an undo; it is never handed to the consumer agent).
    pub revert_token: String,
    /// Instant after which the revert window is closed.
    pub revert_deadline: chrono::DateTime<chrono::Utc>,
}

/// Emit a **born-applied** proposal (act-first).
///
/// Unlike [`emit_proposal`], which inserts a `pending` row the sweep later
/// applies, this inserts a row already in `applied` with a `revert_token` + a
/// `revert_deadline` (default [`DEFAULT_REVERT_WINDOW`]). It is the inverse
/// order the structured-wiki emergence needs: the ingest router has *already*
/// created the typed wiki and written the fact, and this records the action as
/// an undoable receipt. The row never passes through `pending`, so neither the
/// auto-apply sweep (selects `pending`) nor the auto-revert sweep (selects
/// `applied_pending_confirm`) ever touches it; the only state transition left
/// is a user-driven revert via [`revert_proposal`] within the window.
///
/// `spec` is the JSON the kind's revert handler reads (for
/// `structured_emerge`: the wiki id, its `index.md` source path, and the
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
    let revert_token = Uuid::new_v4().to_string();
    let now = params.now.unwrap_or_else(chrono::Utc::now);
    let revert_deadline = now + DEFAULT_REVERT_WINDOW;
    let context_json = serde_json::to_string(&params.context)?;
    let questions_json = serde_json::to_string(&params.questions)?;
    let spec_json = serde_json::to_string(&spec)?;

    sqlx::query(
        "INSERT INTO structure_proposals \
         (proposal_id, kind, context, questions, proposed_at, timeout_at, status, \
          applied_at, applied_by, apply_mode, spec, revert_token, revert_deadline, recipient_id) \
         VALUES (?, ?, ?, ?, ?, ?, 'applied', ?, ?, 'auto', ?, ?, ?, ?)",
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
    .bind(&revert_token)
    .bind(revert_deadline.to_rfc3339())
    .bind(params.recipient.as_deref())
    .execute(pool)
    .await?;

    tracing::info!(
        proposal_id,
        kind = params.kind,
        revert_deadline = %revert_deadline.to_rfc3339(),
        recipient = ?params.recipient,
        "proposals: emitted (born applied — act-first)"
    );

    Ok(AppliedEmit {
        proposal_id,
        revert_token,
        revert_deadline,
    })
}

// ---------- Auto-apply sweep + expire fallback ----------

/// Grace period before the expire sweep flips a `pending` row past
/// `timeout_at` to `expired`.
///
/// The auto-apply sweep ([`auto_apply_overdue_proposals`]) gets several
/// retries within this window to recover from transient failures (LLM
/// down, embedding endpoint unreachable). Only after
/// `timeout_at + EXPIRE_GRACE_PERIOD` does the chassis give up on the
/// row and surface it as `expired` with an admin-facing event payload.
pub const EXPIRE_GRACE_PERIOD: chrono::Duration = chrono::Duration::hours(24);

/// Relative dashboard path the auto-apply / auto-revert events embed in
/// their payload so a consumer can render a clickable link to the
/// pending-confirms page.
///
/// Kept as a relative path on purpose — mwe-mcp has no notion of a
/// "public base URL" yet (the operator's deployment may sit behind a
/// reverse proxy, a tunnel, an internal hostname). The consumer
/// concatenates this with whatever base it knows the operator is
/// serving from.
pub const PENDING_CONFIRMS_DASHBOARD_PATH: &str = "/dashboard/proposals/pending-confirms";

/// Summary of one call to [`auto_apply_overdue_proposals`].
#[derive(Debug, Clone, Default)]
pub struct AutoApplySweepReport {
    /// Rows the sweep loaded from `structure_proposals`.
    pub candidates_examined: usize,
    /// `(proposal_id, kind)` of the rows the sweep advanced out of `pending`:
    /// the questionnaire kinds to `applied_pending_confirm`, and a `fact_forget`
    /// whose vote silence resolved straight to `applied`.
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
/// Derives answers via [`build_recommended_answers`]. Each successful
/// auto-apply emits an [`crate::events::EventKind::AutoApplied`] event
/// so the consumer can notify the user.
///
/// **`fact_forget` is the one exception to the two-window auto-apply.** Its
/// `timeout_at` is the *voting* deadline, not a 24 h auto-apply timeout, and the
/// audience's silence past it is already consent — so an overdue, un-blocked
/// `fact_forget` is resolved **straight to `applied`** (the fact is tombstoned)
/// via the internal `apply_fact_forget_now`, skipping the
/// `applied_pending_confirm` confirm window entirely (there is nothing left to
/// confirm — the vote settled it).
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
                let payload = serde_json::json!({
                    "proposal_id": outcome.proposal_id,
                    "kind": outcome.kind,
                    "applied_at": outcome.applied_at,
                    "confirm_deadline": outcome.confirm_deadline,
                    "dashboard_path": PENDING_CONFIRMS_DASHBOARD_PATH,
                    "summary": format!(
                        "auto-applied {} (proposal {}) — confirm or revert by {}",
                        outcome.kind, outcome.proposal_id, outcome.confirm_deadline,
                    ),
                });
                if let Err(e) = crate::events::insert_event(
                    pool,
                    crate::events::EventKind::AutoApplied,
                    None,
                    None,
                    &payload,
                )
                .await
                {
                    // Event emission failure is soft — the row is already
                    // flipped, the user just won't get the notification.
                    // We surface it as a per-row error so the operator
                    // sees something is wrong without rolling back the
                    // auto-apply.
                    report.errors.push((
                        outcome.proposal_id.clone(),
                        format!("auto_applied event emit: {e}"),
                    ));
                }
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

// ---------- Auto-finalize sweep (auto-revert) ----------

/// Summary of one call to [`auto_finalize_unconfirmed_proposals`].
#[derive(Debug, Clone, Default)]
pub struct AutoFinalizeSweepReport {
    /// Rows the sweep loaded from `structure_proposals` (status
    /// `applied_pending_confirm` past `confirm_deadline`).
    pub candidates_examined: usize,
    /// `proposal_id`s the sweep flipped to `applied`.
    pub finalized: Vec<String>,
}

/// Finalize every `applied_pending_confirm` row past `confirm_deadline`
/// by flipping it to `applied` (silence = consent).
///
/// No kind inverse handler is invoked, no `revert_token` is minted, no
/// event is emitted. The user has been warned at auto-apply time
/// (`auto_applied` event with `dashboard_path`); silence within
/// `confirm_window` is now treated as consent, and the row simply
/// stops being a "pending confirm" once the window closes. Operators
/// can still revert via the dashboard before the deadline; after the
/// deadline the row is fixed (no `revert_token`, no further reverts).
///
/// Single-statement sweep, so race-safe by SQL atomicity.
///
/// # Errors
///
/// - [`ApplyError::Db`] for sqlx failures.
pub async fn auto_finalize_unconfirmed_proposals(
    pool: &SqlitePool,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<AutoFinalizeSweepReport, ApplyError> {
    let mut report = AutoFinalizeSweepReport::default();
    let now_iso = now.to_rfc3339();

    // Capture the proposal_ids that will flip, so the report carries
    // something more useful than a count (lets the REM cycle log them).
    let due: Vec<(String,)> = sqlx::query_as(
        "SELECT proposal_id
           FROM structure_proposals
          WHERE status = 'applied_pending_confirm' AND confirm_deadline < ?
          ORDER BY confirm_deadline ASC",
    )
    .bind(&now_iso)
    .fetch_all(pool)
    .await?;
    report.candidates_examined = due.len();

    if due.is_empty() {
        return Ok(report);
    }

    let rows_affected = sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied'
          WHERE status = 'applied_pending_confirm' AND confirm_deadline < ?",
    )
    .bind(&now_iso)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected > 0 {
        tracing::info!(
            finalized = rows_affected,
            "proposals: auto-finalized sweep (silence = consent)",
        );
    }

    // The IDs we collected pre-UPDATE are a best-effort list — a
    // concurrent confirm/revert may have moved a row out from under us
    // between the SELECT and the UPDATE, but that's fine: the report
    // serves logging, not authorization. Cap the report to the actual
    // count of affected rows so the consumer doesn't see ghost finalizations.
    let cap = usize::try_from(rows_affected).unwrap_or(due.len());
    report.finalized = due.into_iter().map(|(id,)| id).take(cap).collect();

    Ok(report)
}

/// Flip overdue `pending` proposals past the grace period to `expired`.
///
/// A row qualifies once `timeout_at + EXPIRE_GRACE_PERIOD < now`. The
/// grace period lets [`auto_apply_overdue_proposals`] retry transient
/// failures (LLM down, embedding endpoint reachable later) before the
/// chassis gives up.
///
/// Single-statement sweep with no per-row handler. Callers that need a
/// per-row event for the admin can fold this into a list-then-emit
/// pattern; the sweep itself doesn't emit anything.
///
/// # Errors
///
/// - [`ApplyError::Db`] for sqlx failures.
pub async fn expire_overdue_proposals(
    pool: &SqlitePool,
) -> std::result::Result<ExpireReport, ApplyError> {
    expire_overdue_proposals_at(pool, chrono::Utc::now()).await
}

/// As [`expire_overdue_proposals`], but with the cutoff clock injected
/// for testability. Production callers should use the now-less form.
///
/// # Errors
///
/// - [`ApplyError::Db`] for sqlx failures.
pub async fn expire_overdue_proposals_at(
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
    use crate::db;
    use serde_json::json;

    async fn fresh_pool() -> SqlitePool {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        db::open_or_init(dir.path()).await.expect("db open")
    }

    /// As [`fresh_pool`], but also returns a [`WikiTree`] rooted at the
    /// same tempdir. The tree's `wikis/` directory is pre-created so
    /// [`WikiTree::open`] can canonicalise it.
    async fn fresh_pool_and_tree() -> (SqlitePool, WikiTree) {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        let tree = WikiTree::open(dir.path()).expect("wiki tree");
        (pool, tree)
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

    /// Seed an `applied` row with an explicit `revert_deadline` (rfc3339
    /// UTC). `deadline_offset_secs` is added to `now` — positive = window
    /// still open, negative = window already closed; `None` leaves the
    /// column NULL (an `applied` row with no revert window). Used by the
    /// `count_in_flight` revertable-window coverage.
    async fn seed_applied_with_deadline(
        pool: &SqlitePool,
        proposal_id: &str,
        deadline_offset_secs: Option<i64>,
    ) {
        let now = chrono::Utc::now();
        let deadline =
            deadline_offset_secs.map(|secs| (now + chrono::Duration::seconds(secs)).to_rfc3339());
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status, revert_deadline) \
             VALUES (?, ?, ?, ?, ?, ?, 'applied', ?)",
        )
        .bind(proposal_id)
        .bind(kind::WIKI_PROMOTE)
        .bind(r#"{"intent":"test"}"#)
        .bind(r#"[{"id":"q1","text":"do it?","options":[]}]"#)
        .bind(now.to_rfc3339())
        .bind((now + chrono::Duration::seconds(86_400)).to_rfc3339())
        .bind(deadline)
        .execute(pool)
        .await
        .unwrap();
    }

    // ---- ProposalStatus enum (5-state) ----

    #[test]
    fn proposal_status_wire_strings_are_stable() {
        assert_eq!(ProposalStatus::Pending.as_str(), "pending");
        assert_eq!(
            ProposalStatus::AppliedPendingConfirm.as_str(),
            "applied_pending_confirm"
        );
        assert_eq!(ProposalStatus::Applied.as_str(), "applied");
        assert_eq!(ProposalStatus::Reverted.as_str(), "reverted");
        assert_eq!(ProposalStatus::Expired.as_str(), "expired");
    }

    #[test]
    fn proposal_status_parse_round_trips_every_variant() {
        for s in [
            ProposalStatus::Pending,
            ProposalStatus::AppliedPendingConfirm,
            ProposalStatus::Applied,
            ProposalStatus::Reverted,
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
    fn kind_constants_match_d15() {
        assert_eq!(kind::WIKI_PROMOTE, "wiki_promote");
        assert_eq!(kind::DEDUP_MERGE, "dedup_merge");
        assert_eq!(kind::BUNDLE, "bundle");
        assert_eq!(kind::FACT_FORGET, "fact_forget");
        // The canonical kinds: three questionnaire kinds + the fact-forget vote.
        assert_eq!(kind::ALL.len(), 4);
        assert!(kind::is_canonical("wiki_promote"));
        assert!(kind::is_canonical("fact_forget"));
        // `wiki_type_forge` was removed with the `wiki_type` registry
        // teardown — it must no longer be canonical.
        assert!(!kind::is_canonical("wiki_type_forge"));
        // `structured_emerge` was removed with the structured-routing machine
        // — it must no longer be canonical.
        assert!(!kind::is_canonical("structured_emerge"));
        assert!(!kind::is_canonical("promote_stage3"));
        assert!(!kind::is_canonical(""));
    }

    // ---- recipient derivation + authorization (0032) ----

    #[test]
    fn recipient_from_fact_prefers_sender_then_owner_else_none() {
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
        // Group / global owner with no sender → unaddressed (admin-fallback).
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
        let pool = fresh_pool().await;
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
        let pool = fresh_pool().await;
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
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(&pool, "p-2", kind::DEDUP_MERGE, "applied", 86_400).await;
        seed(&pool, "p-3", kind::DEDUP_MERGE, "expired", 86_400).await;
        let rows = list(&pool, &ListFilters::default()).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    // ---- count_in_flight (wiki_ingest_message warning) ----

    #[tokio::test]
    async fn count_in_flight_zero_on_fresh_db() {
        let pool = fresh_pool().await;
        let counts = count_in_flight(&pool, None, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(counts, InFlightCounts::default());
        assert_eq!(counts.total(), 0);
    }

    #[tokio::test]
    async fn count_in_flight_sums_pending_applied_pending_confirm_and_revertable_applied() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(&pool, "p-2", kind::DEDUP_MERGE, "pending", 86_400).await;
        seed(
            &pool,
            "p-3",
            kind::WIKI_PROMOTE,
            "applied_pending_confirm",
            86_400,
        )
        .await;
        // A plain `applied` row with NO revert window is terminal — excluded.
        seed(&pool, "p-4", kind::WIKI_PROMOTE, "applied", 86_400).await;
        seed(&pool, "p-5", kind::WIKI_PROMOTE, "reverted", 86_400).await;
        seed(&pool, "p-6", kind::WIKI_PROMOTE, "expired", 86_400).await;

        let counts = count_in_flight(&pool, None, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.applied_pending_confirm, 1);
        assert_eq!(counts.revertable_applied, 0);
        assert_eq!(counts.total(), 3);
    }

    #[tokio::test]
    async fn count_in_flight_counts_open_revert_window_excludes_closed_and_reverted() {
        let pool = fresh_pool().await;
        // An `applied` row with an OPEN revert window (deadline in the
        // future) — a born-applied emergence the user can still undo.
        seed_applied_with_deadline(&pool, "open", Some(86_400)).await;
        // An `applied` row whose revert window already CLOSED — not in flight.
        seed_applied_with_deadline(&pool, "closed", Some(-60)).await;
        // An `applied` row with NO revert deadline at all — not in flight.
        seed_applied_with_deadline(&pool, "no-window", None).await;
        // A `reverted` row carrying a (now-moot) future deadline — excluded
        // because its status is terminal, regardless of the deadline.
        sqlx::query(
            "UPDATE structure_proposals SET status = 'reverted' WHERE proposal_id = 'no-window'",
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_applied_with_deadline(&pool, "reverted-future", Some(86_400)).await;
        sqlx::query(
            "UPDATE structure_proposals SET status = 'reverted' \
             WHERE proposal_id = 'reverted-future'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let counts = count_in_flight(&pool, None, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(counts.pending, 0);
        assert_eq!(counts.applied_pending_confirm, 0);
        assert_eq!(
            counts.revertable_applied, 1,
            "only the open-window row counts"
        );
        assert_eq!(counts.total(), 1);
    }

    // ---- apply_proposal dispatch ----

    #[tokio::test]
    async fn apply_proposal_not_found() {
        let (pool, tree) = fresh_pool_and_tree().await;
        let err = apply_proposal(&pool, &tree, "p-missing", &json!({}), Some("frodo"), true)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::NotFound(ref id) if id == "p-missing"));
    }

    #[tokio::test]
    async fn apply_proposal_rejects_non_pending() {
        let (pool, tree) = fresh_pool_and_tree().await;
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
        let (pool, tree) = fresh_pool_and_tree().await;
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
        // wiki_promote is shipped (see crate::promote tests); the other
        // three canonical kinds still surface KindNotYetImplemented at the
        // chassis boundary. Driving an end-to-end happy path through the
        // chassis is the promote module's job.
        let (pool, tree) = fresh_pool_and_tree().await;
        let unshipped = [kind::BUNDLE];
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
        let (pool, tree) = fresh_pool_and_tree().await;
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
    async fn mark_applied_flips_pending_and_stamps_token() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", Some("frodo"), &json!({"q1":"yes"}), None)
            .await
            .unwrap();
        assert_eq!(out.proposal_id, "p-1");
        assert_eq!(out.kind, kind::WIKI_PROMOTE);
        assert_eq!(out.applied_by.as_deref(), Some("frodo"));
        assert!(!out.revert_token.is_empty());
        // Stored values match the outcome.
        let (status, applied_by, answers, token, deadline): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, applied_by, answers, revert_token, revert_deadline
               FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert_eq!(applied_by.as_deref(), Some("frodo"));
        assert_eq!(answers.as_deref(), Some(r#"{"q1":"yes"}"#));
        assert_eq!(token.as_deref(), Some(out.revert_token.as_str()));
        let deadline_dt =
            chrono::DateTime::parse_from_rfc3339(deadline.as_deref().unwrap()).unwrap();
        let applied_dt = chrono::DateTime::parse_from_rfc3339(&out.applied_at).unwrap();
        let delta = deadline_dt - applied_dt;
        assert_eq!(
            delta, REVERT_WINDOW,
            "revert_deadline must be applied_at + 7d"
        );
    }

    #[tokio::test]
    async fn mark_applied_is_no_op_on_non_pending() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "reverted", 86_400).await;
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
        let pool = fresh_pool().await;
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

    // ---- revert_proposal dispatch ----

    #[tokio::test]
    async fn revert_proposal_not_found() {
        let (pool, tree) = fresh_pool_and_tree().await;
        let err = revert_proposal(&pool, &tree, "p-missing", RevertAuth::Token("any"))
            .await
            .unwrap_err();
        assert!(matches!(err, RevertError::NotFound(ref id) if id == "p-missing"));
    }

    #[tokio::test]
    async fn revert_proposal_rejects_non_revertable_status() {
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token("any"))
            .await
            .unwrap_err();
        // A `pending` row matches neither (Applied, Token) nor
        // (AppliedPendingConfirm, Caller); the dispatcher refuses
        // explicitly via NotRevertable with the observed status.
        match err {
            RevertError::NotRevertable {
                proposal_id,
                status,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(status, "pending");
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn revert_proposal_rejects_bad_token() {
        let (pool, tree) = fresh_pool_and_tree().await;
        // Build an applied row directly so the token mismatch is the
        // first thing the chassis hits.
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_applied(&pool, "p-1", Some("frodo"), &json!({}), None)
            .await
            .unwrap();
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token("wrong-token"))
            .await
            .unwrap_err();
        assert!(matches!(err, RevertError::InvalidRevertToken(ref id) if id == "p-1"));
    }

    #[tokio::test]
    async fn revert_proposal_rejects_closed_window() {
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", Some("frodo"), &json!({}), None)
            .await
            .unwrap();
        // Backdate revert_deadline to the past.
        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET revert_deadline = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind("p-1")
            .execute(&pool)
            .await
            .unwrap();
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token(&out.revert_token))
            .await
            .unwrap_err();
        match err {
            RevertError::RevertWindowClosed {
                proposal_id,
                deadline,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(deadline, past);
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn revert_proposal_wiki_promote_with_null_spec_surfaces_invalid_payload() {
        // wiki_promote's inverse needs a real spec. If a row was somehow
        // flipped to `applied` without a spec (today only possible via
        // direct DB poke or via the unimplemented kinds before they
        // ship), the chassis surfaces InvalidPayload and leaves the row
        // applied so an operator can inspect.
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", Some("frodo"), &json!({}), None)
            .await
            .unwrap();
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token(&out.revert_token))
            .await
            .unwrap_err();
        assert!(matches!(err, RevertError::InvalidPayload(_)), "{err:?}");
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "applied");
    }

    #[tokio::test]
    async fn revert_proposal_bundle_with_malformed_spec_surfaces_invalid_payload() {
        // The bundle inverse ([`crate::bundle::revert_bundle`]) is wired, so a
        // BUNDLE row reverts for real — but a spec that is not a `BundleSpec`
        // (here: no `ops`) surfaces InvalidPayload and leaves the row applied
        // so an operator can inspect, never a silent half-revert.
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::BUNDLE, "pending", 86_400).await;
        let out = mark_applied(
            &pool,
            "p-1",
            Some("frodo"),
            &json!({}),
            Some(&json!({"placeholder":"spec"})),
        )
        .await
        .unwrap();
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token(&out.revert_token))
            .await
            .unwrap_err();
        assert!(matches!(err, RevertError::InvalidPayload(_)), "{err:?}");
        // Row stays applied — handler never reached the state flip.
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "applied");
    }

    #[tokio::test]
    async fn revert_proposal_caller_path_rejects_token_on_applied_pending_confirm() {
        // An applied_pending_confirm row has no revert_token; passing
        // RevertAuth::Token to it must surface InvalidRevertToken (the
        // chassis would otherwise have to silently fall through to the
        // caller path, which would defeat the caller's intent).
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let err = revert_proposal(&pool, &tree, "p-1", RevertAuth::Token("any"))
            .await
            .unwrap_err();
        assert!(matches!(err, RevertError::InvalidRevertToken(ref id) if id == "p-1"));
    }

    #[tokio::test]
    async fn revert_proposal_caller_path_rejects_empty_caller() {
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let err = revert_proposal(
            &pool,
            &tree,
            "p-1",
            RevertAuth::Caller {
                sender: "  ",
                is_admin: true,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            RevertError::RevertNotAuthorized { ref proposal_id, .. } if proposal_id == "p-1"
        ));
    }

    #[tokio::test]
    async fn revert_proposal_caller_path_rejects_expired_confirm_window() {
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET confirm_deadline = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind("p-1")
            .execute(&pool)
            .await
            .unwrap();
        let err = revert_proposal(
            &pool,
            &tree,
            "p-1",
            RevertAuth::Caller {
                sender: "frodo",
                is_admin: true,
            },
        )
        .await
        .unwrap_err();
        match err {
            RevertError::ConfirmWindowExpired {
                proposal_id,
                deadline,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(deadline, past);
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- mark_reverted state-flip ----

    #[tokio::test]
    async fn mark_reverted_flips_and_stamps_reverted_at() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", Some("frodo"), &json!({}), None)
            .await
            .unwrap();
        let rev = mark_reverted(
            &pool,
            "p-1",
            ProposalStatus::Applied,
            RevertAuth::Token(&out.revert_token),
            "user",
        )
        .await
        .unwrap();
        assert_eq!(rev.proposal_id, "p-1");
        assert_eq!(rev.kind, kind::WIKI_PROMOTE);
        assert_eq!(rev.prior_status, ProposalStatus::Applied);
        assert_eq!(rev.revert_triggered_by, "user");
        let (status, reverted_at, triggered_by): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, reverted_at, revert_triggered_by
                       FROM structure_proposals WHERE proposal_id = ?",
            )
            .bind("p-1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "reverted");
        assert_eq!(reverted_at.as_deref(), Some(rev.reverted_at.as_str()));
        assert_eq!(triggered_by.as_deref(), Some("user"));
    }

    #[tokio::test]
    async fn mark_reverted_is_no_op_on_wrong_token() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_applied(&pool, "p-1", None, &json!({}), None)
            .await
            .unwrap();
        let err = mark_reverted(
            &pool,
            "p-1",
            ProposalStatus::Applied,
            RevertAuth::Token("wrong-token"),
            "user",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RevertError::InvalidRevertToken(ref id) if id == "p-1"));
        // Row stays applied.
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "applied");
    }

    #[tokio::test]
    async fn mark_reverted_is_idempotent_safe() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_applied(&pool, "p-1", None, &json!({}), None)
            .await
            .unwrap();
        mark_reverted(
            &pool,
            "p-1",
            ProposalStatus::Applied,
            RevertAuth::Token(&out.revert_token),
            "user",
        )
        .await
        .unwrap();
        // Second revert with same token must fail cleanly (row already reverted).
        let err = mark_reverted(
            &pool,
            "p-1",
            ProposalStatus::Applied,
            RevertAuth::Token(&out.revert_token),
            "user",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RevertError::InvalidRevertToken(_)));
    }

    #[tokio::test]
    async fn mark_reverted_caller_path_flips_applied_pending_confirm() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let rev = mark_reverted(
            &pool,
            "p-1",
            ProposalStatus::AppliedPendingConfirm,
            RevertAuth::Caller {
                sender: "frodo",
                is_admin: true,
            },
            "user",
        )
        .await
        .unwrap();
        assert_eq!(rev.prior_status, ProposalStatus::AppliedPendingConfirm);
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "reverted");
    }

    // ---- auto_apply_proposal / mark_auto_applied ----

    type AutoAppliedSnapshot = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    #[tokio::test]
    async fn mark_auto_applied_flips_to_pending_confirm_and_sets_deadline() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        let out = mark_auto_applied(&pool, "p-1", &json!({"q1": "rec"}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        assert_eq!(out.proposal_id, "p-1");
        assert_eq!(out.apply_mode, ApplyMode::Auto);
        let (status, apply_mode, applied_by, applied_at, confirm_deadline, answers): AutoAppliedSnapshot =
            sqlx::query_as(
                "SELECT status, apply_mode, applied_by, applied_at, confirm_deadline, answers
                       FROM structure_proposals WHERE proposal_id = ?",
            )
            .bind("p-1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "applied_pending_confirm");
        assert_eq!(apply_mode.as_deref(), Some("auto"));
        assert_eq!(
            applied_by, None,
            "sweep auto-apply must leave applied_by NULL"
        );
        assert_eq!(answers.as_deref(), Some(r#"{"q1":"rec"}"#));
        let deadline =
            chrono::DateTime::parse_from_rfc3339(confirm_deadline.as_deref().unwrap()).unwrap();
        let applied = chrono::DateTime::parse_from_rfc3339(applied_at.as_deref().unwrap()).unwrap();
        assert_eq!(deadline - applied, CONFIRM_WINDOW);
    }

    #[tokio::test]
    async fn mark_auto_applied_does_not_mint_revert_token() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), None)
            .await
            .unwrap();
        let (token, deadline): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT revert_token, revert_deadline FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            token.is_none(),
            "auto-apply must not mint a revert_token (the user reverts via Caller path)"
        );
        assert!(deadline.is_none());
    }

    #[tokio::test]
    async fn mark_auto_applied_is_no_op_on_non_pending() {
        let pool = fresh_pool().await;
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

    // ---- confirm_proposal / mark_confirmed ----

    #[tokio::test]
    async fn confirm_proposal_promotes_pending_confirm_to_applied() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let out = confirm_proposal(&pool, "p-1", "frodo", true).await.unwrap();
        assert_eq!(out.proposal_id, "p-1");
        assert_eq!(out.confirmed_by, "frodo");
        assert_eq!(out.apply_mode, ApplyMode::Auto);
        assert!(!out.revert_token.is_empty());
        let (status, apply_mode, confirmed_by, token, deadline): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, apply_mode, confirmed_by, revert_token, revert_deadline
                   FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert_eq!(
            apply_mode.as_deref(),
            Some("auto"),
            "confirm must not rewrite apply_mode to 'manual'"
        );
        assert_eq!(confirmed_by.as_deref(), Some("frodo"));
        assert_eq!(token.as_deref(), Some(out.revert_token.as_str()));
        let deadline_dt =
            chrono::DateTime::parse_from_rfc3339(deadline.as_deref().unwrap()).unwrap();
        let confirmed_dt = chrono::DateTime::parse_from_rfc3339(&out.confirmed_at).unwrap();
        assert_eq!(deadline_dt - confirmed_dt, REVERT_WINDOW);
    }

    #[tokio::test]
    async fn confirm_proposal_rejects_non_pending_confirm_row() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "applied", 86_400).await;
        let err = confirm_proposal(&pool, "p-1", "frodo", true)
            .await
            .unwrap_err();
        match err {
            ConfirmError::NotPendingConfirm {
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
    async fn confirm_proposal_rejects_expired_confirm_window() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET confirm_deadline = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind("p-1")
            .execute(&pool)
            .await
            .unwrap();
        let err = confirm_proposal(&pool, "p-1", "frodo", true)
            .await
            .unwrap_err();
        match err {
            ConfirmError::ConfirmWindowExpired {
                proposal_id,
                deadline,
            } => {
                assert_eq!(proposal_id, "p-1");
                assert_eq!(deadline, past);
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_proposal_not_found() {
        let pool = fresh_pool().await;
        let err = confirm_proposal(&pool, "p-missing", "frodo", true)
            .await
            .unwrap_err();
        assert!(matches!(err, ConfirmError::NotFound(ref id) if id == "p-missing"));
    }

    #[tokio::test]
    async fn confirmed_row_can_still_be_reverted_with_minted_token() {
        // End-to-end shape: auto-apply → confirm → revert-with-token must
        // still succeed (the token comes from confirm, not from a manual
        // apply). Uses kind WIKI_PROMOTE with a real spec so the inverse
        // dispatcher doesn't reject on NULL spec.
        let (pool, tree) = fresh_pool_and_tree().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        // Inject a placeholder spec via mark_auto_applied so the revert
        // dispatcher has something to deserialise. The promote inverse
        // will then reject the placeholder shape with InvalidPayload —
        // that's what we assert: the revert path got past the token /
        // deadline gates and reached the kind handler.
        mark_auto_applied(
            &pool,
            "p-1",
            &json!({}),
            Some(&json!({"placeholder": "spec"})),
        )
        .await
        .unwrap();
        let confirm = confirm_proposal(&pool, "p-1", "frodo", true).await.unwrap();
        let err = revert_proposal(
            &pool,
            &tree,
            "p-1",
            RevertAuth::Token(&confirm.revert_token),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RevertError::InvalidPayload(_)),
            "expected promote inverse to reject placeholder spec, got: {err:?}"
        );
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
        let (pool, tree) = fresh_pool_and_tree().await;
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
        let (pool, tree) = fresh_pool_and_tree().await;
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
        let (pool, tree) = fresh_pool_and_tree().await;
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
        let pool = fresh_pool().await;
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
        let report = expire_overdue_proposals_at(&pool, now).await.unwrap();
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

    // ---- auto_finalize_unconfirmed_proposals sweep ----

    #[tokio::test]
    async fn auto_finalize_sweep_skips_pending_confirm_within_window() {
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        // confirm_deadline is 7gg from now — sweep should skip.
        let report = auto_finalize_unconfirmed_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(report.candidates_examined, 0);
        assert!(report.finalized.is_empty());
        // Row stays in applied_pending_confirm.
        let s: String =
            sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind("p-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s, "applied_pending_confirm");
    }

    #[tokio::test]
    async fn auto_finalize_sweep_flips_past_deadline_to_applied_without_token() {
        // Silence past confirm_deadline → applied
        // (locked, no revert_token minted, no event emitted). Kind
        // inverse handler is NOT invoked (the modifications stay on
        // disk).
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(
            &pool,
            "p-1",
            &json!({}),
            Some(&json!({"placeholder": "spec"})),
        )
        .await
        .unwrap();
        // Backdate confirm_deadline into the past so the sweep picks it up.
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET confirm_deadline = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind("p-1")
            .execute(&pool)
            .await
            .unwrap();
        let report = auto_finalize_unconfirmed_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(report.candidates_examined, 1);
        assert_eq!(report.finalized, vec!["p-1".to_owned()]);
        let (status, apply_mode, revert_token, revert_triggered_by): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, apply_mode, revert_token, revert_triggered_by
                   FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied", "silence finalizes to applied");
        assert_eq!(
            apply_mode.as_deref(),
            Some("auto"),
            "apply_mode preserved as 'auto'"
        );
        assert!(
            revert_token.is_none(),
            "no revert_token minted on silent finalize"
        );
        assert!(
            revert_triggered_by.is_none(),
            "finalize is not a revert, no triggered_by stamped"
        );
        // No wiki_events row emitted (silence is silence on both ends).
        let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wiki_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            event_count, 0,
            "finalize emits no event (user already notified at auto_applied time)"
        );
    }

    #[tokio::test]
    async fn auto_finalize_sweep_is_idempotent() {
        // A second run finds nothing more to finalize.
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", 86_400).await;
        mark_auto_applied(&pool, "p-1", &json!({}), Some(&json!({"x": 1})))
            .await
            .unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE structure_proposals SET confirm_deadline = ? WHERE proposal_id = ?")
            .bind(&past)
            .bind("p-1")
            .execute(&pool)
            .await
            .unwrap();
        let first = auto_finalize_unconfirmed_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        let second = auto_finalize_unconfirmed_proposals(&pool, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(first.finalized.len(), 1);
        assert_eq!(second.candidates_examined, 0);
    }

    // ---- expire_overdue_proposals (now grace-period gated) ----

    #[tokio::test]
    async fn expire_sweep_flips_past_grace_only() {
        // Expire requires timeout_at + EXPIRE_GRACE_PERIOD
        // (24h) before the row is given up. A row 1 min past timeout
        // does NOT expire — the auto-apply sweep still owns it.
        let pool = fresh_pool().await;
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

        let report = expire_overdue_proposals(&pool).await.unwrap();
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
        let pool = fresh_pool().await;
        seed(&pool, "p-1", kind::WIKI_PROMOTE, "pending", -(48 * 3600)).await;
        let first = expire_overdue_proposals(&pool).await.unwrap();
        let second = expire_overdue_proposals(&pool).await.unwrap();
        assert_eq!(first.expired, 1);
        assert_eq!(second.expired, 0);
    }
}
