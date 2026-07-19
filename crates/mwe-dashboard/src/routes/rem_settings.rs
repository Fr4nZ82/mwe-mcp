// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editor for `<workdir>/mwe-mcp.config.yaml > rem.policy` —
//! the operator REM-settings panel (the dream cycle's behaviour knobs).
//!
//! Two routes, both behind [`AdminUser`], mirroring the
//! [recall-settings editor](super::recall_settings):
//!
//! - GET  `/admin/rem-settings` — render the knob table with the
//!   current overrides (empty input = Rust default, shown as the
//!   placeholder). Overrides are read from the on-disk YAML — the
//!   shared runtime handle holds the *resolved* policy, the YAML is
//!   where the operator's `Option`-shaped overrides live.
//! - POST `/admin/rem-settings` — atomic save of the YAML config
//!   (backup `.bak` first, serialize, atomic-write) followed by an
//!   in-place swap of the shared [`RemPolicy`] handle — the interval
//!   scheduler snapshots it at **each** cycle start and the Dream
//!   console at each trigger, so the next dream honours the new
//!   values, no restart needed.
//!
//! Only **resources** are configured here (per-cycle caps, mass bars,
//! the briefing grace): semantic judgment — what to merge, promote, or
//! rewrite — stays with the LLM sub-jobs, never in a knob. The dream
//! *cadence* (light/full intervals) stays in the YAML `rem.schedule:`
//! section; the sub-jobs' model tiers are the slots in the
//! [LLM config editor](super::llm_config).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, RemPolicyConfig};
use mwe_core::rem::RemPolicy;
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/rem-settings`. Mounted inside the
/// authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/rem-settings", get(page).post(save))
}

/// One knob row: the YAML/form field name, a short label, the default
/// rendered as the input placeholder, and the help line.
struct Knob {
    field: &'static str,
    label: &'static str,
    default: String,
    help: String,
}

/// The knob roster — every [`RemPolicyConfig`] field — derived from the
/// live Rust defaults so the panel never hardcodes a stale number.
fn knobs() -> Vec<Knob> {
    let def = RemPolicy::default();
    vec![
        Knob {
            field: "auto_promote_min_page_facts",
            label: "Auto-promote — min page mass (facts)",
            default: def.auto_promote_min_page_facts.to_string(),
            help: "Active facts a page must hold before the line→page split pass shows it \
                   to the LLM (a resource pre-filter, not a semantic gate)."
                .to_owned(),
        },
        Knob {
            field: "auto_promote_subwiki_min_page_facts",
            label: "Auto-promote — sub-wiki min page mass",
            default: def.auto_promote_subwiki_min_page_facts.to_string(),
            help: "Page mass before a topic page becomes a page→sub-wiki emergence candidate."
                .to_owned(),
        },
        Knob {
            field: "auto_promote_cap",
            label: "Auto-promote — changes per cycle",
            default: def.auto_promote_cap.to_string(),
            help: "Structural changes the auto-promote sub-job may apply per cycle \
                   (both rungs share it)."
                .to_owned(),
        },
        Knob {
            field: "page_merge_cap",
            label: "Page merge — confirmations per cycle",
            default: def.page_merge_cap.to_string(),
            help: "Candidate pairs the page-merge sub-job sends to the LLM confirmer per \
                   cycle. 0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "completion_sweep_cap",
            label: "Completion sweep — evidence facts per cycle",
            default: def.completion_sweep_cap.to_string(),
            help: "Evidence facts the completion sweep sends to the LLM per cycle. \
                   0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "contradiction_sweep_cap",
            label: "Contradiction sweep — seeds per cycle",
            default: def.contradiction_sweep_cap.to_string(),
            help: "Freshly contradicted seeds the cluster sweep sends to the LLM per cycle. \
                   0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "date_normalize_cap",
            label: "Date normalizer — facts per cycle",
            default: def.date_normalize_cap.to_string(),
            help: "Lexically flagged facts the date normalizer sends to the LLM per cycle, \
                   oldest first. 0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "provenance_hygiene_cap",
            label: "Provenance hygiene — repairs per cycle",
            default: def.provenance_hygiene_cap.to_string(),
            help: "Trailing source-pointer facts repaired per cycle (deterministic — \
                   embedder spend only). 0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "briefing_processor_grace_secs",
            label: "Briefing processor — grace (seconds)",
            default: def.briefing_processor_grace.num_seconds().to_string(),
            help: "How long a fresh dashboard comment is left alone before the cycle \
                   interprets it (the operator might still be editing). The synchronous \
                   dashboard Submit bypasses the grace."
                .to_owned(),
        },
        Knob {
            field: "husk_gc_cap",
            label: "Husk-page GC — removals per cycle",
            default: def.husk_gc_cap.to_string(),
            help: "Plan-absent husk page files (every fact tombstoned or superseded past \
                   the revert window) removed per full cycle. 0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "recall_repair_cap",
            label: "Recall repair — misses per cycle",
            default: def.recall_repair_cap.to_string(),
            help: "Pending recall misses judged per cycle (each costs a proposal call plus \
                   a gold-set gate replay on a scratch snapshot). 0 disables the sub-job."
                .to_owned(),
        },
        Knob {
            field: "recall_tuning_recurrence",
            label: "Recall repair — recurrence for the operator notice",
            default: def.recall_tuning_recurrence.to_string(),
            help: "Miss count on the same fact at which an unrepaired miss queues the \
                   recall-tuning operator notice (never auto-applied)."
                .to_owned(),
        },
    ]
}

/// Current override value for `field`, rendered into the input. Empty
/// string = no override (the placeholder shows the default).
fn override_value(cfg: &RemPolicyConfig, field: &str) -> String {
    fn s<T: ToString>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    match field {
        "auto_promote_min_page_facts" => s(cfg.auto_promote_min_page_facts),
        "auto_promote_subwiki_min_page_facts" => s(cfg.auto_promote_subwiki_min_page_facts),
        "auto_promote_cap" => s(cfg.auto_promote_cap),
        "page_merge_cap" => s(cfg.page_merge_cap),
        "completion_sweep_cap" => s(cfg.completion_sweep_cap),
        "contradiction_sweep_cap" => s(cfg.contradiction_sweep_cap),
        "date_normalize_cap" => s(cfg.date_normalize_cap),
        "provenance_hygiene_cap" => s(cfg.provenance_hygiene_cap),
        "briefing_processor_grace_secs" => s(cfg.briefing_processor_grace_secs),
        "husk_gc_cap" => s(cfg.husk_gc_cap),
        "recall_repair_cap" => s(cfg.recall_repair_cap),
        "recall_tuning_recurrence" => s(cfg.recall_tuning_recurrence),
        _ => String::new(),
    }
}

// ---------- GET ----------

struct Flash<'a> {
    kind: &'static str,
    msg: &'a str,
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    // The runtime handle holds the resolved policy; the Option-shaped
    // overrides this form edits live in the YAML, so read them there.
    let workdir = workdir_of(&state)?;
    let cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    let body = render(admin.session(), &cfg.rem.policy, None);
    Ok(Html(body))
}

fn render(
    session: &crate::auth::SessionUser,
    cfg: &RemPolicyConfig,
    flash: Option<Flash<'_>>,
) -> String {
    let body: Markup = html! {
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        p.flash.flash-info {
            strong { "Changes go live on the next dream cycle." }
            " Saving rewrites " code { (CONFIG_FILENAME) } " atomically and "
            "hot-swaps the running policy — the interval scheduler reads it "
            "at each cycle start and the Dream console at each trigger, "
            "no restart needed."
        }

        h2 { "REM settings" }
        p.muted {
            "Behaviour knobs of the REM cycle (auto-promote mass bars, "
            "per-cycle sweep caps, the briefing-processor grace), backing "
            "the " code { "rem.policy:" } " section of " code { (CONFIG_FILENAME) }
            ". Leave a field empty to keep the built-in default (shown "
            "as the placeholder). Semantic judgment — what to merge, "
            "promote, or rewrite — stays with the LLM sub-jobs, not here."
        }

        form action="/dashboard/admin/rem-settings" method="post" {
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
            p { button type="submit" { "Save REM settings" } }
        }

        p.muted {
            "Related dials elsewhere: the " strong { "dream cadence" }
            " (light/full intervals) is the " code { "rem.schedule:" }
            " section of the YAML; the sub-jobs' " strong { "model tiers" }
            " are the slots in the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; the per-turn recall resources are the "
            a href="/dashboard/admin/recall-settings" { "recall settings" }
            "."
        }
    };
    layout::authenticated_page("REM settings", session, &body)
}

// ---------- POST ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_form(&form)?;

    // Preserve every non-REM-policy section of the existing Config by
    // re-loading from disk and replacing only the `rem.policy` field
    // (`rem.schedule` stays as the operator wrote it). Raw load — env
    // overrides are runtime-only and must never be baked into the file
    // by a save; Config::load_raw returns Default when the file is
    // missing — the first save materialises it.
    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.rem.policy = parsed.clone();

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
    // the two still boots with the new YAML. The handle carries the
    // *resolved* policy (defaults + the fresh overrides).
    state.replace_rem_policy(cfg.rem.resolved_policy());

    tracing::info!(
        admin = %admin.session().sender_id,
        path = %path.display(),
        "rem-settings: saved from dashboard (hot-reloaded)"
    );

    let body = render(
        admin.session(),
        &parsed,
        Some(Flash {
            kind: "success",
            msg: "REM settings saved and hot-reloaded — the next dream cycle uses them.",
        }),
    );
    Ok(Html(body).into_response())
}

/// Workdir root for the YAML path. The on-disk config only exists on
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

/// Decode the flat form into a fresh [`RemPolicyConfig`]. Empty string →
/// `None` (keep the Rust default); a malformed number is a hard 422
/// naming the field, so a typo never silently lands as a default.
fn parse_form(form: &HashMap<String, String>) -> Result<RemPolicyConfig> {
    Ok(RemPolicyConfig {
        auto_promote_min_page_facts: parse_usize(form, "auto_promote_min_page_facts")?,
        auto_promote_subwiki_min_page_facts: parse_usize(
            form,
            "auto_promote_subwiki_min_page_facts",
        )?,
        auto_promote_cap: parse_usize(form, "auto_promote_cap")?,
        page_merge_cap: parse_usize(form, "page_merge_cap")?,
        completion_sweep_cap: parse_usize(form, "completion_sweep_cap")?,
        contradiction_sweep_cap: parse_usize(form, "contradiction_sweep_cap")?,
        date_normalize_cap: parse_usize(form, "date_normalize_cap")?,
        provenance_hygiene_cap: parse_usize(form, "provenance_hygiene_cap")?,
        briefing_processor_grace_secs: parse_u64(form, "briefing_processor_grace_secs")?,
        husk_gc_cap: parse_usize(form, "husk_gc_cap")?,
        recall_repair_cap: parse_usize(form, "recall_repair_cap")?,
        recall_tuning_recurrence: parse_i64(form, "recall_tuning_recurrence")?,
    })
}

fn parse_usize(form: &HashMap<String, String>, field: &'static str) -> Result<Option<usize>> {
    parse_num(form, field)
}

fn parse_i64(form: &HashMap<String, String>, field: &'static str) -> Result<Option<i64>> {
    parse_num(form, field)
}

fn parse_u64(form: &HashMap<String, String>, field: &'static str) -> Result<Option<u64>> {
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
