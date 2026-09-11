// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Recall traces** — the last recalls, listed and replayed.
//!
//! Three routes, open to every signed-in user:
//!
//! - GET `/recall-traces` — the journal (newest first): one row per
//!   recorded recall run, linking to its viewer.
//! - GET `/recall-traces/:id` — the viewer page: the animated 3D replay
//!   of the route the recall took (WebGL, `recall-trace.js`) mounted above the
//!   full textual trace — which is also the no-JS / no-WebGL fallback.
//! - GET `/recall-traces/:id/data` — the JSON payload the viewer fetches
//!   (`meta` + the decoded [`mwe_core::recall_trace::RecallTrace`]).
//!
//! # Ownership is the gate, and it is the only gate
//!
//! **A trace belongs to the sender it was recorded for**, and
//! [`readable`] is the whole policy: your own always, anybody else's only
//! under the reveal switch ([`crate::reveal`]) — which is itself
//! admin-only, so a non-admin can never widen past themself.
//!
//! **The admin role is deliberately not the gate here.** It would be the only
//! one available if the journal listed *everybody's* recalls; scoped to its own
//! sender, a trace stops being system telemetry — a user opening their own is
//! seeing how the memory arrived at the answer it gave **them**, over their own
//! pages, inside their own permissions. That is transparency, and withholding
//! it from the person the recall ran for is backwards: the 3D replay of the
//! route is the clearest explanation of the product anyone gets.
//!
//! The consequence to keep in mind when editing: with no role gate,
//! **the route itself is what stops one user reading another's
//! trace**. There is no outer check to fall back on. `load_readable` is
//! that check, and it answers [`DashboardError::NotFound`] rather than
//! `Forbidden` — trace ids are a dense autoincrement, so `403` would
//! confirm that somebody else's recall exists at that id.
//!
//! Widening the journal past yourself is still the reveal switch — the
//! same single control that governs the facts table and the wiki pages,
//! not a second policy, and still lockable from the config file
//! (`instance.admin_reveal_locked`).
//!
//! The journal itself is written by the two recall producers (see
//! [`mwe_core::recall_trace`]): the ingest per-turn injection and the
//! `wiki_navigate` tool. This surface only reads it.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use mwe_core::recall_nav::HopTrace;
use mwe_core::recall_trace::{self, RecallTrace, TraceRow, TraceSource};

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

/// Routes for the Recall-traces surface. Merged into the authenticated
/// tree, and deliberately **not** under `/admin/`: the page is scoped to
/// the reader's own recalls, so it is not an operator surface.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/recall-traces", get(index))
        .route("/recall-traces/:id", get(viewer))
        .route("/recall-traces/:id/data", get(data))
}

/// How many traces the journal page lists. A **page size**, not a retention
/// bound: the window that decides how long a trace exists at all is
/// `recall.trace_retention_days`, and the journal holds months of it. Sized
/// to stay a single readable page — this surface is "my recent recalls", and
/// the analysis surface for anything longer is the table itself.
const INDEX_PAGE_LIMIT: i64 = 50;

/// May `user` open the trace `row` was recorded for?
///
/// Own traces always; anyone else's only under the reveal switch (which is
/// itself admin-only, so `reveal` can never be true for a non-admin).
///
/// This is the **only** thing standing between two users' traces — no role
/// gate sits above it. Every handler in this module goes through it, the two
/// by-id ones via [`load_readable`].
fn readable(row: &TraceRow, user: &SessionUser, reveal: bool) -> bool {
    reveal || row.sender_id == user.sender_id
}

/// `GET /dashboard/recall-traces` — the journal, newest first.
///
/// Scoped to the reader's own recalls unless the reveal switch is on.
async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let reveal = crate::reveal::active(&state, &user, &jar);
    // Scope in the query, not after the fetch. The journal now holds months
    // of traffic rather than ten rows, so filtering a deployment-wide page
    // down to the reader's own would show them fewer and fewer of their own
    // traces the busier the deployment got — the listing would silently
    // become a function of everyone else's activity.
    let rows: Vec<TraceRow> = if reveal {
        recall_trace::recent_traces(&state.pool, INDEX_PAGE_LIMIT)
            .await
            .map_err(|e| DashboardError::Internal(format!("recall_trace::recent_traces: {e}")))?
    } else {
        recall_trace::recent_traces_for_sender(&state.pool, &user.sender_id, INDEX_PAGE_LIMIT)
            .await
            .map_err(|e| {
                DashboardError::Internal(format!("recall_trace::recent_traces_for_sender: {e}"))
            })?
    };
    debug_assert!(rows.iter().all(|r| readable(r, &user, reveal)));
    Ok(Html(layout::authenticated_page(
        chrome,
        "Recall traces",
        &user,
        &render_index_body(&rows, reveal, user.is_admin),
    )))
}

