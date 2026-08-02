// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-tool handlers for the 19 MCP tools.
//!
//! Each handler reads the validated [`IdentityProfile`], deserialises
//! the JSON args into a private input struct, calls the matching
//! `mwe-core` API, and returns a `serde_json::Value` that the dispatcher
//! wraps in a `CallToolResult`.
//!
//! ## Dispatch contract
//!
//! - Every handler returns `Result<serde_json::Value, ToolError>`.
//! - The dispatcher wraps the `Ok` value into `CallToolResult` and the
//!   `Err` value into an `McpError` via [`super::error::into_mcp_error`].
//! - Soft failures (e.g. LLM transport down inside `wiki_ingest_message`)
//!   are absorbed inside the relevant `mwe-core` call and surface as a
//!   degraded-but-successful response — never as `Err`.
//! - Hard failures (DB down, filesystem broken) bubble up as
//!   `ToolErrorClass::InternalError`.

use std::sync::Arc;
use std::time::Duration;

use mwe_core::audit::{self, ResultStatus, SearchFilters};
use mwe_core::config::LlmFunction;
use mwe_core::consumers;
use mwe_core::enrollment;
use mwe_core::events;
use mwe_core::fact_index::FactFilters;
use mwe_core::ingest::{
    self, ContextHint, IngestMetadata, IngestRequest, MessageRole, RecentMessage,
};
use mwe_core::jwt::{self, TokenClaims};
use mwe_core::lint::{self, Check, LintScope};
use mwe_core::proposals;
use mwe_core::recall::{self, SenderContext};
use mwe_core::types::WikiId;
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{ToolError, ToolErrorClass, invalid_input};
use super::state::{IdentityProfile, McpState};

/// Default `device_label` for session tokens issued by `dashboard_link`
/// — matches `mwe-dashboard::auth::session::SESSION_DEVICE_LABEL`.
const DASHBOARD_DEVICE_LABEL: &str = "dashboard-session";
/// `rate_limit_id` baked into `dashboard_link`-minted sessions.
const DASHBOARD_RATE_LIMIT_ID: &str = "dashboard";
/// Sliding TTL the dashboard cookie middleware refreshes. We mint the
/// initial link with the same length so the URL stamp matches the
/// cookie behaviour the user will see after the first interaction.
const DASHBOARD_LINK_TTL: Duration = Duration::from_secs(10 * 60);

// ---------- Common deserialisation helpers ----------

fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, ToolError> {
    serde_json::from_value::<T>(args.clone())
        .map_err(|e| invalid_input(format!("malformed arguments: {e}")))
}

fn forbid_sender_mismatch(
    identity: &IdentityProfile,
    claimed: Option<&str>,
) -> Result<(), ToolError> {
    if let Some(s) = claimed
        && s != identity.sender_id
    {
        return Err(ToolError::new(
            ToolErrorClass::SenderTokenMismatch,
            format!(
                "sender_id arg `{s}` ≠ token sender `{}`",
                identity.sender_id
            ),
        ));
    }
    Ok(())
}

/// Refuse a tool to the builtin `guest` pseudo-identity (the
/// unidentified-human sender, roadmap 40).
///
/// Guest turns are ephemeral by contract: an unidentified person must not
/// leave permanent state (documents, briefing notes, registry writes) nor
/// receive operator surfaces (audit search, signed dashboard links). The
/// read tools and `wiki_ingest_message` stay open — the ACL confines them
/// to the public slice, and ingest itself files nothing for guest.
fn forbid_guest(identity: &IdentityProfile, what: &str) -> Result<(), ToolError> {
    if mwe_core::enrollment::is_guest(&identity.sender_id) {
        return Err(ToolError::new(
            ToolErrorClass::SenderUnauthorized,
            format!(
                "{what} is not available to the builtin `guest` pseudo-identity — guest turns \
                 are ephemeral and leave no permanent state"
            ),
        ));
    }
    Ok(())
}

// ============================================================
// A — wiki_ingest_message
// ============================================================

#[derive(Debug, Deserialize)]
struct IngestArgs {
    text: String,
    sender_id: Option<String>,
    /// `"user"` (default) or `"assistant"`. The consumer sets `"assistant"`
    /// only when feeding back the agent's OWN reply for extraction (roadmap 27,
    /// agent-authored memory).
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    recent_messages: Vec<RecentMessageArg>,
    #[serde(default)]
    context_hint: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    attachments: Vec<AttachmentArg>,
    /// `always` | `never` — forces the paste-into-chat promotion
    /// backstop (roadmap 46c); absent = oversized document-shaped
    /// user turns are promoted automatically.
    #[serde(default)]
    promote: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RecentMessageArg {
    role: String,
    text: String,
    #[serde(default)]
    timestamp: Option<String>,
}

/// One media attachment riding the turn — the wire shape of the
/// `attachments` array. The bytes were uploaded out of band via
/// `POST /media`; this only carries the minted key plus annotations.
/// `kind` is accepted for the consumer's own bookkeeping but the
/// catalog row's kind is authoritative.
#[derive(Debug, Deserialize)]
struct AttachmentArg {
    catalog_id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Resolve the wire attachments against the media catalog: every id
/// must parse, exist, and be readable by the effective sender — an
/// ingest must not be able to link (and later ACL-widen) someone
/// else's media sight-unseen.
async fn resolve_attachments(
    state: &McpState,
    identity: &IdentityProfile,
    args: Vec<AttachmentArg>,
) -> Result<Vec<ingest::IngestAttachment>, ToolError> {
    if args.is_empty() {
        return Ok(Vec::new());
    }
    let groups = mwe_core::enrollment::groups_for(&state.pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, format!("groups_for: {e}")))?;
    let mut out = Vec::with_capacity(args.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for arg in args {
        let catalog_id = mwe_core::types::CatalogId::parse(&arg.catalog_id)
            .map_err(|e| invalid_input(format!("attachments: {e}")))?;
        // A duplicated id (client retry artifact) would burn the vision
        // byte budget twice and make the fallback file two identical
        // facts — first occurrence wins.
        if !seen.insert(catalog_id.as_str().to_owned()) {
            tracing::debug!(catalog_id = %catalog_id, "ingest attachments: duplicate id dropped");
            continue;
        }
        let row = mwe_core::media::find_by_id(&state.pool, &catalog_id)
            .await
            .map_err(|e| {
                ToolError::new(ToolErrorClass::InternalError, format!("media lookup: {e}"))
            })?
            .ok_or_else(|| {
                invalid_input(format!(
                    "attachments: no catalog row for `{catalog_id}` — upload via POST /media first"
                ))
            })?;
        if !mwe_core::media::row_visible_to(&row, &identity.sender_id, &groups) {
            return Err(ToolError::new(
                ToolErrorClass::SenderUnauthorized,
                format!("attachments: `{catalog_id}` is not readable by the effective sender"),
            ));
        }
        if let Some(k) = arg.kind.as_deref()
            && k != row.kind
        {
            tracing::debug!(
                catalog_id = %catalog_id,
                declared = k,
                catalog = %row.kind,
                "ingest attachments: declared kind differs — catalog wins"
            );
        }
        out.push(ingest::IngestAttachment {
            catalog_id,
            kind: row.kind.clone(),
            caption: arg.caption.or(row.caption),
            description: arg.description.or(row.description),
        });
    }
    Ok(out)
}

/// Pull the dispatcher-honoured keys out of the free-form `metadata`
/// object: `disambig_choice` (returned separately — it rides on the
/// request itself) plus the [`IngestMetadata`] signals (`locale`,
/// `occurred_at`).
fn parse_ingest_metadata(
    metadata: Option<&Value>,
) -> Result<(Option<String>, IngestMetadata), ToolError> {
    let disambig_choice = metadata
        .and_then(|m| m.get("disambig_choice"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let locale = metadata
        .and_then(|m| m.get("locale"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // A malformed timestamp is a consumer bug worth surfacing — silently
    // falling back to the server clock would mis-date every validity
    // window in a backlog replay.
    let occurred_at = metadata
        .and_then(|m| m.get("occurred_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    invalid_input(format!(
                        "metadata.occurred_at is not ISO-8601/RFC-3339: {e}"
                    ))
                })
        })
        .transpose()?;

    // Provenance breadcrumbs (`[[wiki_id/page]]`) the consumer carries from
    // its preceding `wiki_admin_push` so consolidation can link instead of
    // duplicate (roadmap group 17). Accept a JSON array of strings; trim and
    // drop blanks. Non-array / non-string entries are ignored rather than
    // rejected — additive, never breaks an existing turn.
    let authored_refs = metadata
        .and_then(|m| m.get("authored_refs"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Opaque surface label for the cross-consumer recent window (group 43):
    // multi-channel consumers tag their surfaces apart so only the
    // requesting one is excluded from what the window serves back.
    let channel = metadata
        .and_then(|m| m.get("channel"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    Ok((
        disambig_choice,
        IngestMetadata {
            locale,
            occurred_at,
            authored_refs,
            channel,
        },
    ))
}

/// What the paste-into-chat promotion hands back to the turn ingest.
struct TurnPromotion {
    /// Bounded head excerpt + hand-off note the conversational
    /// pipeline ingests instead of the full paste.
    text_stub: String,
    /// The promoted document riding the turn as a linked attachment
    /// (the existing media-on-a-turn seam).
    attachment: ingest::IngestAttachment,
    /// The `document_promoted` response block.
    receipt: Value,
}

/// Verbatim source promotion, the paste-into-chat door (roadmap 46c).
///
/// An oversized document-shaped user turn is archived verbatim on the
/// media rail and extracted by the document pipeline; the
/// conversational ingest then sees a bounded excerpt plus the
/// attachment link, so the thread stays coherent without
/// double-extracting the body. `None` = the turn is not a promotion
/// candidate: guests never mint permanent state (their turns are
/// ephemeral by design), the dashboard command surface is exempt, and
/// assistant-authored feedback turns never carry a paste.
async fn maybe_promote_turn(
    state: &McpState,
    identity: &IdentityProfile,
    text: &str,
    promote: Option<mwe_core::document::PromoteHint>,
    author: MessageRole,
    context_hint: ContextHint,
    metadata: &IngestMetadata,
) -> Result<Option<TurnPromotion>, ToolError> {
    // The stub keeps enough head for thread coherence; the hand-off
    // note tells the classifier not to re-extract.
    const PROMOTED_TURN_HEAD_CHARS: usize = 400;

    if !matches!(author, MessageRole::User)
        || matches!(context_hint, ContextHint::DashboardCommand)
        || mwe_core::enrollment::is_guest(&identity.sender_id)
        || !mwe_core::document::should_promote_turn(
            text,
            promote,
            &mwe_core::document::PromotionPolicy::default(),
        )
    {
        return Ok(None);
    }

    let owner: mwe_core::types::Principal = format!("user:{}", identity.sender_id)
        .parse()
        .map_err(|e| {
            ToolError::new(
                ToolErrorClass::InternalError,
                format!("effective sender: {e}"),
            )
        })?;
    let row = promote_text_to_media(state, identity, &owner, text, None).await?;
    let outcome = mwe_core::document::enqueue(
        &state.pool,
        &state.document_policy,
        mwe_core::document::EnqueueRequest {
            source_kind: "media".into(),
            source_ref: Some(row.catalog_id.as_str().to_owned()),
            text: text.to_owned(),
            title_hint: row.caption.clone(),
            disposition: None,
            format: None,
            occurred_at: metadata.occurred_at.map(|d| d.to_rfc3339()),
            owner,
            allow: Vec::new(),
            sender: None,
            force: false,
        },
    )
    .await
    .map_err(|e| map_document_err(&e))?;

    let total_chars = text.chars().count();
    let head: String = text.chars().take(PROMOTED_TURN_HEAD_CHARS).collect();
    let text_stub = format!(
        "{head}…\n\n[mwe: the full pasted text ({total_chars} chars) was archived verbatim \
         as document {} and queued for document ingestion — this excerpt is context only]",
        row.catalog_id
    );
    let receipt = json!({
        "catalog_id": row.catalog_id.as_str(),
        "job_id": outcome.job_id,
        "existing": outcome.existing,
    });
    Ok(Some(TurnPromotion {
        text_stub,
        attachment: ingest::IngestAttachment {
            catalog_id: row.catalog_id.clone(),
            kind: row.kind.clone(),
            caption: row.caption.clone(),
            description: Some("pasted document promoted to the media rail".into()),
        },
        receipt,
    }))
}

/// Parse the enum-shaped dials of one ingest turn, rejecting unknown
/// tokens as invalid input.
fn parse_turn_dials(
    args: &IngestArgs,
) -> Result<
    (
        ContextHint,
        MessageRole,
        Option<mwe_core::document::PromoteHint>,
    ),
    ToolError,
> {
    let context_hint = match args.context_hint.as_deref() {
        Some("dashboard_command") => ContextHint::DashboardCommand,
        Some("import") => ContextHint::Import,
        Some("conversation") | None => ContextHint::Conversation,
        Some(other) => {
            return Err(invalid_input(format!("unknown context_hint: {other}")));
        },
    };
    let author = match args.author.as_deref() {
        Some("assistant") => MessageRole::Assistant,
        Some("user") | None => MessageRole::User,
        Some(other) => {
            return Err(invalid_input(format!("unknown author: {other}")));
        },
    };
    let promote = args
        .promote
        .as_deref()
        .map(|s| {
            mwe_core::document::PromoteHint::parse(s)
                .ok_or_else(|| invalid_input(format!("unknown promote: {s}")))
        })
        .transpose()?;
    Ok((context_hint, author, promote))
}

pub(super) async fn call_wiki_ingest_message(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: IngestArgs = parse_args(&args)?;
    forbid_sender_mismatch(identity, args.sender_id.as_deref())?;
    if args.text.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorClass::InvalidInput,
            "text must not be empty",
        ));
    }
    let (context_hint, author, promote) = parse_turn_dials(&args)?;
    let recent_messages = parse_recent_messages(args.recent_messages)?;
    let (disambig_choice, metadata) = parse_ingest_metadata(args.metadata.as_ref())?;
    let mut attachments = resolve_attachments(state, identity, args.attachments).await?;

    // The slot gate sits before the promotion backstop: promotion mints
    // permanent state and enqueues a document job, so refuse first if no
    // worker could ever run it.
    let llm_slot = state.llm_config.slot(LlmFunction::Ingest).ok_or_else(|| {
        ToolError::new(
            ToolErrorClass::ServiceUnavailable,
            "llm.ingest not configured in mwe-mcp.config.yaml",
        )
    })?;

    // Verbatim source promotion, the paste-into-chat door (roadmap 46c).
    let promotion = maybe_promote_turn(
        state,
        identity,
        &args.text,
        promote,
        author,
        context_hint,
        &metadata,
    )
    .await?;
    let (text, document_promoted) = match promotion {
        Some(p) => {
            attachments.push(p.attachment);
            (p.text_stub, Some(p.receipt))
        },
        None => (args.text, None),
    };

    let request = IngestRequest {
        text,
        author,
        sender_id: identity.sender_id.clone(),
        consumer_id: identity.consumer_id.clone(),
        recent_messages,
        context_hint,
        disambig_choice,
        metadata,
        attachments,
    };

    // The recall knobs come from the shared operator settings (the
    // `recall:` config section, hot-editable from the dashboard); the
    // classifier prompt-budget knobs stay at their defaults.
    let policy = state
        .recall
        .read()
        .expect("recall settings rwlock poisoned")
        .resolved_ingest_policy();
    let llm = llm_slot
        .build_backend(LlmFunction::Ingest)
        .map_err(|e| ToolError::new(ToolErrorClass::ServiceUnavailable, format!("llm: {e}")))?;
    let navigator = build_navigator(state);

    let resp = ingest::wiki_ingest_message(
        &state.pool,
        &state.tree,
        Arc::clone(&state.embedder),
        llm.as_ref(),
        navigator.as_deref(),
        request,
        &policy,
    )
    .await
    .map_err(|e| map_ingest_err(&e))?;

    let (pending_attention, pending_votes) = governance_blocks(state, identity).await?;

    let mut payload = json!({
        "intent_classified": resp.intent.as_str(),
        "context_snippet": resp.context_snippet,
        "rules": resp.rules,
        "suggested_seed": resp.suggested_seed,
        "recent_window": resp.recent_window,
        "capture_id": resp.capture_id.map(|f| f.as_str().to_owned()),
        "needs_disambig": resp.needs_disambig,
        "disambig_candidates": resp.disambig_candidates
            .into_iter()
            .map(|d| json!({"candidate_id": d.candidate_id, "description": d.description}))
            .collect::<Vec<_>>(),
        "llm_used": resp.llm_used,
        "took_ms": resp.took_ms,
    });
    let extra_blocks = [
        ("pending_attention", pending_attention),
        ("pending_votes", pending_votes),
        ("document_promoted", document_promoted),
    ];
    for (key, block) in extra_blocks {
        if let Some(block) = block {
            payload
                .as_object_mut()
                .expect("json! root is an object")
                .insert(key.into(), block);
        }
    }
    Ok(payload)
}

/// Map the wire-shape `recent_messages` array into the core type,
/// rejecting unknown roles as invalid input.
fn parse_recent_messages(args: Vec<RecentMessageArg>) -> Result<Vec<RecentMessage>, ToolError> {
    args.into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "user" => Ok(MessageRole::User),
                "assistant" => Ok(MessageRole::Assistant),
                other => Err(invalid_input(format!(
                    "unknown recent_message.role: {other}"
                ))),
            }?;
            Ok::<_, ToolError>(RecentMessage {
                role,
                text: m.text,
                timestamp: m.timestamp,
            })
        })
        .collect()
}

