// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin **Dream** console + run history.
//!
//! `GET /dashboard/dream` is a full page: the three on-demand dream triggers at
//! the top, the persisted run history (newest first) below it. Each history row
//! opens a modal with that run's full log, fetched from
//! `GET /dashboard/dream/runs/:id`.
//!
//! The three triggers delegate to the composition entry points in
//! [`mwe_core::dream`](../../../mwe-core/src/dream.rs), all admin-gated and
//! serialized on the per-state [`rem_gate`]:
//!
//! - `POST /dashboard/dream/light`   → [`mwe_core::dream::run_light`]
//! - `POST /dashboard/dream/compile` → [`mwe_core::dream::run_compile`]
//! - `POST /dashboard/dream/full`    → [`mwe_core::dream::run_full`]
//!
//! Every finished run — manual here, and the scheduler's nightly / interval runs
//! over in `rem_scheduler` — is recorded in the
//! [`mwe_core::dream_journal`] journal (`dream_runs`), so the history survives a
//! restart and finally gives the admin a window onto the automatic runs.
//!
//! [`rem_gate`]: crate::state::DashboardState::rem_gate

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::config::LlmFunction;
use mwe_core::dream_journal::{self, DreamKind, DreamRun, DreamTrigger};
use mwe_core::dream_light::LightPolicy;
use mwe_core::llm::LlmBackend;
use mwe_core::rem::RemLlms;
use sqlx::SqlitePool;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::{DashboardState, DreamLast, MemoryHandles};
use crate::ui::layout;

/// Routes for the admin Dream console. Merged into the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/dream", get(index))
        .route("/dream/status", get(status))
        .route("/dream/runs/:id", get(run_log))
        .route("/dream/light", post(run_light))
        .route("/dream/compile", post(run_compile))
        .route("/dream/full", post(run_full))
}

/// `GET /dashboard/dream` — the console page: the three trigger forms, then the
/// persisted run history (newest first). Admin-only.
async fn index(State(state): State<DashboardState>, user: SessionUser) -> Result<Html<String>> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let runs = dream_journal::recent_runs(&state.pool, dream_journal::MAX_HISTORY)
        .await
        .map_err(|e| DashboardError::Internal(format!("dream_journal::recent_runs: {e}")))?;
    Ok(Html(layout::authenticated_page(
        "Dream",
        &user,
        &render_index_body(&runs),
    )))
}

async fn run_light(
    State(state): State<DashboardState>,
    user: SessionUser,
    headers: HeaderMap,
) -> Result<Response> {
    dispatch(state, user, &headers, DreamKind::Light).await
}

async fn run_compile(
    State(state): State<DashboardState>,
    user: SessionUser,
    headers: HeaderMap,
) -> Result<Response> {
    dispatch(state, user, &headers, DreamKind::Compile).await
}

async fn run_full(
    State(state): State<DashboardState>,
    user: SessionUser,
    headers: HeaderMap,
) -> Result<Response> {
    dispatch(state, user, &headers, DreamKind::Full).await
}

/// `GET /dashboard/dream/status` — JSON the topnav indicator polls while a
/// background dream runs. Admin-only, like the triggers.
async fn status(State(state): State<DashboardState>, user: SessionUser) -> Result<Response> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let snapshot = state
        .dream_status
        .lock()
        .expect("dream_status mutex poisoned")
        .clone();
    Ok(axum::Json(snapshot).into_response())
}

/// `GET /dashboard/dream/runs/:id` — the HTML fragment the per-row log modal
/// injects: a small header (kind / trigger / status / timestamps) plus the full
/// report dump in a `<pre>`. Admin-only.
async fn run_log(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<i64>,
) -> Result<Html<String>> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let run = dream_journal::get_run(&state.pool, id)
        .await
        .map_err(|e| DashboardError::Internal(format!("dream_journal::get_run: {e}")))?
        .ok_or(DashboardError::NotFound)?;
    Ok(Html(render_run_log(&run).into_string()))
}

