// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chat onnipresente — unico punto di ingresso per le chiamate LLM dal
//! cruscotto ([agentic chat](../../../../wiki/design-notes/agentic-chat.md)).
//!
//! - `GET /dashboard/chat`: stand-alone chat page (header + main + the
//!   right-side panel rendered by the layout). The page itself only
//!   carries a heading + instructions; the conversation lives inside the
//!   panel, hydrated client-side from `localStorage`.
//! - `POST /dashboard/chat`: accepts a single user turn (`text`), runs
//!   it through [`mwe_core::ingest::wiki_ingest_message`] with
//!   [`ContextHint::DashboardCommand`], and returns either:
//!     - a JSON envelope `{ user_text, response_html }` when the client
//!       sends `Accept: application/json` (the right-side panel does
//!       this);
//!     - the full HTML page with the response inline, as a
//!       no-JavaScript fallback that still works for visitors hitting
//!       `/dashboard/chat` directly.
//!
//! The conversation history is not persisted server-side. Fact
//! continuity across turns rides the auto-capture + recall path (every
//! fact the user tells the chat lands in their wiki on the spot, and the
//! next turn recalls it through `wiki_recall` on relevance) rather than a
//! replayed context window. The one exception is the agentic loop's
//! propose → confirm → act handshake: each submission replays a **bounded
//! recent `{user, assistant}` window** (sent by `chat.js`, clamped by
//! [`parse_chat_history`]) so a bare "sì" resolves against the
//! assistant's prior proposal — see the
//! [agentic chat design](../../../../wiki/design-notes/agentic-chat.md).
//! The full chat history lives in the browser's `localStorage` for the
//! user's benefit (scrollback), trimmed FIFO at 100 entries by
//! [`crate::assets`]' `chat.js`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::config::LlmFunction;
use mwe_core::enrollment;
use mwe_core::ingest::{
    self, ContextHint, IngestMetadata, IngestRequest, IngestResponse, IntentKind, MessageRole,
};
use mwe_core::llm::{ChatMessage, ChatRequest, Role, ToolCall};
use mwe_core::recall::SenderContext;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agentic::{self, AgenticContext, MAX_AGENTIC_ITERATIONS};
use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::{BackendForError, DashboardState};
use crate::ui::{components, layout};

/// Bundled default for the dashboard chat panel agentic system prompt.
///
/// Used by the hybrid loader [`mwe_core::prompts::load`] when no
/// operator override sits at `<workdir>/prompts/agentic-chat-panel.md`.
/// The verbatim prompt body lives in
/// `crates/mwe-dashboard/prompts/agentic-chat-panel.md` (frontmatter +
/// a single ```text ... ``` fenced block); see the
/// [agentic chat design](../../../../wiki/design-notes/agentic-chat.md)
/// for the design narrative. Referenced
/// from [`crate::BUNDLED_PROMPTS`] so `mwe-mcp init` materialises it
/// under the workdir.
pub const BUNDLED_AGENTIC_PROMPT_MD: &str = include_str!("../../prompts/agentic-chat-panel.md");

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/chat", get(get_page).post(post_message))
        .route("/chat/agentic", post(post_agentic))
}

#[derive(Debug, Deserialize)]
pub struct ChatSubmission {
    pub text: String,
    /// A JSON array of the most recent `{user, assistant}` turns, sent by
    /// `chat.js` from its `localStorage` scrollback so the agentic loop
    /// can see the immediately-preceding exchange. This is what makes the
    /// propose → confirm → act handshake work: a bare "sì" in a new
    /// submission resolves against the assistant's prior "vuoi che
    /// proceda?". Absent on the no-JS `post_message` fallback (and ignored
    /// there — that path does not run the agentic loop). Parsed and
    /// clamped by [`parse_chat_history`].
    #[serde(default)]
    pub history: Option<String>,
}

/// One prior conversational turn replayed into the agentic loop: the
/// user's message and the assistant's final reply (no tool trace — the
/// confirmation handshake only needs the natural-language exchange, and
/// a small local workhorse degrades on a tool-heavy replayed context).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatHistoryTurn {
    pub user: String,
    pub assistant: String,
}

