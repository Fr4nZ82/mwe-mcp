// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal action routes — the bridge endpoints behind the chat.
//!
//! The proposals **questionnaire / tray form surface is retired**:
//! proposals are now reviewed,
//! applied, confirmed, and reverted by talking to the dashboard chat
//! (`/dashboard/chat`), which drives the same `mwe_core::proposals`
//! chassis through its agentic tools (`structure_proposal_*`). What
//! survives here are the **action routes** and the **open-in-chat
//! bridge**, kept mounted as endpoints the chat / consumer links target:
//!
//! - POST `/dashboard/proposals/:id/apply` — apply a pending proposal
//!   (with form answers, for any deep-link that still posts them).
//! - POST `/dashboard/proposals/:id/confirm` — confirm an
//!   `applied_pending_confirm` proposal (the auto-apply sweep landed it;
//!   the user keeps it).
//! - POST `/dashboard/proposals/:id/revert` — undo an `applied` /
//!   `applied_pending_confirm` proposal within its window.
//! - GET `/dashboard/proposals/:id/open-in-chat` — server-side primer
//!   that lands the operator inside the chat with the proposal already
//!   summarised (a review/apply primer for a pending questionnaire, a
//!   modify/undo primer for an already-applied structured-wiki
//!   emergence).
//!
//! The three POST routes no longer render a page: the form that called
//! them is gone, so each performs its chassis action and **303-redirects
//! to `/dashboard/chat`** (the single operational surface) on both
//! success and classified error — the chat is where the operator
//! continues. Their auth / `memory` wiring is unchanged; only the
//! response shape moved from HTML to a redirect.

use axum::Form;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use maud::{PreEscaped, html};
use mwe_core::proposals;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::routes::chat;
use crate::state::DashboardState;
use crate::ui::layout;

/// The single operational surface the action routes hand back to.
const CHAT_SURFACE: &str = "/dashboard/chat";

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/proposals/in-flight-count", get(in_flight_count))
        .route("/proposals/in-flight/chat-turn", get(in_flight_chat_turn))
        .route("/proposals/:proposal_id/apply", post(apply))
        .route("/proposals/:proposal_id/confirm", post(confirm))
        .route("/proposals/:proposal_id/revert", post(revert))
        .route("/proposals/:proposal_id/open-in-chat", get(open_in_chat))
}

/// JSON shape returned by [`in_flight_count`] — the per-class counts the
/// topnav badge reads. `total` is `pending + applied_pending_confirm +
/// revertable_applied`, exactly [`proposals::InFlightCounts::total`].
#[derive(Debug, Serialize)]
struct InFlightCountJson {
    pending: i64,
    applied_pending_confirm: i64,
    revertable_applied: i64,
    total: i64,
}