/// Build the recall navigator's backend from the `navigator` config slot.
/// Optional by contract: a missing or unbuildable slot degrades to
/// flat-only recall (navigation off), never a failed turn.
fn build_navigator(state: &McpState) -> Option<Box<dyn mwe_core::llm::LlmBackend>> {
    state
        .llm_config
        .slot(LlmFunction::Navigator)
        .and_then(|slot| match slot.build_backend(LlmFunction::Navigator) {
            Ok(backend) => Some(backend),
            Err(e) => {
                tracing::warn!(error = %e, "navigator backend build failed — navigation off");
                None
            },
        })
}

/// Wiki ingest path (`/dashboard/proposals`) the consumer agent should
/// nudge the user toward when at least one structure proposal is in
/// flight. Kept here (not derived from `dashboard_link` intents)
/// because the warning is a structured hint, not a signed link: the
/// consumer composes the user-visible URL via [`call_dashboard_link`]
/// with `intent: "home"` and tells the user to navigate to this path,
/// or surfaces it raw on already-authenticated dashboards.
const PENDING_ATTENTION_DASHBOARD_PATH: &str = "/dashboard/proposals";

/// Build the `pending_attention` block surfaced in the
/// [`call_wiki_ingest_message`] response when at least one
/// `structure_proposals` row is `pending` or
/// `applied_pending_confirm`. Returns `None` when the count is
/// zero — we keep the default wire shape quiet so the consumer agent
/// only sees the block when there is something to warn about.
///
/// The count is scoped to the acting caller: every identity — admins
/// included — sees only rows addressed to them plus the unaddressed ones.
/// The dashboard admin ACL-reveal switch deliberately does not reach the
/// MCP tool surface (`crate::reveal` is dashboard-only); MCP tools always
/// honour the ACL, so the count never lifts to deployment-wide here.
/// The two governance blocks appended to an ingest response, suppressed
/// on guest turns: the attention count includes unaddressed proposals,
/// and a guest can neither open the dashboard nor owe a vote.
async fn governance_blocks(
    state: &McpState,
    identity: &IdentityProfile,
) -> Result<(Option<Value>, Option<Value>), ToolError> {
    if mwe_core::enrollment::is_guest(&identity.sender_id) {
        return Ok((None, None));
    }
    Ok((
        pending_attention_block(&state.pool, identity).await?,
        pending_votes_block(&state.pool, identity).await?,
    ))
}

async fn pending_attention_block(
    pool: &sqlx::SqlitePool,
    identity: &IdentityProfile,
) -> Result<Option<Value>, ToolError> {
    let recipient = Some(format!("user:{}", identity.sender_id));
    let counts = proposals::count_in_flight(pool, recipient.as_deref(), chrono::Utc::now())
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    // The ingest nudge is specifically about not piling more state on top
    // of an *unconfirmed* change, so it gates on `pending` +
    // `applied_pending_confirm` only. The third in-flight class
    // (born-applied structural receipts with an open revert window) is
    // already applied and surfaced by the dashboard badge + its
    // `structure_applied` notice on the event stream; folding it in here
    // would make the warning fire on every just-applied change, which is
    // noise for the consumer.
    let unconfirmed = counts.pending + counts.applied_pending_confirm;
    if unconfirmed == 0 {
        return Ok(None);
    }
    Ok(Some(json!({
        "pending_count": counts.pending,
        "applied_pending_confirm_count": counts.applied_pending_confirm,
        "dashboard_path": PENDING_ATTENTION_DASHBOARD_PATH,
        "note": "scoped_to_recipient",
    })))
}

/// Build the `pending_votes` block surfaced in the
/// [`call_wiki_ingest_message`] response when the acting member owes a vote on
/// a pending **fact-forget request** (the write-authority model). Returns `None` when
/// there is nothing to vote on, so the default wire shape stays quiet.
///
/// Pull-only by design: the reminder appears the next time the member interacts
/// with their agent; there is no push. A member who never looks consents by
/// silence when the request's window closes (the fact is then forgotten). The
/// member casts the vote by asking their agent, which the operator resolves from
/// the dashboard (`/dashboard/proposals`) — the same surface every other
/// proposal action uses.
async fn pending_votes_block(
    pool: &sqlx::SqlitePool,
    identity: &IdentityProfile,
) -> Result<Option<Value>, ToolError> {
    let pending = mwe_core::votes::pending_votes_for(pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    if pending.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "count": pending.len(),
        "requests": pending,
        "dashboard_path": PENDING_ATTENTION_DASHBOARD_PATH,
        "note": "vote_no_to_block_silence_is_consent",
    })))
}

fn map_ingest_err(e: &ingest::IngestError) -> ToolError {
    match e {
        ingest::IngestError::EmptyText => {
            ToolError::new(ToolErrorClass::InvalidInput, "text must not be empty")
        },
        _ => ToolError::new(ToolErrorClass::InternalError, e.to_string()),
    }
}

// ============================================================
// B — events_poll / events_ack
// ============================================================

#[derive(Debug, Deserialize)]
struct EventsPollArgs {
    consumer_id: String,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    top_k: Option<i64>,
}

pub(super) async fn call_events_poll(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: EventsPollArgs = parse_args(&args)?;
    enforce_consumer_match(identity, &args.consumer_id)?;
    require_consumer_registered(state, &args.consumer_id).await?;

    // `sender_id` is the caller's verified identity: it widens the recipient
    // scope to their own notices, which is what lets a smart consumer (no
    // system user, no delegation) receive its owner's mail unconfigured.
    let outcome = events::poll_events(
        &state.pool,
        &args.consumer_id,
        &identity.sender_id,
        args.since.as_deref(),
        &args.kinds,
        args.top_k.unwrap_or(events::DEFAULT_POLL_TOP_K),
    )
    .await
    .map_err(|e| map_events_err(&e))?;

    Ok(json!({
        "events": outcome.events
            .into_iter()
            .map(|e| json!({
                "event_id": e.event_id,
                "kind": e.kind,
                "wiki_id": e.wiki_id,
                "fact_id": e.fact_id,
                "payload": e.payload,
                "emitted_at": e.emitted_at,
            }))
            .collect::<Vec<_>>(),
        "has_more": outcome.has_more,
    }))
}

#[derive(Debug, Deserialize)]
struct EventsAckArgs {
    consumer_id: String,
    event_ids: Vec<i64>,
}

pub(super) async fn call_events_ack(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: EventsAckArgs = parse_args(&args)?;
    enforce_consumer_match(identity, &args.consumer_id)?;
    require_consumer_registered(state, &args.consumer_id).await?;
    if args.event_ids.is_empty() {
        return Err(invalid_input("event_ids must not be empty"));
    }

    let outcome = events::ack_events(&state.pool, &args.consumer_id, &args.event_ids)
        .await
        .map_err(|e| map_events_err(&e))?;
    Ok(json!({
        "acked": outcome.acked,
        "unknown": outcome.unknown,
    }))
}

fn map_events_err(e: &events::EventsError) -> ToolError {
    ToolError::new(ToolErrorClass::InternalError, e.to_string())
}

fn enforce_consumer_match(identity: &IdentityProfile, arg: &str) -> Result<(), ToolError> {
    match &identity.consumer_id {
        Some(token_consumer) if token_consumer == arg => Ok(()),
        Some(token_consumer) => Err(ToolError::new(
            ToolErrorClass::SenderUnauthorized,
            format!("consumer_id `{arg}` ≠ token consumer `{token_consumer}`"),
        )),
        None => {
            // Token has no consumer_id ⇒ caller is a human user, not a
            // bot. They can drain events for any registered consumer
            // (admin debugging surface). This may be tightened later.
            if identity.is_admin {
                Ok(())
            } else {
                Err(ToolError::new(
                    ToolErrorClass::SenderUnauthorized,
                    "token is not bound to a consumer_id (admin-only fallback)",
                ))
            }
        },
    }
}

async fn require_consumer_registered(state: &McpState, consumer_id: &str) -> Result<(), ToolError> {
    let known = consumers::is_registered(&state.pool, consumer_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    if known {
        Ok(())
    } else {
        Err(ToolError::new(
            ToolErrorClass::ConsumerNotRegistered,
            format!("consumer `{consumer_id}` is not registered"),
        ))
    }
}

// The whole `structure_proposal_*` family (the `_apply` / `_confirm` /
// `_revert` writes and the `_list` read) has been removed from the MCP
// surface. Structural changes apply directly in REM and reach the
// consumer as `structure_applied` notices over `events_poll`; the
// notice names the affected user and carries the `dashboard_path` of
// the undo surface (the dashboard calls `mwe-core::proposals`
// directly).

// ============================================================
// D — wiki_read / wiki_search
// ============================================================

#[derive(Debug, Deserialize)]
struct WikiReadArgs {
    wiki_id: String,
    sender_id: Option<String>,
    /// Page path relative to the wiki directory (default `index.md`).
    /// Validated by [`mwe_core::wiki::is_safe_page_path`].
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default)]
    format: Option<String>,
}

