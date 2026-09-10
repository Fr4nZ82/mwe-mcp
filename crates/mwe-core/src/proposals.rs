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

    /// A turn asserted a second, different value for a slot somebody else's
    /// fact already fills, and the speaker has no standing to rewrite it.
    ///
    /// The question goes to the person that fact is about — the card is
    /// theirs, whoever happened to state what is on it: *the memory says this,
    /// somebody else says that — does yours still hold?* Only they answer it,
    /// and the recommended answer is **keep**, so a proposal nobody reaches
    /// leaves the stored value exactly where it was. Nothing else in the
    /// engine ever chooses between two values of one slot — see
    /// [`super::apply_slot_conflict`].
    pub const SLOT_CONFLICT: &str = "slot_conflict";

    /// Every canonical kind.
    pub const ALL: &[&str] = &[
        WIKI_PROMOTE,
        DEDUP_MERGE,
        FACT_FORGET,
        PAGE_CREATE,
        RAIL_ADD,
        SLOT_CONFLICT,
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

/// Whose rows a listing may return.
///
/// A proposal's `context` carries the material the question was raised
/// about — a fact body in full for [`kind::SLOT_CONFLICT`], a 120-character
/// preview of one for the validity and sharing receipts, the title and
/// description of a page for [`kind::PAGE_CREATE`]. None of it is
/// re-projected per reader, so the scope a caller picks here **is** the
/// read ACL for that material, and there is no second check downstream.
///
/// Two narrowed shapes rather than one because callers disagree about the
/// unaddressed rows (`recipient_id IS NULL`), and both of them are right.
/// To something that offers an answer they are the bucket anybody may act
/// on: nobody in particular was asked, so whoever arrives first answers.
/// To something that lists what already happened they are the operator's:
/// the nightly pass addresses none of its receipts, and they name pages
/// across every wiki.
///
/// [`Self::Everybody`] is the `Default` because that is what a caller with
/// no reader behind it wants — a sweep, a test. **A surface that serves a
/// person names its scope**, and never lets it default.
#[derive(Debug, Default, Clone)]
pub enum RecipientScope {
    /// Every recipient's rows, the deployment-wide view.
    #[default]
    Everybody,
    /// Rows addressed to this principal — a `Principal` wire string like
    /// `"user:frodo"` — **plus** the unaddressed ones.
    AddresseeOrNobody(String),
    /// Rows addressed to this principal, and nothing else.
    Addressee(String),
}

/// The clause that lets an **elector** read the request they are voting
/// on.
///
/// A `fact_forget` request is addressed to whoever asked for the forget
/// ([`crate::votes::open_forget_request`]), and the people being asked to
/// vote are everybody else who can read the fact. Scoping on the
/// addressee alone therefore hides the ballot from exactly the people it
/// is a question for: they were told to go and vote and then shown
/// nothing. So the electorate reads it too.
///
/// Narrowed to the one kind that has an electorate, so no other row is
/// ever matched by a stray `eligible_voters` key.
const ELECTOR_CLAUSE: &str = "(kind = 'fact_forget' AND EXISTS (\
     SELECT 1 FROM json_each(structure_proposals.context, '$.eligible_voters') \
      WHERE json_each.value = ?))";

impl RecipientScope {
    /// The principal the scope narrows to, if it narrows at all.
    const fn principal(&self) -> Option<&String> {
        match self {
            Self::Everybody => None,
            Self::AddresseeOrNobody(p) | Self::Addressee(p) => Some(p),
        }
    }

    /// The bare user id inside the principal, which is the form the
    /// electorate is stored in (`["frodo"]`, not `["user:frodo"]`).
    fn voter_id(&self) -> Option<&str> {
        self.principal().and_then(|p| p.strip_prefix("user:"))
    }

    /// The `WHERE` clause that expresses the scope, or `None` for
    /// [`Self::Everybody`].
    ///
    /// Owned rather than `&'static` because the electorate arm is
    /// assembled from two pieces; [`Self::binds`] answers with the values
    /// in the same order.
    fn sql(&self) -> Option<String> {
        match self {
            Self::Everybody => None,
            // 0032: addressed to me OR unaddressed (admin-fallback bucket).
            Self::AddresseeOrNobody(_) => Some(format!(
                "(recipient_id = ? OR recipient_id IS NULL OR {ELECTOR_CLAUSE})"
            )),
            Self::Addressee(_) => Some(format!("(recipient_id = ? OR {ELECTOR_CLAUSE})")),
        }
    }

    /// The values [`Self::sql`] expects, in order: the principal, then the
    /// bare id the electorate is stored under.
    fn binds(&self) -> Vec<String> {
        match (self.principal(), self.voter_id()) {
            (None, _) => Vec::new(),
            (Some(principal), voter) => {
                vec![principal.clone(), voter.unwrap_or(principal).to_owned()]
            },
        }
    }
}

/// Filters for [`list`]. Every field optional + AND-combined.
#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    /// Defaults to `Pending` in the dispatcher; `None` lifts the
    /// filter and returns rows in every state.
    pub status: Option<ProposalStatus>,
    /// Optional kind filter (e.g. `"wiki_promote"`).
    pub kind: Option<String>,
    /// Whose rows come back — the read ACL for the listing, see
    /// [`RecipientScope`].
    pub recipient: RecipientScope,
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
    let mut clauses: Vec<&str> = Vec::new();
    if filters.status.is_some() {
        clauses.push("status = ?");
    }
    if filters.kind.is_some() {
        clauses.push("kind = ?");
    }
    let scope_clause = filters.recipient.sql();
    if let Some(clause) = scope_clause.as_deref() {
        clauses.push(clause);
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
    for value in filters.recipient.binds() {
        q = q.bind(value);
    }
    let rows: Vec<ListTuple> = q.bind(limit).fetch_all(pool).await?;

    rows.into_iter().map(decode).collect()
}

/// One proposal by id, within `scope`.
///
/// `None` covers both "there is no such row" and "the scope does not
/// reach it", on purpose: a caller that could tell them apart would turn
/// the id into a way of asking whether somebody else's proposal exists.
///
/// Separate from [`list`] because a row is looked up by name, not by
/// position: finding it inside a page of the newest rows would lose it
/// as soon as enough newer ones were written, and every link to a
/// proposal outlives that window.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
/// - [`ProposalsError::Json`] when the stored `questions` / `context`
///   blob is no longer valid JSON.
pub async fn get(
    pool: &SqlitePool,
    proposal_id: &str,
    scope: &RecipientScope,
) -> Result<Option<ProposalRow>> {
    let mut sql = String::from(
        "SELECT proposal_id, kind, context, questions, proposed_at, timeout_at, status,
                applied_at, applied_by, recipient_id
           FROM structure_proposals
          WHERE proposal_id = ?",
    );
    if let Some(clause) = scope.sql() {
        sql.push_str(" AND ");
        sql.push_str(&clause);
    }
    let mut q = sqlx::query_as::<_, ListTuple>(&sql).bind(proposal_id);
    for value in scope.binds() {
        q = q.bind(value);
    }
    q.fetch_optional(pool).await?.map(decode).transpose()
}

/// Turn one selected row into a [`ProposalRow`], decoding the two JSON
/// columns. The one place the column order of the `SELECT`s above is
/// tied to the struct.
fn decode(row: ListTuple) -> Result<ProposalRow> {
    let (
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
    ) = row;
    Ok(ProposalRow {
        proposal_id,
        kind,
        context: serde_json::from_str(&context)?,
        questions: serde_json::from_str(&questions)?,
        emitted_at: proposed_at,
        expires_at: timeout_at,
        status: parse_status_lenient(&status),
        applied_at,
        applied_by,
        recipient_id,
    })
}

fn parse_status_lenient(s: &str) -> ProposalStatus {
    ProposalStatus::from_str(s).unwrap_or(ProposalStatus::Pending)
}

// ---------- In-flight count ----------

/// Count the `pending` rows — the proposals still waiting on somebody.
///
/// The only rows anybody can still act on: once a proposal is applied
/// the change stands. `scope` is the same read ACL [`list`] takes, so the
/// badge counts exactly what the page will show — a badge that counts a
/// row the page then withholds sends somebody looking for something that
/// is not there.
///
/// Used by the dashboard in-flight badge.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
pub async fn count_pending(pool: &SqlitePool, scope: &RecipientScope) -> Result<i64> {
    let mut sql = String::from("SELECT COUNT(*) FROM structure_proposals WHERE status = 'pending'");
    if let Some(clause) = scope.sql() {
        sql.push_str(" AND ");
        sql.push_str(&clause);
    }
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    for value in scope.binds() {
        q = q.bind(value);
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

/// The addressee of a question about somebody's identity card: its SUBJECT,
/// whoever happened to say the value that is on it.
///
/// A card belongs to the person it is about, and the one question this
/// addresses — *are these two values both yours, and which is right?* — is
/// theirs to answer. [`recipient_from_fact`] answers a different question
/// (*who should hear that this fact changed?*) and prefers the author for good
/// reasons of its own; borrowing it here sent the question to whoever had
/// spoken last, so a number on Zoe's card that Bob had given went to Bob to
/// settle.
///
/// A group's card has no single person to write to, and the recipient column
/// addresses one: those stay **unaddressed**, which is the bucket the admin
/// already works. Writing `group:<id>` there instead would address it to
/// somebody nothing lets act — no member can apply it and the listing hides
/// it from all of them.
#[must_use]
pub fn recipient_of_the_card(subject: &crate::types::Principal) -> Option<String> {
    match subject {
        crate::types::Principal::User(id) => Some(format!("user:{id}")),
        crate::types::Principal::Group(_) => None,
    }
}

/// Whether a question about this fact and this slot is already waiting on
/// somebody, and which one.
///
/// Three turns restating the same value are one disagreement: without this the
/// card's owner gets the same question three times, each carrying the value
/// they were the only one entitled to read. The pair *(fact already stored,
/// slot)* is the identity of the question — the asserted value may be worded
/// differently each time and it is still the same box being argued over.
///
/// The scan is over PENDING rows — `idx_struct_status` — and not over the
/// table, which is the whole history of everything the memory rearranged and
/// which nothing prunes.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
pub async fn pending_slot_conflict(
    pool: &SqlitePool,
    kept: &crate::types::FactId,
    slot: &str,
) -> Result<Option<String>> {
    let found: Option<(String,)> = sqlx::query_as(
        "SELECT proposal_id FROM structure_proposals
          WHERE kind = ? AND status = 'pending'
            AND json_extract(context, '$.kept_fact_id') = ?
            AND json_extract(context, '$.slot') = ?
          LIMIT 1",
    )
    .bind(kind::SLOT_CONFLICT)
    .bind(kept.as_str())
    .bind(slot)
    .fetch_optional(pool)
    .await?;
    Ok(found.map(|(id,)| id))
}

/// Split a batch of applied changes into one group per addressee.
///
/// A turn closes what it contradicts, and a sweep closes a whole cluster:
/// either can touch facts belonging to **different people**, because
/// somebody a fact was shared with may close it. One receipt for the
/// batch would then be addressed to whoever the first fact happened to
/// name, and would carry a preview of everybody else's text to them —
/// the receipts store 120 characters of each fact they record, and
/// nothing re-projects that per reader.
///
/// So the emitters group first and write one receipt per group, each
/// carrying only the facts of the person who receives it. Groups keep
/// first-appearance order, so a batch with one addressee stays one
/// receipt and the common case is unchanged.
///
/// `recipient_of` is [`recipient_from_fact`] applied to whatever the
/// caller has that knows the fact's subject and author.
pub fn group_by_recipient<T: Clone>(
    items: &[T],
    recipient_of: impl Fn(&T) -> Option<String>,
) -> Vec<(Option<String>, Vec<T>)> {
    let mut groups: Vec<(Option<String>, Vec<T>)> = Vec::new();
    for item in items {
        let recipient = recipient_of(item);
        match groups.iter_mut().find(|(who, _)| *who == recipient) {
            Some((_, batch)) => batch.push(item.clone()),
            None => groups.push((recipient, vec![item.clone()])),
        }
    }
    groups
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
        kind::SLOT_CONFLICT => {
            let spec = apply_slot_conflict(pool, context, answers).await?;
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

// ---------- slot conflict ----------

/// The answer that leaves the memory exactly as it is — and the one the
/// timeout sweep picks, so an unanswered conflict never changes anything.
const SLOT_VERDICT_KEEP: &str = "keep";
/// The answer that retires the stored value.
const SLOT_VERDICT_RETIRE: &str = "retire";
/// The question id both answers are given under.
const SLOT_QUESTION_ID: &str = "verdict";

/// One slot conflict, as the engine hands it to the person who can settle it.
///
/// Two facts want the same slot — a birth date, a residence, a contact, a
/// password — and they say different things, and the person who spoke last may
/// not rewrite the one already stored. The engine will not choose: it asks.
/// This carries everything the question needs so the dashboard can put it in
/// words without going back to the store.
#[derive(Debug, Clone)]
pub struct SlotConflict {
    /// The one thing both values state. A box of the identity card
    /// (`ingest::CARD_SLOTS`) where the question comes from a card, and the
    /// reconciler's own free wording — "the wifi password" — where it comes
    /// from a supersede somebody could not apply. Together with
    /// [`Self::kept_fact_id`] it is what makes two questions the same question
    /// ([`pending_slot_conflict`]).
    pub slot: String,
    /// Who the stored fact is about.
    pub subject: crate::types::Principal,
    /// The fact already stored.
    pub kept_fact_id: crate::types::FactId,
    /// What it says.
    pub kept_text: String,
    /// Who said it, when the fact records an author.
    pub kept_sender: Option<crate::types::Principal>,
    /// The day it was said (date only — the hour is noise in a question).
    pub kept_said_on: Option<String>,
    /// The value this turn asserted instead.
    pub asserted_text: String,
    /// Who asserted it.
    pub asserted_by: crate::types::Principal,
    /// The asserted value as a filed fact, when it was filed. `None` when the
    /// engine held it back — and then the words are in [`Self::parked`]
    /// instead, waiting on this answer.
    pub successor: Option<crate::types::FactId>,
    /// The asserted value PARKED in the capture buffer
    /// (`capture_buffer::CaptureStatus::Held`): written down, in no queue, read
    /// by nothing. Answering `retire` releases it into the queue, so the card
    /// ends up saying what the recipient just said is right; answering `keep`,
    /// or letting the question time out, drops it.
    ///
    /// `None` alongside a [`Self::successor`] (the claim was filed, so there is
    /// nothing to park) and `None` when the parking itself failed, which costs
    /// the answer its second half and nothing else.
    pub parked: Option<crate::types::FactId>,
    /// Why the assertion could not simply replace the fact, in one clause the
    /// recipient can read.
    pub refusal: String,
}

impl SlotConflict {
    /// The sentence the dashboard asks.
    fn question(&self) -> String {
        let said_by = self
            .kept_sender
            .as_ref()
            .map_or_else(|| "nobody on record".to_owned(), ToString::to_string);
        let said_on = self
            .kept_said_on
            .as_deref()
            .map_or_else(String::new, |d| format!(" on {d}"));
        // The slot rides in a clause that disappears when it is empty: a
        // sentence with a hole where the name of the thing should be is worse
        // than one that simply quotes the two values.
        let slot = match self.slot.trim() {
            "" => String::new(),
            named => format!(", which gives {named}"),
        };
        format!(
            "About {subject}, the memory holds \u{ab}{kept}\u{bb} (said by \
             {said_by}{said_on}){slot}. {by} said \u{ab}{asserted}\u{bb} instead, and \
             {refusal}, so the memory was left as it is. Does the stored value still hold?",
            subject = self.subject,
            kept = self.kept_text,
            by = self.asserted_by,
            asserted = self.asserted_text,
            refusal = self.refusal,
        )
    }

    /// The stored `context` the apply handler reads back.
    fn context(&self) -> Value {
        serde_json::json!({
            "variant": kind::SLOT_CONFLICT,
            "slot": self.slot,
            "subject_id": self.subject.to_string(),
            "kept_fact_id": self.kept_fact_id.as_str(),
            "kept_text": self.kept_text,
            "kept_sender": self.kept_sender.as_ref().map(ToString::to_string),
            "kept_said_on": self.kept_said_on,
            "asserted_text": self.asserted_text,
            "asserted_by": self.asserted_by.to_string(),
            "successor_fact_id": self.successor.as_ref().map(|f| f.as_str().to_owned()),
            "parked_capture_id": self.parked.as_ref().map(|f| f.as_str().to_owned()),
            "refusal": self.refusal,
        })
    }
}

/// Open the slot conflict as a pending proposal addressed to whoever can
/// settle it, and return its `proposal_id`.
///
/// Addressed to the SUBJECT of the card ([`recipient_of_the_card`]) and never
/// to the author of the value stored on it. The two are often the same person
/// and the question reads the same either way — but a value on Zoe's card that
/// Bob happened to state is still Zoe's, and sending Bob the question hands
/// him a decision about somebody else's record while the notice the speaker
/// gets, this guide and the changelog all say Zoe was asked.
///
/// **`keep` is the recommended answer**, which is what the timeout sweep
/// applies. A conflict nobody answers therefore leaves the memory as it
/// stands — the stored value kept, the parked claim dropped — and that is the
/// correct outcome rather than a fallback: the stored value was stated by
/// somebody entitled to state it, and silence is not a reason to drop it.
///
/// # Errors
///
/// - [`ProposalsError::Db`] for any SQL failure.
/// - [`ProposalsError::Json`] when the context or questions cannot be
///   serialised.
pub async fn emit_slot_conflict(pool: &SqlitePool, c: &SlotConflict) -> Result<String> {
    // What "no" actually does depends on whether there is another value to
    // install: the one that was said is either filed already (a successor) or
    // parked waiting on this answer. With neither — the parking failed — the
    // answer can only retire what is stored, and the option says so rather
    // than promising a replacement that will not arrive.
    let has_a_replacement = c.successor.is_some() || c.parked.is_some();
    let retire_text = if has_a_replacement {
        "No — it is wrong; stop asserting it, and record what was said instead."
    } else {
        "No — it is wrong; stop asserting it."
    };
    let keep_text = if has_a_replacement {
        "Yes — it still holds; keep it, and drop what was said instead."
    } else {
        "Yes — it still holds; keep it and write nothing."
    };
    let questions = serde_json::json!([{
        "id": SLOT_QUESTION_ID,
        "text": c.question(),
        "options": [
            {
                "id": SLOT_VERDICT_KEEP,
                "value": SLOT_VERDICT_KEEP,
                "text": keep_text,
                "recommended": true,
            },
            {
                "id": SLOT_VERDICT_RETIRE,
                "value": SLOT_VERDICT_RETIRE,
                "text": retire_text,
                "recommended": false,
            },
        ],
    }]);
    emit_proposal(
        pool,
        EmitParams::new(kind::SLOT_CONFLICT, c.context(), questions)
            .with_recipient(recipient_of_the_card(&c.subject)),
    )
    .await
}

/// Apply a `slot_conflict` proposal: the recipient said whether the stored
/// value still holds.
///
/// - `keep` — the stored value stands and the claim that argued with it is
///   dropped. This is also what the timeout sweep applies, so silence leaves
///   the card exactly as it was.
/// - `retire` — the stored value stops being asserted, and the value the
///   recipient just called right takes its place. With a `successor_fact_id`
///   (the claim was filed and only the replacement was refused) the two are
///   welded, so a reader of the old value is sent to the fact that replaced
///   it. With a `parked_capture_id` (the claim was never written) the parked
///   words are released into the queue, and the light dream promotes them onto
///   the card like any other capture — authored by whoever said them, about
///   whoever this card belongs to.
///
///   **This is not the engine writing a claim on somebody's behalf.** It is
///   the person the card belongs to saying which of two values is theirs,
///   which is the one answer nobody else could give and the whole reason they
///   were asked. Retiring the old value and leaving the box empty made the
///   answer a half-answer: the right value existed only inside the question.
///
/// Releasing and dropping are both best-effort — the verdict on the stored
/// value is the load-bearing half and it has already landed.
///
/// # Errors
///
/// [`ApplyError::InvalidPayload`] when the context is missing the fact it
/// names or carries an unparseable id; [`ApplyError::HandlerData`] when the
/// write fails.
async fn apply_slot_conflict(
    pool: &SqlitePool,
    context: &Value,
    answers: &Value,
) -> std::result::Result<Value, ApplyError> {
    let kept_raw = context
        .get("kept_fact_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApplyError::InvalidPayload("slot_conflict context missing kept_fact_id".into())
        })?;
    let kept = crate::types::FactId::parse(kept_raw).map_err(|e| {
        ApplyError::InvalidPayload(format!("slot_conflict kept_fact_id {kept_raw:?}: {e}"))
    })?;
    let verdict = answers
        .get(SLOT_QUESTION_ID)
        .and_then(Value::as_str)
        .unwrap_or(SLOT_VERDICT_KEEP);
    let parked = context
        .get("parked_capture_id")
        .and_then(Value::as_str)
        .map(crate::types::FactId::parse)
        .transpose()
        .map_err(|e| ApplyError::InvalidPayload(format!("slot_conflict parked: {e}")))?;
    if verdict != SLOT_VERDICT_RETIRE {
        // The card stands, so the words that argued with it are not memory and
        // never were.
        let dropped = drop_parked_claim(pool, parked.as_ref()).await;
        return Ok(serde_json::json!({
            "variant": kind::SLOT_CONFLICT,
            "verdict": SLOT_VERDICT_KEEP,
            "kept_fact_id": kept_raw,
            "retired": 0,
            "released": 0,
            "dropped": i32::from(dropped),
        }));
    }
    let successor = context
        .get("successor_fact_id")
        .and_then(Value::as_str)
        .map(crate::types::FactId::parse)
        .transpose()
        .map_err(|e| ApplyError::InvalidPayload(format!("slot_conflict successor: {e}")))?;
    let now = chrono::Utc::now();
    let retired = match &successor {
        Some(new_fact) => crate::fact_index::mark_superseded(pool, &kept, new_fact, now)
            .await
            .map_err(|e| ApplyError::HandlerData(format!("slot_conflict weld: {e}")))?,
        None => crate::fact_index::close_validity(
            pool,
            &kept,
            &crate::fact_index::bound_from_instant(now),
            crate::fact_index::decay::CONTRADICTED,
            None,
        )
        .await
        .map_err(|e| ApplyError::HandlerData(format!("slot_conflict closure: {e}")))?
        .map_or(0, |_| 1),
    };
    let released = match parked.as_ref() {
        Some(id) => match crate::capture_buffer::release_held(pool, id).await {
            Ok(moved) => moved,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    parked_capture_id = id.as_str(),
                    "proposals: the parked claim stayed parked — the stored value is retired \
                     and the box is empty until somebody states it again"
                );
                false
            },
        },
        None => false,
    };
    Ok(serde_json::json!({
        "variant": kind::SLOT_CONFLICT,
        "verdict": SLOT_VERDICT_RETIRE,
        "kept_fact_id": kept_raw,
        "successor_fact_id": successor.as_ref().map(|f| f.as_str().to_owned()),
        "parked_capture_id": parked.as_ref().map(|f| f.as_str().to_owned()),
        "retired": retired,
        "released": i32::from(released),
        "dropped": 0,
    }))
}

/// Drop the claim parked behind a slot question, and say whether one was
/// there. Best-effort: a row that outlives its question is invisible to every
/// reader of the buffer, so failing here leaves nothing a person can see.
async fn drop_parked_claim(pool: &SqlitePool, parked: Option<&crate::types::FactId>) -> bool {
    let Some(id) = parked else { return false };
    match crate::capture_buffer::discard_held(pool, id).await {
        Ok(dropped) => dropped,
        Err(e) => {
            tracing::warn!(
                error = %e,
                parked_capture_id = id.as_str(),
                "proposals: a parked claim could not be dropped (it stays out of every queue)"
            );
            false
        },
    }
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
/// Points at the per-proposal **open-in-chat** primer
/// (`GET /dashboard/proposals/:id/open-in-chat`): it lands the user inside the
/// dashboard's agentic chat with the proposal already summarised, where they
/// can ask to modify it. Acting on a proposal is a conversation, so a link
/// offered to somebody who can still answer opens one — the dashboard's own
/// per-proposal page (`GET /dashboard/proposals/:id`) reads the row and
/// nothing else. Relative on purpose: mwe-mcp has
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
    use crate::types::FactId;
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

    // ---- slot_conflict ----

    /// The fact already on the card, and the one a later turn filed beside it.
    const KEPT_ID: &str = "0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d91";
    const ASSERTED_ID: &str = "0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d92";

    /// Seed one active identity-core fact, returning its id.
    async fn seed_fact(
        pool: &SqlitePool,
        fact_id: &str,
        subject: &str,
        sender: Option<&str>,
    ) -> FactId {
        let fact_id = FactId::parse(fact_id).expect("well-formed fact id");
        crate::fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "bob".to_owned(),
                source_path: "wikis/bob/@profile.md".to_owned(),
                region_start: Some(0),
                region_end: Some(32),
                text: "born on 31 October 2017 at 08:10".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                subject_id: subject.parse().unwrap(),
                allow_ids: Vec::new(),
                sender_id: sender.map(|s| s.parse().unwrap()),
                fact_type: Some("bio".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: Some("high".to_owned()),
                source_ref: None,
            },
        )
        .await
        .expect("insert fact");
        fact_id
    }

    fn conflict(kept: &FactId, successor: Option<&FactId>) -> SlotConflict {
        SlotConflict {
            slot: "the date of birth".to_owned(),
            subject: "user:bob".parse().unwrap(),
            kept_fact_id: kept.clone(),
            kept_text: "born on 31 October 2017 at 08:10".to_owned(),
            kept_sender: Some("user:bob".parse().unwrap()),
            kept_said_on: Some("2026-07-02".to_owned()),
            asserted_text: "born on 8 July 2012".to_owned(),
            asserted_by: "user:carol".parse().unwrap(),
            successor: successor.cloned(),
            parked: None,
            refusal: "they are neither its subject nor the person who said it".to_owned(),
        }
    }

    /// A slot conflict nobody answers changes nothing.
    ///
    /// The recommended option is `keep`, so the timeout sweep applies `keep`,
    /// and applying `keep` writes nothing. The value on the card was put there
    /// by somebody entitled to put it there; silence is not a reason to drop
    /// it, and a sweep that reached for the *asserted* value instead would be
    /// the engine choosing between two people's claims — the one thing this
    /// whole path exists to stop.
    #[tokio::test]
    async fn an_unanswered_slot_conflict_keeps_the_stored_value() {
        let (dir, pool, tree) = fresh_pool_and_tree().await;
        let kept = seed_fact(&pool, KEPT_ID, "user:bob", Some("user:bob")).await;
        let id = emit_slot_conflict(&pool, &conflict(&kept, None))
            .await
            .expect("emit");
        let questions: String =
            sqlx::query_scalar("SELECT questions FROM structure_proposals WHERE proposal_id = ?")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let answers = build_recommended_answers(&questions).expect("recommended");
        assert_eq!(answers["verdict"], "keep", "silence keeps what is stored");

        auto_apply_proposal(&pool, &tree, &id, &answers)
            .await
            .expect("auto-apply");
        let row = crate::fact_index::find_by_id(&pool, &kept)
            .await
            .unwrap()
            .expect("fact still there");
        assert!(row.valid_to.is_none(), "the stored value is still current");
        assert!(row.superseded_at.is_none(), "and nothing replaced it");
        drop(dir);
    }

    /// Answering `retire` with a filed replacement welds the two.
    ///
    /// This is the reconciler's refused supersede, settled by the person who
    /// could settle it: the assertion was already filed and only the
    /// replacement was refused, so the old value is retired **pointing at**
    /// the new one and a reader of the old is sent to the current truth.
    #[tokio::test]
    async fn retiring_with_a_successor_welds_them() {
        let (dir, pool, tree) = fresh_pool_and_tree().await;
        let kept = seed_fact(&pool, KEPT_ID, "user:bob", Some("user:bob")).await;
        let successor = seed_fact(&pool, ASSERTED_ID, "user:bob", Some("user:carol")).await;
        let id = emit_slot_conflict(&pool, &conflict(&kept, Some(&successor)))
            .await
            .expect("emit");
        apply_proposal(
            &pool,
            &tree,
            &id,
            &json!({ "verdict": "retire" }),
            Some("bob"),
            false,
        )
        .await
        .expect("apply");
        let row = crate::fact_index::find_by_id(&pool, &kept)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.superseded_by.as_ref().map(FactId::as_str),
            Some(successor.as_str()),
            "the retired value points at the one that replaced it"
        );
        drop(dir);
    }

    /// Answering `retire` with neither a filed replacement nor a parked claim
    /// retires the old value and mints nothing.
    ///
    /// The apply chassis is not a place a fact is born out of nothing: it
    /// welds a successor that was filed, or releases a claim that was parked,
    /// and where there is neither the window simply closes as `contradicted`.
    /// A conflict raised on this shape is one whose parking failed, and the
    /// answer's load-bearing half — the stored value stops being asserted —
    /// still lands.
    #[tokio::test]
    async fn retiring_without_a_successor_mints_nothing() {
        let (dir, pool, tree) = fresh_pool_and_tree().await;
        let kept = seed_fact(&pool, KEPT_ID, "user:bob", Some("user:bob")).await;
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM fact_index")
            .fetch_one(&pool)
            .await
            .unwrap();
        let id = emit_slot_conflict(&pool, &conflict(&kept, None))
            .await
            .expect("emit");
        apply_proposal(
            &pool,
            &tree,
            &id,
            &json!({ "verdict": "retire" }),
            Some("bob"),
            false,
        )
        .await
        .expect("apply");
        let row = crate::fact_index::find_by_id(&pool, &kept)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_some(), "the rejected value stops holding");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(crate::fact_index::decay::CONTRADICTED)
        );
        assert!(
            row.superseded_at.is_none(),
            "nothing replaced it — no fact was minted from a dashboard click"
        );
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM fact_index")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after, "no new fact was written");
        drop(dir);
    }

    /// The question goes to the person who can answer it — the subject of the
    /// stored fact, never the speaker who could not rewrite it.
    #[tokio::test]
    async fn a_slot_conflict_is_addressed_to_the_stored_facts_owner() {
        let (dir, pool) = fresh_pool().await;
        let kept = seed_fact(&pool, KEPT_ID, "user:bob", Some("user:bob")).await;
        let id = emit_slot_conflict(&pool, &conflict(&kept, None))
            .await
            .expect("emit");
        let recipient: Option<String> = sqlx::query_scalar(
            "SELECT recipient_id FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(recipient.as_deref(), Some("user:bob"));
        drop(dir);
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
    fn kind_constants_are_the_six_the_engine_emits() {
        assert_eq!(kind::WIKI_PROMOTE, "wiki_promote");
        assert_eq!(kind::DEDUP_MERGE, "dedup_merge");
        assert_eq!(kind::FACT_FORGET, "fact_forget");
        assert_eq!(kind::PAGE_CREATE, "page_create");
        assert_eq!(kind::RAIL_ADD, "rail_add");
        assert_eq!(kind::SLOT_CONFLICT, "slot_conflict");
        // Three questionnaire kinds, the fact-forget vote, and two
        // receipt-only kinds — never `pending`, emitted born-applied so what
        // the engine decided about the shape of the memory (a page it
        // invented, a link it required) leaves a record the owner can read.
        assert_eq!(kind::ALL.len(), 6);
        assert!(kind::is_canonical("wiki_promote"));
        assert!(kind::is_canonical("fact_forget"));
        assert!(kind::is_canonical("page_create"));
        assert!(kind::is_canonical("rail_add"));
        assert!(kind::is_canonical("slot_conflict"));
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

    /// The two narrowed scopes differ on exactly one row: the one nobody
    /// was addressed with. [`RecipientScope::AddresseeOrNobody`] hands it
    /// over (it is the bucket anybody may answer),
    /// [`RecipientScope::Addressee`] does not (it is a receipt of what the
    /// engine did, and it describes pages its reader may not be able to
    /// open). Somebody else's row is out of both.
    #[tokio::test]
    async fn the_unaddressed_bucket_is_what_the_two_narrow_scopes_disagree_on() {
        let (_workdir, pool) = fresh_pool().await;
        seed_addressed(&pool, "p-mine", Some("user:frodo")).await;
        seed_addressed(&pool, "p-nobody", None).await;
        seed_addressed(&pool, "p-theirs", Some("user:bilbo")).await;

        let ids = |rows: Vec<ProposalRow>| {
            let mut out: Vec<String> = rows.into_iter().map(|r| r.proposal_id).collect();
            out.sort();
            out
        };

        let with_bucket = list(
            &pool,
            &ListFilters {
                recipient: RecipientScope::AddresseeOrNobody("user:frodo".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ids(with_bucket), ["p-mine", "p-nobody"]);

        let without_bucket = list(
            &pool,
            &ListFilters {
                recipient: RecipientScope::Addressee("user:frodo".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ids(without_bucket), ["p-mine"]);

        let everybody = list(
            &pool,
            &ListFilters {
                recipient: RecipientScope::Everybody,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ids(everybody), ["p-mine", "p-nobody", "p-theirs"]);
    }

    /// A batch that touches several people is split into one group each,
    /// keeping the order they first appear in.
    #[test]
    fn a_batch_is_split_into_one_group_per_person() {
        let items = ["frodo", "bilbo", "frodo", "nobody"];
        let groups = group_by_recipient(&items, |who| {
            (*who != "nobody").then(|| format!("user:{who}"))
        });
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0.as_deref(), Some("user:frodo"));
        assert_eq!(groups[0].1, ["frodo", "frodo"]);
        assert_eq!(groups[1].0.as_deref(), Some("user:bilbo"));
        assert_eq!(groups[2].0, None);
    }

    /// A batch with one addressee stays one receipt: the common case is
    /// untouched by the split.
    #[test]
    fn one_addressee_is_still_one_group() {
        let items = [1, 2, 3];
        let groups = group_by_recipient(&items, |_| Some("user:frodo".to_owned()));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, [1, 2, 3]);
    }

    /// The people being asked to vote can read the request.
    ///
    /// A forget request is addressed to whoever asked for the forget, and
    /// the electorate is everybody else who can read the fact. Scoping on
    /// the addressee alone hid the ballot from exactly the people it is a
    /// question for — and both halves matter: the elector reads it, and a
    /// third person still does not.
    #[tokio::test]
    async fn an_elector_reads_the_forget_request_and_a_stranger_does_not() {
        let (_workdir, pool) = fresh_pool().await;
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status, recipient_id) \
             VALUES ('p-vote', ?, ?, '[]', ?, ?, 'pending', 'user:frodo')",
        )
        .bind(kind::FACT_FORGET)
        .bind(
            serde_json::json!({
                "variant": "fact_forget",
                "fact_id": "0197fa00-0000-7000-8000-000000000001",
                "requester": "frodo",
                "eligible_voters": ["bilbo", "carol"],
            })
            .to_string(),
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

        let seen_by = |who: &str| {
            let pool = pool.clone();
            let scope = RecipientScope::Addressee(format!("user:{who}"));
            async move {
                list(
                    &pool,
                    &ListFilters {
                        recipient: scope,
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .len()
            }
        };
        assert_eq!(seen_by("frodo").await, 1, "the requester is the addressee");
        assert_eq!(seen_by("bilbo").await, 1, "an elector is being asked");
        assert_eq!(seen_by("carol").await, 1, "so is the other elector");
        assert_eq!(seen_by("sam").await, 0, "nobody else is");

        // The count the badge reads answers the same way, or it promises
        // something the page will not show.
        assert_eq!(
            count_pending(&pool, &RecipientScope::Addressee("user:bilbo".to_owned()))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            count_pending(&pool, &RecipientScope::Addressee("user:sam".to_owned()))
                .await
                .unwrap(),
            0
        );
    }

    /// The elector clause is scoped to the one kind that has an
    /// electorate, so a stray key on another row never widens a scope.
    #[tokio::test]
    async fn only_a_forget_request_has_an_electorate() {
        let (_workdir, pool) = fresh_pool().await;
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status, recipient_id) \
             VALUES ('p-other', ?, ?, '[]', ?, ?, 'pending', 'user:frodo')",
        )
        .bind(kind::SLOT_CONFLICT)
        .bind(serde_json::json!({ "eligible_voters": ["bilbo"] }).to_string())
        .bind(chrono::Utc::now().to_rfc3339())
        .bind((chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();
        let rows = list(
            &pool,
            &ListFilters {
                recipient: RecipientScope::Addressee("user:bilbo".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            rows.is_empty(),
            "only fact_forget puts a question to voters"
        );
    }

    /// One `pending` row with an explicit addressee.
    async fn seed_addressed(pool: &SqlitePool, proposal_id: &str, recipient: Option<&str>) {
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
             proposed_at, timeout_at, status, recipient_id) \
             VALUES (?, ?, '{}', '[]', ?, ?, 'pending', ?)",
        )
        .bind(proposal_id)
        .bind(kind::SLOT_CONFLICT)
        .bind(now.to_rfc3339())
        .bind((now + chrono::Duration::hours(24)).to_rfc3339())
        .bind(recipient)
        .execute(pool)
        .await
        .unwrap();
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
