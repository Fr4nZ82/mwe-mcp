// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal routes — the record of what the memory rearranged, and the
//! door into the conversation that answers one.
//!
//! A **proposal** is one row of `structure_proposals`: something the
//! engine changed about the *shape* of the memory and wrote down
//! afterwards (a page it invented, a link it drew, two facts it merged,
//! dates it closed, sharing it changed), or — for the two kinds that
//! really are a question — something it stopped and asked before doing.
//!
//! Two surfaces, and they do different jobs:
//!
//! - **Reading** is this module's pages. GET `/dashboard/proposals` lists
//!   them, GET `/dashboard/proposals/:id` opens one. Both are pure reads:
//!   they run SQL, render, and call no model. That is what lets them be
//!   the whole proposal surface a **frozen instance** offers — the chat
//!   panel is not rendered there ([`crate::ui::layout`]), so without these
//!   pages a shown deployment displays a counter and nothing behind it.
//! - **Acting** is the chat (`/dashboard/chat`), which drives the
//!   `mwe_core::proposals` chassis through its agentic tools
//!   (`structure_proposal_*`). Applying one is a conversation, because the
//!   answers it needs are a conversation. The listing offers the door into
//!   it on a row somebody can still answer, and never on a frozen
//!   deployment, where nothing can be answered.
//!
//! Neither of the two chat primers is reachable on a frozen deployment
//! either, address and all: they run the agentic loop, and
//! [`crate::read_only::COSTLY_ROUTES`] refuses whatever can reach a model
//! there whatever its method. So the listing withholding the link is
//! about what a page should offer, not about what the guard allows —
//! both hold, and each is worth having on its own.
//!
//! The rest of the module is what the chat and the links into it need:
//!
//! - GET `/dashboard/proposals/in-flight-count` — the topnav badge.
//! - GET `/dashboard/proposals/in-flight/chat-turn` — the chat panel's
//!   own primer for whatever is pending.
//! - GET `/dashboard/proposals/:id/open-in-chat` — server-side primer
//!   that lands the operator inside the chat with the proposal already
//!   summarised (a review/apply primer for a pending questionnaire, a
//!   read-what-happened primer for an already-applied structural
//!   emergence).
//!
//! # Who sees which rows
//!
//! [`readable_scope`] is the whole policy, and it is the only thing
//! standing between two people's proposals: a row's `context` carries the
//! material the change was about, it is **not** re-projected per reader,
//! and no check sits downstream of the query.
//!
//! - **A person** sees the rows addressed to them
//!   ([`RecipientScope::Addressee`]) — which, by
//!   `mwe_core::proposals::recipient_from_fact`, means the proposals
//!   raised about their own facts or about what they themselves said.
//! - **An admin** sees those plus the rows addressed to nobody
//!   ([`RecipientScope::AddresseeOrNobody`]) — the nightly pass addresses
//!   none of its receipts, and they name pages across every wiki.
//! - **Admin reveal** ([`crate::reveal`]) lifts the scope to every
//!   recipient, the same posture the facts table and the wiki pages take.
//!   It is admin-only, so a reader can never widen past themself.
//!
//! - **An elector** sees a forget request they are being asked to vote
//!   on. That row is addressed to whoever asked for the forget, and the
//!   people being asked are everybody else who can read the fact, so
//!   scoping on the addressee alone hid the ballot from exactly the
//!   people it is a question for.
//!
//! The badge count and the chat's proposal tools read by the same rule,
//! from the same function: a badge that counts a row the page withholds
//! sends somebody looking for something that is not there, and a chat
//! that lists what the page hides makes the page's restraint
//! decorative.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use mwe_core::proposals::{self, ListFilters, ProposalRow, ProposalStatus, RecipientScope, kind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::routes::chat;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Mount under the authenticated tree.
///
/// `/proposals/in-flight-count` and `/proposals/in-flight/chat-turn` are
/// literal segments, so they win over `/proposals/:proposal_id` in the
/// router's own precedence and no ordering here is load-bearing.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/proposals", get(index))
        .route("/proposals/in-flight-count", get(in_flight_count))
        .route("/proposals/in-flight/chat-turn", get(in_flight_chat_turn))
        .route("/proposals/:proposal_id", get(detail))
        .route("/proposals/:proposal_id/open-in-chat", get(open_in_chat))
}

/// The title the page renders under, and the key the guide's **?** is
/// looked up by (`crate::routes::guide::SCREEN_PAGES`).
const PAGE_TITLE: &str = "Proposals";