/// How many recent turns to replay. A tight window: the confirmation
/// handshake needs only the last turn or two, and an over-long replayed
/// context degrades the workhorse's tool-calling (the local profile
/// budgets ≤ ~3 k input tokens). The browser keeps the full scrollback;
/// only this tail rides the wire.
const MAX_HISTORY_TURNS: usize = 6;

/// Per-message clamp on a replayed turn, so a pathological scrollback
/// entry can't blow the context budget. Generous — assistant replies are
/// 1-3 sentences by the prompt's own style rule.
const MAX_HISTORY_CHARS: usize = 4000;

/// Parse the client-sent `history` field into a bounded, chronological
/// window of replayable turns. Malformed JSON degrades to "no history"
/// rather than failing the turn; empty-on-either-side turns (errors,
/// budget-exhausted, primers) are dropped; the most recent
/// [`MAX_HISTORY_TURNS`] survive in order, each field clamped to
/// [`MAX_HISTORY_CHARS`].
#[must_use]
fn parse_chat_history(raw: Option<&str>) -> Vec<ChatHistoryTurn> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let parsed: Vec<ChatHistoryTurn> = serde_json::from_str(raw).unwrap_or_default();
    let kept: Vec<ChatHistoryTurn> = parsed
        .into_iter()
        .filter(|t| !t.user.trim().is_empty() && !t.assistant.trim().is_empty())
        .collect();
    let start = kept.len().saturating_sub(MAX_HISTORY_TURNS);
    kept[start..]
        .iter()
        .map(|t| ChatHistoryTurn {
            user: clamp_chars(&t.user, MAX_HISTORY_CHARS),
            assistant: clamp_chars(&t.assistant, MAX_HISTORY_CHARS),
        })
        .collect()
}

/// Truncate `s` to at most `max` chars (not bytes — never splits a
/// multi-byte char), appending an ellipsis when cut.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

async fn get_page(user: SessionUser) -> Html<String> {
    Html(render_page(&user, None))
}

async fn post_message(
    State(state): State<DashboardState>,
    user: SessionUser,
    headers: HeaderMap,
    axum::Form(form): axum::Form<ChatSubmission>,
) -> Result<Response> {
    let text = form.text.trim();
    if text.is_empty() {
        return Ok(render_validation_error(&user, &headers));
    }
    let turn = process_submission(&state, &user, text).await?;
    Ok(render_turn(&user, &turn, &headers))
}

/// A processed user turn — the verbatim input plus the engine's response.
///
/// Returned by [`process_submission`] so other routes (notably the
/// welcome wizard's `Save` branch per the
/// [agentic chat design](../../../../wiki/design-notes/agentic-chat.md))
/// can re-use the exact same ingest path the chat handler uses.
#[derive(Debug)]
pub struct ChatTurn {
    pub user_text: String,
    pub response: IngestResponse,
}

