// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal routes — what the chat panel reads, and the door into it.
//!
//! **There is no questionnaire or tray form.** Proposals are reviewed and
//! applied by talking to the dashboard chat (`/dashboard/chat`), which
//! drives the `mwe_core::proposals` chassis through its agentic tools
//! (`structure_proposal_*`) — applying one is a conversation, because the
//! answers it needs are a conversation. This module mounts what the chat
//! and the links into it need:
//!
//! - GET `/dashboard/proposals/in-flight-count` — the topnav badge.
//! - GET `/dashboard/proposals/in-flight/chat-turn` — the chat panel's
//!   own primer for whatever is pending.
//! - GET `/dashboard/proposals/:id/open-in-chat` — server-side primer
//!   that lands the operator inside the chat with the proposal already
//!   summarised (a review/apply primer for a pending questionnaire, a
//!   read-what-happened primer for an already-applied structured-wiki
//!   emergence).

use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{PreEscaped, html};
use mwe_core::proposals;
use serde::Serialize;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::routes::chat;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/proposals/in-flight-count", get(in_flight_count))
        .route("/proposals/in-flight/chat-turn", get(in_flight_chat_turn))
        .route("/proposals/:proposal_id/open-in-chat", get(open_in_chat))
}

/// JSON shape returned by [`in_flight_count`] — what the topnav badge
/// reads. A proposal awaiting the user's answer is the only row anybody
/// can still act on: an applied change is not undone.
#[derive(Debug, Serialize)]
struct InFlightCountJson {
    pending: i64,
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
    let pending = proposals::count_pending(&state.pool, recipient.as_deref())
        .await
        .map_err(|e| DashboardError::Internal(format!("count_pending: {e}")))?;
    Ok(axum::Json(InFlightCountJson { pending }))
}

/// `GET /dashboard/proposals/:id/open-in-chat` — server-side primer
/// for the agentic chat panel.
///
/// The server composes a review/apply primer message, runs it through the
/// agentic loop (same `operator_chat` slot the chat panel uses), and
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
    let chrome = layout::Chrome::of(&state);
    let primer = compose_primer(&proposal_id);
    let reveal = crate::reveal::active(&state, &user, &jar);
    // Fresh primed conversation — no prior turns to replay.
    let turn = chat::agentic_submission(&state, &user, &primer, &[], reveal).await?;
    let intro = html! {
        h2 { "Proposal " code { (proposal_id) } " opened in chat" }
        p {
            "The proposal summary is in the chat panel on the right. "
            "Tell it there what you want to do (modify or apply) "
            "with an explicit confirmation; to close without doing anything go to the "
            a href="/dashboard/chat" { "chat" }
            "."
        }
    };
    Ok(Html(land_turn_in_chat(
        chrome,
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
) -> Response {
    let reveal = crate::reveal::active(&state, &user, &jar);
    match chat::agentic_submission(&state, &user, IN_FLIGHT_PRIMER, &[], reveal).await {
        Ok(turn) => axum::Json(turn).into_response(),
        // Same envelope the chat submit answers with: the badge renders
        // the refusal in the panel, so it needs the sentence, not a
        // status code.
        Err(e) => e.into_json_response(),
    }
}

/// Read-only primer the in-flight badge injects: enumerate the proposals
/// still waiting on the user and stop. English seed — the reply language
/// is governed by the `{locale}` directive in the system prompt, not by
/// this text.
const IN_FLIGHT_PRIMER: &str = "Show me the proposals still waiting on me. \
     List them with `structure_proposal_list` (status pending) and summarise \
     them briefly; do nothing until I ask you to.";

/// Landing render for the single-proposal open-in-chat bridge: serialise
/// the [`AgenticTurn`] into the `window.__mweChatPrimer` payload `chat.js`
/// hydrates, under a short page intro. Used by [`open_in_chat`] — the
/// 303 target of a born-applied structural receipt, a real full-page
/// navigation. The in-flight badge takes the lighter
/// [`in_flight_chat_turn`] JSON path (rendered inline in the panel)
/// instead.
fn land_turn_in_chat(
    chrome: layout::Chrome,
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
    let payload_js = components::script_json(&payload);
    let body = html! {
        (intro)
        script {
            (PreEscaped(format!("window.__mweChatPrimer = {payload_js};")))
        }
    };
    layout::authenticated_page(chrome, title, user, &body)
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
         Apply nothing and vote on nothing in this turn: wait for my explicit \
         instruction on the next turn."
    )
}