/// How many rows the listing shows.
///
/// A **page size**, not a retention bound: nothing sweeps
/// `structure_proposals`, so the table is the whole history and this is
/// the newest slice of it. Sized to the chassis' own ceiling
/// ([`proposals::MAX_LIST_TOP_K`]) so the page never asks for more than
/// the read side will hand over.
const PAGE_LIMIT: i64 = proposals::MAX_LIST_TOP_K;

/// Whose rows this reader may see — **the** read ACL for proposals, and
/// the only one.
///
/// Three surfaces ask the question and they must not answer it
/// differently: this page, the badge count ([`in_flight_count`]) and the
/// chat's proposal tools ([`crate::agentic`]). A badge that counts what
/// the page withholds sends somebody looking for something that is not
/// there; a chat that lists what the page hides makes the page's
/// restraint decorative. So the rule lives here once and they all call
/// it.
///
/// See the module header for what the three cases mean. The electorate of
/// a forget request is folded in by the scope itself
/// ([`RecipientScope`]), because being asked to vote is exactly a reason
/// to be shown the question.
#[must_use]
pub fn readable_scope(sender_id: &str, is_admin: bool, reveal: bool) -> RecipientScope {
    let me = format!("user:{sender_id}");
    if reveal {
        RecipientScope::Everybody
    } else if is_admin {
        RecipientScope::AddresseeOrNobody(me)
    } else {
        RecipientScope::Addressee(me)
    }
}

/// [`readable_scope`] for a page handler, which holds the session and the
/// cookie jar rather than the two answers.
fn scope_for(state: &DashboardState, user: &SessionUser, jar: &CookieJar) -> RecipientScope {
    readable_scope(
        &user.sender_id,
        user.is_admin,
        crate::reveal::active(state, user, jar),
    )
}

/// The `?status=` filter on the listing.
#[derive(Debug, Deserialize)]
struct StatusQuery {
    /// One of the wire status strings, or absent for every state.
    status: Option<String>,
}

/// `GET /dashboard/proposals` — the listing, newest first.
///
/// A pure read on every deployment, frozen or not: it runs one `SELECT`
/// and renders it. Scoped by [`readable_scope`].
async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Query(q): Query<StatusQuery>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let reveal = crate::reveal::active(&state, &user, &jar);
    let status = q.status.as_deref().and_then(parse_status);
    // Scope in the query, not after the fetch: the table is never swept,
    // so a page-sized slice of everybody's rows filtered down afterwards
    // would show a reader fewer of their own the older the deployment got.
    let rows = proposals::list(
        &state.pool,
        &ListFilters {
            status,
            kind: None,
            recipient: scope_for(&state, &user, &jar),
            top_k: Some(PAGE_LIMIT),
        },
    )
    .await
    .map_err(|e| DashboardError::Internal(format!("proposals::list: {e}")))?;
    let body = render_index_body(
        &rows,
        status,
        reveal,
        user.is_admin,
        crate::read_only::hides_writes(&state),
    );
    Ok(Html(layout::authenticated_page(
        chrome, PAGE_TITLE, &user, &body,
    )))
}