/// Run a single user turn through the ingest pipeline. This is the
/// single chokepoint for every LLM call originating from the dashboard:
/// the chat handler, the welcome wizard primer ([agentic chat](../../../../wiki/design-notes/agentic-chat.md)),
/// and anything future wires through here instead of calling
/// [`ingest::wiki_ingest_message`] directly.
///
/// # Errors
/// Returns [`DashboardError::Internal`] when `memory` is not wired,
/// when the `llm.ingest` slot is missing (`mwe-mcp.config.yaml` has no
/// configured ingest backend — the silent fallback is prohibited), or
/// when the underlying [`ingest::wiki_ingest_message`] errors.
pub async fn process_submission(
    state: &DashboardState,
    user: &SessionUser,
    text: &str,
) -> Result<ChatTurn> {
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal("memory handles not wired — start with `mwe-mcp serve`".into())
    })?;

    // Slot-missing is treated as a 422 (Validation) with an actionable
    // message: the operator forgot to wire `llm.ingest` in the YAML,
    // and the dashboard surfaces the fix instead of a generic 500.
    // There is still no silent fallback — the call hard-refuses; the
    // status code is the only thing tuned for UX.
    let backend = memory
        .backend_for(LlmFunction::Ingest)
        .map_err(|e| match e {
            BackendForError::SlotMissing(_) => DashboardError::Validation(
                "The `llm.ingest` LLM is not configured in mwe-mcp.config.yaml. \
             Configure the deployment's chosen model before continuing."
                    .into(),
            ),
            BackendForError::BuildFailed { detail, .. } => {
                DashboardError::Internal(format!("building ingest backend: {detail}"))
            },
        })?;

    let request = IngestRequest {
        text: text.to_owned(),
        // The dashboard chat is always the human operator typing — never the
        // agent's own reply fed back (roadmap 27 is a bridge/MCP path).
        author: MessageRole::User,
        sender_id: user.sender_id.clone(),
        // The operational dashboard chat is the operator, not a bot
        // consumer with its own wiki — no behaviour-rule routing here.
        consumer_id: None,
        recent_messages: Vec::new(),
        context_hint: ContextHint::DashboardCommand,
        disambig_choice: None,
        // The dashboard chat does not surface a per-message locale
        // input today; the orchestrator falls back to
        // `enrollment_users.locale` for the signed-in user, then to
        // the mirror clause.
        metadata: IngestMetadata::default(),
        // No media path in the operational chat — attachments enter
        // through `POST /media` + the MCP ingest only.
        attachments: Vec::new(),
    };
    // The recall knobs come from the shared operator settings (the
    // `recall:` config section, hot-editable from the recall-settings
    // page); the classifier prompt-budget knobs stay at their defaults.
    let policy = state.recall_snapshot().resolved_ingest_policy();
    // The navigator slot is optional by contract: missing or unbuildable
    // degrades to flat-only recall (navigation off), never a failed turn.
    let navigator = memory.backend_for(LlmFunction::Navigator).ok();
    let response = ingest::wiki_ingest_message(
        &state.pool,
        &memory.tree,
        Arc::clone(&memory.embedder),
        backend.as_ref(),
        navigator.as_deref(),
        request,
        &policy,
    )
    .await
    .map_err(|e| DashboardError::Internal(format!("ingest: {e}")))?;

    Ok(ChatTurn {
        user_text: text.to_owned(),
        response,
    })
}

/// Pick the right response shape for `headers`:
///
/// - `Accept: application/json` → JSON envelope `{ user_text,
///   response_html }`. The right-side panel's JS expects this so it can
///   append the rendered fragment into its scroll area without a page
///   navigation.
/// - Anything else → full HTML page with the inline response panel, the
///   no-JS fallback that still works if the visitor lands on
///   `/dashboard/chat` directly.
fn render_turn(user: &SessionUser, turn: &ChatTurn, headers: &HeaderMap) -> Response {
    if wants_json(headers) {
        let fragment = response_panel(&turn.response).into_string();
        let body = json!({
            "user_text": turn.user_text,
            "response_html": fragment,
        });
        axum::Json(body).into_response()
    } else {
        Html(render_page(user, Some(turn))).into_response()
    }
}

fn render_validation_error(user: &SessionUser, headers: &HeaderMap) -> Response {
    if wants_json(headers) {
        (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "Type a message before sending." })),
        )
            .into_response()
    } else {
        Html(render_page_with_error(
            user,
            "Type a message before sending.",
        ))
        .into_response()
    }
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("application/json"))
}

fn render_page(user: &SessionUser, turn: Option<&ChatTurn>) -> String {
    let body = html! {
        p.muted {
            "Type a natural-language command or question. The dashboard runs it through "
            code { "wiki_ingest_message" }
            " with " code { "context_hint=dashboard_command" } " so structural intents bias toward suggestions you can apply later."
        }
        p.muted {
            "The conversation lives in the panel on the right, with history kept locally in your browser. "
            "The chat is the single entry point for every LLM call from the dashboard."
        }

        @if let Some(t) = turn {
            section.chat-server-echo {
                h2 { "Last submission" }
                blockquote { (t.user_text) }
                (response_panel(&t.response))
            }
        }
    };
    layout::authenticated_page("Chat", user, &body)
}