/// JSON ack returned by the `fetch` (background) path of a dream trigger.
#[derive(serde::Serialize)]
struct StartAck {
    /// `true` when this call spawned the dream; `false` when one was already
    /// running (see `busy`).
    started: bool,
    /// `true` when a dream was already running, so this trigger was a no-op
    /// (the REM gate was held). The indicator still polls to the shared end.
    busy: bool,
    /// Human label of the dream (`"Compile"`, `"Full REM"`, `"Light"`).
    kind: &'static str,
}

/// Branch a dream trigger on the `Accept` header: a `fetch` (JSON) kicks the
/// dream off on a background task and gets an immediate ack (the topnav
/// indicator then polls [`status`], and the page reloads to reveal the new
/// history row); a plain form submit (HTML, no-JS) keeps the synchronous
/// behaviour and renders the full report page, so the surface degrades cleanly.
async fn dispatch(
    state: DashboardState,
    user: SessionUser,
    headers: &HeaderMap,
    kind: DreamKind,
) -> Result<Response> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    if wants_json(headers) {
        start_async(&state, kind)
    } else {
        run_dream(&state, &user, kind)
            .await
            .map(IntoResponse::into_response)
    }
}

/// `true` when the caller asked for JSON. Only our `fetch` sets
/// `Accept: application/json`; a browser navigation never lists it literally,
/// so this cleanly separates the background path from the no-JS fallback.
fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"))
}

/// Kick a dream off on a background task and return an immediate JSON ack.
/// Holds [`rem_gate`](DashboardState::rem_gate) for the whole run via an owned
/// guard moved into the task; a second trigger while one runs gets
/// `{started:false, busy:true}` (the gate is taken).
fn start_async(state: &DashboardState, kind: DreamKind) -> Result<Response> {
    // Same precondition the synchronous path checks: without live handles a
    // dream cannot run at all.
    if state.memory.is_none() {
        return Err(DashboardError::Internal(
            "memory handles not wired — start with `mwe-mcp serve`, not the identity-only build"
                .into(),
        ));
    }
    let label = kind.label();
    let Ok(guard) = state.rem_gate.clone().try_lock_owned() else {
        return Ok(axum::Json(StartAck {
            started: false,
            busy: true,
            kind: label,
        })
        .into_response());
    };
    state
        .dream_status
        .lock()
        .expect("dream_status mutex poisoned")
        .running = Some(label.to_owned());

    let bg = state.clone();
    tokio::spawn(async move {
        // The owned guard travels into the task and drops when it ends,
        // releasing the gate only after the dream has fully finished.
        let _guard = guard;
        let started = now_rfc3339();
        let result = run_dream_core(&bg, kind).await;
        let finished = now_rfc3339();
        let (ok, failed, degraded, summary, log_text) = match result {
            Ok(out) => (
                true,
                out.pages_failed,
                out.pages_degraded,
                out.summary,
                out.log_text,
            ),
            // On failure the error message doubles as the row summary and the
            // modal log — there is no structured outcome to dump.
            Err(error) => (false, 0, 0, error.clone(), error),
        };
        // Update the in-memory pill outcome under the std mutex, then drop the
        // guard before the async journal write (a std guard cannot cross await).
        {
            let mut s = bg.dream_status.lock().expect("dream_status mutex poisoned");
            s.running = None;
            s.last = Some(DreamLast {
                kind: label.to_owned(),
                ok,
                summary: summary.clone(),
                finished_at: finished.clone(),
            });
        }
        journal_manual(
            &bg.pool,
            kind,
            ok,
            (failed, degraded),
            &summary,
            &log_text,
            &started,
            &finished,
        )
        .await;
    });

    Ok(axum::Json(StartAck {
        started: true,
        busy: false,
        kind: label,
    })
    .into_response())
}