/// `GET /dashboard/recall-traces/:id` — the viewer page.
async fn viewer(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(id): AxumPath<i64>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let reveal = crate::reveal::active(&state, &user, &jar);
    let row = load_readable(&state, id, &user, reveal).await?;
    let trace = row
        .parse()
        .map_err(|e| DashboardError::Internal(format!("recall_trace payload: {e}")))?;
    Ok(Html(layout::authenticated_page(
        chrome,
        &format!("Recall trace #{id}"),
        &user,
        &render_viewer_body(&row, &trace, reveal),
    )))
}

/// `GET /dashboard/recall-traces/:id/data` — the viewer's JSON feed.
///
/// The one that actually carries the recalled fact bodies, so it goes
/// through [`load_readable`] exactly like the page does.
async fn data(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(id): AxumPath<i64>,
) -> Result<Response> {
    let reveal = crate::reveal::active(&state, &user, &jar);
    let row = load_readable(&state, id, &user, reveal).await?;
    let trace = row
        .parse()
        .map_err(|e| DashboardError::Internal(format!("recall_trace payload: {e}")))?;
    Ok(axum::Json(serde_json::json!({
        "id": row.id,
        "created_at": row.created_at,
        "source": row.source.as_str(),
        "sender_id": row.sender_id,
        "trace": trace,
    }))
    .into_response())
}

/// Fetch trace `id` and apply the same scoping the journal applies.
///
/// A trace the reader may not open is [`DashboardError::NotFound`], not
/// `Forbidden`: the id space is a dense autoincrement, and answering
/// "forbidden" would confirm that someone else's recall exists at that id.
async fn load_readable(
    state: &DashboardState,
    id: i64,
    user: &SessionUser,
    reveal: bool,
) -> Result<TraceRow> {
    let row = recall_trace::get_trace(&state.pool, id)
        .await
        .map_err(|e| DashboardError::Internal(format!("recall_trace::get_trace: {e}")))?
        .ok_or(DashboardError::NotFound)?;
    if readable(&row, user, reveal) {
        Ok(row)
    } else {
        Err(DashboardError::NotFound)
    }
}

// ---------- Rendering: the journal ----------

/// `is_admin` only decides whether the "widen this with reveal" hint is
/// worth showing: reveal is admin-only, so pointing a regular user at a
/// switch they do not have would be an invitation to a dead end.
fn render_index_body(rows: &[TraceRow], reveal: bool, is_admin: bool) -> Markup {
    html! {
        @if reveal { (crate::reveal::banner()) }
        p class="text-text-dim max-w-prose" {
            "The most recent recalls, newest first — what your own turns pulled "
            "out of memory, and what was handed to the consumer (the bot or "
            "assistant that was talking to you). Open one to watch the walk it "
            "took through the pages."
            @if reveal {
                " Admin reveal is on, so this is every user's recall."
            } @else if is_admin {
                " Only your own recalls are listed; turn on Admin reveal in "
                a href="/dashboard/settings/me" { "Settings" }
                " to see everyone's."
            }
        }
        @if rows.is_empty() {
            p class="mt-6" { "No traces yet — they appear as soon as a consumer turn or a deep search runs." }
        } @else {
            table class="config-table mt-4" {
                thead {
                    tr {
                        th { "When" }
                        th { "What ran" }
                        th { "Who it was for" }
                        th { "What was asked" }
                        th { "Facts found" }
                        th { "Steps walked" }
                        th { "Why it stopped" }
                        th { "Text handed over" }
                        th { }
                    }
                }
                tbody {
                    @for row in rows {
                        (render_index_row(row))
                    }
                }
            }
        }
    }
}

