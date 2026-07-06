// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only "Backup now" maintenance trigger (roadmap 19d).
//!
//! Takes a hot workdir snapshot via [`mwe_core::backup::snapshot_workdir`]
//! — the same point-in-time copy the CLI `mwe-mcp backup --out` produces,
//! safe next to the running server (no lockfile, source DB opened
//! read-only). The `backup` CLI stays for cron / server-off operation;
//! this is the in-dashboard convenience for an operator who is already
//! looking at the running server.
//!
//! Two routes, both behind [`AdminUser`]:
//!
//! - GET  `/admin/backup` — render the form, prefilled with a suggested
//!   timestamped destination outside the workdir.
//! - POST `/admin/backup` — run the snapshot and report what was written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use maud::{Markup, html};
use mwe_core::backup::{self, BackupReport};

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/backup`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/admin/backup", get(page).post(run))
}

/// Outcome of a POST, rendered as a flash + (on success) a report table.
enum Outcome {
    Ok { dest: String, report: BackupReport },
    Err(String),
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let workdir = workdir_of(&state)?;
    Ok(Html(render(
        admin.session(),
        &suggested_dest(&workdir),
        None,
    )))
}

async fn run(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let workdir = workdir_of(&state)?;
    let dest = form
        .get("dest")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            DashboardError::Validation("a destination directory is required".to_owned())
        })?;
    let dest_path = PathBuf::from(&dest);

    let outcome = match backup::snapshot_workdir(&workdir, &dest_path).await {
        Ok(report) => {
            tracing::info!(
                admin = %admin.session().sender_id,
                dest = %dest_path.display(),
                db_bytes = report.db_bytes,
                files_copied = report.files_copied,
                "backup: workdir snapshot from dashboard"
            );
            Outcome::Ok {
                dest: dest.clone(),
                report,
            }
        },
        Err(e) => {
            tracing::warn!(
                admin = %admin.session().sender_id,
                dest = %dest_path.display(),
                error = %e,
                "backup: dashboard snapshot failed"
            );
            Outcome::Err(e.to_string())
        },
    };

    // On failure keep the operator's typed path; on success offer a fresh
    // suggestion so a second backup does not collide with the (now
    // non-empty) destination.
    let field = match &outcome {
        Outcome::Ok { .. } => suggested_dest(&workdir),
        Outcome::Err(_) => dest,
    };
    Ok(Html(render(admin.session(), &field, Some(&outcome))))
}

fn render(session: &SessionUser, dest_value: &str, outcome: Option<&Outcome>) -> String {
    let body = html! {
        h2 { "Backup now" }
        p.muted {
            "Take a hot workdir snapshot — a consistent point-in-time copy of "
            code { "engine.db" } " plus the markdown tree, config and "
            code { "mwe-mcp.env" } ". Safe to run next to the live server "
            "(no lockfile). Identical to the CLI " code { "mwe-mcp backup --out" }
            ", which stays for cron / server-off operation."
        }
        p.flash.flash-error {
            strong { "The snapshot contains secrets" } " (" code { "mwe-mcp.env" }
            " holds " code { "MWE_TOKEN_SECRET" } " + API keys) and the cleartext "
            "memory wiki. Write it to an owner-only location and treat it like the "
            "workdir itself."
        }

        @if let Some(o) = outcome {
            (outcome_flash(o))
        }

        form action="/dashboard/admin/backup" method="post" {
            (components::text_field(
                "dest",
                "Destination directory (must be empty and outside the workdir)",
                "text",
                dest_value,
                true,
            ))
            (components::submit("Back up now"))
        }
    };
    layout::authenticated_page("Backup", session, &body)
}

/// The flash + report for a completed POST.
fn outcome_flash(outcome: &Outcome) -> Markup {
    match outcome {
        Outcome::Ok { dest, report } => html! {
            (components::flash("success", "Snapshot complete."))
            table.config-table {
                tbody {
                    tr { td { "Destination" } td { code { (dest) } } }
                    tr { td { "engine.db copy" } td { (human_bytes(report.db_bytes)) } }
                    tr { td { "Files copied" } td { (report.files_copied) } }
                    tr { td { "Bytes copied" } td { (human_bytes(report.bytes_copied)) } }
                }
            }
        },
        Outcome::Err(msg) => html! {
            (components::flash("error", msg))
        },
    }
}

/// A suggested destination: a timestamped sibling of the workdir, which is
/// guaranteed empty (fresh name) and outside the workdir.
fn suggested_dest(workdir: &Path) -> String {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mwe-mcp");
    let parent = workdir.parent().unwrap_or_else(|| Path::new("/tmp"));
    parent
        .join(format!("{name}-backup-{ts}"))
        .to_string_lossy()
        .into_owned()
}

/// Compact human-readable byte count for the report table.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only — a backup size never needs exact integer precision past f64"
    )]
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Workdir root for the snapshot — only present on the full `mwe-mcp serve`
/// build, where the memory handles carry the workdir.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_dest_is_a_timestamped_sibling() {
        let work = Path::new("/srv/mwe/work");
        let s = suggested_dest(work);
        let dest = Path::new(&s);
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(name.starts_with("work-backup-"), "got: {s}");
        assert_eq!(
            dest.parent(),
            work.parent(),
            "must be a sibling, not inside the workdir: {s}"
        );
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }
}
