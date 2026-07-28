// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only live diagnostics page (roadmap 19b).
//!
//! Surfaces the lockfile-free subset of `mwe-mcp doctor` against the
//! **running** server: DB / migration / WAL / blacklist counts, the
//! workdir permission audit, and per-slot LLM reachability. Because it
//! reads the live pool and the live LLM handles it takes no lockfile and
//! never contends with `serve` — the thing the CLI `doctor` cannot do
//! while the daemon is up.
//!
//! The boot-failure-triage checks the CLI `doctor` keeps (lockfile
//! acquisition, the `MWE_TOKEN_SECRET`-from-env probe, the JWT self-test)
//! are intentionally absent here — an in-server page cannot serve them.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::LlmFunction;
use mwe_core::diagnostics::{self, DbDiagnostics, SlotHealth, SlotStatus};
use mwe_core::llm::LlmBackend;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

/// Sub-router for `/admin/health`. Mounted inside the authenticated tree.
///
/// Two routes: the page shell (fast — DB/workdir diagnostics) and the
/// slow per-slot LLM reachability probe. Splitting them keeps the page
/// responsive: the shell paints immediately with a spinner where the
/// slots go, and `ui.js` fetches the probe and swaps the table in. A
/// slow or unreachable backend can no longer stall the whole page.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/health", get(page))
        .route("/admin/health/llm-slots", get(llm_slots))
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })?;

    let db = diagnostics::collect_db(&state.pool, &memory.workdir)
        .await
        .map_err(|e| DashboardError::Internal(format!("diagnostics: {e:#}")))?;

    Ok(Html(render(chrome, admin.session(), &db)))
}

/// Slow companion to [`page`]: probes each LLM slot (a network round-trip
/// per slot, which can hang on an unreachable backend). The Health page
/// paints without it and fetches it client-side, swapping a spinner for
/// the table — so the probe never blocks first paint.
///
/// `?fragment=1` returns the bare slots table for that fetch; a direct
/// navigation (the no-JS `<noscript>` fallback link) returns a full page.
async fn llm_slots(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })?;

    // Probe the *live* LLM config (the dashboard LLM-config editor can
    // hot-swap slots), building each backend through the running handles
    // so dashboard-set API keys / test fakes are honoured.
    let live_cfg = {
        let guard = memory
            .llm_config
            .read()
            .map_err(|_| DashboardError::Internal("llm_config rwlock poisoned".to_owned()))?;
        guard.clone()
    };
    let slots = diagnostics::probe_llm_slots(&live_cfg, |func: LlmFunction| {
        let backend: Arc<dyn LlmBackend> = memory.backend_for(func).map_err(anyhow::Error::new)?;
        Ok(backend)
    })
    .await;

    if params.contains_key("fragment") {
        // Bare table that ui.js swaps into the spinner placeholder.
        Ok(Html(slots_table(&slots).into_string()))
    } else {
        // No-JS fallback: the same table on a standalone page.
        let body = html! {
            p.muted {
                "Live LLM slot reachability for the running server. "
                a href="/dashboard/admin/health" { "Back to Health" } "."
            }
            (slots_section(&slots))
        };
        Ok(Html(layout::authenticated_page(
            chrome,
            "LLM slots",
            admin.session(),
            &body,
        )))
    }
}

fn render(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    db: &DbDiagnostics,
) -> String {
    let body = html! {
        h2 { "Health" }
        p.muted {
            "Live diagnostics for the running server — the lockfile-free "
            "subset of " code { "mwe-mcp doctor" } ", read against the live "
            "database and LLM handles. Reload to re-run."
        }
        (db_body(db))
        (llm_slots_placeholder())
        p.muted {
            "For boot-failure triage (the server won't start), the offline "
            code { "mwe-mcp doctor" } " CLI also checks the workdir lockfile, "
            "the " code { "MWE_TOKEN_SECRET" } " env var, and a JWT self-test."
        }
    };
    layout::authenticated_page(chrome, "Health", session, &body)
}