/// `GET /dashboard/proposals/in-flight-count` — the count the topnav
/// badge fetches client-side (the shell layout is a pure sync render, so
/// it cannot touch the DB itself; see [`crate::ui::layout`]).
///
/// ACL-scoped to the signed-in user: everyone — admins included — counts
/// only rows addressed to them plus the unaddressed/admin-fallback ones
/// (`recipient = Some("user:<sender>")`). The admin ACL-reveal switch
/// ([`crate::reveal::active`]) lifts the scope to the whole deployment
/// (`recipient = None`), the same posture the facts table takes — because
/// a proposal's `context` carries per-fragment-ACL'd fact text, so an
/// unconditional admin-wide count would leak other users' content. The
/// predicate is `false` for any non-admin, so a non-admin is always
/// scoped.
async fn in_flight_count(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<axum::Json<InFlightCountJson>> {
    let recipient =
        (!crate::reveal::active(&state, &user, &jar)).then(|| format!("user:{}", user.sender_id));
    let counts = proposals::count_in_flight(&state.pool, recipient.as_deref(), chrono::Utc::now())
        .await
        .map_err(|e| DashboardError::Internal(format!("count_in_flight: {e}")))?;
    Ok(axum::Json(InFlightCountJson {
        pending: counts.pending,
        applied_pending_confirm: counts.applied_pending_confirm,
        revertable_applied: counts.revertable_applied,
        total: counts.total(),
    }))
}

/// Form fields for the apply submit. The fields a given kind handler
/// actually consumes depend on the kind + variant of the proposal:
///
/// - `wiki_promote` + `variant = "paragraph_to_file"` (or unset):
///   reads `target_page`.
/// - `wiki_promote` + `variant = "file_to_subwiki"`: reads
///   `new_wiki_slug` (optional) and `new_wiki_title` (optional).
/// - `dedup_merge`: ignores every field (the act of posting is the
///   confirmation).
/// - `bundle`: still surfaces `not_implemented_phase_c`.
#[derive(Debug, Deserialize)]
pub struct ApplyForm {
    /// Variant discriminator for `wiki_promote`. Optional; missing
    /// defaults to `paragraph_to_file`.
    #[serde(default)]
    pub variant: Option<String>,
    /// `wiki_promote` paragraph → file: target page name.
    #[serde(default)]
    pub target_page: Option<String>,
    /// `wiki_promote` file → sub-wiki: slug for the new sub-wiki.
    #[serde(default)]
    pub new_wiki_slug: Option<String>,
    /// `wiki_promote` file → sub-wiki: human-readable title.
    #[serde(default)]
    pub new_wiki_title: Option<String>,
}

/// `POST /dashboard/proposals/:id/apply` — apply a pending proposal,
/// then hand the operator back to the chat (the form that used to render
/// the outcome is retired). Errors are not surfaced as a page either:
/// the route classifies them only insofar as it logs them, then
/// 303-redirects to the chat where the operator can inspect state with
/// the read tools and retry conversationally.
async fn apply(
    State(state): State<DashboardState>,
    user: SessionUser,
    Path(proposal_id): Path<String>,
    Form(form): Form<ApplyForm>,
) -> Result<Response> {
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles not wired — start with `mwe-mcp serve` not the identity-only build"
                .into(),
        )
    })?;

    let answers = build_answers(&form);

    match proposals::apply_proposal(
        &state.pool,
        &memory.tree,
        &proposal_id,
        &answers,
        Some(user.sender_id.as_str()),
        user.is_admin,
    )
    .await
    {
        Ok(out) => tracing::info!(
            proposal_id = %out.proposal_id,
            kind = %out.kind,
            "dashboard: proposal applied via action route"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            %proposal_id,
            "dashboard: proposal apply via action route failed"
        ),
    }
    Ok(Redirect::to(CHAT_SURFACE).into_response())
}

/// `POST /dashboard/proposals/:id/confirm` — promote an
/// `applied_pending_confirm` proposal to `applied`, then redirect to the
/// chat. `confirm_proposal` gates by recipient/admin itself.
async fn confirm(
    State(state): State<DashboardState>,
    user: SessionUser,
    Path(proposal_id): Path<String>,
) -> Result<Response> {
    match proposals::confirm_proposal(&state.pool, &proposal_id, &user.sender_id, user.is_admin)
        .await
    {
        Ok(out) => tracing::info!(
            proposal_id = %out.proposal_id,
            kind = %out.kind,
            "dashboard: proposal confirmed via action route"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            %proposal_id,
            "dashboard: proposal confirm via action route failed"
        ),
    }
    Ok(Redirect::to(CHAT_SURFACE).into_response())
}