fn render_page_with_error(user: &SessionUser, error: &str) -> String {
    let body = html! {
        (components::flash("error", error))
        p.muted {
            "Use the chat panel on the right to send a message. Submit from there is intercepted by the local "
            code { "chat.js" } " and persists history in " code { "localStorage" } "."
        }
    };
    layout::authenticated_page("Chat", user, &body)
}

/// Trace of a single tool execution recorded by the agentic loop.
/// One entry per tool call the LLM emitted, in dispatch order. The
/// dashboard surfaces these as compact "tool" bubbles above the final
/// assistant reply so the user sees exactly which `_internal.*`
/// operations the LLM invoked on its behalf (see the
/// [agentic chat design](../../../../wiki/design-notes/agentic-chat.md) —
/// transparency).
#[derive(Debug, Serialize, Clone)]
pub struct ToolCallTrace {
    /// Name of the tool that was invoked.
    pub name: String,
    /// Arguments the LLM produced, echoed verbatim for audit.
    pub arguments: serde_json::Value,
    /// JSON-stringified result body that was handed back to the
    /// model. For successful calls this matches what the dispatcher
    /// returned; for errors it is an `{"error": "..."}` object.
    pub result: String,
    /// `true` when the dispatcher surfaced an `AgenticToolError`. The
    /// loop still feeds the error back to the model so it can retry
    /// or apologise; this flag lets the UI render the trace bubble
    /// with an error style.
    pub is_error: bool,
}

/// One round-trip of the agentic loop, returned to the chat panel.
///
/// The dashboard's `chat.js` consumer renders `trace` as compact
/// tool-call bubbles (with `name` + `arguments_summary` + truncated
/// `result`) in dispatch order, then `final_message` as the bot's
/// reply bubble. Stored in `localStorage` like any other turn.
#[derive(Debug, Serialize)]
pub struct AgenticTurn {
    pub user_text: String,
    pub trace: Vec<ToolCallTrace>,
    /// The model's final natural-language reply (an empty string when
    /// the loop exhausted its iteration budget before the model said
    /// anything textual — the chat panel surfaces that case as an
    /// explicit error bubble). This is the **raw** text: it is what the
    /// chat panel replays into the next turn's confirmation window, so it
    /// must stay free of HTML.
    pub final_message: String,
    /// [`final_message`](Self::final_message) rendered to **safe HTML**
    /// for display, via [`crate::md_render::render`] (markdown →
    /// paragraphs / lists / bold / code, raw HTML stripped). Empty when
    /// `final_message` is empty. The chat panel shows this — so the
    /// model's markdown reads like a normal chat instead of literal
    /// `**` and backticks — while keeping `final_message` for the replay.
    pub final_message_html: String,
    /// How many iterations of the loop ran. Useful for debugging /
    /// audit; the chat panel ignores it.
    pub iterations: usize,
    /// Set when the loop hit `MAX_AGENTIC_ITERATIONS` without the
    /// model producing a textual final answer.
    pub budget_exhausted: bool,
}

