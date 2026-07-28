// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin **Recall traces** — the last recalls, listed and replayed.
//!
//! Three routes, all admin-gated (a trace journals what a specific sender was
//! served, across wiki and ACL lines — operator telemetry, same tier as the
//! engine DB):
//!
//! - GET `/admin/recall-traces` — the journal (newest first): one row per
//!   recorded recall run, linking to its viewer.
//! - GET `/admin/recall-traces/:id` — the viewer page: the animated 3D replay
//!   of the route the recall took (WebGL, `recall-trace.js`) mounted above the
//!   full textual trace — which is also the no-JS / no-WebGL fallback.
//! - GET `/admin/recall-traces/:id/data` — the JSON payload the viewer fetches
//!   (`meta` + the decoded [`mwe_core::recall_trace::RecallTrace`]).
//!
//! Admin is the floor, not the whole gate: **by default every operator —
//! admins included — sees only the traces whose sender is themself**. A trace
//! carries the recalled fact bodies verbatim and is not re-projected per
//! reader, so listing the whole deployment unconditionally would hand one
//! user's memory to another. The reveal switch ([`crate::reveal`]) is what
//! lifts an admin to the whole journal — the same single control that governs
//! the facts table and the wiki pages, not a second policy. Scoping to self is
//! also what the page is usually opened for: to see why **my** last search
//! answered badly.
//!
//! The journal itself is written by the two recall producers (see
//! [`mwe_core::recall_trace`] and the
//! recall pipeline): the
//! ingest per-turn injection and the `wiki_navigate` tool. This surface only
//! reads it.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use mwe_core::recall_nav::HopTrace;
use mwe_core::recall_trace::{self, RecallTrace, TraceHit, TraceRow, TraceSource};

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

/// Routes for the admin Recall-traces surface. Merged into the
/// authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/recall-traces", get(index))
        .route("/admin/recall-traces/:id", get(viewer))
        .route("/admin/recall-traces/:id/data", get(data))
}

/// May `user` open the trace `row` was recorded for?
///
/// Own traces always; anyone else's only under the reveal switch (which is
/// itself admin-only, so `reveal` can never be true for a non-admin).
fn readable(row: &TraceRow, user: &SessionUser, reveal: bool) -> bool {
    reveal || row.sender_id == user.sender_id
}

/// `GET /dashboard/admin/recall-traces` — the journal, newest first.
///
/// Scoped to the reader's own recalls unless the reveal switch is on.
async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<Html<String>> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let reveal = crate::reveal::active(&user, &jar);
    let rows = recall_trace::recent_traces(&state.pool, recall_trace::TRACE_KEEP)
        .await
        .map_err(|e| DashboardError::Internal(format!("recall_trace::recent_traces: {e}")))?;
    // The journal is capped at `TRACE_KEEP` rows deployment-wide, so the
    // scoping filter is a cheap pass over a handful of rows and the policy
    // stays here, next to the reveal decision, instead of in the query.
    let rows: Vec<TraceRow> = rows
        .into_iter()
        .filter(|r| readable(r, &user, reveal))
        .collect();
    Ok(Html(layout::authenticated_page(
        "Recall traces",
        &user,
        &render_index_body(&rows, reveal),
    )))
}

/// `GET /dashboard/admin/recall-traces/:id` — the viewer page.
async fn viewer(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(id): AxumPath<i64>,
) -> Result<Html<String>> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_readable(&state, id, &user, reveal).await?;
    let trace = row
        .parse()
        .map_err(|e| DashboardError::Internal(format!("recall_trace payload: {e}")))?;
    Ok(Html(layout::authenticated_page(
        &format!("Recall trace #{id}"),
        &user,
        &render_viewer_body(&row, &trace, reveal),
    )))
}

/// `GET /dashboard/admin/recall-traces/:id/data` — the viewer's JSON feed.
async fn data(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(id): AxumPath<i64>,
) -> Result<Response> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let reveal = crate::reveal::active(&user, &jar);
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