/// The structured outcome of one dream, ready for the journal: the one-line
/// summary for the table row, the full debug dump for the modal log, and the
/// per-page compile outcome counts (`dream::journal_counts`) that mark a
/// completed-but-not-clean run.
struct DreamCoreOutcome {
    summary: String,
    log_text: String,
    pages_failed: i64,
    pages_degraded: i64,
}

/// The dream work itself, returning the summary + log on success (or an error
/// message) for the background path. The synchronous [`run_dream`] keeps its
/// richer report-page rendering; this is the headless variant the spawned task
/// drives.
async fn run_dream_core(
    state: &DashboardState,
    kind: DreamKind,
) -> std::result::Result<DreamCoreOutcome, String> {
    let memory = state
        .memory
        .as_ref()
        .ok_or_else(|| "memory handles not wired".to_owned())?;
    match kind {
        DreamKind::Light => {
            let backends = DreamBackends::resolve(memory).ok();
            let bag = backends.as_ref().map(DreamBackends::bag);
            let out = mwe_core::dream::run_light(
                &state.pool,
                &memory.tree,
                Arc::clone(&memory.embedder),
                bag.as_ref(),
                &LightPolicy::default(),
            )
            .await
            .map_err(|e| format!("dream::run_light: {e}"))?;
            let (pages_failed, pages_degraded) =
                mwe_core::dream::journal_counts(out.compile.as_ref());
            Ok(DreamCoreOutcome {
                summary: mwe_core::dream::summarize_light(&out),
                log_text: format!("{out:#?}"),
                pages_failed,
                pages_degraded,
            })
        },
        DreamKind::Compile => {
            let backends = DreamBackends::resolve(memory).map_err(|e| e.to_string())?;
            let now = now_rfc3339();
            let report = mwe_core::dream::run_compile(
                &state.pool,
                &memory.tree,
                &backends.bag(),
                mwe_core::dream::Cadence::Full,
                &now,
            )
            .await
            .map_err(|e| format!("dream::run_compile: {e}"))?;
            let (pages_failed, pages_degraded) = mwe_core::dream::journal_counts(Some(&report));
            Ok(DreamCoreOutcome {
                summary: mwe_core::dream::summarize_compile(&report),
                log_text: format!("{report:#?}"),
                pages_failed,
                pages_degraded,
            })
        },
        DreamKind::Full => {
            let backends = DreamBackends::resolve(memory).map_err(|e| e.to_string())?;
            // Snapshot the shared policy handle at trigger time so a
            // REM-settings save mid-run never mutates a running cycle.
            let policy = state.rem_policy_snapshot();
            let out = mwe_core::dream::run_full(
                &state.pool,
                &memory.tree,
                Arc::clone(&memory.embedder),
                &backends.bag(),
                &policy,
            )
            .await
            .map_err(|e| format!("dream::run_full: {e}"))?;
            let (pages_failed, pages_degraded) =
                mwe_core::dream::journal_counts(Some(&out.compile));
            Ok(DreamCoreOutcome {
                summary: mwe_core::dream::summarize_full(&out),
                log_text: format!("{out:#?}"),
                pages_failed,
                pages_degraded,
            })
        },
    }
}

/// Record a finished **manual** dream run in the journal (best-effort: a write
/// failure is logged, never surfaced — the dream itself already stands).
/// `counts` is the compile's `(pages_failed, pages_degraded)` pair
/// (`dream::journal_counts`; `(0, 0)` when the run errored).
async fn journal_manual(
    pool: &SqlitePool,
    kind: DreamKind,
    ok: bool,
    counts: (i64, i64),
    summary: &str,
    log_text: &str,
    started_at: &str,
    finished_at: &str,
) {
    if let Err(e) = dream_journal::record_run(
        pool,
        kind,
        DreamTrigger::Manual,
        ok,
        counts.0,
        counts.1,
        summary,
        log_text,
        started_at,
        finished_at,
    )
    .await
    {
        tracing::warn!(error = %e, "dashboard: failed to journal manual dream run");
    }
}

