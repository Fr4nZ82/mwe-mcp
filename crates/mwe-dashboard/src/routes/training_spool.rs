// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only toggle for the training spool — the prompt/completion
//! pair recorder backing future distillation fine-tunes of the local
//! slot models (`mwe_core::training_spool`).
//!
//! Two routes, both behind [`AdminUser`], mirroring the
//! [REM-settings editor](super::rem_settings):
//!
//! - GET  `/admin/training-spool` — render the toggle with the current
//!   YAML value, plus the on-disk spool inventory (per-day JSONL files
//!   and sizes) so the operator sees what has accumulated.
//! - POST `/admin/training-spool` — atomic save of the YAML config
//!   (backup `.bak` first, serialize, atomic-write) followed by an
//!   in-place flip of the process-wide spool handle
//!   (`mwe_core::training_spool::global`) — the recording decorators
//!   check the flag per LLM call, so the change is live immediately,
//!   no restart needed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config};
use mwe_core::training_spool::TRAINING_SPOOL_DIR;
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/training-spool`. Mounted inside the
/// authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/training-spool", get(page).post(save))
}

// ---------- GET ----------

struct Flash<'a> {
    kind: &'static str,
    msg: &'a str,
}

/// One spool file on disk: name + size, newest first.
struct SpoolFile {
    name: String,
    bytes: u64,
}

/// Read the spool directory inventory. An absent directory is the
/// normal never-enabled state, rendered as an empty list.
fn spool_inventory(workdir: &Path) -> Vec<SpoolFile> {
    let dir = workdir.join(TRAINING_SPOOL_DIR);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<SpoolFile> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| SpoolFile {
                name: e.file_name().to_string_lossy().into_owned(),
                bytes: meta.len(),
            })
        })
        .collect();
    files.sort_by(|a, b| b.name.cmp(&a.name));
    files
}

/// Human-readable byte count for the inventory table.
fn human_bytes(bytes: u64) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display rounding — spool files never approach 2^52 bytes"
    )]
    let b = bytes as f64;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let workdir = workdir_of(&state)?;
    let cfg = Config::load(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    let body = render(admin.session(), cfg.training_spool.enabled, &workdir, None);
    Ok(Html(body))
}

fn render(
    session: &crate::auth::SessionUser,
    enabled: bool,
    workdir: &Path,
    flash: Option<Flash<'_>>,
) -> String {
    let files = spool_inventory(workdir);
    let total: u64 = files.iter().map(|f| f.bytes).sum();
    let dir_display = workdir.join(TRAINING_SPOOL_DIR).display().to_string();
    let body: Markup = html! {
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        h2 { "Training spool" }
        p.muted {
            "Records every internal-LLM exchange — slot, model, full "
            "prompt, full completion — as one JSON line per call into "
            "per-day files under " code { (dir_display) } ". The point: "
            "the strong API slots act as teachers, and their traces "
            "become the fine-tuning dataset that lets a small local "
            "model take over the slot (distillation). Backs the "
            code { "training_spool:" } " section of "
            code { (CONFIG_FILENAME) } "."
        }
        p.flash.flash-info {
            strong { "The spool holds raw prompts" }
            " — including recalled memory content of every user this "
            "deployment serves. It never leaves this machine, but treat "
            "the directory like the wikis themselves: back it up "
            "deliberately or prune it, and clear it before sharing a "
            "dataset."
        }

        form action="/dashboard/admin/training-spool" method="post" {
            p {
                label {
                    input type="checkbox" name="enabled" value="1" checked[enabled];
                    " Record internal-LLM prompt/completion pairs"
                }
            }
            p { button type="submit" { "Save" } }
        }

        h3 { "On disk" }
        @if files.is_empty() {
            p.muted { "No spool files yet." }
        } @else {
            table.config-table {
                thead { tr { th { "File" } th { "Size" } } }
                tbody {
                    @for f in &files {
                        tr {
                            td { code { (f.name) } }
                            td { (human_bytes(f.bytes)) }
                        }
                    }
                    tr {
                        td { strong { "Total" } }
                        td { strong { (human_bytes(total)) } }
                    }
                }
            }
        }

        p.muted {
            "Related dials elsewhere: which model serves each slot is the "
            a href="/dashboard/admin/llm-config" { "LLM config editor" }
            "; health probes are never recorded, and failed calls have "
            "no completion to record."
        }
    };
    layout::authenticated_page("Training spool", session, &body)
}

// ---------- POST ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    // Checkbox idiom: the field is present only when checked.
    let enabled = form.contains_key("enabled");

    // Preserve every other section of the existing Config by re-loading
    // from disk and replacing only `training_spool`. Config::load
    // returns Default when the file is missing — the first save
    // materialises it.
    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.training_spool.enabled = enabled;

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

    // Hot-flip: disk first, then the in-place swap — a crash between
    // the two still boots with the new YAML. The global is installed by
    // the server binary at startup; a missing one (embedded/test use)
    // only means there is nothing to flip.
    if let Some(spool) = mwe_core::training_spool::global() {
        spool.set_enabled(enabled);
    } else {
        tracing::warn!("training-spool: no process-wide spool installed — saved YAML only");
    }

    tracing::info!(
        admin = %admin.session().sender_id,
        enabled,
        path = %path.display(),
        "training-spool: saved from dashboard (hot-flipped)"
    );

    let flash_msg = if enabled {
        "Training spool enabled — internal-LLM calls are being recorded from now on."
    } else {
        "Training spool disabled — recording stopped; existing files stay on disk."
    };
    let body = render(
        admin.session(),
        enabled,
        &workdir,
        Some(Flash {
            kind: "success",
            msg: flash_msg,
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