pub(super) async fn call_wiki_read(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiReadArgs = parse_args(&args)?;
    forbid_sender_mismatch(identity, args.sender_id.as_deref())?;
    let _ = args.include_archived; // accepted, not yet honored (archive surface)
    let _ = args.format;
    // Page selection: default `index.md`, any safe relative path otherwise.
    // The body and the per-fact ACL map MUST resolve to the *same* page —
    // reading page X while loading another page's ACL would be a leak.
    let page_rel = args.path.as_deref().unwrap_or("index.md");
    let page = std::path::Path::new(page_rel);
    if !mwe_core::wiki::is_safe_page_path(page) {
        return Err(invalid_input(format!(
            "path: unsafe page path `{page_rel}`"
        )));
    }
    let wiki_id =
        WikiId::parse(&args.wiki_id).map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let handle = state.tree.locate(&wiki_id).map_err(|e| match e {
        mwe_core::wiki::WikiError::WikiNotFound { .. } => ToolError::new(
            ToolErrorClass::NotFound,
            format!("wiki `{wiki_id:?}` not found"),
        ),
        other => ToolError::new(ToolErrorClass::InternalError, other.to_string()),
    })?;
    let meta = handle.meta();
    let sender_groups = enrollment::groups_for(&state.pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    // Derived wiki visibility: a wiki (and its pages) surfaces to a reader only
    // if they can read ≥1 fact in it — the same gate the recall navigator uses
    // (`ReaderCard::reader_can_read_in`). A reader who can read nothing here gets
    // `not_found`, never a wiki-level render of connective prose / page structure
    // they were never granted. Per-region redaction (`render_for_sender`) still
    // gates every fact on the page below.
    // A smart wiki holds no rows in `fact_index`, so the derived question
    // ("can you read ≥1 fact here?") always fell into the empty-wiki branch and
    // answered *visible*. `wiki_readable_by` routes it to the wiki-level gate.
    if !mwe_core::wiki_admin::wiki_readable_by(
        &state.pool,
        &state.tree,
        &handle,
        &identity.sender_id,
        &sender_groups,
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?
    {
        return Err(ToolError::new(
            ToolErrorClass::NotFound,
            format!("page `{page_rel}` not found in wiki `{wiki_id:?}`"),
        ));
    }
    let raw = handle.read_page(page).map_err(|e| match e {
        mwe_core::wiki::WikiError::PageNotFound { .. } => ToolError::new(
            ToolErrorClass::NotFound,
            format!("page `{page_rel}` not found in wiki `{wiki_id:?}`"),
        ),
        other => ToolError::new(ToolErrorClass::InternalError, other.to_string()),
    })?;
    // The frontmatter (testata) is card metadata derived from the page's
    // facts — not page content — and it carries no ACL markers, so
    // `render_for_sender` would pass it through verbatim and leak the topic
    // words / description of facts the sender cannot read. Strip it before
    // rendering, exactly as the recall navigator does
    // (`recall_nav::open_projected`); the structured card fields the consumer
    // legitimately needs (title, wiki_type, owner) are returned separately in
    // the JSON below. A page without a testata is body-only already.
    let body = mwe_core::wiki::MarkdownDoc::parse(&raw).map_or(raw, |doc| doc.body);
    let effective_acl_default = state
        .tree
        .resolve_scope_principal(meta)
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    // Authoritative per-fact ACL from the engine DB — enforcement reads
    // it by fact key, the inline attributes only cover unindexed
    // regions. A failed load is a hard error: serving the page on
    // weaker gating is not a degradation, it is a leak. `_active` drops
    // superseded/deleted rows so a retired region still on the page
    // redacts fail-closed instead of surfacing to its old audience.
    let source_path = handle.rel_dir().join(page);
    let db_acl =
        mwe_core::fact_index::page_acl_map_active(&state.pool, &source_path.to_string_lossy())
            .await
            .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let rendered = mwe_core::render::render_for_sender(
        &body,
        &db_acl,
        &effective_acl_default,
        &identity.sender_id,
        &sender_groups,
    );
    Ok(json!({
        "wiki_id": meta.wiki_id.as_str(),
        "page": page_rel,
        "title": meta.title,
        "wiki_type": meta.wiki_type,
        "owner": effective_acl_default.to_string(),
        "content_rendered_for_sender": rendered.text,
        // There is no `fully_redacted` boolean. A caller that
        // needs to distinguish "page is entirely private" from "page
        // has some hidden regions" reads it from `content_rendered_for_sender`
        // itself — the body equals the canonical callout
        // `> [!redacted] This entire page is private.` when the collapse
        // fires. The detection lives inside `render_for_sender`.
        "redacted_count": rendered.blocks_redacted,
        "children": meta.children.iter().map(|c| json!({
            "wiki_id": c.wiki_id,
            "slug": c.slug,
            "wiki_type": c.wiki_type,
        })).collect::<Vec<_>>(),
        "parent_wiki_id": meta.parent_wiki_id.as_ref().map(|p| p.as_str().to_owned()),
    }))
}

#[derive(Debug, Deserialize)]
struct WikiSearchArgs {
    query: String,
    sender_id: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    scope: Option<WikiSearchScope>,
}

#[derive(Debug, Deserialize, Default)]
struct WikiSearchScope {
    #[serde(default)]
    owner_ids: Vec<String>,
    #[serde(default)]
    wiki_types: Vec<String>,
    /// Filter hits down to wikis whose
    /// Which corpus to search. `Some(false)` searches the **fact**
    /// store (standard-wiki memory), `Some(true)` searches the
    /// **section** index (smart-wiki documentation), `None` searches
    /// both and merges the ranking.
    ///
    /// This selects the corpus **before** ranking, so the caller always
    /// gets up to `top_k` hits. It used to be a post-filter over a
    /// mixed top-K, which silently shrank the result set — often to
    /// nothing — whenever the discarded family dominated the ranking.
    #[serde(default)]
    smart: Option<bool>,
    /// The dated-query selector (ISO-8601): keep only facts whose
    /// validity window contains this instant — "what was true on June
    /// 4th?". Maps to `FactFilters::valid_at`. Distinct from the
    /// default behaviour, where a closed window only down-ranks.
    #[serde(default)]
    valid_at: Option<String>,
}

#[allow(
    clippy::too_many_lines,
    reason = "post-filter resolves wiki_type per hit + family allowlist inline; splitting hides the linear filter pipeline"
)]
pub(super) async fn call_wiki_search(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiSearchArgs = parse_args(&args)?;
    forbid_sender_mismatch(identity, args.sender_id.as_deref())?;
    let sender_groups = enrollment::groups_for(&state.pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let sender = SenderContext {
        sender_id: identity.sender_id.clone(),
        sender_groups,
    };
    let owner_principal = args
        .scope
        .as_ref()
        .and_then(|s| s.owner_ids.first())
        .map(|s| s.parse::<mwe_core::types::Principal>())
        .transpose()
        .map_err(|e| invalid_input(format!("scope.owner_ids[0]: {e}")))?;
    let valid_at = args
        .scope
        .as_ref()
        .and_then(|s| s.valid_at.as_deref())
        .map(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .map(|_| raw.to_owned())
                .map_err(|e| invalid_input(format!("scope.valid_at: {e}")))
        })
        .transpose()?;
    let filters = FactFilters {
        owner_id: owner_principal,
        valid_at,
        ..Default::default()
    };
    let top_k = args.top_k.unwrap_or(20);
    // Corpus selection happens HERE, before ranking — `scope.smart` picks
    // a table, it is no longer a post-filter over a mixed top-K. That is
    // what makes the caller's `top_k` honoured: asking for 20 non-smart
    // hits used to return whatever survived after the smart hits were
    // discarded, which on a documentation-heavy store was near zero.
    let embedder = Arc::clone(&state.embedder);
    let hits: Vec<recall::SearchHit> = match args.scope.as_ref().and_then(|s| s.smart) {
        Some(false) => {
            recall::wiki_search(&state.pool, embedder, &args.query, top_k, filters, &sender)
                .await
                .map(|v| {
                    v.into_iter()
                        .map(|h| recall::SearchHit::Fact(Box::new(h)))
                        .collect()
                })
        },
        Some(true) => recall::search_sections(&state.pool, embedder, &args.query, top_k, &sender)
            .await
            .map(|v| v.into_iter().map(recall::SearchHit::Section).collect()),
        None => {
            // Whole-corpus search: the fact corpus always, a project's
            // sections only when the turn names it or its signpost
            // description clears the funnel floor (operator knob, hot
            // reloaded from the recall-settings panel).
            let smart_floor = state
                .recall
                .read()
                .expect("recall settings rwlock poisoned")
                .resolved_ingest_policy()
                .smart_corpus_floor;
            recall::search_all(
                &state.pool,
                embedder,
                &args.query,
                top_k,
                filters,
                smart_floor,
                &sender,
            )
            .await
        },
    }
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;

    let allowed_types: Option<std::collections::HashSet<String>> = args
        .scope
        .as_ref()
        .filter(|s| !s.wiki_types.is_empty())
        .map(|s| s.wiki_types.iter().cloned().collect());

    // `wiki_type` is a free-form label that still lives only in
    // `_meta.md`, so this axis stays a per-hit tree lookup (cached).
    // Unknown wiki_ids — the hit's wiki was deleted between recall and
    // filter — drop out of an explicit type filter and pass otherwise.
    let mut type_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut filtered: Vec<recall::SearchHit> = Vec::new();
    for hit in hits {
        if let Some(allow) = allowed_types.as_ref() {
            let wiki_id = hit.wiki_id().to_owned();
            let resolved = type_cache.entry(wiki_id).or_insert_with_key(|id| {
                WikiId::parse(id).ok().and_then(|parsed| {
                    state
                        .tree
                        .locate(&parsed)
                        .ok()
                        .map(|h| h.meta().wiki_type.clone())
                })
            });
            let resolved = resolved.clone();
            let Some(t) = resolved else {
                continue;
            };
            if !allow.contains(&t) {
                continue;
            }
        }
        // Visibility is already settled: fact hits passed the per-fragment
        // ACL check, section hits came only from wikis the sender may read.
        filtered.push(hit);
    }

    let total = filtered.len();
    let results: Vec<Value> = filtered
        .into_iter()
        .map(|h| match h {
            recall::SearchHit::Fact(f) => json!({
                "wiki_id": f.wiki_id,
                "kind": "fact",
                "fact_id": f.fact_id.as_str(),
                "snippet": f.text,
                "score": f.score,
            }),
            recall::SearchHit::Section(s) => json!({
                "wiki_id": s.wiki_id,
                "kind": "section",
                // Sections are keyed by position, not by a fact id: the
                // handle is stable across reindexes, a minted id was not.
                "section": s.handle(),
                "source_path": s.source_path,
                "heading_path": s.heading_path,
                "snippet": s.text,
                "score": s.score,
            }),
        })
        .collect();
    Ok(json!({
        "results": results,
        "total": total,
        // scope_hint is `null` whenever the filters are
        // honoured (they always are now). Kept in the response shape
        // so downstream consumers that learned to look for it earlier
        // don't crash on a missing key.
        "scope_hint": serde_json::Value::Null,
    }))
}

#[derive(Debug, Deserialize)]
struct WikiNavigateArgs {
    query: String,
    sender_id: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
    /// 24b seed family C — caller-supplied topic needles (free text). When
    /// present (with or alongside `owners`), the query-extraction fallback (B)
    /// is skipped.
    #[serde(default)]
    topics: Vec<String>,
    /// 24b seed family C — caller-supplied owner principals (`user:<id>` /
    /// `group:<id>`). Unparseable entries are ignored.
    #[serde(default)]
    owners: Vec<String>,
}

/// Resolve the navigator's `(topics, owners)` seeds for `wiki_navigate`
/// (roadmap 24b cascade): **C** — the caller named `topics`/`owners` — wins;
/// otherwise **B** extracts them from the query via the navigator slot
/// ([`mwe_core::recall_nav::extract_query_seeds`], which degrades to empty →
/// **A**, principal + RAG only). Unparseable owner principals are dropped.
/// The third element labels which rung fired (the recall trace journals it).
async fn navigate_seeds(
    state: &McpState,
    nav_llm: &dyn mwe_core::llm::LlmBackend,
    args: &WikiNavigateArgs,
) -> (Vec<String>, Vec<mwe_core::types::Principal>, &'static str) {
    if !args.topics.is_empty() || !args.owners.is_empty() {
        let owners = args
            .owners
            .iter()
            .filter_map(|o| o.parse::<mwe_core::types::Principal>().ok())
            .collect();
        (args.topics.clone(), owners, "caller")
    } else {
        let (topics, owners) = mwe_core::recall_nav::extract_query_seeds(
            &state.pool,
            &state.workdir,
            nav_llm,
            &args.query,
        )
        .await;
        let mode = if topics.is_empty() && owners.is_empty() {
            "principal_rag_only"
        } else {
            "query_extraction"
        };
        (topics, owners, mode)
    }
}