/// Shared driver for the no-JS synchronous path: gate on admin + the REM mutex,
/// resolve the LLM bag, run the requested dream, journal the success, render its
/// report page.
///
/// Only **successful** runs are journaled here: a failure propagates as a
/// `DashboardError` and surfaces directly to the operator on this page, so it
/// needs no journal row. (The background path journals failures too, because the
/// operator is not watching the request there.)
async fn run_dream(
    state: &DashboardState,
    user: &SessionUser,
    kind: DreamKind,
) -> Result<Html<String>> {
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles not wired — start with `mwe-mcp serve`, not the identity-only build"
                .into(),
        )
    })?;
    // Never run two dreams at once. `try_lock` keeps the click responsive: a
    // busy gate renders a friendly page instead of racing a cycle. (Sharing the
    // gate with the interval scheduler is the tracked `rem-manual-trigger-scheduler-gate`
    // follow-up; during interactive use the scheduler is effectively dormant.)
    let Ok(_guard) = state.rem_gate.try_lock() else {
        return Ok(Html(render_busy(user)));
    };
    let started = now_rfc3339();

    let body = match kind {
        DreamKind::Light => {
            // Light can promote without any LLM bag; the compile step inside is
            // skipped when the bag is absent or lacks a `cronista`.
            let backends = DreamBackends::resolve(memory).ok();
            let bag = backends.as_ref().map(DreamBackends::bag);
            let out = mwe_core::dream::run_light(
                &state.pool,
                &memory.tree,
                Arc::clone(&memory.embedder),
                bag.as_ref(),
                &LightPolicy::default(),
            )
            .await
            .map_err(|e| DashboardError::Internal(format!("dream::run_light: {e}")))?;
            let summary = mwe_core::dream::summarize_light(&out);
            journal_manual(
                &state.pool,
                DreamKind::Light,
                true,
                mwe_core::dream::journal_counts(out.compile.as_ref()),
                &summary,
                &format!("{out:#?}"),
                &started,
                &now_rfc3339(),
            )
            .await;
            render_report(user, "Dream · Light", &summary, &out)
        },
        DreamKind::Compile => {
            let backends = DreamBackends::resolve(memory)?;
            let now = now_rfc3339();
            // Operator-driven compile → full quality (Conciliatore runs);
            // the automatic light dream skips it.
            let report = mwe_core::dream::run_compile(
                &state.pool,
                &memory.tree,
                &backends.bag(),
                mwe_core::dream::Cadence::Full,
                &now,
            )
            .await
            .map_err(|e| DashboardError::Internal(format!("dream::run_compile: {e}")))?;
            let summary = mwe_core::dream::summarize_compile(&report);
            journal_manual(
                &state.pool,
                DreamKind::Compile,
                true,
                mwe_core::dream::journal_counts(Some(&report)),
                &summary,
                &format!("{report:#?}"),
                &started,
                &now_rfc3339(),
            )
            .await;
            render_report(user, "Dream · Compile", &summary, &report)
        },
        DreamKind::Full => {
            let backends = DreamBackends::resolve(memory)?;
            // Honour the operator's configured policy (resolved from
            // `rem.policy:` + any REM-settings save since boot), not
            // the defaults — snapshotted so the lock is not held
            // across the cycle.
            let policy = state.rem_policy_snapshot();
            let out = mwe_core::dream::run_full(
                &state.pool,
                &memory.tree,
                Arc::clone(&memory.embedder),
                &backends.bag(),
                &policy,
            )
            .await
            .map_err(|e| DashboardError::Internal(format!("dream::run_full: {e}")))?;
            let summary = mwe_core::dream::summarize_full(&out);
            journal_manual(
                &state.pool,
                DreamKind::Full,
                true,
                mwe_core::dream::journal_counts(Some(&out.compile)),
                &summary,
                &format!("{out:#?}"),
                &started,
                &now_rfc3339(),
            )
            .await;
            render_report(user, "Dream · Full REM", &summary, &out)
        },
    };
    Ok(Html(body))
}