fn render_index_row(row: &TraceRow) -> Markup {
    // A payload that fails to decode still gets a row (id + link) — the
    // viewer will surface the decode error loudly.
    let trace = row.parse().ok();
    let href = format!("/dashboard/recall-traces/{}", row.id);
    html! {
        tr {
            td { (crate::ui::components::compact_stamp(&row.created_at)) }
            td { (source_badge(row.source)) }
            td { (row.sender_id) }
            td class="trace-turn-cell" {
                @match &trace {
                    Some(t) => (t.turn_text),
                    None => span class="text-text-dim" { "(payload unreadable)" },
                }
            }
            td { @if let Some(t) = &trace { (hit_counts(t)) } }
            td { @if let Some(t) = &trace { (t.hops.len()) } }
            td { @if let Some(t) = &trace {
                @match &t.nav_stop {
                    Some(stop) => span title=(nav_stop_sentence(stop)) { (nav_stop_short(stop)) },
                    None => span class="text-text-dim" { "did not walk" },
                }
            } }
            td { @if let Some(t) = &trace {
                (t.injected_block.as_deref().map_or(0, str::len)) " characters"
            } }
            td { a href=(href) { "view" } }
        }
    }
}

/// The three places a recalled fact can come from, counted, in the
/// words the viewer's own section headings use.
fn hit_counts(t: &RecallTrace) -> String {
    let mut parts = vec![format!("{} by similarity", t.flat_hits.len())];
    if !t.fresh_hits.is_empty() {
        parts.push(format!("{} not yet on a page", t.fresh_hits.len()));
    }
    if !t.due_soon.is_empty() {
        parts.push(format!("{} closing soon", t.due_soon.len()));
    }
    parts.join(" · ")
}

/// What made the recall run: a consumer's own turn, or somebody asking
/// the memory to go and look.
fn source_badge(source: TraceSource) -> Markup {
    let label = match source {
        TraceSource::Ingest => "consumer turn",
        TraceSource::Navigate => "deep search",
    };
    html! { span class="badge" { (label) } }
}

/// Why the walk ended, short enough for a table cell.
///
/// The wire tokens are [`mwe_core::recall_nav::NavStop`]; an unknown one
/// (a journal row written by a newer engine) shows as itself rather than
/// being swallowed.
fn nav_stop_short(stop: &str) -> &str {
    match stop {
        "done" => "collected enough",
        "budget" => "text budget spent",
        "hop_cap" => "depth limit reached",
        "llm_degraded" => "navigator failed",
        "nothing_opened" => "every page refused",
        "pool_exhausted" => "no pages left",
        "empty_fan" => "no doors to walk",
        other => other,
    }
}

/// The same reason said in full. Kept word for word in step with
/// `STOP_SENTENCES` in `assets/recall-trace.js`, which prints it over the
/// replay of the same walk.
fn nav_stop_sentence(stop: &str) -> &str {
    match stop {
        "done" => "the navigator judged what it had collected enough",
        "budget" => "the prose budget ran out",
        "hop_cap" => "the walk reached its depth limit",
        "llm_degraded" => "the navigator model failed — the turn went on with what was collected",
        "nothing_opened" => "every page it asked for was refused",
        "pool_exhausted" => "no unvisited candidate was left",
        "empty_fan" => "there was no door to start from",
        other => other,
    }
}

/// Where the words the search started from came from.
///
/// The wire tokens are the `seed_mode` strings the recall path writes
/// ([`mwe_core::recall_trace::RecallTrace`]); an unknown one shows as
/// itself.
fn seed_mode_in_words(mode: &str) -> &str {
    match mode {
        "classifier" => "what the engine read the message to be about",
        "caller" => "the words the caller named",
        "query_extraction" => "words pulled out of the question",
        "rag_only" => "similarity alone",
        "guest" => "a guest turn, with nobody to look up",
        "repeat" => "the same message again, answered with the first answer",
        other => other,
    }
}