/// The fast diagnostics tables (engine DB + workdir permissions) — pulled
/// out of [`render`] so it can be unit tested without constructing a
/// session. The slow LLM-slot probe lives in its own section
/// ([`llm_slots_placeholder`] / [`slots_section`]).
fn db_body(db: &DbDiagnostics) -> Markup {
    html! {
        h3 { "Engine database" }
        table.config-table {
            tbody {
                tr { td { "Application tables" } td { (db.app_tables) } }
                tr { td { "Migrations applied" } td { (db.applied_migrations) } }
                tr {
                    td { "Stale proposal ops" }
                    td { (db.stale_proposal_ops) }
                    td.muted { "awaiting WAL recovery" }
                }
                tr {
                    td { "Stale REM ops" }
                    td { (db.stale_rem_ops) }
                    td.muted { "awaiting WAL recovery" }
                }
                tr { td { "Token blacklist" } td { (db.token_blacklist_entries) } td.muted { "revoked tokens" } }
            }
        }

        h3 { "Workdir permissions" }
        @if db.perm_findings.is_empty() {
            p.flash.flash-info { "Owner-only — no group/world access. The per-reader ACL holds." }
        } @else {
            p.flash.flash-error {
                (db.perm_findings.len())
                " path(s) reachable by other principals — the per-reader ACL is bypassable "
                "(the cleartext bytes are readable off-server)."
            }
            table.config-table {
                tbody {
                    @for f in &db.perm_findings {
                        tr {
                            td { (f.severity.tag()) }
                            td { code { (f.mode_string()) } }
                            td { code { (f.path.display().to_string()) } }
                        }
                    }
                }
            }
        }
    }
}

/// The "LLM slots" section as rendered on first paint: the heading plus a
/// spinner that `ui.js` replaces with the probed table fetched from
/// `/admin/health/llm-slots?fragment=1`. The `<noscript>` link is the
/// honest fallback for a JS-less visitor.
fn llm_slots_placeholder() -> Markup {
    html! {
        h3 { "LLM slots" }
        div id="llm-slots" {
            p.muted {
                span.spinner {}
                " Probing LLM slot reachability…"
            }
            noscript {
                p.muted {
                    a href="/dashboard/admin/health/llm-slots" {
                        "Load LLM slot diagnostics"
                    }
                }
            }
        }
    }
}

/// Heading + table for the slow LLM-slot probe — used by the no-JS
/// full-page fallback. The JS path fetches only [`slots_table`].
fn slots_section(slots: &[SlotHealth]) -> Markup {
    html! {
        h3 { "LLM slots" }
        (slots_table(slots))
    }
}

/// Just the slots table — the fragment ui.js swaps into the placeholder.
fn slots_table(slots: &[SlotHealth]) -> Markup {
    html! {
        table.config-table {
            tbody {
                @for s in slots {
                    tr {
                        td { code { (s.slot) } }
                        (slot_status_cells(&s.status))
                    }
                }
            }
        }
    }
}

/// The status + detail cells for one LLM slot row.
fn slot_status_cells(status: &SlotStatus) -> Markup {
    match status {
        SlotStatus::Reachable { backend, model } => html! {
            td { "reachable" }
            td.muted { code { (backend) } " · " code { (model) } }
        },
        SlotStatus::Unconfigured => html! {
            td.muted { "unconfigured" }
            td.muted { "feature off" }
        },
        SlotStatus::LoginPending => html! {
            td { "login pending" }
            td.muted { "log in from the LLM config page" }
        },
        SlotStatus::Failed(detail) => html! {
            td { "FAILED" }
            td { (detail) }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_db() -> DbDiagnostics {
        DbDiagnostics {
            app_tables: 42,
            applied_migrations: 41,
            stale_proposal_ops: 0,
            stale_rem_ops: 0,
            token_blacklist_entries: 3,
            perm_findings: Vec::new(),
        }
    }

    #[test]
    fn db_body_renders_counts_and_perms_banner() {
        let out = db_body(&sample_db()).into_string();

        assert!(out.contains("Application tables"));
        assert!(out.contains("42"));
        assert!(
            out.contains("Owner-only"),
            "empty findings render the healthy banner"
        );
        // The slow probe is no longer part of this fast section.
        assert!(
            !out.contains("LLM slots"),
            "slot probe is rendered separately, fetched client-side"
        );
    }

    #[test]
    fn slots_table_renders_each_status() {
        let slots = vec![
            SlotHealth {
                slot: "ingest",
                status: SlotStatus::Reachable {
                    backend: "ollama".to_owned(),
                    model: "qwen3.5:9b-q8_0".to_owned(),
                },
            },
            SlotHealth {
                slot: "navigator",
                status: SlotStatus::Unconfigured,
            },
            SlotHealth {
                slot: "cronista",
                status: SlotStatus::Failed("connection refused".to_owned()),
            },
        ];
        let out = slots_table(&slots).into_string();

        assert!(out.contains("ingest") && out.contains("qwen3.5:9b-q8_0"));
        assert!(out.contains("unconfigured"));
        assert!(out.contains("FAILED") && out.contains("connection refused"));
    }

    #[test]
    fn placeholder_carries_the_spinner_and_fetch_hook() {
        let out = llm_slots_placeholder().into_string();
        // ui.js keys off this id; the spinner gives the loading feedback;
        // the noscript link is the JS-less fallback.
        assert!(out.contains(r#"id="llm-slots""#));
        assert!(out.contains("spinner"));
        assert!(out.contains("/dashboard/admin/health/llm-slots"));
    }
}