/// `POST /dashboard/proposals/:id/revert` — undo an `applied` /
/// `applied_pending_confirm` proposal, then redirect to the chat.
///
/// Mirrors the chassis' status-driven `RevertAuth` selection: `applied`
/// rows revert via [`proposals::RevertAuth::Token`] (fetched server-side
/// from the row), `applied_pending_confirm` rows via
/// [`proposals::RevertAuth::Caller`] (no token exists yet) with the
/// session user as the caller. Anything else is a no-op redirect.
async fn revert(
    State(state): State<DashboardState>,
    user: SessionUser,
    Path(proposal_id): Path<String>,
) -> Result<Response> {
    let memory = state
        .memory
        .as_ref()
        .ok_or_else(|| DashboardError::Internal("memory handles not wired".into()))?;

    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, revert_token FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind(&proposal_id)
    .fetch_optional(&state.pool)
    .await?;

    let auth = match row.as_ref() {
        Some((status, _)) if status == "applied_pending_confirm" => {
            Some(proposals::RevertAuth::Caller {
                sender: &user.sender_id,
                is_admin: user.is_admin,
            })
        },
        Some((status, Some(token))) if status == "applied" => {
            Some(proposals::RevertAuth::Token(token))
        },
        _ => None,
    };

    if let Some(auth) = auth {
        match proposals::revert_proposal(&state.pool, &memory.tree, &proposal_id, auth).await {
            Ok(out) => tracing::info!(
                proposal_id = %out.proposal_id,
                kind = %out.kind,
                "dashboard: proposal reverted via action route"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                %proposal_id,
                "dashboard: proposal revert via action route failed"
            ),
        }
    } else {
        tracing::warn!(
            %proposal_id,
            "dashboard: proposal revert via action route — not in a revertable status (or no token), ignoring"
        );
    }
    Ok(Redirect::to(CHAT_SURFACE).into_response())
}

/// `GET /dashboard/proposals/:id/open-in-chat` — server-side primer
/// for the agentic chat panel.
///
/// The server composes a review/apply primer message, runs it through the
/// agentic loop (same `hub_writer` slot the chat panel uses), and
/// returns a landing page that injects the resulting [`AgenticTurn`]
/// into the chat panel's `localStorage` via `window.__mweChatPrimer`.
/// The user lands directly inside the chat with the proposal already on
/// screen — no second click required.
async fn open_in_chat(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Path(proposal_id): Path<String>,
) -> Result<Html<String>> {
    let primer = compose_primer(&proposal_id);
    let reveal = crate::reveal::active(&state, &user, &jar);
    // Fresh primed conversation — no prior turns to replay.
    let turn = chat::agentic_submission(&state, &user, &primer, &[], reveal).await?;
    let intro = html! {
        h2 { "Proposal " code { (proposal_id) } " opened in chat" }
        p {
            "The proposal summary is in the chat panel on the right. "
            "Tell it there what you want to do (modify, apply, confirm or undo) "
            "with an explicit confirmation; to close without doing anything go to the "
            a href="/dashboard/chat" { "chat" }
            "."
        }
    };
    Ok(Html(land_turn_in_chat(
        &user,
        "Proposal in chat",
        &intro,
        &turn,
    )))
}