/// Run an agentic submission through the operational chat LLM backend
/// (the `operator_chat` slot, falling back to `hub_writer` — see
/// [`crate::state::MemoryHandles::backend_for_chat`])
/// ([agentic chat](../../../../wiki/design-notes/agentic-chat.md)) with the dashboard's
/// whitelisted tool registry.
///
/// `history` is the recent `{user, assistant}` window the chat panel
/// replays so a confirmation resolves against the prior proposal; pass
/// an empty slice for a primed, fresh conversation (the welcome wizard,
/// the proposals / facts deep-links).
///
/// `reveal` is the admin ACL-reveal flag ([`crate::reveal::active`]): it
/// lifts the per-recipient scope on the proposal **read** tools so an
/// admin can review the whole deployment's in-flight items. Compute it
/// from the request's cookie jar; it is always `false` for a non-admin.
///
/// The loop alternates between asking the model for a turn and
/// dispatching whichever tool calls the model produced, feeding the
/// JSON-stringified results back as `Role::Tool` messages. It stops
/// when:
///
/// - The model returns an assistant turn with no `tool_calls` — that
///   message is the final reply.
/// - The iteration counter hits [`MAX_AGENTIC_ITERATIONS`] — `final_message`
///   is left empty and `budget_exhausted` is set so the panel can
///   surface a "loop budget exhausted" notice.
///
/// All `_internal.*` calls are dispatched with the connected user's
/// [`SenderContext`] so the same ACLs an external consumer agent
/// would face apply here too.
///
/// # Errors
/// Returns [`DashboardError::Internal`] when `memory` is not wired,
/// [`DashboardError::Validation`] when **neither** the `llm.operator_chat`
/// nor the `llm.hub_writer` fallback slot is configured (same UX
/// treatment as the `llm.ingest` slot in [`process_submission`]), or when
/// the chat backend itself surfaces an unrecoverable error.
#[allow(clippy::too_many_lines, reason = "one linear LLM-orchestration setup")]
pub async fn agentic_submission(
    state: &DashboardState,
    user: &SessionUser,
    text: &str,
    history: &[ChatHistoryTurn],
    reveal: bool,
) -> Result<AgenticTurn> {
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal("memory handles not wired — start with `mwe-mcp serve`".into())
    })?;
    // The operational chat prefers its dedicated `operator_chat` slot and
    // falls back to `hub_writer` when it is unconfigured — see
    // `MemoryHandles::backend_for_chat`.
    let backend = memory.backend_for_chat().map_err(|e| match e {
        BackendForError::SlotMissing(_) => DashboardError::Validation(
            "The dashboard-chat LLM is not configured in mwe-mcp.config.yaml. \
             Configure the `llm.operator_chat` slot (or `llm.hub_writer`, used \
             as the fallback) for the agentic loop before continuing."
                .into(),
        ),
        BackendForError::BuildFailed { slot, detail } => {
            DashboardError::Internal(format!("building chat backend (slot {slot}): {detail}"))
        },
    })?;

    // The per-slot `temperature` / `max_tokens` knobs the admin
    // sets in `/dashboard/admin/llm-config` reach the model through
    // `apply_defaults_to_chat` below. The helper fills the request
    // fields only when the loop leaves them unset, which is the case
    // here — we never pin temperature/max_tokens for the agentic
    // turn, so the operator's defaults always win (whereas the
    // classifier slots in mwe-core pin explicit values that the
    // helper does not touch).
    let chat_defaults = memory.chat_defaults();

    let sender_groups = enrollment::groups_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("enrollment::groups_for: {e}")))?;
    let ctx = AgenticContext {
        pool: &state.pool,
        tree: &memory.tree,
        embedder: Arc::clone(&memory.embedder),
        sender_ctx: SenderContext {
            sender_id: user.sender_id.clone(),
            sender_groups,
        },
        is_admin: user.is_admin,
        reveal,
    };

    let tools = agentic::tool_descriptors();
    // The system prompt comes from the hybrid loader — operator
    // override at `<workdir>/prompts/agentic-chat-panel.md` wins,
    // otherwise the bundled default embedded via `include_str!` is
    // used. A malformed override surfaces loudly so a hand-edit
    // mistake never silently degrades the dashboard chat.
    // Locale plumbing: resolve the signed-in user's locale from
    // `enrollment_users.locale` (no MCP `metadata.locale` source on
    // the dashboard path — the operator is the sender) and inject the
    // `{locale}` directive via the hybrid renderer.
    let session_locale = enrollment::locale_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("enrollment::locale_for: {e}")))?;
    let language_directive = mwe_core::locale::render_language_directive(session_locale.as_deref());
    let system_prompt = mwe_core::prompts::render(
        "agentic-chat-panel",
        &memory.workdir,
        BUNDLED_AGENTIC_PROMPT_MD,
        &[("locale", language_directive.as_str())],
    )
    .map_err(|e| DashboardError::Internal(format!("prompts::render(agentic-chat-panel): {e}")))?;
    // [system] + the recent `{user, assistant}` window + the new user
    // message. Replaying the window is what lets a confirmation ("sì")
    // resolve against the assistant's prior proposal — the loop is
    // otherwise stateless across submissions (no server-side history).
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(2 + history.len() * 2);
    messages.push(ChatMessage::system(system_prompt));
    for turn in history {
        messages.push(ChatMessage::user(turn.user.clone()));
        messages.push(ChatMessage::assistant(turn.assistant.clone()));
    }
    messages.push(ChatMessage::user(text));
    let mut trace: Vec<ToolCallTrace> = Vec::new();
    let mut iterations = 0usize;
    let mut final_message = String::new();
    let mut budget_exhausted = false;

    while iterations < MAX_AGENTIC_ITERATIONS {
        iterations += 1;
        let mut request = ChatRequest::new(messages.clone()).with_tools(tools.clone());
        if let Some(cfg) = &chat_defaults {
            cfg.apply_defaults_to_chat(&mut request);
        }
        let response = backend
            .chat(request)
            .await
            .map_err(|e| DashboardError::Internal(format!("hub_writer chat: {e}")))?;

        if response.message.tool_calls.is_empty() {
            // No tool calls — this is the model's final textual reply.
            final_message = response.message.content;
            break;
        }

        // Append the assistant turn (with its tool_calls) to the
        // history so the model sees its own reasoning on the next
        // iteration.
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: response.message.content,
            tool_calls: response.message.tool_calls.clone(),
            tool_call_id: None,
        });

        // Dispatch every tool call sequentially, append each result
        // as a Role::Tool message.
        for call in &response.message.tool_calls {
            let (result, is_error) = dispatch_for_trace(call, &ctx).await;
            trace.push(ToolCallTrace {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: result.clone(),
                is_error,
            });
            messages.push(ChatMessage::tool_result(call.id.clone(), result));
        }
    }

    if iterations >= MAX_AGENTIC_ITERATIONS && final_message.is_empty() {
        budget_exhausted = true;
    }

    // Render the reply's markdown to safe HTML server-side (reusing the
    // wiki preview's `pulldown-cmark` path, which strips raw HTML) so the
    // panel can show structured prose. `final_message` itself stays raw —
    // it is the text replayed into the next turn's confirmation window.
    let final_message_html = if final_message.is_empty() {
        String::new()
    } else {
        crate::md_render::render(&final_message)
    };

    Ok(AgenticTurn {
        user_text: text.to_owned(),
        trace,
        final_message,
        final_message_html,
        iterations,
        budget_exhausted,
    })
}