fn render_index_body(rows: &[TraceRow], reveal: bool) -> Markup {
    html! {
        @if reveal { (crate::reveal::banner()) }
        p class="text-text-dim max-w-prose" {
            "The last " (recall_trace::TRACE_KEEP) " recalls, newest first — what each user "
            "action pulled out of memory and what was injected back into the consumer. "
            "Open a trace to replay the navigator's route."
            @if reveal {
                " Admin reveal is on, so this is every user's recall."
            } @else {
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
                        th { "Source" }
                        th { "Sender" }
                        th { "Turn" }
                        th { "Hits" }
                        th { "Hops" }
                        th { "Stop" }
                        th { "Injected" }
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
    let href = format!("/dashboard/admin/recall-traces/{}", row.id);
    html! {
        tr {
            td { (compact_stamp(&row.created_at)) }
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
                @if let Some(stop) = &t.nav_stop { (stop) } @else { span class="text-text-dim" { "flat only" } }
            } }
            td { @if let Some(t) = &trace {
                (t.injected_block.as_deref().map_or(0, str::len)) " ch"
            } }
            td { a href=(href) { "view" } }
        }
    }
}

fn hit_counts(t: &RecallTrace) -> String {
    let mut parts = vec![format!("{} flat", t.flat_hits.len())];
    if !t.fresh_hits.is_empty() {
        parts.push(format!("{} fresh", t.fresh_hits.len()));
    }
    if !t.due_soon.is_empty() {
        parts.push(format!("{} due", t.due_soon.len()));
    }
    parts.join(" · ")
}

fn source_badge(source: TraceSource) -> Markup {
    let label = match source {
        TraceSource::Ingest => "ingest",
        TraceSource::Navigate => "navigate",
    };
    html! { span class="badge" { (label) } }
}

/// `2026-07-03T21:04:05.123+00:00` → `2026-07-03 21:04:05` (fallback: as-is).
fn compact_stamp(iso: &str) -> String {
    match (iso.get(..10), iso.get(11..19)) {
        (Some(d), Some(t)) => format!("{d} {t}"),
        _ => iso.to_owned(),
    }
}

// ---------- Rendering: the viewer ----------

fn render_viewer_body(row: &TraceRow, trace: &RecallTrace, reveal: bool) -> Markup {
    html! {
        @if reveal { (crate::reveal::banner()) }
        // Stage mount for the 3D replay. recall-trace.js (an ES module — it
        // imports the vendored three.module.min.js relative to itself)
        // hydrates it from the /data endpoint; without JS/WebGL the stage
        // stays empty and the textual trace below is the whole surface.
        div id="trace-stage" class="trace-stage" data-trace-id=(row.id) {}
        script src="/dashboard/static/recall-trace.js" type="module" {}

        (render_meta(row, trace))
        (render_hits_section("Flat recall (similarity)", &trace.flat_hits, true))
        (render_hits_section("Fresh (not yet consolidated)", &trace.fresh_hits, false))
        (render_hits_section("Due soon", &trace.due_soon, false))
        (render_fan(trace))
        (render_hops(trace))
        (render_injected(trace))
    }
}

fn render_meta(row: &TraceRow, trace: &RecallTrace) -> Markup {
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { "Turn" }
            p class="whitespace-pre-wrap" { (trace.turn_text) }
            table class="config-table" {
                tbody {
                    tr { th { "When" } td { (compact_stamp(&row.created_at)) } }
                    tr { th { "Source" } td { (source_badge(row.source)) } }
                    tr { th { "Sender" } td { (row.sender_id) } }
                    @if let Some(consumer) = &trace.consumer {
                        tr { th { "Consumer" } td { (consumer) } }
                    }
                    @if let Some(intent) = &trace.intent {
                        tr { th { "Intent" } td { (intent) } }
                    }
                    tr { th { "Seeds" } td {
                        (trace.seed_mode)
                        @if !trace.topics.is_empty() { " · topics: " (trace.topics.join(", ")) }
                        @if !trace.owners.is_empty() { " · owners: " (trace.owners.join(", ")) }
                    } }
                    tr { th { "Navigation" } td {
                        @match &trace.nav_stop {
                            Some(stop) => {
                                (trace.hops.len()) " hop(s), stopped: " (stop) " · "
                                (trace.chars_collected) " / " (trace.char_budget) " chars"
                                @if trace.truncated { " (truncated)" }
                            },
                            None => { "not run (flat only)" },
                        }
                    } }
                    tr { th { "Took" } td { (trace.took_ms) " ms" } }
                }
            }
        }
    }
}

