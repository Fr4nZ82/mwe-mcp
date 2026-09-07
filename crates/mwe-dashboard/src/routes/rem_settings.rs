// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editor for `<workdir>/mwe-mcp.config.yaml > rem.policy` —
//! the operator REM-settings panel (the REM cycle's behaviour knobs — the
//! nightly run reads them, a light dream never does).
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
//! Only **resources** are configured here (per-cycle caps, the fact
//! counts a page must reach before it is split or founds a wiki, the
//! briefing grace): semantic judgment — what to merge, promote, or
//! rewrite — stays with the LLM sub-jobs, never in a knob. The dream
//! *cadence* (`rem.schedule:` — light/full intervals) is the Dream
//! cadence section of the Settings page ([`super::server_settings`]);
//! the sub-jobs' model tiers are the slots in the
//! [LLM config editor](super::llm_config).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
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
#[allow(
    clippy::too_many_lines,
    reason = "one literal per operator knob — splitting it hides the panel's shape"
)]
fn knobs() -> Vec<Knob> {
    let def = RemPolicy::default();
    vec![
        Knob {
            field: "auto_promote_min_page_facts",
            label: "Split a prose page — facts it must hold first",
            default: def.auto_promote_min_page_facts.to_string(),
            help: "Live facts a PROSE page must hold before the split pass even \
                   looks at it. A size floor that saves model calls, not a judgment \
                   that the page should be split."
                .to_owned(),
        },
        Knob {
            field: "auto_promote_min_page_facts_technical",
            label: "Split a technical-prose page — facts it must hold first",
            default: def.auto_promote_min_page_facts_technical.to_string(),
            help: "The same floor for a `prosa-tecnica` page — read point by point \
                   rather than as a thread, so it carries more before it needs \
                   splitting. A `lista` page is never split on size at all: it is \
                   consulted, and half a list is not an answer."
                .to_owned(),
        },
        Knob {
            field: "auto_promote_group_min_pages",
            label: "Found a new wiki — pages on one subject it takes",
            default: def.auto_promote_group_min_pages.to_string(),
            help: "Pages of one subject the regrouping pass must find, anywhere in the memory, \
                   to found a wiki. Birth only: filing into a wiki that exists has no floor."
                .to_owned(),
        },
        Knob {
            field: "auto_promote_cap",
            label: "Splitting and founding — changes per cycle",
            default: def.auto_promote_cap.to_string(),
            help: "Changes to the shape of the memory the promotion pass may make \
                   in one cycle. Splitting a page and founding a wiki share the \
                   allowance."
                .to_owned(),
        },
        Knob {
            field: "page_merge_cap",
            label: "Merging two pages — pairs checked per cycle",
            default: def.page_merge_cap.to_string(),
            help: "Pairs of pages that look like the same subject, sent to the model \
                   to confirm, per cycle. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "structure_review_cap",
            label: "Moving a page to another wiki — moves per cycle",
            default: def.structure_review_cap.to_string(),
            help: "Pages the structural review may move to a DIFFERENT wiki per cycle. It is \
                   the only pass that looks at the whole memory at once, and the only one \
                   that can move a page out of the wiki it was born in. Small on purpose: a \
                   move rewrites paths and retargets links. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "completion_sweep_cap",
            label: "Closing what has been overtaken — facts checked per cycle",
            default: def.completion_sweep_cap.to_string(),
            help: "Facts that look like evidence something older is finished, sent \
                   to the model per cycle. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "contradiction_sweep_cap",
            label: "Facts that contradict each other — starting points per cycle",
            default: def.contradiction_sweep_cap.to_string(),
            help: "Freshly contradicted facts the sweep starts from when it gathers \
                   a cluster for the model, per cycle. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "date_normalize_cap",
            label: "Putting dates in order — facts per cycle",
            default: def.date_normalize_cap.to_string(),
            help: "Facts whose wording looks like a date (\"last Tuesday\") sent to \
                   the model to be turned into a real one, oldest first, per cycle. \
                   0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "provenance_hygiene_cap",
            label: "Repairing where a fact came from — repairs per cycle",
            default: def.provenance_hygiene_cap.to_string(),
            help: "Facts left pointing at the wrong source, repaired per cycle. No \
                   model call — this one only re-reads. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "briefing_processor_grace_secs",
            label: "A new comment is left alone for — seconds",
            default: def.briefing_processor_grace.num_seconds().to_string(),
            help: "How long a fresh comment left on a page is untouched before the \
                   cycle reads it and changes the facts from it — you might still be \
                   editing. \"Mark as read\" on a smart wiki does not wait."
                .to_owned(),
        },
        Knob {
            field: "husk_gc_cap",
            label: "Emptied pages — files removed per cycle",
            default: def.husk_gc_cap.to_string(),
            help: "Page files with nothing live left on them — every fact closed or \
                   replaced — and no longer listed in the engine\'s own notes. Removed \
                   per full cycle. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "recall_repair_cap",
            label: "Something the memory failed to find — cases judged per cycle",
            default: def.recall_repair_cap.to_string(),
            help: "Recorded cases where a recall should have found something and did \
                   not, judged per cycle. Each costs one model call plus a replay of \
                   the known-good set on a throwaway copy. 0 turns the pass off."
                .to_owned(),
        },
        Knob {
            field: "recall_tuning_recurrence",
            label: "Same fact missed this many times — then you are told",
            default: def.recall_tuning_recurrence.to_string(),
            help: "How many times the same fact must be missed, unrepaired, before \
                   the cycle raises a notice for you. Nothing is ever changed on its \
                   own from it."
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
        "auto_promote_min_page_facts_technical" => s(cfg.auto_promote_min_page_facts_technical),
        "auto_promote_group_min_pages" => s(cfg.auto_promote_group_min_pages),
        "auto_promote_cap" => s(cfg.auto_promote_cap),
        "page_merge_cap" => s(cfg.page_merge_cap),
        "structure_review_cap" => s(cfg.structure_review_cap),
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
    let chrome = layout::Chrome::of(&state);
    // The runtime handle holds the resolved policy; the Option-shaped
    // overrides this form edits live in the YAML, so read them there.
    let workdir = workdir_of(&state)?;
    let cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    let body = render(chrome, admin.session(), &cfg.rem.policy, None, None);
    Ok(Html(body))
}

/// `typed` is the raw form body of a save that did not go through: its
/// values win over the stored ones so a refused save hands the admin
/// back what they wrote, instead of a bare error page and fourteen
/// fields to fill again.
fn render(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    cfg: &RemPolicyConfig,
    typed: Option<&HashMap<String, String>>,
    flash: Option<Flash<'_>>,
) -> String {
    let body: Markup = html! {
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        p.flash.flash-info {
            strong { "Changes go live on the next REM cycle." }
            " Saving rewrites " code { (CONFIG_FILENAME) } " atomically and "
            "hot-swaps the running policy — the interval scheduler reads it "
            "at each cycle start and the Dream console at each trigger, "
            "no restart needed."
        }

        p.muted {
            "What the night is allowed to do, and how much of it per cycle: how "
            "big a page has to get before it is split, how many changes each "
            "pass may make, how long a new comment is left alone. These back "
            "the " code { "rem.policy:" } " section of " code { (CONFIG_FILENAME) }
            ". Leave a field empty to keep the built-in default (shown "
            "as the placeholder). Semantic judgment — what to merge, "
            "promote, or rewrite — stays with the passes themselves, not here."
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
                                    value=(typed
                                        .and_then(|t| t.get(k.field))
                                        .cloned()
                                        .unwrap_or_else(|| override_value(cfg, k.field)))
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
            " (light/full intervals — " code { "rem.schedule:" } ") is on the "
            a href="/dashboard/settings/me" { "Settings page" }
            "; which model each pass runs on is set in the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; the per-turn recall resources are the "
            a href="/dashboard/admin/recall-settings" { "recall settings" }
            "."
        }
    };
    layout::authenticated_page(chrome, "REM settings", session, &body)
}

// ---------- POST ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    // A number that will not parse hands the whole form back with what
    // was typed still in it. Losing fourteen fields to one typo is the
    // kind of refusal nobody forgives twice.
    let parsed = match parse_form(&form) {
        Ok(parsed) => parsed,
        Err(e) => return refused(&state, &admin, &form, &e.to_string()),
    };

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
        chrome,
        admin.session(),
        &parsed,
        None,
        Some(Flash {
            kind: "success",
            msg: "REM settings saved and hot-reloaded — the next REM cycle uses them.",
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
        auto_promote_min_page_facts_technical: parse_usize(
            form,
            "auto_promote_min_page_facts_technical",
        )?,
        auto_promote_group_min_pages: parse_usize(form, "auto_promote_group_min_pages")?,
        auto_promote_cap: parse_usize(form, "auto_promote_cap")?,
        page_merge_cap: parse_usize(form, "page_merge_cap")?,
        structure_review_cap: parse_usize(form, "structure_review_cap")?,
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

/// The page a save that did not go through comes back as: the same
/// form, the values the admin typed still in the fields, the reason
/// across the top — and a `422`, because nothing was written.
fn refused(
    state: &DashboardState,
    admin: &AdminUser,
    typed: &HashMap<String, String>,
    msg: &str,
) -> Result<Response> {
    let chrome = layout::Chrome::of(state);
    let workdir = workdir_of(state)?;
    let cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    let body = render(
        chrome,
        admin.session(),
        &cfg.rem.policy,
        Some(typed),
        Some(Flash { kind: "error", msg }),
    );
    Ok((StatusCode::UNPROCESSABLE_ENTITY, Html(body)).into_response())
}