/// Dispatch a single tool call and return the JSON-stringified result
/// that will be fed back to the model. Errors are wrapped as
/// `{"error": "..."}` JSON so the model can read them as ordinary
/// tool output and recover (retry with corrected arguments, ask the
/// user, or apologise).
async fn dispatch_for_trace(call: &ToolCall, ctx: &AgenticContext<'_>) -> (String, bool) {
    match agentic::dispatch(&call.name, &call.arguments, ctx).await {
        Ok(result) => (result, false),
        Err(err) => {
            let body = json!({ "error": err.to_string() }).to_string();
            (body, true)
        },
    }
}

/// `POST /dashboard/chat/agentic` handler. Always returns JSON (the
/// chat panel is the only caller and it sends `Accept:
/// application/json`); a no-JS fallback for this endpoint is not
/// planned because the loop's intermediate states would not render
/// well in a single static page anyway.
async fn post_agentic(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: axum_extra::extract::cookie::CookieJar,
    axum::Form(form): axum::Form<ChatSubmission>,
) -> Result<axum::Json<AgenticTurn>> {
    let text = form.text.trim();
    if text.is_empty() {
        return Err(DashboardError::Validation(
            "Type a message before sending.".into(),
        ));
    }
    let history = parse_chat_history(form.history.as_deref());
    let reveal = crate::reveal::active(&user, &jar);
    let turn = agentic_submission(&state, &user, text, &history, reveal).await?;
    Ok(axum::Json(turn))
}