/// How a page came to be offered to the navigator.
///
/// The wire tokens are the `origin` strings the gatherer and the walk write
/// ([`mwe_core::recall_nav`]); an unknown one shows as itself.
fn origin_in_words(origin: &str) -> &str {
    match origin {
        "rag" => "similarity",
        "description" => "the page says it is about this",
        "topic" => "a topic of the message",
        "situational" => "the situation the consumer described",
        "link" => "a link from a page already open",
        "card" => "a card rail of an open page",
        other => other,
    }
}

/// The seat a recalled fact took in the block — two of the three put a
/// fact there *against* similarity, and the score alone cannot say so.
fn seat_in_words(seat: &str) -> &str {
    match seat {
        "similarity" => "similarity",
        "macrotopic_quota" => "a seat kept for its macrotopic",
        "one_fact_per_kind" => "one fact of its kind",
        other => other,
    }
}

/// Why a fact the search returned never reached the consumer.
fn drop_in_words(reason: &str) -> &str {
    match reason {
        "rules_page" => "it sits on a rules page, which is never recalled as a fact",
        "on_an_injected_page" => "the page it sits on was read whole by the walk",
        "relevance_floor" => "below the relevance floor",
        "intent_skipped_the_slot" => "this turn did not open the facts slot",
        other => other,
    }
}

/// Why a page the navigator asked for did not open.
fn refusal_in_words(reason: &str) -> &str {
    match reason {
        "not_offered" => "not among the pages it was shown",
        "wiki_vanished" => "the wiki is gone",
        "rules_page" => "a rules page, closed to the walk",
        "already_read" => "asked for twice in one decision",
        "acl_unreadable" => "this reader may not open it",
        "unreadable" => "the page could not be read",
        other => other,
    }
}

/// Which slot of the block served an identity card whole.
fn role_in_words(role: &str) -> &str {
    match role {
        "speaker" => "the person speaking",
        "mentioned" => "named in the message",
        other => other,
    }
}

/// Which half of the project-documentation slot a section came from.
fn half_in_words(half: &str) -> &str {
    match half {
        "named" => "the project the message names",
        "signposted" => "a project the memory signposts",
        other => other,
    }
}

/// The text of a recalled fact, looked up by id — the fan names the fact
/// that opened each door by its id.
fn fact_text<'a>(trace: &'a RecallTrace, fact_id: &str) -> Option<&'a str> {
    trace
        .flat_hits
        .iter()
        .find(|h| h.fact_id == fact_id)
        .map(|h| h.text.as_str())
}

fn page_or_none(page: Option<&str>) -> &str {
    page.unwrap_or("no page named")
}

// ---------- Rendering: the viewer ----------

fn render_viewer_body(row: &TraceRow, trace: &RecallTrace, reveal: bool) -> Markup {
    let navigate = row.source == TraceSource::Navigate;
    html! {
        @if reveal { (crate::reveal::banner()) }
        // Stage mount for the 3D replay. recall-trace.js (an ES module — it
        // imports the vendored three.module.min.js relative to itself)
        // hydrates it from the /data endpoint; without JS/WebGL the stage
        // stays empty and the textual trace below is the whole surface.
        div id="trace-stage" class="trace-stage" data-trace-id=(row.id) {}
        script src=(crate::assets::asset_url("recall-trace.js")) type="module" {}

        (render_meta(row, trace, navigate))
        (render_served(trace))
        (render_facts(trace, navigate))
        (render_fresh(trace))
        (render_due(trace))
        (render_docs(trace))
        (render_fan(trace))
        (render_hops(trace))
        (render_reconcile(trace))
        (render_injected(trace, navigate))
    }
}