/// Owned LLM handles for one dream, resolved from [`MemoryHandles::backend_for`]
/// and borrowed into a [`RemLlms`] for the call. The dashboard resolves
/// `Arc<dyn LlmBackend>` (vs the server scheduler's owned `Box`), so it keeps
/// its own holder rather than reusing the scheduler's `OwnedRemLlms`.
struct DreamBackends {
    hub: Arc<dyn LlmBackend>,
    revisor: Arc<dyn LlmBackend>,
    auto_promote: Option<Arc<dyn LlmBackend>>,
    apply: Option<Arc<dyn LlmBackend>>,
    cronista: Option<Arc<dyn LlmBackend>>,
    navigator: Option<Arc<dyn LlmBackend>>,
}

impl DreamBackends {
    /// Resolve the bag. `hub_writer` + `rem_dedup_semantic` are mandatory
    /// (the reorg and the Conciliatore need them); the rest are best-effort.
    fn resolve(memory: &MemoryHandles) -> Result<Self> {
        let hub = memory
            .backend_for(LlmFunction::HubWriter)
            .map_err(|e| DashboardError::Internal(format!("dream: hub_writer slot: {e}")))?;
        let revisor = memory
            .backend_for(LlmFunction::RemDedupSemantic)
            .map_err(|e| {
                DashboardError::Internal(format!("dream: rem_dedup_semantic slot: {e}"))
            })?;
        Ok(Self {
            hub,
            revisor,
            auto_promote: memory.backend_for(LlmFunction::RemPromotions).ok(),
            apply: memory.backend_for(LlmFunction::Ingest).ok(),
            cronista: memory.backend_for(LlmFunction::Cronista).ok(),
            navigator: memory.backend_for(LlmFunction::Navigator).ok(),
        })
    }

    fn bag(&self) -> RemLlms<'_> {
        RemLlms {
            hub_writer: &*self.hub,
            revisor: &*self.revisor,
            auto_promote: self.auto_promote.as_deref(),
            apply: self.apply.as_deref(),
            // The comment interpreter reuses the `ingest` slot, as elsewhere.
            comment_applier: self.apply.as_deref(),
            cronista: self.cronista.as_deref(),
            navigator: self.navigator.as_deref(),
        }
    }
}

/// `chrono::Utc::now()` as an RFC-3339 string — the `now` stamp the planner uses
/// and the start/finish stamps the journal stores.
fn now_rfc3339() -> String {
    dream_journal::now_rfc3339()
}

// ---------- rendering ----------

/// The console page body: intro, the three trigger forms, then the run history.
fn render_index_body(runs: &[DreamRun]) -> Markup {
    html! {
        p class="text-text-dim" style="font-size:.85rem" {
            "Run a dream by hand, or review the history below. Each trigger delegates to "
            code { "mwe_core::dream" }
            " — the same composition the nightly scheduler uses, so a manual trigger never "
            "diverges. The scheduler's own nightly / interval runs land in the same history."
        }
        (dream_forms())
        p.muted style="font-size:.78rem" {
            "Cycle behaviour knobs (per-cycle caps, mass bars, the briefing grace) are the "
            a href="/dashboard/admin/rem-settings" { "REM policy settings" }
            "."
        }
        (run_history(runs))
        (dream_log_modal())
    }
}

