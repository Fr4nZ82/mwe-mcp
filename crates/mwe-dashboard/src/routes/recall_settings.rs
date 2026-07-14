// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editor for `<workdir>/mwe-mcp.config.yaml > recall` —
//! the unified operator recall-settings panel.
//!
//! Two routes, both behind [`AdminUser`]:
//!
//! - GET  `/admin/recall-settings` — render the knob table with the
//!   current overrides (empty input = Rust default, shown as the
//!   placeholder).
//! - POST `/admin/recall-settings` — atomic save of the YAML config
//!   (backup `.bak` first, serialize, atomic-write) followed by an
//!   in-place swap of the shared [`RecallConfig`] handle — the next
//!   ingest turn on **either** transport (MCP dispatcher or dashboard
//!   chat) reads the new values, no restart needed.
//!
//! Only **resources** are configured here (slot sizes, hop depth, prose
//! budget, candidate caps, the due-soon horizon): semantic judgment —
//! which link to open, when to stop — lives in the operator-overridable
//! `navigator` prompt, never in a knob. The navigator's *model tier* is
//! the `navigator` LLM slot in the [LLM config editor](llm_config); the
//! dream cadence stays in the YAML `rem.schedule:` section, while the
//! dream *policy* has its own [REM settings panel](super::rem_settings).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, RecallConfig};
use mwe_core::ingest::IngestPolicy;
use mwe_core::recall::MULTI_HOP_HARD_LIMIT;
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/recall-settings`. Mounted inside the
/// authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/recall-settings", get(page).post(save))
}

/// One knob row: the YAML/form field name, a short label, the default
/// rendered as the input placeholder, and the help line.
struct Knob {
    field: &'static str,
    label: &'static str,
    default: String,
    help: String,
}

/// The knob roster, derived from the live Rust defaults so the panel
/// never hardcodes a stale number.
fn knobs() -> Vec<Knob> {
    let def = IngestPolicy::default();
    vec![
        Knob {
            field: "recall_top_k",
            label: "Flat slot — top-K hits",
            default: def.recall_top_k.to_string(),
            help: "Vector-recall hits fetched per turn (classifier context + RAG entry seeds)."
                .to_owned(),
        },
        Knob {
            field: "recall_fresh_top_k",
            label: "Fresh slot — top-K captures",
            default: def.recall_fresh_top_k.to_string(),
            help: "Un-promoted buffered captures surfaced per turn. 0 disables the slot."
                .to_owned(),
        },
        Knob {
            field: "max_hops",
            label: "Navigator — depth (hops)",
            default: def.nav.max_hops.to_string(),
            help: format!(
                "Navigator decisions per turn — the depth dial. \
                 Clamped to the hard cap of {MULTI_HOP_HARD_LIMIT}."
            ),
        },
        Knob {
            field: "pages_per_hop",
            label: "Navigator — pages per hop",
            default: def.nav.pages_per_hop.to_string(),
            help: "Pages the navigator may open per decision.".to_owned(),
        },
        Knob {
            field: "char_budget",
            label: "Navigator — prose budget (chars)",
            default: def.nav.char_budget.to_string(),
            help: "Total sender-projected prose a navigation may collect.".to_owned(),
        },
        Knob {
            field: "max_candidates",
            label: "Navigator — candidate window",
            default: def.nav.max_candidates.to_string(),
            help: "Candidate pages offered to the navigator per hop.".to_owned(),
        },
        Knob {
            field: "decision_max_tokens",
            label: "Navigator — decision max tokens",
            default: def.nav.decision_max_tokens.to_string(),
            help: "Token cap per navigator completion (cost guard, not a quality knob).".to_owned(),
        },
        Knob {
            field: "due_soon_top_k",
            label: "Due-soon slot — top-K facts",
            default: def.due_soon_top_k.to_string(),
            help: "Imminent-validity facts surfaced per turn. 0 disables the slot.".to_owned(),
        },
        Knob {
            field: "due_soon_horizon_hours",
            label: "Due-soon slot — horizon (hours)",
            default: def.due_soon_horizon_hours.to_string(),
            help: "Look-ahead window of the due-soon pull, hours from the turn's clock.".to_owned(),
        },
        Knob {
            field: "max_agent_identity_chars",
            label: "WHO YOU ARE — budget (chars)",
            default: def.max_agent_identity_chars.to_string(),
            help: "Recall-block agent-identity section: whole bullets fitted, oldest tail drops."
                .to_owned(),
        },
        Knob {
            field: "max_agent_history_chars",
            label: "History with user — budget (chars)",
            default: def.max_agent_history_chars.to_string(),
            help:
                "Recall-block per-user agent-history section: whole bullets fitted, newest first."
                    .to_owned(),
        },
    ]
}

/// Current override value for `field`, rendered into the input. Empty
/// string = no override (the placeholder shows the default).
fn override_value(cfg: &RecallConfig, field: &str) -> String {
    fn s<T: ToString>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    match field {
        "recall_top_k" => s(cfg.recall_top_k),
        "recall_fresh_top_k" => s(cfg.recall_fresh_top_k),
        "max_hops" => s(cfg.max_hops),
        "pages_per_hop" => s(cfg.pages_per_hop),
        "char_budget" => s(cfg.char_budget),
        "max_candidates" => s(cfg.max_candidates),
        "decision_max_tokens" => s(cfg.decision_max_tokens),
        "due_soon_top_k" => s(cfg.due_soon_top_k),
        "due_soon_horizon_hours" => s(cfg.due_soon_horizon_hours),
        "max_agent_identity_chars" => s(cfg.max_agent_identity_chars),
        "max_agent_history_chars" => s(cfg.max_agent_history_chars),
        _ => String::new(),
    }
}

// ---------- GET ----------

struct Flash<'a> {
    kind: &'static str,
    msg: &'a str,
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let body = render(admin.session(), &state.recall_snapshot(), None);
    Ok(Html(body))
}

fn render(
    session: &crate::auth::SessionUser,
    cfg: &RecallConfig,
    flash: Option<Flash<'_>>,
) -> String {
    let body: Markup = html! {
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        p.flash.flash-info {
            strong { "Changes go live on the next turn." }
            " Saving rewrites " code { (CONFIG_FILENAME) } " atomically and "
            "hot-swaps the running settings — both the MCP transport and "
            "the dashboard chat read them per turn, no restart needed."
        }

        h2 { "Recall settings" }
        p.muted {
            "Resource knobs of the per-turn recall block (flat slot, "
            "navigator funnel, due-soon slot), backing the "
            code { "recall:" } " section of " code { (CONFIG_FILENAME) }
            ". Leave a field empty to keep the built-in default (shown "
            "as the placeholder). Semantics — which link to open, when "
            "to stop — live in the " code { "navigator" } " prompt, not here."
        }

        form action="/dashboard/admin/recall-settings" method="post" {
            table.config-table {
                thead { tr {
                    th { "Setting" }
                    th { "Value" }
                    th { "Default" }
                    th { "What it does" }
                } }
                tbody {
                    @for k in knobs() {
                        tr {
                            td { label for=(k.field) { (k.label) } }
                            td {
                                input id=(k.field) name=(k.field) type="number" min="0"
                                    value=(override_value(cfg, k.field))
                                    placeholder=(k.default);
                            }
                            td { code { (k.default) } }
                            td.muted { (k.help) }
                        }
                    }
                }
            }
            p { button type="submit" { "Save recall settings" } }
        }

        p.muted {
            "Related dials elsewhere: the navigator's "
            strong { "model tier" } " is the " code { "navigator" }
            " slot in the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; the " strong { "dream cadence" } " (light/full intervals) "
            "is the " code { "rem.schedule:" } " section of the YAML; the "
            "dream " strong { "policy" } " (per-cycle caps and thresholds) "
            "is the "
            a href="/dashboard/admin/rem-settings" { "REM settings panel" }
            "."
        }
    };
    layout::authenticated_page("Recall settings", session, &body)
}

// ---------- POST ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let mut parsed = parse_form(&form)?;

    // Preserve every non-recall section of the existing Config by
    // re-loading from disk and replacing only the `recall` field.
    // Config::load returns Default when the file is missing — the
    // first save materialises it.
    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    // `ingest_timezone` is not a knob on this panel; carry the live value
    // forward so saving recall settings never wipes a configured zone.
    parsed.ingest_timezone = cfg.recall.ingest_timezone.clone();
    cfg.recall = parsed.clone();

    // Backup `.bak` of the live YAML (if any) before overwriting.
    let path = workdir.join(CONFIG_FILENAME);
    let backup = backup_path_for(&path);
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&backup, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }

    let yaml = serde_yaml::to_string(&cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;

    // Hot-reload: disk first, then the in-place swap — a crash between
    // the two still boots with the new YAML.
    state.replace_recall(parsed.clone());

    tracing::info!(
        admin = %admin.session().sender_id,
        path = %path.display(),
        "recall-settings: saved from dashboard (hot-reloaded)"
    );

    let body = render(
        admin.session(),
        &parsed,
        Some(Flash {
            kind: "success",
            msg: "Recall settings saved and hot-reloaded — the next turn uses them.",
        }),
    );
    Ok(Html(body).into_response())
}

/// Workdir root for the YAML path. The recall settings handle itself
/// lives on [`DashboardState`], but the on-disk config only exists on
/// the full `mwe-mcp serve` build, where the memory handles carry the
/// workdir.
fn workdir_of(state: &DashboardState) -> Result<PathBuf> {
    state
        .memory
        .as_ref()
        .map(|m| m.workdir.clone())
        .ok_or_else(|| {
            DashboardError::Internal(
                "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
            )
        })
}

fn backup_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Decode the flat form into a fresh [`RecallConfig`]. Empty string →
/// `None` (keep the Rust default); a malformed number is a hard 422
/// naming the field, so a typo never silently lands as a default.
fn parse_form(form: &HashMap<String, String>) -> Result<RecallConfig> {
    Ok(RecallConfig {
        recall_top_k: parse_usize(form, "recall_top_k")?,
        recall_fresh_top_k: parse_usize(form, "recall_fresh_top_k")?,
        max_hops: parse_usize(form, "max_hops")?,
        pages_per_hop: parse_usize(form, "pages_per_hop")?,
        char_budget: parse_usize(form, "char_budget")?,
        max_candidates: parse_usize(form, "max_candidates")?,
        decision_max_tokens: parse_u32(form, "decision_max_tokens")?,
        due_soon_top_k: parse_usize(form, "due_soon_top_k")?,
        due_soon_horizon_hours: parse_u32(form, "due_soon_horizon_hours")?,
        max_agent_identity_chars: parse_usize(form, "max_agent_identity_chars")?,
        max_agent_history_chars: parse_usize(form, "max_agent_history_chars")?,
        // Not a recall knob and not on this panel — the caller carries the
        // live value forward so a save here never wipes a configured zone.
        ingest_timezone: None,
    })
}

fn parse_usize(form: &HashMap<String, String>, field: &'static str) -> Result<Option<usize>> {
    parse_num(form, field)
}

fn parse_u32(form: &HashMap<String, String>, field: &'static str) -> Result<Option<u32>> {
    parse_num(form, field)
}

fn parse_num<T: std::str::FromStr>(
    form: &HashMap<String, String>,
    field: &'static str,
) -> Result<Option<T>> {
    let raw = form.get(field).map(|s| s.trim()).unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<T>().map(Some).map_err(|_| {
        DashboardError::Validation(format!("`{field}` must be a non-negative integer"))
    })
}