fn render_meta(row: &TraceRow, trace: &RecallTrace, navigate: bool) -> Markup {
    let light = trace.recall_depth.as_deref() == Some("light");
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "What was asked" }
            p class="whitespace-pre-wrap" { (trace.turn_text) }
            @if let Some(completed) = &trace.completed_message {
                @if completed != &trace.turn_text {
                    p class="mb-1" { strong { "The completed message" } }
                    p class="whitespace-pre-wrap" { (completed) }
                    p.muted {
                        "The engine filled in what the sentence left implicit. "
                        @if trace.flat_hits_from_completed {
                            "The facts below answer this sentence, not the words as written."
                        } @else {
                            "The search on it found nothing: the facts below answer the words as written."
                        }
                    }
                }
            }
            table class="config-table" {
                tbody {
                    tr { th { "When" } td { (crate::ui::components::compact_stamp(&row.created_at)) } }
                    tr { th { "What ran" } td { (source_badge(row.source)) } }
                    tr { th { "Who it was for" } td { (row.sender_id) } }
                    @if let Some(consumer) = &trace.consumer {
                        tr { th { "Consumer" } td {
                            (consumer)
                            span.muted { " — the bot or assistant that asked" }
                        } }
                    }
                    @if let Some(intent) = &trace.intent {
                        tr { th { "Read as" } td {
                            (crate::ui::components::intent_in_words(intent))
                        } }
                    }
                    @if !navigate {
                        tr { th { "How deep" } td {
                            @if light {
                                "light — the consumer asked for a shorter recall, so the walk was skipped"
                            } @else {
                                "full — the walk was allowed to run"
                            }
                        } }
                    }
                    tr { th { "Search started from" } td {
                        (seed_mode_in_words(&trace.seed_mode))
                        @if !trace.topics.is_empty() { " · topics: " (trace.topics.join(", ")) }
                        @if !trace.subjects.is_empty() { " · people: " (trace.subjects.join(", ")) }
                    } }
                    tr { th { "The walk" } td {
                        @match &trace.nav_stop {
                            Some(stop) => {
                                (trace.hops.len()) " step(s), stopped because "
                                (nav_stop_sentence(stop)) " · "
                                (trace.chars_collected) " of " (trace.char_budget)
                                " characters of prose collected"
                                @if trace.truncated { " (the budget cut the last page)" }
                            },
                            None => {
                                @if light { "skipped by request" }
                                @else { "did not run this turn" }
                            },
                        }
                    } }
                    tr { th { "Time" } td {
                        @if navigate {
                            (trace.took_ms) " ms"
                        } @else {
                            "recall " (trace.recall_ms) " ms of a " (trace.took_ms) " ms turn"
                            span.muted { " — the turn also classifies, reconciles and writes" }
                        }
                    } }
                }
            }
        }
    }
}

fn render_served(trace: &RecallTrace) -> Markup {
    if trace.served_pages.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Handed over whole" }
            p.muted {
                "Identity cards the consumer received in full. The walk never "
                "opens these pages: it already has them."
            }
            table class="config-table" {
                thead { tr { th { "Wiki" } th { "Page" } th { "Why" } } }
                tbody {
                    @for s in &trace.served_pages {
                        tr {
                            td { (s.wiki_id) }
                            td { (s.page) }
                            td { (role_in_words(&s.role)) }
                        }
                    }
                }
            }
        }
    }
}