/// The three dream forms — shown at the top of the console page. Styled with
/// inline rules over the phosphor design tokens so it needs no new Tailwind
/// utilities (the compiled stylesheet is a build artefact).
fn dream_forms() -> Markup {
    html! {
        div style="display:flex;flex-direction:column;gap:.75rem;margin-top:.5rem" {
            (dream_form(
                "/dashboard/dream/light",
                "Light",
                "Promotes captures into facts and recompiles dirty pages. Frequent, cheap (promotion without an LLM; the Chronicler only touches the dirty ones)."
            ))
            (dream_form(
                "/dashboard/dream/compile",
                "Compile",
                "Narrative recompilation only (Cartographer → Conciliator → Chronicler → Hub → Reviewer) on dirty pages. For isolating the compiler."
            ))
            (dream_form(
                "/dashboard/dream/full",
                "Full REM",
                "Full cycle: reorg (dedup, auto-promote, archive) + apply parked comments + recompile the prose."
            ))
        }
    }
}

fn dream_form(action: &str, label: &str, desc: &str) -> Markup {
    html! {
        form class="dream-form" method="post" action=(action)
            style="display:flex;flex-direction:column;gap:.35rem;border:1px solid var(--border);border-radius:.5rem;padding:.85rem;background:var(--bg-1)" {
            button type="submit"
                style="align-self:flex-start;padding:.4rem .9rem;font-size:.8rem;font-weight:700;color:var(--p);background:var(--bg-2);border:1px solid var(--p);border-radius:.375rem;cursor:pointer" {
                (label)
            }
            p style="font-size:.78rem;color:var(--text-dim);margin:0;line-height:1.4" { (desc) }
        }
    }
}