/// `wiki_navigate` — deep recall via the funnel navigator (the consumer
/// counterpart of the ingest-side navigation). Whole visible corpus,
/// ACL-filtered; the caller's principal seeds anchor the fan without
/// leading it by construction (`WEIGHT_PRINCIPAL` sits on the topic-wiki
/// rung, so a stronger RAG hit goes first). Returns the
/// navigated prose fragments **with their `(wiki, page)`** (the path that
/// built the context) **and** the flat hits, so depth is a superset of the
/// breadth `wiki_search` would have returned. Smart wikis are funnel-skipped
/// (handled in `recall_nav`); their content still surfaces via the flat
/// component. Degrades to flat-only when no `navigator` LLM slot is wired.
#[allow(
    clippy::too_many_lines,
    reason = "two-corpus flat recall + funnel + JSON shaping + trace live as one linear handler; splitting hides the order the pieces depend on"
)]
pub(super) async fn call_wiki_navigate(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiNavigateArgs = parse_args(&args)?;
    forbid_sender_mismatch(identity, args.sender_id.as_deref())?;
    let sender_groups = enrollment::groups_for(&state.pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let sender = SenderContext {
        sender_id: identity.sender_id.clone(),
        sender_groups,
    };

    // Flat recall over the fact corpus: the breadth floor returned to the
    // caller AND the RAG seeds that feed the funnel. The funnel walks
    // wikilinks and page structure, which only standard wikis carry, so
    // the seeds are facts by construction.
    let top_k = args.top_k.unwrap_or(20);
    let flat_hits = recall::wiki_search(
        &state.pool,
        Arc::clone(&state.embedder),
        &args.query,
        top_k,
        FactFilters::default(),
        &sender,
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;

    // Smart-wiki documentation: surfaced in the flat floor alongside the
    // facts, never fed to the funnel.
    let section_hits = recall::search_sections(
        &state.pool,
        Arc::clone(&state.embedder),
        &args.query,
        top_k,
        &sender,
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;

    // Funnel (depth): only when a `navigator` LLM slot is wired — otherwise
    // degrade to flat-only (flat runs on the embedder alone).
    let start = std::time::Instant::now();
    let nav_policy = state
        .recall
        .read()
        .expect("recall settings rwlock poisoned")
        .resolved_ingest_policy()
        .nav;
    let funnel = run_navigate_funnel(state, &sender, &args, &flat_hits, &nav_policy).await?;
    let NavigateFunnel {
        entries,
        navigated,
        navigator_available,
        seed_mode,
        seed_topics,
        seed_owners,
    } = funnel;

    let navigated_json: Vec<Value> = navigated
        .fragments
        .iter()
        .map(|f| {
            json!({
                "wiki_id": f.wiki_id,
                "page": f.page.to_string_lossy(),
                "text": f.text,
            })
        })
        .collect();
    // The flat floor is the whole visible corpus, so it carries the
    // smart-wiki sections too — they are funnel-skipped (free markdown has
    // no wikilink/heading structure the funnel walks) but must still
    // surface here. Merged into one score-ordered list.
    let mut flat_ranked: Vec<(f32, Value)> = flat_hits
        .iter()
        .map(|h| {
            (
                h.score,
                json!({
                    "wiki_id": h.wiki_id,
                    "kind": "fact",
                    "fact_id": h.fact_id.as_str(),
                    "snippet": h.text,
                    "score": h.score,
                }),
            )
        })
        .collect();
    flat_ranked.extend(section_hits.iter().map(|s| {
        (
            s.score,
            json!({
                "wiki_id": s.wiki_id,
                "kind": "section",
                "section": s.handle(),
                "source_path": s.source_path,
                "heading_path": s.heading_path,
                "snippet": s.text,
                "score": s.score,
            }),
        )
    }));
    flat_ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Less));
    flat_ranked.truncate(top_k);
    let flat_json: Vec<Value> = flat_ranked.into_iter().map(|(_, v)| v).collect();

    let result = json!({
        "navigated": navigated_json,
        "hops": navigated.hops,
        "truncated": navigated.truncated,
        "flat": flat_json,
        "navigator_available": navigator_available,
    });

    record_navigate_trace(NavigateTraceParts {
        state,
        consumer: identity.consumer_id.as_deref(),
        sender_id: &sender.sender_id,
        query: &args.query,
        seed_mode,
        seed_topics,
        seed_owners: &seed_owners,
        flat_hits: &flat_hits,
        entries: &entries,
        navigated: &navigated,
        navigator_available,
        char_budget: nav_policy.char_budget,
        result: &result,
        took: start.elapsed(),
    })
    .await;

    Ok(result)
}

/// The funnel half of a `wiki_navigate` run: seeds resolved (with the rung
/// that fired), the fan, the outcome — [`Default`]-empty when no `navigator`
/// slot is wired (flat-only degradation).
struct NavigateFunnel {
    entries: Vec<mwe_core::recall_nav::EntryPoint>,
    navigated: mwe_core::recall_nav::NavigationOutcome,
    navigator_available: bool,
    seed_mode: &'static str,
    seed_topics: Vec<String>,
    seed_owners: Vec<mwe_core::types::Principal>,
}

/// Resolve seeds (the 24b cascade), gather the fan and run the funnel —
/// or degrade to the empty outcome when no `navigator` slot is wired.
async fn run_navigate_funnel(
    state: &McpState,
    sender: &SenderContext,
    args: &WikiNavigateArgs,
    flat_hits: &[mwe_core::recall::RecallHit],
    nav_policy: &mwe_core::recall_nav::NavigatorPolicy,
) -> Result<NavigateFunnel, ToolError> {
    let Some(nav_llm) = build_navigator(state) else {
        return Ok(NavigateFunnel {
            entries: Vec::new(),
            navigated: mwe_core::recall_nav::NavigationOutcome::default(),
            navigator_available: false,
            seed_mode: "principal_rag_only",
            seed_topics: Vec::new(),
            seed_owners: Vec::new(),
        });
    };
    let (topics, owners, seed_mode) = navigate_seeds(state, nav_llm.as_ref(), args).await;
    let entries = mwe_core::recall_nav::gather_entry_points(
        &state.pool,
        &state.tree,
        sender,
        &topics,
        &owners,
        flat_hits,
        &[], // situational — host-supplied only
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let navigated = mwe_core::recall_nav::navigate(
        &state.pool,
        &state.tree,
        nav_llm.as_ref(),
        sender,
        &args.query,
        &entries,
        nav_policy,
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    Ok(NavigateFunnel {
        entries,
        navigated,
        navigator_available: true,
        seed_mode,
        seed_topics: topics,
        seed_owners: owners,
    })
}

/// Everything [`record_navigate_trace`] journals for one `wiki_navigate` run.
struct NavigateTraceParts<'a> {
    state: &'a McpState,
    consumer: Option<&'a str>,
    sender_id: &'a str,
    query: &'a str,
    seed_mode: &'a str,
    seed_topics: Vec<String>,
    seed_owners: &'a [mwe_core::types::Principal],
    flat_hits: &'a [mwe_core::recall::RecallHit],
    entries: &'a [mwe_core::recall_nav::EntryPoint],
    navigated: &'a mwe_core::recall_nav::NavigationOutcome,
    navigator_available: bool,
    char_budget: usize,
    result: &'a Value,
    took: std::time::Duration,
}

/// Journal the route a `wiki_navigate` run took (the admin Traces page) —
/// best-effort telemetry, never a tool failure. The injected block is the
/// result payload the consumer receives, verbatim.
async fn record_navigate_trace(parts: NavigateTraceParts<'_>) {
    use mwe_core::recall_trace::{self, RecallTrace, TraceEntryPoint, TraceHit, TraceSource};
    let trace = RecallTrace {
        version: recall_trace::TRACE_PAYLOAD_VERSION,
        consumer: parts.consumer.map(str::to_owned),
        turn_text: recall_trace::cap_turn_text(parts.query),
        intent: None,
        seed_mode: parts.seed_mode.to_owned(),
        topics: parts.seed_topics,
        owners: parts.seed_owners.iter().map(ToString::to_string).collect(),
        flat_hits: parts.flat_hits.iter().map(TraceHit::from_hit).collect(),
        fresh_hits: Vec::new(),
        due_soon: Vec::new(),
        entry_points: parts
            .entries
            .iter()
            .map(TraceEntryPoint::from_entry)
            .collect(),
        hops: parts.navigated.trace.clone(),
        nav_stop: parts
            .navigator_available
            .then(|| parts.navigated.stop.as_str().to_owned()),
        char_budget: parts.char_budget,
        chars_collected: parts.navigated.fragments.iter().map(|f| f.text.len()).sum(),
        truncated: parts.navigated.truncated,
        injected_block: serde_json::to_string_pretty(parts.result).ok(),
        rules_block: None,
        took_ms: u64::try_from(parts.took.as_millis()).unwrap_or(u64::MAX),
    };
    if let Err(err) = recall_trace::record_trace(
        &parts.state.pool,
        TraceSource::Navigate,
        parts.sender_id,
        &trace,
    )
    .await
    {
        tracing::warn!(error = %err, "wiki_navigate: recall-trace journal write failed (ignored)");
    }
}

// ============================================================
// E — tool_log_search / wiki_lint
// ============================================================

#[derive(Debug, Deserialize, Default)]
struct ToolLogSearchArgs {
    #[serde(default)]
    sender_id_filter: Option<String>,
    #[serde(default)]
    tool_name_filter: Option<String>,
    #[serde(default)]
    date_range: Option<DateRangeArg>,
    #[serde(default)]
    result_status: Option<String>,
    #[serde(default)]
    top_k: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct DateRangeArg {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

pub(super) async fn call_tool_log_search(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    // Guests share one sender id: opening the self-scoped view would show
    // one stranger another stranger's turns.
    forbid_guest(identity, "tool_log_search")?;
    let mut args: ToolLogSearchArgs = parse_args(&args)?;
    // Non-admin callers can only see their own rows.
    if !identity.is_admin {
        match &args.sender_id_filter {
            Some(s) if s != &identity.sender_id => {
                return Err(ToolError::new(
                    ToolErrorClass::SenderUnauthorized,
                    "non-admin callers can only query their own sender_id",
                ));
            },
            None => args.sender_id_filter = Some(identity.sender_id.clone()),
            _ => {},
        }
    }
    let status = match args.result_status.as_deref() {
        Some("success") => Some(ResultStatus::Success),
        Some("error") => Some(ResultStatus::Error),
        Some(other) => return Err(invalid_input(format!("unknown result_status: {other}"))),
        None => None,
    };
    let filters = SearchFilters {
        sender_id_filter: args.sender_id_filter,
        tool_name_filter: args.tool_name_filter,
        date_from: args.date_range.as_ref().and_then(|d| d.from.clone()),
        date_to: args.date_range.as_ref().and_then(|d| d.to.clone()),
        result_status: status,
        top_k: args.top_k,
    };
    let rows = audit::search(&state.pool, &filters)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let total = rows.len();
    Ok(json!({
        "entries": rows.into_iter().map(|r| json!({
            "timestamp": r.timestamp,
            "tool_name": r.tool_name,
            "sender_id": r.sender_id,
            "device_label": r.device_label,
            "rate_limit_id": r.rate_limit_id,
            "args_hash": r.args_hash,
            "result_status": if r.error.is_none() { "success" } else { "error" },
            "latency_ms": r.latency_ms,
            "cost_estimate": r.cost_estimate,
            "error_code": r.error,
        })).collect::<Vec<_>>(),
        "total": total,
    }))
}

#[derive(Debug, Deserialize, Default)]
struct WikiLintArgs {
    #[serde(default)]
    scope: Option<WikiLintScope>,
    #[serde(default)]
    checks: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct WikiLintScope {
    #[serde(default)]
    wiki_ids: Vec<String>,
}

pub(super) async fn call_wiki_lint(
    state: &McpState,
    _identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiLintArgs = parse_args(&args)?;
    let scope = LintScope {
        wiki_ids: args.scope.map(|s| s.wiki_ids).filter(|v| !v.is_empty()),
    };
    let checks: Vec<Check> = match args.checks {
        None => Check::all().to_vec(),
        Some(strs) => {
            let mut out = Vec::with_capacity(strs.len());
            for s in strs {
                let c = Check::parse(&s)
                    .ok_or_else(|| invalid_input(format!("unknown check name: {s}")))?;
                out.push(c);
            }
            out
        },
    };
    let report = lint::run(&state.pool, &state.tree, &scope, &checks)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    Ok(json!({
        "issues": report.issues,
        "summary": {
            "total": report.total,
            "by_severity": report.by_severity,
            "by_check": report.by_check,
        },
    }))
}

// ============================================================
// F — consumer_register / wiki_ingest_external
// ============================================================

#[derive(Debug, Deserialize)]
struct ConsumerRegisterArgs {
    consumer_id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    callback_url: Option<String>,
    #[serde(default)]
    kinds_subscribed: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Value>,
}

pub(super) async fn call_consumer_register(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    // A guest-turn registration would bind `guest` as the consumer's
    // system user, corrupting the diagonal binding below.
    forbid_guest(identity, "consumer_register")?;
    let args: ConsumerRegisterArgs = parse_args(&args)?;
    // Diagonal identity model: a *standard* consumer *is* a system user, so
    // bind its own `sender_id` as this registration's `system_user_id`
    // (migration 0029). A standard caller registers as itself — it has not
    // (and must not) set `X-MWE-Act-As` on the setup call — so `sender_id`
    // here is the bot's own credential-less identity. Smart consumers are
    // their human owner (Pattern A) and carry no bot identity, so leave the
    // binding untouched.
    let system_user_id = identity
        .consumer_class
        .is_standard()
        .then_some(identity.sender_id.as_str());
    let outcome = consumers::register(
        &state.pool,
        &consumers::RegisterRequest {
            consumer_id: &args.consumer_id,
            display_name: args.display_name.as_deref(),
            callback_url: args.callback_url.as_deref(),
            kinds_subscribed: args.kinds_subscribed,
            metadata: args.metadata,
            system_user_id,
        },
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    Ok(json!({
        "registered": true,
        "fresh_registration": outcome.fresh_registration,
        "consumer_secret": outcome.consumer_secret,
        "registered_at": outcome.registered_at,
    }))
}

#[derive(Debug, Deserialize)]
struct WikiIngestExternalArgs {
    source: WikiIngestExternalSource,
    /// The trusted text seam: consumer-supplied extraction of the source
    /// bytes (mirrors `attachments[].description` in spirit). Required for
    /// non-textual media.
    #[serde(default)]
    text: Option<String>,
    /// `consult` | `dossier` | `dissolve` — forces the dial; absent = the
    /// classifier proposes.
    #[serde(default)]
    disposition: Option<String>,
    /// `prose` | `dialogue` — forces the segmentation shape.
    #[serde(default)]
    format: Option<String>,
    /// Title hint (e.g. the original filename).
    #[serde(default)]
    title: Option<String>,
    /// The document's semantic clock (ISO-8601).
    #[serde(default)]
    occurred_at: Option<String>,
    /// `always` | `never` — forces the inline→media promotion backstop
    /// (roadmap 46); absent = the shape heuristic decides. Meaningful
    /// for `source.type == "inline"` only.
    #[serde(default)]
    promote: Option<String>,
    #[serde(default)]
    dry_run: bool,
    /// Bypass the (text, owner) idempotency check.
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct WikiIngestExternalSource {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    catalog_id: Option<String>,
}

/// Map a core document error onto the wire vocabulary.
fn map_document_err(e: &mwe_core::document::DocumentError) -> ToolError {
    use mwe_core::document::DocumentError as E;
    match e {
        E::Invalid(msg) => invalid_input(msg.clone()),
        E::Llm(inner) => {
            ToolError::new(ToolErrorClass::ServiceUnavailable, format!("llm: {inner}"))
        },
        other => ToolError::new(ToolErrorClass::InternalError, other.to_string()),
    }
}

/// First non-empty line of `text`, capped at 80 chars — the synthesized
/// caption of a promoted blob (what the dashboard media list shows).
fn first_line_excerpt(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut s: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        s.push('…');
    }
    Some(s)
}

/// Verbatim source promotion, the mechanics (roadmap 46b): materialise
/// pasted text as a content-addressed blob + `media_catalog` row (kind
/// `doc`, mime `text/plain`) so the document rail cites a real
/// original. The blob bytes are the text verbatim — the blob's sha256
/// and the document job's `text_sha256` must keep hashing the same
/// bytes so the two dedup layers move together, and retries stay
/// idempotent on both. Ordering is blob → row → job (the job is the
/// caller's next step): a failure after the row leaves only an orphan
/// catalog entry that the next attempt dedups onto.
async fn promote_text_to_media(
    state: &McpState,
    identity: &IdentityProfile,
    owner: &mwe_core::types::Principal,
    text: &str,
    title_hint: Option<&str>,
) -> Result<mwe_core::media::MediaRow, ToolError> {
    let caption = title_hint
        .map(str::to_owned)
        .or_else(|| first_line_excerpt(text));
    let outcome = mwe_core::media::store_media(
        &state.pool,
        &state.workdir,
        mwe_core::media::NewMedia {
            bytes: text.as_bytes().to_vec(),
            kind: "doc".into(),
            // Must stay re-readable by the media arm's textual-mime
            // check (`text/*`), or the pipeline would refuse its own
            // artifact on a later re-resolution.
            mime: "text/plain".into(),
            owner: owner.clone(),
            uploaded_by_consumer: identity.consumer_id.clone(),
            caption,
            description: Some("promoted verbatim from pasted inline text".into()),
            original_filename: None,
        },
    )
    .await
    .map_err(|e| {
        ToolError::new(
            ToolErrorClass::InternalError,
            format!("verbatim promotion: {e}"),
        )
    })?;
    tracing::info!(
        catalog_id = %outcome.row.catalog_id,
        deduplicated = outcome.deduplicated,
        size_bytes = outcome.row.size_bytes,
        "document: inline text promoted to the media rail"
    );
    Ok(outcome.row)
}

/// The resolved source of one document-ingest call: text + provenance +
/// the ACL the extracted facts inherit.
struct ResolvedDocumentSource {
    source_kind: String,
    source_ref: Option<String>,
    text: String,
    title_hint: Option<String>,
    occurred_at: Option<String>,
    owner: mwe_core::types::Principal,
    allow: Vec<mwe_core::types::Principal>,
}

/// Verbatim source promotion inside source resolution (roadmap 46).
///
/// Document-shaped inline text is backstopped onto the media rail so
/// the original stays citable; the resolved source then looks exactly
/// like a caller-uploaded `source.type=media` — the media-gated
/// machinery downstream (embed marker, blob-ACL widening, fact-detail
/// link) engages on its own.
async fn resolve_promoted_inline(
    state: &McpState,
    identity: &IdentityProfile,
    args: &WikiIngestExternalArgs,
    effective_owner: mwe_core::types::Principal,
    content: String,
) -> Result<ResolvedDocumentSource, ToolError> {
    let row = promote_text_to_media(
        state,
        identity,
        &effective_owner,
        &content,
        args.title.as_deref(),
    )
    .await?;
    Ok(ResolvedDocumentSource {
        source_kind: "media".into(),
        source_ref: Some(row.catalog_id.as_str().to_owned()),
        text: content,
        title_hint: args.title.clone().or(row.caption),
        occurred_at: args
            .occurred_at
            .clone()
            .or_else(|| Some(row.created_at.clone())),
        owner: row.owner_id,
        allow: row.allow_ids,
    })
}

async fn resolve_document_source(
    state: &McpState,
    identity: &IdentityProfile,
    args: &WikiIngestExternalArgs,
    promote_to_media: bool,
) -> Result<ResolvedDocumentSource, ToolError> {
    let effective_owner: mwe_core::types::Principal = format!("user:{}", identity.sender_id)
        .parse()
        .map_err(|e| {
            ToolError::new(
                ToolErrorClass::InternalError,
                format!("effective sender: {e}"),
            )
        })?;
    match args.source.kind.as_str() {
        "inline" => {
            let content = args.source.content.clone().ok_or_else(|| {
                invalid_input("source.content required when source.type == 'inline'")
            })?;
            if promote_to_media {
                return resolve_promoted_inline(state, identity, args, effective_owner, content)
                    .await;
            }
            Ok(ResolvedDocumentSource {
                source_kind: "inline".into(),
                source_ref: None,
                text: content,
                title_hint: args.title.clone(),
                occurred_at: args.occurred_at.clone(),
                owner: effective_owner,
                allow: Vec::new(),
            })
        },
        "media" => {
            let raw = args.source.catalog_id.as_deref().ok_or_else(|| {
                invalid_input("source.catalog_id required when source.type == 'media'")
            })?;
            let catalog_id = mwe_core::types::CatalogId::parse(raw)
                .map_err(|e| invalid_input(format!("source.catalog_id: {e}")))?;
            let row = mwe_core::media::find_by_id(&state.pool, &catalog_id)
                .await
                .map_err(|e| {
                    ToolError::new(ToolErrorClass::InternalError, format!("media lookup: {e}"))
                })?
                .ok_or_else(|| {
                    invalid_input(format!(
                        "source.catalog_id: no catalog row for `{catalog_id}` — upload via POST /media first"
                    ))
                })?;
            let groups = mwe_core::enrollment::groups_for(&state.pool, &identity.sender_id)
                .await
                .map_err(|e| {
                    ToolError::new(ToolErrorClass::InternalError, format!("groups_for: {e}"))
                })?;
            if !mwe_core::media::row_visible_to(&row, &identity.sender_id, &groups) {
                return Err(ToolError::new(
                    ToolErrorClass::SenderUnauthorized,
                    format!(
                        "source.catalog_id: `{catalog_id}` is not readable by the effective sender"
                    ),
                ));
            }
            // The trusted text seam wins; otherwise the server extracts —
            // v1 reads UTF-8 textual blobs only (PDF & co. arrive via the
            // seam; extraction-from-bytes is a deployment capability).
            let text = if let Some(t) = args.text.clone() {
                t
            } else if row.mime.starts_with("text/") || row.mime == "application/markdown" {
                let blob = mwe_core::media::blob_path(&state.workdir, &row.sha256);
                let bytes = std::fs::read(&blob).map_err(|e| {
                    ToolError::new(ToolErrorClass::InternalError, format!("blob read: {e}"))
                })?;
                String::from_utf8(bytes).map_err(|_| {
                    invalid_input(format!(
                        "blob `{catalog_id}` is not valid UTF-8 — supply the extracted `text`"
                    ))
                })?
            } else {
                return Err(invalid_input(format!(
                    "blob `{catalog_id}` has non-textual mime `{}` — supply the extracted `text`",
                    row.mime
                )));
            };
            // Extracted facts inherit the catalog row's CURRENT read set:
            // monotone with what the document already was.
            Ok(ResolvedDocumentSource {
                source_kind: "media".into(),
                source_ref: Some(catalog_id.as_str().to_owned()),
                text,
                title_hint: args
                    .title
                    .clone()
                    .or_else(|| row.original_filename.clone())
                    .or_else(|| row.caption.clone()),
                occurred_at: args
                    .occurred_at
                    .clone()
                    .or_else(|| Some(row.created_at.clone())),
                owner: row.owner_id.clone(),
                allow: row.allow_ids.clone(),
            })
        },
        "file" | "git" | "url" => Err(ToolError::new(
            ToolErrorClass::NotImplementedPhaseC,
            format!("source.type=`{}` is not yet implemented", args.source.kind),
        )),
        other => Err(invalid_input(format!("unknown source.type: {other}"))),
    }
}

/// The synchronous `dry_run` branch: classify + segment, write nothing.
async fn ingest_external_dry_run(
    state: &McpState,
    llm_slot: &mwe_core::config::LlmFunctionConfig,
    resolved: &ResolvedDocumentSource,
    disposition: Option<mwe_core::document::Disposition>,
    format: Option<mwe_core::document::DocFormat>,
) -> Result<Value, ToolError> {
    use mwe_core::document;

    let llm = llm_slot
        .build_backend(LlmFunction::Ingest)
        .map_err(|e| ToolError::new(ToolErrorClass::ServiceUnavailable, format!("llm: {e}")))?;
    // Same language rule as the real run (`document::process_job`): the plan
    // the caller previews must be the plan they would get, title and summary
    // included.
    let language_directive = mwe_core::locale::render_memory_language_directive(
        mwe_core::enrollment::locale_for_principal(&state.pool, &resolved.owner)
            .await
            .unwrap_or_default()
            .as_deref(),
    );
    let input = document::ClassifyInput {
        text: &resolved.text,
        title_hint: resolved.title_hint.as_deref(),
        source_kind: &resolved.source_kind,
        occurred_at: resolved.occurred_at.as_deref(),
        forced_disposition: disposition,
        forced_format: format,
        owner: &resolved.owner,
        language_directive: &language_directive,
    };
    let plan = document::classify_document(
        llm.as_ref(),
        &state.tree,
        &state.workdir,
        &state.document_policy,
        &input,
    )
    .await
    .map_err(|e| map_document_err(&e))?;
    let segments = document::segment_document(
        &resolved.text,
        plan.format,
        resolved.occurred_at.as_deref(),
        &state.document_policy,
    );
    Ok(json!({
        "dry_run": true,
        "disposition": plan.disposition.as_str(),
        "format": plan.format.as_str(),
        "title": plan.title,
        "target_wiki_id": plan.target_wiki_id,
        "document_page": plan.page.to_string_lossy(),
        "summary": plan.summary,
        "segments_planned": segments.len(),
        "note": "dry run — nothing written; call again without dry_run to enqueue",
    }))
}

/// Validate the `promote` dial and make the 46a promotion decision.
///
/// Made before source resolution so a dry run can report
/// `would_promote` without minting anything. The forced dial wins;
/// absent = the shape heuristic decides. `promote` on a non-inline
/// source is caller confusion — rejected loudly.
fn inline_promotion_decision(args: &WikiIngestExternalArgs) -> Result<bool, ToolError> {
    use mwe_core::document;

    let promote = args
        .promote
        .as_deref()
        .map(|s| {
            document::PromoteHint::parse(s)
                .ok_or_else(|| invalid_input(format!("unknown promote: {s}")))
        })
        .transpose()?;
    if promote.is_some() && args.source.kind != "inline" {
        return Err(invalid_input(
            "promote applies to source.type == 'inline' only",
        ));
    }
    Ok(args.source.kind == "inline"
        && document::should_promote_inline(
            args.source.content.as_deref().unwrap_or(""),
            promote,
            &document::PromotionPolicy::default(),
        ))
}

pub(super) async fn call_wiki_ingest_external(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    use mwe_core::document::{self, Disposition, DocFormat};

    // Document ingest mints permanent facts — never from an unidentified
    // sender.
    forbid_guest(identity, "wiki_ingest_external")?;
    let args: WikiIngestExternalArgs = parse_args(&args)?;
    let disposition = args
        .disposition
        .as_deref()
        .map(|s| {
            Disposition::parse(s).ok_or_else(|| invalid_input(format!("unknown disposition: {s}")))
        })
        .transpose()?;
    let format = args
        .format
        .as_deref()
        .map(|s| DocFormat::parse(s).ok_or_else(|| invalid_input(format!("unknown format: {s}"))))
        .transpose()?;
    if let Some(at) = args.occurred_at.as_deref()
        && chrono::DateTime::parse_from_rfc3339(at).is_err()
    {
        return Err(invalid_input(format!(
            "occurred_at must be ISO-8601/RFC-3339, got `{at}`"
        )));
    }
    let would_promote = inline_promotion_decision(&args)?;

    // Source validation first (a `file` source must answer 501 regardless
    // of LLM availability), then the slot gate: the pipeline runs on the
    // ingest slot, so refuse rather than queueing a job no worker can run.
    let resolved =
        resolve_document_source(state, identity, &args, would_promote && !args.dry_run).await?;
    let llm_slot = state.llm_config.slot(LlmFunction::Ingest).ok_or_else(|| {
        ToolError::new(
            ToolErrorClass::ServiceUnavailable,
            "llm.ingest not configured in mwe-mcp.config.yaml",
        )
    })?;
    let effective_sender: mwe_core::types::Principal = format!("user:{}", identity.sender_id)
        .parse()
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, format!("sender: {e}")))?;

    if args.dry_run {
        let mut out =
            ingest_external_dry_run(state, llm_slot, &resolved, disposition, format).await?;
        out.as_object_mut()
            .expect("dry-run json root is an object")
            .insert("would_promote".into(), json!(would_promote));
        return Ok(out);
    }

    let promoted_catalog_id = would_promote.then(|| resolved.source_ref.clone()).flatten();
    let sender = (resolved.owner != effective_sender).then_some(effective_sender);
    let outcome = document::enqueue(
        &state.pool,
        &state.document_policy,
        document::EnqueueRequest {
            source_kind: resolved.source_kind,
            source_ref: resolved.source_ref,
            text: resolved.text,
            title_hint: resolved.title_hint,
            disposition,
            format,
            occurred_at: resolved.occurred_at,
            owner: resolved.owner,
            allow: resolved.allow,
            sender,
            force: args.force,
        },
    )
    .await
    .map_err(|e| map_document_err(&e))?;
    let mut out = json!({
        "job_id": outcome.job_id,
        "status": if outcome.existing { "existing" } else { "queued" },
        "existing": outcome.existing,
        "size_chars": outcome.size_chars,
        "note": if outcome.existing {
            "an equivalent non-failed job already exists for this document — returning it (use force to re-ingest)"
        } else {
            "queued — the worker classifies (consult/dossier/dissolve), extracts, and notifies via events_poll (document_ingested)"
        },
    });
    if let Some(catalog_id) = promoted_catalog_id {
        out.as_object_mut()
            .expect("json! root is an object")
            .insert("promoted_catalog_id".into(), json!(catalog_id));
    }
    Ok(out)
}

// ============================================================
// G — dashboard_link
// ============================================================

#[derive(Debug, Deserialize)]
struct DashboardLinkArgs {
    intent: String,
    sender_id: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default)]
    channel: Option<String>,
}

#[allow(clippy::unused_async, reason = "uniform async dispatcher signature")]
pub(super) async fn call_dashboard_link(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    // The link embeds a signed dashboard session token for the effective
    // sender — never hand one to an unidentified person.
    forbid_guest(identity, "dashboard_link")?;
    let args: DashboardLinkArgs = parse_args(&args)?;
    forbid_sender_mismatch(identity, args.sender_id.as_deref())?;
    let allowed = matches!(
        args.intent.as_str(),
        "home"
            | "modify_wiki"
            | "view_wiki"
            | "answer_proposal"
            | "archive_view"
            | "audit"
            | "costs"
            | "settings"
    );
    if !allowed {
        return Err(invalid_input(format!("unknown intent: {}", args.intent)));
    }
    if matches!(args.intent.as_str(), "settings" | "audit" | "costs") && !identity.is_admin {
        return Err(ToolError::new(
            ToolErrorClass::SenderUnauthorized,
            format!("intent `{}` is admin-only", args.intent),
        ));
    }
    let mut claims = TokenClaims::new(
        &identity.sender_id,
        DASHBOARD_DEVICE_LABEL,
        DASHBOARD_RATE_LIMIT_ID,
        DASHBOARD_LINK_TTL,
    );
    claims.is_admin = identity.is_admin;
    let token = jwt::issue(&state.secret, &claims)
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, format!("jwt: {e}")))?;

    let path = match args.intent.as_str() {
        "home" | "audit" | "costs" | "settings" => format!("/dashboard/{}", args.intent),
        "modify_wiki" | "view_wiki" => {
            let wiki_id = args
                .context
                .as_ref()
                .and_then(|c| c.get("wiki_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_input("context.wiki_id required for this intent"))?;
            format!("/dashboard/wiki/{wiki_id}")
        },
        "answer_proposal" => {
            let pid = args
                .context
                .as_ref()
                .and_then(|c| c.get("proposal_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_input("context.proposal_id required for answer_proposal"))?;
            format!("/dashboard/proposals/{pid}")
        },
        "archive_view" => "/dashboard/archive".to_owned(),
        _ => unreachable!(),
    };
    // Point the user-facing URL at the single-use redemption endpoint
    // (migration 0032): `next` carries the full deep-link (with any chat_seed
    // folded in), url-encoded once. The redemption route at
    // `/dashboard/auth/link` verifies + burns the token, sets the
    // session cookie, then redirects to `next` with the token stripped.
    let next = match args
        .context
        .as_ref()
        .and_then(|c| c.get("chat_seed"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(seed) => format!("{path}?chat_seed={}", urlencode(seed)),
        None => path,
    };
    let url = format!(
        "/dashboard/auth/link?token={token}&next={}",
        urlencode(&next)
    );
    let exp_iso = chrono::DateTime::<chrono::Utc>::from_timestamp(claims.exp, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();
    let _ = args.channel;
    Ok(json!({
        "url": url,
        "token_expires_at": exp_iso,
        "base_ttl_seconds": DASHBOARD_LINK_TTL.as_secs(),
    }))
}

// ============================================================
// H — wiki_admin_push / wiki_admin_pull
// ============================================================

#[derive(Debug, Deserialize)]
struct WikiAdminPushArgs {
    mode: String,
    #[serde(default)]
    wiki_id: Option<String>,
    #[serde(default)]
    parent_wiki_id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    wiki_type: Option<String>,
    /// Set `true` on create to forge a smart wiki (markerless,
    /// content-indexed). Optional; default `false`.
    #[serde(default)]
    smart: bool,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    pages: Vec<WikiAdminPushPageArg>,
    #[serde(default)]
    deletes: Vec<String>,
    /// Opaque `bi_<N>` (or bare `<N>`) ids to mark
    /// `processed_at = NOW()` atomically with the push.
    #[serde(default)]
    mark_processed: Vec<String>,
    /// Optimistic-concurrency guard (upsert only): the `op_log` head the
    /// caller last synced to. The push is rejected with
    /// `conflicting_op_log_head` if a newer write op landed since. Omit
    /// for last-writer-wins.
    #[serde(default)]
    expected_op_log_head: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct WikiAdminPushPageArg {
    path: String,
    content: String,
}

pub(super) async fn call_wiki_admin_push(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiAdminPushArgs = parse_args(&args)?;
    let mode = match args.mode.as_str() {
        "create" => mwe_core::wiki_admin::PushMode::Create,
        "upsert" => mwe_core::wiki_admin::PushMode::Upsert,
        other => return Err(invalid_input(format!("unknown mode: {other}"))),
    };
    let wiki_id = args
        .wiki_id
        .as_deref()
        .map(WikiId::parse)
        .transpose()
        .map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let parent_wiki_id = args
        .parent_wiki_id
        .as_deref()
        .map(WikiId::parse)
        .transpose()
        .map_err(|e| invalid_input(format!("parent_wiki_id: {e}")))?;
    let pages: Vec<mwe_core::wiki_admin::PushPage> = args
        .pages
        .into_iter()
        .map(|p| mwe_core::wiki_admin::PushPage {
            path: p.path,
            content: p.content,
        })
        .collect();
    // Pages touched this push (writes + deletes) — section-indexed
    // synchronously after the commit so a markerless smart wiki is
    // recallable immediately (the watcher is only the backstop).
    let affected: Vec<String> = pages
        .iter()
        .map(|p| p.path.clone())
        .chain(args.deletes.iter().cloned())
        .collect();
    let req = mwe_core::wiki_admin::PushRequest {
        mode,
        wiki_id,
        parent_wiki_id,
        slug: args.slug,
        title: args.title,
        wiki_type: args.wiki_type,
        smart: args.smart,
        project_id: args.project_id,
        pages,
        deletes: args.deletes,
        mark_processed: args.mark_processed,
        expected_op_log_head: args.expected_op_log_head,
    };
    let caller = admin_caller(identity);
    // MCP `wiki_admin_push` is the smart-consumer surface — the
    // `actor_kind` is fixed here so the gates documented in
    // `tool-reference.md §H` (smart token + smart-family) keep
    // firing exactly as before.
    let resp = mwe_core::wiki_admin::push(
        &state.pool,
        &state.tree,
        &caller,
        mwe_core::wiki_admin::ActorKind::SmartConsumer,
        req,
    )
    .await
    .map_err(|e| admin_error_to_tool_error(&e))?;
    // Markerless smart wikis are content-indexed: hand each touched page
    // to the reindex queue (the watcher's channel — the marker protocol
    // hides our own writes from the watcher itself) and ack immediately.
    // Embedding large pages inline would hold the HTTP response past
    // proxy timeouts (Cloudflare cuts at ~100 s) — the client would see
    // an error on a committed push and retry, multiplying the embedding
    // work; the single queue worker serialises those retries into
    // near-free idempotent re-runs instead. Best-effort either way — the
    // safety-net sweep is the backstop, so an index hiccup never fails a
    // committed push. Without a queue handle (tests, degraded boot) we
    // index inline as before.
    let mut section_indexing = "queued";
    if let Ok(handle) = state.tree.locate(&resp.wiki_id) {
        for rel in &affected {
            let abs = handle.abs_dir().join(rel);
            // Touched covers deletes too: `reindex_file` re-derives from
            // disk state and cleans up sections of a missing file.
            let queued = state.reindex_tx.as_ref().is_some_and(|tx| {
                tx.send(mwe_core::watcher::WatchedChange::Touched(abs.clone()))
                    .is_ok()
            });
            if queued {
                continue;
            }
            section_indexing = "inline";
            if let Err(e) = mwe_core::reindex::reindex_file(
                &state.pool,
                &state.tree,
                Arc::clone(&state.embedder),
                &abs,
            )
            .await
            {
                tracing::warn!(error = %e, page = %abs.display(), "wiki_admin_push: section-index failed");
            }
        }
    }
    Ok(json!({
        "wiki_id": resp.wiki_id.as_str(),
        "ops_applied": {
            "created": resp.ops_applied.created,
            "updated": resp.ops_applied.updated,
            "deleted": resp.ops_applied.deleted,
        },
        "op_log_id": resp.op_log_id,
        "warnings": resp.warnings,
        "marked_processed": resp.marked_processed,
        "authored_refs": resp.authored_refs,
        // "queued": section-indexing (embedding included) runs in the
        // background — recall over brand-new sections may lag by the
        // queue depth. "inline": indexed before this ack.
        "section_indexing": section_indexing,
        // Roadmap 48f. The moment a push lands is the moment something
        // worth signposting just happened, and the agent is already
        // here — a nudge attached to an action it already performs beats
        // "remember at the end of the session", because sessions end
        // abruptly. `null` when the signposts are current.
        "signpost_hint": signpost_hint(state, &resp.wiki_id).await,
    }))
}

/// One-line reminder to refresh this project's signposts, or `None` when
/// there is nothing to say. Best-effort: a read failure is silence, never
/// a failed push.
async fn signpost_hint(state: &McpState, wiki_id: &WikiId) -> Option<String> {
    let status = match mwe_core::signposts::status(&state.pool, &state.tree, wiki_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(error = %e, "wiki_admin_push: signpost status unavailable");
            return None;
        },
    };
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    if !status.has_description {
        return Some(format!(
            "This project has no signpost yet, so the owner's standard memory does not know it exists. Call `wiki_admin_signpost` with a short non-technical `description` (max {} chars).",
            mwe_core::signposts::MAX_DESCRIPTION_CHARS
        ));
    }
    if status.last_activity_day.as_deref() != Some(today.as_str()) {
        return Some(format!(
            "No activity signpost for {today} yet. If this push carried real work, call `wiki_admin_signpost` with an `activity` line for today (max {} chars, plain language).",
            mwe_core::signposts::MAX_ACTIVITY_CHARS
        ));
    }
    None
}

#[derive(Debug, Deserialize)]
struct WikiAdminPullArgs {
    wiki_id: String,
    /// Narrow to these wiki-relative page paths. Empty = whole wiki.
    #[serde(default)]
    paths: Vec<String>,
    /// Return each page's section shape instead of its bytes.
    #[serde(default)]
    shape: bool,
}

pub(super) async fn call_wiki_admin_pull(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiAdminPullArgs = parse_args(&args)?;
    let wiki_id =
        WikiId::parse(&args.wiki_id).map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let caller = admin_caller(identity);
    let req = mwe_core::wiki_admin::PullRequest {
        wiki_id: wiki_id.clone(),
        paths: args.paths,
        shape: args.shape,
    };
    let resp = mwe_core::wiki_admin::pull(&state.pool, &state.tree, &caller, &req)
        .await
        .map_err(|e| admin_error_to_tool_error(&e))?;
    // Roadmap 51f. In shape mode the page bytes stay on the server: what
    // comes back is what the index will make of each page, plus the one
    // line the consumer can relay to a human as-is.
    let needing_repair = resp
        .pages
        .iter()
        .filter(|p| p.shape.is_some_and(|s| s.needs_repair()))
        .count();
    let page_count = resp.pages.len();
    let pages_json: Vec<Value> = resp
        .pages
        .into_iter()
        .map(|p| match p.shape {
            Some(s) => json!({
                "path": p.path,
                "shape": {
                    "chars": s.chars,
                    "sections": s.sections,
                    "sections_sharing_a_heading": s.sections_sharing_a_heading,
                    "oversize_blocks": s.oversize_blocks,
                    "oversize_chars": s.oversize_chars,
                    "longest_block_chars": s.longest_block_chars,
                    "needs_repair": s.needs_repair(),
                    "note": s.warning(&p.path),
                },
            }),
            None => json!({ "path": p.path, "content": p.content }),
        })
        .collect();
    let mut out = json!({
        "wiki_id": wiki_id.as_str(),
        "pages": pages_json,
        "op_log_head": resp.op_log_head,
    });
    if req.shape
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "shape_summary".to_owned(),
            json!({ "pages": page_count, "pages_needing_repair": needing_repair }),
        );
    }
    Ok(out)
}

// ----- wiki_admin_lease_acquire + _release -----

#[derive(Debug, Deserialize)]
struct WikiAdminLeaseAcquireArgs {
    wiki_id: String,
    #[serde(default)]
    ttl_sec: Option<i64>,
}

pub(super) async fn call_wiki_admin_lease_acquire(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiAdminLeaseAcquireArgs = parse_args(&args)?;
    let wiki_id =
        WikiId::parse(&args.wiki_id).map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let caller = admin_caller(identity);
    let outcome =
        mwe_core::wiki_admin_leases::acquire(&state.pool, &caller, &wiki_id, args.ttl_sec)
            .await
            .map_err(|e| lease_acquire_error_to_tool_error(&e))?;
    Ok(json!({
        "lease_id": outcome.lease_id,
        "wiki_id": outcome.wiki_id.as_str(),
        "sender_id": outcome.sender_id,
        "consumer_id": outcome.consumer_id,
        "acquired_at": outcome.acquired_at,
        "expires_at": outcome.expires_at,
        "renewed": outcome.renewed,
    }))
}

#[derive(Debug, Deserialize)]
struct WikiAdminLeaseReleaseArgs {
    lease_id: String,
}

pub(super) async fn call_wiki_admin_lease_release(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiAdminLeaseReleaseArgs = parse_args(&args)?;
    let caller = admin_caller(identity);
    let outcome = mwe_core::wiki_admin_leases::release(&state.pool, &caller, &args.lease_id)
        .await
        .map_err(|e| lease_release_error_to_tool_error(&e))?;
    Ok(json!({
        "lease_id": outcome.lease_id,
        "wiki_id": outcome.wiki_id.as_str(),
        "released_at": outcome.released_at,
    }))
}

fn lease_acquire_error_to_tool_error(err: &mwe_core::wiki_admin_leases::AcquireError) -> ToolError {
    use mwe_core::wiki_admin_leases::AcquireError as E;
    let (class, msg) = match err {
        E::RequiresSmart => (
            ToolErrorClass::RequiresConsumerClassSmart,
            "requires consumer_class=smart".to_owned(),
        ),
        E::InvalidTtl { .. } => (ToolErrorClass::InvalidInput, err.to_string()),
        E::WikiLockedByLease { .. } => (ToolErrorClass::WikiLockedByLease, err.to_string()),
        E::Db(_) => (ToolErrorClass::InternalError, err.to_string()),
    };
    ToolError::new(class, msg)
}

fn lease_release_error_to_tool_error(err: &mwe_core::wiki_admin_leases::ReleaseError) -> ToolError {
    use mwe_core::wiki_admin_leases::ReleaseError as E;
    let (class, msg) = match err {
        E::RequiresSmart => (
            ToolErrorClass::RequiresConsumerClassSmart,
            "requires consumer_class=smart".to_owned(),
        ),
        E::NotHeldByCaller { .. } => (ToolErrorClass::NotFound, err.to_string()),
        E::Db(_) => (ToolErrorClass::InternalError, err.to_string()),
    };
    ToolError::new(class, msg)
}

// ----- wiki_admin_signpost -----

#[derive(Debug, Deserialize)]
struct WikiAdminSignpostArgs {
    wiki_id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    activity: Option<WikiAdminSignpostActivity>,
}

#[derive(Debug, Deserialize)]
struct WikiAdminSignpostActivity {
    day: String,
    text: String,
}

pub(super) async fn call_wiki_admin_signpost(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    // The class gate lives here, not in core: *who owns the project* is a
    // data invariant (enforced by `signposts::write`), *which MCP surface
    // may write signposts* is a property of this tool.
    if !identity.consumer_class.is_smart() {
        return Err(ToolError::new(
            ToolErrorClass::RequiresConsumerClassSmart,
            "requires consumer_class=smart".to_owned(),
        ));
    }
    let args: WikiAdminSignpostArgs = parse_args(&args)?;
    let wiki_id =
        WikiId::parse(&args.wiki_id).map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let req = mwe_core::signposts::SignpostRequest {
        project_wiki_id: wiki_id,
        description: args.description,
        activity: args.activity.map(|a| mwe_core::signposts::ActivityLine {
            day: a.day,
            text: a.text,
        }),
    };
    let report = mwe_core::signposts::write(
        &state.pool,
        &state.tree,
        Arc::clone(&state.embedder),
        &identity.sender_id,
        req,
    )
    .await
    .map_err(|e| signpost_error_to_tool_error(&e))?;
    Ok(json!({
        "owner_wiki_id": report.owner_wiki_id,
        "page": report.source_path,
        "description": report.description.as_ref().map(mwe_core::signposts::SignpostOutcome::as_str),
        "activity": report.activity.as_ref().map(mwe_core::signposts::SignpostOutcome::as_str),
        // Activity lines dropped for falling out of the rolling window.
        "retired": report.retired,
        "active_days": report.active_days,
    }))
}

fn signpost_error_to_tool_error(err: &mwe_core::signposts::SignpostError) -> ToolError {
    use mwe_core::signposts::SignpostError as E;
    let (class, msg) = match err {
        E::NotOwner { .. } | E::GroupOwned { .. } => {
            (ToolErrorClass::WikiOwnedByOtherUser, err.to_string())
        },
        E::NotSmart { .. } => (ToolErrorClass::WikiTypeNotAdminWritable, err.to_string()),
        // The caps are the point of the tool: a refusal has to say what
        // was measured, so the agent can rewrite shorter instead of
        // guessing.
        E::Empty | E::TooLong { .. } | E::BadDay { .. } | E::BadWikiId { .. } => {
            (ToolErrorClass::InvalidInput, err.to_string())
        },
        E::Wiki(_) => (ToolErrorClass::NotFound, err.to_string()),
        E::Capture(_) | E::FactIndex(_) | E::Db(_) => {
            (ToolErrorClass::InternalError, err.to_string())
        },
    };
    ToolError::new(class, msg)
}

// ----- wiki_admin_notify -----

#[derive(Debug, Deserialize)]
struct WikiAdminNotifyArgs {
    wiki_id: String,
    topic: String,
    body: String,
    source: WikiAdminNotifySource,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    target_cite: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WikiAdminNotifySource {
    kind: String,
    #[serde(rename = "ref")]
    source_ref: String,
}

pub(super) async fn call_wiki_admin_notify(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    // A briefing note is a persistent write; a `shared_with: global` wiki
    // would otherwise accept one from an unidentified sender.
    forbid_guest(identity, "wiki_admin_notify")?;
    let args: WikiAdminNotifyArgs = parse_args(&args)?;
    let wiki_id =
        WikiId::parse(&args.wiki_id).map_err(|e| invalid_input(format!("wiki_id: {e}")))?;
    let source_kind = match args.source.kind.as_str() {
        "user" => mwe_core::briefing::BriefingSourceKind::User,
        "rem" => mwe_core::briefing::BriefingSourceKind::Rem,
        "consumer" => mwe_core::briefing::BriefingSourceKind::Consumer,
        "dashboard" => mwe_core::briefing::BriefingSourceKind::Dashboard,
        other => return Err(invalid_input(format!("unknown source.kind: {other}"))),
    };
    let req = mwe_core::briefing::NotifyRequest {
        wiki_id,
        topic: args.topic,
        body: args.body,
        source_kind,
        source_ref: args.source.source_ref,
        kind: args.kind,
        target_cite: args.target_cite,
        ts: args.ts,
    };
    let caller = mwe_core::briefing::NotifyCaller {
        sender_id: identity.sender_id.clone(),
        consumer_class: identity.consumer_class,
    };
    let resp = mwe_core::briefing::notify(&state.pool, &state.tree, &caller, req)
        .await
        .map_err(|e| briefing_error_to_tool_error(&e))?;
    Ok(json!({
        "briefing_item_id": resp.briefing_item_id,
        "ts": resp.ts,
    }))
}

fn briefing_error_to_tool_error(err: &mwe_core::briefing::BriefingError) -> ToolError {
    use mwe_core::briefing::BriefingError as E;
    let (class, msg) = match err {
        E::WikiTypeNotBriefingCapable { .. } => {
            (ToolErrorClass::WikiTypeNotBriefingCapable, err.to_string())
        },
        E::SmartDoesNotNotifyOwnWiki { .. } => {
            (ToolErrorClass::SmartDoesNotNotifyOwnWiki, err.to_string())
        },
        E::StandardUsesIngestForMemory { .. } => {
            (ToolErrorClass::StandardUsesIngestForMemory, err.to_string())
        },
        E::ConsumerClassWikiFamilyMismatch { .. } => (
            ToolErrorClass::ConsumerClassWikiFamilyMismatch,
            err.to_string(),
        ),
        E::NotFound(_) => (ToolErrorClass::NotFound, err.to_string()),
        // Read-access denial + ambiguous-owner both map to
        // `sender_unauthorized` (the canonical 403 for "you can't
        // touch this wiki"). The distinct message preserves the
        // diagnostic.
        E::ReadAccessDenied { .. } | E::AmbiguousOwner { .. } => {
            (ToolErrorClass::SenderUnauthorized, err.to_string())
        },
        E::RateLimited(_) => (ToolErrorClass::RateLimited, err.to_string()),
        E::InvalidInput(_) => (ToolErrorClass::InvalidInput, err.to_string()),
        E::Wiki(_) | E::Db(_) | E::Io(_) => (ToolErrorClass::InternalError, err.to_string()),
    };
    ToolError::new(class, msg)
}

fn admin_caller(identity: &IdentityProfile) -> mwe_core::wiki_admin::AdminCaller {
    mwe_core::wiki_admin::AdminCaller {
        sender_id: identity.sender_id.clone(),
        consumer_id: identity.consumer_id.clone(),
        consumer_class: identity.consumer_class,
    }
}

fn admin_error_to_tool_error(err: &mwe_core::wiki_admin::AdminError) -> ToolError {
    use mwe_core::wiki_admin::AdminError as E;
    let (class, msg) = match err {
        E::RequiresSmart => (
            ToolErrorClass::RequiresConsumerClassSmart,
            "requires consumer_class=smart".to_owned(),
        ),
        // AmbiguousOwner shares the wire code with WikiOwnedByOtherUser:
        // both signal that the smart consumer cannot write here. The
        // distinct error message preserves the diagnostic.
        E::WikiOwnedByOtherUser { .. } | E::AmbiguousOwner { .. } => {
            (ToolErrorClass::WikiOwnedByOtherUser, err.to_string())
        },
        E::WikiTypeNotAdminWritable { .. } => {
            (ToolErrorClass::WikiTypeNotAdminWritable, err.to_string())
        },
        // The reserved `agent` label is a caller mistake about its own
        // identity, not a wiki-type capability question: plain invalid input.
        E::AgentLabelReserved { .. } => (ToolErrorClass::InvalidInput, err.to_string()),
        // The child-only template gate gets its
        // own wire code so callers can branch without string-matching.
        E::WikiTypeRequiresParent { .. } => {
            (ToolErrorClass::WikiTypeRequiresParent, err.to_string())
        },
        // NotFound surfaces as 404 — caller missed a wiki id.
        E::NotFound(_) => (ToolErrorClass::NotFound, err.to_string()),
        E::InvalidInput(_) | E::WikiIdParse(_) | E::SlugParse(_) => {
            (ToolErrorClass::InvalidInput, err.to_string())
        },
        E::WikiLockedByLease { .. } => (ToolErrorClass::WikiLockedByLease, err.to_string()),
        // Split the new variants out from `InvalidInput` so
        // smart consumers can branch on the wire class without
        // string-matching the human message.
        E::UnknownBriefingItemId { .. } => (ToolErrorClass::UnknownBriefingItemId, err.to_string()),
        E::TooManyBriefingItems { .. } => (ToolErrorClass::TooManyBriefingItems, err.to_string()),
        // Optimistic-concurrency conflict: the consumer's
        // `expected_op_log_head` is stale. Wire code `conflicting_op_log_head`
        // — the smart-consumer skill instructs a pull → re-diff → re-push.
        E::ConflictingOpLogHead { .. } => (ToolErrorClass::ConflictingOpLogHead, err.to_string()),
        E::Wiki(_) | E::Db(_) | E::Io(_) | E::FactIndex(_) => {
            (ToolErrorClass::InternalError, err.to_string())
        },
    };
    ToolError::new(class, msg)
}

// ============================================================
// I — skill_list + skill_fetch
// ============================================================

#[derive(Debug, Deserialize)]
struct SkillListArgs {
    // Reserved for future class-aware filtering; today every consumer
    // sees the full bundle. Accepted but currently unused.
    #[serde(default)]
    #[allow(dead_code)]
    consumer_class: Option<String>,
}

#[allow(
    clippy::unused_async,
    reason = "uniform async dispatch table in mcp/mod.rs awaits every tool handler; the bundled-only catalog read no longer needs to await"
)]
pub(super) async fn call_skill_list(
    _state: &McpState,
    _identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let _args: SkillListArgs = parse_args(&args)?;
    let skills = mwe_core::skills::list_bundled().map_err(|e| skill_error_to_tool_error(&e))?;
    Ok(json!({
        "skills": skills.into_iter().map(skill_summary_json).collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Deserialize)]
struct SkillFetchArgs {
    name: String,
    // Version pin reserved for the future `/skills/<name>/<version>.md`
    // plumbing; today the only on-disk version is the current one and
    // this field is accepted-but-ignored.
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<String>,
}

#[allow(
    clippy::unused_async,
    reason = "uniform async dispatch table in mcp/mod.rs awaits every tool handler; the bundled-only catalog read no longer needs to await"
)]
pub(super) async fn call_skill_fetch(
    _state: &McpState,
    _identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: SkillFetchArgs = parse_args(&args)?;
    if args.name.is_empty() {
        return Err(invalid_input("name must be non-empty"));
    }
    let (skill, content) =
        mwe_core::skills::fetch(&args.name).map_err(|e| skill_error_to_tool_error(&e))?;
    let mut out = skill_summary_json(skill);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("content".to_owned(), Value::String(content));
    }
    Ok(out)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "skill_summary_json moves the Skill's String/Vec fields into the JSON value; taking by reference would force per-field clones for no benefit"
)]
fn skill_summary_json(skill: mwe_core::skills::Skill) -> Value {
    let source = match &skill.source {
        mwe_core::skills::SkillSource::Bundled => json!({ "kind": "bundled" }),
    };
    json!({
        "name": skill.name,
        "version": skill.version,
        "description": skill.description,
        "depends_on": skill.depends_on,
        "etag": skill.etag,
        "source": source,
    })
}

fn skill_error_to_tool_error(err: &mwe_core::skills::SkillError) -> ToolError {
    use mwe_core::skills::SkillError as E;
    let (class, msg) = match err {
        E::NotFound(_) => (ToolErrorClass::NotFound, err.to_string()),
        E::MalformedBundled { .. } | E::Db(_) => (ToolErrorClass::InternalError, err.to_string()),
    };
    ToolError::new(class, msg)
}

// ============================================================
// K — smart_bootstrap / recall_core_global
// ============================================================

#[derive(Debug, Deserialize, Default)]
struct SmartBootstrapArgs {
    #[serde(default)]
    project_hint: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    briefing_limit_per_wiki: Option<i64>,
}

pub(super) async fn call_smart_bootstrap(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: SmartBootstrapArgs = parse_args(&args)?;
    let briefing_limit = match args.briefing_limit_per_wiki {
        Some(v) if v < 1 => return Err(invalid_input("briefing_limit_per_wiki must be >= 1")),
        Some(v) => Some(usize::try_from(v).unwrap_or(usize::MAX)),
        None => None,
    };
    let caller = admin_caller(identity);
    let resp = mwe_core::smart::bootstrap(
        &state.pool,
        &state.tree,
        &caller,
        mwe_core::smart::BootstrapRequest {
            project_hint: args.project_hint,
            project_id: args.project_id,
            briefing_limit_per_wiki: briefing_limit,
        },
    )
    .await
    .map_err(|e| smart_error_to_tool_error(&e))?;
    let wikis: Vec<Value> = resp
        .smart_wikis
        .into_iter()
        .map(|c| {
            let recent: Vec<Value> = c
                .recent_briefing
                .into_iter()
                .map(|bi| {
                    json!({
                        "briefing_item_id": bi.briefing_item_id,
                        "kind": bi.kind.map(mwe_core::briefing::BriefingKind::as_str),
                        "topic": bi.topic,
                        "body": bi.body,
                        "target_cite": bi.target_cite,
                        "ts": bi.ts,
                    })
                })
                .collect();
            json!({
                "wiki_id": c.wiki_id.as_str(),
                "wiki_type": c.wiki_type,
                "title": c.title,
                "slug": c.slug,
                "project_id": c.project_id,
                "matches_project_hint": c.matches_project_hint,
                "matches_project_id": c.matches_project_id,
                "is_self": c.is_self,
                "last_op_log_id": c.last_op_log_id,
                "last_op_log_ts": c.last_op_log_ts,
                "briefing_counts": {
                    "pending_observation": c.briefing_counts.pending_observation,
                    "pending_reasoning": c.briefing_counts.pending_reasoning,
                    "pending_external": c.briefing_counts.pending_external,
                    "pending_unclassified": c.briefing_counts.pending_unclassified,
                    "total": c.briefing_counts.total,
                },
                "recent_briefing": recent,
            })
        })
        .collect();
    // Roadmap 51a. Volunteered, not asked for: the agent learns a project
    // has no memory from the response it already reads, the way
    // `wiki_admin_push` volunteers `signpost_hint`. `null` unless the
    // caller passed a `project_id`.
    let first_connect = resp.first_connect.map(|fc| {
        json!({
            "project_id": fc.project_id,
            "wiki_id": fc.wiki_id.as_ref().map(mwe_core::types::WikiId::as_str),
            "wiki_found": fc.wiki_id.is_some(),
            "hint": fc.hint,
        })
    });
    Ok(json!({
        "caller_sender_id": resp.caller_sender_id,
        "project_hint": resp.project_hint,
        "first_connect": first_connect,
        "smart_wikis": wikis,
    }))
}

#[derive(Debug, Deserialize)]
struct RecallCoreGlobalArgs {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
}

pub(super) async fn call_recall_core_global(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: RecallCoreGlobalArgs = parse_args(&args)?;
    let limit = match args.limit {
        Some(v) if v < 1 => return Err(invalid_input("limit must be >= 1")),
        Some(v) => Some(usize::try_from(v).unwrap_or(usize::MAX)),
        None => None,
    };
    let caller = admin_caller(identity);
    let sender_groups = enrollment::groups_for(&state.pool, &identity.sender_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    let resp = mwe_core::smart::recall_core_global(
        &state.pool,
        &state.tree,
        Arc::clone(&state.embedder),
        &caller,
        sender_groups,
        mwe_core::smart::RecallCoreGlobalRequest {
            query: args.query,
            limit,
        },
    )
    .await
    .map_err(|e| smart_error_to_tool_error(&e))?;
    let hits: Vec<Value> = resp
        .hits
        .into_iter()
        .map(|h| {
            json!({
                "wiki_id": h.wiki_id,
                "wiki_type": h.wiki_type,
                "fact_id": h.fact_id,
                "snippet": h.snippet,
                "score": h.score,
            })
        })
        .collect();
    Ok(json!({
        "query": resp.query,
        "filter_applied": {
            "owner_user": resp.filter_applied.owner_user,
            "excluded_wiki_types": resp.filter_applied.excluded_wiki_types,
        },
        "hits": hits,
    }))
}

fn smart_error_to_tool_error(err: &mwe_core::smart::SmartError) -> ToolError {
    use mwe_core::smart::SmartError as E;
    let (class, msg) = match err {
        E::RequiresSmart => (
            ToolErrorClass::RequiresConsumerClassSmart,
            "requires consumer_class=smart".to_owned(),
        ),
        E::InvalidInput(_) => (ToolErrorClass::InvalidInput, err.to_string()),
        E::Wiki(_) | E::Briefing(_) | E::Recall(_) | E::Db(_) => {
            (ToolErrorClass::InternalError, err.to_string())
        },
    };
    ToolError::new(class, msg)
}

// ============================================================
// L — wiki_forget / wiki_forget_bulk
// ============================================================

#[derive(Debug, Deserialize)]
struct WikiForgetArgs {
    fact_id: String,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "audit-only note; accepted on the wire, not yet persisted"
    )]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WikiForgetBulkArgs {
    scope: String,
    #[serde(default)]
    wiki_id: Option<String>,
    #[serde(default)]
    page: Option<String>,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "audit-only note; accepted on the wire, not yet persisted"
    )]
    reason: Option<String>,
}