fn render_facts(trace: &RecallTrace, navigate: bool) -> Markup {
    if trace.flat_hits.is_empty() {
        return html! {};
    }
    let title = if navigate {
        "Fragments the search returned"
    } else {
        "Facts the search returned"
    };
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { (title) }
            p.muted {
                "Each fact says which seat it took: similarity ranks most of them, "
                "but a seat kept for the macrotopic and the one-fact-of-each-kind "
                "seat put a fact here against the score."
            }
            table class="config-table" {
                thead {
                    tr {
                        th { "Score" }
                        th { "Seat" }
                        th { "Kind" }
                        th { "Wiki / page" }
                        th { "Region" }
                        th { "Text" }
                        th { "After the search" }
                    }
                }
                tbody {
                    @for h in &trace.flat_hits {
                        tr {
                            td {
                                (format!("{:.3}", h.score))
                                @if h.link_key_win { br; span class="text-text-dim text-xs" { "on the link key" } }
                            }
                            td { (h.seat.as_deref().map_or("similarity", seat_in_words)) }
                            td { (h.fact_type.as_deref().unwrap_or("—")) }
                            td { (h.wiki_id) br; span class="text-text-dim text-xs" { (h.source_path) } }
                            td { @match (h.region_start, h.region_end) {
                                (Some(s), Some(e)) => { (s) "–" (e) },
                                _ => { "—" },
                            } }
                            td class="whitespace-pre-wrap" { (h.text) }
                            td class="text-xs" {
                                @if let Some(v) = h.voted_score { "voted " (format!("{v:.3}")) br; }
                                @match &h.dropped {
                                    Some(reason) => { span class="text-amber" { "dropped: " (drop_in_words(reason)) } },
                                    None => { "handed over" },
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_fresh(trace: &RecallTrace) -> Markup {
    if trace.fresh_hits.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Not yet on a page" }
            p.muted { "Captured recently and still waiting to be filed: no wiki, no page, handed over as they are." }
            table class="config-table" {
                thead { tr { th { "Score" } th { "Kind" } th { "Text" } } }
                tbody {
                    @for h in &trace.fresh_hits {
                        tr {
                            td { (format!("{:.3}", h.score)) }
                            td { (h.fact_type.as_deref().unwrap_or("—")) }
                            td class="whitespace-pre-wrap" { (h.text) }
                        }
                    }
                }
            }
        }
    }
}

fn render_due(trace: &RecallTrace) -> Markup {
    if trace.due_soon.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Closing soon" }
            p.muted { "Dated items that close within the horizon — the date is what put them here, not a score." }
            table class="config-table" {
                thead { tr { th { "Due" } th { "Wiki / page" } th { "Text" } } }
                tbody {
                    @for h in &trace.due_soon {
                        tr {
                            td { (h.valid_to.as_deref().unwrap_or("—")) }
                            td { (h.wiki_id) br; span class="text-text-dim text-xs" { (h.source_path) } }
                            td class="whitespace-pre-wrap" { (h.text) }
                        }
                    }
                }
            }
        }
    }
}

fn render_docs(trace: &RecallTrace) -> Markup {
    if trace.project_docs.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Project notes" }
            p.muted { "Documentation a coding assistant keeps, handed over as reference — never filed as a fact." }
            table class="config-table" {
                thead { tr { th { "Score" } th { "Which project" } th { "Wiki / page" } th { "Section" } th { "Text" } } }
                tbody {
                    @for d in &trace.project_docs {
                        tr {
                            td { (format!("{:.3}", d.score)) }
                            td { (half_in_words(&d.half)) }
                            td { (d.wiki_id) br; span class="text-text-dim text-xs" { (d.source_path) } }
                            td { (d.heading_path.as_deref().unwrap_or("—")) }
                            td class="whitespace-pre-wrap" { (d.text) }
                        }
                    }
                }
            }
        }
    }
}

fn render_fan(trace: &RecallTrace) -> Markup {
    if trace.entry_points.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Doors the walk could start from" }
            p.muted {
                "The pages the navigator was offered as a starting point, "
                "strongest first. A similarity door names the fact that opened it; "
                "the others were found by the page itself."
            }
            table class="config-table" {
                thead { tr { th { "Strength" } th { "Found by" } th { "Wiki" } th { "Page" } th { "Opened by" } } }
                tbody {
                    @for ep in &trace.entry_points {
                        tr {
                            td { (format!("{:.2}", ep.weight)) }
                            td { (origin_in_words(&ep.origin)) }
                            td { (ep.wiki_id) }
                            td { (page_or_none(ep.page.as_deref())) }
                            td class="text-xs whitespace-pre-wrap" {
                                @match ep.matched_fact.as_deref().and_then(|id| fact_text(trace, id)) {
                                    Some(text) => { (text) },
                                    None => { "—" },
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_hops(trace: &RecallTrace) -> Markup {
    if trace.hops.is_empty() {
        return html! {};
    }
    html! {
        @for (i, hop) in trace.hops.iter().enumerate() {
            (render_hop(i, hop))
        }
    }
}

fn render_hop(i: usize, hop: &HopTrace) -> Markup {
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Step " (i + 1) }
            @if let Some(note) = &hop.note {
                p { "Navigator: " em { (PreEscaped("\u{201c}")) (note) (PreEscaped("\u{201d}")) }
                    @if hop.done { " · done" } }
            } @else if hop.done { p { "Navigator: done" } }
            details {
                summary { (hop.candidates.len()) " page(s) offered to choose from" }
                table class="config-table" {
                    thead { tr { th { "Found by" } th { "Wiki" } th { "Page" } th { "What it says it holds" } } }
                    tbody {
                        @for c in &hop.candidates {
                            tr {
                                td { (origin_in_words(&c.origin)) }
                                td { (c.wiki_id) }
                                td { (page_or_none(c.page.as_deref())) }
                                td class="text-xs" {
                                    @if let Some(s) = &c.summary { (s) br; }
                                    span class="text-text-dim" { (c.keywords.join(", ")) }
                                }
                            }
                        }
                    }
                }
            }
            @if !hop.requested.is_empty() {
                p class="mb-1" { "It asked for:" }
                ul class="mt-0" {
                    @for r in &hop.requested {
                        li {
                            (r.wiki_id) "/" (page_or_none(r.page.as_deref()))
                            @if r.opened {
                                span class="text-text-dim" { " — opened" }
                            } @else {
                                span class="text-amber" {
                                    " — not opened"
                                    @if let Some(reason) = &r.reason { ": " (refusal_in_words(reason)) }
                                }
                            }
                        }
                    }
                }
            }
            @for o in &hop.opened {
                div class="mt-2" {
                    p class="mb-1" {
                        strong { (o.wiki_id) "/" (o.page) }
                        span class="text-text-dim" {
                            " · " (o.chars) " characters read · " (o.discovered)
                            " further page(s) it could lead to"
                        }
                    }
                    pre class="whitespace-pre-wrap text-xs" { (o.excerpt) }
                }
            }
        }
    }
}

/// The reconciliation stage: what it was shown, what it answered, and which
/// of the changes it asked for the engine did not make.
///
/// The only call in a turn that can retire a stored fact, and the only one
/// whose answer is worth reading raw — beside it, the refusals, because a
/// change the engine threw out reads in the verdict exactly like one it made.
fn render_reconcile(trace: &RecallTrace) -> Markup {
    if trace.reconcile_candidates.is_empty() && trace.reconcile_verdict.is_none() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Weighed against what was already there" }
            p.muted {
                "After the walk, the turn asks one question about the facts it just "
                "read: does this message retire, replace, re-date or re-share any of "
                "them? These were the facts it was shown, and this is what it said."
            }
            @if trace.reconcile_candidates.is_empty() {
                p.muted { "No fact was put to it." }
            } @else {
                table class="config-table" {
                    thead { tr { th { "Fact" } th { "Text" } } }
                    tbody {
                        @for c in &trace.reconcile_candidates {
                            tr {
                                td { code { (c.fact_id) } }
                                td { (c.text) }
                            }
                        }
                    }
                }
            }
            @if let Some(verdict) = &trace.reconcile_verdict {
                pre class="whitespace-pre-wrap text-xs mt-2" { (verdict) }
            } @else {
                p.muted { "It gave no answer: the model was unreachable, or no call was made." }
            }
        }
        @if !trace.refused_changes.is_empty() {
            section class="term-panel mt-4 p-4" {
                h2 class="mt-0" { "Changes it asked for and did not get" }
                p.muted {
                    "Replacing a stored fact and closing one both take it out of what "
                    "the memory answers with, so every one is checked before it is "
                    "carried out. These did not pass, and the stored fact is exactly "
                    "as it was."
                }
                table class="config-table" {
                    thead {
                        tr {
                            th { "Asked for" }
                            th { "The fact" }
                            th { "Replaced by" }
                            th { "Over" }
                            th { "Why not" }
                        }
                    }
                    tbody {
                        @for r in &trace.refused_changes {
                            tr {
                                td { (r.verb) }
                                td { code { (r.target) } }
                                td { code { (r.successor) } }
                                td { (r.slot) }
                                td { code { (r.reason) } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_injected(trace: &RecallTrace, navigate: bool) -> Markup {
    html! {
        @if let Some(block) = &trace.injected_block {
            section class="term-panel mt-4 p-4" {
                @if navigate {
                    h2 class="mt-0" { "Answered to the caller" }
                    p.muted { "A deep search answers with fragments, as data, not with a block of memory." }
                } @else {
                    h2 class="mt-0" { "Handed to the consumer" }
                }
                pre class="whitespace-pre-wrap" { (block) }
            }
        }
        @if let Some(rules) = &trace.rules_block {
            section class="term-panel mt-4 p-4" {
                h2 class="mt-0" { "Standing rules handed over" }
                pre class="whitespace-pre-wrap" { (rules) }
            }
        }
    }
}