/// Rendered representation of an [`IngestResponse`]. Exposed because
/// the right-side chat panel injects this exact fragment into its
/// scroll area on every turn ([agentic chat](../../../../wiki/design-notes/agentic-chat.md)).
#[must_use]
pub fn response_panel(response: &IngestResponse) -> Markup {
    html! {
        section.chat-response {
            h2 { "Response" }
            dl {
                dt { "intent" } dd { code { (response.intent.as_str()) } }
                dt { "llm_used" } dd { (response.llm_used) }
                dt { "took_ms" } dd { (response.took_ms) }
                @if let Some(id) = &response.capture_id {
                    dt { "capture_id" } dd { code { (id.as_str()) } }
                }
                @if response.needs_disambig {
                    dt { "needs_disambig" } dd { "true" }
                }
            }

            @if let Some(seed) = &response.suggested_seed {
                h3 { "Suggested reply seed" }
                pre.suggested-seed { (seed) }
            }

            @if let Some(rules) = &response.rules {
                h3 { "Behaviour rules" }
                pre.context-snippet { (rules) }
            }

            @if let Some(snippet) = &response.context_snippet {
                h3 { "Recall snippet" }
                pre.context-snippet { (snippet) }
            }

            @if response.needs_disambig && !response.disambig_candidates.is_empty() {
                h3 { "Disambiguation candidates" }
                ul {
                    @for c in &response.disambig_candidates {
                        li { code { (c.candidate_id) } " — " (c.description) }
                    }
                }
                p.muted {
                    "For now, pick the right candidate on a future "
                    "submit by appending " code { "?disambig_choice=<id>" }
                    " — a full UI is planned."
                }
            }

            @if matches!(response.intent, IntentKind::Structural) {
                p.muted {
                    "Structural intent → "
                    a href="/dashboard/wiki" { "open the wiki list" }
                    " to act on it."
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_history_absent_or_blank_is_empty() {
        assert!(parse_chat_history(None).is_empty());
        assert!(parse_chat_history(Some("")).is_empty());
        assert!(parse_chat_history(Some("   ")).is_empty());
    }

    #[test]
    fn parse_chat_history_malformed_json_degrades_to_empty() {
        // A garbled payload must never fail the turn — it degrades to
        // "no history" so the submission still runs (just stateless).
        assert!(parse_chat_history(Some("{not json")).is_empty());
        assert!(parse_chat_history(Some("[{\"user\":1}]")).is_empty());
    }

    #[test]
    fn parse_chat_history_drops_empty_sided_turns() {
        // Error / budget-exhausted / primer turns carry no assistant text
        // (or no user text) and must not be replayed.
        let raw = r#"[
            {"user":"a","assistant":""},
            {"user":"","assistant":"b"},
            {"user":"keep-u","assistant":"keep-a"}
        ]"#;
        let out = parse_chat_history(Some(raw));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].user, "keep-u");
        assert_eq!(out[0].assistant, "keep-a");
    }

    #[test]
    fn parse_chat_history_keeps_only_recent_window_in_order() {
        let turns: Vec<String> = (0..MAX_HISTORY_TURNS + 4)
            .map(|i| format!("{{\"user\":\"u{i}\",\"assistant\":\"a{i}\"}}"))
            .collect();
        let raw = format!("[{}]", turns.join(","));
        let out = parse_chat_history(Some(&raw));
        assert_eq!(out.len(), MAX_HISTORY_TURNS);
        // The tail survives, chronological order preserved.
        assert_eq!(out.first().unwrap().user, "u4");
        assert_eq!(
            out.last().unwrap().user,
            format!("u{}", MAX_HISTORY_TURNS + 3)
        );
    }

    #[test]
    fn parse_chat_history_clamps_long_fields() {
        let long = "x".repeat(MAX_HISTORY_CHARS + 50);
        let raw = format!("[{{\"user\":\"{long}\",\"assistant\":\"ok\"}}]");
        let out = parse_chat_history(Some(&raw));
        assert_eq!(out.len(), 1);
        // Clamped to MAX_HISTORY_CHARS chars + an ellipsis marker.
        assert_eq!(out[0].user.chars().count(), MAX_HISTORY_CHARS + 1);
        assert!(out[0].user.ends_with('…'));
        assert_eq!(out[0].assistant, "ok");
    }
}