/// `GET /dashboard/proposals/in-flight/chat-turn` — the data endpoint the
/// topnav in-flight badge fetches. Runs the fixed "show me everything
/// pending" primer through the agentic loop (read-only: it lists and
/// summarises, touches nothing) and returns the resulting [`AgenticTurn`]
/// as JSON. `chat.js` opens the chat panel and renders the turn inline,
/// with a spinner while the overview is composed — so clicking the badge
/// feels like asking the chat "what do I have in flight?", with no
/// full-page navigation. The badge is revealed only with JS (`ui.js`), so
/// there is no no-JS consumer for this to serve as a page; the count and
/// listing are ACL-scoped (admins see the whole deployment only under
/// `reveal`, like the badge count).
async fn in_flight_chat_turn(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<axum::Json<chat::AgenticTurn>> {
    let reveal = crate::reveal::active(&state, &user, &jar);
    let turn = chat::agentic_submission(&state, &user, IN_FLIGHT_PRIMER, &[], reveal).await?;
    Ok(axum::Json(turn))
}

/// Read-only primer the in-flight badge injects: enumerate everything the
/// user can still act on across the three in-flight classes and stop.
/// English seed — the reply language is governed by the `{locale}`
/// directive in the system prompt, not by this text.
const IN_FLIGHT_PRIMER: &str = "Show me everything I have in flight: \
     proposals to review, applications awaiting confirmation, and emergences \
     still within their undo window. List them with `structure_proposal_list` \
     (statuses pending, applied_pending_confirm, applied) and summarise them \
     briefly; do nothing until I ask you to.";

/// Landing render for the single-proposal open-in-chat bridge: serialise
/// the [`AgenticTurn`] into the `window.__mweChatPrimer` payload `chat.js`
/// hydrates, under a short page intro. Used by [`open_in_chat`] — the
/// 303 target of a born-applied structural receipt, a real full-page
/// navigation. The in-flight badge takes the lighter
/// [`in_flight_chat_turn`] JSON path (rendered inline in the panel)
/// instead.
fn land_turn_in_chat(
    user: &SessionUser,
    title: &str,
    intro: &maud::Markup,
    turn: &chat::AgenticTurn,
) -> String {
    let payload = serde_json::json!({
        "user_text": turn.user_text,
        "trace": turn.trace,
        "final_message": turn.final_message,
        "final_message_html": turn.final_message_html,
        "iterations": turn.iterations,
        "budget_exhausted": turn.budget_exhausted,
        "ts": chrono::Utc::now().timestamp_millis(),
    });
    let payload_js = serde_json::to_string(&payload).unwrap_or_else(|_| "null".into());
    let body = html! {
        (intro)
        script {
            (PreEscaped(format!("window.__mweChatPrimer = {payload_js};")))
        }
    };
    layout::authenticated_page(title, user, &body)
}

/// Compose the review/apply primer injected into the agentic loop for a
/// pending proposal. English seed — reply language is governed by the
/// `{locale}` directive in the system prompt, not by this text.
fn compose_primer(proposal_id: &str) -> String {
    format!(
        "I want to review the proposal `{proposal_id}`. \
         Show me the proposal's contents using the appropriate tool, then explain \
         briefly what it is about and which answers are needed to apply it. If it is \
         a fact-forget request I am eligible to vote on, tell me I can approve or \
         reject it (`structure_proposal_vote`) — a NO majority blocks the forget (the \
         fact stays), silence lets it through. \
         Apply, vote, or undo nothing in this turn: wait for my explicit instruction \
         on the next turn."
    )
}

/// Build a workhorse LLM backend from the dashboard's `llm.ingest`
/// slot when one is configured. Returns `None` otherwise — kind
/// handlers that require an LLM surface their own `handler_data`
/// failure in that case.
///
/// Routes through [`MemoryHandles::backend_for`] so the API key
/// Build the `answers` JSON the chassis expects from the submitted
/// form fields. The chassis dispatches by `kind` (which is on the
/// proposal row, not the form), so this helper picks the fields a
/// given combination of kind + variant cares about and packs them
/// into a JSON object.
///
/// Heuristics:
///
/// - `variant = "file_to_subwiki"` → `wiki_promote` sub-wiki form.
/// - Otherwise → `wiki_promote` paragraph-to-file (or `dedup_merge`,
///   which ignores answers anyway).
fn build_answers(form: &ApplyForm) -> Value {
    let variant = form.variant.as_deref().map_or("", str::trim);
    if variant == "file_to_subwiki" {
        let mut obj = serde_json::Map::new();
        obj.insert("variant".into(), Value::String(variant.to_owned()));
        if let Some(slug) = form.new_wiki_slug.as_deref()
            && !slug.trim().is_empty()
        {
            obj.insert(
                "new_wiki_slug".into(),
                Value::String(slug.trim().to_owned()),
            );
        }
        if let Some(title) = form.new_wiki_title.as_deref()
            && !title.trim().is_empty()
        {
            obj.insert(
                "new_wiki_title".into(),
                Value::String(title.trim().to_owned()),
            );
        }
        return Value::Object(obj);
    }

    let target = form.target_page.as_deref().map_or("", str::trim);
    if target.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "target_page": target })
    }
}