/// `wiki_forget` — authority-routed forget of a single fact, the consumer-MCP
/// half of the authority-routed forget model ([tool reference](../../../../docs/protocol/tool-reference.md)).
///
/// Routes by the caller's authority over the loaded fact:
/// - **author or admin** ([`mwe_core::acl::can_delete`]) → tombstone it now
///   (`outcome: "forgotten"`).
/// - **owner who did not author it** (subject / owning-group member,
///   [`mwe_core::acl::sender_owns`]) → forgetting needs an audience vote, and a
///   vote is opened **only from the dashboard**, never started in the background
///   by the agent (the write-authority model —
///   [identity and ACL](../../../docs/concepts/identity-and-acl.md)). So the tool does
///   **not** open a request here — it returns `outcome: "request_from_dashboard"`
///   to steer the user there.
/// - **anyone else** → refused (`sender_unauthorized`).
///
/// A missing fact is `not_found`; an already-tombstoned fact is an idempotent
/// success (`outcome: "already_forgotten"`). Opening the request **and** voting
/// on it are both dashboard-only — there is deliberately no consumer vote tool
/// and no consumer request-opening path.
pub(super) async fn call_wiki_forget(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiForgetArgs = parse_args(&args)?;
    let fact_id = mwe_core::types::FactId::parse(&args.fact_id)
        .map_err(|e| invalid_input(format!("fact_id: {e}")))?;

    // The caller acts as the JWT's sender (a bare user id); a consumer is
    // never an admin on the MCP path, so `is_admin` is the token's own flag.
    let caller = identity.sender_id.as_str();
    let is_admin = identity.is_admin;

    let row = mwe_core::fact_index::find_by_id(&state.pool, &fact_id)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?
        .ok_or_else(|| {
            ToolError::new(
                ToolErrorClass::NotFound,
                format!("fact `{}` not found", fact_id.as_str()),
            )
        })?;

    // Already forgotten → idempotent success, never an error.
    if row.deleted_at.is_some() {
        return Ok(json!({
            "outcome": "already_forgotten",
            "fact_id": fact_id.as_str(),
        }));
    }

    // Direct path: the fact's author (or an admin) deletes it now.
    // `capture::wiki_forget` owns both halves: the DB tombstone plus the
    // best-effort excision of the region's on-disk bytes.
    if mwe_core::acl::can_delete(row.sender_id.as_ref(), caller, is_admin) {
        mwe_core::capture::wiki_forget(
            &state.tree,
            &state.pool,
            state.embedder.clone(),
            &fact_id,
            "consumer_forget",
        )
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
        return Ok(json!({
            "outcome": "forgotten",
            "fact_id": fact_id.as_str(),
        }));
    }

    // Non-author path. Forgetting a fact you did not author needs an **audience
    // vote**, and a vote is opened **only from the dashboard**, where the
    // requester and the audience it polls can see why — never started in the
    // background by the agent (the write-authority model). So we
    // do NOT open a request here: if the caller owns the fact (its subject, or a
    // member of an owning group) point them at the dashboard; otherwise they have
    // no path at all → refused.
    let caller_groups = mwe_core::enrollment::groups_for(&state.pool, caller)
        .await
        .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;
    if mwe_core::acl::sender_owns(&row.owner_id, caller, &caller_groups) {
        return Ok(json!({
            "outcome": "request_from_dashboard",
            "fact_id": fact_id.as_str(),
            "detail": "You own this fact but did not author it, so forgetting it needs an \
                       audience vote — which is opened from the dashboard, not by the agent. \
                       Tell the user to open the forget request there (a `dashboard_link` helps).",
        }));
    }
    Err(ToolError::new(
        ToolErrorClass::SenderUnauthorized,
        "you can neither forget nor request to forget this fact \
         (you are not its author, owner, or an owning-group member)",
    ))
}