fn render_hits_section(title: &str, hits: &[TraceHit], show_region: bool) -> Markup {
    if hits.is_empty() {
        return html! {};
    }
    html! {
        section class="term-panel mt-4 p-4" {
            h2 class="mt-0" { (title) }
            table class="config-table" {
                thead {
                    tr {
                        th { "Score" }
                        th { "Wiki / page" }
                        @if show_region { th { "Region" } }
                        th { "Text" }
                    }
                }
                tbody {
                    @for h in hits {
                        tr {
                            td { (format!("{:.3}", h.score)) }
                            td { (h.wiki_id) br; span class="text-text-dim text-xs" { (h.source_path) } }
                            @if show_region {
                                td { @match (h.region_start, h.region_end) {
                                    (Some(s), Some(e)) => { (s) "–" (e) },
                                    _ => { "—" },
                                } }
                            }
                            td class="whitespace-pre-wrap" { (h.text) }
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
            h2 class="mt-0" { "Entry-point fan" }
            table class="config-table" {
                thead { tr { th { "Weight" } th { "Family" } th { "Wiki" } th { "Page" } } }
                tbody {
                    @for ep in &trace.entry_points {
                        tr {
                            td { (format!("{:.2}", ep.weight)) }
                            td { (ep.origin) }
                            td { (ep.wiki_id) }
                            td { (ep.page.as_deref().unwrap_or("(overview)")) }
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
            h2 class="mt-0" { "Hop " (i + 1) }
            @if let Some(note) = &hop.note {
                p { "Navigator: " em { (PreEscaped("\u{201c}")) (note) (PreEscaped("\u{201d}")) }
                    @if hop.done { " · done" } }
            } @else if hop.done { p { "Navigator: done" } }
            details {
                summary { (hop.candidates.len()) " candidate(s) offered" }
                table class="config-table" {
                    thead { tr { th { "Via" } th { "Wiki" } th { "Page" } th { "Card" } } }
                    tbody {
                        @for c in &hop.candidates {
                            tr {
                                td { (c.origin) }
                                td { (c.wiki_id) }
                                td { (c.page.as_deref().unwrap_or("(overview)")) }
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
                p class="mb-1" { "Opened:" }
                ul class="mt-0" {
                    @for r in &hop.requested {
                        li {
                            (r.wiki_id) "/" (r.page.as_deref().unwrap_or("(overview)"))
                            @if !r.opened { span class="text-amber" { " — discarded by vetting" } }
                        }
                    }
                }
            }
            @for o in &hop.opened {
                div class="mt-2" {
                    p class="mb-1" {
                        strong { (o.wiki_id) "/" (o.page) }
                        span class="text-text-dim" { " · " (o.chars) " chars · " (o.discovered) " new candidate(s) exposed" }
                    }
                    pre class="whitespace-pre-wrap text-xs" { (o.excerpt) }
                }
            }
        }
    }
}

fn render_injected(trace: &RecallTrace) -> Markup {
    html! {
        @if let Some(block) = &trace.injected_block {
            section class="term-panel mt-4 p-4" {
                h2 class="mt-0" { "Injected into the consumer" }
                pre class="whitespace-pre-wrap" { (block) }
            }
        }
        @if let Some(rules) = &trace.rules_block {
            section class="term-panel mt-4 p-4" {
                h2 class="mt-0" { "Rules field" }
                pre class="whitespace-pre-wrap" { (rules) }
            }
        }
    }
}