/// `GET /dashboard/proposals/:id` — one proposal, read.
///
/// A row outside [`readable_scope`] answers [`DashboardError::NotFound`]
/// rather than `Forbidden`: "you may not read this" and "there is nothing
/// here" have to be the same answer, or the id itself becomes a way to
/// ask whether somebody else's proposal exists.
async fn detail(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Path(proposal_id): Path<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let reveal = crate::reveal::active(&state, &user, &jar);
    // The scope is applied by the same query that finds the row, so
    // there is no path here that loads a proposal first and checks
    // afterwards.
    let row = proposals::get(&state.pool, &proposal_id, &scope_for(&state, &user, &jar))
        .await
        .map_err(|e| DashboardError::Internal(format!("proposals::get: {e}")))?
        .ok_or(DashboardError::NotFound)?;
    let body = render_detail_body(&row, reveal, crate::read_only::hides_writes(&state));
    Ok(Html(layout::authenticated_page(
        chrome, PAGE_TITLE, &user, &body,
    )))
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
/// Counted through [`readable_scope`], the same rule the listing reads
/// by: the badge is a promise that there is something to open, and it has
/// to be a promise the page keeps.
async fn in_flight_count(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<axum::Json<InFlightCountJson>> {
    let pending = proposals::count_pending(&state.pool, &scope_for(&state, &user, &jar))
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

// ---------------------------------------------------------------------------
// The listing
// ---------------------------------------------------------------------------

/// The status tabs, in the order the page offers them: every row first,
/// because most of the table is a record of what already happened and a
/// page that opened on the empty "still waiting" slice would look broken
/// on a healthy deployment.
const STATUS_TABS: &[(&str, Option<ProposalStatus>)] = &[
    ("All", None),
    ("Pending", Some(ProposalStatus::Pending)),
    ("Applied", Some(ProposalStatus::Applied)),
    ("Expired", Some(ProposalStatus::Expired)),
];

/// A `?status=` value, or `None` for "every state" — which is also what
/// an unrecognised value falls back to, so a hand-typed address lands on
/// the page instead of on an error.
fn parse_status(raw: &str) -> Option<ProposalStatus> {
    STATUS_TABS
        .iter()
        .find_map(|&(_, status)| status.filter(|s| s.as_str() == raw))
}

#[allow(
    clippy::too_many_lines,
    reason = "one maud page body; splitting it would hurt readability"
)]
fn render_index_body(
    rows: &[ProposalRow],
    status: Option<ProposalStatus>,
    reveal: bool,
    is_admin: bool,
    frozen: bool,
) -> Markup {
    let tabs: Vec<(&str, String, bool)> = STATUS_TABS
        .iter()
        .map(|&(label, tab)| {
            let href = tab.map_or_else(
                || "/dashboard/proposals".to_owned(),
                |s| format!("/dashboard/proposals?status={}", s.as_str()),
            );
            (label, href, tab == status)
        })
        .collect();
    let tab_refs: Vec<(&str, &str, bool)> =
        tabs.iter().map(|(l, h, a)| (*l, h.as_str(), *a)).collect();
    html! {
        @if reveal { (crate::reveal::banner()) }
        p class="text-text-dim max-w-prose" {
            "What the memory changed about its own shape, newest first — a page it "
            "made, a link it drew, facts it merged, dates it closed, sharing it "
            "changed. Most of these are a record: the engine acted and wrote down "
            "what it did, and a change of shape is not undone. The ones marked "
            strong { "Pending" }
            " are the two kinds it stops and asks about instead."
            @if reveal {
                " Admin reveal is on, so this is every recipient's."
            } @else if is_admin {
                " Listed here are the ones addressed to you and the ones addressed to "
                "nobody in particular; turn on Admin reveal in "
                a href="/dashboard/settings/me" { "Settings" }
                " to see everyone's."
            } @else {
                " Listed here are the ones about your own facts, or about what you said."
            }
        }
        (components::tab_bar(&tab_refs))
        @if rows.is_empty() {
            p class="mt-6" {
                "Nothing here. Rows appear on their own: the nightly cycle writes one "
                "each time it rearranges something, and a turn writes one when it "
                "closes, re-files or re-shares a fact, or has to ask before storing."
            }
        } @else {
            table class="config-table mt-4" {
                thead {
                    tr {
                        th { "What" }
                        th { "What it says" }
                        th { "Who it is for" }
                        th { "Raised" }
                        th { "State" }
                        th { }
                    }
                }
                tbody {
                    @for row in rows {
                        (render_index_row(row, frozen))
                    }
                }
            }
        }
    }
}

/// How much of the one-line summary a table cell carries before the rest
/// is left to the row's own page.
const HEADLINE_PREVIEW_CHARS: usize = 140;

fn render_index_row(row: &ProposalRow, frozen: bool) -> Markup {
    let href = format!("/dashboard/proposals/{}", row.proposal_id);
    html! {
        tr {
            td { span class="badge" { (label(row)) } }
            td { (components::truncate_chars(&headline(row), HEADLINE_PREVIEW_CHARS)) }
            td { (recipient_in_words(row.recipient_id.as_deref())) }
            td { (components::compact_stamp(&row.emitted_at)) }
            td { (state_cell(row)) }
            td {
                a href=(href) { "open" }
                @if let Some(chat) = answer_in_chat_href(row, frozen) {
                    " · "
                    a href=(chat) { "answer in chat" }
                }
            }
        }
    }
}

/// Where the "answer in chat" link goes, or `None` when there is nothing
/// to answer.
///
/// Only a `pending` row can still be answered, and only on a deployment
/// that is not frozen: a frozen one refuses every write the conversation
/// would make, and does not render the chat panel the primer lands in.
fn answer_in_chat_href(row: &ProposalRow, frozen: bool) -> Option<String> {
    (!frozen && row.status == ProposalStatus::Pending)
        .then(|| proposals::proposal_dashboard_path(&row.proposal_id))
}

/// The state, and the date that goes with it.
fn state_cell(row: &ProposalRow) -> Markup {
    html! {
        (status_badge(row.status))
        @match row.status {
            ProposalStatus::Pending => {
                div class="text-text-dim text-xs" {
                    "answer by " (components::compact_stamp(&row.expires_at))
                }
            },
            ProposalStatus::Applied => {
                @if let Some(at) = &row.applied_at {
                    div class="text-text-dim text-xs" {
                        (components::compact_stamp(at)) " · " (applied_by_in_words(row))
                    }
                }
            },
            ProposalStatus::Expired => {
                div class="text-text-dim text-xs" {
                    "was due " (components::compact_stamp(&row.expires_at))
                }
            },
        }
    }
}

/// The state, in the words the tabs and the guide use.
fn status_badge(status: ProposalStatus) -> Markup {
    // Reusing the badge classes the facts table already ships, so the
    // page needs no new utility compiled into `tailwind.css`.
    let (label, class) = match status {
        ProposalStatus::Pending => ("Pending", "badge badge-fresh"),
        ProposalStatus::Applied => ("Applied", "badge badge-default"),
        ProposalStatus::Expired => ("Expired", "badge badge-superseded"),
    };
    html! { span class=(class) { (label) } }
}

/// Who applied the change.
///
/// `applied_by` carries the raw sender id of whoever answered, or the
/// name of the pass that acted on its own — `planner` for a page it
/// minted, `rem` for a link it drew. An empty column would read as "we
/// do not know"; the engine always knows.
fn applied_by_in_words(row: &ProposalRow) -> String {
    match row.applied_by.as_deref() {
        Some("planner" | "rem") | None => "by the memory itself".to_owned(),
        Some(who) => format!("by {who}"),
    }
}

/// The addressee, as the person reading it thinks of it.
fn recipient_in_words(recipient_id: Option<&str>) -> String {
    recipient_id.map_or_else(
        || "nobody in particular".to_owned(),
        |p| principal_in_words(p).to_owned(),
    )
}

/// A `Principal` wire string as the name a reader knows.
///
/// `"user:frodo"` is how the engine writes a person everywhere it stores
/// one; a group keeps its prefix, because "famiglia" alone would read as
/// somebody's name.
fn principal_in_words(principal: &str) -> &str {
    principal.strip_prefix("user:").unwrap_or(principal)
}

// ---------------------------------------------------------------------------
// One proposal
// ---------------------------------------------------------------------------

fn render_detail_body(row: &ProposalRow, reveal: bool, frozen: bool) -> Markup {
    html! {
        @if reveal { (crate::reveal::banner()) }
        p { a href="/dashboard/proposals" { "← All proposals" } }
        h2 { (label(row)) }
        p class="max-w-prose" { (headline(row)) }
        table class="config-table mt-4" {
            tbody {
                tr { th { "State" } td { (state_cell(row)) } }
                tr { th { "Who it is for" } td { (recipient_in_words(row.recipient_id.as_deref())) } }
                tr { th { "Raised" } td { (components::compact_stamp(&row.emitted_at)) } }
                @if row.status == ProposalStatus::Pending {
                    tr { th { "Answer by" } td { (components::compact_stamp(&row.expires_at)) } }
                }
                @if let Some(at) = &row.applied_at {
                    tr {
                        th { "Applied" }
                        td { (components::compact_stamp(at)) " " (applied_by_in_words(row)) }
                    }
                }
                tr { th { "Reference" } td { code { (row.proposal_id) } } }
            }
        }
        (questions_section(row))
        h3 { "What it touched" }
        (value_markup(&row.context))
        @if let Some(chat) = answer_in_chat_href(row, frozen) {
            p class="mt-4" {
                a href=(chat) { "Answer this in the chat →" }
                " — reading it here changes nothing; the chat is where an answer is given."
            }
        }
    }
}

/// The questionnaire, when the row carries one.
///
/// Rendered verbatim rather than re-worded: the question is the sentence
/// the engine composed when it raised the proposal, and it is the same
/// sentence the chat puts to the reader. A second wording of it here
/// would be a second thing to keep true.
///
/// The receipts (`page_create`, `rail_add`) carry an empty array, and get
/// no section at all.
fn questions_section(row: &ProposalRow) -> Markup {
    let empty: Vec<Value> = Vec::new();
    let questions = row.questions.as_array().unwrap_or(&empty);
    if questions.is_empty() {
        return html! {};
    }
    html! {
        h3 {
            @if row.status == ProposalStatus::Pending { "What it asks" } @else { "What was asked" }
        }
        @for q in questions {
            @if let Some(text) = str_at(q, "text") {
                p class="max-w-prose" { (text) }
            }
            @let options = q.get("options").and_then(Value::as_array).unwrap_or(&empty);
            @if !options.is_empty() {
                ul {
                    @for opt in options {
                        li {
                            (option_in_words(opt))
                            @if opt.get("recommended").and_then(Value::as_bool) == Some(true) {
                                " "
                                span class="badge" { "what happens if nobody answers" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One answer, as it was written into the row.
///
/// The emitters disagree about which key carries the wording — some write
/// `label`, the slot conflict writes `text` — so both are read, and the
/// option's `id` is the last resort rather than nothing at all.
fn option_in_words(option: &Value) -> String {
    str_at(option, "label")
        .or_else(|| str_at(option, "text"))
        .or_else(|| str_at(option, "id"))
        .unwrap_or("(unnamed)")
        .to_owned()
}

// ---------------------------------------------------------------------------
// What one proposal says, in the reader's words
// ---------------------------------------------------------------------------

/// The kind of change, short enough to be a column.
fn label(row: &ProposalRow) -> &'static str {
    match row.kind.as_str() {
        kind::WIKI_PROMOTE => match promote_variant(&row.context) {
            mwe_core::promote::VARIANT_PARAGRAPH_TO_FILE => "A page of its own",
            mwe_core::promote::VARIANT_FACT_REFILE => "A fact moved",
            mwe_core::promote::VARIANT_PAGE_MERGE => "Two pages merged",
            mwe_core::promote::VARIANT_PAGES_TO_NEW_WIKI => "A wiki of their own",
            mwe_core::promote::VARIANT_PAGES_INTO_WIKI => "Pages filed away",
            mwe_core::promote::VARIANT_PAGES_REHOME => "Pages moved wiki",
            mwe_core::promote::VARIANT_VALIDITY_CLOSE => "Facts closed",
            mwe_core::promote::VARIANT_VALIDITY_EDIT => "Dates corrected",
            mwe_core::promote::VARIANT_ACL_CHANGE => "Sharing changed",
            _ => "The memory rearranged",
        },
        kind::DEDUP_MERGE => "Two facts merged",
        kind::FACT_FORGET => "A request to forget",
        kind::PAGE_CREATE => "A page the memory made",
        kind::RAIL_ADD => "A link between pages",
        kind::SLOT_CONFLICT => "Two values for one detail",
        // A row written by a newer engine shows as itself rather than
        // being swallowed into a wrong label.
        _ => "Something changed",
    }
}

/// What one proposal is about, in one sentence.
///
/// Composed here from the row's `context` because the page must call no
/// model: a frozen instance is the only place these two pages are the
/// whole proposal surface, and the one thing it may never do is spend a
/// model call to explain a row.
fn headline(row: &ProposalRow) -> String {
    let c = &row.context;
    match row.kind.as_str() {
        kind::WIKI_PROMOTE => promote_headline(c),
        kind::DEDUP_MERGE => {
            "Two facts that said the same thing became one; the other was retired.".to_owned()
        },
        kind::FACT_FORGET => format!(
            "Asked by {who}, who is not the one who said the fact: everybody who can \
             read it is voting. Silence lets the forget through; a majority of no keeps \
             the fact.",
            who = str_at(c, "requester").map_or("somebody", principal_in_words),
        ),
        kind::PAGE_CREATE => format!(
            "The memory gathered {n} facts onto a page nobody named, \u{ab}{title}\u{bb}, \
             in the wiki {wiki}.",
            n = num_at(c, "fact_count"),
            title = str_at(c, "title").unwrap_or("(untitled)"),
            wiki = str_at(c, "wiki_id").unwrap_or("(unknown)"),
        ),
        kind::RAIL_ADD => format!(
            "The page {from} was given a link to {to}, so a reader of the first is \
             sent to the second.",
            from = str_at(c, "from").unwrap_or("(unknown)"),
            to = str_at(c, "to").unwrap_or("(unknown)"),
        ),
        kind::SLOT_CONFLICT => format!(
            "The memory holds one answer for {slot} about {subject} and was told a \
             different one. Whoever said the second could not overwrite the first, so \
             nothing was written and the question went to the person it is about.",
            slot = str_at(c, "slot").unwrap_or("something there can only be one of"),
            subject = str_at(c, "subject_id").map_or("somebody", principal_in_words),
        ),
        other => format!("The memory recorded a change of kind `{other}`."),
    }
}

/// The `variant` discriminator inside a `wiki_promote` context.
///
/// Absent means the oldest shape, which is what the apply handler also
/// falls back to (`mwe_core::promote`).
fn promote_variant(context: &Value) -> &str {
    str_at(context, "variant").unwrap_or(mwe_core::promote::VARIANT_PARAGRAPH_TO_FILE)
}

fn promote_headline(c: &Value) -> String {
    use mwe_core::promote as v;
    match promote_variant(c) {
        v::VARIANT_PARAGRAPH_TO_FILE => format!(
            "{n} facts left {page} for a page of their own, {target}.",
            n = len_at(c, "fact_ids"),
            page = str_at(c, "source_page").unwrap_or("their page"),
            target = str_at(c, "recommended_target_page").unwrap_or("a new page"),
        ),
        v::VARIANT_FACT_REFILE => format!(
            "One fact left {page} and was filed on {dest} in the wiki {wiki}.",
            page = str_at(c, "source_page").unwrap_or("its page"),
            dest = str_at(c, "recommended_dest_page").unwrap_or("another page"),
            wiki = str_at(c, "recommended_dest_wiki_id").unwrap_or("(unknown)"),
        ),
        v::VARIANT_PAGE_MERGE => format!(
            "The page {husk} said the same thing as {target}, so its {n} facts moved \
             there and it was deleted.",
            husk = str_at(c, "source_page").unwrap_or("one page"),
            target = str_at(c, "recommended_target_page").unwrap_or("another"),
            n = len_at(c, "fact_ids"),
        ),
        v::VARIANT_PAGES_TO_NEW_WIKI => format!(
            "{n} pages turned out to be one subject, so they became a wiki of their \
             own, {slug}.",
            n = len_at(c, "pages"),
            slug = str_at(c, "new_wiki_slug").unwrap_or("(unnamed)"),
        ),
        v::VARIANT_PAGES_INTO_WIKI => format!(
            "{n} pages were filed into the wiki {target}.",
            n = len_at(c, "pages"),
            target = str_at(c, "target_wiki_id").unwrap_or("(unknown)"),
        ),
        v::VARIANT_PAGES_REHOME => format!(
            "{n} pages were born in the wrong place and moved from the wiki {from} to \
             {to}.",
            n = len_at(c, "pages"),
            from = str_at(c, "source_wiki_id").unwrap_or("(unknown)"),
            to = str_at(c, "target_wiki_id").unwrap_or("(unknown)"),
        ),
        v::VARIANT_VALIDITY_CLOSE => format!(
            "{n} facts stopped being true, and the memory closed them.",
            n = len_at(c, "closed"),
        ),
        v::VARIANT_VALIDITY_EDIT => format!(
            "{n} facts kept their meaning and had their dates corrected.",
            n = len_at(c, "edited"),
        ),
        v::VARIANT_ACL_CHANGE => {
            format!("Who may read {n} facts changed.", n = len_at(c, "changed"))
        },
        other => format!("The memory rearranged itself, in a way it calls `{other}`."),
    }
}

// ---------------------------------------------------------------------------
// The stored row, field by field
// ---------------------------------------------------------------------------

/// The `context` object, rendered as what it holds.
///
/// Deliberately **generic**: it walks whatever the row carries instead of
/// naming the fields of each kind. A per-kind template would be a second
/// copy of what the emitters write, and the day one of them gained a
/// field the page would go on printing the old set without saying so.
/// Here an unknown key shows under its own name, which is the failure
/// worth having.
fn value_markup(value: &Value) -> Markup {
    match value {
        Value::Object(fields) => html! {
            table class="config-table" {
                tbody {
                    @for (key, v) in fields {
                        @if !v.is_null() && key.as_str() != "variant" {
                            tr {
                                th { (field_label(key)) }
                                td { (value_markup(v)) }
                            }
                        }
                    }
                }
            }
        },
        Value::Array(items) if items.is_empty() => html! { span class="muted" { "none" } },
        Value::Array(items) => html! {
            ul {
                @for item in items {
                    li { (value_markup(item)) }
                }
            }
        },
        // A stored instant is written to the nine digits a column wants
        // and read to the second, the same way every other date on the
        // dashboard is. Anything that is not one is passed through: a
        // validity bound can be a bare year or an open end.
        Value::String(s) => html! {
            @if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                (components::compact_stamp(s))
            } @else {
                (s)
            }
        },
        other => html! { (other.to_string()) },
    }
}

/// What each stored key is called on screen, key first.
///
/// The keys are what the emitters in `mwe_core::promote`,
/// `mwe_core::planner`, `mwe_core::rem`, `mwe_core::votes`,
/// `mwe_core::dedup` and `mwe_core::proposals` write. A table rather than
/// a `match` because that is what it is — one line per key, in the order
/// a reader would look one up.
const FIELD_LABELS: &[(&str, &str)] = &[
    ("asserted_by", "Who said the other one"),
    ("asserted_text", "What was said instead"),
    ("audit_id", "Disclosure record"),
    ("changed", "Sharing changed"),
    ("closed", "Facts closed"),
    ("cycle_id", "Which night"),
    ("description", "What it is about"),
    ("edited", "Dates corrected"),
    ("eligible_voters", "Who may vote"),
    ("fact_count", "Facts on it"),
    ("fact_id", "The fact"),
    ("fact_ids", "The facts"),
    ("for_fact", "The fact"),
    ("from", "From the page"),
    ("gesture", "What was said at the time"),
    ("group_pages", "Pages in the group"),
    ("husk_description", "What the page that went was about"),
    ("husk_style", "How that page was written"),
    ("husk_title", "Name of the page that went"),
    ("instead_of", "Instead of a link to"),
    ("jaccard", "How alike they were"),
    ("kept_fact_id", "The fact the memory kept"),
    ("kept_said_on", "Said on"),
    ("kept_sender", "Said by"),
    ("kept_text", "What the memory holds"),
    ("loser_fact_id", "The fact that was retired"),
    ("minted_at", "Made on"),
    ("new_allow_ids", "Now readable by"),
    ("new_subject_id", "Now about"),
    ("new_valid_from", "Now true from"),
    ("new_valid_to", "Now true until"),
    ("new_wiki_description", "What it is about"),
    ("new_wiki_slug", "The new wiki"),
    ("new_wiki_style", "How it is written"),
    ("new_wiki_title", "Its name"),
    ("page_path", "The page"),
    ("pages", "The pages"),
    (
        "parked_capture_id",
        "The other value, waiting on this answer",
    ),
    ("preview", "The fact, as far as it fits"),
    ("prev_allow_ids", "Was readable by"),
    ("prev_subject_id", "Was about"),
    ("prev_valid_from", "Was true from"),
    ("prev_valid_to", "Was true until"),
    ("reason", "Why"),
    ("recall_count_30d", "Times recalled in 30 days"),
    ("recommended_dest_page", "To the page"),
    ("recommended_dest_wiki_id", "To the wiki"),
    ("recommended_target_page", "To the page"),
    ("refusal", "Why it was not simply rewritten"),
    ("requester", "Asked by"),
    ("slot", "The thing there is only one of"),
    ("slug", "Its short name"),
    ("source_page", "From the page"),
    ("source_wiki_id", "In the wiki"),
    ("source_wiki_pages", "Pages in the wiki they left"),
    ("subject_id", "About"),
    ("successor_fact_id", "The fact that replaced it"),
    ("surface", "Where it was stored"),
    ("target_wiki_id", "To the wiki"),
    ("title", "Its name"),
    ("to", "To the page"),
    ("trigger_page_facts", "Facts on the page at the time"),
    ("valid_to", "Stopped being true on"),
    ("why", "Why"),
    ("widening", "It opened the fact up"),
    ("wiki_id", "In the wiki"),
    ("winner_fact_id", "The fact that stayed"),
];

/// What one stored key is called on screen.
///
/// A key with no entry in [`FIELD_LABELS`] is shown as itself with the
/// underscores opened out, so a field the engine grows tomorrow arrives
/// readable rather than invisible.
fn field_label(key: &str) -> String {
    FIELD_LABELS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(|| key.replace('_', " "), |(_, label)| (*label).to_owned())
}

/// One string field of a JSON object, when it is there and is a string.
fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// One counted field, as a number a sentence can carry (`0` when absent).
fn num_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// How many items one array field holds (`0` when absent).
fn len_at(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, context: Value, status: ProposalStatus) -> ProposalRow {
        ProposalRow {
            proposal_id: "0197fa00-0000-7000-8000-000000000001".to_owned(),
            kind: kind.to_owned(),
            context,
            questions: serde_json::json!([]),
            emitted_at: "2026-09-09T10:00:00+00:00".to_owned(),
            expires_at: "2026-09-10T10:00:00+00:00".to_owned(),
            status,
            applied_at: None,
            applied_by: None,
            recipient_id: None,
        }
    }

    /// The whole point of the page on a frozen instance: every kind says
    /// something a person can read, out of the stored row alone. A kind
    /// that fell through to the raw JSON would be the failure.
    #[test]
    fn every_kind_says_something_in_words() {
        for k in kind::ALL {
            let r = row(k, serde_json::json!({}), ProposalStatus::Applied);
            let line = headline(&r);
            assert!(
                !line.contains('{') && line.ends_with('.') && line.len() > 20,
                "{k}: {line}"
            );
            assert_ne!(label(&r), "Something changed", "{k} has no label");
        }
    }

    /// Each `wiki_promote` variant is its own sentence: the nine of them
    /// are the bulk of the table, and collapsing them into one would
    /// leave nine identical rows on screen.
    #[test]
    fn every_promote_variant_says_something_of_its_own() {
        use mwe_core::promote as v;
        let variants = [
            v::VARIANT_PARAGRAPH_TO_FILE,
            v::VARIANT_FACT_REFILE,
            v::VARIANT_PAGE_MERGE,
            v::VARIANT_PAGES_TO_NEW_WIKI,
            v::VARIANT_PAGES_INTO_WIKI,
            v::VARIANT_PAGES_REHOME,
            v::VARIANT_VALIDITY_CLOSE,
            v::VARIANT_VALIDITY_EDIT,
            v::VARIANT_ACL_CHANGE,
        ];
        let mut seen: Vec<String> = Vec::new();
        for variant in variants {
            let r = row(
                kind::WIKI_PROMOTE,
                serde_json::json!({ "variant": variant }),
                ProposalStatus::Applied,
            );
            let line = headline(&r);
            assert!(
                !line.contains(variant),
                "{variant} is shown as its token: {line}"
            );
            assert!(
                !seen.contains(&line),
                "{variant} repeats an earlier sentence: {line}"
            );
            seen.push(line);
        }
    }

    /// A kind the engine grows tomorrow shows as itself instead of being
    /// dressed up as one of today's.
    #[test]
    fn an_unknown_kind_shows_as_itself() {
        let r = row(
            "something_new",
            serde_json::json!({}),
            ProposalStatus::Applied,
        );
        assert!(headline(&r).contains("something_new"));
        assert_eq!(label(&r), "Something changed");
    }

    /// The listing offers the chat only where an answer can still be
    /// given: not on a change that already happened, and never on a
    /// frozen deployment — where the chat panel is not even rendered.
    #[test]
    fn the_chat_is_offered_only_where_an_answer_is_still_possible() {
        let pending = row(
            kind::SLOT_CONFLICT,
            serde_json::json!({}),
            ProposalStatus::Pending,
        );
        assert!(answer_in_chat_href(&pending, /* frozen */ false).is_some());
        assert!(answer_in_chat_href(&pending, /* frozen */ true).is_none());

        for done in [ProposalStatus::Applied, ProposalStatus::Expired] {
            let r = row(kind::DEDUP_MERGE, serde_json::json!({}), done);
            assert!(answer_in_chat_href(&r, false).is_none(), "{done:?}");
        }
    }

    /// A key nobody wrote a label for is still readable, and the
    /// discriminator is not printed twice (it is already the heading).
    #[test]
    fn an_unlabelled_field_is_shown_under_its_own_name() {
        assert_eq!(field_label("kept_text"), "What the memory holds");
        assert_eq!(field_label("some_new_key"), "some new key");
        let html = value_markup(&serde_json::json!({
            "variant": "paragraph_to_file",
            "some_new_key": "value",
        }))
        .into_string();
        assert!(html.contains("some new key"), "{html}");
        assert!(!html.contains("paragraph_to_file"), "{html}");
    }

    /// A `null` is the emitters' way of saying "there was none", so the
    /// page leaves the row out instead of printing an empty one.
    #[test]
    fn an_absent_value_gets_no_row() {
        let html =
            value_markup(&serde_json::json!({ "reason": null, "why": "because" })).into_string();
        assert!(html.contains("because"), "{html}");
        assert!(!html.contains("Why</th><td></td>"), "{html}");
    }

    /// The pass that acts on its own has a name in the row; a person has
    /// their own.
    #[test]
    fn who_applied_it_reads_as_a_person_or_as_the_engine() {
        let mut r = row(
            kind::PAGE_CREATE,
            serde_json::json!({}),
            ProposalStatus::Applied,
        );
        r.applied_by = Some("planner".to_owned());
        assert_eq!(applied_by_in_words(&r), "by the memory itself");
        r.applied_by = Some("frodo".to_owned());
        assert_eq!(applied_by_in_words(&r), "by frodo");
        r.applied_by = None;
        assert_eq!(applied_by_in_words(&r), "by the memory itself");
    }

    #[test]
    fn the_status_filter_reads_the_wire_words_and_ignores_anything_else() {
        assert_eq!(parse_status("pending"), Some(ProposalStatus::Pending));
        assert_eq!(parse_status("applied"), Some(ProposalStatus::Applied));
        assert_eq!(parse_status("expired"), Some(ProposalStatus::Expired));
        assert_eq!(parse_status("whatever"), None);
    }

    /// Both wordings the emitters use for an answer are read, so no
    /// option ever renders as its bare id when it has a sentence.
    #[test]
    fn an_answer_is_read_from_whichever_key_carries_its_wording() {
        assert_eq!(
            option_in_words(&serde_json::json!({ "id": "merge", "label": "Merge" })),
            "Merge"
        );
        assert_eq!(
            option_in_words(&serde_json::json!({ "id": "keep", "text": "Yes — it still holds." })),
            "Yes — it still holds."
        );
        assert_eq!(
            option_in_words(&serde_json::json!({ "id": "forget" })),
            "forget"
        );
    }
}