/// `wiki_forget_bulk` — bulk **self**-delete: tombstone every still-active fact
/// the caller authored, narrowed by `scope` (the bulk primitive of the
/// authority-routed forget model).
///
/// `scope` is required and one of:
/// - `"all"` — every fact the caller authored, across all wikis;
/// - `"wiki"` — those in `wiki_id` (required);
/// - `"page"` — those on one page (`wiki_id` + `page`, the page's file name).
///
/// Only the caller's OWN facts (`sender == ` the JWT principal) are ever
/// touched — there is no path to another author's fact, and no vote (you may
/// always delete your own). Returns `{ outcome: "forgotten_bulk", scope,
/// forgotten: <count>, ... }`.
pub(super) async fn call_wiki_forget_bulk(
    state: &McpState,
    identity: &IdentityProfile,
    args: Value,
) -> Result<Value, ToolError> {
    let args: WikiForgetBulkArgs = parse_args(&args)?;
    // The caller can only ever reach their own facts: the filter is keyed on
    // the JWT's own principal, never an arbitrary sender.
    let sender = mwe_core::types::Principal::User(identity.sender_id.clone());

    let nonempty = |o: &Option<String>| o.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    let (wiki_id, source_path): (Option<String>, Option<String>) = match args.scope.as_str() {
        "all" => (None, None),
        "wiki" => {
            let w = nonempty(&args.wiki_id)
                .ok_or_else(|| invalid_input("scope `wiki` requires `wiki_id`".to_owned()))?;
            (Some(w), None)
        },
        "page" => {
            let w = nonempty(&args.wiki_id)
                .ok_or_else(|| invalid_input("scope `page` requires `wiki_id`".to_owned()))?;
            let p = nonempty(&args.page)
                .ok_or_else(|| invalid_input("scope `page` requires `page`".to_owned()))?;
            // Canonical page source_path: `wikis/<wiki>/<page>.md` — `page` is
            // the file name within the wiki; `.md` is appended if omitted.
            let file = if std::path::Path::new(&p)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                p
            } else {
                format!("{p}.md")
            };
            (Some(w.clone()), Some(format!("wikis/{w}/{file}")))
        },
        other => {
            return Err(invalid_input(format!(
                "scope must be one of \"all\", \"wiki\", \"page\" (got {other:?})"
            )));
        },
    };

    // Snapshot the affected ids first so the disk half can excise each
    // tombstoned fact's on-disk region after the single bulk UPDATE (a row
    // slipping in between simply rides the light-dream hygiene sweep).
    let affected = mwe_core::fact_index::find_active_fact_ids_by_sender(
        &state.pool,
        &sender,
        wiki_id.as_deref(),
        source_path.as_deref(),
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;

    let forgotten = mwe_core::fact_index::mark_forgotten_by_sender(
        &state.pool,
        &sender,
        wiki_id.as_deref(),
        source_path.as_deref(),
        "consumer_forget_bulk",
    )
    .await
    .map_err(|e| ToolError::new(ToolErrorClass::InternalError, e.to_string()))?;

    // Disk half of the bulk forget: excise each retired region's bytes.
    // Best-effort per fact — the strip only touches retired rows, and a
    // failure leaves fail-closed-redacted residue for the hygiene sweep.
    for fid in &affected {
        if let Err(e) = mwe_core::reindex::strip_fact_region(
            &state.pool,
            &state.tree,
            state.embedder.clone(),
            fid,
        )
        .await
        {
            tracing::warn!(
                fact_id = fid.as_str(),
                error = %e,
                "wiki_forget_bulk: page-strip failed (redaction still applies)"
            );
        }
    }

    Ok(json!({
        "outcome": "forgotten_bulk",
        "scope": args.scope,
        "wiki_id": wiki_id,
        "source_path": source_path,
        "forgotten": forgotten,
    }))
}

