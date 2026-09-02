// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ingest pipeline — `wiki_ingest_message`.
//!
//! This is the flagship MCP tool (ingest pipeline):
//! the single conversational entry point a consumer LLM agent talks to,
//! every turn. The orchestrator owns the "messaggio raw → memoria
//! gestita" loop end-to-end so the agent stays agnostic of structure,
//! paths, and routing decisions.
//!
//! ## Pipeline (per call)
//!
//! ```text
//! 1. recall context        recall::wiki_recall   (ranked hits, ACL filtered)
//! 2. enumerate wikis       WikiTree::walk        (engine-internal — the classifier is shown NO wikis)
//! 3. LLM intent + plan     llm::complete         (`ingest` slot, JSON out)
//! 4. route by intent       capture::wiki_capture | recall snippet | dashboard hint | noop
//! 5. legacy closure path  confirm_topic_closures (`ingest` slot — see below: the SHIPPED prompt never reaches it)
//! 6. recall-block tail     recall_nav::navigate  (`navigator` slot, optional) + recall::recall_due_soon
//! 7. reconcile what was read  reconcile_after_reading (`ingest` slot — supersede / close / re-date / re-share)
//! 8. assemble response     IngestResponse        (context_snippet + suggested_seed + capture_id)
//! ```
//!
//! **Two model calls on the `ingest` slot per turn**, not one and not three:
//! the classifier (step 3) and the reconciler (step 7).
//!
//! Step 3 is one call and that is the claim — the classifier is asked for one
//! strict JSON object encoding both the intent and the operational plan
//! rather than intent → routing → seed as three round trips, which keeps
//! latency inside the conversational budget. Step 7 is separate on purpose:
//! it acts on **what the turn has since read**, which step 3 had not seen.
//!
//! **Step 5 is a compatibility path, not a phase.** The shipped classifier
//! prompt emits neither `closures` nor `closure_topics` — reconciling against
//! stored facts is the reconciler's job — so `plan.closures` is empty and
//! this step does nothing. It stays wired for a deployment whose operator
//! overrode `prompts/ingest.md` with a version that still emits them.
//!
//! ## Fallback policy
//!
//! Every error path that the spec marks as "503 `llm_unavailable`" or
//! "ingest could not route" demotes to `IntentKind::Skip` with a canned
//! `suggested_seed`. The orchestrator never explodes in front of the
//! consumer — its contract is "always return something the agent can
//! turn into a reply". Real server-side failures (database, embedder,
//! filesystem) still propagate as [`IngestError`] so they reach the
//! transport layer's error mapping.
//!
//! ## What is deferred
//!
//! - **Disambiguation follow-up** (`disambig_choice`): when the
//!   consumer re-calls with the candidate the user picked, the
//!   orchestrator forwards the choice to the LLM prompt as an explicit
//!   "user resolved the ambiguity to `<id>`" line. The classifier is
//!   instructed to commit (no `needs_disambig=true` on the second
//!   turn). Plumbed alongside the dispatcher.
//! - **Structural proposals**: an `intent=structural` outcome surfaces
//!   the dashboard suggestion but does not yet emit a
//!   `structure_proposal` row — that lands with the REM interpreter
//!   which owns the proposal lifecycle.
//! - **`recent_messages` weighting in recall**: passed through as
//!   prompt context for the LLM, not (yet) used by `wiki_recall` to
//!   skew scoring. Same deferral note as in [`recall`].
//! - **Audit log row** in `tool_log_search`: once the audit
//!   table ships.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::acl;
use crate::capture::{self, CaptureAction, CaptureError, CaptureOutcome, CaptureRequest};
use crate::capture_buffer::{self, CaptureBufferError};
use crate::disclosure_audit;
use crate::embedder::Embedder;
use crate::enrollment;
use crate::events::{self, EventKind};
use crate::fact_index;
use crate::llm::{CompletionRequest, ImageInput, LlmBackend};
use crate::locale;
use crate::media;
use crate::promote;
use crate::prompts;
use crate::proposals;
use crate::recall::{self, DEFAULT_DEDUP_THRESHOLD, RecallError, RecallHit, SenderContext};
use crate::recall_log;
use crate::recall_nav;
use crate::types::{
    CatalogId, FactId, FactIdParseError, Principal, PrincipalParseError, WikiId, WikiIdParseError,
};
use crate::wiki::{self, WikiError, WikiTree, is_safe_page_path};

// ---------- Public input types ----------

/// Conversational role of a [`RecentMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// User-authored turn.
    User,
    /// Assistant / consumer-agent turn.
    Assistant,
}

impl MessageRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One element of the rolling conversational context.
///
/// Carried alongside the current message and surfaced to the LLM for
/// coreference resolution. The body is bounded by
/// [`IngestPolicy::max_recent_message_chars`] before prompt injection.
#[derive(Debug, Clone)]
pub struct RecentMessage {
    /// Who authored the turn.
    pub role: MessageRole,
    /// Body text. Trimmed in the prompt at the policy ceiling.
    pub text: String,
    /// Optional ISO 8601 wall-clock. Surfaced as a prompt hint when
    /// present, ignored otherwise.
    pub timestamp: Option<String>,
}

/// Hint about the call site of `wiki_ingest_message`. Routed to the
/// LLM verbatim — the classifier may use it to bias intent (e.g.
/// `DashboardCommand` skews toward `structural`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextHint {
    /// Standard conversational turn from the consumer agent — the
    /// default the spec calls out for `wiki_ingest_message`.
    #[default]
    Conversation,
    /// The user is typing in the dashboard chat. Bias toward
    /// structural intent.
    DashboardCommand,
    /// Batch ingestion of an external corpus (`wiki_ingest_external`).
    /// Bias toward capture, disable structural.
    Import,
}

impl ContextHint {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::DashboardCommand => "dashboard_command",
            Self::Import => "import",
        }
    }
}

/// Input to [`wiki_ingest_message`].
#[derive(Debug, Clone)]
pub struct IngestRequest {
    /// Raw user message. Trimmed by [`IngestError::EmptyText`] if blank.
    pub text: String,
    /// Who authored `text` this turn. [`MessageRole::User`] is the default and
    /// the overwhelming common case — a message from the end user. The
    /// orchestrator flips to [`MessageRole::Assistant`] only when the consumer
    /// agent feeds back its OWN prior reply for extraction
    /// (agent-authored memory): then the classifier applies the agent-turn
    /// discriminator (prompt Part 9) and any captured fact is attributed with
    /// `sender = <the calling agent>` (resolved from [`Self::consumer_id`]) instead
    /// of the user — so the agent remembers the synthesis in its own reply (a
    /// deadline it derived, advice it gave) without it masquerading as a
    /// user-asserted fact. `sender_id` still names the user the agent was
    /// talking to (the subject candidate and the recall/ACL scope), so episodic
    /// and personalised facts still land in that user's wiki.
    pub author: MessageRole,
    /// Identifier of the user (NO `user:` prefix — that wire format
    /// is reconstructed where needed).
    pub sender_id: String,
    /// The calling consumer's deployment id (the `consumer_id` JWT
    /// claim), when present. Used to resolve the consumer's **own**
    /// memory wiki so a behaviour rule the user dictates to their agent
    /// lands there (carrying `sender=<user>`) instead of in the sender's
    /// fact memory. `None` for callers without a consumer id (e.g. the
    /// dashboard chat) — then behaviour-rule routing is skipped.
    pub consumer_id: Option<String>,
    /// Recent conversation, oldest first. The orchestrator truncates
    /// at policy bounds before prompting.
    pub recent_messages: Vec<RecentMessage>,
    /// Optional bias signal — see [`ContextHint`].
    pub context_hint: ContextHint,
    /// When the consumer is calling back after the user picked one of
    /// the previous turn's [`DisambigCandidate::candidate_id`] values,
    /// the chosen id rides here. Forwarded to the LLM as
    /// `disambig_choice: <id>` so the classifier commits to that
    /// candidate instead of re-asking.
    pub disambig_choice: Option<String>,
    /// Optional metadata propagated by the consumer along with the
    /// raw message. Today `locale` (explicit `LANGUAGE` directive) and
    /// `occurred_at` (the turn's semantic clock) are consumed; future
    /// signals (timezone, channel hints) can land here without further
    /// breaking changes.
    pub metadata: IngestMetadata,
    /// Media items riding this turn, already uploaded out of band via
    /// `POST /media` (media pipeline).
    /// The dispatcher resolves each entry against the media catalog
    /// (the row's `kind` is authoritative) and verifies the caller may
    /// read it before threading it here. Empty for the common
    /// text-only turn.
    pub attachments: Vec<IngestAttachment>,
}

impl IngestRequest {
    /// The turn's SEMANTIC clock: one instant every time-anchored judgment of
    /// this turn resolves against — the classifier's `current_time` anchor,
    /// the validity windows, the due-soon horizon, and the instant a supersede
    /// closes on when the replacement states no start of its own.
    ///
    /// `metadata.occurred_at` when the caller gave one, so a backlog replay
    /// re-lives the turn at utterance time; the wall clock otherwise, which is
    /// the same thing on a live turn. Operational timestamps
    /// (`created_at`, `superseded_at`) are a different question and keep the
    /// wall clock.
    #[must_use]
    pub fn turn_now(&self) -> chrono::DateTime<chrono::Utc> {
        self.metadata.occurred_at.unwrap_or_else(chrono::Utc::now)
    }
}

/// One media attachment riding an [`IngestRequest`].
#[derive(Debug, Clone)]
pub struct IngestAttachment {
    /// Catalog key minted at upload.
    pub catalog_id: CatalogId,
    /// Canonical kind from the catalog row (`photo` / `video` /
    /// `audio` / `doc`).
    pub kind: String,
    /// User caption for the media, when one rode the message.
    pub caption: Option<String>,
    /// Consumer-supplied description (a smart consumer's own vision, or
    /// a host-side recognizer). When present the server trusts it and
    /// the image bytes do NOT ride the classifier call.
    pub description: Option<String>,
}

/// Auxiliary signals the consumer can attach to an [`IngestRequest`].
///
/// All fields are optional and additive; the orchestrator never
/// rejects a request because a metadata field is missing.
#[derive(Debug, Clone, Default)]
pub struct IngestMetadata {
    /// BCP-47 locale (`it-IT`, `en-US`, ...) the consumer wants the
    /// reply rendered in. When unset, the orchestrator falls back to
    /// `enrollment::locale_for(sender_id)` and finally to the legacy
    /// "mirror the user's message" rule (`prompts/README.md`).
    pub locale: Option<String>,
    /// Instant the message was originally uttered, for backlog replays
    /// and imports. When set it becomes the turn's **semantic clock**:
    /// the classifier's `current_time:` anchor (so relative dates and
    /// validity windows resolve against the utterance time, not the
    /// server clock) and the due-soon window's `now`. It does **not**
    /// backdate operational timestamps (`created_at` stays wall-clock —
    /// the audit trail records when the engine saw the message).
    /// Defaults to the server's now.
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Project-wiki pages this turn authored, as plain `[[wiki_id/page]]`
    /// wikilinks (the form [`crate::capture::wiki_link`] emits and
    /// [`crate::recall::extract_wikilinks`] parses). A **smart**
    /// consumer that just wrote detail to its project wiki via
    /// `wiki_admin_push` carries the breadcrumbs that call returned
    /// (`PushResponse::authored_refs`) into this turn's ingest, so
    /// consolidation can record a **reference** to that page instead of
    /// re-storing the body — the "link, don't duplicate" provenance tube.
    /// Empty for a pure-standard turn. Downstream
    /// persistence + reference-not-body consolidation land in 17d.
    pub authored_refs: Vec<String>,
    /// Opaque surface label the consumer chose for this conversation
    /// (`telegram:123`, `salotto`, ...). Multi-channel consumers — one
    /// token, many chats — use it so the cross-consumer recent window
    /// can tag their surfaces apart and exclude only the
    /// requesting one from what it serves back. Unset → the consumer is
    /// treated as a single surface.
    pub channel: Option<String>,
}

// ---------- Public output types ----------

/// Coarse intent classification for one ingest turn.
///
/// Surfaced verbatim back to the consumer for audit / debug visibility
/// (tool reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentKind {
    /// The message carried a new fact — captured into a wiki.
    Capture,
    /// The message asked about existing memory — recall hits returned
    /// in `context_snippet`, no write.
    Recall,
    /// The user wants to modify the structure (forge a type, move a
    /// wiki, change scope). The orchestrator does not act; the
    /// `suggested_seed` nudges the agent toward `dashboard_link`.
    Structural,
    /// Nothing actionable. Greeting, ack, off-topic. No write.
    Skip,
}

impl IntentKind {
    /// Wire-shape token: `"capture" | "recall" | "structural" | "skip"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Recall => "recall",
            Self::Structural => "structural",
            Self::Skip => "skip",
        }
    }
}

/// One disambiguation candidate returned by the LLM when the message
/// is ambiguous and the agent should ask the user to choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisambigCandidate {
    /// Opaque id the agent echoes back in a follow-up call.
    pub candidate_id: String,
    /// Short human-readable description for the agent to show.
    pub description: String,
}

/// Output of [`wiki_ingest_message`]. Matches the JSON shape documented
/// in tool reference.
#[derive(Debug, Clone)]
pub struct IngestResponse {
    /// What the orchestrator decided. Always present.
    pub intent: IntentKind,
    /// Recall context formatted for the agent's system prompt — the
    /// **recall block** of recalled MEMORY (never directives), role-labelled
    /// sections in canonical order: `WHO YOU ARE` (the agent wiki's abstract
    /// and identity self-facts), `WHO IS SPEAKING` (the sender's one-line
    /// card), `YOUR RECENT HISTORY WITH THIS USER`, `RELEVANT MEMORY` (the
    /// deterministic flat hit-list, fresh slot included, deduplicated
    /// against the navigated pages), `NAVIGATED PAGES` (sender-projected
    /// prose the navigator funnel collected), and `UPCOMING` (facts whose
    /// validity window closes inside the operator horizon). An empty
    /// section is omitted; `None` when every section came up empty.
    /// Standing **directives** (behaviour rules) ride the dedicated
    /// [`rules`](Self::rules) field, not here.
    pub context_snippet: Option<String>,
    /// Standing **behaviour directives** the consumer agent must apply when
    /// composing its reply this turn — kept structurally separate from the
    /// recalled memory in [`context_snippet`](Self::context_snippet) so a
    /// binding rule is never indistinguishable from a remembered fact.
    /// Carries the served user's behaviour rules (how to
    /// converse / operate with them, recalled from the agent's own
    /// `@rules.md`) and, leading, any one-shot governance notice (e.g. an
    /// agent-wide change refused for a non-admin this turn). `None` when the
    /// turn surfaced no directive. The agent applies these as instructions,
    /// not as material to relay.
    pub rules: Option<String>,
    /// Natural-language seed the agent can refine into the final reply.
    /// `None` is legal — the agent decides what to say.
    pub suggested_seed: Option<String>,
    /// The user's live thread from their OTHER surfaces — the
    /// cross-consumer recent window. A self-labelled section
    /// (`RECENT EXCHANGES ON YOUR OTHER CHANNELS …`) the consumer injects
    /// verbatim, like [`rules`](Self::rules): entries carry their relative
    /// age and origin surface, oldest first, newest kept under the char
    /// budget. `None` when the buffer has nothing for this user, when the
    /// only exchanges are the requesting surface's own, or when the knobs
    /// disable the window.
    pub recent_window: Option<String>,
    /// `fact_id` of the newly captured row. Audit-only — the agent
    /// must not cross-link to it in chat (ingest pipeline).
    pub capture_id: Option<FactId>,
    /// True when the LLM flagged the message as ambiguous and the agent
    /// should ask the user to choose a candidate.
    pub needs_disambig: bool,
    /// Candidates the agent surfaces. Empty when `needs_disambig` is
    /// false.
    pub disambig_candidates: Vec<DisambigCandidate>,
    /// `true` when the classifier answered — even when its answer could
    /// not be read or applied and the turn fell back; `false` only when
    /// it could not be reached at all. A fallback turn is told apart by
    /// its `suggested_seed`, which says nothing was stored.
    pub llm_used: bool,
    /// Wall-clock duration of the orchestrator. Echoed as `took_ms` in
    /// the MCP response.
    pub took_ms: u64,
}

// ---------- Policy ----------

/// Knobs the operator (or a config layer) tunes without recompiling.
///
/// All defaults are sized for the target workload (handful of
/// users, low thousands of active regions). Defaults can be overridden
/// from `mwe-mcp.config.yaml` via the config layer; the orchestrator
/// itself takes the policy by reference so the call site is
/// dependency-injected.
#[derive(Debug, Clone)]
pub struct IngestPolicy {
    /// Top-K recall hits to fetch as LLM context.
    ///
    /// **Raised from 5 to 10 by the founder, 2026-08-05.** The bound exists
    /// to keep the prompt sane, not to be small: the turn that failed in
    /// production sits behind a five-fact block, so at 5 it cannot surface
    /// however well the ranking is tuned — see
    /// [`crate::recall::SUBJECT_COVERAGE_UPLIFT`], where the coverage weight
    /// moved it to 7th and the note recorded that the rest of the gap closes
    /// only here. The cost is prompt characters, not compute: the scan and
    /// the scoring read the whole readable corpus either way, and `top_k`
    /// only decides how much of the ranking survives into the block.
    pub recall_top_k: usize,
    /// How deep the **entry fan** may look down the same ranking for doors
    /// on distinct pages.
    ///
    /// The doors it keeps are still `recall_top_k` — this only decides how
    /// far past them it may go to find one on a page it has not already
    /// opened a door onto (founder, 2026-08-21; see
    /// [`recall_nav::hits_as_doors`]). Cheap: the scan and the scoring read
    /// the whole readable corpus either way, so a deeper `top_k` costs rows
    /// carried out of one query, not a second search. Clamped up to
    /// `recall_top_k` — a shallower value would starve the block.
    pub nav_seed_depth: usize,
    /// Size of the separate "fresh / unconsolidated" recall slot — how many
    /// un-promoted buffered captures the mid-range bridge surfaces per turn
    /// (see [`recall::recall_fresh_captures`]). `0` disables the slot. Small by
    /// design: each candidate is re-embedded at recall time. PROVISIONAL —
    /// revisit after the recall-strategy review.
    pub recall_fresh_top_k: usize,
    /// Size of the **project-docs** slot: how many smart-wiki sections a
    /// turn may pull when the message *names* a project the sender can
    /// read (see [`recall::recall_project_docs`]). `0` disables the
    /// slot. A conversational turn otherwise recalls facts only — this is
    /// the narrow, name-triggered exception for "how does `AcmeSigns` do X?".
    pub project_docs_top_k: usize,
    /// Character budget for that slot. Whole sections only — a section is
    /// never cut mid-text. Documentation sections are long, so this — not
    /// `project_docs_top_k` — is usually what bounds the slot.
    ///
    /// The overrun rule is a **stop, not a skip**: the first section that
    /// would exceed the remaining budget ends the slot, and the
    /// lower-ranked ones behind it do not get a turn even when one of them
    /// would have fit ([`crate::recall`], the section-ranking loop). That
    /// keeps the slot faithful to the ranking — a smaller section never
    /// overtakes a better one on size alone — at the price of leaving
    /// budget unspent. The one exception is the first hit, which is
    /// admitted whatever its size, because an empty slot is worse than one
    /// oversized answer.
    ///
    /// Sized against [`document::SECTION_MAX_CHARS`]: strictly greater, so
    /// that even a maximal section leaves room for a second hit rather
    /// than consuming the slot alone (the budget always admits the first
    /// hit, whatever its size — the alternative is an empty slot).
    pub project_docs_char_budget: usize,
    /// Similarity floor for the **signpost-triggered** half of that slot
    /// — the relevance gate that keeps a passing mention of a project
    /// from dragging its documentation into an unrelated turn. Does not
    /// apply when the message names the project outright. See
    /// [`recall::DEFAULT_SIGNPOST_FLOOR`].
    pub project_docs_signpost_floor: f32,
    /// Similarity a project's signpost description must reach before the
    /// merged whole-corpus view ([`crate::recall::search_all`]) may read
    /// that project's sections at all — the smart-corpus funnel. See
    /// [`crate::recall::DEFAULT_SMART_CORPUS_FLOOR`] for the measurement
    /// behind the default; naming the project bypasses it.
    pub smart_corpus_floor: f32,
    /// Similarity the turn's **best promoted flat hit** must clear before
    /// [`format_snippet`] renders any promoted hit in the `RELEVANT
    /// MEMORY` slot — a turn-level relevance floor, not a per-hit trim.
    /// See [`crate::recall::DEFAULT_RELEVANCE_FLOOR`] for the measurement
    /// and the reasoning behind the default and its turn-level shape.
    /// `0` disables it (same off-switch idiom as
    /// [`crate::recall::admitted_smart_wikis`]).
    pub relevance_floor: f32,
    /// Jaccard threshold passed through to [`capture::wiki_capture`]
    /// when routing intent `capture`.
    pub dedup_threshold: f32,
    /// Number of recent messages injected in the prompt — the classifier's
    /// sliding window ("keepTurns×2"). Older messages are dropped
    /// silently. The consumer owns the transcript and supplies the window via
    /// `IngestRequest.recent_messages`; this caps how much of it the prompt
    /// carries.
    pub max_recent_messages: usize,
    /// Per-message character cap before prompt injection (oldest
    /// trimmed first). Stops a runaway tail from blowing the prompt
    /// budget.
    pub max_recent_message_chars: usize,
    /// Cap on the `list_pages` inventory — the only placement surface the
    /// classifier is shown.
    ///
    /// It replaced the wiki window, and the two are not the same size of
    /// problem: a wiki is an address [`derive_target_wiki`] computes, while a
    /// list's file name is knowledge only the store holds. Lists are few by
    /// nature (a deployment has a shopping list, a watchlist, some errands),
    /// so this bounds a pathological corpus rather than an ordinary one.
    /// Beyond the cap the extras are dropped, and a turn adding to a dropped
    /// list proposes a fresh name instead — the cost of raising it is prompt
    /// weight on every turn. See [`fact_index::list_pages_readable_by`].
    pub max_list_pages_in_prompt: usize,
    /// Per-group character cap on the `scope` prose injected into the
    /// `sender_groups` section. Sized to fit a rich scope — including
    /// its exclusion clause ("NOT: personal facts …"), which is what
    /// teaches the classifier *not* to over-share — without letting a
    /// pathological scope blow the prompt budget.
    pub max_group_scope_chars: usize,
    /// Character cap on the sender's `@rules.md` policy injected into the
    /// prompt's `sender_rules` section. Bounds a pathological
    /// hand-edited policy from blowing the prompt budget; a normal policy
    /// is a short paragraph or two. Also bounds the `YOUR RULES` section
    /// of the response's `rules` field (whole-bullet fitting there).
    pub max_sender_rules_chars: usize,
    /// Character cap on the recall block's `WHO YOU ARE` section (the
    /// agent wiki's summary line + the agent's identity self-facts).
    /// A resource cap, not a semantic gate: whole bullets are fitted
    /// newest-first and the oldest tail falls off ([`fit_bullets`]).
    pub max_agent_identity_chars: usize,
    /// Character cap on the recall block's `YOUR RECENT HISTORY WITH THIS
    /// USER` section. Same whole-bullet fitting as
    /// [`Self::max_agent_identity_chars`].
    pub max_agent_history_chars: usize,
    /// Character cap on the recall block's `WHO IS SPEAKING` section — the
    /// sender's identity card, served deterministically from their
    /// `@profile.md`.
    ///
    /// A **failsafe, not a curation knob**: what belongs on the card and how
    /// dense it is are REM's judgement (69c, hard ceiling 2 500 characters),
    /// and this bound only stops a runaway page from swallowing the block.
    /// It fits **whole paragraphs** and warns when it fires, because a cut
    /// card silently drops whatever the author put last.
    pub max_sender_identity_chars: usize,
    /// How many **named third parties** get their identity card served into
    /// the block alongside the sender's. `0` disables the slot.
    ///
    /// Founder's ruling, 2026-08-04: *«la scheda degli utenti nominati va
    /// inserita deterministicamente nel recall, ci costa caratteri, ma ci dà
    /// una porta di ingresso con le cose più importanti di un utente che
    /// comunque vanno sempre prese in considerazione»*. The gate is
    /// deliberately the coarse one — the turn **names** them, or the search
    /// returned a fact **about** them — because the card holds what is worth
    /// knowing about a person whenever they come up at all.
    ///
    /// Bounded by count rather than by a second character budget: each card
    /// is already fitted to `max_sender_identity_chars`, which is 69c's
    /// authored ceiling, so a card within its bound is never cut. The count
    /// is what stops a turn about four people from spending the whole block
    /// on biographies. It is also the only route to a card: a card is not a
    /// link destination (`compiler::recommended_link_targets`), so no walk
    /// arrives at one.
    pub max_mentioned_cards: usize,
    /// Canned `suggested_seed` for a turn the classifier filed as
    /// nothing to keep, or a capture turn that filed nothing new. Short
    /// on purpose — the agent will rewrite it.
    pub fallback_suggested_seed: String,
    /// Canned `suggested_seed` for a turn the engine could not serve: the
    /// model was unreachable, or its answer could not be read or applied.
    /// Nothing was stored, and the seed must say so — "I've noted that."
    /// on such a turn is the one sentence the consumer must never relay.
    pub degraded_suggested_seed: String,
    /// Canned `suggested_seed` returned for `intent=structural` when
    /// the LLM did not supply its own seed.
    pub structural_suggested_seed: String,
    /// Resource knobs for the navigator funnel (the recall-block tail).
    /// Semantics live in the `navigator` prompt; these bound hops, pages
    /// per hop, the prose budget, and the candidate window. Inert when
    /// the call site passes no navigator backend.
    pub nav: recall_nav::NavigatorPolicy,
    /// Top-K facts in the recall block's `UPCOMING` (due-soon) slot. `0`
    /// disables the slot.
    pub due_soon_top_k: usize,
    /// Look-ahead horizon of the due-soon slot, in hours from the turn's
    /// clock. An operator setting (surfaced with the recall-settings
    /// panel).
    pub due_soon_horizon_hours: u32,
    /// IANA timezone name of the deployment's users (e.g. `Europe/Rome`), or
    /// `None` when unset. When set, [`build_prompt`] adds a `user_timezone:`
    /// line so the classifier resolves a bare wall-clock time the user speaks
    /// ("alle 16") in that zone and converts it to UTC for `valid_from` /
    /// `valid_to` — instead of stamping the local hour as UTC (a systematic
    /// +offset error). DST-aware conversion is left to the classifier; no tz
    /// database is compiled in. `None` keeps the pre-existing UTC-only anchor.
    pub ingest_timezone: Option<String>,
    /// Hard cap on buffered exchanges per user in the cross-consumer
    /// recent window. `0` disables the window entirely —
    /// nothing is buffered, nothing is served.
    pub recent_window_entries: usize,
    /// TTL of a buffered exchange, in hours. Short by design (43-P): the
    /// window serves the *thread of discourse*, not history — a thread is
    /// live on the scale of minutes to hours; older exchanges have either
    /// sedimented into facts through the ordinary ingest or expired with
    /// the conversation they belonged to.
    pub recent_window_ttl_hours: u32,
    /// Character budget of the rendered `recent_window` section. Newest
    /// entries win the budget; the section renders oldest-first. `0`
    /// disables serving (buffering still happens for other surfaces).
    pub recent_window_chars: usize,
    /// Retention of the recall-trace journal, in days — how far back the
    /// record of *how recall behaved* stays readable. `0` disables the
    /// prune. See [`crate::recall_trace::DEFAULT_TRACE_RETENTION_DAYS`] for
    /// why the journal is kept at all rather than capped at a handful of
    /// rows.
    pub trace_retention_days: i64,
}

impl Default for IngestPolicy {
    fn default() -> Self {
        Self {
            recall_top_k: 10,
            nav_seed_depth: 30,
            recall_fresh_top_k: 3,
            project_docs_top_k: 3,
            project_docs_char_budget: 3_000,
            project_docs_signpost_floor: recall::DEFAULT_SIGNPOST_FLOOR,
            smart_corpus_floor: recall::DEFAULT_SMART_CORPUS_FLOOR,
            relevance_floor: recall::DEFAULT_RELEVANCE_FLOOR,
            dedup_threshold: DEFAULT_DEDUP_THRESHOLD,
            max_recent_messages: 16,
            max_recent_message_chars: 280,
            max_list_pages_in_prompt: 32,
            max_group_scope_chars: 1_000,
            max_sender_rules_chars: 1_500,
            max_agent_identity_chars: 900,
            max_agent_history_chars: 1_400,
            // 69c's hard ceiling, so the read path's failsafe and the
            // ceiling REM curates toward are the same number: a card
            // within its authored bound is never cut here.
            max_sender_identity_chars: 2_500,
            // Two: a turn naming three or more people is rare, and two cards
            // at their authored target (~1 800) already cost more than the
            // navigated prose budget. Raise it when the traces say turns name
            // more people than that and the extra card earns its characters.
            max_mentioned_cards: 3,
            fallback_suggested_seed: "I've noted that.".to_owned(),
            degraded_suggested_seed: "Something went wrong on my side and I could not save that. \
                                      Please tell me again in a moment."
                .to_owned(),
            structural_suggested_seed:
                "This looks like a structural change — open the dashboard to continue.".to_owned(),
            nav: recall_nav::NavigatorPolicy::default(),
            due_soon_top_k: 3,
            due_soon_horizon_hours: 168, // 7 days
            ingest_timezone: None,
            recent_window_entries: 32,
            recent_window_ttl_hours: 4,
            recent_window_chars: 1_200,
            trace_retention_days: crate::recall_trace::DEFAULT_TRACE_RETENTION_DAYS,
        }
    }
}

// ---------- Errors ----------

/// Errors raised by the ingest orchestrator.
///
/// Only server-side failures surface here — agent-visible "soft"
/// failures (LLM down, plan invalid) are absorbed into the response as
/// `IntentKind::Skip` with the canned seed.
#[derive(Debug, Error)]
pub enum IngestError {
    /// `text` was empty or whitespace-only after trim.
    #[error("ingest: text is empty")]
    EmptyText,
    /// Recall failed with an infrastructure error (DB / embedder).
    /// Soft-recoverable recall failures degrade silently to an empty
    /// hit list; only the unrecoverable ones make it here.
    #[error("ingest recall: {0}")]
    Recall(#[from] RecallError),
    /// `wiki_capture` returned an infrastructure failure (DB, IO,
    /// filesystem). LLM-plan validation failures do NOT surface here.
    #[error("ingest capture: {0}")]
    Capture(#[from] CaptureError),
    /// Buffering a standard-wiki capture returned an infrastructure
    /// failure (DB, IO, journal). LLM-plan validation failures do NOT surface
    /// here — they demote to skip upstream.
    #[error("ingest capture buffer: {0}")]
    CaptureBuffer(#[from] CaptureBufferError),
    /// Walking the wiki tree to enumerate available wikis failed.
    #[error("ingest wiki tree: {0}")]
    Wiki(#[from] WikiError),
    /// The hybrid prompt loader failed — either the override file in
    /// `<workdir>/prompts/ingest.md` is unreadable / malformed, or
    /// (regression) the bundled default itself is malformed. The
    /// dispatcher renders this as a transport error rather than a
    /// soft-skip: the operator should see a hand-edit mistake
    /// immediately, not have it absorbed into a Skip turn.
    #[error("ingest prompt loader: {0}")]
    Prompt(#[from] prompts::PromptError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, IngestError>;

// ---------- Internal: LLM plan + capture-plan validation ----------

#[derive(Debug, Deserialize)]
// Four independent boolean flags mirror the LLM's JSON output
// (requested_container / engine_rule / behaviour_rule / needs_disambig) — each
// a distinct routing signal the model sets per turn, not a state to model as an
// enum.
#[allow(clippy::struct_excessive_bools)]
struct LlmIngestPlan {
    intent: String,
    #[serde(default)]
    suggested_seed: Option<String>,
    #[serde(default)]
    target_wiki_id: Option<String>,
    #[serde(default)]
    target_page: Option<String>,
    #[serde(default)]
    #[serde(alias = "owner_id")]
    subject_id: Option<String>,
    /// The name of what the fact is about when that is not a principal. See
    /// [`LlmExtraction::subject_external`].
    #[serde(default)]
    subject_external: Option<String>,
    #[serde(default)]
    allow_ids: Vec<String>,
    #[serde(default)]
    fact_type: Option<String>,
    /// Per-fact **validity interval**.
    /// `valid_from` = when the fact starts holding (the turn's `current_time`
    /// for a present fact); `valid_to` = when it stops, or `None` for an OPEN
    /// horizon ("true now, no known end"). The classifier resolves both
    /// against `current_time`. Parsed and traced here; threaded
    /// into [`fact_index::NewFact`]. Plan-level mirror
    /// for the legacy single-fact fallback; the per-fact values live on
    /// [`LlmExtraction`].
    #[serde(default)]
    valid_from: Option<String>,
    #[serde(default)]
    valid_to: Option<String>,
    /// The TARGET PAGE's writing
    /// **`style`** (`prosa` | `prosa-tecnica` | `lista`) and a one-line
    /// natural-language **`page_description`** of what the page holds. Per-PAGE
    /// render/recall hints, decided per extraction;
    /// written into the page frontmatter. Parsed and traced
    /// here. Plan-level mirror for the legacy single-fact fallback.
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    page_description: Option<String>,
    /// Per-fact **salience** (`high` | `normal` | `low`;
    /// absent = unspecified). `high` = "must be known in every interaction"
    /// (identity, health/safety, hard constraints) → routed to the actor-wiki
    /// `@profile.md` identity card. The classifier decides it; no hardcoded
    /// gate. Plan-level mirror for the legacy single-fact fallback; the per-fact
    /// value lives on [`LlmExtraction`].
    #[serde(default)]
    salience: Option<String>,
    /// The classifier flags an
    /// **explicitly requested container** (a list, a collection, a named note
    /// the user asked to keep). A requested container is created and written
    /// **live** at ingest via the direct path, bypassing the narrative buffer so
    /// it is there immediately (a shopping list cannot wait for the dream);
    /// accumulated knowledge — no explicit request — stays buffer→dream
    /// (the live exception). The classifier decides; there
    /// is no hard-coded gate. Plan-level mirror for the legacy single-fact
    /// fallback; the per-fact value lives on [`LlmExtraction`].
    #[serde(default)]
    requested_container: bool,
    /// The classifier flags an
    /// **engine-rule**: a standing *governance* directive for the memory engine
    /// (a privacy/sharing policy, or a do-not-store rule), not a fact about the
    /// world. An engine-rule is appended as prose to the sender's `@rules.md`
    /// (read back as `sender_rules`) instead of being filed in `fact_index` —
    /// it never becomes a fact. The classifier decides; no hard-coded gate. The
    /// world/household `rule` `fact_type` ("in casa non si fuma") is a normal
    /// fact and leaves this `false`. Plan-level mirror for the legacy
    /// single-fact fallback; the per-fact value lives on [`LlmExtraction`].
    #[serde(default)]
    engine_rule: bool,
    /// The classifier flags a
    /// **behaviour-rule**: a standing directive about how the CALLING AGENT
    /// should converse (tone, style, length, form of address, the
    /// language/name to use with this agent) — not a fact about the user, and
    /// not an engine governance rule. Filed in the consumer agent's OWN wiki
    /// (resolved from [`IngestRequest::consumer_id`]) attributed to the
    /// sender, never as a fact in the user's wiki. The classifier decides; no
    /// hard-coded gate. Plan-level mirror for the legacy single-fact fallback;
    /// the per-fact value lives on [`LlmExtraction`].
    #[serde(default)]
    behaviour_rule: bool,
    /// Behaviour-rule scope mirror for the legacy single-fact fallback; the
    /// per-fact value lives on [`LlmExtraction::behaviour_scope`].
    #[serde(default)]
    behaviour_scope: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    needs_disambig: bool,
    #[serde(default)]
    disambig_candidates: Vec<LlmDisambig>,
    /// Turn-level judgement: would this project's
    /// documentation help answer the turn? Set when the recall block
    /// surfaced a **project signpost** and the message is actually about
    /// what that project does — not merely near it (an invoice, an
    /// appointment, a delivery mention its docs say nothing about).
    ///
    /// A judgement rather than a threshold because it was measured as a
    /// threshold and no similarity signal separated the two cases; see
    /// [`recall::recall_signposted_project_docs`]. Defaults to `false`,
    /// so an older prompt (or a fallback plan) simply never digs.
    #[serde(default)]
    needs_project_docs: bool,
    /// `fact_id` the model wants to supersede (when the new message
    /// updates / contradicts a row already in `recalled_memory`). When
    /// set, the orchestrator routes the capture branch through
    /// [`capture::wiki_supersede`] instead of [`capture::wiki_capture`]
    /// so the old row is tombstoned and chained to the new one.
    #[serde(default)]
    supersede_target: Option<String>,
    /// Multi-fact extraction. When the model splits a turn into
    /// several atomic facts it returns them here, one [`LlmExtraction`] each,
    /// and the router files every one. When empty the orchestrator falls back
    /// to the legacy single-fact shape (the top-level `body` / `target_wiki_id`
    /// / … fields), so older prompts and the existing tests keep working.
    #[serde(default)]
    extractions: Vec<LlmExtraction>,
    /// The closure half of the turn — existing facts whose validity this
    /// message CLOSES: a completion ("I bought the milk" closes the open
    /// shopping item) or a relayed forget/abandon gesture ("forget what I
    /// told you about the greenhouse"). Independent from `extractions` — a pure
    /// gesture closes facts and captures nothing; the Jumanji case does both.
    /// Targets must come from this turn's `recalled_memory` (the same
    /// anti-hallucination rule as `supersede_target`).
    #[serde(default)]
    closures: Vec<LlmClosure>,
    /// Topics the turn's gesture closes but whose facts the classifier
    /// could NOT see in `recalled_memory` (the whole-message embedding
    /// can wash the gesture's topic out of the first recall window).
    /// Each topic triggers a focused second recall +
    /// [`confirm_topic_closures`] confirm call instead of letting the
    /// model aim a closure at a vaguely-related recalled fact.
    #[serde(default)]
    closure_topics: Vec<String>,
    /// The validity-edit half of the turn — existing facts whose dates this
    /// message CORRECTS ("the milk expires on the 20th, not the 25th", "the
    /// project started in March"). Distinct from `closures`: a correction fixes the
    /// interval, it is not a completion/retraction (and never touches
    /// `decay_reason`). Targets must come from this turn's `recalled_memory`,
    /// and only the fact's SUBJECT may edit it from chat.
    #[serde(default)]
    validity_edits: Vec<LlmValidityEdit>,
    /// The acl-change half of the turn — existing facts whose SHARING this
    /// message changes ("make this memory visible to everyone", "share it with
    /// the family group"). Targets must come from this turn's
    /// `recalled_memory`, and only the fact's SUBJECT may change it from chat.
    #[serde(default)]
    acl_changes: Vec<LlmAclChange>,
    /// The turn's message with what the speaker left implicit written in —
    /// «i suoi reni sono peggiorati» → «i reni di bob sono peggiorati».
    ///
    /// Everything that has to know what the message MEANS runs after this call
    /// and would otherwise be handed the message alone: the search, which can look for no word
    /// the turn does not contain, and the two stages that judge stored facts
    /// against it, for which *«l'ho comprato»* matches nothing. The classifier
    /// is the first thing in the turn with the conversation in front of it, so
    /// it is the first thing that can say what the turn is really about — and
    /// saying it costs nothing, the sentence riding the JSON it was already
    /// returning.
    ///
    /// `None` on the turns that need nothing written in, which is most of
    /// them.
    #[serde(default)]
    completed_message: Option<String>,
    /// The classifier's vote on how well each recalled fact answers THIS
    /// turn, one multiplier per fact it cares to judge.
    ///
    /// It is the only thing in the read path that has read the question and
    /// the facts and understood both: cosine measures a distance, and a
    /// distance does not know that «likes Metallica» answers «what music do I
    /// like» while «born on 12 March» does not. Optional in every direction —
    /// an absent array, an unparseable one, or one naming facts that were
    /// never shown all leave the order exactly as recall left it.
    #[serde(default)]
    fact_scores: Vec<LlmFactScore>,
}

/// One vote on a recalled fact (see
/// [`LlmIngestPlan::fact_scores`]).
#[derive(Debug, Clone, Deserialize)]
struct LlmFactScore {
    /// `fact_id` from `recalled_memory`.
    #[serde(default)]
    #[serde(alias = "fact_id")]
    target: Option<String>,
    /// How much better or worse this fact answers the turn than its distance
    /// suggests. Clamped server-side to [`FACT_SCORE_MIN`]..=[`FACT_SCORE_MAX`].
    #[serde(default)]
    #[serde(alias = "score")]
    multiplier: Option<f32>,
}

/// The two words a fact carries about what it is ABOUT: the macrotopic, then
/// the microtopic.
///
/// Two, because the pair exists to be **counted**: a word groups facts only
/// when it comes back, and a word coined for one fact and never used again
/// says nothing about anything. Which of the two is a macrotopic is decided
/// by [`crate::topic_rank`] from the counts, never here and never by the
/// classifier: the pair is two words, not a level and its child.
pub const MAX_FACT_TOPICS: usize = 2;

/// Prefixes the ENGINE writes into the same column for its own bookkeeping.
///
/// They are not topics and never were: the project signposts needed a way to
/// find their own rows and reused a column that already existed
/// (`crate::signposts`). A classifier that could write one would forge a
/// signpost, and — the reason this list will outlive that one — a ranking
/// that counted them would rank `signpost-day:2026-07-14`, a fresh word every
/// day, forever.
const ENGINE_TOPIC_PREFIXES: &[&str] = &["signpost-", "project-signpost"];

/// Normalises what the classifier wrote into the pair, dropping the rest.
fn normalize_fact_topics(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(MAX_FACT_TOPICS);
    for word in raw {
        let word = word.trim().to_lowercase();
        if word.is_empty()
            || ENGINE_TOPIC_PREFIXES.iter().any(|p| word.starts_with(p))
            || out.contains(&word)
        {
            continue;
        }
        out.push(word);
        if out.len() == MAX_FACT_TOPICS {
            break;
        }
    }
    out
}

/// One requested validity closure of an existing fact (see
/// [`LlmIngestPlan::closures`]).
#[derive(Debug, Clone, Deserialize)]
struct LlmClosure {
    /// `fact_id` from `recalled_memory`.
    #[serde(default)]
    target: Option<String>,
    /// Why the window closes: `completed` | `retracted` | `contradicted`
    /// (mapped onto [`fact_index::decay`]).
    #[serde(default)]
    reason: Option<String>,
    /// When the fact stopped holding (RFC3339 UTC instant, resolved
    /// against `current_time`); absent/empty = the turn's own instant.
    #[serde(default)]
    valid_to: Option<String>,
}

/// One requested validity-date *correction* of an existing fact (see
/// [`LlmIngestPlan::validity_edits`]). The twin of [`LlmClosure`], but for a
/// correction of the dates rather than a completion/retraction — it never
/// stamps `decay_reason`.
#[derive(Debug, Clone, Deserialize)]
struct LlmValidityEdit {
    /// `fact_id` from `recalled_memory`.
    #[serde(default)]
    target: Option<String>,
    /// Corrected `valid_from` (RFC3339 UTC instant); absent/null = leave
    /// unchanged.
    #[serde(default)]
    valid_from: Option<String>,
    /// Corrected `valid_to` (RFC3339 UTC instant); absent/null = leave
    /// unchanged.
    #[serde(default)]
    valid_to: Option<String>,
}

/// One requested ACL *change* of an existing fact (see
/// [`LlmIngestPlan::acl_changes`]). The subject broadens or narrows who can
/// read their OWN fact; the LLM resolves the natural-language scope into the
/// `subject_id` + `allow_ids` principals.
#[derive(Debug, Clone, Deserialize)]
struct LlmAclChange {
    /// `fact_id` from `recalled_memory`.
    #[serde(default)]
    target: Option<String>,
    /// New subject principal wire string; absent/null = keep the existing
    /// subject.
    #[serde(default)]
    #[serde(alias = "owner_id")]
    subject_id: Option<String>,
    /// New allow-list principal wire strings (replaces the old list).
    #[serde(default)]
    allow_ids: Vec<String>,
}

/// One atomic fact in a multi-fact `capture` plan. Mirrors the per-fact
/// subset of [`LlmIngestPlan`]; the turn-level fields (`intent`,
/// `suggested_seed`, disambiguation) stay on the plan.
#[derive(Debug, Deserialize)]
struct LlmExtraction {
    #[serde(default)]
    target_wiki_id: Option<String>,
    /// The name of what this fact is about when that is not a principal —
    /// a person who does not use the product, an animal, a place, a thing.
    /// Absent for the ordinary fact, which is about its `subject_id`.
    #[serde(default)]
    subject_external: Option<String>,
    #[serde(default)]
    target_page: Option<String>,
    #[serde(default)]
    #[serde(alias = "owner_id")]
    subject_id: Option<String>,
    #[serde(default)]
    allow_ids: Vec<String>,
    #[serde(default)]
    fact_type: Option<String>,
    /// Per-fact validity interval (`valid_from`/`valid_to`, RFC3339 UTC
    /// instants; `valid_to = None` = open horizon).
    /// See [`LlmIngestPlan::valid_from`].
    #[serde(default)]
    valid_from: Option<String>,
    #[serde(default)]
    valid_to: Option<String>,
    /// Per-page `style` +
    /// `page_description`. See [`LlmIngestPlan::style`].
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    page_description: Option<String>,
    /// Per-fact salience (`high` | `normal` | `low`).
    /// See [`LlmIngestPlan::salience`].
    #[serde(default)]
    salience: Option<String>,
    /// Requested-container flag. See
    /// [`LlmIngestPlan::requested_container`].
    #[serde(default)]
    requested_container: bool,
    /// Engine-rule flag. See
    /// [`LlmIngestPlan::engine_rule`].
    #[serde(default)]
    engine_rule: bool,
    /// Behaviour-rule flag. See
    /// [`LlmIngestPlan::behaviour_rule`].
    #[serde(default)]
    behaviour_rule: bool,
    /// Behaviour-rule scope (only read when `behaviour_rule` is `true`),
    /// read from the grammatical addressee.
    /// `"per-user"` (or absent) = addressed to the speaker ("with me / my
    /// things", or a bare imperative) — open to any user, filed per-user.
    /// `"agent-wide"` = impersonal / universal ("with everyone", or a how-the-agent-
    /// works directive with no per-speaker scope) — admin-only, filed
    /// subject = agent. `"user-global"` = explicitly every-assistant ("tutti gli
    /// assistenti") — open to any user, filed in THEIR identity wiki. Default
    /// on omission: per-user (the open side).
    /// See [`CaptureUnit::behaviour_scope`] and the dispatch in [`run`].
    #[serde(default)]
    behaviour_scope: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    supersede_target: Option<String>,
    /// Catalog ids (from this turn's `attachments:` window) whose media
    /// this extraction describes. The orchestrator validates each id
    /// against the request's attachment list (anti-hallucination) and
    /// appends the `{{embed=…}}` markers itself — the model never
    /// writes marker syntax.
    #[serde(default)]
    attachments: Vec<String>,
}

/// A single fact to file this turn — a borrowed, source-agnostic view over
/// either one [`LlmExtraction`] or the legacy top-level single-fact fields.
/// Both [`validate_capture_plan`] and [`validate_supersede_target`] operate on
/// this so the router can loop uniformly over one or many facts. `Copy` (all
/// fields are borrows) so the filing loop can detach a local view and clear a
/// field the enrollment guard rejects without touching the plan.
#[derive(Clone, Copy)]
struct CaptureUnit<'a> {
    target_wiki_id: Option<&'a str>,
    target_page: Option<&'a str>,
    subject_id: Option<&'a str>,
    /// Borrowed view of the external subject — the NAME of what the fact is
    /// about when that is not a principal (see
    /// [`crate::fact_index::FactIndexRow::subject_external`]). Independent of
    /// `subject_id`, which keeps answering for the fact.
    subject_external: Option<&'a str>,
    allow_ids: &'a [String],
    fact_type: Option<&'a str>,
    /// Borrowed view of the per-fact
    /// validity interval (see [`LlmIngestPlan::valid_from`]). Threaded
    /// into [`fact_index::NewFact`]; here they are only traced.
    valid_from: Option<&'a str>,
    valid_to: Option<&'a str>,
    /// Borrowed view of the per-page
    /// `style` + `page_description` (see [`LlmIngestPlan::style`]).
    style: Option<&'a str>,
    page_description: Option<&'a str>,
    /// Borrowed view of the per-fact salience (see
    /// [`LlmIngestPlan::salience`]). Threaded into the capture row.
    salience: Option<&'a str>,
    /// Requested-container routing flag
    /// (see [`LlmIngestPlan::requested_container`]). `true` → the fact is written
    /// live even into a standard wiki, bypassing the buffer.
    requested_container: bool,
    /// Engine-rule routing flag (see
    /// [`LlmIngestPlan::engine_rule`]). `true` → the body is appended to the
    /// sender's `@rules.md` as prose, never filed as a fact.
    engine_rule: bool,
    /// Behaviour-rule routing flag (see
    /// [`LlmIngestPlan::behaviour_rule`]). `true` → the body is filed on the
    /// scope's home rules page (the calling agent's wiki, or the sender's
    /// identity wiki for user-global), never as a fact about the user.
    behaviour_rule: bool,
    /// Behaviour-rule scope discriminator (only read when `behaviour_rule`),
    /// read from the addressee.
    /// `Some("per-user")` / `None` → addressed to the speaker → any user may
    /// set it, filed subject = user in the agent's wiki.
    /// `Some("agent-wide")` → impersonal / universal → admin-only, filed
    /// subject = agent. `Some("user-global")` → explicitly every-assistant →
    /// any user, filed subject = user in THEIR identity wiki. The engine, not
    /// the model, enforces authority (the model never sees who is admin).
    behaviour_scope: Option<&'a str>,
    topics: &'a [String],
    body: Option<&'a str>,
    supersede_target: Option<&'a str>,
    /// Catalog ids this fact claims from the turn's attachment window
    /// (see [`LlmExtraction::attachments`]). Empty on the legacy
    /// single-fact shape — unclaimed attachments are filed by the
    /// deterministic fallback.
    attachments: &'a [String],
}

impl LlmIngestPlan {
    /// The facts to file for a `capture` intent. Prefers the multi-fact
    /// `extractions` array; otherwise synthesises a single unit from the
    /// legacy top-level fields when the model emitted a `body` or a target.
    /// Returns empty when there is nothing to capture (the caller demotes the
    /// turn to a skip).
    fn capture_units(&self) -> Vec<CaptureUnit<'_>> {
        if !self.extractions.is_empty() {
            return self
                .extractions
                .iter()
                .map(|e| CaptureUnit {
                    target_wiki_id: e.target_wiki_id.as_deref(),
                    target_page: e.target_page.as_deref(),
                    subject_id: e.subject_id.as_deref(),
                    subject_external: e.subject_external.as_deref(),
                    allow_ids: &e.allow_ids,
                    fact_type: e.fact_type.as_deref(),
                    valid_from: e.valid_from.as_deref(),
                    valid_to: e.valid_to.as_deref(),
                    style: e.style.as_deref(),
                    page_description: e.page_description.as_deref(),
                    salience: e.salience.as_deref(),
                    requested_container: e.requested_container,
                    engine_rule: e.engine_rule,
                    behaviour_rule: e.behaviour_rule,
                    behaviour_scope: e.behaviour_scope.as_deref(),
                    topics: &e.topics,
                    body: e.body.as_deref(),
                    supersede_target: e.supersede_target.as_deref(),
                    attachments: &e.attachments,
                })
                .collect();
        }
        // Legacy single-fact shape: synthesise one unit from the top-level
        // fields when the model emitted a `body` (the prompt always emits
        // `extractions`; this keeps older plans working).
        //
        // **The `body` is the trigger, and only the body.** A stray
        // `target_wiki_id` must not arm this arm on its own: the unit it would
        // synthesise has no body, and `validate_capture_plan` then resolves
        // that to `request.text` under `allow_message_fallback`. A gesture turn
        // whose `extractions` are empty BY DESIGN — *«forget what I told you
        // about the greenhouse»* — would file one fact whose body is that
        // sentence, purely because the cheap model also echoed a key the
        // prompt forbids. A destination with nothing to put at it is not a
        // capture.
        if self.body.is_some() {
            return vec![CaptureUnit {
                target_wiki_id: self.target_wiki_id.as_deref(),
                target_page: self.target_page.as_deref(),
                subject_id: self.subject_id.as_deref(),
                subject_external: self.subject_external.as_deref(),
                allow_ids: &self.allow_ids,
                fact_type: self.fact_type.as_deref(),
                valid_from: self.valid_from.as_deref(),
                valid_to: self.valid_to.as_deref(),
                style: self.style.as_deref(),
                page_description: self.page_description.as_deref(),
                salience: self.salience.as_deref(),
                requested_container: self.requested_container,
                engine_rule: self.engine_rule,
                behaviour_rule: self.behaviour_rule,
                behaviour_scope: self.behaviour_scope.as_deref(),
                topics: &self.topics,
                body: self.body.as_deref(),
                supersede_target: self.supersede_target.as_deref(),
                attachments: &[],
            }];
        }
        Vec::new()
    }
}

#[derive(Debug, Deserialize)]
struct LlmDisambig {
    #[serde(default)]
    candidate_id: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Error)]
enum CapturePlanError {
    /// No wiki could be resolved for the capture. Since the classifier
    /// stopped choosing one, this is not a model failure but a deployment
    /// one: [`derive_target_wiki`] exhausted every arm, which means the
    /// sender themself has no writable wiki — an enrollment gap, or a tree
    /// where every candidate is smart-managed. The extraction is dropped and
    /// the turn continues.
    #[error("no writable wiki for this capture (subject nor sender has one)")]
    MissingTargetWiki,
    /// A multi-fact extraction carried no `body`. The legacy single-fact shape
    /// falls back to the raw message; an extraction must supply its own text or
    /// it would duplicate the whole message under each fact.
    #[error("extraction has no body")]
    MissingBody,
    #[error("invalid wiki_id: {0}")]
    BadWikiId(#[from] WikiIdParseError),
    #[error("invalid principal: {0}")]
    BadPrincipal(#[from] PrincipalParseError),
    /// A non-`self` fact (owned by a user or group) named an AGENT's own wiki
    /// as its `target_wiki_id`. The agent wiki is reserved for the agent's
    /// `subject_id:"self"` autobiography; a user/group fact there
    /// fragments that principal's memory across two wikis. When the
    /// subject's own wiki is in the window the plan is
    /// redirected there; when it is not, the extraction is dropped with this
    /// error rather than misfiled.
    #[error(
        "target_wiki_id `{target}` is an agent wiki; a {subject}-owned fact cannot be filed there and no subject home wiki was in the window"
    )]
    TargetIsAgentWiki { target: String, subject: String },
    /// `supersede_target` carried a string that is not a well-formed
    /// `FactId`. Same demote-to-skip treatment as
    /// [`Self::MissingTargetWiki`].
    #[error("invalid supersede_target fact_id: {0}")]
    BadSupersedeFactId(#[from] FactIdParseError),
    /// `supersede_target` named a `fact_id` that does not appear in the
    /// `recalled_memory` window of the current turn. Anti-hallucination
    /// guard parallel to [`Self::MissingTargetWiki`]: if the model
    /// emits an id it never saw in context, the capture would crash
    /// inside [`capture::wiki_supersede`] with `PreviousFactNotFound`;
    /// we'd rather demote the whole turn to a skip with a warn log.
    #[error("supersede_target `{id}` is not in recalled_memory ({available})")]
    SupersedeTargetNotInRecall { id: String, available: String },
    /// `supersede_target` named a recalled fact OWNED by a different
    /// principal than the new capture's subject. Superseding means
    /// "replace a prior statement about the SAME subject"; closing
    /// another principal's fact from this turn's ingest would let one
    /// user silently rewrite another's memory — the cross-user supersede
    /// leak. The subject axis is the subject (see
    /// [`crate::types::Principal`]); a public fact carries its subject in
    /// `subject` and `global` only in `allow`, so two users' public facts
    /// never collapse to the same subject. Demote-to-skip like the other
    /// supersede guards.
    #[error(
        "supersede_target `{id}` is owned by {target_subject}, not the new fact's subject {new_subject}"
    )]
    SupersedeCrossSubject {
        id: String,
        target_subject: String,
        new_subject: String,
    },
    /// `supersede_target` named a fact whose subject the sender is not
    /// — neither the named user nor a member of the named group. The
    /// same-subject guard above compares the target with the **new**
    /// fact's subject, and that subject is the model's choice: a slip
    /// that copies the target's subject onto the new fact would pass it.
    /// This one asks the question the reconciliation stage asks of every
    /// supersede (`acl::sender_is_subject`): a supersede replaces what
    /// somebody's fact says, and only that somebody may replace it.
    #[error("supersede_target `{id}` is about {target_subject}, which the sender {sender} is not")]
    SupersedeNotOwned {
        id: String,
        target_subject: String,
        sender: String,
    },
}

/// Turn the LLM-proposed `target_page` into a safe `.md` page path.
///
/// `target_page` is untrusted model output: per the project's
/// robustness stance (normalise in code, never trust the model)
/// we cannot pass it to [`capture::wiki_capture`] verbatim. Three
/// failure modes observed in the wild, all fixed here:
///
/// 1. the model omits the extension (`"lista_spesa"`) — without a
///    trailing `.md` the capture writes an extension-less file that the
///    page walk skips (it filters on the extension) and `wiki_read`
///    (index-only) never surfaces;
/// 2. the model emits a name with characters outside the safe charset
///    (`"lista spesa"`, `"attività"`) — [`is_safe_page_path`] rejects it
///    and the capture errors out to the consumer with an opaque
///    internal error;
/// 3. the model spells the *same* topic differently across turns
///    (`"lista-spesa"` vs `"lista_spesa"`, `"Argo"` vs `"argo"`) — all
///    spellings pass [`is_safe_page_path`], so one topic fragments into
///    duplicate near-identical pages.
///
/// Policy: canonicalise through
/// [`crate::planner::canonical_page_path`] (one canonical spelling per
/// segment: lowercase, non-alphanumeric runs → `_`), then require the
/// result to pass [`is_safe_page_path`].
///
/// `None` means **nobody named a usable page** — absent, or a name that
/// slugifies to nothing, or traversal. That is an answer, not a failure: the
/// claim waits in the capture buffer and a placement pass decides where it
/// goes, so a normal message can never crash ingest and can never invent a
/// page out of noise either.
///
/// The classifier prompt shows neither wikis nor page names — the one
/// exception being the `list_pages` inventory, whose entries are exact names
/// to be copied — so canonicalising here is fighting a name the model
/// **coined**, never one it read off disk; a hand-authored hyphenated page
/// stays readable (`is_safe_page_path` still admits `-`) but ingest will not
/// target it.
pub(crate) fn normalize_capture_page(raw: Option<&str>) -> Option<PathBuf> {
    let canon = raw.and_then(crate::planner::canonical_page_path)?;
    let candidate = PathBuf::from(canon);
    is_safe_page_path(&candidate).then_some(candidate)
}

/// True when `wiki_id` is the identity wiki of `subject` itself.
///
/// An identity wiki is a root whose id **is** its principal's id
/// (`create_identity_wiki`), so the string compare is the whole test — no tree
/// lookup, and it holds for a user and a group alike. Used by the agent-wiki
/// guard to tell "someone else's fact parked in the agent's space" (what the
/// guard exists to stop) from "the agent's own fact, at home" (what the wiki is
/// for).
fn subject_is_the_wikis_own_principal(subject: &Principal, wiki_id: &str) -> bool {
    match subject {
        Principal::User(id) | Principal::Group(id) => id == wiki_id,
    }
}

/// Resolve the wiki a capture is filed into, in four descending preferences.
///
/// The classifier is not shown the wiki tree — it cannot choose from a list it
/// cannot see, and the list would cost a full per-turn prompt block for one
/// decision out of the twenty-odd it makes. So the destination is derived from
/// things the turn already decided:
///
/// 1. **An explicit `target_wiki_id`**, when something emits one. The bundled
///    prompt does not; an operator override on
///    `<workdir>/prompts/ingest.md` may, and honouring it costs one lookup
///    and keeps those deployments working. Still gated on `available`, so an
///    invented or smart-managed id does not slip through.
/// 2. **The list page the turn is adding to.** "Add detergent to the shopping
///    list" names a page from the inventory, and that page's own wiki is the
///    answer — this is the one route by which a capture still reaches a topic
///    wiki from a turn, and it is exactly the case where the user said so.
///    **Matched on the name AND the subject's wiki**, falling back to the name
///    alone: two wikis may both hold a `spesa.md`, and the file name does not
///    say which one the turn meant while the resolved `subject_id` does.
/// 3. **The subject's own wiki.** An identity wiki's id IS its principal's id,
///    so a fact about `user:marco` belongs in `marco` and a fact the family
///    owns belongs in `family`. This is the ordinary path.
/// 4. **The sender's own wiki** — for a `global`-owned fact (no wiki carries
///    that principal) and for a subject with no wiki of their own.
///
/// `None` only when even the sender has no wiki in `available`, which the
/// caller reports as a missing destination and drops the extraction.
fn derive_target_wiki(
    unit: &CaptureUnit<'_>,
    request: &IngestRequest,
    subject: &Principal,
    honoured_page: Option<&str>,
    available: &[AvailableWiki],
    list_pages: &[fact_index::ListPage],
) -> Option<String> {
    // `available` is the full tree minus the smart wikis, so every arm below
    // inherits the smart-wiki exclusion for free.
    let known = |id: &str| {
        available
            .iter()
            .any(|w| w.wiki_id == id)
            .then(|| id.to_owned())
    };
    if let Some(explicit) = unit.target_wiki_id.and_then(known) {
        return Some(explicit);
    }
    let home = match subject {
        Principal::User(id) | Principal::Group(id) => id.as_str(),
    };
    if let Some(name) = honoured_page
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        // A file name is not an address. The inventory spans every wiki the
        // sender may read, so `spesa.md` can name Alice's shopping list AND
        // the family's — and matching on the name alone handed the turn
        // whichever one the inventory happens to list first. The SUBJECT the
        // classifier already resolved is the tiebreak: *«aggiungi il detersivo
        // alla lista della spesa di famiglia»* arrives with
        // `subject_id: group:famiglia`, and without it the fact goes to
        // `alice` because `a` sorts before `f`.
        && let Some(hit) = list_pages
            .iter()
            .find(|l| l.page == name && l.wiki_id == home)
            .or_else(|| list_pages.iter().find(|l| l.page == name))
        && let Some(id) = known(&hit.wiki_id)
    {
        return Some(id);
    }
    known(home).or_else(|| known(&request.sender_id))
}

/// Why a list item could not be filed — the two ways a list loses its page.
///
/// Both end the same way for the user: nothing was written, and they are told
/// so this turn. Kept apart only so the notice can say which happened, because
/// one is theirs to fix (free up a list) and the other is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListRefusal {
    /// The classifier named one of the reserved pages. A model slip: the
    /// prompt forbids it and the engine enforces it rather than trusting it.
    ReservedName,
    /// The wiki already holds [`MAX_LIST_PAGES_PER_WIKI`] lists and this item
    /// would mint one more.
    WikiAtListCap,
}

impl ListRefusal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReservedName => "reserved_page_name",
            Self::WikiAtListCap => "wiki_at_list_cap",
        }
    }

    /// The one-shot notice the turn carries in [`IngestResponse::rules`].
    ///
    /// Behaviour guidance, not memory: the agent applies it when composing the
    /// reply. It must say the item was NOT saved — a list the user believes is
    /// complete and is not is the failure worth preventing here.
    fn notice(self) -> String {
        let why = match self {
            Self::ReservedName => {
                "the page name proposed for it is reserved by the engine, so the list could not be created"
            },
            Self::WikiAtListCap => {
                "that memory already holds the maximum number of lists, so no new one could be created"
            },
        };
        format!(
            "NOTE — the user asked to put something on a list and it was NOT saved: {why}.              Tell them plainly that this item was not remembered and why, so they can retry              on an existing list. Do NOT say it was noted or remembered."
        )
    }
}

/// Is this extraction list-shaped material? The one place the `lista` style is
/// read off the classifier's output, so the live/wait/refuse decisions cannot
/// disagree about what a list is.
fn is_list_shaped(unit: &CaptureUnit<'_>) -> bool {
    unit.style
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("lista"))
}

/// How many `lista` pages one wiki may hold (founder, 2026-08-09).
///
/// A **product** limit — «se non ci entrano più di 24 persone non deve far
/// creare l'utente, stessa cosa per gruppo e liste» — counted **per wiki**,
/// so one person's shopping lists never consume another's allowance. Distinct
/// in kind from [`IngestPolicy::max_list_pages_in_prompt`], which is a
/// scalability cap on the inventory *shown* to the classifier: cutting that
/// list hides a list, and a hidden list is one the turn mints a second copy
/// of, live, in front of the user.
pub const MAX_LIST_PAGES_PER_WIKI: usize = 32;

/// Send a capture that would mint the wiki's 33rd list to the buffer instead.
///
/// **Growth only, and never a lost fact.** An existing list stays addable-to
/// however many the wiki holds — the check fires only for a name the wiki
/// does not already have. And the refusal downgrades the *destination*, not
/// the capture: with its page taken away the claim is one nobody has placed,
/// so it waits in the buffer with the rest and the placement pass settles it
/// like any other unplaced prose (the router reads the page for exactly that
/// — see `route_to_buffer`). Refusing the whole extraction would throw away
/// what the person said in order to enforce a filing limit.
///
/// Soft on error: a count that cannot be taken lets the capture through
/// unchanged — a limit is not worth dropping a turn's work over.
async fn refuse_new_list_over_cap(
    pool: &SqlitePool,
    cap_req: &mut CaptureRequest,
    unit: &CaptureUnit<'_>,
    list_pages: &[fact_index::ListPage],
) -> std::result::Result<(), fact_index::FactIndexError> {
    if !is_list_shaped(unit) {
        return Ok(());
    }
    let wiki_id = cap_req.wiki_id.as_str().to_owned();
    let Some(page) = cap_req
        .page
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_owned)
    else {
        return Ok(());
    };
    // Already a list here — adding to it is never growth.
    if list_pages
        .iter()
        .any(|l| l.wiki_id == wiki_id && l.page == page)
    {
        return Ok(());
    }
    let held = fact_index::count_list_pages_in_wiki(pool, &wiki_id).await?;
    if held < MAX_LIST_PAGES_PER_WIKI {
        return Ok(());
    }
    tracing::warn!(
        wiki_id,
        page,
        held,
        cap = MAX_LIST_PAGES_PER_WIKI,
        "ingest: wiki is at its list limit — the new list is not minted, the fact goes to the buffer"
    );
    cap_req.page = None;
    Ok(())
}

fn validate_capture_plan(
    unit: &CaptureUnit<'_>,
    request: &IngestRequest,
    policy: &IngestPolicy,
    available: &[AvailableWiki],
    list_pages: &[fact_index::ListPage],
    allow_message_fallback: bool,
) -> std::result::Result<CaptureRequest, CapturePlanError> {
    // Subject first: since the classifier stopped choosing a wiki, the subject
    // is an INPUT to the destination rather than a sibling decision.
    let subject = match unit.subject_id {
        Some(s) => Principal::from_str(s)?,
        None => Principal::User(request.sender_id.clone()),
    };
    // A page name is honoured exactly when THE WRITE CANNOT WAIT.
    //
    // **The classifier does not choose where a fact goes** (founder,
    // 2026-08-06; restated 2026-08-20 closing the contradiction with his own
    // ruling of 2026-08-03, which gave that job to a per-wiki map that no
    // longer exists). Placement belongs to the hourly round, which reads the
    // whole memory before deciding. The exceptions below are the WHOLE list:
    // two independent switches on the plan, plus standing rules, which never
    // reach this router because they never become facts (`engine_rule` →
    // the sender's `@rules.md`, `behaviour_rule` → the calling agent's own
    // wiki).
    //
    // - **List-shaped material.** A list is a *set*: it is right or it is
    //   wrong, and half a shopping list is a wrong answer rather than a
    //   partial one — so it must land on the page it belongs to, by its exact
    //   name, which is what the `list_pages` inventory exists to supply.
    // - **A requested container** (the user asked *now* for a list, a
    //   collection, a named note). That write bypasses the buffer and reaches
    //   disk inside the turn, so the page has to exist inside the turn too.
    //   Deliberately independent of `style`: a note someone asks you to keep
    //   can be prose, and it still needs the name they gave it.
    //
    // Everything else carries NO page, and that is what makes the name
    // unnecessary: no single page is the right answer at capture time, the
    // classifier is shown no prose pages to check against, and any name it
    // proposes is a guess the consolidation would have to undo. Those claims
    // wait in the capture buffer, and a placement pass settles them later.
    //
    // Enforced here rather than asked for in the prompt: a guessed page name
    // that reaches disk is a page, and pages outlive the turn that minted them.
    let names_its_page = is_list_shaped(unit) || unit.requested_container;
    let page = names_its_page
        .then(|| normalize_capture_page(unit.target_page))
        .flatten()
        // The guarantee the prompt makes — «a capture aimed at a reserved page
        // is not filed there» — enforced, not asked for. A model that names
        // `@rules.md` ends up with no page, which the router reads as "this
        // claim knows no page": it waits, and the placement pass settles it
        // like any other unplaced prose.
        .filter(|coined| {
            let ok = !wiki::names_reserved_page(coined);
            if !ok {
                tracing::warn!(
                    page = %coined.display(),
                    "ingest: capture named a reserved page — left for the queue to place"
                );
            }
            ok
        });
    let target_wiki_str = derive_target_wiki(
        unit,
        request,
        &subject,
        names_its_page.then_some(unit.target_page).flatten(),
        available,
        list_pages,
    )
    .ok_or(CapturePlanError::MissingTargetWiki)?;
    let target_wiki_str = target_wiki_str.as_str();
    let mut wiki_id = WikiId::parse(target_wiki_str)?;
    // Guard: a fact about SOMEONE ELSE must never be physically
    // filed into an AGENT's own wiki — that space is the agent's autobiography.
    // subject↔wiki are otherwise DECOUPLED by design (a
    // group-owned fact may live in a user wiki and vice versa), so
    // this fires ONLY when the target wiki is flagged `is_agent`. `self` and
    // behaviour-rule facts are handled before this function and never reach
    // here. Redirect to the subject's OWN wiki when it is in the window;
    // otherwise drop this extraction rather than fragment the principal's
    // memory across two wikis.
    //
    // The subject being the agent ITSELF is the exception, and it is why the
    // test is `home != target` and not "is the target an agent wiki": an
    // identity wiki's id IS its principal's id, so `home == target` means the
    // agent's wiki is the subject's own home — the one place that fact belongs.
    // It arrives here whenever a USER states something about the agent ("sei
    // bravo con l'INPS"): the `self` sentinel upstream only fires on an
    // assistant turn, so without this exception the guard would look for a
    // *non*-agent wiki named after the agent, find none, and silently drop
    // every user-stated fact about the assistant.
    if available
        .iter()
        .find(|w| w.wiki_id == target_wiki_str)
        .is_some_and(|w| w.is_agent)
        && !subject_is_the_wikis_own_principal(&subject, target_wiki_str)
    {
        let home = match &subject {
            Principal::User(id) | Principal::Group(id) => id.as_str(),
        };
        // `wiki_id == home` is the whole test: an identity wiki's id IS its
        // principal's, so a match is the subject's own wiki by construction. It
        // deliberately does NOT also demand a non-agent wiki — with two bots
        // enrolled, a fact about bot B aimed at bot A's wiki has a perfectly
        // good home (B's own), and rejecting it for being an agent wiki would
        // drop it on the floor.
        match available.iter().find(|w| w.wiki_id == home) {
            Some(w) => {
                tracing::warn!(
                    from = %target_wiki_str,
                    to = %w.wiki_id,
                    subject = %subject,
                    "ingest: non-self fact targeted an agent wiki — redirected to the subject's own wiki"
                );
                wiki_id = WikiId::parse(w.wiki_id.as_str())?;
            },
            None => {
                return Err(CapturePlanError::TargetIsAgentWiki {
                    target: target_wiki_str.to_owned(),
                    subject: subject.to_string(),
                });
            },
        }
    }
    let allow: std::result::Result<Vec<Principal>, _> = unit
        .allow_ids
        .iter()
        .map(|s| Principal::from_str(s))
        .collect();
    let mut allow = allow?;
    // The classifier occasionally echoes the sender into `allow`. On this
    // LLM-fed path that redundancy is expected noise, not a caller bug:
    // strip it here so capture's `SenderRedundantInAllow` lint (which
    // protects hand-written calls) cannot kill the whole ingest turn.
    let sender_principal = Principal::User(request.sender_id.clone());
    allow.retain(|p| *p != sender_principal);
    // Body: the legacy single-fact shape may omit it (fall back to the raw
    // message); a multi-fact extraction MUST carry its own body, else filing
    // the whole message under every extraction would duplicate it.
    let body = match unit.body.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => b.to_owned(),
        None if allow_message_fallback => request.text.clone(),
        None => return Err(CapturePlanError::MissingBody),
    };
    Ok(CaptureRequest {
        wiki_id,
        page,
        body,
        subject,
        subject_external: unit.subject_external.map(str::to_owned),
        allow,
        sender: Some(Principal::User(request.sender_id.clone())),
        fact_type: unit.fact_type.map(str::to_owned),
        topics: normalize_fact_topics(unit.topics),
        dedup_threshold: Some(policy.dedup_threshold),
        // Thread the per-fact validity interval the classifier deduced
        // through to the capture row, canonicalised: `fact_index` compares
        // these columns lexicographically (due-soon ranges, expiry), so one
        // fixed-width spelling has to land there. A bound naming a DAY becomes
        // that day's edge; one naming no date at all degrades to open (see
        // [`normalize_capture_bound`]).
        valid_from: normalize_capture_bound(unit.valid_from, fact_index::DayEdge::Start),
        valid_to: normalize_capture_bound(unit.valid_to, fact_index::DayEdge::End),
        // `style` is a property of the FACT — is this list-shaped material or
        // prose — and survives whoever ends up choosing the page; the compiler
        // takes a page's style from the majority of the facts on it.
        style: crate::wiki::PageStyle::parse_lenient(unit.style),
        // `page_description` describes the TARGET PAGE, so it only means
        // anything when this extraction actually named one. On the prose path
        // it would describe a page nobody chose.
        page_description: names_its_page
            .then(|| unit.page_description.map(str::to_owned))
            .flatten(),
        // Thread the per-fact salience the classifier
        // deduced through to the capture row (`high` is routed to the card).
        salience: unit.salience.map(str::to_owned),
        // Turn-level provenance breadcrumbs: the project-wiki
        // pages this conversation turn authored, carried in via
        // `metadata.authored_refs`. Attached to every capture from the turn;
        // the personal-vs-project precision is the skill's judgement +
        // downstream consolidation, not a server gate.
        authored_refs: request.metadata.authored_refs.clone(),
    })
}

/// Resolve the optional `supersede_target` field from the LLM plan to a
/// concrete [`FactId`].
///
/// Returns `Ok(None)` when the model did not request a supersede (the
/// common case — every capture turn that is purely additive). Returns
/// `Ok(Some(id))` when the model named a `fact_id` that the orchestrator
/// itself put into `recalled_memory` this turn. Anything else surfaces
/// as a [`CapturePlanError`] so the caller can demote the whole turn to
/// a skip response instead of crashing inside `wiki_supersede`.
fn validate_supersede_target(
    unit: &CaptureUnit<'_>,
    request: &IngestRequest,
    sender_groups: &[String],
    recall_hits: &[RecallHit],
) -> std::result::Result<Option<FactId>, CapturePlanError> {
    let raw = match unit.supersede_target {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Ok(None),
    };
    let fact_id = FactId::parse(raw)?;
    let Some(hit) = recall_hits.iter().find(|h| h.fact_id == fact_id) else {
        let listed = recall_hits
            .iter()
            .map(|h| h.fact_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CapturePlanError::SupersedeTargetNotInRecall {
            id: raw.to_owned(),
            available: if listed.is_empty() {
                "<none>".to_owned()
            } else {
                listed
            },
        });
    };
    // Same-subject guard: a capture may only supersede a fact about the
    // SAME subject. Superseding replaces a prior statement about this
    // subject; without this, an ingest from user X could close a fact
    // owned by user Y when recall surfaces a similar-looking fact (the
    // cross-user supersede leak). The new fact's subject is `unit.subject_id`,
    // else the sender default — mirroring [`validate_capture_plan`].
    let new_subject = match unit.subject_id {
        Some(s) => Principal::from_str(s)?,
        None => Principal::User(request.sender_id.clone()),
    };
    if hit.subject_id != new_subject {
        return Err(CapturePlanError::SupersedeCrossSubject {
            id: raw.to_owned(),
            target_subject: hit.subject_id.to_string(),
            new_subject: new_subject.to_string(),
        });
    }
    if !crate::acl::sender_is_subject(&hit.subject_id, &request.sender_id, sender_groups) {
        return Err(CapturePlanError::SupersedeNotOwned {
            id: raw.to_owned(),
            target_subject: hit.subject_id.to_string(),
            sender: request.sender_id.clone(),
        });
    }
    Ok(Some(fact_id))
}

// ---------- Attachment threading (media pipeline) ----------

/// Resolve the catalog ids one extraction claims against the turn's
/// attachment window. Unknown ids are dropped with a warning (the same
/// anti-hallucination stance as `supersede_target`); duplicates and ids
/// already claimed by an earlier extraction are dropped silently.
fn resolve_unit_attachments(
    unit_ids: &[String],
    request: &IngestRequest,
    claimed: &mut std::collections::HashSet<String>,
) -> Vec<CatalogId> {
    let mut out = Vec::new();
    for raw in unit_ids {
        let raw = raw.trim();
        let Some(att) = request
            .attachments
            .iter()
            .find(|a| a.catalog_id.as_str() == raw)
        else {
            tracing::warn!(
                catalog_id = raw,
                "ingest: extraction claims a catalog_id outside this turn's attachments — dropped"
            );
            continue;
        };
        if claimed.insert(raw.to_owned()) {
            out.push(att.catalog_id.clone());
        }
    }
    out
}

/// Append the code-rendered `{{embed=…}}` markers for `ids` to `body`,
/// space-separated on the body's last line (markers must not span
/// newlines and must ride inside the fact's region so page
/// reorganizations move them with the fact).
fn append_embed_markers(body: &mut String, ids: &[CatalogId]) {
    for id in ids {
        if !body.is_empty() && !body.ends_with(' ') {
            body.push(' ');
        }
        body.push_str(&capture::render_embed_marker(id));
    }
}

/// Widen each linked media row's ACL to the filed fact's read set —
/// soft-fail: a widening hiccup never kills the turn (the bytes stay
/// reachable by the uploader; the next embed of the same media retries).
async fn widen_media_acl_soft(
    pool: &SqlitePool,
    ids: &[CatalogId],
    subject: &Principal,
    allow: &[Principal],
    sender: Option<&Principal>,
) {
    for id in ids {
        if let Err(e) = media::widen_acl(pool, id, subject, allow, sender).await {
            tracing::warn!(catalog_id = %id, error = %e, "ingest: media ACL widening failed");
        }
    }
}

/// Ceiling on images riding one classifier call.
const MAX_VISION_IMAGES: usize = 4;
/// Ceiling on the combined raw byte size of those images (base64 adds
/// ~37% on top; Gemini caps the whole request at ~20 MB).
const MAX_VISION_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Load the blob bytes for the photo attachments the classifier should
/// *look at*: kind `photo`, no consumer-supplied description. Soft-fails
/// per item — a missing row/blob or a cap overrun skips that image (the
/// caption/fallback path still files the fact).
async fn load_attachment_images(
    pool: &SqlitePool,
    workdir: &std::path::Path,
    attachments: &[IngestAttachment],
    llm: &dyn LlmBackend,
) -> Vec<ImageInput> {
    use base64::Engine as _;
    // A model that cannot see gets no images at all: sending them anyway
    // is a 400 that takes the *whole turn* with it, while leaving them out
    // still files the fact from the caption.
    if !llm.accepts_images() {
        let photos = attachments
            .iter()
            .filter(|a| a.kind == media::kind::PHOTO && a.description.is_none())
            .count();
        if photos > 0 {
            tracing::warn!(
                model = llm.model_id(),
                photos,
                "ingest: the classifier's model does not accept images — filing from the caption"
            );
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut total = 0usize;
    for att in attachments {
        if att.kind != media::kind::PHOTO || att.description.is_some() {
            continue;
        }
        if out.len() >= MAX_VISION_IMAGES {
            tracing::warn!(
                catalog_id = %att.catalog_id,
                "ingest: vision image cap reached — photo not shown to the classifier"
            );
            continue;
        }
        let row = match media::find_by_id(pool, &att.catalog_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::warn!(catalog_id = %att.catalog_id, "ingest: attachment has no catalog row");
                continue;
            },
            Err(e) => {
                tracing::warn!(catalog_id = %att.catalog_id, error = %e, "ingest: catalog lookup failed");
                continue;
            },
        };
        if !row.mime.starts_with("image/") {
            continue;
        }
        // The declared type has to be one every provider accepts. A
        // consumer's `image/jpg` is the registered `image/jpeg` and is
        // normalised; a HEIC — which Gemini reads and the others refuse —
        // is dropped here, where it costs one photo, rather than at the
        // API, where it costs the turn.
        let mime = media::canonical_mime(&row.mime);
        if !crate::llm::image_mime_is_portable(&mime) {
            tracing::warn!(
                catalog_id = %att.catalog_id,
                mime = %mime,
                "ingest: image type not accepted by every provider — photo not shown to the classifier"
            );
            continue;
        }
        let blob_abs = media::blob_path(workdir, &row.sha256);
        let bytes = match std::fs::read(&blob_abs) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(catalog_id = %att.catalog_id, error = %e, "ingest: blob read failed");
                continue;
            },
        };
        if total + bytes.len() > MAX_VISION_TOTAL_BYTES {
            tracing::warn!(
                catalog_id = %att.catalog_id,
                size = bytes.len(),
                "ingest: vision byte budget exhausted — photo not shown to the classifier"
            );
            continue;
        }
        total += bytes.len();
        out.push(ImageInput {
            mime_type: mime,
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    out
}

/// Body for an unclaimed attachment's fallback fact: the consumer
/// description, else the caption. `None` — file nothing — when neither
/// carries text, or when the text would break the buffer validators
/// (marker braces / the journal's comment delimiter; it survives on
/// the catalog row either way).
fn unclaimed_attachment_body(att: &IngestAttachment) -> Option<String> {
    let body = att
        .description
        .as_deref()
        .or(att.caption.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();
    if body.contains("{{") || body.contains("}}") || body.contains("<!--") {
        return None;
    }
    Some(body)
}

/// File every unclaimed attachment that carries SOME describing text
/// (consumer description or caption): one buffered fact per attachment,
/// body = that text + the embed marker, targeted at the sender's
/// identity wiki. A text-less unclaimed item files NOTHING — a fact
/// whose whole body is the kind word ("audio") has no recall surface
/// and only pollutes the page; the blob stays catalogued and reachable
/// (dashboard/media), just outside the wiki. Runs on every path —
/// including the LLM-down fallback. Returns the first filed id.
async fn file_unclaimed_attachments(
    pool: &SqlitePool,
    tree: &WikiTree,
    request: &IngestRequest,
    available: &[AvailableWiki],
    policy: &IngestPolicy,
    claimed: &std::collections::HashSet<String>,
) -> Option<FactId> {
    let unclaimed: Vec<&IngestAttachment> = request
        .attachments
        .iter()
        .filter(|a| !claimed.contains(a.catalog_id.as_str()))
        .collect();
    if unclaimed.is_empty() {
        return None;
    }
    // The precondition, not a destination: these captures wait in the buffer,
    // which names no wiki, and promotion parks them in the SUBJECT's own wiki
    // — here the sender's, since an unclaimed attachment is filed about
    // whoever sent it. So the sender having an identity wiki is what decides
    // whether the claim can ever land; without one it would sit buffered for
    // ever, and leaving the blob merely catalogued is the better failure.
    //
    // Resolved against the FULL tree, not the prompt's `available` window: an
    // identity wiki is exempt from the cap and so is always in the window
    // today, but this must hold even if that changes — and `available` may be
    // the empty list a soft enumeration failure left behind. `available_wikis`
    // drops smart wikis itself: a smart-managed identity wiki never takes
    // buffered captures.
    let target = available
        .iter()
        .find(|w| w.wiki_id == request.sender_id)
        .cloned()
        .or_else(|| {
            available_wikis(tree, usize::MAX)
                .ok()?
                .into_iter()
                .find(|w| w.wiki_id == request.sender_id)
        });
    let Some(target) = target else {
        tracing::warn!(
            sender_id = request.sender_id.as_str(),
            unclaimed = unclaimed.len(),
            "ingest: unclaimed attachments but the sender has no identity wiki — media stays catalogued, unfiled"
        );
        return None;
    };
    let Ok(wiki_id) = WikiId::parse(&target.wiki_id) else {
        return None;
    };
    let mut first: Option<FactId> = None;
    for att in unclaimed {
        let Some(mut body) = unclaimed_attachment_body(att) else {
            // No usable describing text: filing would mint a fact whose
            // whole recall surface is the kind word. The blob stays
            // catalogued (dashboard/media); the wiki gets nothing.
            tracing::info!(
                catalog_id = %att.catalog_id,
                kind = att.kind.as_str(),
                "ingest: unclaimed attachment has no usable caption/description — stays catalogued, unfiled"
            );
            continue;
        };
        append_embed_markers(&mut body, std::slice::from_ref(&att.catalog_id));
        let subject = Principal::User(request.sender_id.clone());
        let cap_req = CaptureRequest {
            wiki_id: wiki_id.clone(),
            subject_external: None,
            // Nobody placed this: it waits in the queue like any other claim.
            page: None,
            body,
            subject: subject.clone(),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: Vec::new(),
            dedup_threshold: Some(policy.dedup_threshold),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            // Unclaimed-media fallback captures are not the turn's authored
            // project content — no provenance breadcrumbs.
            authored_refs: Vec::new(),
        };
        match capture_buffer::buffer_capture(pool, cap_req, None).await {
            Ok(buffered) => {
                tracing::info!(
                    capture_id = buffered.capture_id.as_str(),
                    catalog_id = %att.catalog_id,
                    "ingest: unclaimed attachment filed via the deterministic fallback"
                );
                widen_media_acl_soft(
                    pool,
                    std::slice::from_ref(&att.catalog_id),
                    &subject,
                    &[],
                    None,
                )
                .await;
                if first.is_none() {
                    first = Some(buffered.capture_id);
                }
            },
            Err(e) => {
                tracing::warn!(
                    catalog_id = %att.catalog_id,
                    error = %e,
                    "ingest: deterministic attachment filing failed — media stays catalogued, unfiled"
                );
            },
        }
    }
    first
}

/// The skip-fallback response with the media guarantee attached: file
/// whatever attachments are still unclaimed before degrading, so even
/// an LLM-down turn never strands catalogued media. `capture_id`
/// carries the first fallback-filed fact when one filed.
#[allow(clippy::too_many_arguments)] // a degraded-path bundle, mirrors fallback_response + the filing context
async fn fallback_with_unclaimed_media(
    pool: &SqlitePool,
    tree: &WikiTree,
    request: &IngestRequest,
    available: &[AvailableWiki],
    policy: &IngestPolicy,
    recall_hits: &[RecallHit],
    elapsed: std::time::Duration,
    llm_used: bool,
    claimed: &std::collections::HashSet<String>,
) -> IngestResponse {
    let mut resp = fallback_response(request, recall_hits, policy, elapsed, llm_used);
    resp.capture_id =
        file_unclaimed_attachments(pool, tree, request, available, policy, claimed).await;
    resp
}

/// Why a requested closure was refused (warn-and-skip — a bad closure
/// never demotes the turn; parallel to [`CapturePlanError`]).
#[derive(Debug, Error)]
enum ClosurePlanError {
    /// The closure carried no `target`.
    #[error("closure has no target")]
    MissingTarget,
    /// `target` is not a well-formed `FactId`.
    #[error("invalid closure target fact_id: {0}")]
    BadFactId(#[from] FactIdParseError),
    /// Anti-hallucination guard, parallel to
    /// [`CapturePlanError::SupersedeTargetNotInRecall`]: the model may only
    /// close ids it actually saw in this turn's `recalled_memory`.
    #[error("closure target `{id}` is not in recalled_memory ({available})")]
    TargetNotInRecall { id: String, available: String },
    /// `reason` is missing or outside the closed vocabulary.
    #[error("closure reason `{0}` is not one of completed|retracted|contradicted")]
    UnknownReason(String),
    /// The sender is neither the target's subject nor the one who said it.
    /// A closure withdraws an assertion, so it is open to the subject (the
    /// named user, or a member of the named non-global group) and to
    /// whoever made the assertion — and to nobody else, which is what keeps
    /// one user's turn from closing a fact of another's. A world fact with
    /// no recorded author is closable by no one from chat.
    /// See [`crate::acl::sender_may_retract`].
    #[error("closure target `{id}` is about {subject} and was said by somebody else")]
    NotSubjectOrAuthor { id: String, subject: String },
}

/// Validate one requested closure against this turn's recall window.
///
/// Returns the matched [`RecallHit`] (its `wiki_id` + text feed the
/// receipt) and the canonical [`fact_index::decay`] reason. Tolerant on
/// the reason's *spelling* (a few obvious aliases map in), strict on its
/// *presence*: a closure without a recognisable reason is skipped, never
/// guessed.
fn validate_closure<'a>(
    closure: &LlmClosure,
    recall_hits: &'a [RecallHit],
    sender_id: &str,
    sender_groups: &[String],
) -> std::result::Result<(&'a RecallHit, &'static str), ClosurePlanError> {
    let raw = closure
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ClosurePlanError::MissingTarget)?;
    let fact_id = FactId::parse(raw)?;
    let Some(hit) = recall_hits.iter().find(|h| h.fact_id == fact_id) else {
        let listed = recall_hits
            .iter()
            .map(|h| h.fact_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ClosurePlanError::TargetNotInRecall {
            id: raw.to_owned(),
            available: if listed.is_empty() {
                "<none>".to_owned()
            } else {
                listed
            },
        });
    };
    // Retraction gate: closing a fact's validity withdraws an assertion, so
    // the subject may do it (the named user or a member of the named
    // non-global group) and so may whoever MADE the assertion. A world fact
    // (subject=global) with no recorded author stays closable by no one from
    // chat.
    if !crate::acl::sender_may_retract(
        &hit.subject_id,
        hit.sender_id.as_ref(),
        sender_id,
        sender_groups,
    ) {
        return Err(ClosurePlanError::NotSubjectOrAuthor {
            id: raw.to_owned(),
            subject: hit.subject_id.to_string(),
        });
    }
    let reason = match closure
        .reason
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("completed" | "done" | "consumed") => fact_index::decay::COMPLETED,
        Some("retracted" | "abandoned" | "forgotten" | "cancelled") => fact_index::decay::RETRACTED,
        Some("contradicted" | "contradiction") => fact_index::decay::CONTRADICTED,
        other => {
            return Err(ClosurePlanError::UnknownReason(
                other.unwrap_or_default().to_owned(),
            ));
        },
    };
    Ok((hit, reason))
}

/// The confirmer's strict-JSON reply shape (see
/// [`BUNDLED_INGEST_CLOSURES_MD`]).
#[derive(Debug, Deserialize)]
struct TopicClosureDecision {
    #[serde(default)]
    closures: Vec<LlmClosure>,
}

/// What the reconciliation stage decided about facts that already existed.
///
/// Deliberately the SAME per-verb types the whole apply side is written
/// against, so the three `apply_plan_*` functions — and their guards and
/// receipts — are reused rather than reimplemented. All three empty is the
/// common, correct answer.
#[derive(Debug, Default, serde::Deserialize)]
struct ReconcileDecision {
    #[serde(default)]
    closures: Vec<LlmClosure>,
    #[serde(default)]
    supersedes: Vec<LlmSupersede>,
    #[serde(default)]
    validity_edits: Vec<LlmValidityEdit>,
    #[serde(default)]
    acl_changes: Vec<LlmAclChange>,
}

impl ReconcileDecision {
    const fn is_empty(&self) -> bool {
        self.closures.is_empty()
            && self.supersedes.is_empty()
            && self.validity_edits.is_empty()
            && self.acl_changes.is_empty()
    }
}

/// One requested supersede: an existing fact is replaced by one this turn
/// wrote.
#[derive(Debug, serde::Deserialize)]
struct LlmSupersede {
    /// `fact_id` of the OLD fact, from the candidate list.
    #[serde(default)]
    target: Option<String>,
    /// `fact_id` of the NEW fact — one this turn filed.
    #[serde(default)]
    successor: Option<String>,
}

/// Vet one requested supersede, refusing rather than guessing.
///
/// Three guards, and each one answers a different way of being wrong:
/// - the **target** must be one of the candidates the stage was shown, so a
///   hallucinated id retires nothing;
/// - the **successor** must be one of the facts this turn actually filed, so a
///   fact can never be welded to something that does not exist, or to itself;
/// - the sender must **own** the target, through
///   [`crate::acl::sender_is_subject`] — the same call its two siblings
///   (`apply_plan_validity_edits`, `apply_plan_acl_changes`) make, so a member
///   of an owning group counts as the subject. Reading a fact is not authority
///   over it, and a supersede rewrites both its validity and its successor
///   pointer.
fn vet_supersede<'a>(
    s: &LlmSupersede,
    candidates: &'a [RecallHit],
    turn_facts: &[(FactId, String)],
    sender_id: &str,
    sender_groups: &[String],
) -> Option<(FactId, FactId, &'a RecallHit)> {
    let (Some(target_raw), Some(successor_raw)) = (s.target.as_deref(), s.successor.as_deref())
    else {
        tracing::warn!("ingest: reconcile supersede missing target or successor — skipped");
        return None;
    };
    let (Ok(target_id), Ok(successor_id)) =
        (FactId::parse(target_raw), FactId::parse(successor_raw))
    else {
        tracing::warn!(
            target = target_raw,
            successor = successor_raw,
            "ingest: reconcile supersede carries an unparseable id"
        );
        return None;
    };
    // Nothing replaces itself. The two lists the judge picks from are disjoint
    // by construction — [`reconcile_candidates`] keeps the turn's own facts out
    // of the candidate set — so this can only fire on an id the model invented
    // and then named twice. It stands ahead of the candidate lookup so the log
    // names the mistake that was actually made, and it is worth refusing
    // loudly: acted on, a self-supersede is destructive rather than merely
    // useless, because the weld closes the target's window and writes no
    // successor link to explain it, leaving a fact that nothing contradicted
    // marked `contradicted`.
    if target_id == successor_id {
        tracing::warn!(
            target = target_raw,
            "ingest: reconcile supersede names one fact as both the replaced and the replacement — refused"
        );
        return None;
    }
    let Some(prev) = candidates.iter().find(|h| h.fact_id == target_id) else {
        tracing::warn!(
            target = target_raw,
            "ingest: reconcile supersede target is not a candidate — refused"
        );
        return None;
    };
    if !turn_facts.iter().any(|(id, _)| *id == successor_id) {
        tracing::warn!(
            successor = successor_raw,
            "ingest: reconcile supersede successor is not a fact this turn filed — refused"
        );
        return None;
    }
    if !crate::acl::sender_is_subject(&prev.subject_id, sender_id, sender_groups) {
        tracing::warn!(
            target = target_raw,
            subject = %prev.subject_id,
            "ingest: reconcile supersede refused — the sender does not own the target"
        );
        return None;
    }
    Some((target_id, successor_id, prev))
}

/// Carry the superseded fact's audience onto its successor, wherever the
/// successor currently lives.
///
/// The fact store is probed first; `false` there means the successor is still
/// a buffered capture, and since the id is stable across promotion the buffer
/// row is the one to correct. Returns whether the audience landed.
///
/// **The allow list only.** The successor keeps its own `subject_id` and its own
/// `sender_id`: a supersede replaces what a fact says, never whose fact it is.
/// See [`fact_index::inherit_allow`] for what carrying the subject across would
/// cost — it is the case where a fact about Bob ends up unreadable by Bob.
///
/// `allow` reaches here already stripped of `global` by
/// [`apply_reconciled_supersedes`], which skips the call outright when
/// nothing else is left to carry.
async fn inherit_audience(pool: &SqlitePool, successor: &FactId, allow: &[Principal]) -> bool {
    match fact_index::inherit_allow(pool, successor, allow).await {
        Ok(true) => true,
        Ok(false) => matches!(
            capture_buffer::inherit_allow(pool, successor, allow).await,
            Ok(true)
        ),
        Err(err) => {
            tracing::warn!(error = %err, "ingest: supersede audience inheritance failed");
            false
        },
    }
}

/// Apply the reconciler's supersedes: weld each new fact to the one it
/// replaces, **audience first**.
///
/// The order is the whole point. A supersede is a **content update, not a
/// sharing change**: the new fact inherits the superseded fact's allow list,
/// `global` excepted. The reconciler can tell that a claim was restated; it
/// must never be relied on to restate who may read it, because a re-statement
/// that quietly drops the allow list re-privatises a shared fact and nothing
/// anywhere says so. `global` is the one reader that does not travel: it is
/// not a wider audience but the absence of one, and a mis-paired supersede
/// that carried it would publish a fact somebody had just decided to keep
/// inside their household.
/// Deciding the supersede after the new fact is written makes this a second
/// write, where inheriting before the write would have been free. That is the
/// price of asking the question where it can be answered honestly: the
/// classifier does not see enough to answer it.
///
/// **The allow list, and nothing else.** The successor keeps its own subject and
/// its own sender: a supersede does not move the subject either. Alice
/// retiring "Alice is at the dentist Thursday" by saying "it is Bob who goes"
/// mints a fact owned by Bob — and a reader set is `subject ∪ allow ∪ sender`,
/// so carrying Alice over as the subject would take the fact about Bob away
/// from Bob while Alice was trying to tell him. Where the subject does not
/// change (a restated wifi password) the subject was already the same, which is
/// why this was invisible.
///
/// Inheriting before welding also fails in the recoverable direction: a failed
/// inheritance leaves the old fact open beside the new one, which is visibly
/// wrong, where a failed weld after a good inheritance leaves the new fact
/// already correctly shared. The current sender is stripped from the inherited
/// list, mirroring `validate_capture_plan`'s `SenderRedundantInAllow` guard.
/// Every step is soft: a refused or failed supersede is logged and skipped,
/// never fatal.
///
/// Returns how many were applied.
async fn apply_reconciled_supersedes(
    pool: &SqlitePool,
    supersedes: &[LlmSupersede],
    candidates: &[RecallHit],
    turn_facts: &[(FactId, String)],
    request: &IngestRequest,
) -> usize {
    let sender = Principal::User(request.sender_id.clone());
    // Resolve the sender's groups once so the subject gate can admit a
    // member of an owning group, not just the owning user.
    let sender_groups = enrollment::groups_for(pool, &request.sender_id)
        .await
        .unwrap_or_default();
    let mut applied = 0usize;
    for s in supersedes {
        let Some((target_id, successor_id, prev)) = vet_supersede(
            s,
            candidates,
            turn_facts,
            request.sender_id.as_str(),
            &sender_groups,
        ) else {
            continue;
        };
        // The audience travels, EXCEPT `global`. Inheritance exists so a
        // restatement cannot quietly hide a fact from people who could
        // already read it; turning a per-fact decision into a public one is
        // the opposite failure and it is the worse one — public is not a
        // wider audience, it is the absence of one. So `global` is only ever
        // reached by deciding it for THIS fact, on its own merits.
        //
        // When stripping it leaves nothing, there is nobody to carry over:
        // the successor keeps the audience the classifier gave it.
        let inherited: Vec<Principal> = prev
            .allow_ids
            .iter()
            .filter(|p| **p != sender && !p.is_global())
            .cloned()
            .collect();
        if inherited.is_empty() {
            if weld_supersede(pool, &target_id, &successor_id, request.turn_now()).await {
                applied += 1;
                tracing::info!(
                    target = target_id.as_str(),
                    successor = successor_id.as_str(),
                    "ingest: reconcile superseded a fact; its audience was public, \
                     so the successor keeps its own"
                );
            }
            continue;
        }
        if !inherit_audience(pool, &successor_id, &inherited).await {
            tracing::warn!(
                successor = successor_id.as_str(),
                "ingest: supersede successor not found in either store — not superseding"
            );
            continue;
        }
        if weld_supersede(pool, &target_id, &successor_id, request.turn_now()).await {
            applied += 1;
            tracing::info!(
                target = target_id.as_str(),
                successor = successor_id.as_str(),
                inherited = inherited.len(),
                "ingest: reconcile superseded a fact, audience carried over"
            );
        } else {
            tracing::warn!(
                target = target_id.as_str(),
                "ingest: reconcile supersede target is in neither store as a live row — nothing to do"
            );
        }
    }
    applied
}

/// Retire the superseded fact, wherever it lives.
///
/// The fact store is the normal case. `0` rows there means one of two things,
/// and reading it as the first would be silent and wrong: the target is
/// already closed, **or** it was never promoted and is still a buffered
/// capture — `mark_superseded`'s `WHERE fact_id = ?` matches no buffer row, so
/// the applier would report *«already closed»* for a retirement that had not
/// happened. On the same-day flow (a claim captured this morning, corrected
/// this afternoon, before the light dream ran) that is every supersede.
///
/// The buffered half closes the **staged** window with the same
/// `contradicted` reason, exactly as the closure verb's buffered half does
/// ([`capture_buffer::close_validity`]); promotion then carries the closed
/// window onto the fact. No successor pointer is staged, by the same standing
/// rule that applies to closures — the buffer stages none, and the id is
/// stable across promotion.
///
/// Returns whether a live row was actually retired. Soft on both stores, like
/// [`inherit_audience`]: a failed write is logged and the supersede is skipped,
/// never fatal to the turn.
async fn weld_supersede(
    pool: &SqlitePool,
    target: &FactId,
    successor: &FactId,
    turn_now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match fact_index::mark_superseded(pool, target, successor, turn_now).await {
        Ok(n) if n > 0 => return true,
        Ok(_) => {},
        Err(err) => {
            tracing::warn!(error = %err, "ingest: supersede weld failed on the fact store");
            return false;
        },
    }
    // The same two clocks as `fact_index::mark_superseded`: the window closes
    // when its replacement started, not when the engine noticed.
    let closed_at = fact_index::successor_valid_from(pool, successor)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| fact_index::bound_from_instant(turn_now));
    match capture_buffer::close_validity(pool, target, &closed_at, fact_index::decay::CONTRADICTED)
        .await
    {
        Ok(Some(_)) => {
            tracing::info!(
                target = target.as_str(),
                "ingest: supersede target was still buffered — staged window closed instead"
            );
            true
        },
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: supersede weld failed on the capture buffer");
            false
        },
    }
}

/// A candidate's validity as the two judging stages state it, decided against
/// the turn's clock and **never** by the presence of a `valid_to`.
///
/// A horizon still ahead of us is an OPEN fact carrying a deadline, and both
/// prompts that show a candidate tell the model to leave a closed one alone.
/// Deciding "closed" on the field's presence hides exactly the class those
/// stages exist for, because the classifier stamps a `valid_to` on **every**
/// dated commitment: *«devo comprare il latte entro venerdì»* … *«l'ho
/// comprato»* renders the candidate `closed <friday>`, the model obeys its own
/// rule, and nothing closes. Same predicate the read side uses
/// ([`crate::recall::window_closed_at`]).
///
/// One renderer for both stages. They ask the same question of the same rows,
/// so a difference in how the row reads to them would be a difference nobody
/// chose.
fn candidate_validity(h: &RecallHit, now: &chrono::DateTime<chrono::Utc>) -> String {
    match (h.valid_from.as_deref(), h.valid_to.as_deref()) {
        (_, Some(to)) if recall::window_closed_at(Some(to), now) => format!("closed {to}"),
        (Some(from), Some(to)) => format!("open since {from}, due {to}"),
        (None, Some(to)) => format!("open, due {to}"),
        (Some(from), None) => format!("open since {from}"),
        (None, None) => "open".to_owned(),
    }
}

/// Render one candidate as the reconciliation stage's prompt sees it:
/// `fact_id · validity · audience · text`.
///
/// The audience is rendered because `acl_changes` REPLACES the allow list:
/// a model asked to add the family to a fact has to be shown who is already
/// on it, or "share it with the family too" silently drops everyone else.
fn reconcile_candidate_line(h: &RecallHit, now: &chrono::DateTime<chrono::Utc>) -> String {
    let validity = candidate_validity(h, now);
    let audience = if h.allow_ids.is_empty() {
        format!("subject {}", h.subject_id)
    } else {
        let allow = h
            .allow_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!("subject {} allow [{allow}]", h.subject_id)
    };
    format!(
        "{} · {validity} · {audience} · {}",
        h.fact_id,
        truncate(&h.text, 160)
    )
}

/// Render one candidate as the closure confirmer's prompt sees it:
/// `fact_id · validity · text`.
///
/// No audience: this stage has one verb and it closes, so who may read the
/// fact is not its business and showing it would only invite a judgement it
/// cannot act on.
fn closure_candidate_line(h: &RecallHit, now: &chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{} · {} · {}",
        h.fact_id,
        candidate_validity(h, now),
        truncate(&h.text, 160)
    )
}

/// Assemble the reconciliation stage's candidate set: the union, deduplicated
/// by `fact_id`, of what this turn actually read.
///
/// The structural leg is EVERY page whose prose the turn injected, not only
/// the navigator's walk. An identity card is served in full and
/// deterministically, whatever the navigator decides, so its facts are the
/// most certainly-read facts of the whole turn — and leaving them out made a
/// correction to one of them («I don't live in Bologna any more») land only
/// when similarity had happened to fish the old fact up as well.
///
/// Order matters and is not arbitrary. The flat hits come first because they
/// are ranked by relevance to this very message; the page-scoped facts follow
/// because they are complete rather than ranked, so if the cap ever bites it
/// takes from the tail of the structural leg rather than from the head of the
/// relevant one.
///
/// **The buffered leg is re-fetched with NO already-in-context suppression**,
/// the same call `confirm_topic_closures` makes and for the same stated
/// reason: that suppression exists to stop the recall *block* saying a thing
/// twice, and it must never hide a candidate from a verb that acts on it. The
/// flat hits arrive already filtered by it — so a claim the user made two
/// turns ago, whose message is still in the window, would be invisible here,
/// which is precisely the claim a correction arriving now is correcting. Costs
/// one extra embed of the message; the buffered rows carry staged vectors.
///
/// **The facts this turn filed are not candidates.** Their ids seed the
/// union's dedup set, so whichever leg surfaces one drops it the way it drops
/// a repeat, and no verb can name it. They reach the stage by the other door,
/// the `{new_facts}` block, where they are legal only as a `successor`. A verb
/// allowed to name them would be judging a claim against itself: two atomics
/// off one message marked as contradicting each other, or an episode archived
/// as spent in the instant it is born, which the next turn's recall then
/// down-ranks. The fresh leg's budget is widened by their count so the
/// exclusion costs it no slots — its default is three, and the turn's own
/// claims are the buffered rows nearest the message being searched, so they
/// would take the whole slot before any older candidate reached it.
async fn reconcile_candidates(
    pool: &SqlitePool,
    embedder: &Arc<dyn Embedder>,
    query: &str,
    flat: &[RecallHit],
    injected_pages: &[String],
    sender_ctx: &SenderContext,
    fresh_top_k: usize,
    turn_facts: &[(FactId, String)],
) -> Vec<RecallHit> {
    let mut out: Vec<RecallHit> = Vec::new();
    let mut seen: std::collections::HashSet<String> = turn_facts
        .iter()
        .map(|(id, _)| id.as_str().to_owned())
        .collect();
    for h in flat {
        if seen.insert(h.fact_id.as_str().to_owned()) {
            out.push(h.clone());
        }
    }
    match recall::facts_on_pages(pool, injected_pages, sender_ctx, RECONCILE_CANDIDATE_CAP).await {
        Ok(hits) => {
            for h in hits {
                if seen.insert(h.fact_id.as_str().to_owned()) {
                    out.push(h);
                }
            }
        },
        Err(err) => {
            // Soft, like every other leg: reconciling against less is worse
            // than reconciling against nothing only if the turn then claims
            // completeness, and it never does.
            tracing::warn!(error = %err, "ingest: page-scoped candidates unavailable — reconciling on the flat hits alone");
        },
    }
    match recall::recall_fresh_captures(
        pool,
        embedder.as_ref(),
        query,
        sender_ctx,
        fresh_top_k.saturating_add(turn_facts.len()),
        &std::collections::HashSet::new(),
    )
    .await
    {
        Ok(hits) => {
            for h in hits {
                if seen.insert(h.fact_id.as_str().to_owned()) {
                    out.push(h);
                }
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "ingest: buffered candidates unavailable — reconciling without the fresh leg");
        },
    }
    out.truncate(RECONCILE_CANDIDATE_CAP);
    out
}

/// The **reconciliation stage** — one call, after the navigator, deciding what
/// this turn does to facts that ALREADY EXIST.
///
/// Runs at the last point in the turn where the engine has actually *read* the
/// memory, which is the whole reason it is here and not in the classifier:
/// the four verbs all judge stored facts, and the classifier is shown a top-K
/// similarity sample. The founder's rule — *a slot may reconcile against a set
/// it is shown COMPLETE, never against a sample* — is satisfiable here because
/// [`recall::facts_on_pages`] returns every readable fact on each page the
/// turn read, cards included.
///
/// **It is shown the completed message beside the raw one.** The stage acts
/// only on a candidate whose text plainly matches what the message says, and
/// *«l'ho comprato»*, *«fatto!»*, *«sistemato, non serve più»* say nothing
/// matchable in their own words — the subject lives in the exchange before
/// them, which this stage never sees. The classifier does see it, and the
/// sentence it wrote with the implicit part filled in costs nothing to carry
/// down here. Without it the textbook case — *«l'ho comprato»* closing *«devo
/// comprare il latte»* — lands only when the two phrasings happen to share
/// words. The raw message stays in front of it too: the completion is a
/// reading, and a reading can be wrong about what the user actually said.
///
/// Skipped with no model call when the candidate set is empty: a turn that
/// read nothing has nothing to reconcile against, and that is the shape of a
/// pure chat turn.
///
/// Every failure is soft — an unreachable model, an unparseable answer, a
/// hallucinated id — and returns "nothing changed". The stage may only ever
/// retire, re-date or re-share something the message plainly named; when in
/// doubt it does nothing, because a missed reconciliation comes back next turn
/// and a wrong one has already forgotten the wrong thing.
async fn reconcile_after_reading(
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    request: &IngestRequest,
    turn_now: chrono::DateTime<chrono::Utc>,
    candidates: &[RecallHit],
    turn_facts: &[(FactId, String)],
    completed_message: Option<&str>,
) -> ReconcileDecision {
    if candidates.is_empty() {
        return ReconcileDecision::default();
    }
    let lines = candidates
        .iter()
        .map(|h| reconcile_candidate_line(h, &turn_now))
        .collect::<Vec<_>>()
        .join("\n");
    let turn_time = format!("{} ({})", turn_now.to_rfc3339(), turn_now.format("%A"));
    // The only legal `successor` values. `(none)` rather than an empty block:
    // a gesture turn files nothing, and the model has to see that there is
    // nothing to weld to rather than infer it from a blank.
    let new_facts = if turn_facts.is_empty() {
        "(none)".to_owned()
    } else {
        turn_facts
            .iter()
            .map(|(id, text)| format!("{id} · {text}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let prompt = match prompts::render(
        "ingest-reconcile",
        tree.workdir(),
        BUNDLED_INGEST_RECONCILE_MD,
        &[
            ("message", request.text.as_str()),
            ("completed_message", completed_message.unwrap_or("(none)")),
            ("current_time", turn_time.as_str()),
            ("candidates", lines.as_str()),
            ("new_facts", new_facts.as_str()),
        ],
    ) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: reconcile prompt failed — nothing reconciled");
            return ReconcileDecision::default();
        },
    };
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(1024),
        )
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: reconciler unavailable — nothing reconciled");
            return ReconcileDecision::default();
        },
    };
    let decision = parse_first_json::<ReconcileDecision>(&resp.text).unwrap_or_else(|| {
        tracing::warn!("ingest: reconciler answer unparseable — nothing reconciled");
        ReconcileDecision::default()
    });
    tracing::info!(
        candidates = candidates.len(),
        closures = decision.closures.len(),
        validity_edits = decision.validity_edits.len(),
        acl_changes = decision.acl_changes.len(),
        "ingest: reconciliation stage done"
    );
    decision
}

/// The focused per-topic recall of [`confirm_topic_closures`]: each
/// topic is its own query against promoted facts AND the fresh buffered
/// slot (a same-day target lives only there), the union deduplicated by
/// fact id. Every failure is soft — a topic that cannot be recalled
/// contributes no candidates.
///
/// The facts this turn filed are held out, on the same terms as
/// [`reconcile_candidates`]: a closure gesture ends something that was
/// already there, and the buffered rows nearest this message are the claims
/// this very message just made. The fresh budget is widened by their count so
/// holding them out costs the slot nothing.
async fn recall_topic_candidates(
    pool: &SqlitePool,
    embedder: &Arc<dyn Embedder>,
    topics: &[String],
    sender_ctx: &SenderContext,
    policy: &IngestPolicy,
    turn_facts: &[(FactId, String)],
) -> Vec<RecallHit> {
    let mut candidates: Vec<RecallHit> = Vec::new();
    for topic in topics
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .take(CLOSURE_TOPICS_CAP)
    {
        let promoted = recall::wiki_recall(
            pool,
            Arc::clone(embedder),
            topic,
            policy.recall_top_k,
            fact_index::FactFilters::default(),
            sender_ctx,
        )
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, topic, "ingest: closure-topic recall failed — skipped");
            Vec::new()
        });
        // No already-in-context suppression here, deliberately. This pass is
        // hunting for the TARGETS of a closure gesture, and a claim the user
        // made moments ago — one whose message is still in the window — is
        // precisely the kind of thing a "forget that" gesture closes. The
        // suppression exists to stop the recall block saying a thing twice;
        // it must not hide a candidate from a verb that acts on it.
        let fresh = recall::recall_fresh_captures(
            pool,
            embedder.as_ref(),
            topic,
            sender_ctx,
            policy.recall_fresh_top_k.saturating_add(turn_facts.len()),
            &std::collections::HashSet::new(),
        )
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, topic, "ingest: closure-topic fresh recall failed");
            Vec::new()
        });
        for hit in promoted.into_iter().chain(fresh) {
            if turn_facts.iter().any(|(id, _)| *id == hit.fact_id) {
                continue;
            }
            if !candidates.iter().any(|c| c.fact_id == hit.fact_id) {
                candidates.push(hit);
            }
        }
    }
    candidates
}

/// The closure-aware second recall pass — aim correction for a
/// recall-starved gesture.
///
/// The dogfood re-run (2026-06-11) showed the failure: for *"forget what I
/// told you about the greenhouse: I have given up on the project"* the
/// whole-message embedding ranked a dozen shopping items above the greenhouse
/// facts, so the classifier saw none of its targets and spent the closure
/// on a wrong recalled fact. When the classifier instead names the
/// gesture's TOPICS (`closure_topics` — targets it could not see), this
/// pass recalls each topic as its own focused query (promoted facts + the
/// fresh buffered slot, so a same-day target is reachable, less whatever this
/// turn itself filed), shows the deduplicated candidate union to a strict
/// confirmer on the same ingest slot, and returns the confirmed closures
/// together with the candidates they validate against. Every limit here is a
/// resource cap; which candidates close is the confirmer's judgment, and an
/// empty answer is always a valid one.
///
/// The confirmer reads the completed message beside the raw one, for the
/// reason [`reconcile_after_reading`] writes down: a closure gesture is
/// exactly the shape of message that leaves its subject in the previous
/// exchange, and *«l'ho comprato»* on its own matches no candidate's words.
///
/// Soft end to end: a recall or LLM failure returns no closures — the
/// turn never dies on the second pass.
#[expect(
    clippy::too_many_arguments,
    reason = "the turn's full context (store, tree, embedder, slot, request, clock, topics, sender, policy, the facts it filed, the completed message); a one-off bundle struct for the single call site would only rename the problem"
)]
async fn confirm_topic_closures(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    request: &IngestRequest,
    turn_now: chrono::DateTime<chrono::Utc>,
    topics: &[String],
    sender_ctx: &SenderContext,
    policy: &IngestPolicy,
    turn_facts: &[(FactId, String)],
    completed_message: Option<&str>,
) -> (Vec<LlmClosure>, Vec<RecallHit>) {
    let candidates =
        recall_topic_candidates(pool, embedder, topics, sender_ctx, policy, turn_facts).await;
    if candidates.is_empty() {
        tracing::info!("ingest: closure topics recalled no candidates — nothing to close");
        return (Vec::new(), Vec::new());
    }

    let lines = candidates
        .iter()
        .map(|h| closure_candidate_line(h, &turn_now))
        .collect::<Vec<_>>()
        .join("\n");
    let turn_time = format!("{} ({})", turn_now.to_rfc3339(), turn_now.format("%A"));
    let prompt = match prompts::render(
        "ingest-closures",
        tree.workdir(),
        BUNDLED_INGEST_CLOSURES_MD,
        &[
            ("message", request.text.as_str()),
            ("completed_message", completed_message.unwrap_or("(none)")),
            ("current_time", turn_time.as_str()),
            ("candidates", lines.as_str()),
        ],
    ) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: closure-confirmer prompt failed — skipped");
            return (Vec::new(), Vec::new());
        },
    };
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(1024),
        )
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: closure confirmer unavailable — no closures");
            return (Vec::new(), candidates);
        },
    };
    let confirmed = parse_first_json::<TopicClosureDecision>(&resp.text).map_or_else(
        || {
            tracing::warn!("ingest: closure confirmer answer unparseable — no closures");
            Vec::new()
        },
        |d| d.closures,
    );
    tracing::info!(
        topics = topics.len(),
        candidates = candidates.len(),
        confirmed = confirmed.len(),
        "ingest: closure-topic pass done"
    );
    (confirmed, candidates)
}

/// Apply the plan's requested closures — the ingest half of the closure
/// verb ("ingest closes the validity of existing facts").
///
/// Shared by the two fronts the maintainer decided (2026-06-11): the
/// **completion trigger** (the message states an open item is spent —
/// Jumanji watched, the milk bought) and the **relayed forget/abandon
/// gesture** ("forget what I told you about the greenhouse"). The LLM
/// decides the blast radius — which recalled facts close — and the code
/// executes act-first: stamp the validity (fact row first, then the
/// still-buffered capture — the id is stable across promotion), emit ONE
/// born-applied `validity_close` receipt for the turn, and post the
/// `structure_applied` notice pointing at the dashboard, where the
/// closure can be read.
///
/// Every step is soft: an invalid closure, a vanished target, or a DB
/// hiccup is logged and skipped — a closure never kills the turn.
///
/// Returns the number of closures applied.
async fn apply_plan_closures(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan_closures: &[LlmClosure],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
    turn_now: chrono::DateTime<chrono::Utc>,
) -> usize {
    // Resolve the sender's groups once so the subject gate can admit a
    // member of the owning group, not only the owning user.
    let sender_groups = enrollment::groups_for(pool, &request.sender_id)
        .await
        .unwrap_or_default();
    let mut applied: Vec<promote::AppliedClosure> = Vec::new();
    for closure in plan_closures {
        let (hit, reason) =
            match validate_closure(closure, recall_hits, &request.sender_id, &sender_groups) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(error = %err, "ingest: closure invalid — skipped");
                    continue;
                },
            };
        if applied.iter().any(|a| a.fact_id == hit.fact_id) {
            continue; // the model repeated a target — first one wins
        }
        // Validate the LLM's `valid_to` the same way the validity-edit path
        // does: a well-formed ISO-8601 bound is kept, an absent/empty one and
        // a MALFORMED one both fall back to this turn's instant. Passing a
        // non-ISO string straight to `close_validity` would poison the stored
        // `valid_to` column with garbage, so a bad date is treated as absent.
        let valid_to =
            match normalize_iso_bound(closure.valid_to.as_deref(), fact_index::DayEdge::End) {
                Ok(Some(iso)) => iso,
                Ok(None) | Err(_) => fact_index::bound_from_instant(turn_now),
            };
        // The fact row first; a miss falls through to the still-buffered
        // capture (the same-day flow). Both misses = the target vanished
        // between recall and now — skip, never fail the turn. No successor
        // pointer here: the classifier does not link a closure to the
        // capture that replaces it (a turn with a true replacement goes
        // through the supersede verb instead).
        let fact_close =
            fact_index::close_validity(pool, &hit.fact_id, &valid_to, reason, None).await;
        let (prev, surface) = match fact_close {
            Ok(Some(prev)) => (prev, promote::ClosureSurface::Fact),
            Ok(None) => {
                match capture_buffer::close_validity(pool, &hit.fact_id, &valid_to, reason).await {
                    Ok(Some(prev)) => (prev, promote::ClosureSurface::Buffer),
                    Ok(None) => {
                        tracing::warn!(
                            fact_id = %hit.fact_id,
                            "ingest: closure target vanished after recall — skipped"
                        );
                        continue;
                    },
                    Err(err) => {
                        tracing::warn!(error = %err, "ingest: buffer closure failed — skipped");
                        continue;
                    },
                }
            },
            Err(err) => {
                tracing::warn!(error = %err, "ingest: fact closure failed — skipped");
                continue;
            },
        };
        tracing::info!(
            fact_id = %hit.fact_id,
            reason,
            valid_to,
            surface = surface.as_str(),
            "ingest: validity CLOSED (closure verb)"
        );
        applied.push(promote::AppliedClosure {
            fact_id: hit.fact_id.clone(),
            wiki_id: hit.wiki_id.clone(),
            preview: truncate(&hit.text, 120),
            valid_to,
            reason: reason.to_owned(),
            prev,
            surface,
        });
    }
    if applied.is_empty() {
        return 0;
    }
    // A closed item on a LIST is shown now, not at the next dream: the page is
    // rebuilt from the facts as they stand, so «ho comprato il latte» carries
    // its `✓` in the same turn that said it. Nothing happens for any other
    // page — a `lista` is the one shape whose maintenance costs no model call,
    // which is the whole reason it does not wait
    // ([`crate::compiler::refresh_list_page`]). Best-effort: a page that
    // cannot be refreshed is a stale render, never a lost closure — the
    // authoritative state is the row, and the compile reconciles it.
    let mut refreshed: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for a in &applied {
        let Ok(Some(row)) = fact_index::find_by_id(pool, &a.fact_id).await else {
            continue;
        };
        if !refreshed.insert((row.wiki_id.clone(), row.source_path.clone())) {
            continue;
        }
        if let Err(e) =
            crate::compiler::refresh_list_page(pool, tree, &row.wiki_id, &row.source_path).await
        {
            tracing::warn!(
                error = %e,
                source_path = %row.source_path,
                "ingest: list page not refreshed after a closure (the compile will)"
            );
        }
    }
    emit_closure_paper_trail(pool, &applied, recall_hits, request).await;
    applied.len()
}

/// The act-first paper trail of a closure batch: ONE born-applied
/// `validity_close` receipt + the `structure_applied` dashboard notice.
///
/// Both best-effort — the closures stand regardless. The addressee
/// follows REM's convention (`recipient_from_fact` on the first closed
/// target): a well-formed Principal wire string, while `applied_by`
/// stays the raw session sender for the audit column.
async fn emit_closure_paper_trail(
    pool: &SqlitePool,
    applied: &[promote::AppliedClosure],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
) {
    let recipient = recall_hits
        .iter()
        .find(|h| h.fact_id == applied[0].fact_id)
        .and_then(|h| proposals::recipient_from_fact(&h.subject_id, h.sender_id.as_ref()));
    let gesture = truncate(&request.text, 160);
    match promote::emit_validity_close_receipt(
        pool,
        applied,
        Some(&gesture),
        Some(request.sender_id.as_str()),
        recipient.clone(),
    )
    .await
    {
        Ok(receipt) => {
            let payload = serde_json::json!({
                "proposal_id": receipt.proposal_id,
                "variant": "validity_close",
                "closed_facts": applied
                    .iter()
                    .map(|c| c.fact_id.as_str())
                    .collect::<Vec<_>>(),
                "recipient_id": recipient,
                "dashboard_path":
                    format!("/dashboard/proposals/{}/open-in-chat", receipt.proposal_id),
            });
            if let Err(err) = events::insert_event(
                pool,
                EventKind::StructureApplied,
                Some(applied[0].wiki_id.as_str()),
                Some(applied[0].fact_id.as_str()),
                &payload,
            )
            .await
            {
                tracing::warn!(error = %err, "ingest: closure notice event failed");
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "ingest: closures applied but receipt failed");
        },
    }
}

/// Emit one [`EventKind::FactMintedForYou`] per beneficiary of this turn
/// — the server half of the consumer-push contract (INTEGRATING step 8).
///
/// A beneficiary is a user principal that OWNS a fact this turn filed
/// while not being the conversation's human (`request.sender_id`). The
/// caller batches per subject, so a turn yields at most one event per
/// recipient no matter how many facts it minted. The payload carries the
/// fact bodies themselves — the delivery ruling (2026-07-23) wants the
/// CONTENT to reach the recipient, not a pointer, so the bridge's agent
/// needs no recall round-trip. Agent principals are skipped here (no
/// inbox — their facts are their own diary), which also covers the
/// `is_agent` lookup failing open. Non-fatal: a failed insert is
/// warn-logged and never demotes the turn.
async fn emit_beneficiary_notices(
    pool: &SqlitePool,
    request: &IngestRequest,
    via_assistant: bool,
    notices: std::collections::BTreeMap<String, Vec<(FactId, WikiId, String)>>,
) {
    for (recipient, facts) in notices {
        if enrollment::is_agent(pool, &recipient)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        let Some((first_id, first_wiki, _)) = facts.first() else {
            continue;
        };
        let payload = serde_json::json!({
            "recipient_id": format!("user:{recipient}"),
            "from_user_id": request.sender_id.as_str(),
            "origin": if via_assistant { "assistant_turn" } else { "user_turn" },
            "facts": facts
                .iter()
                .map(|(id, wiki, body)| {
                    serde_json::json!({
                        "fact_id": id.as_str(),
                        "wiki_id": wiki.as_str(),
                        "body": body,
                    })
                })
                .collect::<Vec<_>>(),
            "dashboard_path": format!("/dashboard/wiki/{}", first_wiki.as_str()),
        });
        if let Err(err) = events::insert_event(
            pool,
            EventKind::FactMintedForYou,
            Some(first_wiki.as_str()),
            Some(first_id.as_str()),
            &payload,
        )
        .await
        {
            tracing::warn!(
                error = %err,
                recipient = recipient.as_str(),
                "ingest: fact-minted-for-you notice failed"
            );
        } else {
            tracing::info!(
                recipient = recipient.as_str(),
                facts = facts.len(),
                "ingest: fact-minted-for-you notice emitted"
            );
        }
    }
}

/// Why a requested validity edit was refused (warn-and-skip — a bad edit
/// never demotes the turn; the twin of [`ClosurePlanError`]).
#[derive(Debug, Error)]
enum ValidityEditPlanError {
    /// The edit carried no `target`.
    #[error("validity_edit has no target")]
    MissingTarget,
    /// `target` is not a well-formed `FactId`.
    #[error("invalid validity_edit target fact_id: {0}")]
    BadFactId(#[from] FactIdParseError),
    /// Anti-hallucination guard: the model may only edit ids it actually
    /// saw in this turn's `recalled_memory`.
    #[error("validity_edit target `{id}` is not in recalled_memory ({available})")]
    TargetNotInRecall { id: String, available: String },
    /// Neither `valid_from` nor `valid_to` was given — nothing to correct.
    #[error("validity_edit gave neither valid_from nor valid_to")]
    NoBounds,
    /// The retraction gate: editing a fact's validity from chat is open to
    /// its subject and to whoever said it. See
    /// [`crate::acl::sender_may_retract`].
    #[error("validity_edit sender is neither the fact's subject nor its author")]
    NotSubjectOrAuthor,
    /// A provided bound did not parse as ISO-8601 / RFC3339.
    #[error("validity_edit date `{0}` is not ISO-8601")]
    BadDate(String),
}

/// Validate one requested validity edit against this turn's recall window.
///
/// Returns the matched [`RecallHit`] plus the two normalized bounds (each
/// `Some(value)` SETS that bound, `None` LEAVES it). Tolerant on which
/// bound is given (at least one required), strict on the anti-hallucination
/// rule, the retraction gate, and date well-formedness.
fn validate_validity_edit<'a>(
    edit: &LlmValidityEdit,
    recall_hits: &'a [RecallHit],
    sender_id: &str,
    sender_groups: &[String],
) -> std::result::Result<(&'a RecallHit, Option<String>, Option<String>), ValidityEditPlanError> {
    let raw = edit
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ValidityEditPlanError::MissingTarget)?;
    let fact_id = FactId::parse(raw)?;
    let Some(hit) = recall_hits.iter().find(|h| h.fact_id == fact_id) else {
        let listed = recall_hits
            .iter()
            .map(|h| h.fact_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ValidityEditPlanError::TargetNotInRecall {
            id: raw.to_owned(),
            available: if listed.is_empty() {
                "<none>".to_owned()
            } else {
                listed
            },
        });
    };
    // The retraction gate: closing a fact's validity withdraws an assertion,
    // so its subject may do it and so may whoever made it. Rewriting and the
    // ACL stay with the subject — see [`crate::acl::sender_may_retract`].
    if !crate::acl::sender_may_retract(
        &hit.subject_id,
        hit.sender_id.as_ref(),
        sender_id,
        sender_groups,
    ) {
        return Err(ValidityEditPlanError::NotSubjectOrAuthor);
    }
    let valid_from = normalize_iso_bound(edit.valid_from.as_deref(), fact_index::DayEdge::Start)?;
    let valid_to = normalize_iso_bound(edit.valid_to.as_deref(), fact_index::DayEdge::End)?;
    if valid_from.is_none() && valid_to.is_none() {
        return Err(ValidityEditPlanError::NoBounds);
    }
    Ok((hit, valid_from, valid_to))
}

/// Trim a bound, treat empty as absent, and require any present value to
/// parse as an RFC3339 UTC instant.
fn normalize_iso_bound(
    raw: Option<&str>,
    edge: fact_index::DayEdge,
) -> std::result::Result<Option<String>, ValidityEditPlanError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    fact_index::canonical_bound(s, edge)
        .map(Some)
        .ok_or_else(|| ValidityEditPlanError::BadDate(s.to_owned()))
}

/// Normalize an LLM-proposed validity bound for a NEW capture row through
/// [`normalize_iso_bound`]. `fact_index` compares validity bounds
/// lexicographically (due-soon range scans, expiry judgements), so an
/// unresolved relative phrase stored verbatim ("domani sera") sorts after
/// every date and never expires. For a capture the correct degraded value
/// is an OPEN bound — not the turn's own instant, which would fabricate a
/// start/expiry the user never stated — so a value that names no date is
/// dropped with a warn instead of killing the fact.
fn normalize_capture_bound(raw: Option<&str>, edge: fact_index::DayEdge) -> Option<String> {
    normalize_iso_bound(raw, edge).unwrap_or_else(|_| {
        tracing::warn!(
            field = match edge {
                fact_index::DayEdge::Start => "valid_from",
                fact_index::DayEdge::End => "valid_to",
            },
            value = raw.unwrap_or_default(),
            "ingest: capture validity bound names no date — stored as open"
        );
        None
    })
}

/// Apply the plan's requested validity edits — the ingest half of the
/// validity-edit verb. The twin of [`apply_plan_closures`], for a
/// *correction* of the dates: stamp the interval act-first (fact row first,
/// then the still-buffered capture — the id is stable across promotion),
/// emit ONE born-applied `validity_edit` receipt, and post the
/// `structure_applied` notice. No topic-widening — these target an explicit
/// recalled fact.
///
/// Every step is soft: an invalid edit, a vanished target, or a DB hiccup
/// is logged and skipped — an edit never kills the turn.
///
/// Returns the number of edits applied.
/// Whether a recalled hit's wiki is a SMART wiki — per-fragment ACL /
/// validity edits are refused on smart wikis (their governance is
/// wiki-level and markerless). Fails closed: a wiki that cannot be resolved is
/// treated as smart, so an edit never mutates a row whose family is
/// unknown.
fn hit_wiki_is_smart(tree: &WikiTree, wiki_id: &str) -> bool {
    WikiId::parse(wiki_id)
        .ok()
        .and_then(|id| tree.locate(&id).ok())
        .is_none_or(|h| h.meta().smart)
}

async fn apply_plan_validity_edits(
    pool: &SqlitePool,
    tree: &WikiTree,
    edits: &[LlmValidityEdit],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
) -> usize {
    // Resolve the sender's groups once so the subject gate can admit a
    // member of an owning group, not just the owning user.
    let sender_groups = enrollment::groups_for(pool, &request.sender_id)
        .await
        .unwrap_or_default();
    let mut applied: Vec<promote::AppliedValidityEdit> = Vec::new();
    for edit in edits {
        let (hit, valid_from, valid_to) = match validate_validity_edit(
            edit,
            recall_hits,
            request.sender_id.as_str(),
            &sender_groups,
        ) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, "ingest: validity_edit invalid — skipped");
                continue;
            },
        };
        if applied.iter().any(|a| a.fact_id == hit.fact_id) {
            continue; // the model repeated a target — first one wins
        }
        // 6j.4: smart wikis have no per-fragment validity — their facts are
        // content-indexed section rows governed at the wiki level. Refuse
        // here (the dashboard twin gates this via `enforce_standard_wiki`).
        if hit_wiki_is_smart(tree, &hit.wiki_id) {
            tracing::warn!(
                fact_id = %hit.fact_id,
                wiki_id = %hit.wiki_id,
                "ingest: validity_edit targets a smart wiki — skipped (per-fragment validity is standard-wikis only)"
            );
            continue;
        }
        let fact_edit = fact_index::set_validity(
            pool,
            &hit.fact_id,
            valid_from.as_deref(),
            valid_to.as_deref(),
        )
        .await;
        let (prev, surface) = match fact_edit {
            Ok(Some(prev)) => (prev, promote::ClosureSurface::Fact),
            Ok(None) => {
                match capture_buffer::set_validity(
                    pool,
                    &hit.fact_id,
                    valid_from.as_deref(),
                    valid_to.as_deref(),
                )
                .await
                {
                    Ok(Some(prev)) => (prev, promote::ClosureSurface::Buffer),
                    Ok(None) => {
                        tracing::warn!(
                            fact_id = %hit.fact_id,
                            "ingest: validity_edit target vanished after recall — skipped"
                        );
                        continue;
                    },
                    Err(err) => {
                        tracing::warn!(error = %err, "ingest: buffer validity_edit failed — skipped");
                        continue;
                    },
                }
            },
            Err(err) => {
                tracing::warn!(error = %err, "ingest: fact validity_edit failed — skipped");
                continue;
            },
        };
        tracing::info!(
            fact_id = %hit.fact_id,
            ?valid_from,
            ?valid_to,
            surface = surface.as_str(),
            "ingest: validity EDITED (validity_edit verb)"
        );
        applied.push(promote::AppliedValidityEdit {
            fact_id: hit.fact_id.clone(),
            wiki_id: hit.wiki_id.clone(),
            preview: truncate(&hit.text, 120),
            new_valid_from: valid_from,
            new_valid_to: valid_to,
            prev,
            surface,
        });
    }
    if applied.is_empty() {
        return 0;
    }
    emit_validity_edit_paper_trail(pool, &applied, recall_hits, request).await;
    applied.len()
}

/// The act-first paper trail of a validity-edit batch: ONE born-applied
/// `validity_edit` receipt + the `structure_applied` dashboard notice.
/// Both best-effort — the edits stand regardless.
async fn emit_validity_edit_paper_trail(
    pool: &SqlitePool,
    applied: &[promote::AppliedValidityEdit],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
) {
    let recipient = recall_hits
        .iter()
        .find(|h| h.fact_id == applied[0].fact_id)
        .and_then(|h| proposals::recipient_from_fact(&h.subject_id, h.sender_id.as_ref()));
    let gesture = truncate(&request.text, 160);
    match promote::emit_validity_edit_receipt(
        pool,
        applied,
        Some(&gesture),
        Some(request.sender_id.as_str()),
        recipient.clone(),
    )
    .await
    {
        Ok(receipt) => {
            let payload = serde_json::json!({
                "proposal_id": receipt.proposal_id,
                "variant": "validity_edit",
                "edited_facts": applied
                    .iter()
                    .map(|e| e.fact_id.as_str())
                    .collect::<Vec<_>>(),
                "recipient_id": recipient,
                "dashboard_path":
                    format!("/dashboard/proposals/{}/open-in-chat", receipt.proposal_id),
            });
            if let Err(err) = events::insert_event(
                pool,
                EventKind::StructureApplied,
                Some(applied[0].wiki_id.as_str()),
                Some(applied[0].fact_id.as_str()),
                &payload,
            )
            .await
            {
                tracing::warn!(error = %err, "ingest: validity_edit notice event failed");
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "ingest: validity_edits applied but receipt failed");
        },
    }
}

/// Why a requested ACL change was refused (warn-and-skip — a bad change
/// never demotes the turn; the twin of [`ClosurePlanError`]).
#[derive(Debug, Error)]
enum AclChangePlanError {
    /// The change carried no `target`.
    #[error("acl_change has no target")]
    MissingTarget,
    /// `target` is not a well-formed `FactId`.
    #[error("invalid acl_change target fact_id: {0}")]
    BadFactId(#[from] FactIdParseError),
    /// Anti-hallucination guard: the model may only change ids it actually
    /// saw in this turn's `recalled_memory`.
    #[error("acl_change target `{id}` is not in recalled_memory ({available})")]
    TargetNotInRecall { id: String, available: String },
    /// The subject gate: only the fact's subject may change its ACL from chat.
    #[error("acl_change sender is not the fact's subject")]
    NotSubject,
    /// A principal wire string (`subject_id` or one of `allow_ids`) did not
    /// parse.
    #[error("acl_change bad principal: {0}")]
    BadPrincipal(#[from] PrincipalParseError),
}

/// Validate one requested ACL change against this turn's recall window.
///
/// Returns the matched [`RecallHit`], the new subject (defaulting to the
/// existing subject when the LLM omits one), and the new allow-list. Strict
/// on the anti-hallucination rule, the subject gate, and principal
/// well-formedness.
fn validate_acl_change<'a>(
    change: &LlmAclChange,
    recall_hits: &'a [RecallHit],
    sender_id: &str,
    sender_groups: &[String],
) -> std::result::Result<(&'a RecallHit, Principal, Vec<Principal>), AclChangePlanError> {
    let raw = change
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(AclChangePlanError::MissingTarget)?;
    let fact_id = FactId::parse(raw)?;
    let Some(hit) = recall_hits.iter().find(|h| h.fact_id == fact_id) else {
        let listed = recall_hits
            .iter()
            .map(|h| h.fact_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AclChangePlanError::TargetNotInRecall {
            id: raw.to_owned(),
            available: if listed.is_empty() {
                "<none>".to_owned()
            } else {
                listed
            },
        });
    };
    // The subject gate: only a subject changes their fact's ACL from chat —
    // the owning user, or a member of the owning group.
    if !crate::acl::sender_is_subject(&hit.subject_id, sender_id, sender_groups) {
        return Err(AclChangePlanError::NotSubject);
    }
    // Default to keeping the existing subject when the LLM omits it.
    let new_subject = match change
        .subject_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.parse::<Principal>()?,
        None => hit.subject_id.clone(),
    };
    let new_allow = change
        .allow_ids
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::parse::<Principal>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((hit, new_subject, new_allow))
}

/// Apply the plan's requested ACL changes — the ingest half of the
/// acl-change verb. The twin of [`apply_plan_closures`], but it also
/// computes the disclosure-widening signal, writes a
/// [`crate::disclosure_audit`] row per change, and threads the returned
/// `audit_id` into the receipt so the change and its audit row line up.
///
/// Every step is soft: an invalid change, a vanished target, or a DB hiccup
/// is logged and skipped — a change never kills the turn.
///
/// Returns the number of changes applied.
#[allow(
    clippy::too_many_lines,
    reason = "validate + smart-guard + act-first stamp + widening/audit + receipt live as one act-first orchestrator, mirroring apply_plan_closures"
)]
async fn apply_plan_acl_changes(
    pool: &SqlitePool,
    tree: &WikiTree,
    changes: &[LlmAclChange],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
) -> usize {
    // Resolve the sender's groups once so the subject gate can admit a
    // member of an owning group, not just the owning user.
    let sender_groups = enrollment::groups_for(pool, &request.sender_id)
        .await
        .unwrap_or_default();
    let mut applied: Vec<promote::AppliedAclChange> = Vec::new();
    for change in changes {
        let (hit, new_subject, new_allow) = match validate_acl_change(
            change,
            recall_hits,
            request.sender_id.as_str(),
            &sender_groups,
        ) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, "ingest: acl_change invalid — skipped");
                continue;
            },
        };
        if applied.iter().any(|a| a.fact_id == hit.fact_id) {
            continue; // the model repeated a target — first one wins
        }
        // 6j.4: smart wikis have no per-fragment ACL — their governance is
        // wiki-level (markerless). Refuse here so no fact_index row is
        // mutated and no disclosure_audit row is written (the dashboard
        // twin gates this via `enforce_standard_wiki`).
        if hit_wiki_is_smart(tree, &hit.wiki_id) {
            tracing::warn!(
                fact_id = %hit.fact_id,
                wiki_id = %hit.wiki_id,
                "ingest: acl_change targets a smart wiki — skipped (per-fragment ACL is standard-wikis only; smart governance is wiki-level)"
            );
            continue;
        }
        // The chat verb changes subject/allow only — the fact's cross-user
        // attribution (`sender`, who captured it) is PRESERVED, so the
        // capturer keeps the read shortcut. The widening signal is computed
        // against the PREVIOUS read-set, returned by set_acl.
        let keep_sender = hit.sender_id.as_ref();
        let fact_set =
            fact_index::set_acl(pool, &hit.fact_id, &new_subject, &new_allow, keep_sender).await;
        let (prev, surface) = match fact_set {
            Ok(Some(prev)) => (prev, promote::ClosureSurface::Fact),
            Ok(None) => {
                match capture_buffer::set_acl(
                    pool,
                    &hit.fact_id,
                    &new_subject,
                    &new_allow,
                    keep_sender,
                )
                .await
                {
                    Ok(Some(prev)) => (prev, promote::ClosureSurface::Buffer),
                    Ok(None) => {
                        tracing::warn!(
                            fact_id = %hit.fact_id,
                            "ingest: acl_change target vanished after recall — skipped"
                        );
                        continue;
                    },
                    Err(err) => {
                        tracing::warn!(error = %err, "ingest: buffer acl_change failed — skipped");
                        continue;
                    },
                }
            },
            Err(err) => {
                tracing::warn!(error = %err, "ingest: fact acl_change failed — skipped");
                continue;
            },
        };
        let widening = acl::widens(
            &prev.prev_subject_id,
            &prev.prev_allow_ids,
            &new_subject,
            &new_allow,
        );
        let audit_id = match disclosure_audit::record(
            pool,
            &hit.fact_id,
            &hit.wiki_id,
            request.sender_id.as_str(),
            &prev,
            &new_subject,
            &new_allow,
            keep_sender,
            widening,
        )
        .await
        {
            Ok(id) => id,
            Err(err) => {
                // The ACL is already changed; a missing audit row must not
                // strand the change. Log loudly and proceed without the
                // audit anchor (-1 sentinel — nothing downstream looks it up).
                tracing::error!(error = %err, "ingest: acl_change applied but audit row failed");
                -1
            },
        };
        tracing::info!(
            fact_id = %hit.fact_id,
            subject = %new_subject,
            widening,
            surface = surface.as_str(),
            "ingest: ACL CHANGED (acl_change verb)"
        );
        applied.push(promote::AppliedAclChange {
            fact_id: hit.fact_id.clone(),
            wiki_id: hit.wiki_id.clone(),
            preview: truncate(&hit.text, 120),
            new_subject,
            new_allow,
            prev,
            audit_id,
            widening,
            surface,
        });
    }
    if applied.is_empty() {
        return 0;
    }
    emit_acl_change_paper_trail(pool, &applied, recall_hits, request).await;
    applied.len()
}

/// The act-first paper trail of an ACL-change batch: ONE born-applied
/// `acl_change` receipt + the `structure_applied` dashboard notice. Both
/// best-effort — the changes stand regardless.
async fn emit_acl_change_paper_trail(
    pool: &SqlitePool,
    applied: &[promote::AppliedAclChange],
    recall_hits: &[RecallHit],
    request: &IngestRequest,
) {
    let recipient = recall_hits
        .iter()
        .find(|h| h.fact_id == applied[0].fact_id)
        .and_then(|h| proposals::recipient_from_fact(&h.subject_id, h.sender_id.as_ref()));
    let gesture = truncate(&request.text, 160);
    match promote::emit_acl_change_receipt(
        pool,
        applied,
        Some(&gesture),
        Some(request.sender_id.as_str()),
        recipient.clone(),
    )
    .await
    {
        Ok(receipt) => {
            let payload = serde_json::json!({
                "proposal_id": receipt.proposal_id,
                "variant": "acl_change",
                "changed_facts": applied
                    .iter()
                    .map(|c| c.fact_id.as_str())
                    .collect::<Vec<_>>(),
                "widening": applied.iter().any(|c| c.widening),
                "recipient_id": recipient,
                "dashboard_path":
                    format!("/dashboard/proposals/{}/open-in-chat", receipt.proposal_id),
            });
            if let Err(err) = events::insert_event(
                pool,
                EventKind::StructureApplied,
                Some(applied[0].wiki_id.as_str()),
                Some(applied[0].fact_id.as_str()),
                &payload,
            )
            .await
            {
                tracing::warn!(error = %err, "ingest: acl_change notice event failed");
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "ingest: acl_changes applied but receipt failed");
        },
    }
}

fn parse_intent(s: &str) -> IntentKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "capture" => IntentKind::Capture,
        "recall" => IntentKind::Recall,
        "structural" => IntentKind::Structural,
        _ => IntentKind::Skip,
    }
}

/// Extract the first `{ ... }` JSON object from `raw` and deserialize.
/// LLMs reliably wrap JSON in markdown fences or prose; this scanner
/// matches the outermost balanced braces and ignores everything else.
fn parse_plan(raw: &str) -> Option<LlmIngestPlan> {
    parse_first_json(raw)
}

/// The generic half of [`parse_plan`]: locate the outermost balanced
/// `{ ... }` in `raw` and deserialize it as `T`. Shared with the closure
/// confirmer's reply parsing.
pub(crate) fn parse_first_json<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &raw[start..=i];
                    return serde_json::from_str::<T>(slice).ok();
                }
            },
            _ => {},
        }
    }
    None
}

// ---------- Internal: prompt building ----------

/// Bundled default for the `ingest` system prompt.
///
/// Used by the hybrid loader [`prompts::load`] when no operator
/// override sits at `<workdir>/prompts/ingest.md`. The verbatim
/// prompt body lives in `crates/mwe-core/prompts/ingest.md`
/// (frontmatter + a single ```text ... ``` fenced block); see
/// ingest pipeline for the
/// design narrative and version history. Referenced from [`prompts::BUNDLED`] so
/// `mwe-mcp init` materialises it under the workdir.
pub const BUNDLED_INGEST_PROMPT_MD: &str = include_str!("../prompts/ingest.md");

/// The `ingest` classifier's own-turn rules (`ingest-assistant-turn.md`).
///
/// A **part** of that prompt: it carries what applies when the agent feeds
/// back its own prior reply, and it is appended to the turn context only when
/// the turn is one.
///
/// Not a section of `ingest.md` because a normal turn has no use for it: nine
/// thousand characters, on one turn in five, in front of the smallest model in
/// the fleet. Not appended to the SYSTEM half either — that half is
/// byte-identical call to call, which is the whole reason the prompt cache
/// serves two thirds of this slot's tokens, and a conditional block there
/// would split one warm prefix into two cold ones.
pub const BUNDLED_INGEST_ASSISTANT_TURN_MD: &str =
    include_str!("../prompts/ingest-assistant-turn.md");

/// The `ingest` classifier's attachment rules (`ingest-attachments.md`).
///
/// A **part** of that prompt on the same terms as
/// [`BUNDLED_INGEST_ASSISTANT_TURN_MD`]: claiming and describing media is
/// wanted by the turns that carry any, which is about one in five, and it
/// rides the turn so the cacheable system half stays untouched.
pub const BUNDLED_INGEST_ATTACHMENTS_MD: &str = include_str!("../prompts/ingest-attachments.md");

/// Bundled default for the closure-confirmer prompt.
///
/// The topic-focused second recall pass of a closure-bearing turn —
/// see [`confirm_topic_closures`]. Operator override:
/// `<workdir>/prompts/ingest-closures.md`.
pub const BUNDLED_INGEST_CLOSURES_MD: &str = include_str!("../prompts/ingest-closures.md");

/// Bundled system prompt of the **reconciliation stage**
/// ([`reconcile_after_reading`]). Operator override:
/// `<workdir>/prompts/ingest-reconcile.md`.
pub const BUNDLED_INGEST_RECONCILE_MD: &str = include_str!("../prompts/ingest-reconcile.md");

/// Resource bound on the reconciliation stage's candidate set.
///
/// Not a semantic limit: the point of the stage is to be shown the opened
/// pages COMPLETE, so this exists only to keep one pathological turn from
/// rendering a thousand lines into a prompt. [`recall::facts_on_pages`] logs
/// when its own share of it bites.
const RECONCILE_CANDIDATE_CAP: usize = 120;

/// Standing directive returned on the `rules` channel for every turn of the
/// builtin `guest` pseudo-identity (the unidentified-human sender).
///
/// A guest turn is ephemeral by construction — the orchestrator files
/// nothing — so the consumer agent must neither promise memory nor treat
/// the speaker as an enrolled user. Fixed prose, not a wiki-recalled rule:
/// guest has no wiki to hold one.
const GUEST_RULES_NOTICE: &str = "UNIDENTIFIED SPEAKER (guest). This turn comes from a person \
the deployment could not identify. Behave reservedly: do not disclose personal or household \
information beyond what this turn's recalled context (public memory only) already shows, do \
not act or take commitments on behalf of enrolled users, and do not promise to remember \
anything — nothing from this turn is stored. If this person should be remembered, an admin \
can enroll them from the dashboard and delegate their identity to this consumer.";

/// Resource cap on the classifier's `closure_topics` — how many focused
/// re-recall queries one turn may spend. A cap on cost, not on judgment:
/// which candidates close stays the confirmer LLM's call.
const CLOSURE_TOPICS_CAP: usize = 3;

/// Emit the `current_time:` anchor — THIS turn's reference instant the
/// classifier resolves relative dates against ("giovedì"/"domani"/"tra due
/// settimane" → a concrete ISO datetime), load-bearing for a `wiki-cron`
/// `due_at`. UTC ISO-8601 to the second + the English weekday name (so
/// "giovedì prossimo" is computable). `now` is passed in (not read from
/// `Utc::now()`) so [`build_prompt`] stays deterministic and unit-testable.
///
/// When the deployment declares its users' local `timezone` (IANA name, e.g.
/// `Europe/Rome`), a second `user_timezone:` line names it. `current_time`
/// stays UTC; the prompt's time rule then tells the classifier to read a bare
/// wall-clock time the user speaks ("alle 16") in THAT zone and convert it to
/// UTC — instead of stamping the local hour verbatim as UTC, a systematic
/// +offset error on every dated commitment. The DST-aware conversion is left
/// to the classifier's timezone knowledge; no tz database is compiled in.
fn push_reference_time(
    out: &mut String,
    now: chrono::DateTime<chrono::Utc>,
    timezone: Option<&str>,
) {
    let _ = writeln!(
        out,
        "current_time: {} ({})",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        now.format("%A")
    );
    if let Some(tz) = timezone {
        let _ = writeln!(out, "user_timezone: {tz}");
    }
}

/// How a recalled fact stands in time, relative to THIS turn's reference
/// instant — the line the classifier was missing.
///
/// The engine has always known this: `fact_index` stores both bounds, recall
/// carries them onto every [`crate::recall::RecallHit`], and the ranker
/// already down-ranks a closed window
/// ([`crate::recall::CLOSED_WINDOW_DOWNRANK`]). It just never told the model,
/// so a spent fact arrived in `recalled_memory` looking exactly like a durable
/// one and read as the present.
///
/// The failure that produced this (demo corpus, 2026-03-26): *"Zoe is away
/// until Wednesday 26 March. Alice and Bob are dining together during this
/// period"* — `valid_to` 26 March 00:00 — was recalled for a turn at 21:30 on
/// the 26th, twenty-one hours after it stopped holding. The classifier reused
/// its "Alice and Bob" for a message that said *"we ate without him"* and
/// wrote **"Alice and Bob ate dinner without waiting for Bob"**. The detail
/// was not invented; it was imported from a fact that had expired.
///
/// Returns `None` for a fact that makes no time claim (no bounds, or an
/// in-force window with no end), so the prompt stays byte-identical for the
/// durable majority.
fn validity_status(
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let from = valid_from.and_then(parse);
    let to = valid_to.and_then(parse);
    if let Some(end) = to
        && end <= now
    {
        return Some(format!("ENDED {} — history, not the present", day_of(end)));
    }
    if let Some(start) = from
        && start > now
    {
        return Some(format!("STARTS {} — not in force yet", day_of(start)));
    }
    to.map(|end| format!("in force until {}", day_of(end)))
}

/// `YYYY-MM-DD` of an instant — the granularity a classifier reasons in.
fn day_of(t: chrono::DateTime<chrono::FixedOffset>) -> String {
    t.format("%Y-%m-%d").to_string()
}

/// Log a prompt list that the cap actually cut.
///
/// Silence here is the defect: a truncated block reads to the model exactly
/// like a complete one, so the operator has to be able to see it happened.
fn warn_if_capped(kind: &str, total: usize, cap: usize) {
    if total > cap {
        tracing::warn!(
            kind,
            total,
            cap,
            dropped = total - cap,
            "ingest: prompt list truncated — entries the turn did not name were cut"
        );
    }
}

/// How many named things the classifier is shown at once.
///
/// The block is a roster, not a corpus: it exists so a name already decided is
/// copied rather than judged again, and a name the turn does not mention costs
/// the model attention for nothing. Ordered commonest-first, so what falls off
/// the end is what the memory barely knows — and a fact about it is a first
/// decision anyway, which is the case the roster cannot help with.
pub(crate) const KNOWN_ENTITIES_CAP: i64 = 60;

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // a sequential context-bundle builder; one block per section reads top-to-bottom, splitting hides the layout — and each per-sender input is one section, so the argument list IS the section list
fn build_prompt(
    request: &IngestRequest,
    recall_hits: &[RecallHit],
    list_pages: &[fact_index::ListPage],
    sender_groups: &[(String, Option<String>)],
    known_users: &[enrollment::EnrolledUserLite],
    known_entities: &[fact_index::KnownEntity],
    sender_rules: Option<&str>,
    sender_timezone: Option<&str>,
    language_directive: &str,
    parti: &[&str],
    now: chrono::DateTime<chrono::Utc>,
    policy: &IngestPolicy,
) -> String {
    let mut out = String::with_capacity(2_048);
    // The language the fact bodies are written in, at the TOP of the turn.
    //
    // It is also the last line of the system prompt, where the prompt document
    // declares it — but that document is ~110 000 characters and this slot is
    // the small one, and a directive that far from the work is not acted on.
    // Measured on one Italian turn repeated from a clean memory: written only
    // in the system prompt, 0 of 4 facts came back in Italian; repeated at the
    // top of the system prompt, 1 of 4; carried on the turn, 4 of 4. The rule
    // is stated once and obeyed here (founder's call, 2026-08-25: put it where
    // the person's own words are).
    out.push_str(language_directive);
    out.push_str("\n\n");
    // The parts this turn needs, above everything else about it. They open the
    // turn for the same reason the language does: they govern what follows and
    // they are what this turn is. A turn that needs none carries none.
    for parte in parti {
        out.push_str(parte);
        out.push_str("\n\n");
    }
    out.push_str("sender_id: ");
    out.push_str(&request.sender_id);
    out.push_str("\ncontext_hint: ");
    out.push_str(request.context_hint.as_str());
    out.push('\n');
    // author: who wrote `text` this turn. The default `user` path stays silent
    // — the whole prompt already assumes a user message, and emitting nothing
    // keeps that 99% path byte-identical. When the consumer agent feeds back
    // its OWN prior reply the line flips to `assistant`, and the rules for
    // that case are already above: `own_turn_rules` opened this turn, so the
    // line states the fact and points at nothing.
    if request.author == MessageRole::Assistant {
        out.push_str(
            "author: assistant\n\
             # THIS TURN'S `text` IS YOUR OWN PRIOR REPLY, not a user message — \
             the rules that opened this turn govern it.\n",
        );
    }
    // Reference-time zone, most specific wins: the sender's own zone
    // (enrollment) over the deployment default (`recall.ingest_timezone`).
    push_reference_time(
        &mut out,
        now,
        sender_timezone.or(policy.ingest_timezone.as_deref()),
    );
    if let Some(choice) = &request.disambig_choice {
        out.push_str("disambig_choice: ");
        out.push_str(choice);
        out.push_str(
            "\n# The user resolved the prior turn's ambiguity to this candidate. \
             Commit to it — do not set needs_disambig=true on this turn.\n",
        );
    }

    // sender_groups: the groups the sender belongs to, each with the
    // operator-written `scope` prose. This is the context the `subject_id`
    // decision routes on — without
    // it the classifier is blind to the group domain and falls back to
    // a private capture.
    out.push_str("\nsender_groups:\n");
    if sender_groups.is_empty() {
        out.push_str("  (none)\n");
    } else {
        // NOT cut. `MAX_GROUPS_PER_USER` refuses the ninth group where it is
        // created (`enrollment::mirror_to_db`, 2026-08-09), and a product
        // limit is enforced there, "never by cutting the list when the prompt
        // is built" (founder). The cut this replaced was worse than
        // redundant: the builtin `global` row is appended to every sender and
        // is not counted by that refusal, so a user in the maximum legal 8
        // groups lost one on every turn — the one that sorted last, silently,
        // and with it the group domain that fact should have been routed to.
        for (id, scope) in sender_groups {
            out.push_str("  - id: ");
            out.push_str(id);
            out.push_str("\n    scope: ");
            match scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(s) => out.push_str(&truncate(s, policy.max_group_scope_chars)),
                None => out.push_str("(no scope configured)"),
            }
            out.push('\n');
        }
    }

    // sender_rules: the sender's own standing policy (their `@rules.md`).
    // The classifier honours the privacy/sharing rules here when it
    // decides each fact's `subject_id`/`allow_ids` (e.g. "keep health private" →
    // subject-only), and surfaces the behaviour rules to the consumer. Absent →
    // "(none)": decide ACL as before. No hard gate — an aid to the decision.
    //
    // **Rendered whole.** The prompt calls this the sender's policy «in full»,
    // under the standing rule that a slot may act against a set it is shown
    // COMPLETE and never against a sample — and this is the block that carries
    // governance. Cutting it at `max_sender_rules_chars` would drop a rule
    // mid-word with nothing logged: a user whose `@rules.md` runs past 1 500
    // characters, and whose last line is *«i fatti sulla mia salute restano
    // privati»*, would lose that rule while the prompt tells the model it has
    // seen everything. A rules file is written by a person and is short; the
    // number stays as the point where an unusual one is worth saying out loud.
    out.push_str("\nsender_rules:\n");
    match sender_rules.map(str::trim).filter(|s| !s.is_empty()) {
        Some(rules) => {
            if rules.chars().count() > policy.max_sender_rules_chars {
                tracing::warn!(
                    chars = rules.chars().count(),
                    expected_under = policy.max_sender_rules_chars,
                    "ingest: sender rules are unusually long — shown in full, but they cost every turn"
                );
            }
            out.push_str(rules);
            out.push('\n');
        },
        None => out.push_str("  (none)\n"),
    }

    // known_users: the enrolled people the classifier can attribute facts to
    // by canonical name. A message from one user about another ("Bob
    // prefers tea") routes `subject_id` to the named person via this roster
    // rather than filing it under the sender. Aliases let the model resolve
    // informal references to the canonical `user_id`.
    //
    // The assistant is in this roster too — it is an enrolled user like any
    // other (the diagonal identity model) — and `is_agent: true` says which
    // entry it is. Without it the classifier reads its own id as one more
    // person: the "you" of every turn is then a stranger in the list, and a
    // sentence addressed TO the assistant looks like a sentence ABOUT a
    // third party. Emitted only when set, like `available_wikis` below.
    out.push_str("\nknown_users:\n");
    if known_users.is_empty() {
        out.push_str("  (none)\n");
    } else {
        // NOT cut, for the same reason as `sender_groups` above:
        // `MAX_ENROLLED_USERS` refuses the 25th user at enrolment. The cut
        // here was alphabetical, so it hid whoever sorts late — and losing a
        // person from this roster does not lose the fact, it files the fact
        // about them under the sender instead, which is a wrong answer that
        // looks like a right one.
        for u in known_users {
            out.push_str("  - id: ");
            out.push_str(&u.user_id);
            if !u.aliases.is_empty() {
                out.push_str("\n    aliases: ");
                out.push_str(&u.aliases.join(", "));
            }
            if u.is_agent {
                out.push_str("\n    is_agent: true");
            }
            out.push('\n');
        }
    }

    // known_entities: the named things this memory already holds facts about,
    // each with the principal its facts are filed under. `known_users` is the
    // roster of principals; this is the roster of everything else, and it is
    // here for one reason: deciding who answers for a fact about a
    // non-principal is a judgement, and a judgement re-made every turn drifts.
    // Shown the answer already given, the classifier copies it.
    out.push_str("\nknown_entities:\n");
    if known_entities.is_empty() {
        out.push_str("  (none yet)\n");
    } else {
        for e in known_entities {
            out.push_str("  - name: ");
            out.push_str(&e.name);
            out.push_str("\n    subject_id: ");
            out.push_str(&e.subject_id);
            out.push_str("\n    facts: ");
            out.push_str(&e.facts.to_string());
            out.push('\n');
        }
    }

    // list_pages: the lists that already exist and the sender may add to.
    // The classifier is shown NO wikis — a wiki is an address the engine
    // derives from the fact's subject ([`derive_target_wiki`]) — and exactly
    // one placement surface: the file names of the open lists. That is the
    // one write that cannot wait for consolidation, and the one name the
    // model has no other way to learn: a recalled fact carries its wiki, its
    // subject and its audience, never the page it sits on. Without this block
    // "add detergent to the shopping list" invents a name and mints a second
    // shopping list in front of the user.
    out.push_str("\nlist_pages:\n");
    if list_pages.is_empty() {
        out.push_str("  (none yet)\n");
    } else {
        // Same class again. It matters more here than anywhere: a list the
        // classifier cannot see is a list it mints a SECOND copy of, live, in
        // front of the user — which is the exact failure this inventory exists
        // to prevent.
        warn_if_capped(
            "list_pages",
            list_pages.len(),
            policy.max_list_pages_in_prompt,
        );
        for l in list_pages.iter().take(policy.max_list_pages_in_prompt) {
            out.push_str("  - page: ");
            out.push_str(&l.page);
            if let Some(d) = l
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                out.push_str("\n    holds: ");
                out.push_str(&truncate(d, policy.max_group_scope_chars));
            }
            out.push('\n');
        }
    }

    out.push_str("\nrecent_messages:\n");
    if request.recent_messages.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let take_from = request
            .recent_messages
            .len()
            .saturating_sub(policy.max_recent_messages);
        for m in &request.recent_messages[take_from..] {
            out.push_str("  - role: ");
            out.push_str(m.role.as_str());
            if let Some(ts) = &m.timestamp {
                out.push_str("\n    ts: ");
                out.push_str(ts);
            }
            out.push_str("\n    text: ");
            out.push_str(&truncate(&m.text, policy.max_recent_message_chars));
            out.push('\n');
        }
    }

    out.push_str("\nrecalled_memory:\n");
    if recall_hits.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for h in recall_hits {
            out.push_str("  - fact_id: ");
            out.push_str(h.fact_id.as_str());
            out.push_str("\n    wiki_id: ");
            out.push_str(&h.wiki_id);
            // subject (the subject) + allow (the current audience) so the
            // classifier can tell which facts the sender owns and faithfully
            // reproduce the read-set on a REPLACE-semantics `acl_change` /
            // inherit it on a supersede. Without these the model is blind to
            // the current ACL and silently drops allow principals.
            out.push_str("\n    subject: ");
            out.push_str(&h.subject_id.to_string());
            out.push_str("\n    allow: ");
            if h.allow_ids.is_empty() {
                out.push_str("(none)");
            } else {
                let joined = h
                    .allow_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&joined);
            }
            // validity: where this fact stands in time against THIS turn's
            // reference instant. Omitted for a fact that makes no time claim,
            // so the durable majority renders exactly as before.
            if let Some(status) =
                validity_status(h.valid_from.as_deref(), h.valid_to.as_deref(), now)
            {
                out.push_str("\n    validity: ");
                out.push_str(&status);
            }
            out.push_str("\n    score: ");
            let _ = write!(out, "{:.3}", h.score);
            out.push_str("\n    text: ");
            out.push_str(&truncate(&h.text, policy.max_recent_message_chars));
            out.push('\n');
        }
    }

    // attachments: media riding this turn (uploaded out of band). Only
    // emitted when present so text-only prompts stay byte-identical.
    // The classifier claims each id in an extraction's `attachments`
    // array; image bytes for undescribed photos ride the same call as
    // inline image parts.
    if !request.attachments.is_empty() {
        out.push_str("\nattachments:\n");
        for a in &request.attachments {
            out.push_str("  - catalog_id: ");
            out.push_str(a.catalog_id.as_str());
            out.push_str("\n    kind: ");
            out.push_str(&a.kind);
            if let Some(c) = a
                .caption
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.push_str("\n    caption: ");
                out.push_str(&truncate(c, policy.max_recent_message_chars));
            }
            if let Some(d) = a
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.push_str("\n    description: ");
                out.push_str(&truncate(d, policy.max_recent_message_chars));
            }
            out.push('\n');
        }
    }

    out.push_str("\ncurrent_message: ");
    out.push_str(&request.text);
    out.push('\n');
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.replace('\n', " ");
    }
    let mut acc = String::with_capacity(max_chars + 1);
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            acc.push('…');
            break;
        }
        acc.push(if c == '\n' { ' ' } else { c });
    }
    acc
}

/// Compose one role-labelled recall-block section: the `header` line, then
/// one `- ` bullet per item on its own line, **whole bullets** fitted
/// against `max_chars` (header included in the count). Items are taken in
/// the given order — the callers pass newest-first, so what falls off when
/// the budget fills is the oldest tail — and the fit stops at the first
/// bullet that does not fit (a prefix cut, never a mid-word cut). The one
/// exception: a *first* bullet that alone exceeds the whole budget is
/// char-truncated with an ellipsis, so a section with content is never
/// rendered empty. `None` when no item survives trimming — the section is
/// omitted entirely, header included (the empty-section contract).
///
/// This is the injected-block sibling of [`truncate`], which flattens
/// newlines for one-line *prompt* fields (`sender_rules`, recent messages)
/// and stays in use there; recall-block sections keep their line structure.
fn fit_bullets<'a, I>(header: &str, items: I, max_chars: usize) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out = String::from(header);
    let mut used = header.chars().count();
    let mut any = false;
    for item in items {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let cost = item.chars().count() + 3; // "\n- "
        if used + cost > max_chars {
            if !any {
                // Pathological first bullet longer than the whole budget:
                // keep the section non-empty rather than dropping it.
                out.push_str("\n- ");
                let room = max_chars.saturating_sub(used + 3).max(1);
                out.extend(item.chars().take(room));
                out.push('…');
                any = true;
            }
            break;
        }
        out.push_str("\n- ");
        out.push_str(item);
        used += cost;
        any = true;
    }
    any.then_some(out)
}

// ---------- Internal: available-wikis enumeration ----------

#[derive(Debug, Clone)]
pub(crate) struct AvailableWiki {
    pub(crate) wiki_id: String,
    pub(crate) title: String,
    pub(crate) wiki_type: String,
    /// The wiki's `scope` prose — the category's **authored intent**, "what
    /// goes in here", surfaced to the classifier as a placement **and**
    /// audience signal (the wiki's `scope` is an `allow_ids` input
    /// alongside the group `scope`). `None` for a wiki with no
    /// description configured — which today is every standard wiki:
    /// `create_identity_wiki` and both emergence paths stamp `scope: None`,
    /// and the only writer is the smart-wiki door sign, a family this window
    /// never carries. It is kept because a hand-authored `_meta.md` round-trips
    /// it verbatim, and because intent and content are different claims — see
    /// [`Self::summary`].
    pub(crate) scope: Option<String>,
    /// The wiki's `_meta.extra["summary"]` — the compiled **abstract**, "what
    /// it actually holds", written by the compiler from the card its own
    /// foundation page was written with (and seeded at emergence from the
    /// promoting model's description).
    ///
    /// The twin of [`Self::scope`] and deliberately NOT merged into it: this
    /// one is machine-written and rewritten on every compile of the wiki's
    /// foundation page, so folding it onto `scope` would have the nightly
    /// compile overwrite a human's authored line. It is the field that
    /// actually carries a description today, and the only description an
    /// **emerged** wiki has — the case where the wiki id alone says least,
    /// because it names a topic rather than an enrolled principal.
    pub(crate) summary: Option<String>,
    /// Per-wiki `smart` flag read straight from `_meta.md`. A smart
    /// wiki is smart-consumer-owned (`wiki_admin_*`, never
    /// `wiki_ingest_message`) so it is hidden from the router window and
    /// never buffered; everything else is the standard-wiki path the
    /// narrative compiler writes.
    pub(crate) smart: bool,
    /// Per-wiki `is_agent` flag from `_meta.md`: true for an AGENT's own wiki
    /// (a `wiki-user` stamped `is_agent: true`, e.g. `hermes1`), reserved for
    /// the agent's `subject_id:"self"` autobiography. The x2 guard reads it to
    /// keep user/group facts out of the agent wiki.
    pub(crate) is_agent: bool,
}

/// Render the `available_wikis:` routing window — the ONE renderer, shared by
/// the conversational classifier ([`build_prompt`]) and the document extractor
/// ([`crate::document`]), so the two windows cannot drift on what a wiki looks
/// like or on what counts as its description.
///
/// Per wiki: id, title, type, `is_agent` when set, then the description as up
/// to two lines, each omitted when empty:
///
/// - `scope:` — the **authored** intent of the wiki (human-written).
/// - `holds:` — the **compiled** abstract, "what it actually holds", refreshed
///   by the compiler from the wiki's own foundation card.
///
/// Neither present → `about: (not described yet)`, the honest answer and
/// shorter than two empty keys. Both are cut at `max_desc_chars` (a runaway
/// hand-edited `_meta.md` must not eat the prompt budget).
///
/// `is_agent` is emitted **only when true**. It has to be emitted at all
/// because an agent's own wiki is a `wiki-user` exactly like a human's — an
/// agent is an enrolled user (the diagonal identity model) — so without the
/// line the classifier can only tell them apart by guessing at the title, and
/// a fact about a person lands in the agent's autobiography (which the x2
/// guard in [`validate_capture_plan`] then has to undo). It is omitted when
/// false because the vast majority of wikis are human, and a `false` on every
/// line would be prompt weight spent on nothing.
pub(crate) fn render_available_wikis(
    out: &mut String,
    wikis: &[AvailableWiki],
    max_desc_chars: usize,
) {
    out.push_str("available_wikis:\n");
    if wikis.is_empty() {
        out.push_str("  (none yet — capture will need a wiki to be forged first)\n");
        return;
    }
    for w in wikis {
        out.push_str("  - wiki_id: ");
        out.push_str(&w.wiki_id);
        out.push_str("\n    title: ");
        out.push_str(&w.title);
        out.push_str("\n    type: ");
        out.push_str(&w.wiki_type);
        if w.is_agent {
            out.push_str("\n    is_agent: true");
        }
        let scope = w.scope.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let holds = w
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if scope.is_none() && holds.is_none() {
            out.push_str("\n    about: (not described yet)");
        }
        if let Some(s) = scope {
            out.push_str("\n    scope: ");
            out.push_str(&truncate(s, max_desc_chars));
        }
        if let Some(s) = holds {
            out.push_str("\n    holds: ");
            out.push_str(&truncate(s, max_desc_chars));
        }
        out.push('\n');
    }
}

/// True for an **identity wiki**: a root (`parent_wiki_id == None`) whose
/// `wiki_type` is one of the two identity types. Its id **is** its principal's
/// id, which is what makes it self-describing in the routing window — and what
/// makes it exempt from the cap below.
///
/// An agent's wiki is an [`wiki::IDENTITY_WIKI_TYPE`] root like a human's (it
/// only adds the `is_agent` marker), so it is an identity wiki here too.
fn is_identity_wiki(meta: &wiki::WikiMeta) -> bool {
    meta.parent_wiki_id.is_none()
        && matches!(
            meta.wiki_type.as_str(),
            wiki::IDENTITY_WIKI_TYPE | wiki::GROUP_IDENTITY_WIKI_TYPE
        )
}

/// Enumerate the wikis a capture may be routed into, as the classifier will
/// see them.
///
/// Three rules, in this order — each one exists because the naive version cost
/// the router something it could not get back:
///
/// 1. **Smart wikis leave first, before anything is counted.** They are
///    authoritatively written through the `wiki_admin_*` family and are never a
///    routing target, so letting one consume a slot would silently shrink the
///    window by the number of project notebooks the deployment happens to have.
/// 2. **Identity wikis always enter, outside the count.** A deployment's users
///    and groups ARE its routing domains: dropping one does not degrade the
///    choice, it removes the only correct answer for every fact about that
///    person. They are also bounded by enrollment, not by memory growth.
/// 3. **Only emerged wikis are capped, oldest first.** They are the unbounded
///    set (REM mints them from page groups), so the cap belongs here. Ordering
///    by `created` keeps the settled subject areas — the ones with a history of
///    facts landing in them — and drops the newest, which are the most likely to
///    be re-absorbed by a later consolidation anyway. A truncation is logged: a
///    bounded window must say what it dropped.
///
/// The returned order is identity wikis in tree order, then the surviving
/// emerged ones oldest-first — deterministic, so the same tree always renders
/// the same prompt.
pub(crate) fn available_wikis(tree: &WikiTree, cap: usize) -> Result<Vec<AvailableWiki>> {
    let mut identity = Vec::new();
    let mut emerged: Vec<(String, AvailableWiki)> = Vec::new();
    for d in tree.walk()? {
        if d.meta.smart {
            continue;
        }
        let w = AvailableWiki {
            wiki_id: d.meta.wiki_id.as_str().to_owned(),
            title: d.meta.title.clone(),
            wiki_type: d.meta.wiki_type.clone(),
            scope: d.meta.scope.clone(),
            summary: wiki::meta_summary(&d.meta),
            smart: d.meta.smart,
            is_agent: d.meta.is_agent,
        };
        if is_identity_wiki(&d.meta) {
            identity.push(w);
        } else {
            // Sort key: the creation instant, oldest first. An undated wiki
            // (hand-authored, or born before the field existed) sorts as the
            // oldest — by every other sign it has been there longest. The
            // `wiki_id` breaks ties so the order never depends on the walk.
            emerged.push((d.meta.created.clone().unwrap_or_default(), w));
        }
    }
    emerged.sort_by(|(a_created, a), (b_created, b)| {
        a_created
            .cmp(b_created)
            .then_with(|| a.wiki_id.cmp(&b.wiki_id))
    });
    if emerged.len() > cap {
        tracing::warn!(
            emerged = emerged.len(),
            cap,
            dropped = emerged.len() - cap,
            "ingest: emerged-wiki window truncated — the newest wikis are not offered this turn"
        );
        emerged.truncate(cap);
    }
    identity.extend(emerged.into_iter().map(|(_, w)| w));
    Ok(identity)
}

/// Read the sender's `@rules.md` user-policy — governance PROSE only,
/// best-effort.
///
/// The sender's identity wiki is `wiki_id == sender_id`; its `@rules.md`
/// ([`crate::wiki::RULES_FILENAME`]) holds the standing privacy/sharing policy
/// the classifier honours when it assigns per-fact ACL. The
/// same page also carries the user's USER-GLOBAL behaviour rules as `{{f=…}}`
/// fact regions — those reach the classifier separately, with `fact_id`s, via
/// `agent_behaviour_rules` ([`push_behaviour_rules_section`]), so the regions
/// are stripped here: only the free prose is the governance policy, and no
/// rule is injected twice (or under the wrong section). Headings are dropped
/// with them — a heading is structure, and the page is seeded with one and
/// nothing else, so a page carrying only headings carries no policy. Returns
/// `None` — and the prompt's `sender_rules` section reads `(none)`, so the
/// classifier decides ACL as it does for anyone who set no rule — for a
/// sender with no identity wiki, no `@rules.md`, a page with no policy on it,
/// or any read error.
/// Best-effort by design: a policy is an aid to the ACL decision, never a hard
/// gate, so it must never fail the ingest (pillar: the LLM decides).
fn sender_rules(tree: &WikiTree, sender_id: &str) -> Option<String> {
    let id = WikiId::parse(sender_id).ok()?;
    let body = tree
        .locate(&id)
        .ok()?
        .read_page(Path::new(crate::wiki::RULES_FILENAME))
        .ok()?;
    let prose: String = crate::parser::parse(&body)
        .events
        .into_iter()
        .filter_map(|e| match e {
            crate::parser::ParseEvent::Prose { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    let policy = prose
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    (!policy.trim().is_empty()).then_some(policy)
}

/// Append an engine-rule to the sender's `@rules.md`.
///
/// The write side of the engine-rule loop: when the classifier marks an
/// extraction as a standing *governance* directive, the orchestrator routes the
/// body here instead of [`capture::wiki_capture`] — the rule lives as prose in
/// `wikis/<sender_id>/@rules.md` and is read straight back as [`sender_rules`]
/// next turn (a tight write→read loop), never as a row in `fact_index`.
///
/// Returns `Ok(true)` when the rule was written, `Ok(false)` when the sender has
/// no locatable identity wiki (the rule is dropped, mirroring the best-effort
/// posture of the read side, rather than failing the turn). A genuine IO write
/// failure bubbles as [`IngestError::Wiki`] — consistent with how a real
/// `wiki_capture` filesystem error propagates.
fn append_sender_rule(tree: &WikiTree, sender_id: &str, rule: &str) -> Result<bool> {
    let Ok(id) = WikiId::parse(sender_id) else {
        return Ok(false);
    };
    let Ok(handle) = tree.locate(&id) else {
        return Ok(false);
    };
    crate::wiki::append_engine_rule(&handle, rule)?;
    Ok(true)
}

/// Page where behaviour rules are filed (the ingest prompt's Part 7).
/// Unified with the engine-policy page name: in the *agent's*
/// wiki this `@rules.md` holds the per-user and agent-wide behaviour facts —
/// no collision, since [`sender_rules`] (the engine-policy reader) never runs
/// for the agent (it is never a sender). In the *user's* identity wiki the
/// same page carries their USER-GLOBAL behaviour facts alongside the
/// governance prose — [`sender_rules`] reads the prose only and
/// skips the fact regions. The per-fact `subject` scopes each rule (the served
/// user for a per-user or user-global rule, the agent for an agent-wide one);
/// the home wiki tells per-user and user-global apart.
const BEHAVIOUR_RULES_PAGE: &str = crate::wiki::RULES_FILENAME;

/// The governance scope of a behaviour-rule — who may set it and how widely it
/// applies. Read from the grammatical **addressee** by the classifier
/// (`behaviour_scope`, prompt Part 7), enforced by the engine. *soul vs
/// operational* (style vs tools) is just an optional content tag now — it no
/// longer routes anything; this scope axis does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BehaviourScope {
    /// Addressed to the speaker ("with me / my things", or a bare imperative
    /// with no audience): shapes how the agent behaves WITH THIS USER. Open to
    /// **anyone** — it only touches them; filed `subject = the user` in the
    /// calling agent's wiki, recalled only for that user on that agent. The
    /// default when the addressee is unclear.
    PerUser,
    /// Impersonal / universal ("con tutti / con chiunque", or a how-the-agent-
    /// works directive with no per-speaker scope): changes the agent's
    /// behaviour for EVERYONE. **Admin-only**; filed `subject = the agent`,
    /// recalled for every user.
    AgentWide,
    /// Explicitly addressed to EVERY assistant the user talks to ("tutti gli
    /// assistenti", "con qualunque assistente", "chiunque tu sia"): the user's
    /// own standing rule for all their consumers. Open to
    /// **anyone** — it binds only their own conversations; filed `subject = the
    /// sender` in the sender's IDENTITY wiki, recalled by every consumer
    /// serving them.
    UserGlobal,
}

impl BehaviourScope {
    /// Map the classifier's `behaviour_scope` string to a scope. Only the
    /// explicit wire tokens widen the reach — `"agent-wide"` (everyone on this
    /// agent, admin-gated) and `"user-global"` (this user on every agent);
    /// anything else — including absent or a bare imperative — defaults to
    /// **per-user**, the open side that touches only the speaker on this one
    /// agent.
    fn from_hint(hint: Option<&str>) -> Self {
        match hint {
            Some("agent-wide") => Self::AgentWide,
            Some("user-global") => Self::UserGlobal,
            _ => Self::PerUser,
        }
    }

    /// The classifier's wire token for this scope — used when the existing
    /// rules are injected back into the prompt, so the model restates the
    /// right scope when it revises one.
    const fn as_hint(self) -> &'static str {
        match self {
            Self::PerUser => "per-user",
            Self::AgentWide => "agent-wide",
            Self::UserGlobal => "user-global",
        }
    }
}

/// File a behaviour-rule fact on the `@rules.md` page its scope calls home,
/// written LIVE (direct path) so it is in effect on the
/// next turn.
///
/// `scope` decides BOTH the home wiki and the subject, which together decide
/// reach (see [`recall_behaviour_rules`]):
/// - [`BehaviourScope::PerUser`] → the CALLING AGENT's own wiki — resolved
///   from [`IngestRequest::consumer_id`] via
///   [`crate::consumers::system_user_for`], falling back to the sender's own
///   wiki when no binding resolves (a smart consumer IS its user) — with
///   `subject = the sender`, so different users' per-user rules stay distinct
///   facts and recall pulls only the served user's own — "how the agent
///   behaves WITH ME".
/// - [`BehaviourScope::AgentWide`] → the agent's wiki, `subject = the agent`, so
///   the rule is the agent's standing operation, recalled for **every** user.
///   The dispatch in [`run`] only reaches here for an agent-wide rule after
///   confirming the sender is the admin ([`crate::enrollment::is_admin`]).
/// - [`BehaviourScope::UserGlobal`] → the SENDER's identity wiki, `subject = the
///   sender` — the user's own rule for every assistant serving them, recalled
///   by every consumer regardless of which one heard it. On a smart consumer
///   the per-user fallback and this home coincide (its wiki IS the user's), so
///   the two scopes deliberately collapse there.
///
/// Returns the new `fact_id`, or `None` when no target wiki could be located
/// (the rule is dropped, mirroring the best-effort posture of [`append_sender_rule`]).
async fn capture_behaviour_rule(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    request: &IngestRequest,
    rule: &str,
    scope: BehaviourScope,
    supersede: Option<&FactId>,
) -> Result<Option<FactId>> {
    let sender = request.sender_id.as_str();
    // Target wiki: a USER-GLOBAL rule lives in the sender's own identity wiki
    // whoever is calling; the agent-scoped rules live in the calling agent's
    // OWN wiki when a system-user binding resolves (a standard/bot consumer),
    // else the sender's own wiki (a smart consumer IS its user). Resolution is
    // best-effort — a DB miss falls back rather than failing the turn.
    let target = if scope == BehaviourScope::UserGlobal {
        sender.to_owned()
    } else {
        let bound = match request.consumer_id.as_deref() {
            Some(cid) => crate::consumers::system_user_for(pool, cid)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        match bound {
            Some(sys) if sys != sender => sys,
            _ => sender.to_owned(),
        }
    };
    let Ok(wiki_id) = WikiId::parse(&target) else {
        return Ok(None);
    };
    // The SUBJECT is the scope. PER-USER and USER-GLOBAL ⇒ the subject is the
    // USER who dictated it, so different users' rules are distinct facts
    // (subject-scoped dedup never folds franz's into bilbo's) and recall pulls
    // only the served user's own — the home wiki tells the two apart.
    // AGENT-WIDE ⇒ the subject is the AGENT itself: one policy for everyone,
    // recalled for every user, deduped across the agent's own standing rules.
    // Either way subject == the principal ⇒ no separate sender attribution.
    let subject = match scope {
        BehaviourScope::PerUser | BehaviourScope::UserGlobal => Principal::User(sender.to_owned()),
        BehaviourScope::AgentWide => Principal::User(target.clone()),
    };
    let page_description = match scope {
        BehaviourScope::PerUser => {
            "How this agent should behave, per requesting user (per-user \
             behaviour rules; one subject per user)."
        },
        BehaviourScope::AgentWide => {
            "How this agent behaves for everyone — tone, tools, workflow, \
             standing cautions (agent-wide behaviour rules set by the admin)."
        },
        BehaviourScope::UserGlobal => {
            "This user's standing rules for EVERY assistant serving them \
             (user-global behaviour rules), alongside their governance policy \
             prose."
        },
    };

    let cap_req = CaptureRequest {
        wiki_id,
        subject_external: None,
        page: Some(PathBuf::from(BEHAVIOUR_RULES_PAGE)),
        body: rule.to_owned(),
        subject,
        allow: Vec::new(),
        sender: None,
        fact_type: Some("rule".to_owned()),
        topics: Vec::new(),
        dedup_threshold: None,
        valid_from: None,
        valid_to: None,
        style: Some(crate::wiki::PageStyle::ProsaTecnica),
        // The behaviour-rules page's own card, written on its testata the
        // first time the page is created (`capture::seed_page_card`).
        page_description: Some(page_description.to_owned()),
        salience: None,
        authored_refs: Vec::new(),
    };
    // Supersede when the user revises a directive the classifier was shown;
    // else additive (deduped against this user's own rules by subject scope).
    let outcome = match supersede {
        Some(old) => {
            capture::wiki_supersede(tree, pool, embedder, old, cap_req, request.turn_now()).await?
        },
        None => capture::wiki_capture(tree, pool, embedder, cap_req).await?,
    };
    Ok(Some(outcome.fact_id))
}

/// File a fact the agent states about ITSELF — the self side of agent-authored
/// memory (ingest pipeline).
/// Which page a `self` fact lands on. The engine decides — not
/// the model's proposed `target_page` — mirroring how a self-fact's wiki is
/// already engine-pinned to the agent's own wiki. An IDENTITY fact
/// (user-agnostic, injected every turn) carries no page: it waits in the
/// capture buffer, and the placement pass
/// lifts it onto the identity pages it consolidates. A RELATIONSHIP fact goes
/// to a per-served-user page `esperienze_<user>.md`, so the agent's history
/// with each user grows in its own space instead of piling into one
/// heterogeneous catch-all — one `esperienze_agente.md` holding everything the
/// agent ever did for anybody, which is a page nobody can find a thing in. A
/// relationship fact with no served user carries no page either.
///
/// Recall is page-agnostic (`recall_agent_self` buckets by the served-user
/// topic tag, not the page), so this write-time routing is invisible to reads.
fn agent_self_fact_page(is_identity: bool, sender_id: &str) -> Option<PathBuf> {
    if is_identity || sender_id.is_empty() {
        None
    } else {
        normalize_capture_page(Some(&format!("esperienze_{sender_id}.md")))
    }
}

/// The `subject_id: "self"` sentinel on an assistant turn
/// (prompt Part 9) routes here: the body is filed as a normal fact in the
/// calling agent's OWN wiki, **owned by the agent** (`subject == sender == the
/// agent` ⇒ no separate sender), so it becomes the agent's emergent self — its
/// identity (high-salience facts the REM consolidates onto its card) and its
/// history with each user. The fact is auto-tagged with the served user's id as
/// a topic, so the read side ([`recall_agent_self`]) can scope "your history
/// with THIS user" without surfacing the agent's history with anyone else.
/// Written LIVE so the agent can recall it on the very next turn; the REM later
/// consolidates it like any wiki. Returns `None` when no agent wiki resolves (a
/// smart consumer IS its user — its replies are already its own facts on the
/// normal path) or the body is empty.
async fn capture_agent_self_fact(
    tree: &WikiTree,
    pool: &SqlitePool,
    embedder: Arc<dyn Embedder>,
    request: &IngestRequest,
    agent_id: &str,
    unit: &CaptureUnit<'_>,
    known_users: &[enrollment::EnrolledUserLite],
    policy: &IngestPolicy,
) -> Result<Option<FactId>> {
    let Some(body) = unit.body.map(str::trim).filter(|b| !b.is_empty()) else {
        tracing::warn!("ingest: subject_id=self extraction has no body — dropped");
        return Ok(None);
    };
    let Ok(wiki_id) = WikiId::parse(agent_id) else {
        return Ok(None);
    };
    // Tag the self-fact with the served user so the read side can pull "your
    // history with THIS user" by topic — but ONLY for a relationship/activity
    // fact. An IDENTITY fact ("the agent assists Franz's household") is
    // user-agnostic and stays UNTAGGED, so it injects as the always-on
    // identity for every interaction and never leaks into another user's
    // relationship slot. The discriminator matches the read-side bucket
    // ([`recall_agent_self`]): `salience high` ∨ `fact_type bio` ⇒ identity
    // ⇒ no user tag; otherwise ⇒ relationship ⇒ tag with the served user.
    //
    // The partner tag is EXCLUSIVE: on an agent self-fact a
    // user-id topic means "an action WITH that user", so any *other*
    // enrolled user's id the classifier put in `topics` (a mere mention —
    // "advised Morgana about Matteo") is stripped. Without this, the
    // mentioned user's turns would inherit another user's history. Only
    // enrolled user ids are partner-capable — a subject that never speaks
    // keeps its content tag.
    let is_identity = unit.salience == Some("high") || unit.fact_type == Some("bio");
    let mut topics: Vec<String> = unit
        .topics
        .iter()
        .filter(|t| {
            let mentioned_user =
                **t != request.sender_id && known_users.iter().any(|u| &u.user_id == *t);
            if mentioned_user {
                tracing::debug!(
                    topic = %t,
                    "ingest: self-fact topic names another enrolled user — stripped (partner tag is exclusive)"
                );
            }
            !mentioned_user
        })
        .cloned()
        .collect();
    if !is_identity
        && !request.sender_id.is_empty()
        && !topics.iter().any(|t| t == &request.sender_id)
    {
        topics.push(request.sender_id.clone());
    }
    let page = agent_self_fact_page(is_identity, &request.sender_id);
    let cap_req = CaptureRequest {
        wiki_id,
        subject_external: None,
        page,
        body: body.to_owned(),
        // OWNED BY THE AGENT — this is its own self-knowledge, not about the
        // user. subject == the agent ⇒ no separate sender attribution.
        subject: Principal::User(agent_id.to_owned()),
        allow: Vec::new(),
        sender: None,
        fact_type: unit.fact_type.map(str::to_owned),
        topics,
        dedup_threshold: Some(policy.dedup_threshold),
        // Same normalisation as `validate_capture_plan`: a malformed
        // LLM bound degrades to open, never lands verbatim in
        // `fact_index`'s lexicographically-compared columns.
        valid_from: normalize_capture_bound(unit.valid_from, fact_index::DayEdge::Start),
        valid_to: normalize_capture_bound(unit.valid_to, fact_index::DayEdge::End),
        style: crate::wiki::PageStyle::parse_lenient(unit.style),
        page_description: unit.page_description.map(str::to_owned).or_else(|| {
            Some("The agent's own memory: who it is and its history with each user.".to_owned())
        }),
        salience: unit.salience.map(str::to_owned),
        authored_refs: Vec::new(),
    };
    // An identity self-fact names no page, so it waits in the queue like any
    // other unplaced claim; a relationship fact named its own page and is
    // written now.
    if cap_req.page.is_none() {
        let staging =
            capture_buffer::BufferStaging::build(embedder.as_ref(), &cap_req.body, None).await;
        let buffered = capture_buffer::buffer_capture_staged(pool, cap_req, None, staging).await?;
        return Ok(Some(buffered.capture_id));
    }
    let outcome = capture::wiki_capture(tree, pool, embedder, cap_req).await?;
    Ok(Some(outcome.fact_id))
}

/// Cap on how many behaviour-rule facts the read side pulls per turn — a
/// safety bound; in practice a user holds a handful of standing directives.
const BEHAVIOUR_RULES_RECALL_CAP: usize = 50;

/// Pull the behaviour-rule facts whose SUBJECT is this principal, on the
/// agent's `@rules.md`
/// page, via [`fact_index::find_behaviour_rules`]. Page-scoped
/// on purpose: a `subject = agent` query would otherwise drag in the agent's
/// self-facts, which live on its content pages, not here. The
/// rules-page predicate sits **in the SQL, before the cap**, so unrelated
/// facts under the same subject can never starve old rules out of the LIMIT
/// window; and the query filters validity at *now* — a rule whose window was
/// closed (retracted from chat, or dated and expired) stops being served,
/// while the fact itself stays (closing is never deleting). Best-effort — a
/// DB miss yields nothing.
async fn behaviour_rows_on_page(
    pool: &SqlitePool,
    agent_wiki: &str,
    subject: &Principal,
) -> Vec<(FactId, String)> {
    let now = chrono::Utc::now().to_rfc3339();
    match fact_index::find_behaviour_rules(
        pool,
        agent_wiki,
        subject,
        &now,
        BEHAVIOUR_RULES_RECALL_CAP,
    )
    .await
    {
        Ok(rows) => rows.into_iter().map(|r| (r.fact_id, r.text)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "ingest: behaviour-rule recall failed (best-effort)");
            Vec::new()
        },
    }
}

/// Recall the behaviour rules in force for THIS turn — the read side of the
/// behaviour-rule loop. Three scopes, two homes:
/// **agent-wide** (the agent's wiki, `subject = the agent`) applies for every
/// user of this agent; **user-global** (the SENDER's identity wiki, `subject =
/// the sender`) applies on every consumer serving this user; **per-user** (the
/// agent's wiki, `subject = the served user`) applies only to this user on this
/// agent. Returns `(fact_id, body, scope)` so the consumer applies them every
/// turn and the classifier can supersede any of the three (the scope gates who
/// may — see the dispatch in [`run`]). Order pinned, most specific last:
/// agent-wide (the floor) → user-global → per-user. A smart consumer (no
/// distinct agent wiki) gets only the user-global set — its wiki IS the
/// user's, so everything on that rules page is the user's own everywhere-rule.
/// Best-effort throughout.
async fn recall_behaviour_rules(
    pool: &SqlitePool,
    request: &IngestRequest,
) -> Vec<(FactId, String, BehaviourScope)> {
    fn tag(
        rows: Vec<(FactId, String)>,
        scope: BehaviourScope,
    ) -> impl Iterator<Item = (FactId, String, BehaviourScope)> {
        rows.into_iter().map(move |(id, body)| (id, body, scope))
    }
    let sender = Principal::User(request.sender_id.clone());
    // The user's everywhere-rules: their identity wiki's rules page, owned by
    // themself — in force on EVERY consumer serving them.
    let user_global = behaviour_rows_on_page(pool, request.sender_id.as_str(), &sender).await;
    let agent_wiki = match request.consumer_id.as_deref() {
        Some(cid) => crate::consumers::system_user_for(pool, cid)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let Some(agent_wiki) = agent_wiki.filter(|w| *w != request.sender_id) else {
        // Smart consumer / no binding — no distinct agent wiki, so the only
        // dedicated channel source is the user's own everywhere-set.
        return tag(user_global, BehaviourScope::UserGlobal).collect();
    };
    // Agent-wide rules (subject = the agent) — recalled for everyone.
    let mut rules: Vec<_> = tag(
        behaviour_rows_on_page(pool, &agent_wiki, &Principal::User(agent_wiki.clone())).await,
        BehaviourScope::AgentWide,
    )
    .collect();
    rules.extend(tag(user_global, BehaviourScope::UserGlobal));
    // The served user's own per-user rules (subject = the user) — "WITH ME".
    rules.extend(tag(
        behaviour_rows_on_page(pool, &agent_wiki, &sender).await,
        BehaviourScope::PerUser,
    ));
    rules
}

/// Stable header of the `rules` field's directives section — the `YOUR
/// RULES` role section of the injected turn context (the host places the
/// `rules` field adjacent to the recall block).
const HDR_YOUR_RULES: &str = "YOUR RULES (standing directives — agent-wide, this user's for \
     every assistant, and this user's for you; apply them in your reply, never relay them):";

/// Render recalled behaviour rules as the `YOUR RULES` section the
/// consumer agent applies when it composes its reply. Flat on purpose — the
/// per-bullet scope is a governance detail the classifier needs (it rides
/// [`push_behaviour_rules_section`]), not the consumer: a rule in force is a
/// rule in force. `None` when empty; whole-bullet fitted against
/// `policy.max_sender_rules_chars` ([`fit_bullets`] — one rule per line,
/// never a mid-word cut).
fn format_behaviour_rules(
    rules: &[(FactId, String, BehaviourScope)],
    policy: &IngestPolicy,
) -> Option<String> {
    fit_bullets(
        HDR_YOUR_RULES,
        rules.iter().map(|(_, body, _)| body.as_str()),
        policy.max_sender_rules_chars,
    )
}

/// Stable header of the `recent_window` field — the user's live thread
/// from their OTHER surfaces (the cross-consumer recent window).
/// The "do not re-answer" framing is load-bearing: replayed turns at a
/// context tail invite a model to answer them again.
const HDR_RECENT_EXCHANGES: &str = "RECENT EXCHANGES ON YOUR OTHER CHANNELS WITH THIS USER \
     (reference — the thread may have moved on; do not re-answer these):";

/// Per-entry text ceiling inside the `recent_window` section — one
/// utterance never eats the whole section budget.
const RECENT_ENTRY_CHARS: usize = 240;

/// Render the cross-consumer recent window as its self-labelled section.
/// Same discipline as [`fit_bullets`] — whole entries, never a mid-word
/// cut — but the budget walk runs newest-first while the render stays
/// oldest-first, so when the budget bites it is the oldest exchanges that
/// fall off, not the freshest.
fn format_recent_window(
    entries: &[crate::recent_window::RecentExchange],
    now: chrono::DateTime<chrono::Utc>,
    policy: &IngestPolicy,
) -> Option<String> {
    if entries.is_empty() || policy.recent_window_chars == 0 {
        return None;
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            let mut text: String = e.text.trim().chars().take(RECENT_ENTRY_CHARS).collect();
            if text.chars().count() == RECENT_ENTRY_CHARS {
                text.push('…');
            }
            let surface = match (e.consumer_id.as_str(), e.channel.as_str()) {
                ("", "") => String::from("another channel"),
                (c, "") => c.to_owned(),
                ("", ch) => ch.to_owned(),
                (c, ch) => format!("{c}/{ch}"),
            };
            let speaker = match e.author {
                MessageRole::User => "user",
                MessageRole::Assistant => "agent",
            };
            format!(
                "[{} · via {surface}] {speaker}: {text}",
                crate::recent_window::relative_age(&e.occurred_at, now)
            )
        })
        .collect();
    // Newest-first budget walk over the oldest-first render order.
    let header_cost = HDR_RECENT_EXCHANGES.chars().count();
    let mut used = header_cost;
    let mut keep_from = lines.len();
    for (i, line) in lines.iter().enumerate().rev() {
        let cost = line.chars().count() + 3; // "\n- "
        if used + cost > policy.recent_window_chars && keep_from < lines.len() {
            break;
        }
        if used + cost > policy.recent_window_chars {
            // Pathological single entry over the whole budget: keep it —
            // an empty section would hide a live thread entirely.
            keep_from = i;
            break;
        }
        used += cost;
        keep_from = i;
    }
    fit_bullets(
        HDR_RECENT_EXCHANGES,
        lines[keep_from..].iter().map(String::as_str),
        policy.recent_window_chars,
    )
}

/// Cap on agent self-facts pulled per turn for the self-context block — a
/// safety bound; the identity core + one user's relationship are few.
const AGENT_SELF_RECALL_CAP: usize = 100;

/// The agent's self-context pulled for one turn: the agent wiki's one-line
/// abstract plus the two self-fact buckets. All empty for a smart consumer
/// (no distinct agent wiki) or when the bot acts as itself.
#[derive(Default)]
struct AgentSelf {
    /// The agent wiki's `_meta.summary` — the compiled autobiography's
    /// abstract, refreshed by the compiler's abstract sync.
    summary: Option<String>,
    /// Identity self-facts (`salience high` ∨ `fact_type bio`) — WHO IT IS,
    /// user-agnostic, injected on every turn. Newest first.
    identity: Vec<String>,
    /// Self-facts tagged with the served sender — ITS HISTORY WITH THIS
    /// USER, scoped so one user's relationship never surfaces in another's
    /// turn. Newest first.
    relationship: Vec<String>,
}

/// Recall the agent's OWN memory for the self-context sections — the read
/// side of agent-authored memory. Best-effort on a DB miss; the summary is
/// read from the agent wiki's `_meta.md` ([`wiki::meta_summary`]).
/// Mirrors [`recall_behaviour_rules`], for the agent's own facts.
async fn recall_agent_self(
    pool: &SqlitePool,
    tree: &WikiTree,
    request: &IngestRequest,
) -> AgentSelf {
    let Some(consumer_id) = request.consumer_id.as_deref() else {
        return AgentSelf::default();
    };
    let Some(agent_wiki) = crate::consumers::system_user_for(pool, consumer_id)
        .await
        .ok()
        .flatten()
    else {
        return AgentSelf::default();
    };
    if agent_wiki == request.sender_id {
        // The bot is acting as itself — its self-facts are its own normal recall.
        return AgentSelf::default();
    }
    let summary = WikiId::parse(&agent_wiki)
        .ok()
        .and_then(|id| tree.locate(&id).ok())
        .and_then(|h| crate::wiki::meta_summary(h.meta()));
    let filters = fact_index::FactFilters {
        wiki_id: Some(agent_wiki.clone()),
        subject_id: Some(Principal::User(agent_wiki)),
        limit: AGENT_SELF_RECALL_CAP,
        ..Default::default()
    };
    let rows = match fact_index::find_by_filters(pool, &filters).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "ingest: agent self-recall failed (best-effort)");
            return AgentSelf {
                summary,
                ..AgentSelf::default()
            };
        },
    };
    let mut identity = Vec::new();
    let mut relationship = Vec::new();
    // Fact ids actually surfaced this turn, so the agent-self path bumps recall
    // hits like the normal recall path ([`fact_index::bump_recall_hits`]).
    // Without this every self-fact stays `recall_count_30d = 0` /
    // `last_recall_at = NULL` forever — the agent's autobiography IS injected
    // each turn but reads as never-used, and recall-weighted REM (the
    // paragraph-split scorer) treats the whole agent wiki as cold
    //.
    let mut surfaced: Vec<FactId> = Vec::new();
    for row in rows {
        // The agent's agent-wide behaviour-rules are subject=agent too, but they
        // are policy, not self-knowledge — they belong to the behaviour-rule
        // channel ([`recall_behaviour_rules`]), not the self-context block.
        // Keyed on the exact rules-page predicate (a `house_rules.md`-style
        // content page is self-knowledge, not policy).
        if crate::wiki::is_rules_page(&row.source_path) {
            continue;
        }
        let text = row.text.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        // Identity = high salience OR a `bio`-typed self-fact: what the agent
        // IS, not what it did. Everything else scopes by the served sender's
        // partner tag (exclusive at capture — `capture_agent_self_fact`).
        if row.salience.as_deref() == Some("high") || row.fact_type.as_deref() == Some("bio") {
            identity.push(text);
            surfaced.push(row.fact_id.clone());
        } else if row.topics.iter().any(|t| t == &request.sender_id) {
            relationship.push(text);
            surfaced.push(row.fact_id.clone());
        }
    }
    // Best-effort: a recall-tracking miss must never break the recall block.
    if let Err(e) = fact_index::bump_recall_hits(pool, &surfaced).await {
        tracing::warn!(error = %e, "ingest: agent self-recall hit-bump failed (best-effort)");
    }
    AgentSelf {
        summary,
        identity,
        relationship,
    }
}

/// Stable header of the recall block's agent-identity section.
const HDR_WHO_YOU_ARE: &str =
    "WHO YOU ARE (your own memory — this is you; apply it as your identity):";
/// Stable header of the recall block's agent-history section.
const HDR_YOUR_HISTORY: &str = "YOUR RECENT HISTORY WITH THIS USER (what you have done \
     together / advised before, newest first):";
/// Stable header of the recall block's sender-identity section.
const HDR_WHO_IS_SPEAKING: &str = "WHO IS SPEAKING:";

/// Render the `WHO YOU ARE` section: the agent wiki's summary line leads,
/// then the identity self-facts, whole-bullet fitted against
/// `policy.max_agent_identity_chars`. `None` when there is nothing to say.
fn format_who_you_are(agent: &AgentSelf, policy: &IngestPolicy) -> Option<String> {
    fit_bullets(
        HDR_WHO_YOU_ARE,
        agent
            .summary
            .as_deref()
            .into_iter()
            .chain(agent.identity.iter().map(String::as_str)),
        policy.max_agent_identity_chars,
    )
}

/// Render the `YOUR RECENT HISTORY WITH THIS USER` section, whole-bullet
/// fitted against `policy.max_agent_history_chars` (newest first — the
/// oldest tail falls off). `None` when the relationship is empty.
fn format_history_with_user(agent: &AgentSelf, policy: &IngestPolicy) -> Option<String> {
    fit_bullets(
        HDR_YOUR_HISTORY,
        agent.relationship.iter().map(String::as_str),
        policy.max_agent_history_chars,
    )
}

/// The identity card of a principal's wiki — **`@profile.md`**.
///
/// A content page like any other, except that this slot serves it
/// deterministically. A standard wiki has no root page to confuse it with.
///
/// This is the page the placement routes the identity core onto, and the page
/// this slot reads.
const IDENTITY_PAGE: &str = wiki::PROFILE_FILENAME;

/// What the `WHO IS SPEAKING` slot produced.
#[derive(Debug)]
struct SpeakerCard {
    /// The rendered section, ready to join the recall block.
    section: String,
    /// Workdir-relative path of the identity page whose prose the section
    /// carries. Three consumers: the flat slot drops a hit homed there, the
    /// funnel treats it as already visited (it is never
    /// navigated), and the recall log counts it among the pages this turn
    /// surfaced, so restating one of its facts is not scored as a miss.
    /// `None` when only the one-line summary was served — that duplicates
    /// nothing.
    page_path: Option<String>,
    /// `(wiki_id, projected card markdown)` for `navigate`'s `served_cards`
    /// — the card's own `[[wikilinks]]`, which the funnel can reach by no
    /// other route once the page is marked already-served.
    rails: Option<(String, String)>,
}

/// Render the `WHO IS SPEAKING` section — the sender's identity card.
///
/// The slot serves the sender's **`@profile.md`**, not a one-line
/// abstract of it: the card is the set of facts the classifier deterministically
/// routed to the identity page (name, birthdate, contacts, family ties — but
/// also the standing health constraints and the characterising preferences a
/// `bio`-typed query would miss), and it is the one thing the turn needs whatever
/// was asked. Serving it here means it **always** arrives, at no walk, no model
/// decision and no page open — where before it arrived only if the navigator
/// chose to spend a hop on it.
///
/// Three properties this must hold, whatever REM later writes on the page:
///
/// 1. **Projected per sender, always** ([`crate::render::render_for_sender_segments`])
///    — never a raw marker, and never a precomputed blob: which fragments a reader
///    may see depends on who is asking. Today the sender owns their own card and the
///    projection is nearly a no-op; 69d serves a *subject's* card to a different
///    reader, and the invariant has to already be there when it does.
/// 2. **`[[wikilinks]]` rendered plainly** — at injection only. They are the
///    navigator's rails and REM authors them deliberately, so they are never
///    touched on disk ([`plain_wikilinks`]).
/// 3. **Injected once, and never re-read.** The page's prose could also arrive
///    as a flat hit; [`SpeakerCard::page_path`] is what drops it. It cannot
///    arrive as a navigated fragment at all — the funnel is handed the page as
///    already visited, so it is not a navigation destination for its own subject
///    (69b).
///
/// Falls back to the wiki's one-line `_meta.summary` when there is no readable
/// card — one line is still better than saying nothing.
/// `None` when the sender has no identity wiki, or it yields neither.
async fn who_is_speaking_section(
    pool: &SqlitePool,
    tree: &WikiTree,
    sender: &SenderContext,
    policy: &IngestPolicy,
) -> Option<SpeakerCard> {
    let sender_id = sender.sender_id.as_str();
    let handle = WikiId::parse(sender_id)
        .ok()
        .and_then(|id| tree.locate(&id).ok())?;
    let summary = crate::wiki::meta_summary(handle.meta())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let card = identity_card(pool, tree, &handle, sender, policy).await;

    // The label line is the only thing that names the sender's *id* — the
    // card prose says who they are, never what they are called on the wire.
    let mut section = String::from(HDR_WHO_IS_SPEAKING);
    match &summary {
        Some(s) => {
            let _ = write!(section, "\n- {sender_id} — {s}");
        },
        None if card.is_some() => {
            let _ = write!(section, "\n- {sender_id}");
        },
        None => return None,
    }
    let mut rails = None;
    let page_path = card.map(|c| {
        section.push_str("\n\n");
        section.push_str(&c.prose);
        rails = Some((sender_id.to_owned(), c.rails_source));
        c.source_path
    });
    Some(SpeakerCard {
        section,
        page_path,
        rails,
    })
}

/// Header of the block's third-party identity slot.
const HDR_PEOPLE_MENTIONED: &str = "PEOPLE THIS TURN IS ABOUT:";

/// The cards served for the third parties a turn names, and the bookkeeping
/// every other slot needs to not repeat them.
struct MentionedCards {
    /// The formatted section.
    section: String,
    /// Workdir-relative source paths, for the flat slot's page dedup.
    page_paths: Vec<String>,
    /// The same pages in the funnel's `(wiki, page)` terms, for
    /// `navigate`'s [`recall_nav::Served::pages`].
    served: Vec<(String, PathBuf)>,
    /// `(wiki_id, projected card markdown)` per card, for `navigate`'s
    /// `served_cards` — see [`SpeakerCard::rails`].
    rails: Vec<(String, String)>,
}

/// **`PEOPLE THIS TURN IS ABOUT`** — the identity card of each enrolled
/// person the turn names OR the search found facts about, served
/// deterministically.
///
/// Founder's ruling, 2026-08-04, on the measurement below: *«la scheda degli
/// utenti nominati va inserita deterministicamente nel recall, ci costa
/// caratteri, ma ci dà una porta di ingresso con le cose più importanti di un
/// utente che comunque vanno sempre prese in considerazione»*.
///
/// **What it repairs.** Similarity cannot be relied on to surface who someone
/// *is*. Measured on the live corpus with `examples/subject_gate.rs`, on the
/// founder's own phrase — *«sta sera cucino io, cosa faccio per carol?»* —
/// the flat slot returns ten facts about household expenses, meal prep and
/// baby clothes, and **neither the coeliac disease nor the pregnancy**. Reword
/// it to *«cosa cucino stasera per carol?»* and both appear (ranks 3 and 8):
/// the same question, reworded, gets a different answer, because the corpus
/// packs that turn's candidates into a 0.585–0.612 band where wording decides
/// the order. Following the walk does not save it either — what a real
/// navigator reaches is *«Mini Muffin (senza glutine e senza lattosio)»*, a
/// property of a muffin, never *«Carol è celiaca: non può consumare
/// glutine»*. That sentence, and the pregnancy, live on the card.
///
/// **Two gates, in this order, and neither costs a model call.**
///
/// First, **the turn names them** — [`recall::turn_subjects`], a word match
/// over the enrolled roster. Deliberately coarse: the card is what is worth
/// knowing about a person *whenever they come up*, so a passing mention is
/// not a false positive, it is a cheap piece of context. Mention order is
/// the selection where the list is cut (founder, 2026-08-09), so these lead.
///
/// Then, **the search found facts about them**: every hit carries its
/// subject, so the people the turn is really about arrive even when it names
/// none of them. *«Cosa può mangiare mia moglie?»* matches the roster nowhere
/// — and returns her coeliac facts, which say whose they are. This is the
/// gate that reads a paraphrase, and it is free: the hits are already in hand
/// here, ranked, from the search that ran before the classifier.
///
/// A person the turn names and the memory knows nothing about no longer
/// spends a seat on an empty card — they only reach the first gate, and the
/// second fills the seat with somebody the search actually found.
///
/// What bounds the slot is the **count**
/// ([`IngestPolicy::max_mentioned_cards`]), not a cleverer gate. It is the
/// only route to a card now: a card is not a link destination
/// (`compiler::recommended_link_targets`), so nothing else can carry a reader
/// to one.
///
/// **Projected for the reader, never the subject.** [`identity_card`] is
/// given `sender` — the person the block is being built for — so a region on
/// Carol's card that only Bob may read is redacted before Franz sees it.
/// This is the slot that makes 69a's projection invariant load-bearing rather
/// than a no-op, which is why its test had to become real before this shipped.
///
/// The sender is excluded: `WHO IS SPEAKING` already serves them.
async fn people_mentioned_section(
    pool: &SqlitePool,
    tree: &WikiTree,
    sender: &SenderContext,
    turn_text: &str,
    hits: &[recall::RecallHit],
    policy: &IngestPolicy,
) -> Option<MentionedCards> {
    if policy.max_mentioned_cards == 0 {
        return None;
    }
    let roster = match enrollment::list_users(pool).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: roster unavailable, no mentioned cards");
            return None;
        },
    };
    let mut subjects = recall::turn_subjects(turn_text, &sender.sender_id, &roster);
    // The second gate: whoever the returned facts are about, in the order the
    // search ranked them. A group-owned fact names no person and is skipped.
    for hit in hits {
        if let Principal::User(id) = &hit.subject_id {
            let id = id.to_lowercase();
            if !subjects.contains(&id) {
                subjects.push(id);
            }
        }
    }
    let mut section = String::from(HDR_PEOPLE_MENTIONED);
    let mut page_paths = Vec::new();
    let mut served = Vec::new();
    let mut rails = Vec::new();
    for subject in subjects {
        if served.len() >= policy.max_mentioned_cards {
            break;
        }
        if subject == sender.sender_id {
            continue;
        }
        let Some(handle) = WikiId::parse(&subject)
            .ok()
            .and_then(|id| tree.locate(&id).ok())
        else {
            continue;
        };
        let Some(card) = identity_card(pool, tree, &handle, sender, policy).await else {
            continue;
        };
        let _ = write!(section, "\n\n- {subject}\n{}", card.prose);
        page_paths.push(card.source_path);
        rails.push((subject.clone(), card.rails_source));
        served.push((subject, PathBuf::from(IDENTITY_PAGE)));
    }
    (!served.is_empty()).then_some(MentionedCards {
        section,
        page_paths,
        served,
        rails,
    })
}

/// What one served identity card yields.
struct IdentityCard {
    /// Injectable prose: projected, testata dropped, `[[wikilinks]]` flattened
    /// to plain names, fitted to the budget.
    prose: String,
    /// The page's workdir-relative source path.
    source_path: String,
    /// The **same** projection with its `[[wikilinks]]` intact and before the
    /// budget cut — the navigator's rails off this card
    /// ([`recall_nav::navigate`]'s `served_cards`). Kept separate from `prose`
    /// on purpose: the consumer's copy has nothing to navigate from, so its
    /// links are flattened, and a rail dropped by the character budget is
    /// still a page worth reaching.
    rails_source: String,
}

/// Read, project and prepare the sender's identity page for injection.
/// `None` — logged at debug, never fatal — when the page is absent,
/// unreadable, or renders to nothing for this reader.
async fn identity_card(
    pool: &SqlitePool,
    tree: &WikiTree,
    handle: &crate::wiki::WikiHandle,
    sender: &SenderContext,
    policy: &IngestPolicy,
) -> Option<IdentityCard> {
    if policy.max_sender_identity_chars == 0 {
        return None;
    }
    let abs = handle.abs_dir().join(IDENTITY_PAGE);
    let source_path = crate::wiki::workdir_relative_source_path(tree.workdir(), &abs);
    let raw = match std::fs::read_to_string(&abs) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::debug!(
                sender = %sender.sender_id,
                error = %err,
                "ingest: sender has no readable identity page, serving the summary line"
            );
            return None;
        },
    };
    // Fail closed on the ACL, exactly like the navigator: a page whose map
    // cannot load is skipped, never rendered on weaker gating. `_active`
    // keeps superseded regions whose bytes still sit on the page out.
    let db_acl = match fact_index::page_acl_map_active(pool, &source_path).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                sender = %sender.sender_id,
                error = %err,
                "ingest: identity-page ACL map unloadable, card not served"
            );
            return None;
        },
    };
    // The testata is card metadata, not prose — and its `topics:` list alone
    // runs to a hundred words, so dropping it is most of the injected size.
    let body = crate::wiki::MarkdownDoc::parse(&raw).map_or(raw, |doc| doc.body);
    let projected = crate::render::render_for_sender_segments(
        &body,
        &db_acl,
        &sender.sender_id,
        &sender.sender_groups,
    );
    // A page whose injected prose carries **no fact this reader may see** is
    // scaffolding, not a card: a freshly seeded `@profile.md` is a heading and
    // a sentence of connective tissue, and serving that on every turn
    // forever is noise. What earns the slot is the identity core the
    // classifier routed onto the page — so the test is on the *rendered*
    // page, not on the DB: a fact the index homes here whose region is not
    // on the page contributes nothing to read, and neither does one this
    // reader is denied (which is what 69d makes reachable). No fact
    // survived ⇒ fall back to the one-line summary.
    if !projected.segments.iter().any(|s| s.fact_id.is_some()) {
        tracing::debug!(
            sender = %sender.sender_id,
            "ingest: identity page carries no readable fact, serving the summary line"
        );
        return None;
    }
    let projected = projected.into_output().text;
    let rails_source = projected.trim().to_owned();
    let (prose, cut) = fit_paragraphs(
        plain_wikilinks(&rails_source).trim(),
        policy.max_sender_identity_chars,
    );
    if cut {
        // Firing means the card outgrew the ceiling REM curates it toward
        // (69c). It is a real event, not routine trimming: something the
        // author put last stopped reaching the turn.
        tracing::warn!(
            sender = %sender.sender_id,
            budget = policy.max_sender_identity_chars,
            "ingest: identity card exceeded its budget and was cut — it needs curating, not a bigger budget"
        );
    }
    (!prose.is_empty()).then_some(IdentityCard {
        prose,
        source_path,
        rails_source,
    })
}

/// Render `[[wikilinks]]` as plain text for an **injected** copy of a page.
///
/// The consumer's block has nothing to navigate from, so `[[franz/lnprint]]`
/// in injected prose is a broken affordance — but deleting it outright leaves
/// mutilated sentences ("*i dettagli dei miei lavori su e*"). So the link
/// becomes the name it points at: the `|display` alias when the author wrote
/// one, otherwise the last path segment — the page's own name, which is the
/// noun the sentence is about. Unclosed `[[` is left verbatim.
///
/// Injection only. On disk the links stay: they are the navigator's rails, and
/// the REM rewiring pass exists to *add* them, not to remove them.
pub(crate) fn plain_wikilinks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let (before, from_open) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = from_open.find("]]") else {
            out.push_str(from_open);
            return out;
        };
        let inner = &from_open[2..close];
        let label = match inner.split_once('|') {
            // A `|display` alias is presentation — it is exactly what the
            // author wanted a reader to see.
            Some((_, alias)) if !alias.trim().is_empty() => alias.trim(),
            _ => {
                let head = inner.split('|').next().unwrap_or(inner).trim();
                head.rsplit('/').next().unwrap_or(head).trim()
            },
        };
        out.push_str(label);
        rest = &from_open[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Fit `text` to `max_chars` on a **paragraph** boundary, never mid-sentence.
///
/// The prose sibling of [`fit_bullets`], with the same escape hatch: a first
/// paragraph that alone exceeds the whole budget is char-truncated with an
/// ellipsis, so a page with content is never rendered empty. Returns the kept
/// text and whether anything was dropped — the caller warns on a cut, because
/// here a cut means curation failed upstream.
fn fit_paragraphs(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let sep = usize::from(!out.is_empty()) * 2; // "\n\n"
        let cost = para.chars().count() + sep;
        if used + cost > max_chars {
            if out.is_empty() {
                let room = max_chars.saturating_sub(1).max(1);
                out.extend(para.chars().take(room));
                out.push('…');
            }
            return (out, true);
        }
        if sep > 0 {
            out.push_str("\n\n");
        }
        out.push_str(para);
        used += cost;
    }
    (out, true)
}

/// Inject the behaviour rules in force — WITH their `fact_id`s and scope
/// tokens — into the classifier prompt, so the model can revise one by
/// setting an extraction's `supersede_target` to its id (exactly as it
/// supersedes a recalled fact) and restate the right `behaviour_scope` when
/// it does. Mirrors the `sender_rules` injection. No-op when empty.
fn push_behaviour_rules_section(out: &mut String, rules: &[(FactId, String, BehaviourScope)]) {
    if rules.is_empty() {
        return;
    }
    out.push_str(
        "\nagent_behaviour_rules (the standing directives in force for this user, each with \
         its scope; to revise one, set an extraction's supersede_target to its fact_id):\n",
    );
    for (id, body, scope) in rules {
        out.push_str("  - [");
        out.push_str(id.as_str());
        out.push_str("] (");
        out.push_str(scope.as_hint());
        out.push_str(") ");
        out.push_str(body.trim());
        out.push('\n');
    }
}

/// Resolve a behaviour-rule extraction's `supersede_target` against the
/// behaviour rules in force this turn — any of the three scope sources
/// (anti-hallucination, like [`validate_supersede_target`] for recalled
/// facts). Returns the target WITH its scope, so the dispatch can admin-gate
/// the revision of an agent-wide rule. `None` when unset, or when the model
/// named an id it was not shown (logged, then treated as additive).
fn behaviour_supersede_target(
    unit: &CaptureUnit<'_>,
    behaviour_rules: &[(FactId, String, BehaviourScope)],
) -> Option<(FactId, BehaviourScope)> {
    let raw = unit
        .supersede_target
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let id = FactId::parse(raw).ok()?;
    if let Some((_, _, scope)) = behaviour_rules.iter().find(|(rid, _, _)| *rid == id) {
        Some((id, *scope))
    } else {
        tracing::warn!(
            supersede_target = raw,
            "ingest: behaviour_rule supersede_target not among the user's known rules — additive"
        );
        None
    }
}

// ---------- Internal: snippet formatting ----------

/// Stable header of the recall block's flat-hits section.
const HDR_RELEVANT_MEMORY: &str =
    "RELEVANT MEMORY (recalled facts — the dates are signals, not filters):";

/// Render the `RELEVANT MEMORY` section: the deterministic flat hit-list,
/// trust-tagged, with the fresh (un-promoted) hits in their own labelled
/// sub-slot. Two filters keep the section honest:
///
/// - `navigated_paths` — workdir-relative source paths of the pages the
///   navigator already injected below: a durable hit homed on one of them
///   is dropped (its content rides the page prose; injecting it twice is
///   noise). Fresh hits have no published page and are never deduped.
/// - rules-page hits are skipped — standing directives reach the consumer
///   through the dedicated `rules` field only, never as recalled memory.
///
/// ## The turn-level relevance floor (card 61, §37-39)
///
/// `relevance_floor` gates the **promoted** hits as a group, not one at a
/// time: it is compared against the MAXIMUM score among the turn's
/// promoted (non-fresh) hits — every `RecallHit` in `hits` with
/// `fresh == false`, before the two filters above ever run, so the gate's
/// outcome depends only on the turn's own recall scores, never on which
/// pages the navigator happened to open this same turn or on whether a
/// hit happens to be a rules-page hit. A per-hit threshold cannot do this
/// job — measured (§37) on two real turns: on the one that NEEDED its
/// recall the right answer scored `0.4813` / `0.4811`, while on the one
/// that needed none the noise it recited ran to `0.4306` — the bands
/// overlap, so any per-hit cut that removes one removes the other. Below
/// the floor, the turn's flat recall has nothing to say and the promoted
/// section is not opened at all (not "the weak hits are trimmed" — no
/// promoted hit renders, however strong). At or above it, every promoted
/// hit renders, including ones individually below the floor — the
/// guarantee the first turn needs, since its `0.4813` hit clears only
/// because the turn's best was `0.5474`. See
/// [`recall::DEFAULT_RELEVANCE_FLOOR`] for the measurement.
///
/// Deliberately **NOT** gated by `relevance_floor` (§39):
/// - the **fresh** (un-promoted) captures below — a different signal
///   (things said a few turns ago, not durable memory) that keeps
///   rendering even when every promoted hit is dropped;
/// - the caller's `UPCOMING` (due-soon) slot — time-driven by design,
///   deliberately never similarity-gated;
/// - the project-docs slot (`project_docs`) — it already has its own
///   floor ([`recall::DEFAULT_SIGNPOST_FLOOR`] /
///   [`recall::DEFAULT_SMART_CORPUS_FLOOR`]);
/// - the flat hits' other two uses (the classifier's input, the
///   navigator's RAG seeds) — both happen upstream of this function and
///   read `hits` unfiltered; this floor decides only whether *this
///   render* shows a promoted hit, never whether recall found one.
///
/// `relevance_floor <= 0.0` is the off switch — same idiom as
/// [`recall::admitted_smart_wikis`] — and renders every promoted hit
/// exactly as before the floor existed.
///
/// `None` when nothing survives — the section is omitted entirely.
/// Floor of the classifier's vote, and its ceiling
/// ([`FACT_SCORE_MAX`]). Founder's ruling, 2026-08-03: **±10 %**.
///
/// A band and not a free hand, because the classifier is being asked to
/// revise a number it can see. On this corpus the 1st and 10th hits of a turn
/// sit **0.1026** apart, so ±10 % on a typical `0.45` hit is `±0.045` —
/// roughly 45 % of the whole band. A strong voice by construction, which is
/// the intent; anything wider would not be a revision but a replacement.
pub const FACT_SCORE_MIN: f32 = 0.90;
/// Ceiling of the classifier's vote. See [`FACT_SCORE_MIN`].
pub const FACT_SCORE_MAX: f32 = 1.10;

/// Applies the classifier's vote and re-sorts, returning a NEW list and
/// leaving the caller's untouched.
///
/// **A separate list is the point, not an implementation detail.** The same
/// `Vec<RecallHit>` is handed to the navigator's entry fan, where a `rag`
/// seed's weight IS the hit's score — so re-scoring in place would also
/// reorder the pages the walk STARTS from. That is the more valuable half and
/// the riskier one, and it waits on the reproducibility measurement: a score
/// that lands differently on each call is worse than cosine, which at least
/// is wrong the same way every time.
///
/// Anti-hallucination is inherited, not re-invented: an id the model did not
/// see is **dropped**, exactly as `supersede_target`, `closures`,
/// `validity_edits` and `acl_changes` drop theirs. A missing or non-finite
/// multiplier is dropped the same way, so a malformed array is worth exactly
/// as much as an absent one.
fn apply_classifier_vote(hits: &[RecallHit], scores: &[LlmFactScore]) -> Vec<RecallHit> {
    let mut out = hits.to_vec();
    if scores.is_empty() {
        return out;
    }
    let mut applied = 0usize;
    for score in scores {
        let (Some(target), Some(multiplier)) = (score.target.as_deref(), score.multiplier) else {
            continue;
        };
        if !multiplier.is_finite() {
            continue;
        }
        let Some(hit) = out.iter_mut().find(|h| h.fact_id.as_str() == target) else {
            continue;
        };
        hit.score *= multiplier.clamp(FACT_SCORE_MIN, FACT_SCORE_MAX);
        applied += 1;
    }
    if applied == 0 {
        return out;
    }
    // Stable by construction: `sort_by` keeps the recall order among hits the
    // classifier left alone, so a turn it declines to judge renders exactly as
    // it did before.
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    tracing::debug!(
        applied,
        offered = scores.len(),
        "ingest: classifier voted on the recalled facts"
    );
    out
}

fn format_snippet(
    hits: &[RecallHit],
    navigated_paths: &[String],
    project_docs: &[recall::SectionHit],
    relevance_floor: f32,
) -> Option<String> {
    let keep = |h: &&RecallHit| -> bool {
        if crate::wiki::is_rules_page(&h.source_path) {
            return false;
        }
        h.fresh || !navigated_paths.iter().any(|p| p == &h.source_path)
    };
    // Turn-level gate: the MAXIMUM score among ALL promoted hits (not the
    // subset that survives `keep`) decides whether the promoted section
    // opens at all. See "The turn-level relevance floor" above.
    let promoted_gate_open = relevance_floor <= 0.0
        || hits
            .iter()
            .filter(|h| !h.fresh)
            .map(|h| h.score)
            .fold(f32::NEG_INFINITY, f32::max)
            >= relevance_floor;
    let mut out = String::new();
    // Promoted (durable) facts first — withheld as a group when the gate
    // above is shut; never trimmed hit by hit.
    if promoted_gate_open {
        for h in hits.iter().filter(|h| !h.fresh).filter(keep) {
            out.push_str("\n- (");
            out.push_str(&h.wiki_id);
            out.push_str(") ");
            out.push_str(&h.text);
            push_trust_tag(&mut out, h);
        }
    }
    // Mid-range bridge: un-promoted buffered captures in a labelled slot, so the
    // agent reads them as recent and provisional (may be superseded soon).
    let mut fresh = hits.iter().filter(|h| h.fresh).filter(keep).peekable();
    if fresh.peek().is_some() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("\nRecent (not yet consolidated):");
        for h in fresh {
            out.push_str("\n- (");
            out.push_str(&h.wiki_id);
            out.push_str(") ");
            out.push_str(&h.text);
            push_trust_tag(&mut out, h);
        }
    }
    // Project docs the message NAMED — a separate, labelled slot so the
    // classifier reads it as reference material, not as something the
    // user just told it. The label is load-bearing: without it a
    // documentation paragraph in the recall block looks exactly like a
    // recalled fact, and the classifier would happily file it back as a
    // new fact about the sender (the ANTI-LOOP rule in the prompt).
    if !project_docs.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("\nProject documentation (reference — never file this as a fact):");
        for d in project_docs {
            out.push_str("\n- (");
            out.push_str(&d.wiki_id);
            out.push_str(" · ");
            out.push_str(page_of(&d.source_path));
            out.push_str(") ");
            out.push_str(&d.text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(format!("{HDR_RELEVANT_MEMORY}{out}"))
    }
}

/// The page part of a workdir-relative `source_path`, for the citation in
/// the project-docs slot: `wikis/franz/acmesigns/architecture/X.md` →
/// `architecture/X.md`. Falls back to the whole path when the shape is
/// unexpected — a slightly long citation beats a wrong one.
fn page_of(source_path: &str) -> &str {
    source_path
        .strip_prefix("wikis/")
        .and_then(|rest| rest.split_once('/'))
        .map_or(source_path, |(_subject, rest)| {
            rest.split_once('/').map_or(rest, |(_wiki, page)| page)
        })
}

/// In-band trust tag on a snippet line: when the fact was noted and —
/// when one exists — the end of its validity window, dates only. No
/// clock and no judgment here: the raw window is the signal, and the
/// consumer model (which knows today's date) reasons about staleness
/// itself — validity is a signal, never a filter.
fn push_trust_tag(out: &mut String, h: &RecallHit) {
    out.push_str(" [noted ");
    out.push_str(date_part(&h.created_at));
    if let Some(vt) = &h.valid_to {
        out.push_str(" · valid to ");
        out.push_str(date_part(vt));
    }
    out.push(']');
}

/// First 10 chars of an ISO 8601 timestamp — the date. Falls back to
/// the whole string when shorter (degraded data beats a panic).
fn date_part(iso: &str) -> &str {
    iso.get(..10).unwrap_or(iso)
}

// ---------- Internal: recall-block tail (navigation + due-soon) ----------

/// Navigation seeds the classifier already produced for this turn.
#[derive(Default)]
struct NavSeeds {
    topics: Vec<String>,
    subjects: Vec<Principal>,
}

/// Union of the plan's capture-unit `topics` plus their parsed
/// `subject_id`s. For a recall intent both are typically empty, which
/// leaves the principal + RAG fan — the designed degenerate case.
fn nav_seeds(plan: &LlmIngestPlan) -> NavSeeds {
    let mut topics: Vec<String> = Vec::new();
    let mut subjects: Vec<Principal> = Vec::new();
    for unit in plan.capture_units() {
        for t in unit.topics {
            if !topics.iter().any(|seen| seen == t) {
                topics.push(t.clone());
            }
        }
        if let Some(subject_str) = unit.subject_id
            && let Ok(p) = Principal::from_str(subject_str)
            && !subjects.contains(&p)
        {
            subjects.push(p);
        }
    }
    NavSeeds { topics, subjects }
}

/// Stable header of the recall block's navigated-prose section.
const HDR_NAVIGATED_PAGES: &str = "NAVIGATED PAGES (prose collected from memory pages this turn):";

/// What the navigation tail brings back: the formatted `NAVIGATED PAGES`
/// section (when any prose was collected) plus the structured route — fan
/// and funnel journal — the recall trace persists.
struct NavigatedTail {
    /// The formatted section; `None` when nothing was collected.
    section: Option<String>,
    /// Workdir-relative source paths of the injected pages, so the flat
    /// slot can drop hits whose page prose already rides below
    /// ([`format_snippet`] dedup).
    page_paths: Vec<String>,
    /// The entry-point fan that fed the funnel.
    entries: Vec<recall_nav::EntryPoint>,
    /// The funnel outcome (fragments, hop journal, stop reason).
    outcome: recall_nav::NavigationOutcome,
}

/// Run gather → navigate and format the `NAVIGATED PAGES` section.
/// Everything here is soft: a gather or funnel failure logs a warning
/// and returns `None` — the turn survives on the flat snippet.
///
/// `served_identity` is the sender's identity page, when `WHO IS SPEAKING`
/// served it this turn. It is handed to the funnel as
/// **already visited**, so the walk neither offers nor opens it by any route
/// — fan seed, card rail or `[[wikilink]]`. Founder's ruling,
/// 2026-08-03: *«la pagina di identità la escluderei dai risultati del rag,
/// visto che già c'è nel recall… non ci frega dell'indice se col rag
/// arriviamo già sulle pagine giuste»*. The page's prose is in the block
/// whatever the navigator does, and the hub's routing is not needed when the
/// recalled facts already name the pages that answer the turn.
// One over the threshold, and every argument is a distinct borrow the funnel
// needs; splitting them into a struct would hide which are per-turn inputs.
#[allow(clippy::too_many_arguments)]
async fn navigated_tail(
    pool: &SqlitePool,
    tree: &WikiTree,
    nav_llm: &dyn LlmBackend,
    sender: &SenderContext,
    turn_text: &str,
    seeds: &NavSeeds,
    rag_hits: &[RecallHit],
    seed_tail: &[RecallHit],
    doors: usize,
    nav_policy: &recall_nav::NavigatorPolicy,
    served: recall_nav::Served<'_>,
) -> Option<NavigatedTail> {
    // **One door per page** (founder, 2026-08-21). The block's hits come
    // first — they are the best-ranked — and `seed_tail` supplies a page for
    // each slot two hits on one page would otherwise have wasted. This is
    // the ONLY consumer of the deeper list: everything else this turn reads
    // `rag_hits` unchanged, because what the top-K is for elsewhere
    // (supersede targets, closures, validity edits, the block itself) is a
    // different question from which pages are worth opening.
    let seeding: Vec<RecallHit> = recall_nav::hits_as_doors(
        &rag_hits
            .iter()
            .chain(seed_tail)
            .cloned()
            .collect::<Vec<_>>(),
        doors,
    );
    let entries = match recall_nav::gather_entry_points(
        pool,
        tree,
        sender,
        &seeds.topics,
        &seeding,
        // Situational seeds arrive with the host adapter (context model).
        &[],
    )
    .await
    {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: entry-point gather failed, skipping navigation");
            return None;
        },
    };
    if entries.is_empty() {
        // No fan, no completion spent — but the attempt is trace-worthy
        // (the default outcome's stop reason is `empty_fan`).
        return Some(NavigatedTail {
            section: None,
            page_paths: Vec::new(),
            entries,
            outcome: recall_nav::NavigationOutcome::default(),
        });
    }
    let outcome = match recall_nav::navigate(
        pool, tree, nav_llm, sender, turn_text, &entries, nav_policy, served,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: navigation failed, continuing without it");
            return None;
        },
    };
    if outcome.fragments.is_empty() {
        return Some(NavigatedTail {
            section: None,
            page_paths: Vec::new(),
            entries,
            outcome,
        });
    }
    tracing::info!(
        fragments = outcome.fragments.len(),
        hops = outcome.hops,
        truncated = outcome.truncated,
        "ingest: navigation done"
    );
    let mut out = String::from(HDR_NAVIGATED_PAGES);
    let mut page_paths = Vec::new();
    for f in &outcome.fragments {
        // The page's workdir-relative source path: the flat-slot dedup key,
        // and the freshness lookup key. Best-effort — a vanished wiki just
        // skips both.
        let source_path = fragment_source_path(tree, f);
        let _ = write!(out, "\n\n({}/{}", f.wiki_id, f.page.display());
        if let Some(sp) = &source_path {
            page_paths.push(sp.clone());
            // In-band freshness: the page's most recent fact mutation. Soft
            // best-effort — a lookup failure just drops the annotation.
            if let Some(updated) = fact_index::latest_page_activity(pool, &f.wiki_id, sp)
                .await
                .ok()
                .flatten()
            {
                let _ = write!(out, " · updated {}", date_part(&updated));
            }
        }
        let _ = write!(out, ")\n{}", f.text.trim_end());
        // A page the budget cut says so, on the page. Without it a list page
        // arrives ending at an arbitrary character and reads as a complete
        // list — the reader has no way to know it is looking at part of one,
        // and neither does the agent answering from it.
        if f.truncated {
            out.push_str("\n[… this page is longer — the rest did not fit this turn]");
        }
    }
    Some(NavigatedTail {
        section: Some(out),
        page_paths,
        entries,
        outcome,
    })
}

/// Workdir-relative source path of a navigated fragment's page.
/// Best-effort: an unparseable wiki id or a vanished wiki yields `None`.
fn fragment_source_path(
    tree: &WikiTree,
    fragment: &recall_nav::NavigatedFragment,
) -> Option<String> {
    let wid = WikiId::parse(&fragment.wiki_id).ok()?;
    let handle = tree.locate(&wid).ok()?;
    let source_path = handle.rel_dir().join(&fragment.page);
    source_path.to_str().map(str::to_owned)
}

/// Stable header of the recall block's due-soon section.
const HDR_UPCOMING: &str = "UPCOMING (dated items in memory that close soon):";

/// Pull the due-soon slot and format the `UPCOMING` section: facts whose
/// validity window closes inside the operator horizon, most imminent
/// first. Soft-fails to `None`. Returns the hits alongside the section so
/// the recall trace can journal them structured.
async fn due_soon_section(
    pool: &SqlitePool,
    sender: &SenderContext,
    policy: &IngestPolicy,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(String, Vec<RecallHit>)> {
    if policy.due_soon_top_k == 0 {
        return None;
    }
    let horizon = chrono::Duration::hours(i64::from(policy.due_soon_horizon_hours));
    let hits = match recall::recall_due_soon(pool, sender, now, horizon, policy.due_soon_top_k)
        .await
    {
        Ok(hits) => hits,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: due-soon recall failed, continuing without it");
            return None;
        },
    };
    if hits.is_empty() {
        return None;
    }
    let mut out = String::from(HDR_UPCOMING);
    for h in &hits {
        let _ = write!(out, "\n- ({}) {}", h.wiki_id, h.text);
        if let Some(due) = &h.valid_to {
            let _ = write!(out, " [due {due}]");
        }
    }
    Some((out, hits))
}

/// Join the recall-block sections in their canonical order: `WHO YOU ARE`,
/// `WHO IS SPEAKING`, `YOUR RECENT HISTORY WITH THIS USER`,
/// `RELEVANT MEMORY`, `NAVIGATED PAGES`, `UPCOMING`. An empty section is
/// omitted entirely (header included); all-empty → `None`, preserving the
/// "no context to surface" contract.
///
/// The recall block is recalled MEMORY only — the `YOUR RULES` standing
/// directives ride the dedicated `rules` field ([`assemble_rules_block`]),
/// which the host injects adjacent to this block. The agent's own identity
/// leads: it frames the recalled facts it reads below.
fn assemble_recall_block(
    who_you_are: Option<String>,
    who_is_speaking: Option<String>,
    people_mentioned: Option<String>,
    history: Option<String>,
    relevant: Option<String>,
    navigated: Option<String>,
    upcoming: Option<String>,
) -> Option<String> {
    let sections: Vec<String> = [
        who_you_are,
        who_is_speaking,
        // Directly after the speaker: both are "who is in this conversation",
        // and both are served rather than found (69a, 69d).
        people_mentioned,
        history,
        relevant,
        navigated,
        upcoming,
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.trim().is_empty())
    .collect();
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Assemble the dedicated `rules` field — standing **behaviour directives**,
/// kept structurally apart from the recalled memory in `context_snippet`.
/// A one-shot `notice` (e.g. an agent-wide change refused for a
/// non-admin this turn) leads, then the served user's `behaviour` rules (how to
/// converse / operate with them). `None` when both are empty.
fn assemble_rules_block(notice: Option<String>, behaviour: Option<String>) -> Option<String> {
    let sections: Vec<String> = [notice, behaviour]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Journal the route this turn's recall took ([`crate::recall_trace`] — the
/// admin Traces page). Best-effort telemetry by contract: a journal failure
/// is logged and never touches the turn.
#[allow(clippy::too_many_arguments, reason = "one turn's full recall context")]
async fn record_ingest_trace(
    pool: &SqlitePool,
    request: &IngestRequest,
    intent: IntentKind,
    seed_mode: &str,
    seeds: &NavSeeds,
    recall_hits: &[RecallHit],
    nav_tail: Option<&NavigatedTail>,
    due_soon: Option<&[RecallHit]>,
    policy: &IngestPolicy,
    injected_block: Option<&str>,
    rules_block: Option<&str>,
    took: std::time::Duration,
) {
    use crate::recall_trace::{self, RecallTrace, TraceEntryPoint, TraceHit, TraceSource};

    let trace = RecallTrace {
        version: recall_trace::TRACE_PAYLOAD_VERSION,
        consumer: None,
        turn_text: recall_trace::cap_turn_text(&request.text),
        intent: Some(intent.as_str().to_owned()),
        seed_mode: seed_mode.to_owned(),
        topics: seeds.topics.clone(),
        subjects: seeds.subjects.iter().map(ToString::to_string).collect(),
        flat_hits: recall_hits
            .iter()
            .filter(|h| !h.fresh)
            .map(TraceHit::from_hit)
            .collect(),
        fresh_hits: recall_hits
            .iter()
            .filter(|h| h.fresh)
            .map(TraceHit::from_hit)
            .collect(),
        due_soon: due_soon
            .unwrap_or_default()
            .iter()
            .map(TraceHit::from_hit)
            .collect(),
        entry_points: nav_tail
            .map(|t| t.entries.iter().map(TraceEntryPoint::from_entry).collect())
            .unwrap_or_default(),
        hops: nav_tail
            .map(|t| t.outcome.trace.clone())
            .unwrap_or_default(),
        nav_stop: nav_tail.map(|t| t.outcome.stop.as_str().to_owned()),
        char_budget: policy.nav.char_budget,
        chars_collected: nav_tail.map_or(0, |t| {
            t.outcome.fragments.iter().map(|f| f.text.len()).sum()
        }),
        truncated: nav_tail.is_some_and(|t| t.outcome.truncated),
        injected_block: injected_block.map(str::to_owned),
        rules_block: rules_block.map(str::to_owned),
        took_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
    };
    if let Err(err) = recall_trace::record_trace(
        pool,
        TraceSource::Ingest,
        &request.sender_id,
        &trace,
        policy.trace_retention_days,
    )
    .await
    {
        tracing::warn!(error = %err, "ingest: recall-trace journal write failed (ignored)");
    }
}

/// Output cap of the classifier call: room for a plan of a dozen
/// extracted facts, each a verbose per-fact object.
const CLASSIFIER_MAX_TOKENS: u32 = 4096;
/// The cap of the one retry made when a reply stops at
/// [`CLASSIFIER_MAX_TOKENS`] without parsing.
const CLASSIFIER_MAX_TOKENS_WIDENED: u32 = 8192;

// ---------- Internal: fallback response builder ----------

/// The response of a turn the engine could not serve — every caller of
/// this reached it because the model was unreachable or its plan could
/// not be read or applied, so nothing was stored. The seed says exactly
/// that; the recall block still carries what the search found.
fn fallback_response(
    request: &IngestRequest,
    recall_hits: &[RecallHit],
    policy: &IngestPolicy,
    took: std::time::Duration,
    llm_used: bool,
) -> IngestResponse {
    let context_snippet = format_snippet(recall_hits, &[], &[], policy.relevance_floor);
    let suggested_seed = match request.context_hint {
        ContextHint::DashboardCommand => Some(policy.structural_suggested_seed.clone()),
        _ => Some(policy.degraded_suggested_seed.clone()),
    };
    IngestResponse {
        intent: IntentKind::Skip,
        context_snippet,
        // The degraded path computes no behaviour rules — the dedicated channel
        // is silent, exactly as it was when it rode `context_snippet`.
        rules: None,
        suggested_seed,
        // The degraded path serves no window either: it may not even have
        // a live pool at hand, and a missing section is the contract's
        // "nothing for you this turn".
        recent_window: None,
        capture_id: None,
        needs_disambig: false,
        disambig_candidates: Vec::new(),
        llm_used,
        took_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
    }
}

// ---------- Public orchestrator ----------

/// `_internal.wiki_ingest_message` — the flagship MCP tool.
///
/// See module docs for the full pipeline. Always returns an
/// [`IngestResponse`] the agent can render into a reply: every soft
/// failure (LLM down, malformed plan, invalid capture plan) demotes to
/// `IntentKind::Skip` with a canned seed rather than bubbling.
///
/// `navigator` is the recall navigator's backend (the `navigator` config
/// slot) — `None` turns navigation off and the recall block falls to
/// the flat snippet + due-soon slot. Kept separate from `llm` because
/// the two are different slots by design: the classifier wants the
/// fast workhorse profile, the navigator a strong-but-cheap one.
///
/// # Errors
///
/// - [`IngestError::EmptyText`] when `request.text` is blank.
/// - [`IngestError::Recall`] when the recall layer surfaces an
///   infrastructure-level error (the soft path absorbs the rest).
/// - [`IngestError::Capture`] when the capture layer surfaces a
///   filesystem / database / embedder failure post-validation.
/// - [`IngestError::Wiki`] when enumerating the wiki tree fails.
#[allow(clippy::too_many_lines)] // orchestrator reads top-to-bottom, splitting hides the flow
pub async fn wiki_ingest_message(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    navigator: Option<&dyn LlmBackend>,
    mut request: IngestRequest,
    policy: &IngestPolicy,
) -> Result<IngestResponse> {
    let start = std::time::Instant::now();
    if request.text.trim().is_empty() {
        return Err(IngestError::EmptyText);
    }
    // One semantic clock per turn: every time-anchored judgment below
    // (the classifier's `current_time:` anchor, the due-soon window)
    // reads this single instant, so a backlog replay that sets
    // `metadata.occurred_at` re-lives the turn at utterance time.
    let turn_now = request.turn_now();

    // One scoped lookup feeds both the ACL `SenderContext` (bare ids)
    // and the prompt's `sender_groups` section (id + scope prose).
    // Deriving the ids from the scoped pairs keeps this to a single
    // round-trip; `groups_for` stays the lean scan for the hot ACL paths
    // elsewhere (recall, `wiki_search`, dashboard).
    let sender_groups_scoped = enrollment::groups_with_scope_for(pool, &request.sender_id)
        .await
        .map_err(|e| IngestError::Recall(RecallError::Db(e)))?;
    let sender_ctx = SenderContext {
        sender_id: request.sender_id.clone(),
        sender_groups: sender_groups_scoped
            .iter()
            .map(|(id, _)| id.clone())
            .collect(),
    };
    tracing::info!(
        sender_id = sender_ctx.sender_id,
        sender_groups = ?sender_ctx.sender_groups,
        context_hint = request.context_hint.as_str(),
        text_len = request.text.len(),
        occurred_at = ?request.metadata.occurred_at,
        "ingest: start"
    );

    // Step 1 — recall context. Soft-fail to empty hits so a transient
    // index issue does not kill the entire turn.
    // One search, two consumers with different appetites: the block takes the
    // top `recall_top_k`, the entry fan is allowed to look further down for
    // doors on pages the block's own hits did not already name
    // ([`recall_nav::hits_as_doors`]). Splitting here rather than searching
    // twice — the scan reads the whole readable corpus either way, so the
    // extra rows are carried out of the same query.
    //
    // Both halves are replaced further down on a turn whose classifier
    // returns a `completed_message`: the same division, re-made on the
    // sentence that says what the turn is about.
    let mut ranked = match recall::wiki_recall(
        pool,
        Arc::clone(&embedder),
        &request.text,
        policy.nav_seed_depth.max(policy.recall_top_k),
        fact_index::FactFilters::default(),
        &sender_ctx,
    )
    .await
    {
        Ok(hits) => hits,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: recall failed, continuing without context");
            Vec::new()
        },
    };
    let mut seed_tail: Vec<RecallHit> = ranked.split_off(ranked.len().min(policy.recall_top_k));
    let mut recall_hits = ranked;
    // Cross-consumer recent window, fetched HERE rather than at the
    // end of the turn: it is served back to the consumer as its own field, but
    // it is also half the answer to "what is this agent already looking at",
    // which the fresh slot below needs before it decides what to repeat. The
    // fetch is a read and moving it earlier changes nothing about it; the
    // WRITE (recording this turn's own exchange) stays at the end, so a
    // requester is still never handed the message it just sent.
    //
    // Fresh-session resume (43j, hermes-agent#43008): a requester that carried
    // NO local window has no context a served thread could duplicate — a
    // reborn/blank session, or a consumer that keeps no window at all. Serve it
    // every surface, its own included: its own channel's tail is exactly the
    // thread the user is continuing. A requester that brought its window gets
    // only the other surfaces.
    let consumer_surface = request.consumer_id.clone().unwrap_or_default();
    let surface_filter = if request.recent_messages.is_empty() {
        crate::recent_window::SurfaceFilter::IncludeRequester
    } else {
        crate::recent_window::SurfaceFilter::ExcludeRequester
    };
    let window_entries = match crate::recent_window::fetch_window(
        pool,
        &request.sender_id,
        &consumer_surface,
        request.metadata.channel.as_deref(),
        surface_filter,
        policy.recent_window_ttl_hours,
        policy.recent_window_entries,
    )
    .await
    {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "recent-window: fetch failed (served empty)");
            Vec::new()
        },
    };
    let recent_window = format_recent_window(&window_entries, turn_now, policy);

    // The classifier cannot resolve «i suoi reni» without the conversation,
    // and a consumer is not obliged to send one. When it sends none the window
    // just fetched already holds the requester's own surface — that is what
    // `SurfaceFilter::IncludeRequester` above is for — so the block is filled
    // from it rather than left empty. The consumer's own copy still WINS when
    // it exists: the engine holds only the turns it was shown, so a consumer
    // that calls memory on some turns and not others has the fuller record of
    // its own surface.
    if request.recent_messages.is_empty() {
        request.recent_messages = window_entries
            .iter()
            .rev()
            .take(policy.max_recent_messages)
            .map(|e| RecentMessage {
                role: e.author,
                text: e.text.clone(),
                timestamp: Some(e.occurred_at.clone()),
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
    }

    // What the agent is ALREADY being shown this turn, as message
    // fingerprints: its own replayed transcript plus the window just fetched.
    // The fresh slot uses it to not restate, as an extracted fact, something
    // the agent is reading in the message that produced it.
    let already_in_context: std::collections::HashSet<String> = request
        .recent_messages
        .iter()
        .map(|m| capture_buffer::origin_fingerprint(&m.text))
        .chain(
            window_entries
                .iter()
                .map(|e| capture_buffer::origin_fingerprint(&e.text)),
        )
        .collect();

    // Mid-range bridge: also surface un-promoted buffered captures in a
    // separate "fresh" slot, so material captured but not yet promoted by the
    // light dream stays recall-able. Soft-fails to no fresh hits — never kills
    // the turn. Scoped to the ingest (conversational) path on purpose:
    // `wiki_recall` stays promoted-only for the dashboard, whose edit/locate
    // flows assume published-page offsets the buffer lacks.
    let fresh_hits = match recall::recall_fresh_captures(
        pool,
        embedder.as_ref(),
        &request.text,
        &sender_ctx,
        policy.recall_fresh_top_k,
        &already_in_context,
    )
    .await
    {
        Ok(hits) => hits,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: fresh-capture recall failed, continuing without it");
            Vec::new()
        },
    };
    recall_hits.extend(fresh_hits);

    // Project-docs slot, first half. The turn's recall above is
    // facts-only — a conversation must not be buried under project
    // documentation. A message that NAMES a project ("how does this
    // AcmeSigns thing work?") has declared its own scope, so its
    // docs are pulled here, BEFORE the classifier, and are in front of it
    // when it decides the intent. The second half — a project the turn
    // never named, reached through a signpost — needs a judgement the
    // classifier has not made yet, so it runs after it (step 5b).
    // Soft-fails to an empty slot.
    let docs_slot =
        recall::SlotBudget::new(policy.project_docs_top_k, policy.project_docs_char_budget);
    let mut project_docs = match recall::recall_named_project_docs(
        pool,
        Arc::clone(&embedder),
        &request.text,
        docs_slot,
        &sender_ctx,
    )
    .await
    {
        Ok(hits) => hits,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: project-docs recall failed, continuing without it");
            Vec::new()
        },
    };
    tracing::debug!(
        recall_hits = recall_hits.len(),
        project_docs = project_docs.len(),
        "ingest: recall done"
    );

    // Builtin guest pseudo-identity — the unidentified-human sender. The
    // turn is EPHEMERAL by construction: recall above already ran with the
    // guest sender context (the ACL grants guest only the public slice —
    // the `global` arm is the only principal it can ever match), but no
    // classifier runs and nothing is written: no capture, no closures, no
    // behaviour rules, no buffer row. An identity boundary like redaction,
    // not a semantic gate — what the *consumer* says this turn stays the
    // consumer's judgment, steered by the `rules` directive below.
    if enrollment::is_guest(&request.sender_id) {
        tracing::info!(
            recall_hits = recall_hits.len(),
            "ingest: guest turn — ephemeral, classifier skipped, nothing filed"
        );
        let context_snippet =
            format_snippet(&recall_hits, &[], &project_docs, policy.relevance_floor);
        record_ingest_trace(
            pool,
            &request,
            IntentKind::Skip,
            "guest",
            &NavSeeds::default(),
            &recall_hits,
            None,
            None,
            policy,
            context_snippet.as_deref(),
            Some(GUEST_RULES_NOTICE),
            start.elapsed(),
        )
        .await;
        return Ok(IngestResponse {
            intent: IntentKind::Skip,
            context_snippet,
            rules: Some(GUEST_RULES_NOTICE.to_owned()),
            // No canned seed: the fallback's "I've noted that." would be
            // a lie on a turn that stores nothing.
            suggested_seed: None,
            // Guests are not durable users: nothing is buffered for them
            // and nothing is served to them.
            recent_window: None,
            capture_id: None,
            needs_disambig: false,
            disambig_candidates: Vec::new(),
            llm_used: false,
            took_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
    }

    // Step 2 — enumerate the wikis a capture may be filed into. **Internal
    // only**: this list never reaches the prompt, so it is uncapped — a cap
    // here would bound nothing a model reads.
    // A wiki whose per-wiki smart flag (read from `_meta.md`) is `true` is
    // managed authoritatively by the user's smart consumer via `wiki_admin_*`
    // and is not writable through this orchestrator, so `available_wikis`
    // drops those; every arm of [`derive_target_wiki`] then inherits that
    // exclusion, and the agent-wiki guard inside `validate_capture_plan`
    // reads `is_agent` from the same rows.
    //
    // Everything here is therefore the standard-wiki path: its captures route
    // into the captures buffer (`crate::capture_buffer`) for the compiler
    // instead of the published `.md`. "Standard" is exactly "not smart":
    // the per-wiki flag is the whole test.
    let available: Vec<AvailableWiki> = available_wikis(tree, usize::MAX)?;
    tracing::debug!(available = available.len(), "ingest: enumerated wikis");

    // Step 2b — the list-page inventory, the ONE placement surface the
    // classifier still sees. It is not the wiki window in a smaller hat: a
    // wiki is an address the engine can derive, but the file name of the
    // shopping list is knowledge only the store has, and getting it wrong
    // mints a second shopping list live, in front of the user. Soft-fails to
    // an empty inventory — a turn that cannot read it still captures, it just
    // proposes a fresh name.
    // Fetched UNCAPPED: the store's job is to say what this sender may read,
    // the prompt's job is to decide what fits. What makes the cut safe is the
    // ORDER the store returns — most recently touched first — so the 33rd
    // list `build_prompt` drops is the one nobody has written to in longest,
    // not the one whose wiki id sorts late.
    let list_pages = match fact_index::list_pages_readable_by(
        pool,
        &crate::acl::reader_principals(&sender_ctx.sender_id, &sender_ctx.sender_groups),
        usize::MAX,
    )
    .await
    {
        Ok(pages) => pages,
        Err(err) => {
            tracing::warn!(error = %err, "ingest: list-page inventory failed, continuing without it");
            Vec::new()
        },
    };
    tracing::debug!(list_pages = list_pages.len(), "ingest: list inventory");

    // Step 3 — call the LLM. The system prompt comes from the hybrid
    // loader: operator override at `<workdir>/prompts/ingest.md` wins,
    // otherwise the bundled default embedded via `include_str!` is
    // used. A malformed override surfaces loudly as `IngestError::Wiki`
    // (re-using the existing error class) rather than silently falling
    // back to bundled — the operator's hand-edit deserves attention.
    // Locale resolution (explicit LANGUAGE directive): the
    // request's `metadata.locale` wins; otherwise look up the per-user
    // default the admin configured in `enrollment_users.locale`;
    // otherwise the renderer falls back to the legacy mirror clause.
    let resolved_locale = match request.metadata.locale.clone() {
        Some(loc) => Some(loc),
        None => enrollment::locale_for(pool, &request.sender_id)
            .await
            .map_err(|e| IngestError::Recall(RecallError::Db(e)))?,
    };
    // Timezone resolution (reference-time stamping): the sender's own
    // zone (`enrollment_users.timezone` — users page / welcome wizard)
    // wins over the deployment-wide `recall.ingest_timezone` fallback
    // applied inside `build_prompt`; absent both, spoken wall-clock
    // times are read as UTC. A per-turn zone from the consumer (device
    // time, covers travel) is a tracked protocol extension.
    let sender_timezone = enrollment::timezone_for(pool, &request.sender_id)
        .await
        .map_err(|e| IngestError::Recall(RecallError::Db(e)))?;
    let language_directive = locale::render_language_directive(resolved_locale.as_deref());
    // Which PARTS of this prompt the turn needs, in the order they reach it.
    // Each is loaded only for the turn that uses it, and a part that fails to
    // load is skipped with a warning: the turn is still classified, by exactly
    // the rules an ordinary turn gets.
    let parti_volute: [(&str, &str, bool); 2] = [
        (
            "ingest-assistant-turn",
            BUNDLED_INGEST_ASSISTANT_TURN_MD,
            request.author == MessageRole::Assistant,
        ),
        (
            "ingest-attachments",
            BUNDLED_INGEST_ATTACHMENTS_MD,
            !request.attachments.is_empty(),
        ),
    ];
    let mut parti: Vec<String> = Vec::new();
    for (nome, bundled, serve) in parti_volute {
        if !serve {
            continue;
        }
        match prompts::render(nome, tree.workdir(), bundled, &[]) {
            Ok(p) => parti.push(p),
            Err(e) => tracing::warn!(part = nome, error = %e,
                "ingest: prompt part unread — classifying without it"),
        }
    }
    let parti: Vec<&str> = parti.iter().map(String::as_str).collect();
    let system_prompt = prompts::render(
        "ingest",
        tree.workdir(),
        BUNDLED_INGEST_PROMPT_MD,
        &[("locale", language_directive.as_str())],
    )?;
    // The known-users roster lets the classifier attribute a
    // fact to the right person by canonical name (cross-user attribution).
    let known_users = enrollment::list_users(pool)
        .await
        .map_err(|e| IngestError::Recall(RecallError::Db(e)))?;
    // The sender's standing policy, so the classifier
    // honours their privacy/sharing rules when it assigns per-fact ACL.
    // Best-effort — absent/unreadable → the classifier decides as before.
    let sender_policy = sender_rules(tree, &request.sender_id);
    // The behaviour rules in force for this user (all three scopes — agent's
    // wiki + the sender's identity wiki) — surfaced to the
    // classifier WITH fact_ids and scopes so it can supersede one, and reused
    // below as the recall block's leading slot.
    let behaviour_rules = recall_behaviour_rules(pool, &request).await;
    // Agent-authored memory. When this turn is the consumer
    // agent's OWN reply fed back for extraction, any fact it derives must carry
    // the AGENT as its provenance (`sender`), not the user it was talking to.
    // Resolve the agent principal once — the same system-user binding the
    // behaviour-rule path uses (`system_user_for`). `None` when the turn is a
    // normal user message, when no consumer binding resolves, or when the bot is
    // acting as itself (a smart consumer IS its user, so its replies are already
    // its own facts on the normal path); in every `None` case attribution falls
    // back to the user, exactly as before — the assistant pass simply no-ops.
    let agent_sender: Option<Principal> = if request.author == MessageRole::Assistant {
        match request.consumer_id.as_deref() {
            Some(cid) => crate::consumers::system_user_for(pool, cid)
                .await
                .ok()
                .flatten()
                .filter(|sys| *sys != request.sender_id)
                .map(Principal::User),
            None => None,
        }
    } else {
        None
    };
    if request.author == MessageRole::Assistant {
        tracing::info!(
            sender_id = request.sender_id.as_str(),
            agent_sender = agent_sender
                .as_ref()
                .map(ToString::to_string)
                .as_deref()
                .unwrap_or("none"),
            "ingest: assistant-authored turn — captures attributed to the agent"
        );
    }
    // The named things this memory already holds, so the classifier reuses the
    // answer instead of re-deciding it. Best-effort: an unreadable roster
    // leaves the block empty and the classifier judges as it did before.
    let known_entities = fact_index::known_entities(
        pool,
        &crate::acl::reader_principals(&sender_ctx.sender_id, &sender_ctx.sender_groups),
        KNOWN_ENTITIES_CAP,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "ingest: known entities not read — the block is empty");
        Vec::new()
    });
    let mut prompt = build_prompt(
        &request,
        &recall_hits,
        &list_pages,
        &sender_groups_scoped,
        &known_users,
        &known_entities,
        sender_policy.as_deref(),
        sender_timezone.as_deref(),
        &language_directive,
        &parti,
        turn_now,
        policy,
    );
    push_behaviour_rules_section(&mut prompt, &behaviour_rules);
    // Media riding the turn: stamp late-arriving caption/description on
    // the catalog rows (fill-only), then load the bytes of undescribed
    // photos so the classifier *looks at them* — the consumer-supplied
    // `description` path skips the bytes entirely (the server trusts
    // it). All soft: a media hiccup never kills the turn.
    for att in &request.attachments {
        if (att.caption.is_some() || att.description.is_some())
            && let Ok(Some(row)) = media::find_by_id(pool, &att.catalog_id).await
            && let Err(e) = media::backfill_annotations(
                pool,
                &row,
                att.caption.as_deref(),
                att.description.as_deref(),
            )
            .await
        {
            tracing::warn!(catalog_id = %att.catalog_id, error = %e, "ingest: annotation backfill failed");
        }
    }
    let images = load_attachment_images(pool, tree.workdir(), &request.attachments, llm).await;
    if !images.is_empty() {
        tracing::info!(
            images = images.len(),
            "ingest: photo bytes riding the classifier call (vision)"
        );
    }
    // `max_tokens` sizes the multi-fact `extractions` array. The Gemini
    // backend ignores it and forces `maxOutputTokens: 65536` (combined
    // thinking+output budget); the temperature 0.1 is likewise clamped to
    // Gemini's mandated 1.0 — both bind only on Ollama/Anthropic.
    // The plan is many small JSON objects — one per extracted fact — and a
    // long message can want more than the cap allows. A reply that stops at
    // the ceiling is unbalanced JSON, which parses as nothing at all; when
    // that happens the call is made once more with twice the room, because
    // the alternative is filing nothing and saying so.
    let mut max_tokens = CLASSIFIER_MAX_TOKENS;
    let llm_resp = loop {
        let attempt = llm
            .complete(
                CompletionRequest::new(prompt.clone())
                    .with_system(system_prompt.clone())
                    .with_cached_system()
                    .with_temperature(0.1)
                    .with_max_tokens(max_tokens)
                    .with_images(images.clone()),
            )
            .await;
        match &attempt {
            Ok(resp)
                if resp.finish_reason == crate::llm::FinishReason::MaxTokens
                    && parse_plan(&resp.text).is_none()
                    && max_tokens < CLASSIFIER_MAX_TOKENS_WIDENED =>
            {
                tracing::warn!(
                    max_tokens,
                    "ingest: classifier reply hit the cap and did not parse — retrying wider"
                );
                max_tokens = CLASSIFIER_MAX_TOKENS_WIDENED;
            },
            _ => break attempt,
        }
    };
    let plan = match llm_resp {
        Ok(resp) => {
            if let Some(p) = parse_plan(&resp.text) {
                p
            } else {
                tracing::warn!(
                    text_preview = %resp.text.chars().take(200).collect::<String>(),
                    "ingest: LLM returned unparseable JSON, falling back to skip"
                );
                return Ok(fallback_with_unclaimed_media(
                    pool,
                    tree,
                    &request,
                    &available,
                    policy,
                    &recall_hits,
                    start.elapsed(),
                    true,
                    &std::collections::HashSet::new(),
                )
                .await);
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "ingest: LLM unavailable, falling back to skip");
            return Ok(fallback_with_unclaimed_media(
                pool,
                tree,
                &request,
                &available,
                policy,
                &recall_hits,
                start.elapsed(),
                false,
                &std::collections::HashSet::new(),
            )
            .await);
        },
    };
    tracing::debug!(intent = plan.intent.as_str(), "ingest: LLM plan parsed");

    // The classifier's reading of the turn — the message with what the speaker
    // left implicit written in — kept only where it says something the raw
    // sentence does not: absent, blank, or a copy of what was typed, there is
    // nothing to add. Three things downstream read it, and one binding rather
    // than three copies of this filter is what keeps them agreeing on what the
    // turn means: the second search below, the closure confirmer, and the
    // reconciliation stage. The last two are the ones the raw sentence leaves
    // blind — *«l'ho comprato»* matches no candidate on its own words.
    let completed_message: Option<&str> = plan
        .completed_message
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != request.text.trim());

    // The second search. The first one ran before anything in this turn had
    // read the conversation, so it looked for the words the turn contains and
    // not for what the turn is about; now that the completed message exists,
    // the same search is worth running again on it.
    //
    // It costs no completion — the classifier wrote the sentence inside the
    // call it was already making — and everything downstream is still ahead:
    // the navigator picks where to walk from these hits, the identity cards
    // are served from the people they name, and the block renders last.
    //
    // It TAKES THE PLACE of the first, on both halves: the block takes
    // `recall_top_k` and the entry fan takes the rest, which is the same
    // division the first search made, made again on the better sentence.
    // Keeping the first would mean scoring each fact on its best reading
    // across the two, and one of the two is the vague question — a fact would
    // enter the block for having answered the worse phrasing. The first search
    // has its own job and keeps it: it is the classifier's inventory, and it
    // stands alone on every turn where nothing was left implicit, which is
    // most of them.
    if let Some(completed) = completed_message {
        let depth = policy.nav_seed_depth.max(policy.recall_top_k);
        match recall::wiki_recall(
            pool,
            Arc::clone(&embedder),
            completed,
            depth,
            fact_index::FactFilters::default(),
            &sender_ctx,
        )
        .await
        {
            Ok(mut ranked) => {
                let before = recall_hits.len();
                // The fresh slot is not a similarity result — it is the
                // un-promoted buffer, ranked by recency and rendered under a
                // heading of its own — so it survives the swap untouched.
                let fresh: Vec<RecallHit> = std::mem::take(&mut recall_hits)
                    .into_iter()
                    .filter(|h| h.fresh)
                    .collect();
                seed_tail = ranked.split_off(ranked.len().min(policy.recall_top_k));
                recall_hits = ranked;
                recall_hits.extend(fresh);
                tracing::debug!(
                    completed,
                    before,
                    after = recall_hits.len(),
                    doors = seed_tail.len(),
                    "ingest: searched again on the completed message"
                );
            },
            Err(err) => {
                tracing::warn!(error = %err, "ingest: second search failed, the first stands");
            },
        }
    }

    // Step 4 — route based on intent.
    let intent = parse_intent(&plan.intent);
    let mut capture_id: Option<FactId> = None;
    // Set when a NON-admin asks for an agent-wide behaviour-rule (one that would
    // apply to everyone — admin-only): the rule is NOT filed, and the dedicated
    // `rules` field carries a one-shot notice so the agent declines politely
    // this turn.
    let mut agent_wide_denied = false;
    // A list item the turn could NOT file, because its page name did not
    // survive: the classifier named a reserved page, or the wiki is already at
    // its list limit. The item is refused rather than parked, and the `rules`
    // field carries a one-shot notice so the agent tells the user it was not
    // saved. Founder, 2026-08-18: *«se si raggiungono 32 liste il messaggio
    // dev'essere scartato e avvisato l'utente, generato un errore»*, and
    // A list item never waits: an hour later there would still be no list to
    // put it on.
    let mut list_page_refused: Option<ListRefusal> = None;
    // The flat recall slot is the DETERMINISTIC hit-list ([`format_snippet`]),
    // never an LLM recap. The classifier runs BEFORE the navigator and sees
    // only the shallow flat hits, so a prose recap it wrote here could assert
    // a false negative ("no concert in memory") that the navigator then
    // contradicts two sections down. Composing an answer is the consumer's
    // job; the server surfaces facts. The per-intent arms below only decide
    // WHETHER the slot fills; the rendering happens after navigation, so the
    // hits can be deduplicated against the navigated pages.
    let mut include_flat = false;
    let mut suggested_seed = plan.suggested_seed.clone();
    // Catalog ids claimed by a successfully filed extraction; whatever
    // remains unclaimed after routing is filed by the deterministic
    // fallback so no catalogued media stays dead.
    let mut claimed_attachments: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Self-correcting REM's detection floor ([`crate::recall_log`]):
    // captures buffered this turn (to stamp the recall-log linkage the
    // promotion-time miss detector reads back) and the direct path's
    // write-time dedup hits (the same-turn half of the
    // restated-known-fact miss signal, judged after the navigation tail).
    let mut buffered_ids: Vec<FactId> = Vec::new();
    let mut direct_dedup_hits: Vec<(FactId, f32, String)> = Vec::new();
    // A capture turn that filed nothing: it still reads (the gesture it
    // carries is reconciled downstream), but it must not promise a write
    // that did not happen — the canned seed applies if the reading is empty
    // too.
    let mut nothing_filed = false;
    // EVERY fact this turn filed, on both the buffered and the live path.
    // `capture_id` keeps only the first, as the turn's anchor for the wire;
    // the supersede verb needs them all, because a fact that replaces another
    // has to be NAMEABLE before it can inherit that fact's audience.
    let mut turn_facts: Vec<(FactId, String)> = Vec::new();

    match intent {
        IntentKind::Capture | IntentKind::Structural => {
            // A turn may yield several atomic facts. We file
            // each independently. Legacy single-fact plans (no `extractions`)
            // keep the old "one bad plan demotes the whole turn to skip"
            // contract; a multi-fact plan skips only the offending extraction
            // and files the rest. The body fallback to the raw message is
            // allowed only for the legacy single unit.
            //
            // A STRUCTURAL turn shares this arm since the hybrid
            // "container request + content" message ("voglio creare un
            // ricettario: aggiungi gli spaghetti all'amatriciana…") must
            // not lose its content half: the dashboard nudge stays the
            // turn's outcome, but real `extractions`/`closures` riding the
            // message file normally. Only the explicit multi-fact array
            // files on a structural turn (the legacy top-level synthesis
            // would capture the container request itself), and a
            // structural turn never demotes to skip — the nudge IS its
            // answer even when nothing files.
            let structural = intent == IntentKind::Structural;
            let legacy = plan.extractions.is_empty() && !structural;
            let units = if structural && plan.extractions.is_empty() {
                Vec::new()
            } else {
                plan.capture_units()
            };
            let mut captured_any = false;
            // Reverse-channel accumulator (the server half of the
            // consumer-push contract, INTEGRATING step 8): facts this turn
            // filed for an enrolled user who is NOT the human of the
            // conversation, keyed by that beneficiary so one turn emits at
            // most one notice per recipient. Emitted after the loop.
            let mut beneficiary_notices: std::collections::BTreeMap<
                String,
                Vec<(FactId, WikiId, String)>,
            > = std::collections::BTreeMap::new();
            for unit in &units {
                // Surface the per-fact validity interval + per-page
                // style/description the classifier deduced.
                tracing::info!(
                    valid_from = unit.valid_from.unwrap_or("none"),
                    valid_to = unit.valid_to.unwrap_or("none"),
                    style = unit.style.unwrap_or("none"),
                    page_description = unit.page_description.unwrap_or(""),
                    requested_container = unit.requested_container,
                    fact_type = unit.fact_type.unwrap_or("none"),
                    body = unit.body.unwrap_or(""),
                    "ingest: fact validity + style deduced (placement signal)"
                );

                // An engine-rule is a
                // standing GOVERNANCE directive (a privacy/sharing policy, or a
                // do-not-store rule), not a fact. The classifier flags it; the
                // orchestrator appends it as prose to the sender's `@rules.md`
                // (read back as `sender_rules` next turn) and files NOTHING in
                // `fact_index`. A rule needs only a body — no capture-plan /
                // supersede validation, no target page. The world/household
                // `rule` fact_type ("in casa non si fuma") stays a normal fact.
                if unit.engine_rule {
                    let Some(rule) = unit.body.map(str::trim).filter(|b| !b.is_empty()) else {
                        tracing::warn!("ingest: engine_rule extraction has no body — dropped");
                        continue;
                    };
                    if append_sender_rule(tree, &request.sender_id, rule)? {
                        tracing::info!(
                            sender_id = request.sender_id.as_str(),
                            rule,
                            "ingest: engine-rule appended to sender rules.md (no fact filed)"
                        );
                        captured_any = true;
                    } else {
                        tracing::warn!(
                            sender_id = request.sender_id.as_str(),
                            "ingest: engine_rule but sender has no identity wiki — rule dropped"
                        );
                    }
                    continue;
                }

                // A behaviour-rule is a standing directive about how the
                // CALLING AGENT should converse or operate — neither a fact
                // about the user nor an engine governance rule. The classifier
                // flags it and tags its scope; the orchestrator files it on
                // the scope's home rules page, never in the user's fact
                // memory. GOVERNANCE (behaviour-rule scope from the
                // addressee, prompt Part 7):
                //  - PER-USER (addressed to the speaker, or a bare imperative)
                //    is open to anyone — filed subject=user in the agent's wiki,
                //    recalled only for them on this agent.
                //  - AGENT-WIDE (impersonal / universal) changes the agent for
                //    EVERYONE → ADMIN-ONLY, filed subject=agent. A non-admin's
                //    agent-wide directive is refused (not filed); a one-shot
                //    notice on the `rules` field tells the agent to decline.
                //  - USER-GLOBAL (explicitly every-assistant) is open to
                //    anyone — the user's own rule, filed subject=user in THEIR
                //    identity wiki, recalled by every consumer serving them.
                // Written live so it takes effect next turn; revised in place
                // when the user supersedes one the classifier was shown — but
                // only the admin may revise an AGENT-WIDE rule (a non-admin's
                // revision files at its own scope, leaving the floor intact).
                if unit.behaviour_rule {
                    let Some(rule) = unit.body.map(str::trim).filter(|b| !b.is_empty()) else {
                        tracing::warn!("ingest: behaviour_rule extraction has no body — dropped");
                        continue;
                    };
                    let scope = BehaviourScope::from_hint(unit.behaviour_scope);
                    let mut supersede = behaviour_supersede_target(unit, &behaviour_rules);
                    let touches_everyone = scope == BehaviourScope::AgentWide
                        || matches!(supersede, Some((_, BehaviourScope::AgentWide)));
                    let authorized = !touches_everyone
                        || crate::enrollment::is_admin(pool, request.sender_id.as_str())
                            .await
                            .unwrap_or(false);
                    if !authorized {
                        if scope == BehaviourScope::AgentWide {
                            agent_wide_denied = true;
                            tracing::info!(
                                sender_id = request.sender_id.as_str(),
                                rule,
                                "ingest: agent-wide behaviour-rule from non-admin — refused (admin-only)"
                            );
                            continue;
                        }
                        // The new rule is the sender's own; only the supersede
                        // reached for the agent-wide floor — drop it, file
                        // the rule additively at its own scope.
                        tracing::info!(
                            sender_id = request.sender_id.as_str(),
                            "ingest: non-admin supersede of an agent-wide rule — kept additive"
                        );
                        supersede = None;
                    }
                    match capture_behaviour_rule(
                        tree,
                        pool,
                        Arc::clone(&embedder),
                        &request,
                        rule,
                        scope,
                        supersede.as_ref().map(|(id, _)| id),
                    )
                    .await?
                    {
                        Some(fact_id) => {
                            tracing::info!(
                                sender_id = request.sender_id.as_str(),
                                consumer_id = request.consumer_id.as_deref().unwrap_or("none"),
                                fact_id = fact_id.as_str(),
                                scope = ?scope,
                                rule,
                                "ingest: behaviour-rule filed on its scope's rules page"
                            );
                            captured_any = true;
                            if capture_id.is_none() {
                                capture_id = Some(fact_id);
                            }
                        },
                        None => {
                            tracing::warn!(
                                sender_id = request.sender_id.as_str(),
                                "ingest: behaviour_rule but no consumer/sender wiki resolved — rule dropped"
                            );
                        },
                    }
                    continue;
                }

                // `subject_id: "self"` sentinel (prompt Part 9) → a fact the
                // agent states about ITSELF, filed subject=agent in the agent's
                // own wiki. Only meaningful on an assistant turn
                // where the agent principal resolved (`agent_sender`); on any
                // other turn it is a model slip — skip rather than mis-file. The
                // model's `target_wiki_id` is ignored: the engine knows the
                // agent's own wiki, the model cannot name it.
                //
                // The sentinel has TWO spellings in the wild. `self` is the one
                // Part 9 prescribes; a model that knows its own principal
                // writes it out instead (`user:<agent>`) — the identical claim,
                // "this fact is about me". Matching only the literal lets the
                // spelled-out form fall through to the normal path, and the
                // agent's diary entry then lands in whatever wiki the model
                // named: forty agent-owned facts sat in their users' wikis on
                // the live deployment before both spellings routed here
                // (2026-07-28). No false positives: on a user turn
                // `agent_sender` is `None`, so
                // a user's fact ABOUT the agent is untouched, and on an
                // assistant turn subject==the-speaking-agent IS the self case.
                let self_subject = unit.subject_id.is_some_and(|raw| {
                    raw == "self"
                        || agent_sender.as_ref().is_some_and(|agent| {
                            Principal::from_str(raw).is_ok_and(|subject| subject == *agent)
                        })
                });
                if self_subject {
                    if let Some(Principal::User(agent_id)) = agent_sender.as_ref() {
                        if let Some(fact_id) = capture_agent_self_fact(
                            tree,
                            pool,
                            Arc::clone(&embedder),
                            &request,
                            agent_id,
                            unit,
                            &known_users,
                            policy,
                        )
                        .await?
                        {
                            tracing::info!(
                                agent_id = agent_id.as_str(),
                                fact_id = fact_id.as_str(),
                                "ingest: agent self-fact filed in the agent's own wiki"
                            );
                            captured_any = true;
                            if capture_id.is_none() {
                                capture_id = Some(fact_id);
                            }
                        } else {
                            tracing::warn!(
                                "ingest: subject_id=self but no agent wiki / empty body — dropped"
                            );
                        }
                    } else {
                        tracing::warn!(
                            "ingest: subject_id=self outside a resolved assistant turn — skipped"
                        );
                    }
                    continue;
                }

                // Engine floor of the 2026-06-30 subject-must-be-a-principal
                // ruling (the dangling-principal incident): the `known_users`
                // roster in the prompt steers the classifier away from coining a
                // principal for a person the deployment does not enrol, but
                // nothing enforced it — a dangling `user:<x>` subject matches no
                // reader and splits the fact across homes on re-ingest.
                // Clearing the field routes the unit through the sender
                // default in the validators below, the ruling's own
                // fallback. Fail-open on a DB error: the guard protects
                // against a coined principal, not against an outage.
                let mut unit = *unit;
                if let Some(raw) = unit.subject_id
                    && let Ok(principal) = Principal::from_str(raw)
                    && !enrollment::principal_exists(pool, &principal)
                        .await
                        .unwrap_or(true)
                {
                    tracing::warn!(
                        subject = raw,
                        sender_id = request.sender_id.as_str(),
                        "ingest: subject is not an enrolled principal — re-owned to the sender"
                    );
                    unit.subject_id = None;
                }

                let supersede_target = match validate_supersede_target(
                    &unit,
                    &request,
                    &sender_ctx.sender_groups,
                    &recall_hits,
                ) {
                    Ok(target) => target,
                    Err(err) => {
                        tracing::warn!(error = %err, "ingest: supersede_target invalid");
                        if legacy {
                            return Ok(fallback_with_unclaimed_media(
                                pool,
                                tree,
                                &request,
                                &available,
                                policy,
                                &recall_hits,
                                start.elapsed(),
                                true,
                                &claimed_attachments,
                            )
                            .await);
                        }
                        continue;
                    },
                };
                let mut cap_req = match validate_capture_plan(
                    &unit,
                    &request,
                    policy,
                    &available,
                    &list_pages,
                    legacy,
                ) {
                    Ok(req) => req,
                    Err(err) => {
                        tracing::warn!(error = %err, "ingest: capture plan invalid");
                        if legacy {
                            return Ok(fallback_with_unclaimed_media(
                                pool,
                                tree,
                                &request,
                                &available,
                                policy,
                                &recall_hits,
                                start.elapsed(),
                                true,
                                &claimed_attachments,
                            )
                            .await);
                        }
                        continue;
                    },
                };

                // Product limit: a wiki holds at most `MAX_LIST_PAGES_PER_WIKI`
                // lists, and the limit is enforced by refusing to MINT the
                // next one — never by cutting the inventory the classifier is
                // shown, which would hide a list and have the turn mint a
                // second copy of it live. Growth only: an existing list is
                // always addable-to, however many the wiki already has.
                if let Err(e) =
                    refuse_new_list_over_cap(pool, &mut cap_req, &unit, &list_pages).await
                {
                    tracing::warn!(error = %e, "ingest: list-page cap check failed");
                }

                // **A list item never waits.** Both refusals above express
                // themselves the same way — the claim ends up with no page —
                // and for list material that is not a holding pattern: the
                // item would be placed an hour later by a pass that has no
                // list to place it on. So the extraction is REFUSED and the
                // user is told. A list is a set — silently dropping one item
                // is a wrong answer, not a partial one, and the user must know
                // to retry or free a list.
                if is_list_shaped(&unit) && cap_req.page.is_none() {
                    let reason = if unit.target_page.is_some_and(|p| {
                        normalize_capture_page(Some(p))
                            .is_some_and(|c| wiki::names_reserved_page(&c))
                    }) {
                        ListRefusal::ReservedName
                    } else {
                        ListRefusal::WikiAtListCap
                    };
                    tracing::error!(
                        wiki_id = %cap_req.wiki_id,
                        proposed_page = unit.target_page.unwrap_or("<none>"),
                        reason = reason.as_str(),
                        "ingest: list item REFUSED — no list page to file it on, nothing written"
                    );
                    list_page_refused = Some(reason);
                    continue;
                }

                // Supersede = content update, NOT a sharing change: the new
                // fact INHERITS the superseded fact's audience (`allow`).
                // Sharing changes go through the explicit `acl_change` verb.
                // Without this a re-statement silently re-privatizes a shared
                // fact — the classifier can restate the content but must not be
                // relied on to restate the ACL. The subject is already guaranteed
                // equal by `validate_supersede_target`; `sender` stays the
                // current caller (the re-statement's own provenance). The
                // current sender is stripped from the inherited list, mirroring
                // `validate_capture_plan`'s `SenderRedundantInAllow` guard.
                if let Some(target) = &supersede_target
                    && let Some(prev) = recall_hits.iter().find(|h| &h.fact_id == target)
                {
                    let sender_principal = Principal::User(request.sender_id.clone());
                    cap_req.allow = prev
                        .allow_ids
                        .iter()
                        .filter(|p| **p != sender_principal)
                        .cloned()
                        .collect();
                }

                // The model never writes markers: any `{{embed=…}}` the
                // classifier copied into the body (from the user text, a
                // recalled fact, or a prompt injection) is stripped — the
                // `attachments` claim array is the only sanctioned route.
                // A body whose braces are NOT well-formed embeds would
                // fail the capture validators downstream and 500 the
                // turn, stranding every attachment — skip the unit
                // instead and let the fallback file the media.
                if cap_req.body.contains("{{embed=") {
                    tracing::warn!(
                        "ingest: model wrote embed marker syntax in a body — stripped (claims are the only route)"
                    );
                    let stripped = crate::parser::strip_embed_markers(&cap_req.body);
                    stripped.trim().clone_into(&mut cap_req.body);
                }
                if (cap_req.body.contains("{{") || cap_req.body.contains("}}"))
                    && crate::parser::embed_only_markers(&cap_req.body).is_none()
                {
                    tracing::warn!(
                        "ingest: extraction body carries malformed marker syntax — unit skipped"
                    );
                    continue;
                }
                if cap_req.body.trim().is_empty() {
                    tracing::warn!(
                        "ingest: extraction body empty after marker strip — unit skipped"
                    );
                    continue;
                }

                // Stamp the AGENT as provenance on a fact it derived
                // from its own reply. Only the `sender` axis flips: `subject` stays
                // whoever the fact is ABOUT (the user, for an episode or advice;
                // `global` for kept generic knowledge), so the fact still lands in
                // the right wiki and surfaces on that user's recall. The agent
                // provenance is the trust signal — these are inferences, not
                // user-asserted ground truth, and stay audit/down-weightable by
                // their `sender`. No-op on a user turn (`agent_sender` is `None`).
                if let Some(agent) = &agent_sender {
                    cap_req.sender = Some(agent.clone());
                }

                // Reverse-channel snapshot, taken before `cap_req` moves
                // into the filing call: the notice body is the prose
                // BEFORE the embed markers (media stays behind the
                // dashboard, the notice carries clean text).
                let notice_body = cap_req.body.clone();
                let notice_wiki = cap_req.wiki_id.clone();

                // Media this extraction claims: validate against the
                // turn's attachment window, append the code-rendered
                // embed markers to the body (inside the future region,
                // so reorganizations move them with the fact), and keep
                // the fact's ACL triple for the post-filing widening.
                let unit_media =
                    resolve_unit_attachments(unit.attachments, &request, &mut claimed_attachments);
                append_embed_markers(&mut cap_req.body, &unit_media);
                let media_acl = (
                    cap_req.subject.clone(),
                    cap_req.allow.clone(),
                    cap_req.sender.clone(),
                );

                // Standard wikis buffer the capture for the hourly round;
                // a smart-wiki target would keep the direct-write path,
                // but smart wikis are filtered out of `available` above so in
                // practice every reachable target is a standard wiki.
                // `standard` = "not smart": the per-wiki flag is the whole test.
                let target_is_standard = available
                    .iter()
                    .find(|w| w.wiki_id.as_str() == cap_req.wiki_id.as_str())
                    .is_some_and(|w| !w.smart);
                // The LIVE exception, and it is the same one that decides who
                // names their own page (`names_its_page` in
                // `capture_request_for`): **a fact that already knows where it
                // goes is written now.** Two shapes qualify — a `lista` item,
                // and a container the user asked for *this turn*. Everything
                // else is accumulated knowledge and waits for the dream.
                //
                // The `lista` half landed on 2026-08-18 (founder: *«le liste
                // non ci passano … il classificatore riceve appositamente tutte
                // le liste che l'utente vede e aggiunge l'elemento direttamente
                // lì»*). Until then only `requested_container` bypassed the
                // buffer, so **creating** a shopping list was live while
                // **adding to it** waited up to a dream interval — and the
                // reasoning that kept the exception alive was about lists, not
                // about creation. It was written for a list and hung on the
                // wrong moment.
                //
                // That reasoning, from 2026-08-05, when removing the exception
                // was considered and REJECTED on the founder's question.
                // Recorded here so it is not re-proposed:
                //
                // - "The buffered claim is recallable anyway, so the page can
                //   lag." True for a fact, false for a LIST. The fresh slot is
                //   a ranked top-K (`recall_fresh_top_k`, default 3): add six
                //   items in one dream interval and three of them are simply
                //   not offered. A list's value is completeness — it is a set,
                //   not a relevance ranking — so serving "the three most
                //   similar items" is a wrong answer, not a partial one. The
                //   direct write puts every item on the page and in
                //   `fact_index` at once, where the whole list is readable.
                // - "It collapses dedup to one place." It does not: the dedup
                //   scan is ONE function (`capture::best_dedup_candidate`)
                //   called from the write path and from promotion. What differs
                //   is when it runs, not what it decides.
                //
                // What the live path actually costs, now that the embedding is
                // computed at staging time either way: one dedup scan over the
                // facts about this subject, one atomic page write and one row
                // insert, moved into the turn instead of into the dream. No
                // model call, no extra embedding.
                //
                // The test is the PAGE, and asking it here rather than
                // re-deriving the two shapes keeps one answer where there
                // were two. `validate_capture_plan` already resolved it: a
                // claim that names its own page carries it, everything else
                // carries none — and so does a name that was REFUSED, either
                // as a reserved page or as the wiki's 33rd list. No page is
                // exactly what "nobody has placed this" looks like.
                let route_to_buffer = target_is_standard && cap_req.page.is_none();

                // Cleared when the direct path's write-time dedup proves
                // nothing new filed — a restated fact is no news to its
                // beneficiary. Buffer-time dedup resolves later in the
                // light dream, so a buffered capture always counts.
                let mut filed_fresh = true;
                // Snapshotted before `cap_req` is consumed by either write
                // path: the reconciler is shown what this turn wrote, so it
                // can name a successor without being handed an id alone.
                let this_body = truncate(&cap_req.body, 160);
                let this_id: FactId = if route_to_buffer {
                    // Staged with the claim: the vector (computed once here
                    // instead of on every later read AND again at promotion)
                    // and the fingerprint of the turn it came from, so the
                    // fresh slot can tell whether the agent is already looking
                    // at the message that produced it.
                    let staging = capture_buffer::BufferStaging::build(
                        embedder.as_ref(),
                        &cap_req.body,
                        Some(request.text.as_str()),
                    )
                    .await;
                    let buffered = capture_buffer::buffer_capture_staged(
                        pool,
                        cap_req,
                        supersede_target.clone(),
                        staging,
                    )
                    .await?;
                    tracing::info!(
                        capture_id = buffered.capture_id.as_str(),
                        superseded_hint = supersede_target.is_some(),
                        "ingest: capture BUFFERED (standard wiki; awaits the light dream)"
                    );
                    // Remember the row for the turn's recall-log linkage
                    // (the promotion-time miss detector reads it back).
                    buffered_ids.push(buffered.capture_id.clone());
                    buffered.capture_id
                } else {
                    // Kept aside for the miss signal: a dedup skip proves
                    // the user restated this body ([`format_snippet`]'s
                    // recall set is compared at the end of the turn).
                    let restated_body = cap_req.body.clone();
                    let outcome: CaptureOutcome = match supersede_target {
                        Some(ref old_fact_id) => {
                            match capture::wiki_supersede(
                                tree,
                                pool,
                                Arc::clone(&embedder),
                                old_fact_id,
                                cap_req,
                                turn_now,
                            )
                            .await
                            {
                                Ok(o) => o,
                                // Race with a concurrent forget/supersede
                                // between recall and this call: degrade
                                // gracefully instead of bubbling a 500.
                                Err(CaptureError::PreviousFactNotFound(_)) => {
                                    tracing::warn!(
                                        previous_fact_id = old_fact_id.as_str(),
                                        "ingest: supersede target vanished after recall"
                                    );
                                    // Release the claims — the fact never
                                    // filed, the fallback must pick its
                                    // media up.
                                    for id in &unit_media {
                                        claimed_attachments.remove(id.as_str());
                                    }
                                    if legacy {
                                        return Ok(fallback_with_unclaimed_media(
                                            pool,
                                            tree,
                                            &request,
                                            &available,
                                            policy,
                                            &recall_hits,
                                            start.elapsed(),
                                            true,
                                            &claimed_attachments,
                                        )
                                        .await);
                                    }
                                    continue;
                                },
                                Err(e) => return Err(e.into()),
                            }
                        },
                        None => {
                            capture::wiki_capture(tree, pool, Arc::clone(&embedder), cap_req)
                                .await?
                        },
                    };
                    tracing::info!(
                        fact_id = outcome.fact_id.as_str(),
                        action = capture_action_tag(&outcome.action),
                        superseded = supersede_target.is_some(),
                        "ingest: capture routed (direct write)"
                    );
                    // The direct half of the restated-known-fact miss
                    // signal: the write-time dedup proved the user re-said
                    // an existing fact — whether recall surfaced it is
                    // judged after the navigation tail, below.
                    if let CaptureAction::Skipped {
                        matched_fact_id,
                        similarity,
                    } = &outcome.action
                    {
                        filed_fresh = false;
                        direct_dedup_hits.push((
                            matched_fact_id.clone(),
                            *similarity,
                            restated_body,
                        ));
                    }
                    outcome.fact_id
                };

                // The fact is filed — widen each linked media row's ACL
                // to the fact's read set (monotone union).
                if !unit_media.is_empty() {
                    widen_media_acl_soft(
                        pool,
                        &unit_media,
                        &media_acl.0,
                        &media_acl.1,
                        media_acl.2.as_ref(),
                    )
                    .await;
                }

                // Reverse-channel accumulation: a fact owned by a user who
                // is not the human of this conversation is news TO that
                // user (`request.sender_id` stays the interlocutor on an
                // assistant turn — the assistant-turn flip touches only
                // the fact's `sender` axis above).
                if filed_fresh
                    && let Principal::User(subject_uid) = &media_acl.0
                    && subject_uid != &request.sender_id
                {
                    beneficiary_notices
                        .entry(subject_uid.clone())
                        .or_default()
                        .push((this_id.clone(), notice_wiki, notice_body));
                }

                // Surface the first filed fact as the turn's anchor id, and
                // keep every one of them for the supersede verb.
                turn_facts.push((this_id.clone(), this_body));
                if capture_id.is_none() {
                    capture_id = Some(this_id);
                }
                captured_any = true;
            }

            // Reverse-channel emission: one `fact_minted_for_you` event
            // per beneficiary of this turn, drained by the bridge over
            // `events_poll`. Non-fatal like every notice — a lost event
            // never demotes the turn.
            if !beneficiary_notices.is_empty() {
                // `origin` reflects the turn's ROLE, not the provenance
                // axis: an assistant turn whose consumer binding did not
                // resolve still minted the fact out of the agent's reply.
                emit_beneficiary_notices(
                    pool,
                    &request,
                    request.author == MessageRole::Assistant,
                    beneficiary_notices,
                )
                .await;
            }

            // Reconciliation against facts already stored — closing,
            // replacing, re-dating, re-sharing.
            //
            // **Nothing populates these today.** The bundled prompt stopped
            // asking the classifier for them (v2.59): all four decide the fate
            // of a fact that already exists, which cannot be judged from the
            // ten-hit sample the classifier is shown. The rule the founder
            // drew: a slot may reconcile against a set it sees COMPLETE (the
            // list inventory, the agent's behaviour rules, the sender's own
            // policy) and never against a sample.
            //
            // The machinery below is kept ON PURPOSE, not stranded: it is the
            // substrate of the recall-side **reconciliation stage** — one
            // cheap call after the navigator, judging against the union of the
            // flat hits, the fresh captures and the facts on every page whose
            // prose the turn injected. See
            // §"The reconciliation stage". An operator-overridden prompt that
            // still emits the fields keeps working meanwhile.
            //
            // A pure gesture (closures, no extractions) is real activity: it
            // must not demote to the skip fallback. `closure_topics` widens
            // the aim with a focused second recall + confirm call — the seed
            // the reconciliation stage generalises.
            let mut turn_closures = plan.closures.clone();
            let mut closure_hits = recall_hits.clone();
            if !plan.closure_topics.is_empty() {
                let (confirmed, candidates) = confirm_topic_closures(
                    pool,
                    tree,
                    &embedder,
                    llm,
                    &request,
                    turn_now,
                    &plan.closure_topics,
                    &sender_ctx,
                    policy,
                    &turn_facts,
                    completed_message,
                )
                .await;
                turn_closures.extend(confirmed);
                for hit in candidates {
                    if !closure_hits.iter().any(|h| h.fact_id == hit.fact_id) {
                        closure_hits.push(hit);
                    }
                }
            }
            let closed = apply_plan_closures(
                pool,
                tree,
                &turn_closures,
                &closure_hits,
                &request,
                turn_now,
            )
            .await;
            if closed > 0 {
                captured_any = true;
            }

            // The validity-edit half — date corrections on a recalled fact
            // the sender OWNS. Twin of the closure block, but a correction,
            // not a completion (decay_reason untouched). No topic-widening:
            // these target an explicit recalled fact.
            let edited =
                apply_plan_validity_edits(pool, tree, &plan.validity_edits, &recall_hits, &request)
                    .await;
            if edited > 0 {
                captured_any = true;
            }

            // The acl-change half — sharing changes on a recalled fact the
            // sender OWNS, with a disclosure-audit row per change.
            let reacled =
                apply_plan_acl_changes(pool, tree, &plan.acl_changes, &recall_hits, &request).await;
            if reacled > 0 {
                captured_any = true;
            }

            if structural {
                // The nudge is the structural turn's outcome whether or
                // not the hybrid content filed.
                if suggested_seed.is_none() {
                    suggested_seed = Some(policy.structural_suggested_seed.clone());
                }
            } else {
                // A capture turn ALWAYS flows on to the reading, whether or
                // not an extraction filed.
                //
                // Returning here — demoting the turn to a skip because
                // nothing filed — is the shape to avoid. Since reconciliation
                // left this slot (prompt v2.59) a gesture files NOTHING:
                // "forget what I told you about the greenhouse" is a capture
                // with an empty `extractions` array. Demoting it would (a)
                // answer the user out of an empty context and (b) skip the
                // navigator, which is where the recall-side reconciliation
                // stage reads from — so the one case a demotion would exist
                // for is exactly the case it breaks.
                //
                // `agent_wide_denied` rides the same path: nothing filed, but
                // the turn must still carry the one-shot decline notice. So
                // does `list_page_refused`, for the same reason.
                //
                // Nothing is lost by continuing: unclaimed media is filed by
                // the deterministic pass below, and a turn whose reading also
                // comes back empty still gets the canned seed — see the
                // fallback right after the recall block.
                include_flat = true;
                nothing_filed = !captured_any && !agent_wide_denied && list_page_refused.is_none();
            }
        },
        IntentKind::Recall => {
            include_flat = true;
        },
        IntentKind::Skip => {
            if suggested_seed.is_none() {
                suggested_seed = Some(policy.fallback_suggested_seed.clone());
            }
        },
    }

    // Media the routed plan did not claim — a recall/skip turn carrying
    // a photo, an extraction that never named its attachment — is filed
    // by the deterministic fallback: a catalogued media item never
    // stays dead memory.
    if !request.attachments.is_empty() {
        let fallback_filed = file_unclaimed_attachments(
            pool,
            tree,
            &request,
            &available,
            policy,
            &claimed_attachments,
        )
        .await;
        if capture_id.is_none() {
            capture_id = fallback_filed;
        }
    }

    // Step 5 — the recall-block tail. Navigation costs a navigator
    // completion, so it runs only when the turn's intent justifies it
    // (capture / recall / disambig — a pure skip or a structural nudge
    // must not pay an LLM call) and only when the call site wired a
    // navigator backend. Every failure in the tail is soft: the turn
    // survives on whatever the flat path already produced.
    let seeds = nav_seeds(&plan);
    // `WHO IS SPEAKING` — the sender's identity card, served from their
    // `@profile.md`. It runs FIRST of the tail because it is the
    // deterministic slot the other two defer to: it costs no completion, it
    // arrives whatever the navigator decides, and the page it serves must
    // then be injected nowhere else.
    let speaker = who_is_speaking_section(pool, tree, &sender_ctx, policy).await;
    let identity_path = speaker.as_ref().and_then(|c| c.page_path.as_deref());
    // The same page in the funnel's own `(wiki, page)` terms, so the walk
    // treats it as already visited. The wiki id *is* the sender
    // id — `who_is_speaking_section` locates the wiki by parsing it — and the
    // page is always `IDENTITY_PAGE`, so this one key closes both shapes.
    let mut served_identity: Vec<(String, PathBuf)> = identity_path
        .map(|_| (sender_ctx.sender_id.clone(), PathBuf::from(IDENTITY_PAGE)))
        .into_iter()
        .collect();
    // `PEOPLE THIS TURN IS ABOUT` — the same treatment for the third parties
    // the turn is about (founder 2026-08-04). It runs here, beside the
    // speaker's card and before the walk, for the same three reasons: no
    // completion, arrives whatever the navigator decides, and the pages it
    // serves must then be injected nowhere else.
    let mentioned =
        people_mentioned_section(pool, tree, &sender_ctx, &request.text, &recall_hits, policy)
            .await;
    if let Some(m) = &mentioned {
        served_identity.extend(m.served.iter().cloned());
    }
    // The rails of every card just served. A served page never reaches
    // `open_target`, the only place `[[wikilinks]]` are harvested, so without
    // this the cards' own links are the one part of the corpus the funnel can
    // reach by no route at all — and 69c's character ceiling exists precisely
    // to push a card's detail onto those linked pages.
    let mut served_cards: Vec<(String, String)> = speaker
        .as_ref()
        .and_then(|c| c.rails.clone())
        .into_iter()
        .collect();
    if let Some(m) = &mentioned {
        served_cards.extend(m.rails.iter().cloned());
    }
    let nav_tail = match navigator {
        Some(nav_llm)
            if matches!(intent, IntentKind::Capture | IntentKind::Recall)
                || plan.needs_disambig =>
        {
            navigated_tail(
                pool,
                tree,
                nav_llm,
                &sender_ctx,
                &request.text,
                &seeds,
                &recall_hits,
                &seed_tail,
                policy.recall_top_k,
                &policy.nav,
                recall_nav::Served {
                    pages: &served_identity,
                    cards: &served_cards,
                },
            )
            .await
        },
        _ => None,
    };
    let navigated = nav_tail.as_ref().and_then(|t| t.section.clone());

    // EVERY page whose prose this turn injected: the navigator's walk, the
    // speaker's identity card, and the cards of the people the turn is about.
    // The cards are not a lesser kind of reading — they are served in full,
    // deterministically, whatever the navigator decides, which is exactly why
    // they must not be counted twice or forgotten once.
    //
    // Four readers below mean this same set, and they only stay in agreement
    // by sharing it. The reconciliation stage takes its structural candidates
    // from here, so a correction to something on a card («I don't live in
    // Bologna any more, I've moved») can retire the fact it corrects instead
    // of waiting for similarity to have happened to fish it out. The flat slot
    // drops a hit whose page prose already rides in the block, so the card's
    // facts are not restated under it. The recall log records it as what the
    // turn surfaced, and the miss detector reads it back: a fact the turn had
    // in front of it is not a fact recall failed to find.
    let mut injected_pages: Vec<String> = nav_tail
        .as_ref()
        .map(|t| t.page_paths.clone())
        .unwrap_or_default();
    injected_pages.extend(identity_path.map(str::to_owned));
    if let Some(m) = &mentioned {
        injected_pages.extend(m.page_paths.iter().cloned());
    }

    // Step 5a-bis — THE RECONCILIATION STAGE. The last point in the turn where
    // the engine has actually read the memory, and therefore the only place a
    // judgement about an ALREADY-STORED fact can be made honestly. The
    // classifier cannot: it is shown a top-K similarity sample, and a
    // judgement that needs the store and gets a sample fails silently, by
    // omission, and compounds. See
    // §"The reconciliation stage".
    //
    // Runs on a turn that CAPTURED (a recall turn asks; it does not change
    // what is stored), and only when the turn actually read something — with
    // no candidates there is nothing to reconcile against and no call is made,
    // which is the shape of an ordinary chat turn.
    let mut reconciled = 0usize;
    if matches!(intent, IntentKind::Capture) {
        let candidates = reconcile_candidates(
            pool,
            &embedder,
            &request.text,
            &recall_hits,
            &injected_pages,
            &sender_ctx,
            policy.recall_fresh_top_k,
            &turn_facts,
        )
        .await;
        let decision = reconcile_after_reading(
            tree,
            llm,
            &request,
            turn_now,
            &candidates,
            &turn_facts,
            completed_message,
        )
        .await;
        if !decision.is_empty() {
            reconciled += apply_plan_closures(
                pool,
                tree,
                &decision.closures,
                &candidates,
                &request,
                turn_now,
            )
            .await;
            reconciled += apply_reconciled_supersedes(
                pool,
                &decision.supersedes,
                &candidates,
                &turn_facts,
                &request,
            )
            .await;
            reconciled += apply_plan_validity_edits(
                pool,
                tree,
                &decision.validity_edits,
                &candidates,
                &request,
            )
            .await;
            reconciled +=
                apply_plan_acl_changes(pool, tree, &decision.acl_changes, &candidates, &request)
                    .await;
        }
        // A turn that filed no fact but retired one is not a turn that did
        // nothing. `nothing_filed` was computed before this stage ran, and it
        // gates the canned "I've noted that" seed — leaving it set here would
        // make the engine tell the user nothing happened on the very turn the
        // gesture landed.
        if reconciled > 0 {
            nothing_filed = false;
        }
    }

    // Step 5b — project-docs slot, second half. A signpost
    // in the recall block says a project exists; whether READING that
    // project's docs would help this turn is a judgement, and the
    // classifier has just made it. It is deliberately not a similarity
    // threshold: measured on a 17-sentence bench, no similarity signal
    // separated «i contenuti non si aggiornano» (needs the docs) from
    // «devo andare dal cliente alle 17» (does not) — they sit at the same
    // distance from the corpus. This costs no extra LLM call: the field
    // rides the JSON the classifier already returns. Whatever the named
    // half already pulled is excluded, and it keeps the budget it spent.
    if plan.needs_project_docs {
        let named_wikis: Vec<String> = project_docs.iter().map(|d| d.wiki_id.clone()).collect();
        match recall::recall_signposted_project_docs(
            pool,
            Arc::clone(&embedder),
            &request.text,
            &recall_hits,
            &named_wikis,
            docs_slot.remaining(&project_docs),
            policy.project_docs_signpost_floor,
            &sender_ctx,
        )
        .await
        {
            Ok(hits) => project_docs.extend(hits),
            Err(err) => {
                tracing::warn!(error = %err, "ingest: signposted project-docs recall failed, continuing without it");
            },
        }
    }

    // The flat `RELEVANT MEMORY` slot renders here, AFTER navigation, so a
    // hit whose page prose already rides in the block is dropped instead of
    // arriving twice ([`format_snippet`] dedup) — from the navigated
    // section, or from the identity card the deterministic slot serves
    // (69a: a `bio` fact on `@profile.md` is on both routes by construction).
    let relevant = if include_flat {
        // The classifier's own reading of the hits, applied HERE and nowhere
        // else: navigation has already run above from the unrevised list, so
        // this turn's walk is untouched by construction as well as by
        // intent ([`apply_classifier_vote`]).
        let revised = apply_classifier_vote(&recall_hits, &plan.fact_scores);
        format_snippet(
            &revised,
            &injected_pages,
            &project_docs,
            policy.relevance_floor,
        )
    } else {
        None
    };
    // The due-soon slot is a cheap deterministic pull (no LLM call), so it
    // runs on every LLM-routed turn regardless of intent: an imminent
    // commitment must surface even when the message itself asks nothing.
    let due_soon_tail = due_soon_section(pool, &sender_ctx, policy, turn_now).await;
    let due_soon = due_soon_tail.as_ref().map(|(section, _)| section.clone());

    // Self-correcting REM's detection floor — all best-effort telemetry,
    // never touching the turn. Log what this turn surfaced (flat +
    // fresh + due hits, navigated pages) so the promotion-time detector
    // can look back at it; stamp the log row onto this turn's
    // buffered captures; judge the direct path's dedup hits now that
    // the full surfaced set is known: a restated fact absent from it is a
    // recall MISS — memory held it, recall did not surface it, the user
    // had to re-say it. Rules-page facts are out of scope (channel-
    // delivered, never recalled memory).
    let turn_iso = turn_now.to_rfc3339();
    let mut surfaced_ids: Vec<String> = recall_hits
        .iter()
        .map(|h| h.fact_id.as_str().to_owned())
        .collect();
    if let Some((_, hits)) = &due_soon_tail {
        surfaced_ids.extend(hits.iter().map(|h| h.fact_id.as_str().to_owned()));
    }
    let log_id = match recall_log::record_turn(
        pool,
        &request.sender_id,
        &turn_iso,
        &surfaced_ids,
        &injected_pages,
        &seeds.topics,
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(error = %e, "ingest: recall-log write failed (best-effort)");
            None
        },
    };
    if let Some(id) = log_id
        && !buffered_ids.is_empty()
        && let Err(e) = capture_buffer::stamp_recall_log(pool, &buffered_ids, id).await
    {
        tracing::warn!(error = %e, "ingest: recall-log stamp on buffered captures failed (best-effort)");
    }
    for (fid, similarity, restated_body) in &direct_dedup_hits {
        if surfaced_ids.iter().any(|s| s == fid.as_str()) {
            continue;
        }
        match fact_index::find_by_id(pool, fid).await {
            Ok(Some(row)) => {
                if injected_pages.iter().any(|p| p == &row.source_path)
                    || crate::wiki::is_rules_page(&row.source_path)
                {
                    continue;
                }
                if let Err(e) = recall_log::record_miss(
                    pool,
                    &recall_log::NewMiss {
                        created_at: &turn_iso,
                        sender_id: &request.sender_id,
                        fact_id: fid.as_str(),
                        wiki_id: &row.wiki_id,
                        source_path: &row.source_path,
                        surface: recall_log::MissSurface::Direct,
                        similarity: *similarity,
                        restated_text: restated_body,
                        log_id,
                        seed_topics: &seeds.topics,
                    },
                )
                .await
                {
                    tracing::warn!(error = %e, "ingest: recall-miss record failed (best-effort)");
                }
            },
            Ok(None) => {},
            Err(e) => {
                tracing::warn!(error = %e, "ingest: recall-miss lookup failed (best-effort)");
            },
        }
    }
    // The behaviour rules in force for this user — the `YOUR RULES` section
    // of the dedicated `rules` field. All three scopes (agent's wiki + the
    // sender's identity wiki) — never the user's fact memory.
    // Re-read post-write so a rule SET this turn is already in effect for the
    // consumer's reply (the classifier above saw the pre-write set, which is
    // what it supersedes against).
    let behaviour = format_behaviour_rules(&recall_behaviour_rules(pool, &request).await, policy);
    // Roadmap 27d (read side) + 41: the agent's own self-context — `WHO YOU
    // ARE` (the agent wiki's abstract + identity self-facts) and `YOUR
    // RECENT HISTORY WITH THIS USER` (facts partner-tagged with the sender)
    // — so the consumer composes its reply conscious of itself and the
    // relationship, not just of the user. Read from the agent's own wiki;
    // best-effort, never blocks the turn.
    let agent_self = recall_agent_self(pool, tree, &request).await;
    let who_you_are = format_who_you_are(&agent_self, policy);
    let history = format_history_with_user(&agent_self, policy);
    // `WHO IS SPEAKING` — the sender's identity card, built at the top of
    // the tail (69a) so the flat and navigated slots could defer to it.
    let who_is_speaking = speaker.map(|c| c.section);
    // The third parties the turn names, served beside the speaker (69d).
    let people_mentioned = mentioned.map(|m| m.section);
    // One-shot notice when a non-admin asked for an agent-wide change: the rule
    // was NOT filed; steer the agent to decline politely this turn. It
    // rides the dedicated `rules` field (it is behaviour guidance), not the
    // recalled memory.
    let notice = list_page_refused.map(ListRefusal::notice).or_else(|| {
        agent_wide_denied.then(|| {
            "NOTE — the user asked to set a rule that would apply to EVERYONE (an \
             agent-wide directive). Only the administrator may do that, so it was \
             not applied. Tell the user politely that an agent-wide change is \
             reserved to the admin; do not adopt it. A preference that applies \
             only to them you may still honour."
                .to_owned()
        })
    });
    // Behaviour directives ride their own first-level field, kept
    // apart from the recalled memory in `context_snippet`.
    let rules = assemble_rules_block(notice, behaviour);
    let context_snippet = assemble_recall_block(
        who_you_are,
        who_is_speaking,
        people_mentioned,
        history,
        relevant,
        navigated,
        due_soon,
    );

    // A capture turn that filed nothing AND read nothing has genuinely
    // produced no turn: fall back to the canned seed, which is what the old
    // early-return did. When the reading DID produce something the seed stays
    // whatever the classifier proposed — a forget gesture is answered from
    // the memory it just opened, not with "I've noted that", which would be
    // a lie about a write that never happened.
    if nothing_filed && context_snippet.is_none() && suggested_seed.is_none() {
        suggested_seed = Some(policy.fallback_suggested_seed.clone());
    }

    // The write half of the cross-consumer recent window: buffer
    // this turn for the user's other surfaces — the thread of discourse
    // follows the user. It stays HERE, after everything the turn serves, so a
    // requester can never be handed back the very message it just sent (the
    // fetch ran at the top of the turn). Best-effort: a buffer hiccup never
    // touches the turn.
    let surface_channel = request.metadata.channel.clone().unwrap_or_default();
    if let Err(e) = crate::recent_window::record_exchange(
        pool,
        &request.sender_id,
        &consumer_surface,
        &surface_channel,
        request.author,
        &request.text,
        turn_now,
        policy.recent_window_entries,
        policy.recent_window_ttl_hours,
    )
    .await
    {
        tracing::warn!(error = %e, "recent-window: buffer write failed (turn unaffected)");
    }

    // Journal the route this recall took (the admin Traces page). Best-effort
    // telemetry: a journal failure is logged and never touches the turn.
    record_ingest_trace(
        pool,
        &request,
        intent,
        "classifier",
        &seeds,
        &recall_hits,
        nav_tail.as_ref(),
        due_soon_tail.as_ref().map(|(_, hits)| hits.as_slice()),
        policy,
        context_snippet.as_deref(),
        rules.as_deref(),
        start.elapsed(),
    )
    .await;

    tracing::info!(
        intent = intent.as_str(),
        captured = capture_id.is_some(),
        snippet_chars = context_snippet.as_deref().map_or(0, str::len),
        rules_chars = rules.as_deref().map_or(0, str::len),
        "ingest: done"
    );

    // Disambig follow-up: when the consumer is calling back with the
    // chosen candidate, the orchestrator never re-surfaces ambiguity
    // — even if the LLM ignored the prompt's commit instruction. The
    // contract is "second turn commits".
    let resolving_disambig = request.disambig_choice.is_some();
    let (needs_disambig, disambig_candidates) = if resolving_disambig {
        (false, Vec::new())
    } else {
        (
            plan.needs_disambig,
            plan.disambig_candidates
                .into_iter()
                // A partial LLM reply can yield a candidate object whose
                // `candidate_id` defaulted to empty — unchoosable; drop it.
                .filter(|d| !d.candidate_id.is_empty())
                .map(|d| DisambigCandidate {
                    candidate_id: d.candidate_id,
                    description: d.description,
                })
                .collect(),
        )
    };

    Ok(IngestResponse {
        intent,
        context_snippet,
        rules,
        suggested_seed,
        recent_window,
        capture_id,
        needs_disambig,
        disambig_candidates,
        llm_used: true,
        took_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

const fn capture_action_tag(action: &CaptureAction) -> &'static str {
    match action {
        CaptureAction::Captured { .. } => "captured",
        CaptureAction::Skipped { .. } => "skipped",
        CaptureAction::Superseded { .. } => "superseded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::llm::{FakeLlmBackend, FinishReason, LlmError};
    use crate::types::WikiSlug;
    use crate::wiki::WikiMeta;
    use async_trait::async_trait;
    use tempfile::TempDir;

    // ---------- Test scaffolding ----------

    async fn setup_workdir() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        // Wikis live under <workdir>/wikis/<slug>/
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        // An 'alice' identity wiki: a top-level `wiki-user` root
        // (`parent_wiki_id: null`) — the way `create_identity_wiki` lands
        // real identity wikis — so the scope-principal derivation resolves
        // it to `user:alice`. Parenting it under a non-identity wiki would
        // make the fixture unlike production AND unable to derive a scope
        // principal at all.
        write_wiki(&wikis, "alice", "Alice", "wiki-user", None);
        let tree = WikiTree::open(dir.path()).expect("open tree");
        // ENROLLED, which is what gives her wiki an identity card. Since
        // 2026-08-22 the card is the only placement that needs no model, so a
        // test that drains the buffer deterministically places nothing
        // without it.
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, tree, pool)
    }

    fn write_wiki(
        wikis_dir: &Path,
        slug: &str,
        title: &str,
        wiki_type: &str,
        parent: Option<String>,
    ) {
        let dir = wikis_dir.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let parent_yaml = parent
            .map(|p| format!("\nparent_wiki_id: {p}"))
            .unwrap_or_default();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: {wiki_type}\nslug: {slug}\ntitle: {title}\nacl_default: 'user:{slug}'{parent_yaml}\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), &frontmatter).unwrap();
        std::fs::write(dir.join("cucina.md"), "# cucina\n").unwrap();
    }

    fn req(text: &str, sender: &str) -> IngestRequest {
        IngestRequest {
            text: text.to_owned(),
            author: MessageRole::User,
            sender_id: sender.to_owned(),
            consumer_id: None,
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata::default(),
            attachments: Vec::new(),
        }
    }

    /// [`req`] for a standard consumer call: the bot's deployment id rides as
    /// `consumer_id`, so behaviour-rule routing can resolve the agent's own
    /// wiki (the caller still acts on behalf of `sender`).
    fn req_consumer(text: &str, sender: &str, consumer_id: &str) -> IngestRequest {
        IngestRequest {
            consumer_id: Some(consumer_id.to_owned()),
            ..req(text, sender)
        }
    }

    /// Like [`setup_workdir`] but also materialises an agent wiki
    /// (`samvisebot`) and registers a standard consumer (`botdeploy`) bound to
    /// it, so behaviour-rule routing into the consumer's own wiki is
    /// exercisable end to end.
    async fn setup_agent_workdir() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(&wikis).unwrap();
        // Identity wikis are top-level roots (`parent_wiki_id: null`), the way
        // `create_identity_wiki` lands them, so the scope-principal derivation
        // resolves `alice` → `user:alice` and `samvisebot` → `user:samvisebot`
        // — see `setup_workdir`.
        write_wiki(&wikis, "alice", "Alice", "wiki-user", None);
        write_wiki(&wikis, "samvisebot", "Samvise Bot", "wiki-user", None);
        let tree = WikiTree::open(dir.path()).expect("open tree");
        // The consumer ↔ system-user binding (FK: consumers.system_user_id →
        // enrollment_users.user_id), so `system_user_for` resolves the agent wiki.
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('samvisebot', 0)")
            .execute(&pool)
            .await
            .unwrap();
        crate::consumers::register(
            &pool,
            &crate::consumers::RegisterRequest {
                consumer_id: "botdeploy",
                display_name: None,
                callback_url: None,
                kinds_subscribed: None,
                metadata: None,
                system_user_id: Some("samvisebot"),
            },
        )
        .await
        .expect("register consumer");
        (dir, tree, pool)
    }

    /// A fixed reference instant for `build_prompt` tests — a Thursday at noon
    /// UTC, so assertions on the injected `current_time` line are deterministic.
    fn now_fixture() -> chrono::DateTime<chrono::Utc> {
        "2026-06-04T12:00:00Z"
            .parse()
            .expect("fixed instant parses")
    }

    fn fake_embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.1, 0.2, 0.3, 0.4],
        ))
    }

    /// Drain the capture buffer the way the light dream does, so a test can go
    /// on asserting about `fact_index`.
    ///
    /// Ingest buffers **every** standard capture (founder, 2026-08-05) —
    /// there is no live-write exception — so a fact reaches `fact_index` at
    /// the next light cycle rather than during the turn. A test that is about WHAT ends up filed — the subject, the validity
    /// window, the provenance, the inherited audience — still wants to read
    /// the fact, and reading it through the promotion is better than reaching
    /// into the buffer and re-deriving by hand what promotion would have
    /// copied: it exercises the path the product actually takes.
    ///
    /// Tests that are about the BUFFERING itself assert on the buffer
    /// directly and never call this.
    async fn promote_buffer(pool: &SqlitePool, tree: &WikiTree) {
        crate::dream_light::drain_deterministically(
            pool,
            tree,
            &fake_embedder(),
            &crate::dream_light::LightPolicy::default(),
            "2026-08-18T00:00:00Z",
        )
        .await
        .expect("drain");
    }

    // ---------- smart-family filter ----------

    #[test]
    fn available_wikis_reads_smart_flag_from_meta() {
        // The smart-family gate reads the per-wiki smart
        // flag straight from `_meta.md`. A
        // wiki stamped `smart: true` is hidden from the router
        // window; a plain standard wiki is offered. This is precisely
        // what protects `wiki_ingest_message` from routing writes to a
        // smart-consumer-owned smart wiki.
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let normal = "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
                      slug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n";
        let smart = "---\nwiki_id: proj\nwiki_type: wiki-companion\nparent_wiki_id: null\n\
                     slug: proj\ntitle: Proj\nacl_default: 'user:alice'\nsmart: true\n---\n";
        let (nmeta, _) = WikiMeta::parse(Path::new("_meta.md"), normal).expect("normal meta");
        let (cmeta, _) = WikiMeta::parse(Path::new("_meta.md"), smart).expect("smart-wiki meta");
        crate::wiki::write_wiki_dir(&tree, &nmeta, false).expect("create alice");
        crate::wiki::write_wiki_dir(&tree, &cmeta, false).expect("create proj");

        // The enumerator itself is the gate now: a smart wiki never reaches
        // the window at all, so no caller has to remember to filter.
        let avail = available_wikis(&tree, 100).expect("available");
        assert!(avail.iter().any(|w| w.wiki_id == "alice"));
        assert!(
            !avail.iter().any(|w| w.wiki_id == "proj"),
            "a smart wiki is never offered as a routing target"
        );
        assert!(avail.iter().all(|w| !w.smart));
    }

    /// Frontmatter for a top-level identity wiki.
    fn identity_meta_yaml(id: &str, wiki_type: &str) -> String {
        format!(
            "---\nwiki_id: {id}\nwiki_type: {wiki_type}\nparent_wiki_id: null\n\
             slug: {id}\ntitle: {id}\n---\n"
        )
    }

    /// Frontmatter for an emerged sub-wiki under `parent`, with a creation
    /// stamp so the oldest-first ordering is testable.
    fn emerged_meta_yaml(id: &str, parent: &str, created: &str) -> String {
        format!(
            "---\nwiki_id: {id}\nwiki_type: wiki\nparent_wiki_id: {parent}\n\
             slug: {id}\ntitle: {id}\ncreated: \"{created}\"\n---\n"
        )
    }

    fn write_meta(tree: &WikiTree, yaml: &str) {
        let (meta, _) = WikiMeta::parse(Path::new("_meta.md"), yaml).expect("meta");
        crate::wiki::write_wiki_dir(tree, &meta, false).expect("create wiki");
    }

    #[test]
    fn available_wikis_never_caps_identity_wikis() {
        // A deployment's users and groups ARE its routing domains: dropping
        // one does not degrade the choice, it removes the only correct answer
        // for every fact about that principal. So they are exempt from the cap
        // and are not counted against it — even at cap 0.
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        write_meta(&tree, &identity_meta_yaml("alice", "wiki-user"));
        write_meta(&tree, &identity_meta_yaml("bob", "wiki-user"));
        write_meta(&tree, &identity_meta_yaml("famiglia", "wiki-group"));

        let avail = available_wikis(&tree, 0).expect("available");
        let ids: Vec<&str> = avail.iter().map(|w| w.wiki_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["alice", "bob", "famiglia"],
            "identity wikis ignore the cap entirely"
        );
    }

    #[test]
    fn available_wikis_caps_only_emerged_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        write_meta(&tree, &identity_meta_yaml("alice", "wiki-user"));
        let tree = WikiTree::open(dir.path()).expect("reopen");
        // Deliberately created in a different order than their timestamps, and
        // named so that alphabetical order would give a different answer.
        write_meta(
            &tree,
            &emerged_meta_yaml("anewest", "alice", "2026-03-01T00:00:00Z"),
        );
        write_meta(
            &tree,
            &emerged_meta_yaml("zoldest", "alice", "2026-01-01T00:00:00Z"),
        );
        write_meta(
            &tree,
            &emerged_meta_yaml("mmiddle", "alice", "2026-02-01T00:00:00Z"),
        );

        // Cap 2: the identity wiki rides free, the two OLDEST emerged survive.
        let avail = available_wikis(&tree, 2).expect("available");
        let ids: Vec<&str> = avail.iter().map(|w| w.wiki_id.as_str()).collect();
        assert_eq!(ids, vec!["alice", "zoldest", "mmiddle"]);

        // Uncapped: all three, still oldest-first.
        let all = available_wikis(&tree, 100).expect("available");
        let ids: Vec<&str> = all.iter().map(|w| w.wiki_id.as_str()).collect();
        assert_eq!(ids, vec!["alice", "zoldest", "mmiddle", "anewest"]);
    }

    #[test]
    fn available_wikis_smart_does_not_consume_a_cap_slot() {
        // The cap is applied AFTER the smart family leaves, so a deployment
        // with many project notebooks does not silently get a smaller routing
        // window than one with none.
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        write_meta(&tree, &identity_meta_yaml("alice", "wiki-user"));
        let tree = WikiTree::open(dir.path()).expect("reopen");
        let smart = "---\nwiki_id: proj\nwiki_type: wiki\nparent_wiki_id: alice\n\
                     slug: proj\ntitle: Proj\nsmart: true\ncreated: \"2026-01-01T00:00:00Z\"\n---\n";
        write_meta(&tree, smart);
        write_meta(
            &tree,
            &emerged_meta_yaml("viaggi", "alice", "2026-02-01T00:00:00Z"),
        );

        // Cap 1. Were the smart wiki counted (it is the older of the two) the
        // real emerged wiki would be squeezed out.
        let avail = available_wikis(&tree, 1).expect("available");
        let ids: Vec<&str> = avail.iter().map(|w| w.wiki_id.as_str()).collect();
        assert_eq!(ids, vec!["alice", "viaggi"]);
    }

    #[test]
    fn available_wikis_carries_both_description_fields() {
        // `scope` (authored intent) and `summary` (the compiled abstract) are
        // separate claims and both reach the window; the renderer emits each
        // only when present.
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let described = "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
                         slug: alice\ntitle: Alice\nscope: Alice's own memory\n\
                         summary: What it actually holds\n---\n";
        write_meta(&tree, described);
        write_meta(&tree, &identity_meta_yaml("bob", "wiki-user"));

        let avail = available_wikis(&tree, 100).expect("available");
        let alice = avail.iter().find(|w| w.wiki_id == "alice").expect("alice");
        assert_eq!(alice.scope.as_deref(), Some("Alice's own memory"));
        assert_eq!(alice.summary.as_deref(), Some("What it actually holds"));

        let mut out = String::new();
        render_available_wikis(&mut out, &avail, 1_000);
        assert!(out.contains("scope: Alice's own memory"), "{out}");
        assert!(out.contains("holds: What it actually holds"), "{out}");
        // The undescribed one says so once, instead of two empty keys.
        assert!(out.contains("about: (not described yet)"), "{out}");
        assert!(!out.contains("scope: \n"), "{out}");
    }

    // ---------- sender rules.md read ----------

    #[test]
    fn sender_rules_reads_actor_rules_md_else_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        // No identity wiki for the sender yet → best-effort None.
        assert!(sender_rules(&tree, "alice").is_none());

        // Creating alice's identity wiki seeds a rules.md that is a heading
        // and nothing else — a page with no policy on it, which reaches the
        // classifier as no policy rather than as a rule Alice never wrote.
        let id = WikiId::parse("alice").unwrap();
        crate::wiki::create_identity_wiki(&tree, &id, "Alice", crate::wiki::IdentityKind::User)
            .expect("create alice");
        // Re-open so the registry picks up the new wiki for `locate`.
        let tree = WikiTree::open(dir.path()).expect("reopen");
        assert!(sender_rules(&tree, "alice").is_none());

        // A user-edited policy is read back verbatim.
        let handle = tree.locate(&id).unwrap();
        handle
            .write_page(
                Path::new(crate::wiki::RULES_FILENAME),
                "# Rules\n\nkeep health private",
            )
            .unwrap();
        assert!(
            sender_rules(&tree, "alice")
                .unwrap()
                .contains("keep health private")
        );

        // A user-global behaviour rule lives on the same page as a `{{f=…}}`
        // region — the governance read strips it: the rule
        // reaches the classifier via `agent_behaviour_rules` (with its
        // fact_id), never as policy prose.
        handle
            .write_page(
                Path::new(crate::wiki::RULES_FILENAME),
                "# Rules\n\nkeep health private\n\n\
                 {{f=018f1234-5678-7abc-9def-0123456789ab}}Parlami in italiano.{{/}}\n",
            )
            .unwrap();
        let got = sender_rules(&tree, "alice").unwrap();
        assert!(got.contains("keep health private"));
        assert!(
            !got.contains("Parlami in italiano."),
            "fact regions must be stripped from the governance prose: {got}"
        );

        // A page holding ONLY regions has no governance prose → None.
        handle
            .write_page(
                Path::new(crate::wiki::RULES_FILENAME),
                "{{f=018f1234-5678-7abc-9def-0123456789ab}}Parlami in italiano.{{/}}\n",
            )
            .unwrap();
        assert!(sender_rules(&tree, "alice").is_none());
    }

    #[test]
    fn behaviour_scope_from_hint_maps_wire_tokens() {
        assert_eq!(
            BehaviourScope::from_hint(Some("agent-wide")),
            BehaviourScope::AgentWide
        );
        assert_eq!(
            BehaviourScope::from_hint(Some("user-global")),
            BehaviourScope::UserGlobal
        );
        // The open per-user side is the default for everything else.
        assert_eq!(
            BehaviourScope::from_hint(Some("per-user")),
            BehaviourScope::PerUser
        );
        assert_eq!(
            BehaviourScope::from_hint(Some("galaxy-wide")),
            BehaviourScope::PerUser
        );
        assert_eq!(BehaviourScope::from_hint(None), BehaviourScope::PerUser);
    }

    // ---------- intent parsing ----------

    #[test]
    fn intent_parser_accepts_canonical_strings() {
        assert_eq!(parse_intent("capture"), IntentKind::Capture);
        assert_eq!(parse_intent("Recall"), IntentKind::Recall);
        assert_eq!(parse_intent(" STRUCTURAL "), IntentKind::Structural);
        assert_eq!(parse_intent("skip"), IntentKind::Skip);
    }

    #[test]
    fn intent_parser_defaults_unknown_to_skip() {
        assert_eq!(parse_intent("forget"), IntentKind::Skip);
        assert_eq!(parse_intent(""), IntentKind::Skip);
    }

    // ---------- plan parsing ----------

    /// A classifier reply in the pre-rename shape must still name the subject.
    ///
    /// This is the alias whose absence is worst. Every field on the plan is
    /// `#[serde(default)]`, so an unrecognised key is not an error: `subject_id`
    /// simply arrives as `None`, and `validate_capture_plan` then defaults the
    /// subject to the SENDER. Alice, speaking about Bob, would file a fact whose
    /// subject is Alice — Bob could not read a fact about himself, Alice would
    /// govern its disclosure, and a search for Bob's facts would never return
    /// it. Nothing errors and nothing is logged; the raw plan is not persisted.
    ///
    /// It is reachable on any deployment that ran `init` before this release:
    /// `<workdir>/prompts/*.md` overrides win over the bundled prompt and are
    /// never re-seeded, so the model keeps being told to emit the old key.
    #[test]
    fn a_classifier_reply_in_the_pre_rename_shape_still_names_the_subject() {
        let raw = r#"{"intent":"capture","body":"Bob changed jobs.",
                      "owner_id":"user:bob","allow_ids":[]}"#;
        let plan = parse_plan(raw).expect("plan must parse");
        assert_eq!(
            plan.subject_id.as_deref(),
            Some("user:bob"),
            "the pre-rename key must still reach the subject axis"
        );

        // Control: an unrecognised spelling is silently dropped, not refused —
        // which is exactly why the alias above has to be right.
        let bogus = r#"{"intent":"capture","body":"x","proprietor_id":"user:bob"}"#;
        let plan = parse_plan(bogus).expect("unknown keys are ignored, not rejected");
        assert_eq!(
            plan.subject_id, None,
            "an unknown key leaves the subject unset"
        );
    }

    #[test]
    fn parse_plan_extracts_pure_json() {
        let raw = r#"{"intent":"skip","suggested_seed":"ok"}"#;
        let plan = parse_plan(raw).expect("parsed");
        assert_eq!(plan.intent, "skip");
        assert_eq!(plan.suggested_seed.as_deref(), Some("ok"));
    }

    #[test]
    fn parse_plan_extracts_json_after_prose() {
        let raw = "Sure, here is the plan:\n```json\n{\"intent\":\"recall\",\"suggested_seed\":\"foo\"}\n```\n";
        let plan = parse_plan(raw).expect("parsed");
        assert_eq!(plan.intent, "recall");
        assert_eq!(plan.suggested_seed.as_deref(), Some("foo"));
    }

    #[test]
    fn parse_plan_handles_nested_braces_in_strings() {
        let raw = r#"{"intent":"capture","body":"x = { a: 1 }","target_wiki_id":"alice"}"#;
        let plan = parse_plan(raw).expect("parsed");
        assert_eq!(plan.body.as_deref(), Some("x = { a: 1 }"));
        assert_eq!(plan.target_wiki_id.as_deref(), Some("alice"));
    }

    #[test]
    fn parse_plan_reads_the_project_docs_judgement_and_defaults_it_off() {
        // The classifier decides whether a signposted
        // project's documentation would help this turn. Absent (an older
        // prompt, or a fallback plan) must mean "do not dig" — the
        // expensive direction is never the default.
        let asked = parse_plan(r#"{"intent":"recall","needs_project_docs":true}"#).expect("parsed");
        assert!(asked.needs_project_docs);
        let silent = parse_plan(r#"{"intent":"capture"}"#).expect("parsed");
        assert!(!silent.needs_project_docs);
        let declined =
            parse_plan(r#"{"intent":"capture","needs_project_docs":false}"#).expect("parsed");
        assert!(!declined.needs_project_docs);
    }

    #[test]
    fn parse_plan_returns_none_on_garbage() {
        assert!(parse_plan("totally not json").is_none());
        assert!(parse_plan("").is_none());
        // Unterminated brace
        assert!(parse_plan("{ \"intent\": \"skip\"").is_none());
    }

    // ---------- capture plan validation ----------

    /// A page name is honoured exactly when the write cannot wait.
    ///
    /// Two independent switches reach that state and both must work: LIST
    /// material (a set — half a shopping list is a wrong answer, not a partial
    /// one) and a REQUESTED CONTAINER (the user asked now, the write bypasses
    /// the buffer, so the page has to exist now — whatever its style, since a
    /// note someone asks you to keep can be prose).
    ///
    /// Everything else has no right page at capture time and the classifier is
    /// shown none to check against, so a name it proposes is a guess that would
    /// become a real file on disk. Those carry no page at all.
    #[test]
    fn a_page_name_is_honoured_only_when_the_write_cannot_wait() {
        let request = req("qualcosa", "alice");
        let policy = IngestPolicy::default();
        let available = [sample_available("alice")];
        let no_ids: [String; 0] = [];
        let unit = |style: Option<&'static str>, requested: bool| CaptureUnit {
            subject_external: None,
            target_wiki_id: None,
            target_page: Some("spesa.md"),
            subject_id: None,
            allow_ids: &no_ids,
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style,
            page_description: Some("what the family still needs to buy"),
            salience: None,
            requested_container: requested,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: &no_ids,
            body: Some("latte"),
            supersede_target: None,
            attachments: &no_ids,
        };

        let plan = |style, requested| {
            validate_capture_plan(
                &unit(style, requested),
                &request,
                &policy,
                &available,
                &[],
                true,
            )
            .expect("capture")
        };

        // A list names its page, and may describe the one it is creating.
        let list = plan(Some("lista"), false);
        assert_eq!(list.page, Some(PathBuf::from("spesa.md")));
        assert_eq!(
            list.page_description.as_deref(),
            Some("what the family still needs to buy")
        );

        // A container the user asked for names its page too, PROSE INCLUDED —
        // "keep me a note about project X" is written live, so the page they
        // named has to exist now.
        let note = plan(Some("prosa"), true);
        assert_eq!(
            note.page,
            Some(PathBuf::from("spesa.md")),
            "a requested container keeps the name the user gave it"
        );
        assert!(note.page_description.is_some());

        // Everything else waits, so the engine picks the page.
        for style in [Some("prosa"), Some("prosa-tecnica"), None] {
            let waits = plan(style, false);
            assert_eq!(
                waits.page, None,
                "a page named on material that can wait is discarded ({style:?})"
            );
            assert!(
                waits.page_description.is_none(),
                "no page was chosen, so there is nothing to describe ({style:?})"
            );
        }
    }

    /// The four arms of [`derive_target_wiki`], in the order they fire.
    ///
    /// The list arm is the interesting one: it is the ONLY route left by
    /// which a turn puts a fact into a wiki that is not its subject's, and it
    /// fires exactly when the user pointed at a list themselves.
    #[test]
    fn derive_target_wiki_walks_list_then_subject_then_sender() {
        let request = req("qualcosa", "alice");
        let available = [
            sample_available("alice"),
            sample_available("bob"),
            sample_available("famiglia"),
            sample_available("casa"),
        ];
        let lists = [fact_index::ListPage {
            wiki_id: "casa".into(),
            page: "spesa.md".into(),
            description: None,
        }];
        let no_ids: [String; 0] = [];
        let unit = |wiki: Option<&'static str>, page: Option<&'static str>| CaptureUnit {
            subject_external: None,
            target_wiki_id: wiki,
            target_page: page,
            subject_id: None,
            allow_ids: &no_ids,
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: &no_ids,
            body: Some("qualcosa"),
            supersede_target: None,
            attachments: &no_ids,
        };
        let user = |id: &str| Principal::User(id.to_owned());
        let derive = |wiki, page, subject: Principal| {
            derive_target_wiki(
                &unit(wiki, page),
                &request,
                &subject,
                page,
                &available,
                &lists,
            )
        };

        // 1 — an explicit id still wins, for an operator-overridden prompt…
        assert_eq!(
            derive(Some("bob"), None, user("alice")).as_deref(),
            Some("bob")
        );
        // …but only when it names a real wiki; otherwise it is ignored and
        // the fact survives on the next arm instead of being dropped.
        assert_eq!(
            derive(Some("nowhere"), None, user("bob")).as_deref(),
            Some("bob")
        );
        // 2 — the list the turn names carries its own wiki, beating the subject
        // when no list of the subject's own wiki answers to that name.
        assert_eq!(
            derive(None, Some("spesa.md"), Principal::Group("famiglia".into())).as_deref(),
            Some("casa")
        );
        // …but a file name is not an address. When two wikis both hold a
        // `spesa.md`, the subject the classifier resolved decides which one the
        // turn meant — matching on the name alone handed «aggiungi il
        // detersivo alla lista della spesa DI FAMIGLIA» to whichever wiki the
        // inventory listed first.
        let shared = [
            fact_index::ListPage {
                wiki_id: "casa".into(),
                page: "spesa.md".into(),
                description: None,
            },
            fact_index::ListPage {
                wiki_id: "famiglia".into(),
                page: "spesa.md".into(),
                description: None,
            },
        ];
        assert_eq!(
            derive_target_wiki(
                &unit(None, Some("spesa.md")),
                &request,
                &Principal::Group("famiglia".into()),
                Some("spesa.md"),
                &available,
                &shared,
            )
            .as_deref(),
            Some("famiglia"),
            "the subject's own list wins over a namesake in another wiki"
        );
        // A page that is not a known list is just a page: the subject decides.
        assert_eq!(
            derive(None, Some("altro.md"), Principal::Group("famiglia".into())).as_deref(),
            Some("famiglia")
        );
        // 3 — the subject's own wiki is the ordinary path.
        assert_eq!(derive(None, None, user("bob")).as_deref(), Some("bob"));
        // 4 — a `global`-owned fact has no wiki of its own → the sender's…
        assert_eq!(
            derive(None, None, Principal::global()).as_deref(),
            Some("alice")
        );
        // …and so does a subject with no wiki at all.
        assert_eq!(derive(None, None, user("nobody")).as_deref(), Some("alice"));
        // Nothing left to fall back on → the caller drops the extraction.
        assert_eq!(
            derive_target_wiki(&unit(None, None), &request, &user("nobody"), None, &[], &[]),
            None
        );
    }

    /// No `target_wiki_id` is the NORMAL case now, not a broken plan: the
    /// classifier is shown no wikis and is told not to emit one. The
    /// destination comes from the subject, and a fact with no stated subject
    /// is the sender's own.
    #[test]
    fn validate_capture_plan_refuses_a_reserved_page_name() {
        // The prompt has promised this for weeks — «a capture aimed at a
        // reserved page is not filed there» — and nothing enforced it: the
        // predicate that would have had two callers, both on paths where the
        // name was already gone. A list-shaped unit is the case that reaches
        // disk inside the turn, so it is the one that had to be closed.
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("alice".into()),
            target_page: Some("@rules.md".into()),
            subject_id: None,
            allow_ids: Vec::new(),
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: Some("lista".into()),
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("comprare il latte".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("comprare il latte", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("the capture is filed, just not there");
        assert_eq!(
            cap.page, None,
            "a capture that names a reserved page lands in the buffer, and the placement \
             pass settles it like any other unplaced prose"
        );
        assert_eq!(
            cap.body, "comprare il latte",
            "and the fact itself is never the thing thrown away"
        );
    }

    #[test]
    fn validate_capture_plan_derives_the_wiki_from_the_sender() {
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: None,
            target_page: None,
            subject_id: None,
            allow_ids: Vec::new(),
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("a fact".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("a fact", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("a plan with no wiki is derived, not refused");
        assert_eq!(
            cap.wiki_id.as_str(),
            "alice",
            "a fact with no stated subject is the sender's, and lands in their wiki"
        );
    }

    #[test]
    fn validate_capture_plan_defaults_subject_to_sender() {
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("alice".into()),
            target_page: None,
            subject_id: None,
            allow_ids: Vec::new(),
            fact_type: Some("preference".into()),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: vec!["coffee".into()],
            body: Some("alice prefers coffee black".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("alice prefers coffee black", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("validated");
        assert_eq!(cap.wiki_id.as_str(), "alice");
        assert_eq!(
            cap.page, None,
            "prose that can wait names no page — the queue places it"
        );
        assert!(matches!(cap.subject, Principal::User(ref id) if id == "alice"));
        assert!(matches!(cap.sender, Some(Principal::User(ref id)) if id == "alice"));
        assert_eq!(cap.fact_type.as_deref(), Some("preference"));
        assert_eq!(cap.topics, vec!["coffee".to_owned()]);
    }

    #[test]
    fn validate_capture_plan_strips_sender_echoed_into_allow() {
        // The classifier occasionally echoes the sender into `allow`;
        // the plan validator must strip it (capture's
        // SenderRedundantInAllow lint would otherwise kill the turn)
        // while keeping the legitimate entries.
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("alice".into()),
            target_page: None,
            subject_id: None,
            allow_ids: vec!["user:alice".into(), "group:famiglia".into()],
            fact_type: Some("preference".into()),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("alice prefers coffee black".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("alice prefers coffee black", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("validated");
        assert_eq!(cap.allow, vec![Principal::Group("famiglia".into())]);
    }

    #[test]
    fn validate_capture_plan_rejects_bad_principal() {
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("alice".into()),
            target_page: None,
            subject_id: Some("not-a-principal".into()),
            allow_ids: Vec::new(),
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("alice prefers tea".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("hello", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let err =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect_err("bad principal");
        assert!(matches!(err, CapturePlanError::BadPrincipal(_)));
    }

    /// A destination with nothing to put at it is not a capture.
    ///
    /// The legacy single-fact arm fires on `body` and nothing else. Firing on
    /// `target_wiki_id` too would synthesise a unit with no body, which
    /// `validate_capture_plan` resolves to the whole message under
    /// `allow_message_fallback`: a gesture turn whose `extractions` are empty
    /// by design (*«forget what I told you about the greenhouse»*) would file
    /// one fact whose body is that sentence, purely because the cheap model
    /// also echoed a key the prompt forbids it to emit.
    #[test]
    fn a_stray_target_wiki_id_alone_captures_nothing() {
        let mut plan = plan_with_supersede(None);
        plan.body = None;
        plan.target_wiki_id = Some("alice".into());
        assert!(
            plan.capture_units().is_empty(),
            "no body, no capture — the gesture stays a gesture"
        );
        plan.body = Some("alice prefers tea now".into());
        assert_eq!(
            plan.capture_units().len(),
            1,
            "the legacy shape still works when it carries what it is filing"
        );
    }

    /// A `target_wiki_id` that names nothing on disk is IGNORED, not fatal.
    ///
    /// The shape that produces one: a model reusing the ACL keyword `"global"`
    /// as a wiki id. The classifier is not shown wikis at all, so an id it
    /// emits anyway is noise from an operator-overridden prompt — neither a
    /// crash inside `wiki_capture` → `tree.locate` nor a hard rejection that
    /// drops the extraction. `derive_target_wiki` declines it and falls through
    /// to the sender's own wiki, so the fact survives instead of being thrown
    /// away over a field nobody asked for.
    #[test]
    fn validate_capture_plan_ignores_a_hallucinated_target_wiki() {
        let plan = LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("global".into()),
            target_page: None,
            subject_id: Some("global".into()),
            allow_ids: Vec::new(),
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("public fact".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: None,
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        };
        let request = req("public fact", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("an unknown target_wiki_id is ignored, not fatal");
        assert_eq!(
            cap.wiki_id.as_str(),
            "alice",
            "a `global`-owned fact falls through to the sender's own wiki"
        );
    }

    #[test]
    fn validate_capture_plan_drops_malformed_validity_bound_to_open() {
        // `fact_index` compares validity bounds lexicographically (due-soon
        // ranges, expiry judgements): an unresolved relative phrase the LLM
        // failed to resolve ("domani sera") must degrade to an OPEN bound
        // with a warn — never be stored verbatim, and never fall back to
        // the turn's own instant.
        let request = req("il latte scade domani sera", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];

        let malformed = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"body\":\"the milk expires\",\"valid_to\":\"domani sera\"}",
        )
        .expect("plan parses");
        let cap = validate_capture_plan(
            &first_unit(&malformed),
            &request,
            &policy,
            &available,
            &[],
            true,
        )
        .expect("a malformed bound must not kill the capture");
        assert_eq!(cap.valid_to, None, "malformed valid_to degrades to open");
        assert_eq!(cap.valid_from, None);
    }

    /// A bound that names a DAY is kept, and reaches the column as that day.
    ///
    /// It is the shape a writer produces whenever the source named a day
    /// rather than a moment — "the milk expires on the 4th" — which is most
    /// dated sentences, live or replayed. Refusing it threw away what the user
    /// actually said and left the fact with no horizon at all, so the due-soon
    /// scan had nothing to find and the fact never expired.
    #[test]
    fn validate_capture_plan_keeps_a_bound_that_names_a_day() {
        let request = req("il latte scade il 4 luglio", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];

        let dated = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"body\":\"the milk expires\",\
             \"valid_from\":\"2026-07-01\",\"valid_to\":\"2026-07-04\"}",
        )
        .expect("plan parses");
        let cap = validate_capture_plan(
            &first_unit(&dated),
            &request,
            &policy,
            &available,
            &[],
            true,
        )
        .expect("validated");
        assert_eq!(
            cap.valid_from.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "a window that opens on a day opens when the day does"
        );
        assert_eq!(
            cap.valid_to.as_deref(),
            Some("2026-07-04T23:59:59Z"),
            "and one that closes on a day keeps the whole day it names — \
             closing at midnight would cut off the 4th"
        );
    }

    #[test]
    fn validate_capture_plan_passes_rfc3339_bounds_and_keeps_absent_open() {
        let request = req("il latte scade domani sera", "alice");
        let policy = IngestPolicy::default();
        let available = vec![sample_available("alice")];

        let valid = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"body\":\"the milk expires\",\
             \"valid_from\":\"2026-07-04T10:00:00Z\",\
             \"valid_to\":\"2026-07-05T21:00:00Z\"}",
        )
        .expect("plan parses");
        let cap = validate_capture_plan(
            &first_unit(&valid),
            &request,
            &policy,
            &available,
            &[],
            true,
        )
        .expect("validated");
        assert_eq!(cap.valid_from.as_deref(), Some("2026-07-04T10:00:00Z"));
        assert_eq!(cap.valid_to.as_deref(), Some("2026-07-05T21:00:00Z"));

        let absent = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"body\":\"the milk expires\"}",
        )
        .expect("plan parses");
        let cap = validate_capture_plan(
            &first_unit(&absent),
            &request,
            &policy,
            &available,
            &[],
            true,
        )
        .expect("validated");
        assert_eq!(cap.valid_from, None, "absent valid_from stays open");
        assert_eq!(cap.valid_to, None, "absent valid_to stays open");
    }

    fn sample_available(id: &str) -> AvailableWiki {
        AvailableWiki {
            wiki_id: id.to_owned(),
            title: id.to_owned(),
            wiki_type: "wiki-user".to_owned(),
            scope: None,
            summary: None,
            smart: false,
            is_agent: false,
        }
    }

    /// An `AvailableWiki` flagged as an agent's own wiki.
    fn sample_agent_available(id: &str) -> AvailableWiki {
        AvailableWiki {
            is_agent: true,
            ..sample_available(id)
        }
    }

    #[test]
    fn validate_capture_plan_redirects_non_self_fact_off_agent_wiki() {
        let request = req("some fact", "morgana");
        let policy = IngestPolicy::default();
        // hermes1 is an agent wiki; morgana (the subject's own wiki) is in the window.
        let available = vec![
            sample_agent_available("hermes1"),
            sample_available("morgana"),
        ];
        // subject user:morgana, but the model aimed the fact at the agent wiki.
        let plan = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"hermes1\",\
             \"subject_id\":\"user:morgana\",\"body\":\"Morgana prefers herbal tea\"}",
        )
        .expect("plan parses");
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("a redirect must succeed");
        assert_eq!(
            cap.wiki_id.as_str(),
            "morgana",
            "a user-owned fact aimed at an agent wiki must be redirected to the subject's own wiki"
        );

        // With no resolvable home wiki in the window, the extraction is dropped
        // rather than misfiled into the agent wiki.
        let available_no_home = vec![sample_agent_available("hermes1")];
        let err = validate_capture_plan(
            &first_unit(&plan),
            &request,
            &policy,
            &available_no_home,
            &[],
            true,
        )
        .expect_err("no resolvable home → drop");
        assert!(
            matches!(err, CapturePlanError::TargetIsAgentWiki { .. }),
            "expected TargetIsAgentWiki, got {err:?}"
        );
    }

    /// The exception the guard needs to be a guard and not a shredder: a fact
    /// whose subject IS the agent belongs in the agent's wiki, because that wiki
    /// is the subject's own home. It reaches this function whenever a USER states
    /// something about the assistant — the `self` sentinel upstream only fires
    /// on an assistant turn. Without the exception the guard hunts for a
    /// *non*-agent wiki named `hermes1`, finds none, and drops the fact.
    #[test]
    fn validate_capture_plan_keeps_an_agent_owned_fact_in_the_agent_wiki() {
        let request = req("sei bravo con le pratiche INPS", "morgana");
        let policy = IngestPolicy::default();
        let available = vec![
            sample_agent_available("hermes1"),
            sample_available("morgana"),
        ];
        let plan = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"hermes1\",\
             \"subject_id\":\"user:hermes1\",\"body\":\"L'agente è competente sulle pratiche INPS\"}",
        )
        .expect("plan parses");
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("the agent's own fact stays home");
        assert_eq!(cap.wiki_id.as_str(), "hermes1");
        assert_eq!(cap.subject, Principal::User("hermes1".to_owned()));

        // And it is not a blanket bypass: another principal's fact aimed at the
        // same wiki still leaves it.
        let other = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"hermes1\",\
             \"subject_id\":\"user:morgana\",\"body\":\"Morgana ha una pratica INPS aperta\"}",
        )
        .expect("plan parses");
        let cap = validate_capture_plan(
            &first_unit(&other),
            &request,
            &policy,
            &available,
            &[],
            true,
        )
        .expect("redirect");
        assert_eq!(cap.wiki_id.as_str(), "morgana");
    }

    /// Two bots enrolled: a fact about bot B aimed at bot A's wiki goes to B's
    /// own wiki. The redirect looks for the subject's home and nothing else — a
    /// home that happens to be another agent's wiki is still the right home,
    /// and refusing it would drop a perfectly placeable fact.
    #[test]
    fn validate_capture_plan_redirects_between_two_agent_wikis() {
        let request = req("some fact", "morgana");
        let policy = IngestPolicy::default();
        let available = vec![
            sample_agent_available("hermes1"),
            sample_agent_available("samvisebot"),
            sample_available("morgana"),
        ];
        let plan = parse_plan(
            "{\"intent\":\"capture\",\"target_wiki_id\":\"hermes1\",\
             \"subject_id\":\"user:samvisebot\",\"body\":\"Samvise gestisce le prenotazioni\"}",
        )
        .expect("plan parses");
        let cap =
            validate_capture_plan(&first_unit(&plan), &request, &policy, &available, &[], true)
                .expect("redirect to the other agent's own wiki");
        assert_eq!(cap.wiki_id.as_str(), "samvisebot");
    }

    // ---------- supersede_target validation ----------

    fn sample_recall_hit(fact_id_str: &str) -> RecallHit {
        RecallHit {
            fact_id: FactId::parse(fact_id_str).unwrap(),
            wiki_id: "alice".into(),
            source_path: "wikis/alice/preferenze.md".into(),
            region_start: None,
            region_end: None,
            text: "alice prefers coffee black".into(),
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("preference".into()),
            created_at: "2026-05-21".into(),
            valid_from: None,
            valid_to: None,
            score: 0.91,
            fresh: false,
        }
    }

    /// Nothing replaces itself, and the judge is allowed to think it does.
    ///
    /// The candidate list and the facts this turn wrote share no id, so one id
    /// arriving in both roles is a model that invented it. The guard refuses
    /// it ahead of the candidate lookup, which would refuse it too, because
    /// acted on a self-supersede is destructive rather than merely useless:
    /// the weld finds the target still buffered, closes its window and writes
    /// no supersede link, leaving a fact that nothing contradicted marked
    /// `contradicted`, dated the moment the engine noticed.
    #[test]
    fn a_fact_never_supersedes_itself() {
        let id = "0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77";
        let hit = sample_recall_hit(id);
        let same = LlmSupersede {
            target: Some(id.to_owned()),
            successor: Some(id.to_owned()),
        };
        assert!(
            vet_supersede(
                &same,
                std::slice::from_ref(&hit),
                &[(FactId::parse(id).unwrap(), String::new())],
                "alice",
                &[],
            )
            .is_none(),
            "one fact named for both roles is refused"
        );

        // Two different facts still supersede normally.
        let other = "0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d78";
        let pair = LlmSupersede {
            target: Some(id.to_owned()),
            successor: Some(other.to_owned()),
        };
        assert!(
            vet_supersede(
                &pair,
                std::slice::from_ref(&hit),
                &[(FactId::parse(other).unwrap(), String::new())],
                "alice",
                &[],
            )
            .is_some(),
            "a genuine replacement is untouched"
        );
    }

    fn plan_with_supersede(target: Option<&str>) -> LlmIngestPlan {
        LlmIngestPlan {
            subject_external: None,
            intent: "capture".into(),
            suggested_seed: None,
            target_wiki_id: Some("alice".into()),
            target_page: None,
            subject_id: None,
            allow_ids: Vec::new(),
            fact_type: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
            requested_container: false,
            engine_rule: false,
            behaviour_rule: false,
            behaviour_scope: None,
            topics: Vec::new(),
            body: Some("alice prefers tea now".into()),
            needs_disambig: false,
            needs_project_docs: false,
            disambig_candidates: Vec::new(),
            supersede_target: target.map(str::to_owned),
            extractions: Vec::new(),
            closures: Vec::new(),
            closure_topics: Vec::new(),
            validity_edits: Vec::new(),
            acl_changes: Vec::new(),
            fact_scores: Vec::new(),
            completed_message: None,
        }
    }

    /// Test helper: the single capture unit a legacy (single-fact) plan yields.
    fn first_unit(plan: &LlmIngestPlan) -> CaptureUnit<'_> {
        plan.capture_units()
            .into_iter()
            .next()
            .expect("plan yields one capture unit")
    }

    #[test]
    fn validate_supersede_target_returns_none_when_field_absent() {
        let plan = plan_with_supersede(None);
        let hits = vec![sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab")];
        let resolved =
            validate_supersede_target(&first_unit(&plan), &req("supersede", "alice"), &[], &hits)
                .expect("absent supersede is fine");
        assert!(resolved.is_none());
    }

    #[test]
    fn validate_supersede_target_returns_none_for_empty_string() {
        let plan = plan_with_supersede(Some("   "));
        let hits = vec![sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab")];
        let resolved =
            validate_supersede_target(&first_unit(&plan), &req("supersede", "alice"), &[], &hits)
                .expect("blank supersede is none");
        assert!(resolved.is_none());
    }

    #[test]
    fn validate_supersede_target_accepts_id_in_recall() {
        let id = "018f1234-5678-7abc-9def-0123456789ab";
        let plan = plan_with_supersede(Some(id));
        let hits = vec![sample_recall_hit(id)];
        let resolved =
            validate_supersede_target(&first_unit(&plan), &req("supersede", "alice"), &[], &hits)
                .expect("resolved");
        assert_eq!(resolved.as_ref().map(FactId::as_str), Some(id));
    }

    /// Cross-user supersede guard: a capture from one user must not close
    /// a fact OWNED by another. This is the bug where morgana's primer
    /// ingest superseded franz's public profile fact — the recall
    /// surfaced his fact and the classifier mis-targeted it.
    #[test]
    fn validate_supersede_target_rejects_cross_subject() {
        let id = "018f1234-5678-7abc-9def-0123456789ab";
        let plan = plan_with_supersede(Some(id)); // subject_id None → subject = sender
        let hits = vec![sample_recall_hit(id)]; // hit subject = user:alice
        // Sender is morgana, so the new fact's subject differs from the
        // recalled fact's subject (alice): the supersede must be refused.
        let err = validate_supersede_target(&first_unit(&plan), &req("x", "morgana"), &[], &hits)
            .expect_err("cross-subject supersede must fail");
        match err {
            CapturePlanError::SupersedeCrossSubject {
                id: got_id,
                target_subject,
                new_subject,
            } => {
                assert_eq!(got_id, id);
                assert_eq!(target_subject, "user:alice");
                assert_eq!(new_subject, "user:morgana");
            },
            other => panic!("expected SupersedeCrossSubject, got {other:?}"),
        }
    }

    /// The same-subject guard compares the target with the subject the
    /// model chose for the new fact — so a plan that copies the target's
    /// subject passes it. The sender must actually *be* that subject:
    /// bob's fact, recalled into alice's turn, is not alice's to replace
    /// however the model labels the replacement.
    #[test]
    fn validate_supersede_target_rejects_a_sender_who_is_not_the_subject() {
        let id = "018f1234-5678-7abc-9def-0123456789ab";
        let mut plan = plan_with_supersede(Some(id));
        plan.subject_id = Some("user:alice".to_owned());
        let hits = vec![sample_recall_hit(id)]; // hit subject = user:alice
        let err = validate_supersede_target(&first_unit(&plan), &req("x", "bob"), &[], &hits)
            .expect_err("a sender who is not the subject must not supersede");
        assert!(
            matches!(err, CapturePlanError::SupersedeNotOwned { ref sender, .. } if sender == "bob"),
            "expected SupersedeNotOwned, got {err:?}"
        );
        // A member of the subject group is the subject.
        plan.subject_id = Some("group:team".to_owned());
        let mut hit = sample_recall_hit(id);
        hit.subject_id = "group:team".parse().unwrap();
        let ok = validate_supersede_target(
            &first_unit(&plan),
            &req("x", "bob"),
            &["team".to_owned()],
            &[hit],
        )
        .expect("a member of the subject group supersedes");
        assert!(ok.is_some());
    }

    /// Who may close a fact from chat: its subject, and whoever said it.
    ///
    /// A stranger is neither, and a turn of theirs must not reach into
    /// somebody else's memory to mark a fact finished. The author IS one of
    /// the two — closing withdraws an assertion, and withdrawing your own
    /// assertion is not editing somebody else's record — which is the half
    /// this test's second block pins.
    #[test]
    fn validate_closure_admits_the_subject_and_the_author_only() {
        let id = "018f1234-5678-7abc-9def-0123456789ab";
        let closure = LlmClosure {
            target: Some(id.to_owned()),
            reason: Some("completed".to_owned()),
            valid_to: None,
        };
        let hits = vec![sample_recall_hit(id)]; // hit subject = user:alice
        let err = validate_closure(&closure, &hits, "morgana", &[])
            .expect_err("cross-subject closure must fail");
        match err {
            ClosurePlanError::NotSubjectOrAuthor {
                id: got_id,
                subject,
            } => {
                assert_eq!(got_id, id);
                assert_eq!(subject, "user:alice");
            },
            other => panic!("expected NotSubjectOrAuthor, got {other:?}"),
        }
        // The subject herself can close it.
        assert!(validate_closure(&closure, &hits, "alice", &[]).is_ok());

        // And so can the person who said it, about somebody else: the same
        // hit, now carrying carol as its author, is closable by carol.
        let mut authored = sample_recall_hit(id);
        authored.sender_id = Some(Principal::User("carol".into()));
        let hits = vec![authored];
        assert!(
            validate_closure(&closure, &hits, "carol", &[]).is_ok(),
            "the author may withdraw what they said, whoever it was about"
        );
        // A third party is still neither.
        assert!(validate_closure(&closure, &hits, "morgana", &[]).is_err());
    }

    #[test]
    fn validate_supersede_target_rejects_malformed_fact_id() {
        let plan = plan_with_supersede(Some("not-a-uuid"));
        let hits = vec![sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab")];
        let err =
            validate_supersede_target(&first_unit(&plan), &req("supersede", "alice"), &[], &hits)
                .expect_err("malformed fact_id");
        assert!(matches!(err, CapturePlanError::BadSupersedeFactId(_)));
    }

    /// Anti-hallucination guard: even a well-formed `UUIDv7` must be
    /// rejected if it does not appear in `recalled_memory`. Otherwise
    /// the capture would crash inside `wiki_supersede` with
    /// `PreviousFactNotFound` and bubble as a hard error.
    #[test]
    fn validate_supersede_target_rejects_well_formed_id_not_in_recall() {
        let recall_id = "018f1234-5678-7abc-9def-0123456789ab";
        let hallucinated = "018f9999-9999-7999-9999-999999999999";
        let plan = plan_with_supersede(Some(hallucinated));
        let hits = vec![sample_recall_hit(recall_id)];
        let err =
            validate_supersede_target(&first_unit(&plan), &req("supersede", "alice"), &[], &hits)
                .expect_err("hallucinated supersede_target must fail");
        match err {
            CapturePlanError::SupersedeTargetNotInRecall { id, available } => {
                assert_eq!(id, hallucinated);
                assert!(
                    available.contains(recall_id),
                    "available list must enumerate the recalled id: {available}"
                );
            },
            other => panic!("expected SupersedeTargetNotInRecall, got {other:?}"),
        }
    }

    // ---------- prompt building ----------

    /// The orchestrator must surface every recall hit's `fact_id` in
    /// the prompt — otherwise the LLM cannot fill the `supersede_target`
    /// field in its plan with a value tied to something it actually saw.
    /// A recalled fact whose window has CLOSED must reach the classifier
    /// marked as history. The engine has always known — `fact_index` stores
    /// both bounds and the ranker already down-ranks a closed window — but the
    /// prompt dropped the datum at the last step, so a spent fact arrived
    /// looking durable. That is how "Zoe is away until 26 March; Alice and Bob
    /// are dining together during this period" (`valid_to` 26 March 00:00)
    /// came back for a turn at 21:30 on the 26th and lent its "Alice and Bob"
    /// to a message that said "we ate without him".
    #[test]
    fn build_prompt_marks_an_expired_recalled_fact_as_history() {
        let request = req("we ate without him", "alice");
        let policy = IngestPolicy::default();
        let mut hit = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab");
        hit.valid_from = Some("2026-03-22T08:15:00Z".to_owned());
        hit.valid_to = Some("2026-03-26T00:00:00Z".to_owned());
        let now = chrono::DateTime::parse_from_rfc3339("2026-03-26T21:30:00Z")
            .expect("fixed now")
            .with_timezone(&chrono::Utc);
        let prompt = build_prompt(
            &request,
            &[hit],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now,
            &policy,
        );
        assert!(
            prompt.contains("validity: ENDED 2026-03-26 \u{2014} history, not the present"),
            "an expired recalled fact must be marked as history; prompt was:\n{prompt}"
        );
    }

    /// The mirror case, and the one that keeps the prompt cheap: a fact with
    /// no bounds makes no claim about time and renders exactly as before, so
    /// the durable majority of hits costs no extra tokens.
    #[test]
    fn build_prompt_says_nothing_about_time_for_a_durable_recalled_fact() {
        let request = req("alice now prefers tea", "alice");
        let policy = IngestPolicy::default();
        let hit = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab");
        let prompt = build_prompt(
            &request,
            &[hit],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            !prompt.contains("validity:"),
            "a fact with no window must not grow a validity line; prompt was:\n{prompt}"
        );
    }

    #[test]
    fn build_prompt_emits_fact_id_for_recall_hits() {
        let request = req("alice now prefers tea", "alice");
        let policy = IngestPolicy::default();
        let hit_id = "018f1234-5678-7abc-9def-0123456789ab";
        let hits = vec![sample_recall_hit(hit_id)];
        let prompt = build_prompt(
            &request,
            &hits,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("recalled_memory:"));
        assert!(
            prompt.contains(&format!("fact_id: {hit_id}")),
            "fact_id must be surfaced verbatim so the LLM can reference \
             it from supersede_target; prompt was:\n{prompt}"
        );
    }

    /// Each recalled fact must surface its current `subject` (the subject)
    /// and `allow` (the audience) so the classifier can tell which facts
    /// the sender owns and faithfully reproduce the read-set on a
    /// REPLACE-semantics `acl_change`. Without this the model is blind to
    /// the ACL and silently drops allow principals.
    #[test]
    fn build_prompt_exposes_subject_and_allow_for_recall_hits() {
        let request = req("x", "alice");
        let policy = IngestPolicy::default();
        let mut hit = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab");
        hit.subject_id = Principal::User("morgana".into());
        hit.allow_ids = vec![Principal::Group("famiglia".into())];
        let prompt = build_prompt(
            &request,
            &[hit],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            prompt.contains("subject: user:morgana"),
            "recalled fact must surface its subject; prompt:\n{prompt}"
        );
        assert!(
            prompt.contains("allow: group:famiglia"),
            "recalled fact must surface its current allow; prompt:\n{prompt}"
        );
    }

    /// The sender's group memberships and each group's `scope` prose
    /// must reach the prompt — that is the context the `subject_id`
    /// decision routes on. A group whose scope the operator never set
    /// renders an explicit placeholder, and an over-long scope is
    /// truncated to the policy cap.
    #[test]
    fn build_prompt_emits_sender_groups_with_scope() {
        let request = req("detersivo finito", "alice");
        let policy = IngestPolicy::default();
        let long_scope = "x".repeat(policy.max_group_scope_chars + 50);
        let groups = vec![
            (
                "famiglia".to_owned(),
                Some("Spesa, regole di casa.".to_owned()),
            ),
            ("admins".to_owned(), None),
            ("verbose".to_owned(), Some(long_scope)),
        ];
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &groups,
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("sender_groups:"));
        assert!(prompt.contains("- id: famiglia"));
        assert!(prompt.contains("scope: Spesa, regole di casa."));
        // A group with no operator-set scope is surfaced explicitly, not
        // as an empty value the model might misread.
        assert!(prompt.contains("- id: admins"));
        assert!(prompt.contains("scope: (no scope configured)"));
        // The over-long scope is truncated with the ellipsis sentinel.
        assert!(
            prompt.contains('…'),
            "long scope must be truncated; prompt was:\n{prompt}"
        );
    }

    /// The classifier prompt names NO wiki — the destination is derived
    /// ([`derive_target_wiki`]) rather than chosen, and the window that used
    /// to carry it was a per-turn block paid at full price for one decision
    /// out of the twenty-odd this call makes. It also carried a one-line
    /// abstract of every principal's memory into every turn's prompt.
    ///
    /// The renderer itself still exists for the document extractor, which
    /// does place segments across the tree; `available_wikis_carries_both_description_fields`
    /// covers it there.
    #[test]
    fn build_prompt_names_no_wiki() {
        let request = req("la mia pressione è alta", "alice");
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            !prompt.contains("available_wikis"),
            "the wiki window must be gone; prompt was:\n{prompt}"
        );
        assert!(
            !prompt.contains("wiki_id: alice"),
            "no wiki may be named to the classifier; prompt was:\n{prompt}"
        );
        assert!(
            prompt.contains("list_pages:\n  (none yet)"),
            "the list inventory is the only placement surface; prompt was:\n{prompt}"
        );
    }

    /// The sender's `@rules.md` policy is injected into the
    /// `sender_rules` section so the classifier can honour it when assigning
    /// per-fact ACL; absent → an explicit `(none)`, and an over-long policy is
    /// truncated to the budget.
    #[test]
    fn build_prompt_emits_sender_rules_when_present_else_none() {
        let request = req("la mia pressione è alta", "alice");
        let policy = IngestPolicy::default();

        // Absent → explicit (none).
        let none = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(none.contains("sender_rules:\n  (none)"));

        // Present → the policy body is injected verbatim under the section.
        let rules = "# Rules\n\nkeep anything about my health private";
        let with = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            Some(rules),
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(with.contains("sender_rules:"));
        assert!(with.contains("keep anything about my health private"));

        // An over-long policy is shown WHOLE. The prompt calls this block the
        // sender's policy «in full», under the rule that a slot may act
        // against a set it is shown complete and never against a sample — and
        // this is the block that carries governance. Cutting it dropped the
        // last rule of anyone whose `@rules.md` had grown, silently, while the
        // prompt asserted they had seen everything.
        let long = format!(
            "{}\ni fatti sulla mia salute restano privati",
            "x".repeat(policy.max_sender_rules_chars + 50)
        );
        let whole = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            Some(&long),
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            whole.contains("i fatti sulla mia salute restano privati"),
            "the last rule of a long policy still reaches the model:\n{whole}"
        );
        assert!(!whole.contains('…'), "and nothing is cut mid-word: {whole}");
    }

    /// The known-users roster is injected with id + aliases so the
    /// classifier can attribute a fact to the right person by canonical name.
    #[test]
    fn build_prompt_emits_known_users_with_aliases() {
        let request = req("Bob preferisce il tè", "alice");
        let policy = IngestPolicy::default();
        let known = vec![
            enrollment::EnrolledUserLite {
                user_id: "bob".to_owned(),
                aliases: vec!["Bob".to_owned(), "Bobby".to_owned()],
                is_agent: false,
            },
            enrollment::EnrolledUserLite {
                user_id: "alice".to_owned(),
                aliases: Vec::new(),
                is_agent: false,
            },
        ];
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &known,
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("known_users:"));
        assert!(prompt.contains("- id: bob"));
        assert!(prompt.contains("aliases: Bob, Bobby"));
        assert!(prompt.contains("- id: alice"));
        assert!(
            !prompt.contains("is_agent"),
            "a roster of humans says nothing about agents"
        );
    }

    /// The assistant is in the roster like any other principal, and says so:
    /// without the flag the classifier reads its own id as one more person.
    #[test]
    fn build_prompt_names_the_agent_in_the_known_users_roster() {
        let request = req("Gandalf, che ore sono?", "alice");
        let policy = IngestPolicy::default();
        let known = vec![
            enrollment::EnrolledUserLite {
                user_id: "alice".to_owned(),
                aliases: Vec::new(),
                is_agent: false,
            },
            enrollment::EnrolledUserLite {
                user_id: "hermes1".to_owned(),
                aliases: vec!["Gandalf".to_owned()],
                is_agent: true,
            },
        ];
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &known,
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            prompt.contains("- id: hermes1\n    aliases: Gandalf\n    is_agent: true"),
            "the agent's own roster entry carries the flag: {prompt}"
        );
        let alice_line = prompt
            .split("- id: alice")
            .nth(1)
            .expect("alice in the roster");
        assert!(
            !alice_line.starts_with("\n    is_agent"),
            "a human's entry stays flagless"
        );
    }

    /// With no enrolled users the section renders an explicit `(none)`.
    #[test]
    fn build_prompt_renders_none_when_no_known_users() {
        let request = req("ciao", "alice");
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("known_users:\n  (none)"));
    }

    /// With no group memberships the section renders an explicit
    /// `(none)` so the model never sees a dangling `sender_groups:`
    /// header it might try to fill from imagination.
    #[test]
    fn build_prompt_renders_none_when_sender_has_no_groups() {
        let request = req("ciao", "alice");
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("sender_groups:\n  (none)"));
    }

    /// Every group the sender is in reaches the prompt — the roster is not
    /// cut here, and the builtin `global` row costs nobody a real group.
    ///
    /// `MAX_GROUPS_PER_USER` refuses the ninth group where a group is
    /// created, and a product limit is enforced there, never by cutting the
    /// list when the prompt is built (founder, 2026-08-09). The cut this
    /// replaced was worse than redundant: `global` is appended to every
    /// sender and is not counted by that refusal, so a user in the maximum
    /// legal 8 groups always lost the one that sorted last — and the fact
    /// that should have been routed to that group's domain was filed private
    /// instead.
    #[test]
    fn build_prompt_shows_every_group_including_the_builtin() {
        let request = req("now", "alice");
        let mut groups: Vec<(String, Option<String>)> = (0..enrollment::MAX_GROUPS_PER_USER)
            .map(|i| (format!("g{i}"), Some(format!("scope {i}"))))
            .collect();
        // The builtin, exactly as the caller appends it: one row past the cap.
        groups.push(("global".to_owned(), None));
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &groups,
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &IngestPolicy::default(),
        );
        for (id, _) in &groups {
            assert!(
                prompt.contains(&format!("- id: {id}")),
                "{id} reached the prompt: {prompt}"
            );
        }
    }

    #[test]
    fn agent_self_fact_page_leaves_identity_unplaced_and_splits_relationships_per_user() {
        // An identity self-fact names no page: it waits in the queue, and the
        // placement pass lifts it onto the card it consolidates.
        assert_eq!(agent_self_fact_page(true, "morgana"), None);
        // Relationship self-facts → a per-served-user page, flat slug.
        assert_eq!(
            agent_self_fact_page(false, "morgana"),
            Some(PathBuf::from("esperienze_morgana.md"))
        );
        assert_eq!(
            agent_self_fact_page(false, "frodo"),
            Some(PathBuf::from("esperienze_frodo.md"))
        );
        // A relationship fact with no served user names no page either.
        assert_eq!(agent_self_fact_page(false, ""), None);
    }

    #[test]
    fn normalize_capture_page_handles_untrusted_target_page() {
        // `None` is the answer, not a failure: nobody named a usable page, so
        // the claim waits in the queue.
        assert_eq!(normalize_capture_page(None), None);
        assert_eq!(normalize_capture_page(Some("   ")), None);

        // Already a canonical .md path → preserved verbatim.
        assert_eq!(
            normalize_capture_page(Some("spesa.md")),
            Some(PathBuf::from("spesa.md"))
        );
        // A coined name is ONE segment: a `/` is flattened, never honoured.
        // Left alone it made the folder — the write creates missing parents —
        // so a container nobody asked for was born out of a page name
        // (founder, 2026-08-19: *«un utente non può poter creare cartelle»*).
        assert_eq!(
            normalize_capture_page(Some("spesa/detersivi.md")),
            Some(PathBuf::from("spesa_detersivi.md"))
        );

        // Missing extension on a safe slug → `.md` appended (the
        // `lista_spesa` extension-less-file bug).
        assert_eq!(
            normalize_capture_page(Some("lista_spesa")),
            Some(PathBuf::from("lista_spesa.md"))
        );

        // Spelling variants collapse to ONE canonical page (the
        // `lista-spesa.md` vs `lista_spesa.md` duplicate-page bug).
        assert_eq!(
            normalize_capture_page(Some("lista-spesa")),
            Some(PathBuf::from("lista_spesa.md"))
        );
        assert_eq!(
            normalize_capture_page(Some("Lista della Spesa.md")),
            Some(PathBuf::from("lista_della_spesa.md"))
        );

        // Uppercase / accented names are slugified instead of being dropped
        // (the `Argo` internal-error bug: the fact now lands on its own
        // sensible page).
        assert_eq!(
            normalize_capture_page(Some("Argo")),
            Some(PathBuf::from("argo.md"))
        );
        assert_eq!(
            normalize_capture_page(Some("attività")),
            Some(PathBuf::from("attivit.md"))
        );

        // Path traversal names no page.
        assert_eq!(normalize_capture_page(Some("../escape")), None);
        assert_eq!(normalize_capture_page(Some("---")), None);
    }

    #[test]
    fn build_prompt_lists_the_open_lists_and_the_message() {
        let request = req("the quick brown fox", "alice");
        let list_pages = vec![
            fact_index::ListPage {
                wiki_id: "famiglia".into(),
                page: "spesa.md".into(),
                description: Some("what the family still needs to buy".into()),
            },
            fact_index::ListPage {
                wiki_id: "alice".into(),
                page: "da_leggere.md".into(),
                description: None,
            },
        ];
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &list_pages,
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(prompt.contains("sender_id: alice"));
        assert!(prompt.contains("context_hint: conversation"));
        // The page name and what it holds — the two things the model needs to
        // reuse an existing list instead of minting a second one.
        assert!(prompt.contains("- page: spesa.md"), "{prompt}");
        assert!(
            prompt.contains("holds: what the family still needs to buy"),
            "{prompt}"
        );
        // A list with no recorded description takes one line, not an empty key.
        assert!(prompt.contains("- page: da_leggere.md\n"), "{prompt}");
        assert!(!prompt.contains("holds: \n"), "{prompt}");
        // The wiki a list lives in is the ENGINE's business, never the model's.
        assert!(!prompt.contains("famiglia"), "{prompt}");
        assert!(prompt.contains("current_message: the quick brown fox"));
        assert!(prompt.contains("recalled_memory:\n  (none)"));
    }

    #[test]
    fn build_prompt_injects_current_time_anchor() {
        // The reference instant is injected verbatim (ISO-8601, seconds, UTC)
        // plus the English weekday, so the classifier can resolve a relative
        // date ("giovedì alle 17") into a concrete `due_at` for a wiki-cron.
        let request = req("giovedì alle 17 dal dentista", "alice");
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            prompt.contains("current_time: 2026-06-04T12:00:00Z (Thursday)"),
            "missing reference-time anchor:\n{prompt}"
        );
    }

    #[test]
    fn build_prompt_omits_user_timezone_when_unset() {
        // Default policy has no timezone → the anchor stays UTC-only, byte for
        // byte the historical behaviour (no `user_timezone:` line).
        let request = req("alle 16 prendo il pane", "alice");
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            !prompt.contains("user_timezone:"),
            "unset timezone must not emit a user_timezone line:\n{prompt}"
        );
    }

    #[test]
    fn build_prompt_injects_user_timezone_when_set() {
        // With a declared zone the classifier gets a `user_timezone:` line so
        // it can convert a wall-clock time the user speaks to UTC.
        let request = req("alle 16 prendo il pane", "alice");
        let policy = IngestPolicy {
            ingest_timezone: Some("Europe/Rome".to_owned()),
            ..IngestPolicy::default()
        };
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            prompt.contains("current_time: 2026-06-04T12:00:00Z (Thursday)"),
            "the UTC anchor must remain:\n{prompt}"
        );
        assert!(
            prompt.contains("user_timezone: Europe/Rome"),
            "missing user_timezone line:\n{prompt}"
        );
    }

    #[test]
    fn build_prompt_sender_timezone_wins_over_deployment_default() {
        // Two users of the same deployment can live in different zones
        // (the founder in London, another user in Sydney): the sender's
        // own zone (`enrollment_users.timezone`) overrides the
        // deployment-wide `recall.ingest_timezone`; without one, the
        // deployment default still applies.
        let request = req("ricordamelo domani alle 9", "carol");
        let policy = IngestPolicy {
            ingest_timezone: Some("Europe/London".to_owned()),
            ..IngestPolicy::default()
        };
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            Some("Australia/Sydney"),
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            prompt.contains("user_timezone: Australia/Sydney"),
            "sender zone must win:\n{prompt}"
        );
        assert!(
            !prompt.contains("Europe/London"),
            "deployment default must not leak alongside:\n{prompt}"
        );
    }

    #[test]
    fn build_prompt_truncates_long_recent_message() {
        let mut request = req("now", "alice");
        let long = "x".repeat(1_000);
        request.recent_messages.push(RecentMessage {
            role: MessageRole::User,
            text: long,
            timestamp: None,
        });
        let policy = IngestPolicy::default();
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        // The truncated body should end with the ellipsis sentinel.
        assert!(prompt.contains('…'));
    }

    #[test]
    fn build_prompt_caps_recent_messages_at_policy() {
        let mut request = req("now", "alice");
        for i in 0..10 {
            request.recent_messages.push(RecentMessage {
                role: MessageRole::User,
                text: format!("m{i}"),
                timestamp: None,
            });
        }
        let policy = IngestPolicy {
            max_recent_messages: 3,
            ..IngestPolicy::default()
        };
        let prompt = build_prompt(
            &request,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        // We should see only the last 3 (m7, m8, m9), not the earlier ones.
        assert!(prompt.contains("m7"));
        assert!(prompt.contains("m9"));
        assert!(!prompt.contains("m0"));
        assert!(!prompt.contains("m5"));
    }

    // ---------- recall-miss detection (self-correcting REM floor) ----------

    /// The direct half of the judge-free miss signal: the user restates a
    /// fact memory holds, this turn's recall does not surface it (blind
    /// top-K), the write-time dedup skips the capture — one `recall_miss`
    /// record lands, linked to the turn's recall log. The promotion half is
    /// pinned beside its own path, in `dream_light`.
    #[tokio::test]
    async fn ingest_records_recall_miss_on_unsurfaced_dedup_hit() {
        let (dir, tree, pool) = setup_workdir().await;
        let existing = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-00000000f101").unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/colore.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "Il colore preferito di Alice è l'indaco.".to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            // Materialised, like every real write — provenance is never NULL
            // (`capture::normalize_sender_attribution`), and the dedup compares
            // it as stored.
            sender_id: Some(Principal::User("alice".into())),
            fact_type: Some("preference".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            salience: None,
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(&pool, &existing).await.expect("insert");

        // requested_container → the direct path (write-time dedup fires
        // in-turn); recall_top_k 0 → the turn's recall is blind, which is the
        // premise: memory held the fact and did not offer it, so the user had
        // to say it again.
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"colore.md\",\
            \"subject_id\":\"user:alice\",\
            \"body\":\"Il colore preferito di Alice è l'indaco.\",\
            \"fact_type\":\"preference\",\"requested_container\":true}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy {
            recall_top_k: 0,
            recall_fresh_top_k: 0,
            ..IngestPolicy::default()
        };
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("il mio colore preferito è l'indaco", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);
        let misses = recall_log::recent_misses(&pool, 10).await.unwrap();
        assert_eq!(misses.len(), 1, "one restated-known-fact miss: {misses:?}");
        assert_eq!(misses[0].fact_id, "018f1234-5678-7abc-9def-00000000f101");
        assert_eq!(misses[0].surface, "direct");
        assert_eq!(misses[0].sender_id, "alice");
        assert!(
            misses[0].log_id.is_some(),
            "linked to the turn's recall-log row"
        );
        drop(dir);
    }

    /// No false positive: when the turn's recall DID surface the fact the
    /// user restated, the dedup skip is just a dedup — no miss recorded.
    #[tokio::test]
    async fn ingest_records_no_miss_when_recall_surfaced_the_fact() {
        let (dir, tree, pool) = setup_workdir().await;
        let existing = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-00000000f102").unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/colore.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "Il colore preferito di Alice è l'indaco.".to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("preference".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            salience: None,
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(&pool, &existing).await.expect("insert");

        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"colore.md\",\
            \"subject_id\":\"user:alice\",\
            \"body\":\"Il colore preferito di Alice è l'indaco.\",\
            \"fact_type\":\"preference\",\"requested_container\":true}]}";
        let llm = FakeLlmBackend::new("fake", json);
        // Default top-K + the fixed fake embedding → the flat recall
        // surfaces the existing fact this same turn.
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("il mio colore preferito è l'indaco", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        let misses = recall_log::recent_misses(&pool, 10).await.unwrap();
        assert!(
            misses.is_empty(),
            "a surfaced fact is a plain dedup, not a miss: {misses:?}"
        );
        drop(dir);
    }

    // ---------- snippet formatting ----------

    #[test]
    fn project_docs_render_in_their_own_labelled_slot() {
        // The slot must be unmistakably reference material: without the
        // label a documentation paragraph reads exactly like a recalled
        // fact, and the classifier would file it back as one.
        let fact = RecallHit {
            fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap(),
            wiki_id: "franz".into(),
            source_path: "wikis/franz/preferenze.md".into(),
            region_start: None,
            region_end: None,
            text: "franz lives in Bologna".into(),
            subject_id: Principal::User("franz".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            created_at: "2026-05-18".into(),
            valid_from: None,
            valid_to: None,
            score: 0.9,
            fresh: false,
        };
        let doc = recall::SectionHit {
            wiki_id: "franz-acmesigns".into(),
            source_path: "wikis/franz/acmesigns/architecture/Delivery.md".into(),
            section_ord: 2,
            heading_path: Some("Delivery".into()),
            text: "Content is pushed to each display over a websocket.".into(),
            score: 0.88,
        };

        let snippet =
            format_snippet(std::slice::from_ref(&fact), &[], &[doc], 0.0).expect("renders");
        assert!(snippet.contains("franz lives in Bologna"), "{snippet}");
        assert!(
            snippet.contains("Project documentation (reference — never file this as a fact):"),
            "the slot must be labelled: {snippet}"
        );
        // The citation names the wiki and the page, not the workdir path.
        assert!(
            snippet.contains("(franz-acmesigns · architecture/Delivery.md)"),
            "{snippet}"
        );
        // Facts keep their trust tag; a doc line carries none — it has no
        // validity window and no capture date to reason about.
        assert!(snippet.contains("[noted 2026-05-18]"), "{snippet}");
        assert!(
            !snippet.contains("websocket.\n [noted"),
            "a doc line must not be tagged like a fact: {snippet}"
        );

        // No docs → no slot at all, so an ordinary turn's block is unchanged.
        let plain = format_snippet(&[fact], &[], &[], 0.0).expect("renders");
        assert!(!plain.contains("Project documentation"), "{plain}");
    }

    #[test]
    fn page_of_strips_the_workdir_and_wiki_prefix() {
        assert_eq!(
            page_of("wikis/franz/acmesigns/architecture/X.md"),
            "architecture/X.md"
        );
        assert_eq!(page_of("wikis/franz/acmesigns/README.md"), "README.md");
        // Unexpected shapes fall back to the whole path rather than lying.
        assert_eq!(page_of("odd.md"), "odd.md");
    }

    // ---------- the two words a fact carries ----------

    #[test]
    fn the_pair_is_two_words_lowercased_and_in_the_order_written() {
        assert_eq!(
            normalize_fact_topics(&[
                "  Salute ".into(),
                "NAUSEA".into(),
                "terzo".into(),
                "quarto".into()
            ]),
            vec!["salute", "nausea"],
            "macrotopic first, microtopic second, the rest dropped"
        );
    }

    /// Empty, blank and repeated words cost nothing and take no slot: a
    /// classifier that says the same word twice has said one word.
    #[test]
    fn blanks_and_repeats_do_not_take_a_slot() {
        assert_eq!(
            normalize_fact_topics(&[
                String::new(),
                "  ".into(),
                "auto".into(),
                "AUTO".into(),
                "finanziamento".into()
            ]),
            vec!["auto", "finanziamento"]
        );
        assert!(normalize_fact_topics(&[]).is_empty());
    }

    /// The engine writes its own bookkeeping into this column, and the
    /// classifier must not be able to write it: one of these words forges a
    /// project signpost, and `signpost-day:` coins a fresh word every day —
    /// which a ranking over these words would count as a subject.
    #[test]
    fn the_engines_own_markers_can_never_be_written_by_the_model() {
        assert_eq!(
            normalize_fact_topics(&[
                "signpost-description".into(),
                "project-signpost".into(),
                "signpost-day:2026-07-14".into(),
                "salute".into(),
            ]),
            vec!["salute"]
        );
    }

    // ---------- the completed message ----------

    /// The second search TAKES THE PLACE of the first, so the block it fills
    /// is still `recall_top_k` deep and not the whole seed depth.
    ///
    /// The seed depth is what the entry fan needs to find doors on distinct
    /// pages; carrying it into the block would treble the characters injected
    /// into the consumer's prompt on every turn the classifier completes.
    #[tokio::test]
    async fn the_completed_message_does_not_widen_the_block() {
        let (dir, tree, pool) = setup_workdir().await;
        for i in 0..6u8 {
            insert_page_fact(
                &pool,
                &format!("018f1234-5678-7abc-9def-0000000d000{i}"),
                "alice",
                "wikis/alice/note.md",
                &format!("Alice keeps spare key number {i} at the gate."),
                Principal::User("alice".into()),
            )
            .await;
        }
        let llm = FakeLlmBackend::new(
            "fake",
            r#"{"intent":"recall","completed_message":"le chiavi di alice"}"#,
        );
        let policy = IngestPolicy {
            recall_top_k: 2,
            nav_seed_depth: 6,
            relevance_floor: 0.0,
            ..IngestPolicy::default()
        };
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("dove sono le sue?", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let snippet = resp.context_snippet.expect("recall block present");
        let promoted = snippet.lines().filter(|l| l.starts_with("- (")).count();
        assert_eq!(
            promoted, 2,
            "the block keeps recall_top_k, not the seed depth: {snippet}"
        );
        drop(dir);
    }

    /// The field the second search runs on parses off the wire, and is absent
    /// on the turns that need nothing written in — which is most of them.
    #[test]
    fn the_completed_message_parses_and_is_absent_by_default() {
        let with: LlmIngestPlan = serde_json::from_str(
            r#"{"intent":"recall","completed_message":"i reni di bob sono peggiorati"}"#,
        )
        .expect("parses");
        assert_eq!(
            with.completed_message.as_deref(),
            Some("i reni di bob sono peggiorati")
        );

        let without: LlmIngestPlan =
            serde_json::from_str(r#"{"intent":"recall"}"#).expect("parses");
        assert_eq!(
            without.completed_message, None,
            "most turns complete nothing"
        );
    }

    // ---------- the classifier's vote on the recalled facts ----------

    /// Two hits, a whisker apart, and the classifier says the second one is
    /// the one that answers.
    fn scored_pair() -> Vec<RecallHit> {
        let mk = |id: &str, text: &str, score: f32| RecallHit {
            fact_id: FactId::parse(id).unwrap(),
            wiki_id: "alice".into(),
            source_path: "wikis/alice/note.md".into(),
            region_start: None,
            region_end: None,
            text: text.into(),
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            created_at: "2026-05-18".into(),
            valid_from: None,
            valid_to: None,
            score,
            fresh: false,
        };
        vec![
            mk(
                "018f1234-5678-7abc-9def-0123456789ab",
                "alice was born on 12 march",
                0.47,
            ),
            mk(
                "018f1234-5678-7abc-9def-0123456789ac",
                "alice likes metallica",
                0.45,
            ),
        ]
    }

    fn score_of(hits: &[RecallHit], id: &str) -> f32 {
        hits.iter()
            .find(|h| h.fact_id.as_str() == id)
            .expect("hit present")
            .score
    }

    /// The whole point: a distance cannot tell which fact ANSWERS, and the
    /// classifier can. Two hits 0.02 apart swap places on a ±10 % band.
    #[test]
    fn the_classifier_can_lift_the_fact_that_answers_over_the_one_that_merely_matches() {
        let scores = vec![
            LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ab".into()),
                multiplier: Some(0.90),
            },
            LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ac".into()),
                multiplier: Some(1.10),
            },
        ];
        let out = apply_classifier_vote(&scored_pair(), &scores);
        assert_eq!(out[0].text, "alice likes metallica", "{out:?}");
    }

    /// The band is a fence, not a suggestion: anything outside it is pulled
    /// back to the edge rather than refused, so one wild number costs the
    /// ordering nothing.
    #[test]
    fn a_multiplier_outside_the_band_is_clamped_to_it() {
        let scores = vec![
            LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ab".into()),
                multiplier: Some(9.0),
            },
            LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ac".into()),
                multiplier: Some(0.0),
            },
        ];
        let out = apply_classifier_vote(&scored_pair(), &scores);
        let close = |got: f32, want: f32| (got - want).abs() < 1e-6;
        let lifted = 0.47f32 * FACT_SCORE_MAX;
        let pushed = 0.45f32 * FACT_SCORE_MIN;
        assert!(
            close(
                score_of(&out, "018f1234-5678-7abc-9def-0123456789ab"),
                lifted
            ),
            "{out:?}"
        );
        assert!(
            close(
                score_of(&out, "018f1234-5678-7abc-9def-0123456789ac"),
                pushed
            ),
            "{out:?}"
        );
    }

    /// An id the model never saw is dropped, the same way `supersede_target`,
    /// `closures`, `validity_edits` and `acl_changes` drop theirs — and a
    /// malformed entry is worth exactly what an absent one is worth.
    #[test]
    fn an_unseen_id_or_a_missing_multiplier_changes_nothing() {
        let hits = scored_pair();
        for scores in [
            vec![LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ff".into()),
                multiplier: Some(1.10),
            }],
            vec![LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ab".into()),
                multiplier: None,
            }],
            vec![LlmFactScore {
                target: None,
                multiplier: Some(1.10),
            }],
            vec![LlmFactScore {
                target: Some("018f1234-5678-7abc-9def-0123456789ab".into()),
                multiplier: Some(f32::NAN),
            }],
            Vec::new(),
        ] {
            let out = apply_classifier_vote(&hits, &scores);
            assert_eq!(out[0].text, hits[0].text, "{scores:?}");
            assert!((score_of(&out, "018f1234-5678-7abc-9def-0123456789ab") - 0.47).abs() < 1e-6);
        }
    }

    /// A fact the classifier declined to judge keeps its place. Half an
    /// opinion must not shuffle the half it had no opinion about.
    #[test]
    fn a_fact_left_unjudged_keeps_its_place() {
        let scores = vec![LlmFactScore {
            target: Some("018f1234-5678-7abc-9def-0123456789ac".into()),
            multiplier: Some(1.02),
        }];
        let out = apply_classifier_vote(&scored_pair(), &scores);
        assert_eq!(
            out[0].text, "alice was born on 12 march",
            "0.47 > 0.45 * 1.02: {out:?}"
        );
        assert!((score_of(&out, "018f1234-5678-7abc-9def-0123456789ab") - 0.47).abs() < 1e-6);
    }

    #[test]
    fn format_snippet_joins_hits_with_wiki_prefix() {
        let hits = vec![
            RecallHit {
                fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap(),
                wiki_id: "alice".into(),
                source_path: "wikis/alice/preferenze.md".into(),
                region_start: None,
                region_end: None,
                text: "alice likes coffee".into(),
                subject_id: Principal::User("alice".into()),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                created_at: "2026-05-18".into(),
                valid_from: None,
                valid_to: None,
                score: 0.91,
                fresh: false,
            },
            RecallHit {
                fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ac").unwrap(),
                wiki_id: "bob".into(),
                source_path: "wikis/bob/preferenze.md".into(),
                region_start: None,
                region_end: None,
                text: "bob likes tea".into(),
                subject_id: Principal::User("bob".into()),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                created_at: "2026-05-18".into(),
                valid_from: None,
                valid_to: None,
                score: 0.84,
                fresh: false,
            },
            RecallHit {
                fact_id: FactId::parse("018f1234-5678-7abc-9def-0123456789ad").unwrap(),
                wiki_id: "alice".into(),
                source_path: String::new(), // fresh: nothing written yet
                region_start: None,
                region_end: None,
                text: "alice just joined a gym".into(),
                subject_id: Principal::User("alice".into()),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                created_at: "2026-06-02".into(),
                valid_from: None,
                valid_to: None,
                score: 0.88,
                fresh: true,
            },
        ];
        let snippet = format_snippet(&hits, &[], &[], 0.0).expect("non-empty hits render");
        // The flat slot is a labelled role section now.
        assert!(snippet.starts_with(HDR_RELEVANT_MEMORY), "{snippet}");
        assert!(snippet.contains("(alice) alice likes coffee"));
        assert!(snippet.contains("(bob) bob likes tea"));
        // The fresh (un-promoted) capture lands in its own labelled slot,
        // after the promoted facts.
        assert!(snippet.contains("Recent (not yet consolidated):"));
        assert!(snippet.contains("(alice) alice just joined a gym"));
        assert!(
            snippet.find("alice likes coffee").unwrap()
                < snippet.find("Recent (not yet consolidated):").unwrap()
        );
        // Every line carries the in-band trust tag (the noted date).
        assert!(snippet.contains("alice likes coffee [noted 2026-05-18]"));
        assert!(snippet.contains("alice just joined a gym [noted 2026-06-02]"));
    }

    #[test]
    fn format_snippet_dedups_navigated_pages_and_skips_rules_hits() {
        let mut navigated_home = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab");
        navigated_home.text = "franz likes indigo".into();
        navigated_home.source_path = "wikis/franz/preferenze.md".into();
        let mut rules_hit = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ac");
        rules_hit.text = "always use the claude-code skill".into();
        rules_hit.source_path = "wikis/hermes1/@rules.md".into();
        let mut kept = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ad");
        kept.text = "matteo's pronouns are he/him".into();
        kept.source_path = "wikis/matteo/preferenze.md".into();

        let nav_paths = vec!["wikis/franz/preferenze.md".to_owned()];
        let snippet = format_snippet(
            &[navigated_home, rules_hit, kept.clone()],
            &nav_paths,
            &[],
            0.0,
        )
        .expect("one hit survives");
        // A hit homed on a navigated page is dropped (its prose rides the
        // navigated section); a rules-page hit is channel-only.
        assert!(!snippet.contains("franz likes indigo"), "{snippet}");
        assert!(!snippet.contains("claude-code skill"), "{snippet}");
        assert!(snippet.contains("matteo's pronouns are he/him"));
        // All hits filtered → the whole section is omitted.
        assert_eq!(
            format_snippet(&[kept], &["wikis/matteo/preferenze.md".into()], &[], 0.0),
            None
        );
    }

    // ---------- WHO IS SPEAKING — the identity card ----------

    /// Add the one-line `summary` key to a wiki's `_meta.md`, so
    /// [`crate::wiki::meta_summary`] has a line to fall back to.
    fn set_wiki_summary(wikis_dir: &Path, slug: &str, summary: &str) {
        let path = wikis_dir.join(slug).join("_meta.md");
        let meta = std::fs::read_to_string(&path).unwrap();
        let patched = meta.replace("\n---\n", &format!("\nsummary: '{summary}'\n---\n"));
        std::fs::write(&path, patched).unwrap();
    }

    /// Insert one `fact_index` row homed on `source_path`, owned by
    /// `subject` — the DB half of a `{{f=…}}` region on the page.
    async fn insert_page_fact(
        pool: &SqlitePool,
        fact_id: &str,
        wiki_id: &str,
        source_path: &str,
        text: &str,
        subject: Principal,
    ) {
        let fact = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse(fact_id).unwrap(),
            wiki_id: wiki_id.to_owned(),
            source_path: source_path.to_owned(),
            region_start: None,
            region_end: None,
            text: text.to_owned(),
            embedding: vec![0.9, -0.3, 0.2, -0.1],
            subject_id: subject,
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("bio".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            salience: Some("high".to_owned()),
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(pool, &fact).await.expect("insert fact");
    }

    const ALICE_FACT_A: &str = "018f1234-5678-7abc-9def-00000000c001";
    const ALICE_FACT_B: &str = "018f1234-5678-7abc-9def-00000000c002";

    /// Write alice a real identity card: a testata, two fact regions, a
    /// paragraph of connective prose, and an authored `[[wikilink]]`.
    async fn seed_alice_card(dir: &TempDir, pool: &SqlitePool) {
        let wikis = dir.path().join("wikis");
        set_wiki_summary(
            &wikis,
            "alice",
            "Profile of Alice, a bookbinder in Bologna.",
        );
        std::fs::write(
            wikis.join("alice").join("@profile.md"),
            format!(
                "---\ntitle: Alice\nkeywords:\n  topics: bio, city, craft\n---\n\n\
                 {{{{f={ALICE_FACT_A}}}}}Alice lives in Bologna.{{{{/}}}}\n\n\
                 She binds books by hand. {{{{f={ALICE_FACT_B}}}}}Alice is allergic to walnuts.{{{{/}}}}\n\n\
                 Her workshop is written up on [[alice/hobbies]].\n"
            ),
        )
        .unwrap();
        insert_page_fact(
            pool,
            ALICE_FACT_A,
            "alice",
            "wikis/alice/@profile.md",
            "Alice lives in Bologna.",
            Principal::User("alice".into()),
        )
        .await;
        insert_page_fact(
            pool,
            ALICE_FACT_B,
            "alice",
            "wikis/alice/@profile.md",
            "Alice is allergic to walnuts.",
            Principal::User("alice".into()),
        )
        .await;
    }

    /// 69d: a third party the turn NAMES gets their card served — and the
    /// card is projected for **the reader**, not for its subject.
    ///
    /// This is the test that makes 69a's projection invariant load-bearing:
    /// on the sender's own card a foreign-owned region is a no-op by
    /// construction (nobody else's fact lands there), but here Franz is
    /// handed *Alice's* card, and a region on it that only Carol may read
    /// must not reach him.
    #[tokio::test]
    async fn a_named_third_party_gets_their_card_served_projected_for_the_reader() {
        const CAROL_ONLY: &str = "018f1234-5678-7abc-9def-00000000c009";
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let page = dir
            .path()
            .join("wikis")
            .join("alice")
            .join(crate::wiki::PROFILE_FILENAME);
        let existing = std::fs::read_to_string(&page).unwrap();
        std::fs::write(
            &page,
            format!("{existing}\n{{{{f={CAROL_ONLY}}}}}Alice is planning a surprise.{{{{/}}}}\n"),
        )
        .unwrap();
        insert_page_fact(
            &pool,
            CAROL_ONLY,
            "alice",
            "wikis/alice/@profile.md",
            "Alice is planning a surprise.",
            Principal::User("carol".into()),
        )
        .await;
        // A card whose every fact is private to its subject renders to
        // nothing for anybody else — correct, and it would make this test
        // pass for the wrong reason. Alice's home town is public, the way
        // most of an identity card is; the surprise stays Carol's.
        sqlx::query("UPDATE fact_index SET subject_id = 'global' WHERE fact_id = ?")
            .bind(ALICE_FACT_A)
            .execute(&pool)
            .await
            .expect("publish the town");
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: ["alice", "franz"]
                .into_iter()
                .map(|id| crate::enrollment::UserEntry {
                    id: id.to_owned(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                })
                .collect(),
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let out = people_mentioned_section(
            &pool,
            &tree,
            &SenderContext::user("franz"),
            "cosa cucino stasera per alice?",
            &[],
            &IngestPolicy::default(),
        )
        .await
        .expect("alice is named, so her card is served");

        assert!(
            out.section.starts_with(HDR_PEOPLE_MENTIONED),
            "{out:?}",
            out = out.section
        );
        assert!(
            out.section.contains("Alice lives in Bologna."),
            "the card's prose rides the block: {}",
            out.section
        );
        assert!(
            !out.section.contains("Alice is planning a surprise."),
            "a region THIS reader may not see must never reach the block: {}",
            out.section
        );
        assert_eq!(
            out.served,
            vec![("alice".to_owned(), PathBuf::from(IDENTITY_PAGE))],
            "the served page is handed to the walk as already visited"
        );
        assert_eq!(
            out.page_paths,
            vec!["wikis/alice/@profile.md".to_owned()],
            "and to the flat slot, so its facts are not restated"
        );
    }

    /// The sender is served by `WHO IS SPEAKING`; naming themselves must not
    /// buy a second copy. And a turn that names nobody produces no slot.
    #[tokio::test]
    async fn the_speaker_is_never_their_own_mentioned_card_and_no_name_means_no_slot() {
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: vec![crate::enrollment::UserEntry {
                id: "alice".to_owned(),
                aliases: Vec::new(),
                is_admin: false,
                locale: None,
                timezone: None,
            }],
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");
        let tree = WikiTree::open(dir.path()).expect("reopen tree");
        let policy = IngestPolicy::default();

        assert!(
            people_mentioned_section(
                &pool,
                &tree,
                &SenderContext::user("alice"),
                "cosa mangio stasera?",
                &[],
                &policy,
            )
            .await
            .is_none(),
            "the sender's own card is WHO IS SPEAKING's job"
        );
        assert!(
            people_mentioned_section(
                &pool,
                &tree,
                &SenderContext::user("franz"),
                "ricordami di chiamare l'idraulico",
                &[],
                &policy,
            )
            .await
            .is_none(),
            "a turn naming nobody opens no slot"
        );
    }

    /// The second gate: a turn that names nobody the roster can match still
    /// gets the card, because the facts the search returned say whose they
    /// are. This is the paraphrase case — *«mia moglie»* is in no roster —
    /// and it is the only route left, a card being no link destination.
    #[tokio::test]
    async fn a_card_arrives_from_the_facts_the_search_found_when_the_turn_names_nobody() {
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        // A card private to its subject renders to nothing for anybody else,
        // which would pass this test for the ACL's reason and not the gate's.
        sqlx::query("UPDATE fact_index SET subject_id = 'global' WHERE fact_id = ?")
            .bind(ALICE_FACT_A)
            .execute(&pool)
            .await
            .expect("publish the town");
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: ["alice", "franz"]
                .into_iter()
                .map(|id| crate::enrollment::UserEntry {
                    id: id.to_owned(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                })
                .collect(),
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");
        let tree = WikiTree::open(dir.path()).expect("reopen tree");
        let policy = IngestPolicy::default();
        let turn = "cosa cucino stasera per mia moglie?";

        assert!(
            people_mentioned_section(
                &pool,
                &tree,
                &SenderContext::user("franz"),
                turn,
                &[],
                &policy,
            )
            .await
            .is_none(),
            "the roster matches nothing in this turn — the first gate is shut"
        );

        let hits = vec![sample_recall_hit("01a03e05-65da-7b50-9af4-9e40272e1d21")];
        let out = people_mentioned_section(
            &pool,
            &tree,
            &SenderContext::user("franz"),
            turn,
            &hits,
            &policy,
        )
        .await
        .expect("the hit's subject opens the second gate");
        assert!(
            out.section.contains("Alice lives in Bologna."),
            "the card of whoever the search found rides the block: {}",
            out.section
        );
        assert_eq!(
            out.served,
            vec![("alice".to_owned(), PathBuf::from(IDENTITY_PAGE))],
        );
    }

    /// A group-owned hit names no person, and the speaker is served
    /// elsewhere: neither may spend one of the slot's seats.
    #[tokio::test]
    async fn neither_a_group_hit_nor_the_speakers_own_facts_open_the_second_gate() {
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: vec![crate::enrollment::UserEntry {
                id: "alice".to_owned(),
                aliases: Vec::new(),
                is_admin: false,
                locale: None,
                timezone: None,
            }],
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let mut group_hit = sample_recall_hit("01a03e05-65da-7b50-9af4-9e40272e1d21");
        group_hit.subject_id = Principal::Group("famiglia".to_owned());
        let own_hit = sample_recall_hit("01a03e07-c115-71e1-9f5d-0679ab67171c");

        assert!(
            people_mentioned_section(
                &pool,
                &tree,
                &SenderContext::user("alice"),
                "cosa mangio stasera?",
                &[group_hit, own_hit],
                &IngestPolicy::default(),
            )
            .await
            .is_none(),
            "a group is not a person, and the speaker is WHO IS SPEAKING's job"
        );
    }

    /// 69a: the slot serves the identity **page**, not one line of
    /// `_meta.summary`. The testata is dropped, the markers are gone, the
    /// authored `[[wikilink]]` is rendered plainly for a consumer that has
    /// nothing to navigate from — and the served page is reported back so
    /// the other slots can defer to it.
    #[tokio::test]
    async fn who_is_speaking_serves_the_identity_page_projected_and_link_free() {
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let card = who_is_speaking_section(
            &pool,
            &tree,
            &SenderContext::user("alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("card served");

        assert!(card.section.starts_with(HDR_WHO_IS_SPEAKING), "{card:?}");
        // The label line still names the sender's wire id — the prose never
        // says what they are called on the wire.
        assert!(
            card.section.contains("- alice — Profile of Alice"),
            "{}",
            card.section
        );
        // The page's own facts arrive, which is the whole point: a `bio`
        // query would have caught the first, the allergy is the standing
        // constraint that characterises her.
        assert!(card.section.contains("Alice lives in Bologna."));
        assert!(card.section.contains("Alice is allergic to walnuts."));
        assert!(card.section.contains("She binds books by hand."));
        // Testata, markers and link syntax all stay out of the block.
        assert!(!card.section.contains("keywords"), "{}", card.section);
        assert!(!card.section.contains("{{f="), "{}", card.section);
        assert!(!card.section.contains("[["), "{}", card.section);
        assert!(
            card.section.contains("written up on hobbies."),
            "the link becomes the name it points at: {}",
            card.section
        );
        assert_eq!(
            card.page_path.as_deref(),
            Some("wikis/alice/@profile.md"),
            "the served page is reported so the flat and navigated slots can drop it"
        );
        drop(dir);
    }

    /// A page carrying no readable fact is scaffolding, not a card — the
    /// slot yields the pre-69a one-line summary rather than serving a
    /// heading on every turn forever, and reports no page (it duplicates
    /// nothing).
    #[tokio::test]
    async fn who_is_speaking_falls_back_to_the_summary_when_the_page_carries_no_fact() {
        let (dir, _, pool) = setup_workdir().await;
        set_wiki_summary(
            &dir.path().join("wikis"),
            "alice",
            "Profile of Alice, a bookbinder in Bologna.",
        );
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let card = who_is_speaking_section(
            &pool,
            &tree,
            &SenderContext::user("alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("summary line served");
        assert_eq!(
            card.section,
            format!("{HDR_WHO_IS_SPEAKING}\n- alice — Profile of Alice, a bookbinder in Bologna.")
        );
        assert_eq!(card.page_path, None);
        drop(dir);
    }

    /// Neither a card nor a summary → no section at all (the empty-section
    /// contract): a sender whose wiki says nothing must not cost the block
    /// a header.
    #[tokio::test]
    async fn who_is_speaking_is_absent_when_there_is_neither_card_nor_summary() {
        let (dir, tree, pool) = setup_workdir().await;
        assert!(
            who_is_speaking_section(
                &pool,
                &tree,
                &SenderContext::user("alice"),
                &IngestPolicy::default(),
            )
            .await
            .is_none()
        );
        drop(dir);
    }

    /// The projection invariant, which must already hold before 69d serves a
    /// *subject's* card to a different reader: a region on the page that the
    /// reader may not see is redacted, never injected. Today the sender owns
    /// their own card and this is nearly a no-op — that is exactly why it is
    /// pinned by a test rather than left to be noticed later.
    #[tokio::test]
    async fn who_is_speaking_projects_the_card_per_sender() {
        // A third fact filed on alice's page but readable only by carol —
        // e.g. something carol told the assistant about alice privately.
        const CAROL_ONLY: &str = "018f1234-5678-7abc-9def-00000000c003";

        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let wikis = dir.path().join("wikis");
        // On the identity card — the page `identity_card` actually opens.
        // Writing this region anywhere else puts it on a file the code never
        // reads, and the negative assertion below then passes without the ACL
        // projection running at all: it would stay green with the projection
        // replaced by a no-op.
        let page = wikis.join("alice").join(crate::wiki::PROFILE_FILENAME);
        let existing = std::fs::read_to_string(&page).unwrap();
        std::fs::write(
            &page,
            format!("{existing}\n{{{{f={CAROL_ONLY}}}}}Alice is planning a surprise.{{{{/}}}}\n"),
        )
        .unwrap();
        insert_page_fact(
            &pool,
            CAROL_ONLY,
            "alice",
            "wikis/alice/@profile.md",
            "Alice is planning a surprise.",
            Principal::User("carol".into()),
        )
        .await;
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let card = who_is_speaking_section(
            &pool,
            &tree,
            &SenderContext::user("alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("card served");
        assert!(card.section.contains("Alice lives in Bologna."));
        assert!(
            !card.section.contains("Alice is planning a surprise."),
            "a region this reader may not see must never reach the card: {}",
            card.section
        );
        drop(dir);
    }

    /// The budget is a failsafe, not a curation knob — and `0` turns the
    /// card off without silencing the slot.
    #[tokio::test]
    async fn who_is_speaking_budget_zero_serves_the_summary_line_alone() {
        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let card = who_is_speaking_section(
            &pool,
            &tree,
            &SenderContext::user("alice"),
            &IngestPolicy {
                max_sender_identity_chars: 0,
                ..IngestPolicy::default()
            },
        )
        .await
        .expect("summary line served");
        assert!(
            !card.section.contains("Alice is allergic to walnuts."),
            "{}",
            card.section
        );
        assert_eq!(card.page_path, None);
        drop(dir);
    }

    /// End to end: the card's prose reaches the turn exactly **once**. The
    /// navigator may still open the card (stopping that is 69b), but its
    /// fragment is dropped, and the flat slot drops the facts the card
    /// already carries.
    #[tokio::test]
    async fn ingest_never_injects_the_identity_page_twice() {
        // A second page of alice's wiki, with a fact of its own so the flat
        // recall seeds it as a door — otherwise the fan holds nothing but the
        // identity anchor and there is no second choice to test against.
        const ALICE_FACT_C: &str = "018f1234-5678-7abc-9def-00000000c003";

        let (dir, _, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        std::fs::write(
            dir.path().join("wikis").join("alice").join("hobbies.md"),
            format!(
                "---\ntitle: Hobbies\n---\n\n\
                 {{{{f={ALICE_FACT_C}}}}}Alice restores marbled endpapers.{{{{/}}}}\n"
            ),
        )
        .unwrap();
        insert_page_fact(
            &pool,
            ALICE_FACT_C,
            "alice",
            "wikis/alice/hobbies.md",
            "Alice restores marbled endpapers.",
            Principal::User("alice".into()),
        )
        .await;
        let tree = WikiTree::open(dir.path()).expect("reopen tree");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"recall\"}");
        // The navigator asks for both alice's wiki root — which resolves to
        // the identity page `WHO IS SPEAKING` already served — and a real
        // second page of the same wiki. Only the second may be opened:
        // the served page is not a destination, and refusing
        // it must not cost the walk its other choice.
        let nav = FakeLlmBackend::new(
            "fake-nav",
            "{\"open\":[{\"wiki_id\":\"alice\"},\
             {\"wiki_id\":\"alice\",\"page\":\"hobbies.md\"}],\"done\":true}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            Some(&nav),
            req("what do you know about me?", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        let snippet = resp.context_snippet.expect("recall block present");

        assert_eq!(
            snippet.matches("Alice lives in Bologna.").count(),
            1,
            "the card's prose must reach the turn exactly once: {snippet}"
        );
        assert!(
            snippet.contains(HDR_WHO_IS_SPEAKING),
            "and it must be the deterministic slot that carries it: {snippet}"
        );
        // The walk still ran and still yielded — the exclusion is one page,
        // not a mute funnel.
        assert!(
            snippet.contains(HDR_NAVIGATED_PAGES)
                && snippet.contains("Alice restores marbled endpapers."),
            "the sibling page must still be navigated and injected: {snippet}"
        );
        let navigated = snippet.split(HDR_NAVIGATED_PAGES).nth(1).unwrap_or("");
        assert!(
            !navigated.contains("alice/preferenze.md"),
            "the identity page is not a navigation destination for its own subject: {navigated}"
        );
        drop(dir);
    }

    #[test]
    fn plain_wikilinks_renders_the_name_a_link_points_at() {
        // Bare wiki hop → the wiki id; page hop → the page's own name, which
        // is the noun the sentence is about.
        assert_eq!(
            plain_wikilinks("details on [[lavorofranz]] and [[franz/lnprint]]"),
            "details on lavorofranz and lnprint"
        );
        // A `|display` alias is what the author wanted a reader to see.
        assert_eq!(
            plain_wikilinks("see [[franz/lnprint|the print shop]]"),
            "see the print shop"
        );
        // A nested slug keeps only its leaf.
        assert_eq!(
            plain_wikilinks("[[franz/projects/telaiojs]] ships"),
            "telaiojs ships"
        );
        // Unclosed syntax is left verbatim — never a truncated sentence.
        assert_eq!(plain_wikilinks("a [[dangling link"), "a [[dangling link");
        assert_eq!(plain_wikilinks("no links here"), "no links here");
    }

    #[test]
    fn fit_paragraphs_keeps_whole_paragraphs_and_flags_the_cut() {
        let text = "first para\n\nsecond para\n\nthird para";
        assert_eq!(fit_paragraphs(text, 1_000), (text.to_owned(), false));
        // "first para" + "\n\n" + "second para" = 23; the third
        // does not fit and the cut is reported, never silent.
        assert_eq!(
            fit_paragraphs(text, 25),
            ("first para\n\nsecond para".to_owned(), true)
        );
        // Never mid-sentence: a paragraph that does not fit is dropped whole.
        assert_eq!(fit_paragraphs(text, 15), ("first para".to_owned(), true));
        // The one exception, mirroring `fit_bullets`: a first paragraph
        // longer than the whole budget is truncated rather than leaving a
        // page with content rendered empty.
        let (kept, cut) = fit_paragraphs("a single very long paragraph", 10);
        assert!(cut);
        assert_eq!(kept, "a single …");
    }

    #[test]
    fn trust_tag_renders_validity_window_without_judging_it() {
        let mut hit = sample_recall_hit("018f1234-5678-7abc-9def-0123456789ab");
        hit.created_at = "2026-05-18T09:30:00Z".into();
        hit.valid_to = Some("2026-06-01T00:00:00Z".into());
        let snippet = format_snippet(&[hit], &[], &[], 0.0).expect("one hit renders");
        // Dates only, raw window — no expired/stale verdict in Rust: the
        // consumer model judges staleness against its own clock.
        assert!(
            snippet.ends_with("[noted 2026-05-18 · valid to 2026-06-01]"),
            "{snippet}"
        );
    }

    // ---------- turn-level relevance floor (card 61, §37-39) ----------

    /// Card 61 §37: on `il volume` the turn's best promoted hit — a
    /// pushchair offer — scored `0.4306`, below the default floor; the
    /// weaker noise under it (a savings passbook, rank 5 near `0.41`)
    /// scored even lower. Below the floor NONE of the promoted hits
    /// render — not a trim of the weak ones, the section is not opened.
    #[test]
    fn format_snippet_renders_no_promoted_hits_when_the_turns_best_score_is_below_the_floor() {
        let mut pushchair = sample_recall_hit("018f1234-5678-7abc-9def-0000000000f1");
        pushchair.text = "a pushchair offer".into();
        pushchair.score = 0.4306;
        let mut passbook = sample_recall_hit("018f1234-5678-7abc-9def-0000000000f2");
        passbook.text = "a savings passbook".into();
        passbook.score = 0.41;

        assert_eq!(
            format_snippet(&[pushchair, passbook], &[], &[], TEST_FLOOR),
            None,
            "the turn's best promoted hit is below the floor, so it has nothing to say"
        );
    }

    /// Same shut gate as above, but the fresh (un-promoted) slot is a
    /// different signal (§39: things said a few turns ago, not durable
    /// memory) and must keep rendering even when every promoted hit is
    /// dropped.
    #[test]
    fn format_snippet_still_renders_fresh_captures_when_the_floor_shuts_the_promoted_section() {
        let mut pushchair = sample_recall_hit("018f1234-5678-7abc-9def-0000000000f3");
        pushchair.text = "a pushchair offer".into();
        pushchair.score = 0.4306;
        let mut fresh = sample_recall_hit("018f1234-5678-7abc-9def-0000000000f4");
        fresh.text = "alice just joined a gym".into();
        fresh.score = 0.9; // strong, but irrelevant — fresh hits never feed the gate
        fresh.fresh = true;

        let snippet = format_snippet(&[pushchair.clone(), fresh.clone()], &[], &[], TEST_FLOOR)
            .expect("the fresh capture still renders");
        assert!(!snippet.contains(&pushchair.text), "{snippet}");
        assert!(
            snippet.contains("Recent (not yet consolidated):"),
            "{snippet}"
        );
        assert!(snippet.contains(&fresh.text), "{snippet}");
    }

    /// Card 61 §37, the guarantee the gate exists to preserve: on the
    /// measured turn the two facts that ARE the answer scored `0.4813` /
    /// `0.4811`, ranked below the turn's best hit (`0.5474`, an unrelated
    /// candidate). The gate reads only the turn's best score, so once it
    /// clears the floor every hit renders — including one that, on its
    /// own, sits under the floor (a per-hit trim would have discarded it
    /// alongside the noise the other measured turn needed gone).
    #[test]
    fn format_snippet_renders_every_hit_once_the_turns_best_clears_the_floor() {
        let mut best = sample_recall_hit("018f1234-5678-7abc-9def-0000000000e1");
        best.text = "the turn's top-ranked hit, unrelated to the question".into();
        best.score = 0.5474;
        let mut answer = sample_recall_hit("018f1234-5678-7abc-9def-0000000000e2");
        answer.text = "the fact that actually answers the question".into();
        answer.score = 0.4813;
        let mut weak = sample_recall_hit("018f1234-5678-7abc-9def-0000000000e3");
        weak.text = "a much weaker hit, individually under the floor".into();
        weak.score = 0.20;

        let hits = vec![best.clone(), answer.clone(), weak.clone()];
        let snippet = format_snippet(&hits, &[], &[], TEST_FLOOR)
            .expect("the turn's best hit clears the floor");
        assert!(snippet.contains(&best.text), "{snippet}");
        assert!(snippet.contains(&answer.text), "{snippet}");
        assert!(
            snippet.contains(&weak.text),
            "a hit scored under the floor still renders once the turn's best clears it: {snippet}"
        );
    }

    /// The floor these unit tests pin the mechanism at. Deliberately a local
    /// constant and not [`recall::DEFAULT_RELEVANCE_FLOOR`]: the shipped
    /// default is a policy choice (today `0.0` — off, pending evidence), while
    /// what these tests guarantee is how the gate behaves *when set*.
    const TEST_FLOOR: f32 = 0.45;

    /// `relevance_floor <= 0.0` is the off switch (same idiom as
    /// [`recall::admitted_smart_wikis`]): every promoted hit renders
    /// exactly as it did before the floor existed, however weak.
    #[test]
    fn format_snippet_a_floor_of_zero_renders_every_hit_as_before_the_gate_existed() {
        let mut weak = sample_recall_hit("018f1234-5678-7abc-9def-0000000000f5");
        weak.text = "an extremely weak hit".into();
        weak.score = 0.01;

        let snippet = format_snippet(&[weak.clone()], &[], &[], 0.0).expect("renders");
        assert!(snippet.contains(&weak.text), "{snippet}");
    }

    // ---------- end-to-end orchestrator ----------

    #[tokio::test]
    async fn ingest_rejects_empty_text() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let err = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("   ", "alice"),
            &policy,
        )
        .await
        .expect_err("must reject");
        assert!(matches!(err, IngestError::EmptyText));
        drop(dir);
    }

    /// When the consumer round-trips `disambig_choice`, the
    /// orchestrator commits — `needs_disambig` is always false on the
    /// second turn, even if the LLM tries to ask again.
    #[tokio::test]
    async fn ingest_with_disambig_choice_never_re_asks() {
        let (dir, tree, pool) = setup_workdir().await;
        // The LLM ignores the commit instruction and tries to ask
        // again; the orchestrator must override.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"skip\",\"needs_disambig\":true,\
             \"disambig_candidates\":[{\"candidate_id\":\"a\",\"description\":\"A\"}]}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                text: "use option A".to_owned(),
                author: MessageRole::User,
                sender_id: "alice".to_owned(),
                consumer_id: None,
                recent_messages: Vec::new(),
                context_hint: ContextHint::Conversation,
                disambig_choice: Some("a".to_owned()),
                metadata: IngestMetadata::default(),
                attachments: Vec::new(),
            },
            &policy,
        )
        .await
        .expect("ingest");
        assert!(!resp.needs_disambig, "follow-up turn never re-asks");
        assert!(resp.disambig_candidates.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_skip_intent_returns_seed_no_write() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"skip\",\"suggested_seed\":\"You're welcome.\"}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("thanks!", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert_eq!(resp.suggested_seed.as_deref(), Some("You're welcome."));
        assert!(resp.capture_id.is_none());
        assert!(resp.llm_used);
        drop(dir);
    }

    /// Cross-consumer recent window: a turn from one
    /// surface is served to the user's other surfaces — tagged with its
    /// origin and relative age. A requester that brings its own local
    /// window never gets its surface echoed back; one that brings none (a
    /// reborn/blank session) resumes its own thread — minus the message it
    /// is speaking right now.
    #[tokio::test]
    async fn ingest_recent_window_crosses_surfaces_and_resumes_blank_sessions() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\",\"suggested_seed\":\"Ok.\"}");
        let policy = IngestPolicy::default();
        let salotto_window = vec![RecentMessage {
            role: MessageRole::User,
            text: "il pollo è venuto benissimo".to_owned(),
            timestamp: None,
        }];
        // Turn A — first exchange ever: nothing to serve, not even itself
        // (the fetch runs before the record).
        let mut turn_a = req_consumer("il pollo è venuto benissimo", "alice", "botdeploy");
        turn_a.metadata.channel = Some("salotto".to_owned());
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, turn_a, &policy)
            .await
            .expect("turn A");
        assert!(
            resp.recent_window.is_none(),
            "first turn served its own current message: {:?}",
            resp.recent_window
        );
        // Turn A2 — same surface, NO local window (idle-expiry reborn
        // session, hermes#43008): it resumes its own thread (43j).
        let mut reborn_a = req_consumer("e la torta com'era?", "alice", "botdeploy");
        reborn_a.metadata.channel = Some("salotto".to_owned());
        let resp =
            wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, reborn_a, &policy)
                .await
                .expect("turn A2");
        let window = resp
            .recent_window
            .expect("blank session resumes its own thread");
        assert!(window.contains("il pollo è venuto benissimo"), "{window}");
        assert!(window.contains("via botdeploy/salotto"), "{window}");
        // Turn A3 — same surface WITH its local window: its own words must
        // not come back (the self-echo exclusion).
        let mut mid_a = req_consumer("aggiungo il rosmarino", "alice", "botdeploy");
        mid_a.metadata.channel = Some("salotto".to_owned());
        mid_a.recent_messages = salotto_window.clone();
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, mid_a, &policy)
            .await
            .expect("turn A3");
        assert!(
            resp.recent_window.is_none(),
            "self-echo served: {:?}",
            resp.recent_window
        );
        // Turn B — a different surface of the same consumer, window in
        // hand: it sees the salotto thread, never its own turn.
        let mut turn_b = req_consumer("mettici meno sale la prossima volta", "alice", "botdeploy");
        turn_b.metadata.channel = Some("telegram".to_owned());
        turn_b.recent_messages = salotto_window;
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, turn_b, &policy)
            .await
            .expect("turn B");
        let window = resp
            .recent_window
            .expect("window served to the other surface");
        assert!(window.starts_with(HDR_RECENT_EXCHANGES), "{window}");
        assert!(window.contains("il pollo è venuto benissimo"), "{window}");
        assert!(window.contains("via botdeploy/salotto"), "{window}");
        assert!(window.contains("just now"), "{window}");
        assert!(
            !window.contains("meno sale"),
            "own turn echoed back: {window}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_capture_intent_writes_fact_index_row() {
        let (dir, tree, pool) = setup_workdir().await;
        // `requested_container: true` takes the live direct-write path so
        // the fact lands in `fact_index` immediately. Every
        // non-smart wiki is a standard wiki, so a plain capture would
        // buffer for the dream instead (covered by
        // `ingest_standard_wiki_buffers_instead_of_writing_md`).
        let json = "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:alice\",\"body\":\"alice prefers coffee black\",\"fact_type\":\"preference\",\"topics\":[\"coffee\"],\"requested_container\":true,\"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("I like my coffee black", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(resp.capture_id.is_some(), "must surface capture_id");
        assert_eq!(resp.suggested_seed.as_deref(), Some("Noted."));
        let cap_id = resp.capture_id.unwrap();
        let row = fact_index::find_by_id(&pool, &cap_id)
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(row.wiki_id, "alice");
        assert_eq!(row.text, "alice prefers coffee black");
        assert_eq!(row.fact_type.as_deref(), Some("preference"));
        assert_eq!(row.topics, vec!["coffee".to_owned()]);
        drop(dir);
    }

    /// Engine floor of the 2026-06-30 subject-must-be-a-principal ruling (the
    /// dangling-principal incident): a classifier-emitted subject that
    /// enrollment does not back is re-filed onto the sender — the ruling's own
    /// fallback — instead of minting a principal no reader matches.
    #[tokio::test]
    async fn ingest_unenrolled_subject_reowns_to_sender() {
        let (dir, tree, pool) = setup_workdir().await;
        // `aragorn` is never enrolled: the classifier coined him.
        let json = "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:aragorn\",\"body\":\"aragorn arrives on Friday\",\"fact_type\":\"plan\",\"topics\":[\"visit\"],\"requested_container\":true,\"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("aragorn arrives on Friday", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let cap_id = resp.capture_id.expect("captured");
        let row = fact_index::find_by_id(&pool, &cap_id)
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(
            row.subject_id,
            Principal::User("alice".to_owned()),
            "an unenrolled subject must fall back to the sender"
        );
        drop(dir);
    }

    /// Counterpart of [`ingest_unenrolled_subject_reowns_to_sender`]: an
    /// enrolled third-party person is a legitimate subject (reciprocal
    /// relationship facts, a fact filed for another family member) and must
    /// pass the guard untouched.
    #[tokio::test]
    async fn ingest_enrolled_third_party_subject_is_kept() {
        let (dir, tree, pool) = setup_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('morgana', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let json = "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:morgana\",\"body\":\"morgana arrives on Friday\",\"fact_type\":\"plan\",\"topics\":[\"visit\"],\"requested_container\":true,\"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("morgana arrives on Friday", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let cap_id = resp.capture_id.expect("captured");
        let row = fact_index::find_by_id(&pool, &cap_id)
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(
            row.subject_id,
            Principal::User("morgana".to_owned()),
            "an enrolled third-party subject must be kept"
        );
        drop(dir);
    }

    /// The assistant-turn face of the same contract (prompt v2.43: the
    /// subject axis is the subject, not the interlocutor): advice the agent
    /// synthesised FOR an enrolled third user — the necessity test — files
    /// owned by that user, exactly as on a user turn.
    #[tokio::test]
    async fn ingest_assistant_turn_keeps_enrolled_beneficiary_subject() {
        let (dir, tree, pool) = setup_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('morgana', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let json = "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:morgana\",\"body\":\"the agent walked alice through what morgana must check at the viewing\",\"fact_type\":\"plan\",\"topics\":[\"viewing\"],\"requested_container\":true,\"suggested_seed\":\"ok\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let mut request = req("checklist for the used-car viewing", "alice");
        request.author = MessageRole::Assistant;
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        let cap_id = resp.capture_id.expect("captured");
        let row = fact_index::find_by_id(&pool, &cap_id)
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(
            row.subject_id,
            Principal::User("morgana".to_owned()),
            "assistant-turn advice for an enrolled beneficiary is owned by the beneficiary"
        );
        // The delivery half: the beneficiary is told, and the notice says
        // the fact came out of the agent's own reply.
        let rows: Vec<(Option<String>,)> =
            sqlx::query_as("SELECT payload FROM wiki_events WHERE kind = 'fact_minted_for_you'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "one beneficiary, one notice");
        let payload: serde_json::Value =
            serde_json::from_str(rows[0].0.as_deref().unwrap()).unwrap();
        assert_eq!(payload["recipient_id"], "user:morgana");
        assert_eq!(payload["from_user_id"], "alice");
        assert_eq!(payload["origin"], "assistant_turn");
        drop(dir);
    }

    /// Reverse channel, user-turn face: facts filed for an enrolled third
    /// user emit ONE `fact_minted_for_you` event per beneficiary and turn
    /// — batched (two facts, one notice) and content-bearing (the bridge's
    /// agent delivers the body without a recall round-trip). The sender's
    /// own fact in the same turn emits nothing.
    #[tokio::test]
    async fn ingest_beneficiary_facts_batch_into_one_minted_notice() {
        let (dir, tree, pool) = setup_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('morgana', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let json = "{\"intent\":\"capture\",\"extractions\":[\
            {\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:morgana\",\"body\":\"morgana handles the viewing on Friday\",\
             \"fact_type\":\"plan\",\"topics\":[\"viewing\"],\"requested_container\":true},\
            {\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:morgana\",\"body\":\"morgana must bring the service booklet\",\
             \"fact_type\":\"plan\",\"topics\":[\"viewing\"],\"requested_container\":true},\
            {\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:alice\",\"body\":\"alice sold her bike\",\
             \"fact_type\":\"episode\",\"topics\":[\"bike\"],\"requested_container\":true}],\
            \"suggested_seed\":\"ok\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("viewing logistics", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT wiki_id, payload FROM wiki_events WHERE kind = 'fact_minted_for_you'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "two facts for one beneficiary batch into one notice"
        );
        assert_eq!(rows[0].0.as_deref(), Some("alice"));
        let payload: serde_json::Value =
            serde_json::from_str(rows[0].1.as_deref().unwrap()).unwrap();
        assert_eq!(payload["recipient_id"], "user:morgana");
        assert_eq!(payload["from_user_id"], "alice");
        assert_eq!(payload["origin"], "user_turn");
        let facts = payload["facts"].as_array().unwrap();
        assert_eq!(facts.len(), 2, "the sender's own fact rides no notice");
        assert_eq!(
            facts[0]["body"], "morgana handles the viewing on Friday",
            "the notice carries the content itself"
        );
        drop(dir);
    }

    /// An agent principal never gets a minted-for-you ping: it has no
    /// inbox to drain — a fact cross-filed under an agent subject is that
    /// agent's own diary, not a delivery.
    #[tokio::test]
    async fn ingest_agent_owned_fact_emits_no_minted_notice() {
        let (dir, tree, pool) = setup_workdir().await;
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, is_admin, is_agent) VALUES ('bot', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let json = "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:bot\",\"body\":\"the bot tracks the pantry stock\",\"fact_type\":\"plan\",\"topics\":[\"pantry\"],\"requested_container\":true,\"suggested_seed\":\"ok\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("pantry tracking", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_events WHERE kind = 'fact_minted_for_you'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "agent principals have no inbox");
        drop(dir);
    }

    /// Group 17 (17a) — a smart consumer is a SUPERSET of a standard one: its
    /// user↔agent conversation still runs the standard personal-memory
    /// pipeline. The project (smart) wiki it authors via `wiki_admin_*` is
    /// never an ingest destination (the `!w.smart` routing filter), so a
    /// conversational capture lands in the user's standard personal wiki. No
    /// `consumer_class` gate exists on this path — `IngestRequest` carries no
    /// such field; the only thing that historically kept the conversation out
    /// of personal memory was the skill instruction (revised in
    /// `smart-consumer`). This proves the engine was already a superset.
    #[tokio::test]
    async fn smart_consumer_conversation_ingests_into_standard_not_smart_wiki() {
        let (dir, tree, pool) = setup_workdir().await;
        // alice also owns a smart project wiki (the kind a smart consumer
        // authors via wiki_admin_push), alongside her standard wiki-user wiki.
        let smart_meta = "---\nwiki_id: proj\nwiki_type: wiki-companion\nparent_wiki_id: null\n\
                          slug: proj\ntitle: Proj\nacl_default: 'user:alice'\nsmart: true\n---\n";
        let (cmeta, _) =
            WikiMeta::parse(Path::new("_meta.md"), smart_meta).expect("smart-wiki meta");
        crate::wiki::write_wiki_dir(&tree, &cmeta, false).expect("create proj");

        // A conversational, durable personal fact. `requested_container: true`
        // files it live into `fact_index`, so the destination wiki is easy to
        // assert.
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
            \"subject_id\":\"user:alice\",\"body\":\"alice prefers tabs over spaces\",\
            \"fact_type\":\"preference\",\"topics\":[\"style\"],\"requested_container\":true}],\
            \"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("I prefer tabs over spaces", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);
        let row = fact_index::find_by_id(&pool, &resp.capture_id.expect("capture_id"))
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(
            row.wiki_id, "alice",
            "the conversation lands in the standard personal wiki, never the smart project wiki"
        );
        drop(dir);
    }

    /// Group 17 (17e) — length is NEVER the fact-extraction gate. A durable
    /// fact buried in a long body must still be captured, and the FULL message
    /// must reach the classifier (`build_prompt` appends `current_message`
    /// untruncated). Storing the verbatim long body is a separate decision
    /// (document-import on explicit request); the classifier still scans for
    /// facts. This guards against a future truncation regression on the
    /// classifier input.
    #[tokio::test]
    async fn ingest_long_message_still_scans_for_buried_fact() {
        let (dir, tree, pool) = setup_workdir().await;
        let buried = "Please remember: my dentist appointment is Thursday at 17:00.";
        let filler = "fix the build, rerun the tests, bump the lints, ".repeat(60);
        let long = format!("{filler}\n\n{buried}\n\n{filler}");
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
            \"subject_id\":\"user:alice\",\"body\":\"alice has a dentist appointment Thursday at 17:00\",\
            \"fact_type\":\"plan\",\"topics\":[\"appointments\"],\"requested_container\":true}],\
            \"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(&long, "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(
            resp.capture_id.is_some(),
            "the buried fact must be captured"
        );
        let prompt = llm.last_prompt().expect("classifier prompt recorded");
        assert!(
            prompt.contains(buried),
            "the buried sentence must reach the classifier untruncated — length never gates extraction"
        );
        drop(dir);
    }

    /// Group 17 (17f) — a conversation-borne dated commitment becomes a fact
    /// with a validity window and surfaces in the due-soon recall slot. It is
    /// the same mechanism, reached from a conversational turn: the
    /// classifier resolves the date and emits `valid_to`, capture persists it
    /// (here on the live `requested_container` path), and `recall_due_soon`
    /// pulls it within the horizon.
    #[tokio::test]
    async fn ingest_dated_commitment_lands_in_due_soon_slot() {
        let (dir, tree, pool) = setup_workdir().await;
        let valid_to = "2026-06-12T17:00:00Z";
        let json = format!(
            "{{\"intent\":\"capture\",\"extractions\":[{{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
            \"subject_id\":\"user:alice\",\"body\":\"alice has a dentist appointment Thursday at 17:00\",\
            \"fact_type\":\"plan\",\"topics\":[\"appointments\"],\"requested_container\":true,\
            \"valid_from\":\"2026-06-10T08:00:00Z\",\"valid_to\":\"{valid_to}\"}}],\
            \"suggested_seed\":\"Noted.\"}}"
        );
        let llm = FakeLlmBackend::new("fake", &json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("giovedì alle 17 dal dentista", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let row = fact_index::find_by_id(&pool, &resp.capture_id.expect("capture_id"))
            .await
            .expect("find")
            .expect("inserted row");
        assert_eq!(
            row.valid_to.as_deref(),
            Some(valid_to),
            "the conversation-borne dated commitment carries its validity window"
        );
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-10T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let hits = recall::recall_due_soon(
            &pool,
            &SenderContext::user("alice"),
            now,
            chrono::Duration::days(7),
            10,
        )
        .await
        .expect("due soon");
        assert!(
            hits.iter().any(|h| h.text.contains("dentist")),
            "the dated commitment surfaces in the due-soon slot"
        );
        drop(dir);
    }

    /// An extraction the classifier marks
    /// as an engine-rule (a standing governance directive) is appended to the
    /// sender's `@rules.md` as prose and is NEVER filed in `fact_index` or the
    /// capture buffer. `setup_workdir` does not seed a `@rules.md`, so this also
    /// exercises the missing-file path (the helper seeds from the default body).
    #[tokio::test]
    async fn ingest_engine_rule_appends_to_rules_md_not_fact_index() {
        let (dir, tree, pool) = setup_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"engine_rule\":true,\"fact_type\":\"rule\",\
            \"body\":\"Health information is always private; never share it with any group.\"}],\
            \"suggested_seed\":\"Got it.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(
                "la mia salute è sempre privata, non condividerla mai",
                "alice",
            ),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        // A rule is not a fact: nothing filed, nothing buffered, no anchor id.
        assert!(resp.capture_id.is_none(), "engine-rule files no fact");
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            0,
            "engine-rule must not write a fact_index row"
        );
        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "engine-rule must not buffer a capture"
        );

        // It landed in alice's rules.md as prose, read straight back next turn.
        let rules = std::fs::read_to_string(
            tree.wikis_dir()
                .join("alice")
                .join(crate::wiki::RULES_FILENAME),
        )
        .expect("@rules.md written");
        assert!(
            rules
                .contains("- Health information is always private; never share it with any group."),
            "rule appended as a bullet; body was:\n{rules}"
        );
        drop(dir);
    }

    /// A behaviour-rule (how the agent should converse) is filed in the
    /// CALLING AGENT's own wiki, OWNED by the served user — never in the user's
    /// fact memory. The consumer id threads from the request through
    /// `consumers.system_user_id` to the target wiki, and the rule surfaces in
    /// the same turn's recall block returned to the consumer.
    #[tokio::test]
    async fn ingest_behaviour_rule_lands_in_agent_wiki_owned_by_the_user() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"behaviour_rule\":true,\"behaviour_scope\":\"per-user\",\
            \"body\":\"Rispondi sempre in modo conciso.\"}],\
            \"suggested_seed\":\"Ok.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("rispondimi sempre conciso", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        // The user's own wiki stays clean — a behaviour rule is not a fact
        // about the user.
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            0,
            "behaviour rule must not land in the user's wiki"
        );
        // It lands in the agent's wiki, attributed to the user who dictated it.
        let rows = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one behaviour rule in the agent wiki"
        );
        assert_eq!(rows[0].text, "Rispondi sempre in modo conciso.");
        assert_eq!(
            rows[0].subject_id,
            Principal::User("alice".into()),
            "the rule is OWNED by the user who dictated it (subject-scoped dedup keeps users' rules distinct)"
        );
        assert_eq!(
            rows[0].sender_id,
            Some(Principal::User("alice".into())),
            "subject is the user, so there is no SEPARATE sender — sender is materialized to the subject, never the agent"
        );
        assert_eq!(
            resp.capture_id.as_ref(),
            Some(&rows[0].fact_id),
            "the filed behaviour rule is the turn's anchor id"
        );
        assert!(
            resp.rules
                .as_deref()
                .unwrap_or_default()
                .contains("Rispondi sempre in modo conciso."),
            "the behaviour rule surfaces in the dedicated `rules` field, \
             not in the recalled-memory snippet"
        );
        assert!(
            resp.context_snippet.is_none(),
            "a pure behaviour-rule turn surfaces no recalled memory in context_snippet"
        );
        // The rule lands on the agent wiki's `@rules.md` page.
        assert!(
            rows[0].source_path.ends_with("@rules.md")
                && !rows[0].source_path.ends_with("behaviour_rules.md"),
            "behaviour rule stored on rules.md, was: {}",
            rows[0].source_path
        );
        drop(dir);
    }

    /// Behaviour-rule recall is scoped to the served user by per-fact `sender`
    /// attribution: two users' directives coexist in the one agent wiki without
    /// bleeding into each other, and a caller with no consumer id (a smart
    /// consumer) draws only the user-global source — nothing per-user leaks.
    #[tokio::test]
    async fn behaviour_rule_recall_is_scoped_to_the_sender() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let policy = IngestPolicy::default();
        let llm_a = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"per-user\",\
             \"body\":\"Rispondi sempre in modo conciso.\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm_a,
            None,
            req_consumer("rispondimi conciso", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest alice");
        let llm_b = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"per-user\",\
             \"body\":\"Dai del lei all'utente.\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm_b,
            None,
            req_consumer("dammi del lei", "bilbo", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest bilbo");

        let alice = recall_behaviour_rules(&pool, &req_consumer("?", "alice", "botdeploy")).await;
        assert_eq!(
            alice.iter().map(|(_, b, _)| b.as_str()).collect::<Vec<_>>(),
            vec!["Rispondi sempre in modo conciso."],
            "alice recalls only her own behaviour rule"
        );
        let bilbo = recall_behaviour_rules(&pool, &req_consumer("?", "bilbo", "botdeploy")).await;
        assert_eq!(
            bilbo.iter().map(|(_, b, _)| b.as_str()).collect::<Vec<_>>(),
            vec!["Dai del lei all'utente."],
            "bilbo recalls only his own behaviour rule"
        );
        // A caller with no consumer binding (a smart consumer) draws only the
        // user-global source — and alice has no everywhere-rules in her own
        // identity wiki, so the channel is empty (her per-user rule lives in
        // the AGENT's wiki and binds that agent only).
        assert!(
            recall_behaviour_rules(&pool, &req("?", "alice"))
                .await
                .is_empty(),
            "per-user rules never leak to a consumer without the agent wiki binding"
        );
        drop(dir);
    }

    /// A behaviour rule the user revises (the classifier supersedes the rule it
    /// was shown) replaces the old one in place — the recall channel then
    /// surfaces only the revision, not both.
    #[tokio::test]
    async fn behaviour_rule_supersede_revises_in_place() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let policy = IngestPolicy::default();
        let llm1 = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"per-user\",\
             \"body\":\"Rispondi sempre in modo conciso.\"}]}",
        );
        let r1 = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm1,
            None,
            req_consumer("rispondimi conciso", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest 1");
        let f1 = r1.capture_id.expect("first rule filed");

        // alice revises it; the model supersedes the rule it was shown.
        let json2 = format!(
            "{{\"intent\":\"capture\",\"extractions\":[{{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"per-user\",\
             \"body\":\"Rispondi pure in modo prolisso.\",\"supersede_target\":\"{}\"}}]}}",
            f1.as_str()
        );
        let llm2 = FakeLlmBackend::new("fake", &json2);
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm2,
            None,
            req_consumer("anzi prolisso", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest 2");

        let rules = recall_behaviour_rules(&pool, &req_consumer("?", "alice", "botdeploy")).await;
        assert_eq!(
            rules.iter().map(|(_, b, _)| b.as_str()).collect::<Vec<_>>(),
            vec!["Rispondi pure in modo prolisso."],
            "supersede replaces the old directive in place"
        );
        drop(dir);
    }

    /// An AGENT-WIDE behaviour-rule (impersonal — applies to everyone) set by
    /// the ADMIN is filed owned by the AGENT (not the admin), so it is recalled
    /// for EVERY user, not just the one who set it.
    #[tokio::test]
    async fn agent_wide_behaviour_rule_from_admin_applies_to_everyone() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        // alice is the operator/admin (samvisebot is is_admin=0, so the partial
        // unique index allows one more admin row).
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('alice', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"agent-wide\",\
             \"body\":\"Per i task pesanti delega a Claude Code.\"}]}",
        );
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("delega a claude", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest admin");

        let rows = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one agent-wide rule in the agent wiki"
        );
        assert_eq!(
            rows[0].subject_id,
            Principal::User("samvisebot".into()),
            "an agent-wide rule is OWNED by the AGENT, not by the admin who set it"
        );
        // Recalled for a DIFFERENT, non-admin user — proving agent-wide reach.
        let bilbo = recall_behaviour_rules(&pool, &req_consumer("?", "bilbo", "botdeploy")).await;
        assert_eq!(
            bilbo.iter().map(|(_, b, _)| b.as_str()).collect::<Vec<_>>(),
            vec!["Per i task pesanti delega a Claude Code."],
            "an agent-wide rule is recalled for EVERY user, not just the admin"
        );
        drop(dir);
    }

    /// An AGENT-WIDE behaviour-rule from a NON-admin is REFUSED: nothing is
    /// filed, and the dedicated `rules` field carries a one-shot notice steering
    /// the agent to decline. A per-user rule from the same user would still be
    /// accepted — only agent-wide changes are admin-gated.
    #[tokio::test]
    async fn agent_wide_behaviour_rule_from_non_admin_is_refused() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        // alice has no is_admin=1 row → not the admin.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"agent-wide\",\
             \"body\":\"Per i task pesanti delega a Claude Code.\"}],\
             \"suggested_seed\":\"Ok.\"}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("delega tutto a claude", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest non-admin");

        let rows = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            rows.is_empty(),
            "a non-admin's agent-wide rule must not be filed anywhere"
        );
        assert!(resp.capture_id.is_none(), "nothing filed → no anchor id");
        assert!(
            resp.rules
                .as_deref()
                .unwrap_or_default()
                .contains("reserved to the admin"),
            "the `rules` field tells the agent to decline (an agent-wide change is admin-only)"
        );
        drop(dir);
    }

    /// A USER-GLOBAL behaviour-rule (explicitly every-assistant)
    /// is filed in the SENDER's identity wiki, owned by the sender — and the
    /// rules channel serves it to every consumer serving that user, the
    /// bindingless smart consumer included. No admin gate: it binds only the
    /// user's own conversations.
    #[tokio::test]
    async fn user_global_rule_lands_in_sender_wiki_and_reaches_every_consumer() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"user-global\",\
             \"body\":\"Parlami in italiano.\"}]}",
        );
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("ogni assistente mi parli in italiano", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest user-global");

        // Home: the sender's own identity wiki — the agent wiki stays clean.
        let in_agent = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            in_agent.is_empty(),
            "a user-global rule never lands in the calling agent's wiki"
        );
        let rows = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("alice".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "exactly one rule in alice's identity wiki");
        assert_eq!(rows[0].subject_id, Principal::User("alice".into()));
        assert!(rows[0].source_path.ends_with("@rules.md"));

        // Served through the channel on a bound consumer…
        let via_agent =
            recall_behaviour_rules(&pool, &req_consumer("?", "alice", "botdeploy")).await;
        assert_eq!(
            via_agent
                .iter()
                .map(|(_, b, s)| (b.as_str(), *s))
                .collect::<Vec<_>>(),
            vec![("Parlami in italiano.", BehaviourScope::UserGlobal)],
            "the user's everywhere-rule reaches a bound consumer"
        );
        // …and on a bindingless (smart) consumer alike.
        let smart = recall_behaviour_rules(&pool, &req("?", "alice")).await;
        assert_eq!(
            smart.iter().map(|(_, b, _)| b.as_str()).collect::<Vec<_>>(),
            vec!["Parlami in italiano."],
            "the user's everywhere-rule reaches a consumer with no agent wiki binding"
        );
        // Never for another user — it is alice's own rule.
        let bilbo = recall_behaviour_rules(&pool, &req_consumer("?", "bilbo", "botdeploy")).await;
        assert!(
            bilbo.is_empty(),
            "another user's channel never carries alice's user-global rule"
        );
        drop(dir);
    }

    /// The `YOUR RULES` order is pinned, most specific last: agent-wide (the
    /// floor) → user-global (the user's everywhere-set) → per-user (this
    /// agent, this user).
    #[tokio::test]
    async fn rules_channel_order_is_agent_wide_then_user_global_then_per_user() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        // alice is the operator/admin, so her agent-wide rule files.
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('alice', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let policy = IngestPolicy::default();
        for (scope, body) in [
            ("per-user", "Rispondi conciso."),
            ("user-global", "Parlami in italiano."),
            ("agent-wide", "Non dare consigli medici."),
        ] {
            let llm = FakeLlmBackend::new(
                "fake",
                format!(
                    "{{\"intent\":\"capture\",\"extractions\":[{{\"behaviour_rule\":true,\
                     \"behaviour_scope\":\"{scope}\",\"body\":\"{body}\"}}]}}"
                ),
            );
            wiki_ingest_message(
                &pool,
                &tree,
                fake_embedder(),
                &llm,
                None,
                req_consumer("una regola", "alice", "botdeploy"),
                &policy,
            )
            .await
            .expect("ingest rule");
        }
        let rules = recall_behaviour_rules(&pool, &req_consumer("?", "alice", "botdeploy")).await;
        assert_eq!(
            rules
                .iter()
                .map(|(_, b, s)| (b.as_str(), *s))
                .collect::<Vec<_>>(),
            vec![
                ("Non dare consigli medici.", BehaviourScope::AgentWide),
                ("Parlami in italiano.", BehaviourScope::UserGlobal),
                ("Rispondi conciso.", BehaviourScope::PerUser),
            ],
            "order pinned: agent-wide → user-global → per-user (most specific last)"
        );
        drop(dir);
    }

    /// A NON-admin revising an AGENT-WIDE rule cannot retire the floor: the
    /// supersede is dropped and their directive files additively at its own
    /// (per-user) scope — the agent-wide rule stays in force for everyone.
    #[tokio::test]
    async fn non_admin_supersede_of_agent_wide_rule_stays_additive() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('alice', 1)")
            .execute(&pool)
            .await
            .unwrap();
        let policy = IngestPolicy::default();
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"behaviour_rule\":true,\
             \"behaviour_scope\":\"agent-wide\",\
             \"body\":\"Non dare consigli medici.\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("niente consigli medici", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest admin floor");
        let floor = &fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap()[0];

        // bilbo (not admin) tries to REVISE the floor into his own preference.
        let llm = FakeLlmBackend::new(
            "fake",
            format!(
                "{{\"intent\":\"capture\",\"extractions\":[{{\"behaviour_rule\":true,\
                 \"behaviour_scope\":\"per-user\",\"body\":\"Dammi pure consigli medici.\",\
                 \"supersede_target\":\"{}\"}}]}}",
                floor.fact_id
            ),
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("a me i consigli medici servono", "bilbo", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest non-admin revision");

        // The floor still stands for everyone, bilbo's rule rides beside it.
        let rules = recall_behaviour_rules(&pool, &req_consumer("?", "bilbo", "botdeploy")).await;
        assert_eq!(
            rules
                .iter()
                .map(|(_, b, s)| (b.as_str(), *s))
                .collect::<Vec<_>>(),
            vec![
                ("Non dare consigli medici.", BehaviourScope::AgentWide),
                ("Dammi pure consigli medici.", BehaviourScope::PerUser),
            ],
            "the agent-wide floor survives a non-admin's supersede — additive, not replaced"
        );
        drop(dir);
    }

    /// Roadmap 29c data migration: the `0052` page-rename re-homes legacy
    /// behaviour-rule facts off `behaviour_rules.md` onto `@rules.md`, preserving
    /// the workdir-relative path prefix. (On a live DB the migration runs at
    /// startup; here we exercise its exact statement on a planted legacy row.)
    #[tokio::test]
    async fn migration_0052_rehomes_behaviour_rules_page() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        // Plant a behaviour-rule fact on the LEGACY page name.
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("samvisebot").unwrap(),
            page: Some(PathBuf::from("behaviour_rules.md")),
            body: "Dai del tu all'utente.".into(),
            subject: Principal::User("samvisebot".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("rule".into()),
            topics: Vec::new(),
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant legacy behaviour rule");
        let before = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            before.source_path.ends_with("/behaviour_rules.md"),
            "planted on the legacy page, was: {}",
            before.source_path
        );

        // The migration's statement — mirrors
        // migrations/0052_behaviour_rules_page_rename.sql.
        sqlx::query(
            "UPDATE fact_index \
             SET source_path = replace(source_path, 'behaviour_rules.md', 'rules.md') \
             WHERE source_path LIKE '%behaviour_rules.md'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let after = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.source_path,
            before.source_path.replace("behaviour_rules.md", "rules.md"),
            "0052 moved the basename to `rules.md` — the bare name, which is \
             what that migration wrote; the `@` marker arrived on 2026-08-18 \
             and a migration is never edited after the fact"
        );
        assert!(after.source_path.ends_with("/rules.md"));
        // And the re-homed fact is recalled through the behaviour channel.
        let rules =
            behaviour_rows_on_page(&pool, "samvisebot", &Principal::User("samvisebot".into()))
                .await;
        assert_eq!(
            rules.iter().map(|(_, b)| b.as_str()).collect::<Vec<_>>(),
            vec!["Dai del tu all'utente."],
            "the re-homed legacy rule recalls from rules.md"
        );
        drop(dir);
    }

    /// A behaviour-rule capture request in the agent's own wiki, on the page
    /// `page` — the shape `capture_behaviour_rule` produces, for planting
    /// channel fixtures directly.
    fn agent_fact_req(page: &str, body: &str, dedup_threshold: Option<f32>) -> CaptureRequest {
        CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("samvisebot").unwrap(),
            page: Some(PathBuf::from(page)),
            body: body.to_owned(),
            subject: Principal::User("samvisebot".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("rule".into()),
            page_description: None,
            topics: Vec::new(),
            dedup_threshold,
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        }
    }

    /// Starvation regression (the cap-before-filter bug): the rules-page
    /// predicate applies IN the SQL before `BEHAVIOUR_RULES_RECALL_CAP`, so
    /// an old rule survives more than a cap's worth of NEWER same-subject
    /// facts on the agent wiki's content pages.
    #[tokio::test]
    async fn behaviour_rules_survive_a_crowded_agent_wiki() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        // The standing rule — backdated so it is unambiguously the OLDEST row.
        let rule = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            agent_fact_req(crate::wiki::RULES_FILENAME, "Dai del tu all'utente.", None),
        )
        .await
        .expect("plant rule");
        sqlx::query("UPDATE fact_index SET created_at = '2020-01-01T00:00:00Z' WHERE fact_id = ?")
            .bind(rule.fact_id.as_str())
            .execute(&pool)
            .await
            .unwrap();
        // Crowd the wiki with MORE THAN the recall cap of newer same-subject
        // facts on a content page (the agent's self-facts).
        for i in 0..(BEHAVIOUR_RULES_RECALL_CAP + 5) {
            capture::wiki_capture(
                &tree,
                &pool,
                fake_embedder(),
                agent_fact_req(
                    "esperienze.md",
                    &format!("agent self note number {i}"),
                    Some(1.01), // crowding, not dedup, is under test
                ),
            )
            .await
            .expect("crowd");
        }

        let rules =
            behaviour_rows_on_page(&pool, "samvisebot", &Principal::User("samvisebot".into()))
                .await;
        assert_eq!(
            rules.iter().map(|(_, b)| b.as_str()).collect::<Vec<_>>(),
            vec!["Dai del tu all'utente."],
            "the old rule must outlive a cap's worth of newer non-rules facts"
        );
        drop(dir);
    }

    /// Retraction semantics: a rule whose validity window was closed (the
    /// conversational closure path) stops being served by the channel, while
    /// the fact itself stays — closing is never deleting.
    #[tokio::test]
    async fn retracted_behaviour_rule_stops_serving_but_the_fact_stays() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let kept = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            agent_fact_req(
                crate::wiki::RULES_FILENAME,
                "Rispondi in modo conciso.",
                None,
            ),
        )
        .await
        .expect("plant kept rule");
        let retracted = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            agent_fact_req(crate::wiki::RULES_FILENAME, "Dai del tu all'utente.", None),
        )
        .await
        .expect("plant retracted rule");

        // The closure verb retracts one (a validity statement, not a delete).
        let closed_at = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        fact_index::close_validity(&pool, &retracted.fact_id, &closed_at, "retracted", None)
            .await
            .expect("close")
            .expect("row exists");

        let rules =
            behaviour_rows_on_page(&pool, "samvisebot", &Principal::User("samvisebot".into()))
                .await;
        assert_eq!(
            rules.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            vec![&kept.fact_id],
            "a closed-window rule must stop steering the agent"
        );
        // Closing is never deleting: the row stays active in the index.
        let row = fact_index::find_by_id(&pool, &retracted.fact_id)
            .await
            .unwrap()
            .expect("row still present");
        assert!(row.deleted_at.is_none() && row.superseded_at.is_none());
        drop(dir);
    }

    /// The bundled ingest prompt keeps the boundary that decides what this
    /// slot may act on.
    ///
    /// `supersede_target` survives for ONE target — a behaviour rule — because
    /// `agent_behaviour_rules` is the complete set of the directives in force,
    /// so the model can see everything it would be replacing. `recalled_memory`
    /// is a similarity sample of a store that may hold thousands, and a
    /// judgement about a stored fact made from a sample fails by omission: the
    /// prompt must keep saying so, and must keep the repeat-vs-revise guard on
    /// the one supersede that remains.
    #[test]
    fn bundled_ingest_prompt_keeps_the_complete_set_boundary() {
        assert!(
            BUNDLED_INGEST_PROMPT_MD.contains("You may act against a set you are shown COMPLETE"),
            "the complete-set-vs-sample rule is gone from the bundled prompt"
        );
        assert!(
            BUNDLED_INGEST_PROMPT_MD.contains("REVISING vs REPEATING a standing directive"),
            "the behaviour-rule repeat guard is gone from the bundled prompt"
        );
        assert!(
            !BUNDLED_INGEST_PROMPT_MD.contains("\"closures\":"),
            "reconciliation against stored facts must not come back to this slot"
        );
    }

    /// The bundled ingest prompt carries the explicit-relationship gate: a
    /// person the message leaves unnamed is never identified with a
    /// `known_users` entry, and relationship facts require the sender to
    /// state the tie explicitly.
    #[test]
    fn bundled_ingest_prompt_carries_the_explicit_relationship_gate() {
        assert!(
            BUNDLED_INGEST_PROMPT_MD.contains("Explicitly stated ONLY — never inferred"),
            "the explicit-relationship gate is gone from the bundled prompt"
        );
        assert!(
            BUNDLED_INGEST_PROMPT_MD.contains("it never runs in reverse"),
            "the no-reverse-alias-resolution guard is gone from the bundled prompt"
        );
    }

    /// A single turn can carry BOTH an engine-rule and an ordinary fact. The
    /// rule is appended to `@rules.md`; the fact still routes normally (buffered
    /// for the standard `alice` wiki) and surfaces as the turn's `capture_id`.
    #[tokio::test]
    async fn ingest_mixed_turn_files_fact_and_appends_rule() {
        let (dir, tree, pool) = setup_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[\
            {\"engine_rule\":true,\"fact_type\":\"rule\",\
             \"body\":\"Never store my exact home address.\"},\
            {\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:alice\",\
             \"body\":\"Alice lives in Bologna.\",\"fact_type\":\"bio\",\"topics\":[\"bio\"]}],\
            \"suggested_seed\":\"Noted.\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("non salvare il mio indirizzo. vivo a Bologna.", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        // The ordinary fact buffers (standard wiki) and anchors the turn.
        let cid = resp
            .capture_id
            .expect("the ordinary fact surfaces a capture_id");
        let buffered = capture_buffer::find_all_buffered(&pool, 100).await.unwrap();
        assert_eq!(buffered.len(), 1, "exactly the one ordinary fact buffers");
        assert_eq!(buffered[0].capture_id, cid);
        assert_eq!(buffered[0].body, "Alice lives in Bologna.");
        // The rule went to rules.md, not the buffer.
        let rules = std::fs::read_to_string(
            tree.wikis_dir()
                .join("alice")
                .join(crate::wiki::RULES_FILENAME),
        )
        .expect("@rules.md written");
        assert!(rules.contains("- Never store my exact home address."));
        drop(dir);
    }

    /// End-to-end of the supersede wiring: with a fact already in
    /// `fact_index` and the LLM emitting `supersede_target` pointing at
    /// its `fact_id`, the orchestrator must (a) write a new row, (b)
    /// stamp `superseded_at`/`superseded_by` on the old row, and (c)
    /// surface the new row's `fact_id` as `capture_id`. This is the
    /// behaviour that was missing before the wiring landed — the prompt
    /// emitted `supersede_target` but the orchestrator ignored it
    /// and just inserted a duplicate new row.
    #[tokio::test]
    async fn ingest_supersede_target_routes_to_wiki_supersede() {
        let (dir, tree, pool) = setup_workdir().await;
        // Plant the row that we want the next turn to supersede.
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice prefers coffee black".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("preference".into()),
            topics: vec!["coffee".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        // LLM returns capture intent with supersede_target pointing at
        // the planted row. The new body contradicts the recalled fact.
        // `requested_container: true` keeps the live direct-write path so
        // the supersede routes to `wiki_supersede` (a plain
        // capture into the standard `alice` wiki would buffer instead).
        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"target_page\":\"preferenze.md\",\"subject_id\":\"user:alice\",\
             \"body\":\"alice now prefers tea\",\
             \"fact_type\":\"preference\",\"topics\":[\"tea\"],\
             \"requested_container\":true,\
             \"supersede_target\":\"{}\",\
             \"suggested_seed\":\"Aggiornato.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let policy = IngestPolicy::default();

        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ora preferisco il tè", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        let new_fact_id = resp.capture_id.expect("must surface the new fact_id");
        assert_ne!(
            new_fact_id, planted.fact_id,
            "supersede must mint a fresh fact_id, not reuse the old one"
        );

        // The new row is active and carries the new body.
        let new_row = fact_index::find_by_id(&pool, &new_fact_id)
            .await
            .expect("find new")
            .expect("new row exists");
        assert_eq!(new_row.text, "alice now prefers tea");
        assert!(
            new_row.superseded_at.is_none(),
            "the freshly captured row must remain active"
        );

        // The old row is now tombstoned with superseded_by pointing at the new one.
        let old_row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find old")
            .expect("old row exists");
        assert!(
            old_row.superseded_at.is_some(),
            "the recalled row must have superseded_at stamped"
        );
        assert_eq!(
            old_row.superseded_by.as_ref(),
            Some(&new_fact_id),
            "old row must point at the new fact_id"
        );

        drop(dir);
    }

    /// Supersede is a CONTENT update, not a sharing change: the new fact
    /// INHERITS the superseded fact's `allow`, even when the classifier's
    /// supersede extraction carries none. Without this a re-statement
    /// silently re-privatizes a shared fact.
    #[tokio::test]
    async fn ingest_supersede_inherits_the_superseded_facts_allow() {
        let (dir, tree, pool) = setup_workdir().await;
        // Plant a fact SHARED with group:famiglia.
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice is 72 kg".into(),
            subject: Principal::User("alice".into()),
            allow: vec![Principal::Group("famiglia".into())],
            sender: None,
            fact_type: Some("state".into()),
            page_description: None,
            topics: vec!["weight".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        // The supersede extraction carries NO allow_ids — the classifier
        // restated only the content.
        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"target_page\":\"preferenze.md\",\"subject_id\":\"user:alice\",\
             \"body\":\"alice is 73 kg\",\
             \"fact_type\":\"state\",\"topics\":[\"weight\"],\
             \"requested_container\":true,\
             \"supersede_target\":\"{}\",\
             \"suggested_seed\":\"Aggiornato.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let policy = IngestPolicy::default();

        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ora peso 73 kg", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        let new_fact_id = resp.capture_id.expect("must surface the new fact_id");
        let new_row = fact_index::find_by_id(&pool, &new_fact_id)
            .await
            .expect("find new")
            .expect("new row exists");
        assert_eq!(
            new_row.allow_ids,
            vec![Principal::Group("famiglia".into())],
            "supersede must INHERIT the superseded fact's allow, not reset it to empty"
        );

        drop(dir);
    }

    // ---------- the closure verb (completion / forget gesture) ----------

    /// A pure closure turn (intent capture, empty extractions, one closure
    /// against a recalled fact) must close the target's validity, emit the
    /// born-applied `validity_close` receipt + the `structure_applied`
    /// notice, and must NOT demote to the skip fallback.
    #[tokio::test]
    async fn ingest_closure_completes_a_recalled_open_fact() {
        let (dir, tree, pool) = setup_workdir().await;
        // Plant the open watchlist item the next turn completes.
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice wants to watch Jumanji".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("plan".into()),
            page_description: None,
            topics: vec!["film".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"closures\":[{{\"target\":\"{}\",\"reason\":\"completed\",\
             \"valid_to\":\"2026-06-10T22:00:00Z\"}}],\
             \"suggested_seed\":\"Segnato come visto.\"}}",
            planted.fact_id.as_str()
        );
        // Scripted rather than a fake that repeats itself: the reconciliation
        // stage runs after the reading and would otherwise be handed the
        // classifier's own JSON back and apply the same closure twice. This
        // test is about the operator-override route into `apply_plan_closures`,
        // so the stage is scripted to change nothing.
        let llm = ScriptedLlm::new(&[
            &llm_resp,
            "{\"closures\":[],\"validity_edits\":[],\"acl_changes\":[]}",
        ]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ieri sera abbiamo visto Jumanji", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        // The turn is real activity — never the skip fallback.
        assert_eq!(resp.intent, IntentKind::Capture);
        assert_eq!(resp.suggested_seed.as_deref(), Some("Segnato come visto."));

        // The target's window closed with the completion stamp.
        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(row.valid_to.as_deref(), Some("2026-06-10T22:00:00Z"));
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
        assert!(row.deleted_at.is_none(), "closure is never a tombstone");

        // One born-applied receipt with the validity_close spec.
        let (status, spec): (String, String) = sqlx::query_as(
            "SELECT status, spec FROM structure_proposals WHERE kind = 'wiki_promote'",
        )
        .fetch_one(&pool)
        .await
        .expect("receipt row");
        assert_eq!(status, "applied");
        let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
        assert_eq!(spec["variant"], "validity_close");
        assert_eq!(spec["closures"][0]["fact_id"], planted.fact_id.as_str());
        assert!(
            spec["closures"][0]["prev_valid_to"].is_null(),
            "the receipt records that the window was open before"
        );

        // The dashboard notice.
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1);
        drop(dir);
    }

    /// A closure whose `valid_to` is a MALFORMED non-ISO string (an
    /// unresolved relative phrase the classifier failed to convert) must not
    /// poison the stored row: the bad bound is treated as absent and falls
    /// back to this turn's instant, exactly like the validity-edit path.
    #[tokio::test]
    async fn ingest_closure_with_malformed_valid_to_falls_back_to_turn_now() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice wants to watch Jumanji".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("plan".into()),
            page_description: None,
            topics: vec!["film".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        // The classifier emits a relative phrase instead of an ISO instant.
        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"closures\":[{{\"target\":\"{}\",\"reason\":\"completed\",\
             \"valid_to\":\"ieri sera\"}}],\
             \"suggested_seed\":\"Segnato come visto.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let occurred = chrono::DateTime::parse_from_rfc3339("2026-06-11T09:00:00Z")
            .expect("fixture timestamp")
            .with_timezone(&chrono::Utc);
        let request = IngestRequest {
            metadata: IngestMetadata {
                occurred_at: Some(occurred),
                ..Default::default()
            },
            ..req("ieri sera abbiamo visto Jumanji", "alice")
        };
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            request,
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);

        // The window closed at the turn's instant, NOT the garbage string —
        // and in the one spelling the column is compared in, since the
        // due-soon scan reads `valid_to` as a string.
        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.valid_to.as_deref(),
            Some(fact_index::bound_from_instant(occurred).as_str()),
            "a malformed valid_to must fall back to turn_now, never store verbatim"
        );
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
        assert!(row.deleted_at.is_none(), "closure is never a tombstone");
        drop(dir);
    }

    /// A closure naming an id outside this turn's recall window is the
    /// anti-hallucination case: skipped with a warn, and with nothing else
    /// in the plan the turn demotes to the skip fallback.
    #[tokio::test]
    async fn ingest_closure_target_not_in_recall_is_skipped() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[],\
             \"closures\":[{\"target\":\"0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99\",\
             \"reason\":\"completed\",\"valid_to\":null}]}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ho comprato il latte", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "nothing applied, but a capture turn always reaches the reading"
        );
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        drop(dir);
    }

    /// The same-day flow: the closure's target is still in the captures
    /// buffer (surfaced by the fresh-capture recall slot). The closure must
    /// land on the buffer row — `valid_to` + `decay_reason` staged — so the
    /// promotion carries them onto the fact.
    #[tokio::test]
    async fn ingest_closure_lands_on_a_buffered_capture() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("lista_spesa.md")),
            body: "manca il latte".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("plan".into()),
            page_description: None,
            topics: vec!["spesa".into()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: Some(crate::wiki::PageStyle::Lista),
            salience: None,
        };
        let buffered = capture_buffer::buffer_capture(&pool, cap_req, None)
            .await
            .expect("buffer");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"closures\":[{{\"target\":\"{}\",\"reason\":\"completed\",\
             \"valid_to\":null}}],\
             \"suggested_seed\":\"Latte segnato come comprato.\"}}",
            buffered.capture_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ho comprato il latte", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);

        // The buffer row carries the staged closure.
        let rows = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffered");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].valid_to.is_some(), "closing valid_to staged");
        assert_eq!(
            rows[0].decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
        drop(dir);
    }

    /// A guest turn is ephemeral: recall runs on the public slice only, the
    /// classifier never fires (a scripted-empty LLM would panic if called),
    /// nothing lands in the buffer, and the `rules` channel carries the
    /// reserved-behaviour directive.
    #[tokio::test]
    async fn guest_turn_is_ephemeral_and_recalls_public_slice_only() {
        let (dir, tree, pool) = setup_workdir().await;
        // One public capture (allow=global) and one private to alice.
        let public = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("bacheca.md")),
            body: "la farmacia chiude alle 19".into(),
            subject: Principal::User("alice".into()),
            allow: vec![Principal::Group("global".into())],
            sender: None,
            fact_type: Some("other".into()),
            page_description: None,
            topics: vec!["paese".into()],
            dedup_threshold: None,
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let private = CaptureRequest {
            body: "alice prende il lexotan la sera".into(),
            allow: Vec::new(),
            page: Some(PathBuf::from("salute.md")),
            ..public.clone()
        };
        capture_buffer::buffer_capture(&pool, public, None)
            .await
            .expect("buffer public");
        capture_buffer::buffer_capture(&pool, private, None)
            .await
            .expect("buffer private");

        // Empty script → any classifier call panics, proving the skip.
        let llm = ScriptedLlm::new(&[]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("a che ora chiude la farmacia?", "guest"),
            &IngestPolicy::default(),
        )
        .await
        .expect("guest ingest");

        assert_eq!(resp.intent, IntentKind::Skip);
        assert!(!resp.llm_used, "no classifier on a guest turn");
        assert!(resp.capture_id.is_none(), "nothing filed");
        assert!(resp.suggested_seed.is_none(), "no canned seed to lie with");
        let rules = resp.rules.expect("guest directive present");
        assert!(rules.contains("UNIDENTIFIED SPEAKER"), "got: {rules}");
        let snippet = resp.context_snippet.expect("public slice recalled");
        assert!(
            snippet.contains("farmacia"),
            "public fact visible: {snippet}"
        );
        assert!(
            !snippet.contains("lexotan"),
            "private fact must not leak to guest: {snippet}"
        );
        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            2,
            "the guest turn buffered nothing new"
        );
        drop(dir);
    }

    // A backend that pops scripted complete() responses in order — for
    // flows that call the slot more than once per turn (the classifier
    // plus the closure confirmer).
    struct ScriptedLlm(std::sync::Mutex<std::collections::VecDeque<String>>);
    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self(std::sync::Mutex::new(
                responses.iter().map(|s| (*s).to_owned()).collect(),
            ))
        }
    }
    #[async_trait]
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
                finish_reason: FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage {
                    prompt_tokens: None,
                    completion_tokens: None,
                    cached_prompt_tokens: None,
                    cache_write_tokens: None,
                },
            })
        }
    }

    /// The 1i hybrid: a structural turn ("voglio un ricettario: aggiungi
    /// l'amatriciana…") keeps its dashboard nudge AND files the content
    /// extractions it carries — the recipe must not be lost while the
    /// container waits.
    #[tokio::test]
    async fn structural_hybrid_files_its_content_extractions() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"structural\",\"extractions\":[{\
              \"target_wiki_id\":\"alice\",\"target_page\":\"ricette.md\",\
              \"subject_id\":\"user:alice\",\"fact_type\":\"other\",\
              \"style\":\"prosa-tecnica\",\
              \"body\":\"Ricetta amatriciana: guanciale, pecorino, passata di pomodoro.\",\
              \"topics\":[\"ricette\"]}]}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(
                "voglio creare un ricettario: aggiungi gli spaghetti all'amatriciana",
                "alice",
            ),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        // The turn stays structural — the nudge is the answer…
        assert_eq!(resp.intent, IntentKind::Structural);
        assert_eq!(
            resp.suggested_seed,
            Some(IngestPolicy::default().structural_suggested_seed)
        );
        // …and the content half filed (buffered for the light dream).
        assert!(resp.capture_id.is_some(), "the recipe fact filed");
        let rows = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffered");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].body.contains("amatriciana"));
        drop(dir);
    }

    /// A structural turn with no extractions never synthesizes a legacy
    /// unit from its top-level fields (that would capture the container
    /// request itself) and never demotes to skip.
    #[tokio::test]
    async fn structural_without_content_stays_a_pure_nudge() {
        let (dir, tree, pool) = setup_workdir().await;
        // Top-level body present (a sloppy model) — must NOT be filed.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"structural\",\"target_wiki_id\":\"alice\",\
             \"body\":\"voglio un quaderno per le ricette\"}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("voglio un quaderno per le ricette", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Structural);
        assert!(resp.capture_id.is_none(), "no legacy synthesis");
        let rows = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffered");
        assert!(rows.is_empty(), "nothing filed");
        drop(dir);
    }

    /// The 1h fix end-to-end: the classifier cannot see its gesture's
    /// targets and names `closure_topics` instead of guessing; the
    /// focused second recall surfaces the starved fact and the confirmer
    /// closes exactly it — receipt and all.
    #[tokio::test]
    async fn closure_topics_second_pass_closes_the_starved_target() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice is building a small greenhouse in the garden".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("plan".into()),
            page_description: None,
            topics: vec!["serra".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let classify = "{\"intent\":\"capture\",\"extractions\":[],\"closures\":[],\
                        \"closure_topics\":[\"serra\"],\
                        \"suggested_seed\":\"Progetto serra dimenticato.\"}";
        let confirm = format!(
            "{{\"closures\":[{{\"target\":\"{}\",\"reason\":\"retracted\",\
             \"valid_to\":null}}]}}",
            planted.fact_id.as_str()
        );
        // Plus the reconciliation stage, which runs after the reading on
        // every capture turn that surfaced candidates; scripted to change
        // nothing so this test keeps measuring the topic pass alone.
        let llm = ScriptedLlm::new(&[
            classify,
            &confirm,
            "{\"closures\":[],\"validity_edits\":[],\"acl_changes\":[]}",
        ]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("dimentica quello che ti ho detto sulla serra", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture, "a real closure turn");

        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::RETRACTED),
            "the starved target closed via the topic pass"
        );
        assert!(row.valid_to.is_some());
        let receipts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM structure_proposals WHERE status = 'applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(receipts, 1, "one born-applied validity_close receipt");
        drop(dir);
    }

    /// The reconciliation stage closes a fact the classifier never even saw a
    /// verb for. The classifier's plan carries NO closures — the bundled
    /// prompt stopped asking for them in v2.59 — and the retirement is decided
    /// after the memory has been read, from the candidate set the turn built.
    #[tokio::test]
    async fn the_reconciliation_stage_retires_what_the_message_says_is_done() {
        let (dir, tree, pool) = setup_workdir().await;
        let planted = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            CaptureRequest {
                subject_external: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("cucina.md")),
                body: "alice deve comprare il latte".into(),
                subject: Principal::User("alice".into()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("plan".into()),
                page_description: None,
                topics: vec!["spesa".into()],
                dedup_threshold: Some(0.99),
                valid_from: None,
                valid_to: None,
                style: None,
                salience: None,
            },
        )
        .await
        .expect("plant");

        // The classifier says only what the MESSAGE is; it names no fact.
        let classify = "{\"intent\":\"capture\",\"extractions\":[],\
                        \"suggested_seed\":\"Fatto.\"}";
        let reconcile = format!(
            "{{\"closures\":[{{\"target\":\"{}\",\"reason\":\"completed\",\"valid_to\":null}}],\
              \"validity_edits\":[],\"acl_changes\":[]}}",
            planted.fact_id.as_str()
        );
        let llm = ScriptedLlm::new(&[classify, &reconcile]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ho comprato il latte", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);

        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED),
            "the reconciliation stage retired the spent commitment"
        );
        assert!(
            row.valid_to.is_some(),
            "and stamped when it stopped holding"
        );
        drop(dir);
    }

    /// The supersede verb, and the half that must never be lost: a fact that
    /// A public predecessor does **not** publish its successor.
    ///
    /// Inheritance exists so a restatement cannot quietly hide a fact from
    /// people who could already read it. `global` is the one reader where
    /// that reasoning inverts: it is not a wider audience but the absence of
    /// one, and carrying it over turns a decision somebody just made — «the
    /// household, nobody else» — into a public claim. Measured on the bench
    /// on 2026-08-26: the classifier answered `group:famiglia` for a
    /// pregnancy, a mis-paired supersede against a public profile fact
    /// carried `global` onto it, and every health fact of that person
    /// followed it public.
    ///
    /// So the successor keeps what the classifier gave it, and the supersede
    /// still applies — the pairing may be wrong, but retiring the old fact is
    /// a separate question from who reads the new one.
    #[tokio::test]
    async fn a_public_predecessor_leaves_its_successors_audience_alone() {
        let (dir, tree, pool) = setup_workdir().await;
        let plant = |body: &'static str, allow: Vec<Principal>| {
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        subject_external: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("alice").unwrap(),
                        page: Some(PathBuf::from("cucina.md")),
                        body: body.into(),
                        subject: Principal::User("alice".into()),
                        allow,
                        sender: None,
                        fact_type: Some("bio".into()),
                        page_description: None,
                        topics: vec!["salute".into()],
                        dedup_threshold: Some(1.01),
                        valid_from: None,
                        valid_to: None,
                        style: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant")
            }
        };
        // The profile fact alice declared public at enrolment, and a health
        // fact the classifier deliberately kept inside the household.
        let old = plant("alice is a mother", vec![Principal::global()]).await;
        let new = plant(
            "alice is 29 weeks pregnant",
            vec![Principal::Group("famiglia".into())],
        )
        .await;

        let candidates = vec![recall::RecallHit::from_row(
            fact_index::find_by_id(&pool, &old.fact_id)
                .await
                .unwrap()
                .unwrap(),
            1.0,
        )];
        let turn_facts = vec![(new.fact_id.clone(), "alice is 29 weeks pregnant".to_owned())];

        let applied = apply_reconciled_supersedes(
            &pool,
            &[LlmSupersede {
                target: Some(old.fact_id.as_str().to_owned()),
                successor: Some(new.fact_id.as_str().to_owned()),
            }],
            &candidates,
            &turn_facts,
            &req("alice is 29 weeks pregnant", "alice"),
        )
        .await;
        assert_eq!(applied, 1, "the supersede still applied");

        let successor = fact_index::find_by_id(&pool, &new.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            successor.allow_ids,
            vec![Principal::Group("famiglia".into())],
            "a public predecessor must not publish its successor; the audience \
             the classifier decided for THIS fact stands"
        );
        drop(dir);
    }

    /// replaces another **inherits its audience**.
    ///
    /// A restatement is a content update, not a sharing change. The reconciler
    /// can tell that a claim was restated; it must never be relied on to
    /// restate WHO MAY READ it, because a shared fact quietly coming back
    /// private is invisible to everyone including its subject. Exercised on the
    /// applier directly: the successor's id is minted inside the turn, so a
    /// scripted end-to-end run cannot name it in advance.
    #[tokio::test]
    async fn a_superseding_fact_inherits_the_audience_of_the_one_it_replaces() {
        let (dir, tree, pool) = setup_workdir().await;
        let plant = |body: &'static str, allow: Vec<Principal>| {
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        subject_external: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("alice").unwrap(),
                        page: Some(PathBuf::from("cucina.md")),
                        body: body.into(),
                        subject: Principal::User("alice".into()),
                        allow,
                        sender: None,
                        fact_type: Some("bio".into()),
                        page_description: None,
                        topics: vec!["lavoro".into()],
                        dedup_threshold: Some(1.01),
                        valid_from: None,
                        valid_to: None,
                        style: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant")
            }
        };
        // The old fact is SHARED with the family; the new one carries no
        // audience of its own — the case the inheritance exists for.
        let old = plant(
            "alice lavora alla Acme",
            vec![Principal::Group("famiglia".into())],
        )
        .await;
        let new = plant("alice lavora alla Initech", Vec::new()).await;

        let candidates = vec![recall::RecallHit::from_row(
            fact_index::find_by_id(&pool, &old.fact_id)
                .await
                .unwrap()
                .unwrap(),
            1.0,
        )];
        let turn_facts = vec![(new.fact_id.clone(), "alice lavora alla Initech".to_owned())];

        let applied = apply_reconciled_supersedes(
            &pool,
            &[LlmSupersede {
                target: Some(old.fact_id.as_str().to_owned()),
                successor: Some(new.fact_id.as_str().to_owned()),
            }],
            &candidates,
            &turn_facts,
            &req("alice adesso lavora alla Initech", "alice"),
        )
        .await;
        assert_eq!(applied, 1, "the supersede applied");

        let successor = fact_index::find_by_id(&pool, &new.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            successor.allow_ids,
            vec![Principal::Group("famiglia".into())],
            "the new fact carries the audience of the one it replaced — a restatement \
             must never re-privatise a shared fact"
        );
        let retired = fact_index::find_by_id(&pool, &old.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retired.superseded_by.as_ref(),
            Some(&new.fact_id),
            "and the old one is welded to its successor"
        );
        drop(dir);
    }

    /// A deadline still ahead of us is rendered **open**, not «closed» — to
    /// BOTH stages that judge a candidate.
    ///
    /// Each one's prompt tells the model to leave closed candidates alone, and
    /// the classifier stamps a `valid_to` on every dated commitment, so
    /// deciding "closed" on the field's presence tells them to ignore exactly
    /// the class they exist for. *«devo comprare il latte entro venerdì»* is
    /// open until Friday, and the closure gesture arrives before Friday or it
    /// would not be a closure.
    ///
    /// The confirmer is asserted beside the reconciler because they are twins
    /// and a twin is where a fixed defect comes back.
    #[tokio::test]
    async fn a_deadline_still_ahead_is_rendered_open_to_both_judging_stages() {
        let (dir, tree, pool) = setup_workdir().await;
        let now = chrono::Utc::now();
        let plant = |body: &'static str, valid_to: Option<String>| {
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        subject_external: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("alice").unwrap(),
                        page: Some(PathBuf::from("appunti.md")),
                        body: body.into(),
                        subject: Principal::User("alice".into()),
                        allow: Vec::new(),
                        sender: None,
                        fact_type: Some("plan".into()),
                        page_description: None,
                        topics: vec!["spesa".into()],
                        dedup_threshold: Some(1.01),
                        valid_from: None,
                        valid_to,
                        style: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant")
                .fact_id
            }
        };
        let due_friday = plant(
            "comprare il latte entro venerdì",
            Some((now + chrono::Duration::days(3)).to_rfc3339()),
        )
        .await;
        let long_over = plant(
            "il pacco è arrivato",
            Some((now - chrono::Duration::days(3)).to_rfc3339()),
        )
        .await;

        // Both renderers, on the same rows: whatever one says of a horizon,
        // the other says too.
        let lines_of = |id: &FactId| {
            let pool = &pool;
            let id = id.clone();
            async move {
                let row = fact_index::find_by_id(pool, &id)
                    .await
                    .unwrap()
                    .expect("row");
                let hit = recall::RecallHit::from_row(row, 1.0);
                [
                    ("reconciler", reconcile_candidate_line(&hit, &now)),
                    ("closure confirmer", closure_candidate_line(&hit, &now)),
                ]
            }
        };
        for (stage, open_line) in lines_of(&due_friday).await {
            assert!(
                open_line.contains("open") && !open_line.contains("closed"),
                "a deadline in three days is an OPEN fact with a due date \
                 to the {stage}: {open_line}"
            );
            assert!(
                open_line.contains("due "),
                "and the {stage} still shows the model the deadline: {open_line}"
            );
        }
        for (stage, over_line) in lines_of(&long_over).await {
            assert!(
                over_line.contains("closed"),
                "a horizon already passed stays closed to the {stage}: {over_line}"
            );
        }
        drop(dir);
    }

    /// The candidate set carries a buffered capture the recall block suppressed.
    ///
    /// `already_in_context` stops the recall *block* showing the agent a claim
    /// extracted from a message it is already reading. The reconciliation stage
    /// is a different question — *does this message close one of these?* — and
    /// a claim made two turns ago, whose message is still in the window, is
    /// precisely what a correction arriving now corrects. Same reasoning
    /// `confirm_topic_closures` already writes down.
    #[tokio::test]
    async fn the_reconciler_sees_a_capture_the_recall_block_suppressed() {
        let (dir, _, pool) = setup_workdir().await;
        let origin = "Ho comprato il latte al Conad.";
        capture_buffer::buffer_capture_staged(
            &pool,
            CaptureRequest {
                subject_external: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("appunti.md")),
                body: "alice ha comprato il latte".into(),
                subject: Principal::User("alice".into()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".into()),
                page_description: None,
                topics: vec!["spesa".into()],
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                salience: None,
            },
            None,
            capture_buffer::BufferStaging {
                embedding: None,
                origin_message_hash: Some(capture_buffer::origin_fingerprint(origin)),
            },
        )
        .await
        .expect("buffer");

        // The recall block filtered it out — its message is on screen.
        let mut in_context = std::collections::HashSet::new();
        in_context.insert(capture_buffer::origin_fingerprint(origin));
        let flat = recall::recall_fresh_captures(
            &pool,
            fake_embedder().as_ref(),
            "il latte",
            &SenderContext::user("alice"),
            10,
            &in_context,
        )
        .await
        .expect("fresh");
        assert!(
            flat.is_empty(),
            "the recall block is right to suppress it: {flat:?}"
        );

        let candidates = reconcile_candidates(
            &pool,
            &fake_embedder(),
            "il latte",
            &flat,
            &[],
            &SenderContext::user("alice"),
            10,
            &[],
        )
        .await;
        assert!(
            candidates.iter().any(|c| c.text.contains("latte")),
            "the stage that acts on it still sees it: {candidates:?}"
        );
        drop(dir);
    }

    /// Buffer one claim of alice's, returning the id it will carry.
    async fn buffer_claim(pool: &SqlitePool, body: &str) -> FactId {
        capture_buffer::buffer_capture(
            pool,
            CaptureRequest {
                subject_external: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("appunti.md")),
                body: body.into(),
                subject: Principal::User("alice".into()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".into()),
                page_description: None,
                topics: vec!["spesa".into()],
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                salience: None,
            },
            None,
        )
        .await
        .expect("buffer")
        .capture_id
    }

    /// The facts this turn just filed are held out of the candidate set, and
    /// holding them out costs the fresh slot no room.
    ///
    /// Every verb of the stage acts on a candidate, so a fact that reaches the
    /// list can be closed, re-dated or marked as replaced by a sibling off the
    /// same message: *«ho comprato il latte»* archiving its own episode as
    /// spent in the instant it is born, or two atomics from one sentence each
    /// named as the other's contradiction. The claim the stage is actually for
    /// is the OLDER one, and it only reaches the list because the fresh
    /// budget is widened by the count held out — at `fresh_top_k` = 1 the
    /// turn's own capture is the row the leg reaches first, and it would take
    /// the only slot there is.
    #[tokio::test]
    async fn the_turns_own_facts_are_not_candidates() {
        let (dir, _, pool) = setup_workdir().await;
        let older = buffer_claim(&pool, "alice deve comprare il latte").await;
        let this_turn = buffer_claim(&pool, "alice ha comprato il latte").await;

        let candidates = reconcile_candidates(
            &pool,
            &fake_embedder(),
            "ho comprato il latte",
            &[],
            &[],
            &SenderContext::user("alice"),
            1,
            &[(this_turn.clone(), "alice ha comprato il latte".to_owned())],
        )
        .await;

        assert!(
            !candidates.iter().any(|c| c.fact_id == this_turn),
            "no verb may name what this turn just wrote: {candidates:?}"
        );
        assert!(
            candidates.iter().any(|c| c.fact_id == older),
            "the claim the stage is for still fits the widened slot: {candidates:?}"
        );
        drop(dir);
    }

    /// The stage judges the completed message, not only the words typed.
    ///
    /// *«l'ho comprato»* matches no candidate on its own words — the noun is
    /// in the exchange before it, which this stage never sees. The classifier
    /// does see it, and wrote the sentence out in full inside the call it was
    /// already making, so carrying it down here costs nothing. The raw message
    /// stays beside it: a completion is a reading, and a reading can be wrong
    /// about what was actually said.
    #[tokio::test]
    async fn the_reconciler_is_shown_the_completed_message() {
        /// The line the `{completed_message}` placeholder rendered into.
        fn says_in_full(prompt: &str) -> String {
            prompt
                .lines()
                .skip_while(|l| !l.starts_with("WHAT IT SAYS IN FULL"))
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        }

        let (dir, tree, _pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", r#"{"closures":[],"supersedes":[]}"#);
        let mut candidate = sample_recall_hit("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77");
        candidate.text = "alice deve comprare il latte".into();

        let _ = reconcile_after_reading(
            &tree,
            &llm,
            &req("l'ho comprato", "alice"),
            chrono::Utc::now(),
            std::slice::from_ref(&candidate),
            &[],
            Some("alice ha comprato il latte"),
        )
        .await;
        let prompt = llm.last_prompt().expect("the stage called the model");
        assert_eq!(
            says_in_full(&prompt),
            "alice ha comprato il latte",
            "the completed message reaches the judge: {prompt}"
        );
        assert!(
            prompt.contains("l'ho comprato"),
            "and the words actually typed stay beside it: {prompt}"
        );

        // A turn that left nothing implicit says so, rather than leaving a
        // blank for the model to read as an instruction.
        let _ = reconcile_after_reading(
            &tree,
            &llm,
            &req("ho comprato il latte", "alice"),
            chrono::Utc::now(),
            std::slice::from_ref(&candidate),
            &[],
            None,
        )
        .await;
        assert_eq!(
            says_in_full(&llm.last_prompt().expect("called")),
            "(none)",
            "nothing left implicit is stated, not left blank"
        );
        drop(dir);
    }

    /// A served identity card is part of what the turn read, so its facts are
    /// reconciliation candidates.
    ///
    /// The card is served in full and deterministically — before the walk,
    /// whatever the navigator decides — which makes its facts the most
    /// certainly-read of the whole turn. Left out of the candidate set, *«non
    /// abito più a Bologna, mi sono trasferito»* retired the claim it corrects
    /// only when similarity happened to fish that claim up as well, and until
    /// the nightly pass the two lived side by side.
    ///
    /// The flat slot and the fresh slot are both zero here, so the card is the
    /// only leg left: the stage either sees it or is never called at all.
    #[tokio::test]
    async fn a_served_identity_card_is_reconcilable() {
        let (dir, tree, pool) = setup_workdir().await;
        seed_alice_card(&dir, &pool).await;
        let retire_the_card_fact = format!(
            "{{\"closures\":[{{\"target\":\"{ALICE_FACT_A}\",\
              \"reason\":\"contradicted\",\"valid_to\":null}}]}}"
        );
        let llm = ScriptedLlm::new(&[
            "{\"intent\":\"capture\",\"extractions\":[{\
              \"target_wiki_id\":\"alice\",\"subject_id\":\"user:alice\",\
              \"body\":\"Alice si è trasferita a Milano.\",\
              \"fact_type\":\"bio\"}]}",
            retire_the_card_fact.as_str(),
        ]);
        let policy = IngestPolicy {
            recall_top_k: 0,
            recall_fresh_top_k: 0,
            ..IngestPolicy::default()
        };
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(
                "non abito più a Bologna, mi sono trasferito a Milano",
                "alice",
            ),
            &policy,
        )
        .await
        .expect("ingest");

        let row = fact_index::find_by_id(&pool, &FactId::parse(ALICE_FACT_A).unwrap())
            .await
            .expect("read back")
            .expect("the card fact is still in the store");
        assert!(
            row.valid_to.is_some(),
            "the card's own claim is retired by the correction: {}",
            row.text
        );
        drop(dir);
    }

    /// A supersede whose target never reached the fact store still retires it.
    ///
    /// The same-day flow: a claim captured this morning, corrected this
    /// afternoon, before the light dream promoted anything.
    /// `fact_index::mark_superseded` matches no buffer row, so without the
    /// buffered half this touches nothing and logs *«already closed»* — a
    /// retirement an operator would read as done.
    #[tokio::test]
    async fn a_supersede_retires_a_target_that_is_still_buffered() {
        let (dir, _, pool) = setup_workdir().await;
        let buffered = capture_buffer::buffer_capture(
            &pool,
            CaptureRequest {
                subject_external: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("appunti.md")),
                body: "la riunione è giovedì".into(),
                subject: Principal::User("alice".into()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("plan".into()),
                page_description: None,
                topics: vec!["lavoro".into()],
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                salience: None,
            },
            None,
        )
        .await
        .expect("buffer")
        .capture_id;
        let successor = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();

        assert!(
            weld_supersede(&pool, &buffered, &successor, chrono::Utc::now()).await,
            "the buffered target is retired, not reported as already closed"
        );
        let row = capture_buffer::find_all_buffered(&pool, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.capture_id == buffered)
            .expect("still buffered, with a closed staged window");
        assert!(
            row.valid_to.is_some(),
            "its staged window closed, and promotion carries that onto the fact"
        );
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::CONTRADICTED)
        );
        assert!(
            !weld_supersede(&pool, &successor, &successor, chrono::Utc::now()).await,
            "and a target in neither store is honestly reported as nothing done"
        );
        drop(dir);
    }

    /// A fact owned by a GROUP can be corrected by a member of that group.
    ///
    /// The gate asks `acl::sender_is_subject`, not `subject == sender`: a principal
    /// comparison can never match `Group("famiglia")` against `User("alice")`,
    /// so a plain equality here refuses every group-owned fact, from
    /// everybody, always — the family calendar readable and never correctable.
    /// Its two siblings (`apply_plan_validity_edits`, `apply_plan_acl_changes`)
    /// ask the right question too.
    #[tokio::test]
    async fn a_member_may_supersede_a_fact_owned_by_their_group() {
        let (dir, tree, pool) = setup_workdir().await;
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("famiglia")
            .bind(r#"["alice"]"#)
            .execute(&pool)
            .await
            .expect("enrol alice in famiglia");
        let plant = |body: &'static str, subject: Principal| {
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        subject_external: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("alice").unwrap(),
                        page: Some(PathBuf::from("appunti.md")),
                        body: body.into(),
                        subject,
                        allow: Vec::new(),
                        sender: None,
                        fact_type: Some("bio".into()),
                        page_description: None,
                        topics: vec!["casa".into()],
                        dedup_threshold: Some(1.01),
                        valid_from: None,
                        valid_to: None,
                        style: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant")
            }
        };
        let old = plant(
            "la casa al mare si apre a giugno",
            Principal::Group("famiglia".into()),
        )
        .await;
        let new = plant(
            "la casa al mare si apre a luglio",
            Principal::Group("famiglia".into()),
        )
        .await;
        let candidates = vec![recall::RecallHit::from_row(
            fact_index::find_by_id(&pool, &old.fact_id)
                .await
                .unwrap()
                .unwrap(),
            1.0,
        )];

        let applied = apply_reconciled_supersedes(
            &pool,
            &[LlmSupersede {
                target: Some(old.fact_id.as_str().to_owned()),
                successor: Some(new.fact_id.as_str().to_owned()),
            }],
            &candidates,
            &[(
                new.fact_id.clone(),
                "la casa al mare si apre a luglio".to_owned(),
            )],
            &req("la casa al mare quest'anno si apre a luglio", "alice"),
        )
        .await;
        assert_eq!(
            applied, 1,
            "a member of the owning group may correct the group's fact"
        );
        assert_eq!(
            fact_index::find_by_id(&pool, &old.fact_id)
                .await
                .unwrap()
                .unwrap()
                .superseded_by
                .as_ref(),
            Some(&new.fact_id),
            "and the old one is welded to its successor"
        );
        drop(dir);
    }

    /// A supersede carries the retired fact's ALLOW LIST — not its subject.
    ///
    /// Alice retires a fact of her own by stating one about Bob. The successor
    /// is born owned by Bob, and a reader set is `subject ∪ allow ∪ sender`:
    /// overwriting its subject with Alice's principal would hand Bob's fact to
    /// Alice alone, taking it from the one person the sentence is about — the
    /// opposite of what the inheritance exists to protect.
    #[tokio::test]
    async fn a_supersede_leaves_the_successor_its_own_subject() {
        let (dir, tree, pool) = setup_workdir().await;
        let plant = |body: &'static str, subject: Principal, allow: Vec<Principal>| {
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        subject_external: None,
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("alice").unwrap(),
                        page: Some(PathBuf::from("appunti.md")),
                        body: body.into(),
                        subject,
                        allow,
                        sender: Some(Principal::User("alice".into())),
                        fact_type: Some("evento".into()),
                        page_description: None,
                        topics: vec!["salute".into()],
                        dedup_threshold: Some(1.01),
                        valid_from: None,
                        valid_to: None,
                        style: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant")
            }
        };
        // Alice's own, private. Then the correction: it is Bob who goes.
        let old = plant(
            "alice ha il dentista giovedì",
            Principal::User("alice".into()),
            Vec::new(),
        )
        .await;
        let new = plant(
            "bob ha il dentista giovedì",
            Principal::User("bob".into()),
            Vec::new(),
        )
        .await;
        let candidates = vec![recall::RecallHit::from_row(
            fact_index::find_by_id(&pool, &old.fact_id)
                .await
                .unwrap()
                .unwrap(),
            1.0,
        )];

        let applied = apply_reconciled_supersedes(
            &pool,
            &[LlmSupersede {
                target: Some(old.fact_id.as_str().to_owned()),
                successor: Some(new.fact_id.as_str().to_owned()),
            }],
            &candidates,
            &[(new.fact_id.clone(), "bob ha il dentista giovedì".to_owned())],
            &req("veramente dal dentista giovedì ci va bob", "alice"),
        )
        .await;
        assert_eq!(applied, 1, "the supersede applied");

        let successor = fact_index::find_by_id(&pool, &new.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            successor.subject_id,
            Principal::User("bob".into()),
            "the fact about Bob stays Bob's — a supersede replaces what a fact \
             says, never whose it is"
        );
        assert_eq!(
            successor.sender_id,
            Some(Principal::User("alice".into())),
            "and it keeps the sender it was filed with"
        );
        let readers = crate::acl::reader_set(
            &successor.subject_id,
            &successor.allow_ids,
            successor.sender_id.as_ref(),
        );
        assert!(
            readers.contains("user:bob"),
            "Bob can read the fact about Bob — the case the old inheritance broke"
        );
        assert!(
            readers.contains("user:alice"),
            "and Alice, who said it, still sees what she corrected"
        );
        drop(dir);
    }

    /// Three refusals, each preferring to change nothing over guessing: a
    /// target nobody showed the stage, a successor this turn did not write,
    /// and a target the sender does not own. Reading a fact is not authority
    /// over it.
    #[tokio::test]
    async fn supersede_refuses_a_stranger_target_a_stranger_successor_and_a_foreign_subject() {
        let (dir, tree, pool) = setup_workdir().await;
        let bobs = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            CaptureRequest {
                subject_external: None,
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: Some(PathBuf::from("cucina.md")),
                body: "bob lavora alla Acme".into(),
                subject: Principal::User("bob".into()),
                allow: vec![Principal::User("alice".into())],
                sender: None,
                fact_type: Some("bio".into()),
                page_description: None,
                topics: vec!["lavoro".into()],
                dedup_threshold: Some(1.01),
                valid_from: None,
                valid_to: None,
                style: None,
                salience: None,
            },
        )
        .await
        .expect("plant");
        let row = fact_index::find_by_id(&pool, &bobs.fact_id)
            .await
            .unwrap()
            .unwrap();
        let candidates = vec![recall::RecallHit::from_row(row, 1.0)];
        let mine = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d01").unwrap();
        let request = req("bob adesso lavora alla Initech", "alice");

        // Alice can READ bob's fact — it is a candidate — but does not own it.
        assert_eq!(
            apply_reconciled_supersedes(
                &pool,
                &[LlmSupersede {
                    target: Some(bobs.fact_id.as_str().to_owned()),
                    successor: Some(mine.as_str().to_owned()),
                }],
                &candidates,
                &[(mine.clone(), "bob lavora alla Initech".to_owned())],
                &request,
            )
            .await,
            0,
            "reading a fact is not authority over it"
        );
        // A target nobody showed the stage.
        assert_eq!(
            apply_reconciled_supersedes(
                &pool,
                &[LlmSupersede {
                    target: Some("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99".to_owned()),
                    successor: Some(mine.as_str().to_owned()),
                }],
                &candidates,
                &[(mine.clone(), "x".to_owned())],
                &request,
            )
            .await,
            0,
            "a hallucinated target retires nothing"
        );
        // A successor this turn did not write.
        assert_eq!(
            apply_reconciled_supersedes(
                &pool,
                &[LlmSupersede {
                    target: Some(bobs.fact_id.as_str().to_owned()),
                    successor: Some("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d98".to_owned()),
                }],
                &candidates,
                &[],
                &request,
            )
            .await,
            0,
            "a fact can only be welded to something this turn actually filed"
        );
        assert!(
            fact_index::find_by_id(&pool, &bobs.fact_id)
                .await
                .unwrap()
                .unwrap()
                .superseded_at
                .is_none(),
            "and after all three refusals the fact is untouched"
        );
        drop(dir);
    }

    /// The stage is a precision instrument: an empty answer changes nothing,
    /// and — the part that matters for cost — a turn whose reading surfaced
    /// no candidate at all makes no second call, so the script's single entry
    /// is never popped twice.
    #[tokio::test]
    async fn the_reconciliation_stage_calls_nothing_when_the_turn_read_nothing() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = ScriptedLlm::new(&["{\"intent\":\"capture\",\"extractions\":[]}"]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("dimentica quello che ti ho detto sul golf", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "a gesture still reaches the reading"
        );
        drop(dir);
    }

    /// A gesture about content the memory simply does not hold: the topic
    /// recall finds no candidates, the confirmer is never called (the
    /// script would panic on a second pop), nothing closes, and the turn
    /// demotes to the skip fallback.
    #[tokio::test]
    async fn closure_topics_with_no_candidates_close_nothing() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = ScriptedLlm::new(&[
            "{\"intent\":\"capture\",\"extractions\":[],\"closures\":[],\
             \"closure_topics\":[\"golf\"]}",
        ]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("dimentica quello che ti ho detto sul golf", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "nothing applied, but a capture turn always reaches the reading"
        );
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        drop(dir);
    }

    /// The confirmer is bound by the same anti-hallucination rule as the
    /// classifier: a closure naming an id outside the candidate window is
    /// skipped, so a wrong aim cannot land.
    #[tokio::test]
    async fn closure_confirmer_cannot_close_outside_its_candidates() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice is building a small greenhouse in the garden".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("plan".into()),
            page_description: None,
            topics: vec!["serra".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm = ScriptedLlm::new(&[
            "{\"intent\":\"capture\",\"extractions\":[],\"closures\":[],\
             \"closure_topics\":[\"serra\"]}",
            // The confirmer hallucinates an id that is in NO candidate list.
            "{\"closures\":[{\"target\":\"0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99\",\
             \"reason\":\"retracted\",\"valid_to\":null}]}",
            // The reconciliation stage runs after the reading on every capture
            // turn that surfaced candidates. This legacy path exercises the
            // operator-override route into the same appliers, so the stage is
            // scripted to change nothing and the test keeps measuring what it
            // was written to measure.
            "{\"closures\":[],\"validity_edits\":[],\"acl_changes\":[]}",
        ]);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("dimentica la serra", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "nothing applied, but a capture turn always reaches the reading"
        );
        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert!(row.decay_reason.is_none(), "the planted fact stays open");
        drop(dir);
    }

    // ---------- the validity-edit verb (date correction) ----------

    /// A `validity_edits` element corrects `valid_to` on a recalled OWNED
    /// fact: the row reflects the new bound, `valid_from` is left unchanged,
    /// `decay_reason` stays untouched, and a born-applied receipt is written.
    #[tokio::test]
    async fn ingest_validity_edit_corrects_dates_on_an_owned_fact() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("dispensa.md")),
            body: "il latte scade il 25 giugno".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("state".into()),
            page_description: None,
            topics: vec!["spesa".into()],
            dedup_threshold: Some(0.99),
            valid_from: Some("2026-06-10T00:00:00Z".into()),
            valid_to: Some("2026-06-25T00:00:00Z".into()),
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"validity_edits\":[{{\"target\":\"{}\",\"valid_from\":null,\
             \"valid_to\":\"2026-06-20T00:00:00Z\"}}],\
             \"suggested_seed\":\"Corretto: scade il 20.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("il latte scade il 20, non il 25", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture, "an edit is real activity");

        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(row.valid_to.as_deref(), Some("2026-06-20T00:00:00Z"));
        assert_eq!(
            row.valid_from.as_deref(),
            Some("2026-06-10T00:00:00Z"),
            "the omitted bound is untouched"
        );
        assert!(
            row.decay_reason.is_none(),
            "a date correction never stamps decay_reason"
        );

        let (status, spec): (String, String) = sqlx::query_as(
            "SELECT status, spec FROM structure_proposals WHERE kind = 'wiki_promote'",
        )
        .fetch_one(&pool)
        .await
        .expect("receipt row");
        assert_eq!(status, "applied");
        let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
        assert_eq!(spec["variant"], "validity_edit");
        assert_eq!(spec["edits"][0]["fact_id"], planted.fact_id.as_str());
        assert_eq!(spec["edits"][0]["prev_valid_to"], "2026-06-25T00:00:00Z");
        drop(dir);
    }

    /// A non-subject's validity edit is skipped (the subject gate). The fact is
    /// owned by `global` — alice can recall it but does not own it.
    #[tokio::test]
    async fn ingest_validity_edit_by_non_subject_is_skipped() {
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("public.md")),
            body: "la biblioteca chiude il 30".into(),
            subject: Principal::global(),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("state".into()),
            page_description: None,
            topics: vec!["orari".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: Some("2026-06-30T00:00:00Z".into()),
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"validity_edits\":[{{\"target\":\"{}\",\"valid_from\":null,\
             \"valid_to\":\"2026-07-15T00:00:00Z\"}}]}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("la biblioteca chiude il 15 luglio", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "the edit was refused, but a capture turn always reaches the reading"
        );
        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.valid_to.as_deref(),
            Some("2026-06-30T00:00:00Z"),
            "the fact's date is unchanged — the gate skipped the edit"
        );
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        drop(dir);
    }

    // ---------- the acl-change verb (sharing change) ----------

    /// An `acl_changes` element widens the allow-list on a recalled OWNED
    /// fact and writes a `disclosure_audit` row with `widening=1`.
    #[tokio::test]
    async fn ingest_acl_change_widens_and_audits() {
        let (_dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice ha un orto sul balcone".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("bio".into()),
            page_description: None,
            topics: vec!["orto".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"acl_changes\":[{{\"target\":\"{}\",\"subject_id\":null,\
             \"allow_ids\":[\"global\"]}}],\
             \"suggested_seed\":\"Reso pubblico.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("esponi a tutti che ho un orto sul balcone", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "an acl change is activity"
        );

        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.allow_ids,
            vec!["global".parse().unwrap()],
            "the allow-list widened to global"
        );

        // One disclosure_audit row, flagged widening.
        let (_audit_id, widening): (i64, i64) =
            sqlx::query_as("SELECT audit_id, widening FROM disclosure_audit WHERE fact_id = ?")
                .bind(planted.fact_id.as_str())
                .fetch_one(&pool)
                .await
                .expect("audit row");
        assert_eq!(widening, 1, "going global is a widening");
    }

    #[tokio::test]
    async fn ingest_acl_change_preserves_cross_user_sender() {
        // A fact alice OWNS but galadriel CAPTURED (cross-user attribution).
        // Alice re-shares it; her acl_change changes subject/allow only —
        // galadriel's capture attribution must survive (she keeps her read
        // shortcut). Regression guard: the apply path must not clear sender.
        let (dir, tree, pool) = setup_workdir().await;
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice ha un cane di nome Fido".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: Some(Principal::User("galadriel".into())),
            fact_type: Some("bio".into()),
            page_description: None,
            topics: vec!["cane".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"extractions\":[],\
             \"acl_changes\":[{{\"target\":\"{}\",\"subject_id\":null,\
             \"allow_ids\":[\"user:bob\"]}}],\
             \"suggested_seed\":\"Condiviso con bob.\"}}",
            planted.fact_id.as_str()
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(
                "condividi con bob che alice ha un cane di nome Fido",
                "alice",
            ),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        let row = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(
            row.allow_ids,
            vec!["user:bob".parse().unwrap()],
            "the allow-list gained bob"
        );
        assert_eq!(
            row.sender_id,
            Some("user:galadriel".parse().unwrap()),
            "the acl_change preserved the cross-user capture attribution"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn apply_acl_change_refuses_smart_wiki_fact() {
        // A fact alice OWNS but living in a SMART wiki: the chat acl-change
        // verb must refuse — smart-wiki governance is wiki-level, markerless
        // (6j.4). The subject gate would pass (she owns it), so the smart guard
        // is what stops the per-fragment write + the disclosure-audit row.
        let (dir, _, pool) = setup_workdir().await;
        let proj_dir = dir.path().join("wikis/proj");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join("_meta.md"),
            "---\nwiki_id: proj\nwiki_type: wiki-tech\nparent_wiki_id: null\n\
             slug: proj\ntitle: Proj\nacl_default: 'user:alice'\nsmart: true\n---\n",
        )
        .unwrap();
        // Re-open so the tree discovers the new smart wiki.
        let tree = WikiTree::open(dir.path()).unwrap();

        // Seed a fact alice owns in the smart wiki (markerless section row).
        let fid = capture::new_fact_id().unwrap();
        fact_index::insert(
            &pool,
            &fact_index::NewFact {
                subject_external: None,
                fact_id: fid.clone(),
                wiki_id: "proj".into(),
                source_path: "wikis/proj/note.md".into(),
                region_start: None,
                region_end: None,
                text: "il progetto usa Rust".into(),
                embedding: vec![0.0; 8],
                subject_id: Principal::User("alice".into()),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
                authored_refs: Vec::new(),
            },
        )
        .await
        .unwrap();

        let mut hit = sample_recall_hit(fid.as_str());
        hit.wiki_id = "proj".into();
        hit.source_path = "wikis/proj/note.md".into();

        let change = LlmAclChange {
            target: Some(fid.as_str().to_owned()),
            subject_id: None,
            allow_ids: vec!["global".into()],
        };
        let applied = apply_plan_acl_changes(
            &pool,
            &tree,
            std::slice::from_ref(&change),
            std::slice::from_ref(&hit),
            &req("esponi a tutti questo del progetto", "alice"),
        )
        .await;
        assert_eq!(applied, 0, "an acl_change on a smart-wiki fact is refused");

        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert!(
            row.allow_ids.is_empty(),
            "the smart-wiki fact's ACL was left untouched"
        );
        let audits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM disclosure_audit WHERE fact_id = ?")
                .bind(fid.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            audits, 0,
            "no disclosure_audit row written for a refused smart-wiki acl_change"
        );
        drop(dir);
    }

    // ---------- standard-wiki captures route to the buffer ----------

    /// A capture into a NARRATIVE wiki (wiki-user) must land in the captures
    /// buffer (the `capture_buffer` row), NOT in `fact_index` or the
    /// published `.md`. The hourly round is what places it and writes the page.
    #[tokio::test]
    async fn ingest_standard_wiki_buffers_instead_of_writing_md() {
        let (dir, tree, pool) = setup_workdir().await;

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
              \"subject_id\":\"user:alice\",\"body\":\"alice loves pasta\",\
              \"fact_type\":\"preference\",\"topics\":[\"food\"]}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("adoro la pasta", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        let cid = resp.capture_id.expect("capture id surfaced");

        // The capture landed in the BUFFER.
        assert_eq!(capture_buffer::count_buffered(&pool).await.unwrap(), 1);
        let buffered = capture_buffer::find_all_buffered(&pool, 100).await.unwrap();
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].capture_id, cid);
        assert_eq!(buffered[0].body, "alice loves pasta");

        // No direct write: fact_index empty, no page carries a fact marker.
        assert!(
            fact_index::find_active_in_wiki(&pool, "alice")
                .await
                .unwrap()
                .is_empty(),
            "standard-wiki ingest must not insert a fact row"
        );
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            !page.contains("{{subject=") && !page.contains("alice loves pasta"),
            "standard-wiki ingest must not write a marker or the claim into the page"
        );
        assert!(
            !dir.path().join("wikis/alice/_captures.md").exists(),
            "a capture writes no file: it lands in the buffer table and the \
             compiler puts it on a page"
        );

        drop(dir);
    }

    /// **A list item is written now, not at the next dream.**
    ///
    /// Founder, 2026-08-18: *«le liste non ci passano … il classificatore
    /// riceve appositamente tutte le liste che l'utente vede e aggiunge
    /// l'elemento direttamente lì»*. Until that day only a container the user
    /// requested *this turn* took the live path, so **creating** a shopping
    /// list was immediate while **adding to it** waited up to a dream
    /// interval — and the reasoning that keeps the exception alive (a list is
    /// a set; the fresh slot serves a ranked top-3, so six items added at once
    /// lose three) is about lists, not about creation.
    ///
    /// `requested_container` is deliberately **false** here: this is the
    /// everyday case, an item added to a list that already exists.
    #[tokio::test]
    async fn a_list_item_is_written_live_not_buffered() {
        let (dir, tree, pool) = setup_workdir().await;

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"user:alice\",\
              \"body\":\"detersivo\",\"style\":\"lista\",\"target_page\":\"spesa.md\",\
              \"page_description\":\"cosa manca da comprare\",\
              \"requested_container\":false,\"fact_type\":\"other\",\"topics\":[]}]}",
        );
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("aggiungi il detersivo alla lista della spesa", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "a list item must not wait in the buffer"
        );
        let rows = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1, "the item is a fact immediately: {rows:?}");
        assert!(
            rows[0].source_path.ends_with("spesa.md"),
            "and it is on the list the user named: {}",
            rows[0].source_path
        );
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md"))
            .expect("the list page exists now");
        assert!(page.contains("detersivo"), "the item is readable: {page}");

        drop(dir);
    }

    /// **A list item with no list page is refused, not parked.**
    ///
    /// The classifier named a reserved page for a shopping item, so the name
    /// was taken away and nothing is left to file the item on. It must not
    /// land on the buffer (that page holds prose waiting to be placed,
    /// and a list item turns it into a shopping list) and it must not wait in
    /// the buffer either (an hour later there would still be no list). So the
    /// extraction is dropped **and the turn tells the user**: a list is a set,
    /// and one silently missing item is a wrong answer.
    #[tokio::test]
    async fn a_list_item_with_no_page_is_refused_and_the_user_is_told() {
        let (dir, tree, pool) = setup_workdir().await;

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"user:alice\",\
              \"body\":\"detersivo\",\"style\":\"lista\",\"target_page\":\"@rules.md\",\
              \"requested_container\":false,\"fact_type\":\"other\",\"topics\":[]}]}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("aggiungi il detersivo", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "nothing waits — a list item is refused, never parked"
        );
        assert!(
            fact_index::find_active_in_wiki(&pool, "alice")
                .await
                .expect("rows")
                .is_empty(),
            "and nothing was written"
        );
        assert!(
            !dir.path().join("wikis/alice/appunti.md").exists(),
            "above all the buffer was not turned into a shopping list"
        );
        let rules = resp.rules.unwrap_or_default();
        assert!(
            rules.contains("was NOT saved") && rules.contains("reserved"),
            "the turn must tell the user it was not saved: {rules}"
        );

        drop(dir);
    }

    /// The same refusal for the wiki's 33rd list — the product limit
    /// (founder, 2026-08-09) refuses to MINT one more, and now says so.
    #[tokio::test]
    async fn the_33rd_list_is_refused_and_the_user_is_told() {
        let (dir, tree, pool) = setup_workdir().await;
        // 32 lists already on `alice`, each one page with one fact.
        for i in 0..MAX_LIST_PAGES_PER_WIKI {
            capture::wiki_capture(
                &tree,
                &pool,
                fake_embedder(),
                CaptureRequest {
                    subject_external: None,
                    wiki_id: WikiId::parse("alice").unwrap(),
                    page: Some(PathBuf::from(format!("lista_{i}.md"))),
                    body: format!("voce {i}"),
                    subject: "user:alice".parse().unwrap(),
                    allow: Vec::new(),
                    sender: Some("user:alice".parse().unwrap()),
                    fact_type: None,
                    page_description: None,
                    topics: Vec::new(),
                    dedup_threshold: None,
                    valid_from: None,
                    valid_to: None,
                    style: Some(crate::wiki::PageStyle::Lista),
                    salience: None,
                    authored_refs: Vec::new(),
                },
            )
            .await
            .expect("seed list");
        }

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"user:alice\",\
              \"body\":\"detersivo\",\"style\":\"lista\",\"target_page\":\"spesa.md\",\
              \"requested_container\":false,\"fact_type\":\"other\",\"topics\":[]}]}",
        );
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("aggiungi il detersivo alla spesa", "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        assert!(
            !dir.path().join("wikis/alice/spesa.md").exists(),
            "the 33rd list is not minted"
        );
        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "and the item does not wait either"
        );
        let rules = resp.rules.unwrap_or_default();
        assert!(
            rules.contains("was NOT saved") && rules.contains("maximum number of lists"),
            "the turn must tell the user: {rules}"
        );

        drop(dir);
    }

    /// A conversational capture is staged with its vector and with the
    /// fingerprint of the turn it came from, and the vector is the one
    /// promotion uses — the work is done once, at the point where the text is
    /// final, instead of once per read AND again at promotion.
    #[tokio::test]
    async fn ingest_stages_the_vector_and_the_origin_message_on_the_capture() {
        let (dir, tree, pool) = setup_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
            \"subject_id\":\"user:alice\",\"body\":\"Alice beve il caffè amaro.\",\
            \"fact_type\":\"state\",\"salience\":\"high\"}]}";
        // `salience: high` and a kind a card can hold are what make the claim
        // placeable without a Cartografo: the identity card is the only
        // deterministic home since 2026-08-22, and it takes what the
        // classifier reserved (`planner::fact_belongs_on_a_card`). This test
        // is about the staged vector, not placement.
        let llm = FakeLlmBackend::new("fake", json);
        let message = "il caffè lo bevo amaro";
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req(message, "alice"),
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");

        let staged = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert_eq!(staged.len(), 1);
        assert!(
            staged[0].embedding.is_some(),
            "the vector is computed once, when the claim is staged"
        );
        assert_eq!(
            staged[0].origin_message_hash.as_deref(),
            Some(capture_buffer::origin_fingerprint(message).as_str()),
            "and the turn it came from is recorded, so recall can tell whether \
             the agent is already reading it"
        );

        // Promotion copies that vector rather than recomputing: the promoted
        // fact carries exactly what was staged.
        let staged_vec = staged[0].embedding.clone().unwrap();
        promote_buffer(&pool, &tree).await;
        let row = fact_index::find_by_id(&pool, &staged[0].capture_id)
            .await
            .expect("find")
            .expect("promoted");
        assert_eq!(row.embedding, staged_vec);
        drop(dir);
    }

    /// The LIVE exception. A capture the classifier flagged as a REQUESTED
    /// CONTAINER (`requested_container: true`) is written live via the direct
    /// path even into a standard wiki — it lands in `fact_index` + the page
    /// marker immediately, NOT in the buffer (a shopping list cannot wait for
    /// the dream). Inverse of
    /// `ingest_standard_wiki_buffers_instead_of_writing_md`.
    ///
    /// Removing it was proposed on 2026-08-05 and rejected: the fresh slot
    /// makes a buffered claim *recallable*, which is enough for a fact and not
    /// enough for a LIST. The slot is a ranked top-K
    /// (`recall_fresh_top_k`, default 3), so a list that grew by more items
    /// than that inside one dream interval would be served incomplete — and an
    /// incomplete list is a wrong answer, not a partial one. The assertion at
    /// the bottom is that guard: it fails if the exception is removed again.
    #[tokio::test]
    async fn ingest_requested_container_writes_live_into_standard_wiki() {
        let (dir, tree, pool) = setup_workdir().await;

        // The classifier asks for a live container (a shopping list).
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[\
               {\"target_wiki_id\":\"alice\",\"target_page\":\"spesa.md\",\"subject_id\":\"user:alice\",\
                \"body\":\"latte\",\"fact_type\":\"task\",\"style\":\"lista\",\"requested_container\":true}\
             ]}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("aggiungi il latte alla spesa", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(
            resp.capture_id.is_some(),
            "the live write anchors the response"
        );

        // LIVE write: the fact is in fact_index and on the page now — NOT buffered.
        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "a requested container must NOT wait in the buffer"
        );
        let facts = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert_eq!(
            facts.len(),
            1,
            "the requested container is written live to fact_index"
        );
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md")).unwrap();
        assert!(
            page.contains("{{f=") && page.contains("latte"),
            "the requested container's fact is written to its page marker now: {page}"
        );

        drop(dir);
    }

    /// Why the live path is not redundant with the fresh slot: a list added to
    /// faster than the dream drains would be served **incomplete**.
    ///
    /// Buffer more items than `recall_fresh_top_k` and ask for the list. The
    /// slot returns its top K by similarity and the rest are simply absent —
    /// which is the right shape for "the most relevant things memory knows"
    /// and the wrong shape for "everything on the list". The live path exists
    /// so a list never depends on this.
    #[tokio::test]
    async fn the_fresh_slot_cannot_serve_a_whole_list() {
        let (dir, _, pool) = setup_workdir().await;
        for item in ["latte", "pane", "uova", "caffè", "riso"] {
            capture_buffer::buffer_capture(
                &pool,
                crate::capture::CaptureRequest {
                    subject_external: None,
                    authored_refs: Vec::new(),
                    wiki_id: crate::types::WikiId::parse("alice").unwrap(),
                    page: Some(PathBuf::from("spesa.md")),
                    body: item.to_owned(),
                    subject: "user:alice".parse().unwrap(),
                    allow: Vec::new(),
                    sender: None,
                    fact_type: Some("task".to_owned()),
                    topics: vec![],
                    dedup_threshold: None,
                    valid_from: None,
                    valid_to: None,
                    style: Some(crate::wiki::PageStyle::Lista),
                    page_description: None,
                    salience: None,
                },
                None,
            )
            .await
            .expect("buffer");
        }
        let policy = IngestPolicy::default();
        let fresh = recall::recall_fresh_captures(
            &pool,
            fake_embedder().as_ref(),
            "cosa c'è sulla lista della spesa",
            &SenderContext::user("alice"),
            policy.recall_fresh_top_k,
            &std::collections::HashSet::new(),
        )
        .await
        .expect("fresh recall");
        assert_eq!(
            fresh.len(),
            policy.recall_fresh_top_k,
            "the slot is a ranked top-K, so it caps at its size"
        );
        assert!(
            fresh.len() < 5,
            "two of the five items are not offered at all — which is why a \
             requested container is written live instead of buffered"
        );
        drop(dir);
    }

    // ---------- agent-authored memory ----------

    /// The capstone of agent-authored memory: when the consumer feeds the
    /// agent's OWN reply back with `author: assistant`, a fact derived from
    /// that reply is filed with `sender = <the agent>` (resolved from the
    /// consumer binding), while its `subject` stays the user the agent was
    /// talking to — so the synthesis lands in the user's wiki yet carries the
    /// agent's provenance. This is the INPS case: the deadline lived only in
    /// the agent's reply, and now it persists. `requested_container` forces the
    /// live path so the fact is readable in `fact_index` (the buffered path
    /// flips the sender identically; this only makes it assertable).
    #[tokio::test]
    async fn ingest_assistant_turn_attributes_derived_fact_to_the_agent() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"inps.md\",\"subject_id\":\"user:alice\",\
            \"body\":\"Dalla lettera INPS caricata dall'utente, la scadenza per inviare il provvedimento è il 27 giugno 2026.\",\
            \"fact_type\":\"plan\",\"style\":\"prosa-tecnica\",\"requested_container\":true}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer(
                    "Ho letto la lettera INPS: la scadenza per inviare il provvedimento è il 27 giugno 2026.",
                    "alice",
                    "botdeploy",
                )
            },
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        let facts = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert_eq!(
            facts.len(),
            1,
            "the agent's derived fact is filed in the user's wiki"
        );
        assert_eq!(
            facts[0].subject_id,
            Principal::User("alice".into()),
            "subject stays the USER the fact is about — it surfaces on alice's recall"
        );
        assert_eq!(
            facts[0].sender_id,
            Some(Principal::User("samvisebot".into())),
            "sender is flipped to the AGENT: the fact is agent-derived provenance, not a user assertion"
        );
        drop(dir);
    }

    /// The control: the SAME consumer binding on a normal user turn stamps NO
    /// agent provenance. subject==sender (both the user) so the capture path
    /// normalises `sender_id` to `None` — exactly as today. The agent
    /// provenance is gated strictly on `author: assistant`; a consumer id alone
    /// never flips it (contrast the assistant-turn test, which yields
    /// `Some(samvisebot)`).
    #[tokio::test]
    async fn ingest_user_turn_stamps_no_agent_provenance_even_with_a_consumer() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"spesa.md\",\"subject_id\":\"user:alice\",\
            \"body\":\"latte\",\"fact_type\":\"task\",\"style\":\"lista\",\"requested_container\":true}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer("aggiungi il latte alla spesa", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("ingest");

        let facts = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].sender_id,
            Some(Principal::User("alice".into())),
            "a user turn stamps no agent provenance — sender is materialized to the subject (the user), never the agent"
        );
        drop(dir);
    }

    /// Graceful fallback: `author: assistant` with NO consumer binding (a smart
    /// consumer IS its user) resolves no distinct agent principal, so
    /// attribution is unchanged from a user turn — sender stays materialized to
    /// the subject (the user), never an agent. The assistant pass simply no-ops on
    /// attribution rather than dropping the fact or inventing a provenance.
    #[tokio::test]
    async fn ingest_assistant_turn_without_consumer_stamps_no_agent_provenance() {
        let (dir, tree, pool) = setup_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"note.md\",\"subject_id\":\"user:alice\",\
            \"body\":\"Promemoria sintetizzato dall'agente.\",\"fact_type\":\"plan\",\
            \"style\":\"prosa-tecnica\",\"requested_container\":true}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req("la mia risposta sintetica", "alice")
            },
            &policy,
        )
        .await
        .expect("ingest");

        let facts = fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].sender_id,
            Some(Principal::User("alice".into())),
            "no consumer binding ⇒ no distinct agent ⇒ sender materialized to the subject (the user), never an agent"
        );
        drop(dir);
    }

    /// `build_prompt` arms Part 9 only on an assistant-authored turn: the
    /// `author: assistant` line + the Part 9 pointer appear for the agent's
    /// own reply, and the 99% user path stays byte-clean (no `author:` line).
    #[test]
    fn build_prompt_marks_assistant_authored_turn_only() {
        let policy = IngestPolicy::default();
        let user_req = req("ciao", "alice");
        let user_prompt = build_prompt(
            &user_req,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            !user_prompt.contains("author:"),
            "a user turn injects no author line — the default path is untouched"
        );

        let asst_req = IngestRequest {
            author: MessageRole::Assistant,
            ..req("la mia risposta", "alice")
        };
        let asst_prompt = build_prompt(
            &asst_req,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[],
            now_fixture(),
            &policy,
        );
        assert!(
            asst_prompt.contains("author: assistant"),
            "an assistant turn injects the author line"
        );
        // The rules themselves have to BE there, and only on this turn. They
        // used to live in the trunk with the turn pointing at them by section
        // number, and the pointer went stale on a renumbering while a test
        // comparing the literal with itself stayed green. There is no pointer
        // to go stale now: what the turn says about itself is either in the
        // turn or nowhere.
        let regole = prompts::render(
            "ingest-assistant-turn",
            std::path::Path::new("/nonexistent"),
            BUNDLED_INGEST_ASSISTANT_TURN_MD,
            &[],
        )
        .expect("the part parses");
        let con_regole = build_prompt(
            &asst_req,
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &crate::locale::render_memory_language_directive(Some("it-IT")),
            &[regole.as_str()],
            now_fixture(),
            &policy,
        );
        assert!(
            con_regole.contains("durable sediment"),
            "the own-turn rules ride the turn that uses them"
        );
        assert!(
            !asst_prompt.contains("durable sediment"),
            "and nothing of them is left in the turn that does not"
        );
        assert!(
            !BUNDLED_INGEST_PROMPT_MD.contains("durable sediment"),
            "nor in the trunk every ordinary turn carries"
        );
    }

    /// The self side. An assistant turn with `subject_id: "self"`
    /// files the fact into the AGENT's OWN wiki, owned by the agent and tagged
    /// with the served user (so the read side can scope "history with THIS
    /// user"), while the user's wiki stays untouched. The model's
    /// `target_wiki_id` (here deliberately the user's) is ignored — the engine
    /// knows the agent's wiki. This is the agent's emergent self.
    #[tokio::test]
    async fn ingest_assistant_turn_subject_self_files_into_the_agent_wiki() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"self\",\
            \"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
            \"fact_type\":\"episode\",\"salience\":\"normal\"}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho aiutato Alice con l'INPS", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        let agent_facts = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            agent_facts.len(),
            1,
            "the self-fact is filed in the agent's own wiki"
        );
        assert_eq!(
            agent_facts[0].subject_id,
            Principal::User("samvisebot".into()),
            "owned by the AGENT — its own self-knowledge, not about the user"
        );
        assert!(
            agent_facts[0].topics.contains(&"alice".to_owned()),
            "auto-tagged with the served user, so the read side can scope per-user"
        );
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            0,
            "subject_id=self never lands in the user's wiki, even when target_wiki_id names it"
        );
        drop(dir);
    }

    /// The sentinel's OTHER spelling. A model that knows its own principal
    /// writes `subject_id: "user:<agent>"` where Part 9 asks for `self` — the
    /// identical claim, "this fact is about me". Matching only the literal
    /// lets the spelled-out form fall through to the normal path, and the
    /// agent's diary entry lands in whichever wiki `target_wiki_id` names —
    /// forty such facts on the live deployment (2026-07-28). Both spellings
    /// must be the same route: agent's wiki, owned by the agent, tagged with
    /// the served user, user's wiki untouched.
    #[tokio::test]
    async fn ingest_assistant_turn_subject_spelled_as_the_agent_files_into_the_agent_wiki() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\"subject_id\":\"user:samvisebot\",\
            \"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
            \"fact_type\":\"episode\",\"salience\":\"normal\"}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho aiutato Alice con l'INPS", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        let agent_facts = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            agent_facts.len(),
            1,
            "subject spelled as the agent's own principal routes like `self`"
        );
        assert_eq!(
            agent_facts[0].subject_id,
            Principal::User("samvisebot".into()),
            "owned by the AGENT — its own self-knowledge, not about the user"
        );
        assert!(
            agent_facts[0].topics.contains(&"alice".to_owned()),
            "and gets the same served-user auto-tag the `self` spelling gets"
        );
        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "alice")
                .await
                .unwrap(),
            0,
            "the diary entry no longer lands in the user's wiki"
        );
        drop(dir);
    }

    /// The boundary the alias must not cross. On a USER turn `agent_sender` is
    /// `None` by construction, so `subject_id: "user:<agent>"` keeps its ordinary
    /// meaning — a fact stated on the USER's turn that happens to be owned by
    /// the agent principal — and stays on the normal path. The self path
    /// short-circuits ahead of every capture validator and ignores
    /// `target_wiki_id`, so its absence is decisive on its own: had the alias
    /// fired, this extraction would sit in the agent's wiki whatever the rest
    /// of the pipeline decided. It does not.
    #[tokio::test]
    async fn ingest_user_turn_subject_naming_the_agent_is_not_a_self_fact() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
            \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
            \"subject_id\":\"user:samvisebot\",\
            \"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
            \"fact_type\":\"episode\",\"salience\":\"normal\"}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req_consumer(
                "ho parlato con l'agente della pratica INPS",
                "alice",
                "botdeploy",
            ),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(
            fact_index::count_active_in_wiki(&pool, "samvisebot")
                .await
                .unwrap(),
            0,
            "the alias must not fire on a user turn — nothing is diverted into the agent's wiki"
        );
        drop(dir);
    }

    /// The read side closes the loop. Two assistant turns seed the
    /// agent's self (a high-salience IDENTITY fact, untagged; a normal
    /// RELATIONSHIP fact, tagged with the user). A later USER turn's recall block
    /// then carries both, leading: WHO YOU ARE (always) + YOUR HISTORY WITH THIS
    /// USER (scoped). The agent composes its reply conscious of itself.
    #[tokio::test]
    async fn ingest_injects_agent_self_context_into_the_recall_block() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let policy = IngestPolicy::default();

        // Seed an IDENTITY fact (high salience ⇒ untagged, always-on).
        let identity_llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"preferenze.md\",\"body\":\"L'agente è l'assistente della famiglia di Franz.\",\
              \"fact_type\":\"bio\",\"salience\":\"high\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &identity_llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("chi sono io", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("seed identity");

        // Seed a RELATIONSHIP fact (normal salience ⇒ tagged with alice).
        let rel_llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"diario.md\",\"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
              \"fact_type\":\"episode\",\"salience\":\"normal\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &rel_llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho aiutato Alice", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("seed relationship");

        // The identity fact named no page, so it waits in the queue; the
        // relationship one named `esperienze_alice.md` and is already filed.
        promote_buffer(&pool, &tree).await;

        // A USER turn: the recall block now carries the agent's self-context.
        let user_llm =
            FakeLlmBackend::new("fake", "{\"intent\":\"recall\",\"context_snippet\":\"\"}");
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &user_llm,
            None,
            req_consumer("cosa sai?", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("user turn");

        let block = resp.context_snippet.unwrap_or_default();
        assert!(
            block.contains(HDR_WHO_YOU_ARE),
            "identity section present: {block}"
        );
        assert!(
            block.contains("assistente della famiglia di Franz"),
            "the identity fact is injected: {block}"
        );
        assert!(
            block.contains(HDR_YOUR_HISTORY),
            "history section present: {block}"
        );
        assert!(
            block.contains("aiutato Alice con la pratica INPS"),
            "the relationship fact is injected: {block}"
        );
        assert!(
            block.find(HDR_WHO_YOU_ARE).unwrap() < block.find(HDR_YOUR_HISTORY).unwrap(),
            "identity leads the history: {block}"
        );
        drop(dir);
    }

    /// `recall_agent_self` surfaces the agent's self-facts
    /// every turn, so it must also bump their recall hit counters like the
    /// normal recall path — otherwise self-memory reads as never-used forever
    /// and recall-weighted REM treats the whole agent wiki as cold.
    #[tokio::test]
    async fn ingest_agent_self_recall_bumps_hit_counters() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let policy = IngestPolicy::default();

        // Seed a relationship self-fact (tagged with the served user).
        let seed_llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"diario.md\",\"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
              \"fact_type\":\"episode\",\"salience\":\"normal\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &seed_llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho aiutato Alice", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("seed relationship");

        let before: i64 = sqlx::query_scalar(
            "SELECT recall_count_30d FROM fact_index \
             WHERE \"text\" LIKE '%pratica INPS%' AND superseded_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        // A user turn surfaces the agent's self-memory into the recall block.
        let user_llm =
            FakeLlmBackend::new("fake", "{\"intent\":\"recall\",\"context_snippet\":\"\"}");
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &user_llm,
            None,
            req_consumer("cosa sai?", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("user turn");

        let after: i64 = sqlx::query_scalar(
            "SELECT recall_count_30d FROM fact_index \
             WHERE \"text\" LIKE '%pratica INPS%' AND superseded_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            after > before,
            "surfacing the self-fact must bump its recall hit counter (before={before}, after={after})"
        );
        let stamped: Option<String> = sqlx::query_scalar(
            "SELECT last_recall_at FROM fact_index \
             WHERE \"text\" LIKE '%pratica INPS%' AND superseded_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(stamped.is_some(), "last_recall_at must be stamped");
        drop(dir);
    }

    /// Write the agent's bio fact straight onto a page of its own wiki, the
    /// shape the placement pass produces once it has decided. Extracted so
    /// the test above stays about the recall block rather than the request.
    async fn plant_agent_bio(tree: &WikiTree, pool: &SqlitePool) {
        capture::wiki_capture(
            tree,
            pool,
            fake_embedder(),
            CaptureRequest {
                subject_external: None,
                wiki_id: WikiId::parse("samvisebot").unwrap(),
                page: Some(PathBuf::from("preferenze.md")),
                body: "L'agente parla italiano e inglese.".to_owned(),
                subject: Principal::User("samvisebot".to_owned()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("bio".to_owned()),
                topics: Vec::new(),
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: Some("normal".to_owned()),
                authored_refs: Vec::new(),
            },
        )
        .await
        .expect("plant the bio fact");
    }

    /// Roadmap 41c + the `WHO IS SPEAKING` section: `WHO YOU ARE` opens with
    /// the agent wiki's `_meta.summary` line and a `bio`-typed self-fact
    /// joins the identity bucket regardless of salience; the sender's own
    /// wiki summary renders as their identity card.
    #[tokio::test]
    async fn ingest_block_opens_with_wiki_summaries_and_bio_identity() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        let policy = IngestPolicy::default();
        // Give both wikis the one-line abstract the abstract sync maintains.
        std::fs::write(
            dir.path().join("wikis/samvisebot/_meta.md"),
            "---\nwiki_id: samvisebot\nwiki_type: wiki-user\nslug: samvisebot\n\
             title: samvisebot\nacl_default: 'user:samvisebot'\n\
             summary: 'The household assistant of the Rossi family.'\n---\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wikis/alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\n\
             title: Alice\nacl_default: 'user:alice'\n\
             summary: 'Alice Rossi, the family cook.'\n---\n",
        )
        .unwrap();

        // A bio self-fact with NORMAL salience: identity by fact_type alone.
        let bio_llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"preferenze.md\",\"body\":\"L'agente parla italiano e inglese.\",\
              \"fact_type\":\"bio\",\"salience\":\"normal\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &bio_llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("parlo italiano e inglese", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("seed bio fact");

        // An identity self-fact names no page, so the turn leaves it in the
        // queue: it is the placement pass, reading the whole memory, that
        // decides where an agent's autobiography goes.
        let queued = crate::capture_buffer::find_all_buffered(&pool, 10)
            .await
            .expect("queue");
        assert_eq!(queued.len(), 1, "the bio self-fact waits: {queued:?}");
        // Identity is user-agnostic: it must NOT carry the partner tag.
        assert!(
            !queued[0].topics.contains(&"alice".to_owned()),
            "a bio identity fact stays untagged: {:?}",
            queued[0].topics
        );
        // Written onto a page, so the read side has something to bucket.
        plant_agent_bio(&tree, &pool).await;

        let user_llm = FakeLlmBackend::new("fake", "{\"intent\":\"recall\"}");
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &user_llm,
            None,
            req_consumer("cosa sai?", "alice", "botdeploy"),
            &policy,
        )
        .await
        .expect("user turn");
        let block = resp.context_snippet.expect("block present");
        assert!(block.starts_with(HDR_WHO_YOU_ARE), "{block}");
        assert!(
            block.contains("- The household assistant of the Rossi family."),
            "the agent wiki summary leads WHO YOU ARE: {block}"
        );
        assert!(
            block.contains("- L'agente parla italiano e inglese."),
            "the bio fact joins the identity bucket: {block}"
        );
        assert!(
            block.contains(&format!(
                "{HDR_WHO_IS_SPEAKING}\n- alice — Alice Rossi, the family cook."
            )),
            "the sender's card is their wiki summary line: {block}"
        );
        assert!(
            block.find(HDR_WHO_YOU_ARE).unwrap() < block.find(HDR_WHO_IS_SPEAKING).unwrap(),
            "canonical order: {block}"
        );
        drop(dir);
    }

    /// The partner tag is exclusive: on an agent self-fact,
    /// another enrolled user's id in the classifier's `topics` is a mere
    /// mention and is stripped, so the mentioned user's turns never inherit
    /// someone else's history. Content tags survive.
    #[tokio::test]
    async fn ingest_self_fact_strips_other_enrolled_users_from_topics() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('bob', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let policy = IngestPolicy::default();

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"diario.md\",\"body\":\"L'agente ha consigliato Alice su Bob.\",\
              \"fact_type\":\"episode\",\"salience\":\"normal\",\
              \"topics\":[\"consigli\",\"bob\"]}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho consigliato Alice su Bob", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("assistant turn");

        let agent_facts = fact_index::find_by_filters(
            &pool,
            &fact_index::FactFilters {
                wiki_id: Some("samvisebot".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(agent_facts.len(), 1);
        let topics = &agent_facts[0].topics;
        assert!(topics.contains(&"consigli".to_owned()), "{topics:?}");
        assert!(
            topics.contains(&"alice".to_owned()),
            "the served user is the partner tag: {topics:?}"
        );
        assert!(
            !topics.contains(&"bob".to_owned()),
            "a mentioned enrolled user is stripped: {topics:?}"
        );
        drop(dir);
    }

    /// The relationship slot is scoped by the served sender — one user's history
    /// never leaks into another's turn. Seed alice's relationship fact, then
    /// serve bob: his block must not carry alice's history.
    #[tokio::test]
    async fn ingest_agent_self_relationship_is_scoped_per_user() {
        let (dir, tree, pool) = setup_agent_workdir().await;
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('bob', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let policy = IngestPolicy::default();

        let rel_llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[{\"subject_id\":\"self\",\
              \"target_page\":\"diario.md\",\"body\":\"L'agente ha aiutato Alice con la pratica INPS.\",\
              \"fact_type\":\"episode\",\"salience\":\"normal\"}]}",
        );
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &rel_llm,
            None,
            IngestRequest {
                author: MessageRole::Assistant,
                ..req_consumer("ho aiutato Alice", "alice", "botdeploy")
            },
            &policy,
        )
        .await
        .expect("seed alice relationship");

        let bob_llm =
            FakeLlmBackend::new("fake", "{\"intent\":\"recall\",\"context_snippet\":\"\"}");
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &bob_llm,
            None,
            req_consumer("ciao", "bob", "botdeploy"),
            &policy,
        )
        .await
        .expect("bob turn");

        let block = resp.context_snippet.unwrap_or_default();
        assert!(
            !block.contains("aiutato Alice"),
            "alice's relationship must NOT surface in bob's turn: {block}"
        );
        drop(dir);
    }

    /// A single turn that states several facts is split into multiple
    /// `extractions`; the router files each one, so a multi-fact message no
    /// longer loses everything past the first claim. Into a standard wiki,
    /// each extraction lands as its own buffered capture.
    #[tokio::test]
    async fn ingest_multi_fact_extractions_each_buffered() {
        let (dir, tree, pool) = setup_workdir().await;

        // The classifier returns three atomic facts in `extractions`.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[\
               {\"target_wiki_id\":\"alice\",\"subject_id\":\"user:alice\",\"body\":\"Alice loves pasta\",\"fact_type\":\"preference\"},\
               {\"target_wiki_id\":\"alice\",\"subject_id\":\"user:alice\",\"body\":\"Alice runs every morning\",\"fact_type\":\"bio\"},\
               {\"target_wiki_id\":\"alice\",\"subject_id\":\"user:alice\",\"body\":\"Alice dislikes loud music\",\"fact_type\":\"preference\"}\
             ]}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("pasta, corsa al mattino, niente musica alta", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(resp.capture_id.is_some(), "first fact anchors the response");
        // All three facts were buffered (not just the first).
        let buffered = capture_buffer::find_all_buffered(&pool, 100).await.unwrap();
        assert_eq!(buffered.len(), 3, "every extraction must be filed");
        let bodies: Vec<&str> = buffered.iter().map(|c| c.body.as_str()).collect();
        assert!(bodies.contains(&"Alice loves pasta"));
        assert!(bodies.contains(&"Alice runs every morning"));
        assert!(bodies.contains(&"Alice dislikes loud music"));

        drop(dir);
    }

    /// An atomic message yields a single-element
    /// `extractions` array — the canonical shape, not a fallback to the
    /// legacy top-level fields. Exactly one fact is filed, with the
    /// per-extraction subject honoured. Locks in "a message that states one
    /// thing produces an array with ONE element".
    #[tokio::test]
    async fn ingest_single_atomic_fact_files_one_via_array() {
        let (dir, tree, pool) = setup_workdir().await;

        // Atomic message → an array with ONE element (not top-level fields).
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[\
               {\"target_wiki_id\":\"alice\",\"subject_id\":\"user:alice\",\"body\":\"Alice vive a Bologna\",\"fact_type\":\"bio\",\"topics\":[\"bologna\"]}\
             ]}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("vivo a Bologna", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(
            resp.capture_id.is_some(),
            "the single fact anchors the response"
        );
        let buffered = capture_buffer::find_all_buffered(&pool, 100).await.unwrap();
        assert_eq!(
            buffered.len(),
            1,
            "a one-element array files exactly one fact"
        );
        assert_eq!(buffered[0].body, "Alice vive a Bologna");

        drop(dir);
    }

    /// "nothing memorable → empty array". If the
    /// model returns `capture` with an empty `extractions` array (and no
    /// legacy top-level body/target), there is nothing to file — the
    /// orchestrator demotes the turn to a skip with the canned seed rather
    /// than writing an empty fact.
    #[tokio::test]
    async fn ingest_capture_with_empty_extractions_still_reads() {
        let (dir, tree, pool) = setup_workdir().await;

        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"extractions\":[],\"suggested_seed\":\"Ok.\"}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("uhm, vediamo", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(
            resp.intent,
            IntentKind::Capture,
            "a capture that filed nothing still reads: the gesture it carries \
             (\"forget the greenhouse\") is reconciled on the reading side, and \
             returning early here would answer out of an empty context"
        );
        assert!(resp.capture_id.is_none(), "no fact written");
        assert!(resp.llm_used, "the LLM did respond, just with no facts");

        drop(dir);
    }

    /// Anti-hallucination at the e2e level: the LLM can hand the
    /// orchestrator a well-formed-looking `fact_id` that does not match
    /// any row in `recalled_memory`. The orchestrator must refuse to
    /// dispatch it (no DB write, no skip-because-LLM-failed log), and
    /// the recalled row must remain active.
    #[tokio::test]
    async fn ingest_supersede_target_not_in_recall_demotes_to_skip() {
        let (dir, tree, pool) = setup_workdir().await;
        // Plant a row so recall has something to surface — but the
        // LLM's supersede_target will name a *different*, unseen id.
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice prefers coffee black".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("preference".into()),
            topics: vec!["coffee".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let planted = capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        // Well-formed UUIDv7 the LLM hallucinated — never seen in recall.
        let hallucinated = "018f9999-9999-7999-9999-999999999999";
        let llm_resp = format!(
            "{{\"intent\":\"capture\",\"target_wiki_id\":\"alice\",\
             \"target_page\":\"preferenze.md\",\"subject_id\":\"user:alice\",\
             \"body\":\"alice now prefers tea\",\
             \"fact_type\":\"preference\",\"topics\":[\"tea\"],\
             \"supersede_target\":\"{hallucinated}\",\
             \"suggested_seed\":\"Aggiornato.\"}}",
        );
        let llm = FakeLlmBackend::new("fake", &llm_resp);
        let policy = IngestPolicy::default();

        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ora preferisco il tè", "alice"),
            &policy,
        )
        .await
        .expect("ingest");

        assert_eq!(
            resp.intent,
            IntentKind::Skip,
            "hallucinated supersede_target must demote to skip"
        );
        assert!(
            resp.capture_id.is_none(),
            "no row must be written when supersede_target is rejected"
        );
        assert!(resp.llm_used, "LLM did respond, just with an invalid plan");

        // The planted row is untouched.
        let still_active = fact_index::find_by_id(&pool, &planted.fact_id)
            .await
            .expect("find planted")
            .expect("planted row exists");
        assert!(
            still_active.superseded_at.is_none(),
            "the recalled row must remain active"
        );

        drop(dir);
    }

    #[tokio::test]
    async fn ingest_recall_intent_surfaces_snippet_no_write() {
        let (dir, tree, pool) = setup_workdir().await;
        // First, plant a captured fact directly so recall has something to find.
        let _wiki = WikiSlug::parse("alice").unwrap();
        let cap_req = CaptureRequest {
            subject_external: None,
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice").unwrap(),
            page: Some(PathBuf::from("cucina.md")),
            body: "alice prefers coffee black".into(),
            subject: Principal::User("alice".into()),
            allow: Vec::new(),
            sender: None,
            fact_type: Some("preference".into()),
            page_description: None,
            topics: vec!["coffee".into()],
            dedup_threshold: Some(0.99),
            valid_from: None,
            valid_to: None,
            style: None,
            salience: None,
        };
        capture::wiki_capture(&tree, &pool, fake_embedder(), cap_req)
            .await
            .expect("plant");

        // The classifier's own `context_snippet` is a sentinel that MUST be
        // dropped: the flat recall slot is the deterministic hit-list, never an
        // LLM recap (which the classifier writes pre-navigation and could turn
        // into a false negative). Only the recalled fact — surfaced from step-1
        // flat recall — may appear. The classifier still routes intent + seed.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"recall\",\"context_snippet\":\"CLASSIFIER RECAP MUST BE IGNORED\",\"suggested_seed\":\"You like coffee black.\"}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("how do I like my coffee?", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Recall);
        assert!(resp.capture_id.is_none());
        assert_eq!(
            resp.suggested_seed.as_deref(),
            Some("You like coffee black.")
        );
        let block = resp.context_snippet.expect("flat recall slot present");
        assert!(
            block.contains("alice prefers coffee black"),
            "the recalled fact surfaces as a deterministic bullet: {block}"
        );
        assert!(
            !block.contains("CLASSIFIER RECAP MUST BE IGNORED"),
            "the classifier's recap prose must never leak into the block: {block}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_structural_intent_returns_dashboard_seed() {
        let (dir, tree, pool) = setup_workdir().await;
        // LLM returns structural intent with no seed — orchestrator
        // should fill the canned dashboard hint.
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"structural\"}");
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("create a wiki for recipes", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Structural);
        assert!(resp.capture_id.is_none());
        assert_eq!(
            resp.suggested_seed,
            Some(IngestPolicy::default().structural_suggested_seed)
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_invalid_capture_plan_demotes_to_skip() {
        let (dir, tree, pool) = setup_workdir().await;
        // A capture whose `subject_id` is not a principal at all: the plan
        // cannot be validated, so the turn demotes to skip with the fallback
        // seed. (A missing `target_wiki_id` does not qualify: that is the
        // normal shape, and the wiki is derived from the subject.)
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"intent\":\"capture\",\"body\":\"orphan fact\",\"subject_id\":\"not a principal\"}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("orphan fact", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert!(resp.capture_id.is_none());
        assert_eq!(
            resp.suggested_seed.as_deref(),
            Some(IngestPolicy::default().degraded_suggested_seed.as_str())
        );
        assert!(resp.llm_used);
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_unparseable_llm_response_demotes_to_skip() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "I refuse to follow your JSON schema, sorry.");
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("anything", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert!(resp.capture_id.is_none());
        assert!(resp.llm_used, "LLM did respond, just unparseably");
        drop(dir);
    }

    // A backend that always errors transport — for the LLM-unavailable path.
    struct FailingLlm;
    #[async_trait]
    impl LlmBackend for FailingLlm {
        fn model_id(&self) -> &'static str {
            "failing"
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> std::result::Result<crate::llm::CompletionResponse, LlmError> {
            Err(LlmError::Transport("simulated outage".into()))
        }
    }

    #[tokio::test]
    async fn ingest_llm_unavailable_returns_canned_skip() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FailingLlm;
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("hello?", "alice"),
            &policy,
        )
        .await
        .expect("ingest must not bubble LLM transport errors");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert!(!resp.llm_used, "fallback path because LLM transport failed");
        assert_eq!(
            resp.suggested_seed.as_deref(),
            Some(IngestPolicy::default().degraded_suggested_seed.as_str())
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_dashboard_command_uses_structural_seed_on_fallback() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FailingLlm;
        let policy = IngestPolicy::default();
        let mut request = req("anything", "alice");
        request.context_hint = ContextHint::DashboardCommand;
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert_eq!(
            resp.suggested_seed,
            Some(IngestPolicy::default().structural_suggested_seed)
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_finish_reason_does_not_change_parse_outcome() {
        // Whether the model truncated at max_tokens or not, the JSON
        // either parses or it doesn't — finish_reason is irrelevant.
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}")
            .with_finish_reason(FinishReason::MaxTokens);
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("hi", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        drop(dir);
    }

    // ---------- meta sanity ----------

    #[test]
    fn intent_kind_wire_strings_are_canonical() {
        assert_eq!(IntentKind::Capture.as_str(), "capture");
        assert_eq!(IntentKind::Recall.as_str(), "recall");
        assert_eq!(IntentKind::Structural.as_str(), "structural");
        assert_eq!(IntentKind::Skip.as_str(), "skip");
    }

    #[test]
    fn context_hint_wire_strings_match_spec() {
        assert_eq!(ContextHint::Conversation.as_str(), "conversation");
        assert_eq!(ContextHint::DashboardCommand.as_str(), "dashboard_command");
        assert_eq!(ContextHint::Import.as_str(), "import");
    }

    #[test]
    fn message_role_wire_strings_match_spec() {
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
    }

    #[test]
    fn ingest_policy_default_uses_recall_dedup_threshold() {
        let p = IngestPolicy::default();
        assert!((p.dedup_threshold - DEFAULT_DEDUP_THRESHOLD).abs() < 1e-6);
    }

    // Re-test of WikiMeta to ensure setup_workdir's serialized YAML
    // round-trips correctly. The legacy `acl_default` line is read and
    // ignored: the owning principal is derived from the topology.
    #[test]
    fn setup_workdir_yaml_parses_back_to_wikimeta() {
        let raw = "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\nparent_wiki_id: root\n---\n";
        let (meta, _body) = WikiMeta::parse(Path::new("_meta.md"), raw).expect("parse");
        assert_eq!(meta.wiki_id.as_str(), "alice");
        assert_eq!(
            meta.parent_wiki_id.as_ref().map(WikiId::as_str),
            Some("root")
        );
        // `acl_default` is ignored on read and never re-emitted.
        assert!(meta.scope.is_none());
        assert!(!meta.to_yaml().expect("to_yaml").contains("acl_default"));
    }

    // ---------- locale plumbing end-to-end ----------

    /// `metadata.locale` from the `IngestRequest` wins over every
    /// fallback and ends up substituted into the `{locale}`
    /// placeholder of the bundled prompt. The fake LLM records the
    /// system prompt verbatim so we can read the directive back.
    #[tokio::test]
    async fn ingest_renders_metadata_locale_into_system_prompt() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let request = IngestRequest {
            text: "ciao".to_owned(),
            author: MessageRole::User,
            sender_id: "alice".to_owned(),
            consumer_id: None,
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata {
                locale: Some("it-IT".to_owned()),
                occurred_at: None,
                ..Default::default()
            },
            attachments: Vec::new(),
        };
        wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        let system = llm
            .last_system_prompt()
            .expect("ingest must have called complete with a system prompt");
        assert!(
            system.contains("User locale: it-IT"),
            "metadata.locale must appear in the system prompt: {system}"
        );
        assert!(
            system.contains("Respond in Italian"),
            "language name must be derived from the BCP-47 primary subtag: {system}"
        );
        drop(dir);
    }

    /// The classifier declares its system half a cacheable prefix.
    ///
    /// It is the engine's heaviest repeated caller by a wide margin —
    /// the 2026-07-29 corpus rebuild spent 4.84M prompt tokens across
    /// 174 ingest calls against 56k of output — and the system half is
    /// `ingest.md` with only `{locale}` substituted, so it repeats
    /// verbatim while everything per-turn rides in the user half. That
    /// rebuild ran with `cached_prompt_tokens` at zero on every single
    /// call, because this was the one hot caller never marked.
    ///
    /// Nothing in the response reveals the flag, so dropping it would
    /// cost money silently and no test would notice. Hence this one.
    #[tokio::test]
    async fn ingest_marks_its_system_prompt_cacheable() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let request = IngestRequest {
            text: "the boiler engineer comes on Thursday".to_owned(),
            author: MessageRole::User,
            sender_id: "alice".to_owned(),
            consumer_id: None,
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata::default(),
            attachments: Vec::new(),
        };
        wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert!(
            llm.last_cache_system(),
            "the classifier's system prompt must be marked as a cacheable prefix"
        );
        drop(dir);
    }

    /// `metadata.occurred_at` is the turn's semantic clock: the
    /// classifier's `current_time:` anchor must carry the utterance
    /// instant (a backlog replay re-lives the turn at that time), not
    /// the server clock.
    #[tokio::test]
    async fn ingest_occurred_at_drives_the_current_time_anchor() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let occurred = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:30:00Z")
            .expect("fixture timestamp")
            .with_timezone(&chrono::Utc);
        let request = IngestRequest {
            text: "domani ho il dentista alle 9".to_owned(),
            author: MessageRole::User,
            sender_id: "alice".to_owned(),
            consumer_id: None,
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata {
                locale: None,
                occurred_at: Some(occurred),
                ..Default::default()
            },
            attachments: Vec::new(),
        };
        wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        let prompt = llm
            .last_prompt()
            .expect("ingest must have called complete with a user prompt");
        assert!(
            prompt.contains("current_time: 2026-04-24T09:30:00Z ("),
            "occurred_at must be the current_time anchor: {prompt}"
        );
        drop(dir);
    }

    /// When `metadata.locale` is unset the orchestrator falls back to
    /// `enrollment_users.locale`. The directive uses that per-user
    /// default — proving the second link of the chain works.
    #[tokio::test]
    async fn ingest_falls_back_to_enrollment_locale() {
        let (dir, tree, pool) = setup_workdir().await;
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: vec![crate::enrollment::UserEntry {
                id: "alice".to_owned(),
                aliases: Vec::new(),
                is_admin: false,
                locale: Some("en-US".to_owned()),
                timezone: None,
            }],
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("hello", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let system = llm
            .last_system_prompt()
            .expect("ingest must have called complete with a system prompt");
        assert!(
            system.contains("User locale: en-US"),
            "enrollment locale must back-fill the directive: {system}"
        );
        assert!(
            system.contains("Respond in English"),
            "expected English language name from en-US: {system}"
        );
        drop(dir);
    }

    /// **Conflict resolution**: when **both** `metadata.locale` and
    /// `enrollment_users.locale` are set the orchestrator MUST pick
    /// the per-call metadata, not the per-user default. This is the
    /// load-bearing locale-precedence invariant — a consumer that explicitly
    /// labels a single ingest call as "this turn happens in Spanish"
    /// gets Spanish even when the user's enrollment default is
    /// Italian. Without this guarantee a multi-locale consumer
    /// (e.g. a chat with code switching) could never override the
    /// default for an individual turn. Closes a project audit which had
    /// flagged a missing end-to-end assertion for the precedence rule.
    #[tokio::test]
    async fn ingest_metadata_locale_wins_over_enrollment_locale_when_both_set() {
        let (dir, tree, pool) = setup_workdir().await;
        // Seed the enrollment default to Italian (it-IT).
        let file = crate::enrollment::EnrollmentFile {
            version: 1,
            users: vec![crate::enrollment::UserEntry {
                id: "alice".to_owned(),
                aliases: Vec::new(),
                is_admin: false,
                locale: Some("it-IT".to_owned()),
                timezone: None,
            }],
            groups: Vec::new(),
        };
        crate::enrollment::mirror_to_db(&pool, &file)
            .await
            .expect("mirror");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        // Per-call metadata says es-ES (Spanish). MUST win over the
        // it-IT enrollment default.
        let request = IngestRequest {
            text: "hola".to_owned(),
            author: MessageRole::User,
            sender_id: "alice".to_owned(),
            consumer_id: None,
            recent_messages: Vec::new(),
            context_hint: ContextHint::Conversation,
            disambig_choice: None,
            metadata: IngestMetadata {
                locale: Some("es-ES".to_owned()),
                occurred_at: None,
                ..Default::default()
            },
            attachments: Vec::new(),
        };
        wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        let system = llm
            .last_system_prompt()
            .expect("ingest must have called complete with a system prompt");
        assert!(
            system.contains("User locale: es-ES"),
            "metadata.locale must win over enrollment default: {system}"
        );
        assert!(
            system.contains("Respond in Spanish"),
            "language name must be derived from the metadata locale, not the enrollment: {system}"
        );
        // Defense-in-depth: the Italian enrollment value MUST NOT
        // leak into the prompt directive.
        assert!(
            !system.contains("User locale: it-IT"),
            "enrollment locale must not appear when metadata.locale is set: {system}"
        );
        assert!(
            !system.contains("Respond in Italian"),
            "Italian directive must not appear when metadata is Spanish: {system}"
        );
        drop(dir);
    }

    /// When neither `metadata.locale` nor `enrollment_users.locale`
    /// is set the renderer emits the legacy mirror clause — the
    /// pre-plumbing behaviour stays the floor.
    #[tokio::test]
    async fn ingest_falls_back_to_mirror_clause_when_no_locale_anywhere() {
        let (dir, tree, pool) = setup_workdir().await;
        // `setup_workdir` does not populate enrollment, so no locale
        // is configured and `metadata.locale` is the default `None`.
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("hello", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        let system = llm
            .last_system_prompt()
            .expect("ingest must have called complete with a system prompt");
        assert!(
            system.contains("Mirror the language"),
            "mirror clause must be the floor: {system}"
        );
        assert!(
            !system.contains("User locale:"),
            "no explicit User locale directive when no source is set: {system}"
        );
        drop(dir);
    }

    // ---------- recall-block tail: navigation + due-soon ----------

    /// Navigator double that fails the test if the funnel consults it —
    /// proves the intent gate (skip / structural pay no navigator call).
    struct PanickingLlm;
    #[async_trait]
    impl LlmBackend for PanickingLlm {
        fn model_id(&self) -> &'static str {
            "panicking"
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> std::result::Result<crate::llm::CompletionResponse, LlmError> {
            panic!("navigator must not be consulted on this intent");
        }
    }

    #[test]
    fn assemble_recall_block_joins_memory_sections_in_order_all_empty_is_none() {
        // The recall block is recalled MEMORY only: the role
        // sections, never behaviour directives.
        assert_eq!(
            assemble_recall_block(None, None, None, None, None, None, None),
            None
        );
        assert_eq!(
            assemble_recall_block(None, None, None, None, Some("  ".into()), None, None),
            None,
            "whitespace-only sections must not resurrect the block"
        );
        assert_eq!(
            assemble_recall_block(
                None,
                None,
                None,
                None,
                Some("flat".into()),
                Some("NAVIGATED PAGES:\n\n(a/preferenze.md)\nx".into()),
                Some("UPCOMING:\n- (a) y".into()),
            )
            .as_deref(),
            Some("flat\n\nNAVIGATED PAGES:\n\n(a/preferenze.md)\nx\n\nUPCOMING:\n- (a) y")
        );
        assert_eq!(
            assemble_recall_block(
                None,
                None,
                None,
                None,
                None,
                None,
                Some("UPCOMING:\n- (a) y".into())
            )
            .as_deref(),
            Some("UPCOMING:\n- (a) y"),
            "the due-soon slot alone carries the block"
        );
        assert_eq!(
            assemble_recall_block(
                Some("WHO YOU ARE: ...".into()),
                Some("WHO IS SPEAKING:\n- franz — dev".into()),
                Some("PEOPLE THIS TURN IS ABOUT:\n\n- carol\nshe is coeliac".into()),
                Some("YOUR RECENT HISTORY WITH THIS USER: ...".into()),
                Some("flat".into()),
                None,
                None
            )
            .as_deref(),
            Some(
                "WHO YOU ARE: ...\n\nWHO IS SPEAKING:\n- franz — dev\n\n\
                 PEOPLE THIS TURN IS ABOUT:\n\n- carol\nshe is coeliac\n\n\
                 YOUR RECENT HISTORY WITH THIS USER: ...\n\nflat"
            ),
            "identity leads, then the speaker, then the people the turn is about, then the history, then the facts"
        );
    }

    #[test]
    fn fit_bullets_fits_whole_bullets_and_never_cuts_mid_word() {
        // Everything fits: header + both bullets, newlines preserved.
        assert_eq!(
            fit_bullets("H:", ["aa", "bb"], 100).as_deref(),
            Some("H:\n- aa\n- bb")
        );
        // Budget for one bullet only: the SECOND (older) falls off whole —
        // no ellipsis, no mid-word cut.
        assert_eq!(
            fit_bullets("H:", ["aaaa", "bbbb"], 10).as_deref(),
            Some("H:\n- aaaa")
        );
        // No items → no section at all (empty-section contract).
        assert_eq!(fit_bullets("H:", [], 100), None);
        assert_eq!(fit_bullets("H:", ["  "], 100), None);
        // Pathological first bullet longer than the whole budget: kept,
        // char-truncated with the ellipsis, so the section is never empty.
        let out = fit_bullets("H:", ["abcdefghij"], 8).expect("section survives");
        assert!(out.starts_with("H:\n- abc"), "{out}");
        assert!(out.ends_with('…'), "{out}");
    }

    #[test]
    fn assemble_rules_block_leads_with_the_notice_all_empty_is_none() {
        // The dedicated behaviour-directive field: a one-shot
        // notice leads, then the served user's behaviour rules.
        assert_eq!(assemble_rules_block(None, None), None);
        assert_eq!(
            assemble_rules_block(None, Some("  ".into())),
            None,
            "whitespace-only sections must not resurrect the field"
        );
        assert_eq!(
            assemble_rules_block(None, Some("behaviour".into())).as_deref(),
            Some("behaviour"),
            "behaviour rules alone carry the field"
        );
        assert_eq!(
            assemble_rules_block(Some("NOTE — refused".into()), Some("behaviour".into()))
                .as_deref(),
            Some("NOTE — refused\n\nbehaviour"),
            "a one-shot notice (e.g. an agent-wide refusal) leads the behaviour rules"
        );
    }

    #[test]
    fn nav_seeds_unions_unit_topics_and_parses_subjects() {
        let json = "{\"intent\":\"capture\",\"extractions\":[\
            {\"target_wiki_id\":\"alice\",\"body\":\"a\",\"subject_id\":\"user:alice\",\
             \"topics\":[\"health\",\"food\"]},\
            {\"target_wiki_id\":\"alice\",\"body\":\"b\",\"subject_id\":\"user:alice\",\
             \"topics\":[\"food\"]},\
            {\"target_wiki_id\":\"alice\",\"body\":\"c\",\"subject_id\":\"not a principal\",\
             \"topics\":[]}]}";
        let plan = parse_plan(json).expect("plan parses");
        let seeds = nav_seeds(&plan);
        assert_eq!(seeds.topics, vec!["health".to_owned(), "food".to_owned()]);
        assert_eq!(seeds.subjects, vec![Principal::User("alice".into())]);
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "recall-block assembly differs on Windows — see issue #1"
    )]
    async fn ingest_recall_turn_appends_navigated_memory_section() {
        let (dir, tree, pool) = setup_workdir().await;
        // A page that is NOT the identity card: the card is served whole by
        // the deterministic `WHO IS SPEAKING` slot (69a) and is dropped from
        // this section by construction, so navigation is exercised on a
        // sibling page instead.
        std::fs::write(
            dir.path().join("wikis/alice/bologna.md"),
            "# bologna\nalice's city page\n",
        )
        .unwrap();
        // One active fact on the opened page makes the fragment header
        // carry the in-band freshness annotation (`· updated <date>`).
        let fact = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-00000000f001").unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/bologna.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "alice lives in Bologna".to_owned(),
            embedding: vec![0.9, -0.3, 0.2, -0.1],
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("bio".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            salience: None,
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(&pool, &fact).await.expect("insert fact");

        // The classifier only routes intent; the flat recap is deterministic.
        // This turn's point is the navigated section below.
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"recall\"}");
        // The fact's own page is a `rag`-seeded candidate; the navigator
        // opens it and stops.
        let nav = FakeLlmBackend::new(
            "fake-nav",
            "{\"open\":[{\"wiki_id\":\"alice\",\"page\":\"bologna.md\"}],\"done\":true}",
        );
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            Some(&nav),
            req("what do you know about me?", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Recall);
        let snippet = resp.context_snippet.expect("recall block present");
        assert!(
            snippet.contains(HDR_NAVIGATED_PAGES),
            "navigated section present: {snippet}"
        );
        assert!(
            snippet.contains("(alice/bologna.md · updated 20"),
            "fragment is page-headed with the freshness annotation: {snippet}"
        );
        assert!(snippet.contains("# bologna"), "projected prose: {snippet}");
        // The flat hit is homed on the page the navigator just injected —
        // the RELEVANT MEMORY slot drops it instead of repeating it (41f).
        assert!(
            !snippet.contains("- (alice) alice lives in Bologna"),
            "a hit homed on a navigated page must not arrive twice: {snippet}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_skip_turn_never_consults_the_navigator() {
        let (dir, tree, pool) = setup_workdir().await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            Some(&PanickingLlm),
            req("thanks!", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_surfaces_due_soon_slot_even_on_skip_turns() {
        let (dir, tree, pool) = setup_workdir().await;
        // One fact whose validity window closes inside the default 7-day
        // horizon. The due-soon pull is time-driven, not query-driven, so a
        // skip turn (no flat snippet at all) must still surface it.
        let due = (chrono::Utc::now() + chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fact = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-00000000d001").unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/preferenze.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "dentist appointment".to_owned(),
            embedding: vec![0.9, -0.3, 0.2, -0.1],
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("plan".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: Some(due.clone()),
            salience: None,
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(&pool, &fact).await.expect("insert fact");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ciao", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        let snippet = resp.context_snippet.expect("due-soon block present");
        assert!(
            snippet.contains(HDR_UPCOMING),
            "due-soon heading: {snippet}"
        );
        assert!(
            snippet.contains("dentist appointment") && snippet.contains(&due),
            "fact + its valid_to rendered: {snippet}"
        );

        // The slot honours its off switch.
        let policy_off = IngestPolicy {
            due_soon_top_k: 0,
            ..IngestPolicy::default()
        };
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ciao", "alice"),
            &policy_off,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.context_snippet, None);
        drop(dir);
    }

    /// Card 61 §39: the due-soon `UPCOMING` slot is time-driven
    /// (`recall::recall_due_soon` ranks by `valid_to` imminence, never by
    /// cosine) and the relevance floor never touches it — only the
    /// promoted half of `RELEVANT MEMORY` is gated. Same fact seeds both
    /// slots here: its stored embedding is nearly orthogonal to the fixed
    /// query embedding the fake embedder returns (cosine ≈ 0.09, well
    /// under the default 0.45 floor), so the flat recall pass surfaces it
    /// as a promoted hit too weak to open `RELEVANT MEMORY` — yet its
    /// `valid_to` sits inside the due-soon horizon, so `UPCOMING` still
    /// renders it.
    #[tokio::test]
    async fn ingest_upcoming_slot_still_renders_when_the_relevance_floor_shuts_the_promoted_section()
     {
        let (dir, tree, pool) = setup_workdir().await;
        let due = (chrono::Utc::now() + chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fact = fact_index::NewFact {
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse("018f1234-5678-7abc-9def-00000000d002").unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/preferenze.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "dentist appointment".to_owned(),
            // cosine ≈ 0.09 against the fixed query embedding [0.1, 0.2, 0.3, 0.4]
            embedding: vec![0.9, -0.3, 0.2, -0.1],
            subject_id: Principal::User("alice".into()),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("plan".to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: Some(due.clone()),
            salience: None,
            target_page: None,
            style: None,
            source_ref: None,
        };
        fact_index::insert(&pool, &fact).await.expect("insert fact");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"recall\"}");
        // The gate ships OFF (`DEFAULT_RELEVANCE_FLOOR = 0.0`), so a test of
        // the mechanism must turn it on explicitly rather than inherit the
        // deployment default — which is a policy choice, not a guarantee.
        let policy = IngestPolicy {
            relevance_floor: 0.45,
            ..IngestPolicy::default()
        };
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            req("ciao", "alice"),
            &policy,
        )
        .await
        .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Recall);
        let snippet = resp.context_snippet.expect("due-soon block present");
        assert!(
            snippet.contains(HDR_UPCOMING) && snippet.contains("dentist appointment"),
            "the due-soon slot is time-driven and ignores the relevance floor: {snippet}"
        );
        assert!(
            !snippet.contains(HDR_RELEVANT_MEMORY),
            "the turn's only flat hit is below the floor, so RELEVANT MEMORY must not open: {snippet}"
        );
        drop(dir);
    }

    // ---------- media attachments (the media pipeline) ----------

    async fn seed_photo(
        pool: &SqlitePool,
        workdir: &Path,
        subject: &str,
        bytes: &[u8],
    ) -> CatalogId {
        seed_photo_typed(pool, workdir, subject, bytes, "image/jpeg").await
    }

    /// Same, with the MIME type the consumer declared — the interesting
    /// axis once a provider started refusing types another accepted.
    async fn seed_photo_typed(
        pool: &SqlitePool,
        workdir: &Path,
        subject: &str,
        bytes: &[u8],
        mime: &str,
    ) -> CatalogId {
        let stored = crate::media::store_media(
            pool,
            workdir,
            crate::media::NewMedia {
                bytes: bytes.to_vec(),
                kind: crate::media::kind::PHOTO.to_owned(),
                mime: mime.to_owned(),
                subject: subject.parse().unwrap(),
                uploaded_by_consumer: None,
                caption: None,
                description: None,
                original_filename: Some("photo.jpg".to_owned()),
            },
        )
        .await
        .expect("store media");
        stored.row.catalog_id
    }

    fn attachment(
        catalog_id: &CatalogId,
        caption: Option<&str>,
        description: Option<&str>,
    ) -> IngestAttachment {
        IngestAttachment {
            catalog_id: catalog_id.clone(),
            kind: crate::media::kind::PHOTO.to_owned(),
            caption: caption.map(str::to_owned),
            description: description.map(str::to_owned),
        }
    }

    /// The full claimed path: the extraction claims the attachment, the
    /// filed fact's body carries the code-rendered embed marker, the
    /// catalog row's ACL widens to the fact's read set, the prompt shows
    /// the attachments window, and the undescribed photo's bytes ride
    /// the classifier call as an image.
    #[tokio::test]
    async fn claimed_attachment_carries_marker_widens_acl_and_rides_as_image() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let json = format!(
            "{{\"intent\":\"capture\",\"extractions\":[{{\
             \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:alice\",\"allow_ids\":[\"group:famiglia\"],\
             \"requested_container\":true,\
             \"body\":\"Foto di Frodo e Sam al cancello del giardino.\",\
             \"attachments\":[\"{cid}\"]}}],\"suggested_seed\":\"Bella foto!\"}}"
        );
        let llm = FakeLlmBackend::new("fake", &json);
        let policy = IngestPolicy::default();
        let mut request = req("guarda che foto!", "alice");
        request.attachments = vec![attachment(&cid, Some("al cancello"), None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Capture);

        // The filed fact's body carries the marker, appended by code.
        let row = fact_index::find_by_id(&pool, &resp.capture_id.expect("filed"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.text.ends_with(&format!("{{{{embed={cid}}}}}")),
            "marker appended: {}",
            row.text
        );

        // The prompt carried the attachments window.
        let prompt = llm.last_prompt().expect("prompt recorded");
        assert!(prompt.contains("attachments:"), "{prompt}");
        assert!(prompt.contains(cid.as_str()), "{prompt}");
        assert!(prompt.contains("al cancello"), "{prompt}");

        // The undescribed photo's bytes rode the call.
        let images = llm.last_images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");

        // The catalog row's ACL widened to the fact's read set.
        let media_row = crate::media::find_by_id(&pool, &cid)
            .await
            .unwrap()
            .unwrap();
        assert!(
            media_row
                .allow_ids
                .contains(&"group:famiglia".parse().unwrap()),
            "allow widened: {:?}",
            media_row.allow_ids
        );
        drop(dir);
    }

    /// The consumer-supplied description path: the server trusts the
    /// description, surfaces it in the prompt, and does NOT load the
    /// image bytes.
    #[tokio::test]
    async fn consumer_described_attachment_skips_the_vision_bytes() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let mut request = req("foto", "alice");
        request.attachments = vec![attachment(
            &cid,
            None,
            Some("photo of frodo and sam at the gate"),
        )];
        wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        let prompt = llm.last_prompt().expect("prompt recorded");
        assert!(
            prompt.contains("photo of frodo and sam at the gate"),
            "{prompt}"
        );
        assert!(
            llm.last_images().is_empty(),
            "described photo must not ride"
        );
        drop(dir);
    }

    /// `image/jpg` is not a MIME type — the registered name is
    /// `image/jpeg` — but it is what a real bridge declared for every one
    /// of the 27 photos in the production catalog. Gemini accepted it;
    /// Anthropic answers HTTP 400 and, before this, took the whole turn
    /// with it. The catalog stores the registered name and the model sees
    /// the registered name.
    #[tokio::test]
    async fn a_clients_image_jpg_becomes_image_jpeg_on_the_row_and_on_the_wire() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid =
            seed_photo_typed(&pool, dir.path(), "user:alice", b"jpegbytes", "image/JPG").await;
        let row = crate::media::find_by_id(&pool, &cid)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.mime, "image/jpeg", "canonicalised at the catalog");

        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let mut request = req("guarda", "alice");
        request.attachments = vec![attachment(&cid, Some("al cancello"), None)];
        wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            request,
            &IngestPolicy::default(),
        )
        .await
        .expect("ingest");
        let images = llm.last_images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
        drop(dir);
    }

    /// An encoding only some providers read — HEIC, straight off a phone —
    /// costs the photo, not the turn. Before this it reached the API and
    /// the 400 came back as "LLM unavailable", so the message was never
    /// classified at all.
    #[tokio::test]
    async fn an_image_type_not_every_provider_reads_is_dropped_not_fatal() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid =
            seed_photo_typed(&pool, dir.path(), "user:alice", b"heicbytes", "image/heic").await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
             \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:alice\",\"allow_ids\":[],\
             \"body\":\"Frodo al cancello del giardino.\"}],\
             \"suggested_seed\":\"Bella foto!\"}";
        let llm = FakeLlmBackend::new("fake", json);
        let mut request = req("guarda", "alice");
        request.attachments = vec![attachment(&cid, Some("al cancello"), None)];
        let resp = wiki_ingest_message(
            &pool,
            &tree,
            fake_embedder(),
            &llm,
            None,
            request,
            &IngestPolicy::default(),
        )
        .await
        .expect("the turn survives the unreadable photo");
        assert_eq!(resp.intent, IntentKind::Capture);
        assert!(resp.capture_id.is_some(), "the fact is still filed");
        assert!(
            llm.last_images().is_empty(),
            "the unreadable photo never reaches the provider"
        );
        // The caption still reached the prompt, so the classifier had
        // something to file.
        let prompt = llm.last_prompt().expect("prompt recorded");
        assert!(prompt.contains("al cancello"), "{prompt}");
        drop(dir);
    }

    /// A skip plan with a CAPTIONED attachment in flight: the
    /// deterministic fallback files the media into the sender's identity
    /// wiki's queue (caption as body + marker) — described media is never
    /// dead. (A text-less one stays catalogued-unfiled — see
    /// `textless_unclaimed_attachment_stays_catalogued_unfiled`.)
    #[tokio::test]
    async fn unclaimed_attachment_is_filed_by_the_deterministic_fallback() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let mut request = req("guarda", "alice");
        request.attachments = vec![attachment(&cid, Some("tramonto sul porto"), None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert!(resp.capture_id.is_some(), "fallback filed the media");
        let buffered = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert_eq!(buffered.len(), 1);
        assert!(buffered[0].body.contains("tramonto sul porto"));
        assert!(
            buffered[0].body.contains(&format!("{{{{embed={cid}}}}}")),
            "{}",
            buffered[0].body
        );
        drop(dir);
    }

    /// Even an unparseable plan (the skip fallback) files the media.
    #[tokio::test]
    async fn unparseable_plan_with_attachment_still_files_media() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let llm = FakeLlmBackend::new("fake", "not json at all");
        let policy = IngestPolicy::default();
        let mut request = req("foto", "alice");
        request.attachments = vec![attachment(&cid, Some("la serra"), None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert_eq!(resp.intent, IntentKind::Skip);
        assert!(
            resp.capture_id.is_some(),
            "media filed despite the bad plan"
        );
        let buffered = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert_eq!(buffered.len(), 1);
        drop(dir);
    }

    /// A hallucinated claim (an id outside the turn's window) is dropped
    /// from the fact, and the real attachment falls back deterministically.
    #[tokio::test]
    async fn hallucinated_attachment_claim_is_dropped_and_media_falls_back() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let json = "{\"intent\":\"capture\",\"extractions\":[{\
             \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:alice\",\"requested_container\":true,\
             \"body\":\"Una foto qualunque.\",\
             \"attachments\":[\"c-2020-01-01-photo-999.jpg\"]}]}";
        let llm = FakeLlmBackend::new("fake", json);
        let policy = IngestPolicy::default();
        let mut request = req("foto", "alice");
        request.attachments = vec![attachment(&cid, Some("il porto"), None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        // The fact filed without any marker (the claim was bogus)…
        let row = fact_index::find_by_id(&pool, &resp.capture_id.expect("filed"))
            .await
            .unwrap()
            .unwrap();
        assert!(!row.text.contains("{{embed="), "{}", row.text);
        // …and the real attachment was filed by the fallback.
        let buffered = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert_eq!(buffered.len(), 1);
        assert!(buffered[0].body.contains(cid.as_str()));
        drop(dir);
    }

    /// A text-less unclaimed attachment (no caption, no description)
    /// files NOTHING: a fact whose whole body would be the kind word has
    /// no recall surface. The blob stays catalogued; the wiki stays clean.
    #[tokio::test]
    async fn textless_unclaimed_attachment_stays_catalogued_unfiled() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let llm = FakeLlmBackend::new("fake", "{\"intent\":\"skip\"}");
        let policy = IngestPolicy::default();
        let mut request = req("guarda", "alice");
        request.attachments = vec![attachment(&cid, None, None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        assert!(resp.capture_id.is_none(), "nothing filed for bare media");
        let buffered = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert!(buffered.is_empty(), "buffer stays empty: {buffered:?}");
        // The catalog row survives untouched.
        assert!(
            crate::media::find_by_id(&pool, &cid)
                .await
                .unwrap()
                .is_some(),
            "blob stays catalogued"
        );
        drop(dir);
    }

    /// The model writing `{{embed=…}}` syntax directly in a body is
    /// stripped — the claims array is the only sanctioned route — and
    /// the unclaimed media falls back deterministically.
    #[tokio::test]
    async fn model_written_embed_marker_in_body_is_stripped_and_media_falls_back() {
        let (dir, tree, pool) = setup_workdir().await;
        let cid = seed_photo(&pool, dir.path(), "user:alice", b"jpegbytes").await;
        let json = format!(
            "{{\"intent\":\"capture\",\"extractions\":[{{\
             \"target_wiki_id\":\"alice\",\"target_page\":\"preferenze.md\",\
             \"subject_id\":\"user:alice\",\"requested_container\":true,\
             \"body\":\"Una foto {{{{embed={cid}}}}} qualunque.\"}}]}}"
        );
        let llm = FakeLlmBackend::new("fake", &json);
        let policy = IngestPolicy::default();
        let mut request = req("foto", "alice");
        request.attachments = vec![attachment(&cid, Some("la foto"), None)];
        let resp = wiki_ingest_message(&pool, &tree, fake_embedder(), &llm, None, request, &policy)
            .await
            .expect("ingest");
        // The filed fact lost the model-written marker…
        let row = fact_index::find_by_id(&pool, &resp.capture_id.expect("filed"))
            .await
            .unwrap()
            .unwrap();
        assert!(!row.text.contains("{{embed="), "{}", row.text);
        // …and the unclaimed media was filed by the fallback instead.
        let buffered = capture_buffer::find_all_buffered(&pool, 100)
            .await
            .expect("buffer read");
        assert_eq!(buffered.len(), 1);
        assert!(buffered[0].body.contains(cid.as_str()));
        drop(dir);
    }
}