/// The run-history table — newest first, styled like the facts browser. Each row
/// carries a "log" button that opens the per-row modal (wired in `ui.js`).
fn run_history(runs: &[DreamRun]) -> Markup {
    html! {
        h2 style="margin:1.4rem 0 .4rem;font-size:1rem;font-weight:700;color:var(--p)" {
            "History"
            span style="font-weight:400;color:var(--text-dim);font-size:.8rem" {
                " · last " (dream_journal::MAX_HISTORY) " runs"
            }
        }
        @if runs.is_empty() {
            p.muted { "No dream runs recorded yet." }
        } @else {
            table.facts-table.compact id="dream-history" {
                thead { tr {
                    th { "finished" }
                    th { "kind" }
                    th { "trigger" }
                    th { "status" }
                    th { "summary" }
                    th { "log" }
                } }
                tbody {
                    @for run in runs {
                        tr.inactive[!run.ok] {
                            td { (fmt_ts(&run.finished_at)) }
                            td { (run.kind.label()) }
                            td { (run.trigger.label()) }
                            td { (status_badge(run)) }
                            td { (run.summary) }
                            td {
                                button.dream-log-open type="button" data-run-id=(run.id)
                                    style="padding:.2rem .6rem;font-size:.72rem;font-weight:700;color:var(--p);background:var(--bg-2);border:1px solid var(--p);border-radius:.375rem;cursor:pointer" {
                                    "log"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The per-row log modal shell. Hidden by default; `ui.js` fetches the run's
/// fragment into `#dream-log-body` and flips `display` to `flex` on a row's
/// "log" click (and back on backdrop / close / Escape). Lives on the page (not
/// the global shell) since the history is the only surface that opens it.
fn dream_log_modal() -> Markup {
    html! {
        div id="dream-log-modal" role="dialog" aria-modal="true" aria-label="Dream run log"
            style="display:none;position:fixed;inset:0;z-index:60;align-items:center;justify-content:center;background:rgba(0,0,0,.62);padding:1rem" {
            div style="max-width:56rem;width:100%;max-height:85vh;overflow:auto;background:var(--bg-1);border:1px solid var(--border-hi);border-radius:.6rem;padding:1.4rem;box-shadow:0 12px 40px rgba(0,0,0,.5)" {
                div style="display:flex;align-items:center;justify-content:space-between;gap:1rem;margin-bottom:.6rem" {
                    h2 style="margin:0;font-size:1rem;font-weight:700;color:var(--p)" { "Dream run log" }
                    button id="dream-log-close" type="button" aria-label="Close"
                        style="width:1.8rem;height:1.8rem;display:inline-flex;align-items:center;justify-content:center;font-size:1.2rem;line-height:1;color:var(--p-dim);background:transparent;border:0;cursor:pointer" {
                        "×"
                    }
                }
                div id="dream-log-body" {}
            }
        }
    }
}

/// The fragment served at `GET /dashboard/dream/runs/:id` — injected into the
/// modal body. A small header line plus the full report dump in a `<pre>`.
fn render_run_log(run: &DreamRun) -> Markup {
    html! {
        p style="font-size:.85rem;margin:0 0 .3rem" {
            strong { (run.kind.label()) }
            " · " (run.trigger.label())
            " · " (status_badge(run))
        }
        p style="font-size:.78rem;color:var(--text-dim);margin:0 0 .3rem" {
            "started " (fmt_ts(&run.started_at)) " · finished " (fmt_ts(&run.finished_at))
        }
        p style="font-size:.85rem;margin:0 0 .6rem" { (run.summary) }
        pre style="overflow-x:auto;font-size:.72rem;line-height:1.35;background:var(--bg-2);border:1px solid var(--border);border-radius:.4rem;padding:.8rem" {
            (run.log_text)
        }
    }
}

/// The ok / warn / error status pill. A run that completed but left pages
/// failed or degraded ([`DreamRun::needs_attention`]) badges `warn` — the
/// journal outcome must not read as plain ok when the compile was not clean
/// (the summary string carries the counts).
fn status_badge(run: &DreamRun) -> Markup {
    let (text, bg) = if !run.ok {
        ("error", "var(--rose)")
    } else if run.needs_attention() {
        ("warn", "var(--amber)")
    } else {
        ("ok", "var(--p)")
    };
    html! {
        span style=(format!(
            "display:inline-flex;align-items:center;padding:.05rem .5rem;font-size:.72rem;font-weight:700;border-radius:9999px;color:var(--bg);background:{bg}"
        )) { (text) }
    }
}

/// Format an ISO-8601 timestamp for the table: drop fractional seconds and the
/// timezone suffix (`2026-06-30T16:32:45.947+00:00` → `2026-06-30 16:32:45`).
/// All stored stamps are UTC, so dropping the offset is lossless for display.
fn fmt_ts(raw: &str) -> String {
    let spaced = raw.replacen('T', " ", 1);
    match spaced.get(..19) {
        Some(s) if s.as_bytes().get(10) == Some(&b' ') => s.to_owned(),
        _ => spaced
            .trim_end_matches('Z')
            .trim_end_matches("+00:00")
            .trim_end()
            .to_owned(),
    }
}

/// Rendered when the REM gate is already held (a dream is running).
fn render_busy(user: &SessionUser) -> String {
    let body = html! {
        section class="flash-info" {
            p { "A dream is already running — try again shortly." }
        }
        p { a href="/dashboard/dream" { "← Dream" } " · " a href="/dashboard/home" { "Home" } }
    };
    layout::authenticated_page("Dream", user, &body)
}

/// Full report dump for the no-JS synchronous path — a `<pre>` under a one-line
/// summary, with a link back to the console.
fn render_report<R: std::fmt::Debug>(
    user: &SessionUser,
    title: &str,
    summary: &str,
    report: &R,
) -> String {
    let dump = format!("{report:#?}");
    let body = html! {
        p style="font-size:.9rem" { (summary) }
        pre style="overflow-x:auto;font-size:.72rem;line-height:1.35;background:var(--bg-1);border:1px solid var(--border);border-radius:.4rem;padding:.8rem" { (dump) }
        p {
            a href="/dashboard/dream" { "← Dream" }
            " · "
            a href="/dashboard/chat" { "Chat" }
            " · "
            a href="/dashboard/home" { "Home" }
        }
    };
    layout::authenticated_page(title, user, &body)
}