/// Minimal percent-encoding of unreserved chars. Avoids pulling
/// `urlencoding` as a fresh dep; the dashboard link is server-side and
/// only carries short seed strings.
fn urlencode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for c in s.bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ingest_metadata_collects_authored_refs() {
        let meta = json!({
            "authored_refs": [
                "[[alice-proj/index]]",
                "  [[alice-proj/modules/auth]]  ",
                "",
                42,
            ]
        });
        let (_disambig, parsed) = parse_ingest_metadata(Some(&meta)).expect("parse");
        // Blanks and non-strings dropped; survivors trimmed, order kept.
        assert_eq!(
            parsed.authored_refs,
            vec![
                "[[alice-proj/index]]".to_owned(),
                "[[alice-proj/modules/auth]]".to_owned(),
            ]
        );
    }

    #[test]
    fn parse_ingest_metadata_defaults_authored_refs_to_empty() {
        let (_d, parsed) = parse_ingest_metadata(None).expect("parse");
        assert!(parsed.authored_refs.is_empty());
        // A non-array value is ignored rather than rejected.
        let meta = json!({ "authored_refs": "not-an-array" });
        let (_d, parsed) = parse_ingest_metadata(Some(&meta)).expect("parse");
        assert!(parsed.authored_refs.is_empty());
    }

    #[test]
    fn parse_ingest_metadata_reads_channel() {
        let (_d, parsed) =
            parse_ingest_metadata(Some(&json!({ "channel": "  telegram:42  " }))).expect("parse");
        assert_eq!(parsed.channel.as_deref(), Some("telegram:42"));
        // Blank normalises to unset; absent stays unset.
        let (_d, parsed) =
            parse_ingest_metadata(Some(&json!({ "channel": "   " }))).expect("parse");
        assert!(parsed.channel.is_none());
        let (_d, parsed) = parse_ingest_metadata(None).expect("parse");
        assert!(parsed.channel.is_none());
    }
}
