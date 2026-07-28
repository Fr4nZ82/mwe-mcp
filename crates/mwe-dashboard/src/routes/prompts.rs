// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only operational prompt editor + drift banner.
//!
//! The prompt library shipped by `mwe-core` + `mwe-dashboard` lives at
//! `<workdir>/prompts/<name>.md` after `mwe-mcp init`. This
//! module exposes that directory through the dashboard so the admin
//! can review the operator-overridable system prompts, edit one in
//! place, and roll back to the bundled default — all without touching
//! the filesystem by hand or restarting the binary (the loader
//! `mwe_core::prompts::load` re-reads the override on every call).
//!
//! Four routes, all behind [`AdminUser`]:
//!
//! - GET  `/prompts`            — list every bundled prompt with a
//!   status badge: `matches default` when the workdir file is
//!   byte-for-byte equal to the bundled body, `modified` when it has
//!   been hand-edited, `bundled only` when the workdir file is missing
//!   (runtime falls back to the `include_str!` default). A second
//!   `drift v1.0 → v2.0` pill appears next to the status when the
//!   workdir file's `default_version_at_bootstrap` does not match
//!   the bundled frontmatter.
//! - GET  `/prompts/:name`      — edit form. Textarea pre-filled with
//!   the current workdir file when present, otherwise with the bundled
//!   body so the admin sees the default before overriding it. The
//!   submitted body is validated by re-parsing through
//!   [`mwe_core::prompts::extract_fenced_text`] — a save that would
//!   leave the loader unable to find the fenced ` ```text ` block is
//!   rejected with an inline error.
//! - POST `/prompts/:name`      — atomic save. If the workdir file
//!   already exists, its current bytes are copied to
//!   `<workdir>/prompts/<name>.md.bak` first (single-slot, each save
//!   overwrites the previous backup), then the new bytes are written
//!   via [`mwe_core::wiki::atomic_write`] so a crash mid-write cannot
//!   leave a half-formed file in place.
//! - POST `/prompts/:name/reset` — reset to bundled default. Same
//!   `.bak` discipline as save: existing file is backed up, then the
//!   bundled body is written verbatim. After a reset the runtime
//!   loader sees `default_version_at_bootstrap: vN.M` from the
//!   bundled frontmatter — the same key the drift banner reads to
//!   decide whether the workdir file is on the latest version.
//!
//! ## Drift detection
//!
//! `default_version_at_bootstrap: vN.M` lives in every bundled
//! prompt's frontmatter. At seed time (`mwe-mcp init`) the bundled
//! body is copied verbatim, so workdir and bundled start equal. When
//! the operator upgrades the binary the *bundled* version may have
//! moved (e.g. `v2.0` → `v2.1`); the workdir file still carries the
//! *old* version it was seeded with. The dashboard reads both via
//! [`mwe_core::prompts::parse_default_version_at_bootstrap`] and
//! surfaces a drift pill + an in-page banner when they differ. No
//! merge, no diff UI — the operator decides whether to keep their
//! edits (no action) or regenerate from bundled (the existing reset
//! button preserves the previous content as `.bak`).
//!
//! Diff / upgrade UI between the workdir file and the bundled default
//! is **deliberately deferred**: it requires a diff
//! library and a structured merge flow, and ships in a later
//! milestone if operator demand justifies it.

use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::Html;
use axum::routing::{get, post};
use maud::html;
use mwe_core::prompts::{self, extract_fenced_text, parse_default_version_at_bootstrap};
use mwe_core::wiki::atomic_write;
use serde::Deserialize;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::state::{DashboardState, MemoryHandles};
use crate::ui::{components, layout};

/// Sub-router for `/prompts/*`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/prompts", get(list))
        .route("/prompts/:name", get(edit_form).post(save))
        .route("/prompts/:name/reset", post(reset))
}

/// Status of a single bundled prompt relative to its workdir override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptStatus {
    /// Workdir file is present and byte-for-byte equal to the bundled
    /// body — this is the post-`init` baseline.
    MatchesDefault,
    /// Workdir file is present and differs from the bundled body. An
    /// operator hand-edit (or a `reset` recovery) puts a file here.
    Modified,
    /// Workdir file is missing — the runtime loader falls back to the
    /// bundled `include_str!` default. Reachable if the operator
    /// deleted the file, or if `mwe-mcp init` was never run on this
    /// workdir.
    BundledOnly,
}

impl PromptStatus {
    const fn badge_label(self) -> &'static str {
        match self {
            Self::MatchesDefault => "matches default",
            Self::Modified => "modified",
            Self::BundledOnly => "bundled only",
        }
    }

    const fn badge_class(self) -> &'static str {
        match self {
            // The CSS already styles `.flash-success` (green) and
            // `.flash-error` (red); reuse those classes so the badge
            // picks up theming without a new selector. `info` falls
            // back to muted text.
            Self::MatchesDefault => "badge-default",
            Self::Modified => "badge-modified",
            Self::BundledOnly => "badge-bundled",
        }
    }
}

/// Drift between the workdir file's `default_version_at_bootstrap`
/// and the bundled frontmatter. `None` means the two strings match
/// (or both are missing) and there is nothing to surface to the
/// operator. `Some(_)` is the banner trigger.
///
/// We treat "workdir has no version" against "bundled has a version"
/// as drift — operators who blank the key by hand-editing
/// frontmatter should still see the upgrade signal. Conversely,
/// "bundled has no version" suppresses the banner entirely: a
/// bundled prompt that opts out of versioning has nothing to compare
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drift {
    /// Version recorded in `<workdir>/prompts/<name>.md`. `None`
    /// when the workdir file is missing the key or the file itself
    /// does not exist.
    workdir_version: Option<String>,
    /// Version baked into the bundled `include_str!` body. Always
    /// present in the prompts this binary ships (a regression guard
    /// lives in `mwe_core::prompts::tests`).
    bundled_version: String,
}

impl Drift {
    fn workdir_display(&self) -> &str {
        self.workdir_version.as_deref().unwrap_or("(unset)")
    }
}

/// Detect drift between the workdir file and the bundled body. The
/// workdir file is read on disk (no extra IO beyond what the status
/// check already does — see [`status_for`]), the bundled body is
/// the compile-time `include_str!` slice. Returns `None` when there
/// is nothing to flag.
fn drift_for(workdir_file: &Path, bundled_body: &str) -> Result<Option<Drift>> {
    let Some(bundled_version) = parse_default_version_at_bootstrap(bundled_body) else {
        // Bundled body has no version — drift detection is opt-in
        // per-prompt at the bundling layer. Nothing to compare.
        return Ok(None);
    };
    let workdir_version = match fs::read_to_string(workdir_file) {
        Ok(s) => parse_default_version_at_bootstrap(&s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No workdir file ⇒ runtime falls back to bundled ⇒
            // there is no "what the operator was seeded with" to
            // drift against. Treat as no drift; the BundledOnly
            // status badge is already telling the operator the
            // file is missing.
            return Ok(None);
        },
        Err(e) => {
            return Err(DashboardError::Internal(format!(
                "read {}: {e}",
                workdir_file.display()
            )));
        },
    };
    if workdir_version.as_deref() == Some(bundled_version.as_str()) {
        Ok(None)
    } else {
        Ok(Some(Drift {
            workdir_version,
            bundled_version,
        }))
    }
}

/// One row of the listing — a bundled prompt + its workdir status +
/// optional drift information.
#[derive(Debug)]
struct PromptRow {
    name: &'static str,
    status: PromptStatus,
    drift: Option<Drift>,
}

/// Iterate over every bundled prompt this deployment ships — the
/// `mwe-core` slice (4 entries at the time of writing) chained with
/// the `mwe-dashboard` slice (1 entry). The loader module stays
/// unaware of which prompts live where (matches the `seed_bundled_into`
/// invariant), so the dashboard explicitly chains both slices when it
/// needs the full picture.
fn bundled_iter() -> impl Iterator<Item = (&'static str, &'static str)> {
    prompts::BUNDLED
        .iter()
        .copied()
        .chain(crate::BUNDLED_PROMPTS.iter().copied())
}

/// Look up the bundled body for `name` across both slices. Returns
/// `None` when the URL path names a prompt this build does not know
/// about — surfaced as a 404 by the caller.
fn bundled_lookup(name: &str) -> Option<&'static str> {
    bundled_iter()
        .find(|(k, _)| *k == name)
        .map(|(_, body)| body)
}

/// Resolve the `<workdir>/prompts/<name>.md` path for a given prompt.
/// The caller is expected to have validated `name` against
/// [`bundled_lookup`] first.
fn workdir_path(memory: &MemoryHandles, name: &str) -> PathBuf {
    memory.workdir.join("prompts").join(format!("{name}.md"))
}

/// Path of the single-slot backup that save and reset write before
/// mutating the live file. Each operation overwrites the previous
/// backup — there is no rolling history, on purpose: more would
/// require a UI for picking among them, which is out of scope.
fn backup_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Read the workdir file (if any) and compare against the bundled
/// body. Filesystem errors other than `NotFound` propagate so the
/// listing page never silently shows the wrong badge.
fn status_for(workdir_file: &Path, bundled_body: &str) -> Result<PromptStatus> {
    match fs::read(workdir_file) {
        Ok(bytes) => {
            if bytes == bundled_body.as_bytes() {
                Ok(PromptStatus::MatchesDefault)
            } else {
                Ok(PromptStatus::Modified)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PromptStatus::BundledOnly),
        Err(e) => Err(DashboardError::Internal(format!(
            "read {}: {e}",
            workdir_file.display()
        ))),
    }
}

/// Copy the current workdir file (if any) to `<file>.bak` so a save
/// or reset can be recovered. No-op when the file does not exist (the
/// caller is about to materialise it for the first time).
fn back_up_existing(workdir_file: &Path) -> Result<()> {
    match fs::read(workdir_file) {
        Ok(bytes) => {
            let backup = backup_path_for(workdir_file);
            atomic_write(&backup, &bytes).map_err(|e| {
                DashboardError::Internal(format!("write backup {}: {e}", backup.display()))
            })?;
            Ok(())
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DashboardError::Internal(format!(
            "read backup source {}: {e}",
            workdir_file.display()
        ))),
    }
}

/// Pull a `MemoryHandles` out of the state or fail with a 500. The
/// prompt editor needs the workdir, which is only wired when the
/// dashboard runs as part of `mwe-mcp serve`. Identity-console-only
/// deployments fall through to this branch — the operator must finish
/// the bootstrap before editing prompts.
fn require_memory(state: &DashboardState) -> Result<&MemoryHandles> {
    state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })
}

// ---------- list ----------

async fn list(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let mut rows: Vec<PromptRow> = Vec::new();
    for (name, body) in bundled_iter() {
        let path = workdir_path(memory, name);
        let status = status_for(&path, body)?;
        let drift = drift_for(&path, body)?;
        rows.push(PromptRow {
            name,
            status,
            drift,
        });
    }
    rows.sort_by_key(|r| r.name);
    Ok(Html(render_list(chrome, admin.session(), &rows, None)))
}

fn render_list(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    rows: &[PromptRow],
    flash_msg: Option<(&str, &str)>,
) -> String {
    let drift_count = rows.iter().filter(|r| r.drift.is_some()).count();
    let body = html! {
        @if let Some((kind, msg)) = flash_msg {
            (components::flash(kind, msg))
        }

        @if drift_count > 0 {
            p.flash.flash-info {
                strong {
                    @if drift_count == 1 {
                        "1 prompt is on an older bundled version."
                    } @else {
                        (drift_count) " prompts are on older bundled versions."
                    }
                }
                " The binary ships a newer default than the one your workdir "
                "was seeded with. Open the prompt to see the version "
                "transition, then use " strong { "reset to bundled" }
                " if you want to take the upgrade (your current content is "
                "preserved as " code { "<name>.md.bak" } ")."
            }
        }

        p.muted {
            "Operator-overridable system prompts loaded by the runtime from "
            code { "<workdir>/prompts/<name>.md" } " when present, otherwise from "
            "the bundled " code { "include_str!" } " default. Editing a prompt "
            "here writes the workdir file atomically; the previous content is "
            "kept as " code { "<name>.md.bak" } " (single-slot backup, "
            "overwritten on every save)."
        }

        table {
            thead { tr {
                th { "Prompt" }
                th { "Status vs bundled default" }
                th { "Actions" }
            } }
            tbody {
                @for r in rows {
                    tr {
                        td { code { (r.name) } }
                        td {
                            span class=(format!("badge {}", r.status.badge_class())) {
                                (r.status.badge_label())
                            }
                            @if let Some(d) = &r.drift {
                                " "
                                span class="badge badge-drift"
                                     title=(format!(
                                        "Workdir file seeded at {} · bundled now at {}",
                                        d.workdir_display(), d.bundled_version
                                     )) {
                                    "drift "
                                    (d.workdir_display())
                                    " → "
                                    (d.bundled_version)
                                }
                            }
                        }
                        td {
                            a href=(format!("/dashboard/prompts/{}", r.name)) { "edit" }
                            @if r.status == PromptStatus::Modified || r.drift.is_some() {
                                " · "
                                form action=(format!("/dashboard/prompts/{}/reset", r.name))
                                     method="post" class="inline-form"
                                     onsubmit=(format!(
                                        "return confirm('Reset {} to bundled default? Current content will be saved to {}.md.bak.')",
                                        r.name, r.name
                                     )) {
                                    button type="submit" class="link-button danger" {
                                        "reset to bundled"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout::authenticated_page(chrome, "Prompts", session, &body)
}

// ---------- edit form + save ----------

#[derive(Debug, Deserialize)]
pub struct PromptSaveSubmission {
    pub body: String,
}

async fn edit_form(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let Some(bundled) = bundled_lookup(&name) else {
        return Err(DashboardError::NotFound);
    };
    let memory = require_memory(&state)?;
    let path = workdir_path(memory, &name);
    let status = status_for(&path, bundled)?;
    let drift = drift_for(&path, bundled)?;
    let current_body = current_body_for(&path, bundled)?;
    Ok(Html(render_edit_form(
        chrome,
        admin.session(),
        &name,
        status,
        drift.as_ref(),
        &current_body,
        None,
        None,
    )))
}

/// Read the workdir override when present; otherwise fall back to the
/// bundled body so the editor always shows the operator something
/// meaningful to start from. Mirrors the runtime loader contract
/// (`mwe_core::prompts::load`) so the textarea reflects exactly what
/// the engine would use today.
fn current_body_for(workdir_file: &Path, bundled_body: &str) -> Result<String> {
    match fs::read_to_string(workdir_file) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(bundled_body.to_owned()),
        Err(e) => Err(DashboardError::Internal(format!(
            "read {}: {e}",
            workdir_file.display()
        ))),
    }
}

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
    axum::Form(form): axum::Form<PromptSaveSubmission>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let Some(bundled) = bundled_lookup(&name) else {
        return Err(DashboardError::NotFound);
    };
    let memory = require_memory(&state)?;
    let path = workdir_path(memory, &name);

    // Validate by re-parsing: a save that would silently break boot
    // (no fenced block, malformed fence) is rejected with the
    // operator's draft preserved so they can fix the typo.
    if let Err(e) = extract_fenced_text(&form.body, &name, "<dashboard-submission>") {
        let status = status_for(&path, bundled)?;
        let drift = drift_for(&path, bundled)?;
        let msg = format!("Refused to save: {e}");
        return Ok(Html(render_edit_form(
            chrome,
            admin.session(),
            &name,
            status,
            drift.as_ref(),
            &form.body,
            Some(&msg),
            None,
        )));
    }

    back_up_existing(&path)?;
    atomic_write(&path, form.body.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write {}: {e}", path.display())))?;

    tracing::info!(
        actor = admin.sender_id(),
        prompt = name.as_str(),
        bytes = form.body.len(),
        "dashboard: prompt override saved"
    );

    let new_status = status_for(&path, bundled)?;
    let new_drift = drift_for(&path, bundled)?;
    let msg = if new_status == PromptStatus::MatchesDefault {
        format!("Saved {name}. Content matches the bundled default — no override in effect.")
    } else {
        format!("Saved {name}. Previous content backed up as {name}.md.bak.")
    };
    Ok(Html(render_edit_form(
        chrome,
        admin.session(),
        &name,
        new_status,
        new_drift.as_ref(),
        &form.body,
        None,
        Some(&msg),
    )))
}

fn render_edit_form(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    name: &str,
    status: PromptStatus,
    drift: Option<&Drift>,
    body: &str,
    error: Option<&str>,
    success: Option<&str>,
) -> String {
    let title = format!("Prompt — {name}");
    let show_reset = status == PromptStatus::Modified || drift.is_some();
    let markup = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        @if let Some(msg) = success {
            (components::flash("success", msg))
        }

        @if let Some(d) = drift {
            p.flash.flash-info {
                strong {
                    "Bundled default moved from "
                    code { (d.workdir_display()) }
                    " to "
                    code { (d.bundled_version) }
                    "."
                }
                " The workdir file at "
                code { (format!("<workdir>/prompts/{name}.md")) }
                " is still seeded at "
                code { (d.workdir_display()) }
                ". Take the upgrade with " strong { "Reset to bundled" }
                " (your current content is saved as "
                code { (format!("{name}.md.bak")) } "), or keep editing on "
                "top of the old version — the runtime keeps reading what "
                "the workdir file says."
            }
        }

        p.muted {
            "Editing " code { (name) } " · "
            span class=(format!("badge {}", status.badge_class())) {
                (status.badge_label())
            }
            @if let Some(d) = drift {
                " "
                span class="badge badge-drift"
                     title=(format!(
                        "Workdir file seeded at {} · bundled now at {}",
                        d.workdir_display(), d.bundled_version
                     )) {
                    "drift "
                    (d.workdir_display())
                    " → "
                    (d.bundled_version)
                }
            }
            ". The textarea holds the full markdown document — frontmatter "
            "and the fenced " code { "```text" } " block. Saves are rejected "
            "if the fenced block becomes unparseable, so the runtime "
            "loader can never end up reading half a file."
        }

        form action=(format!("/dashboard/prompts/{name}")) method="post" {
            p {
                label for="body" { "Prompt source" }
                textarea id="body" name="body" rows="32" class="prompt-editor"
                    spellcheck="false" required { (body) }
            }
            (components::submit("Save override"))
        }

        @if show_reset {
            section.reset-section {
                h2 { "Reset to bundled default" }
                p.muted {
                    @if drift.is_some() {
                        "Upgrades the workdir file to the bundled version this "
                        "binary ships and discards any local edits. "
                    } @else {
                        "Reverts the workdir file to the version this binary "
                        "ships. "
                    }
                    "Your current content is preserved at "
                    code { (format!("{name}.md.bak")) } "."
                }
                form action=(format!("/dashboard/prompts/{name}/reset")) method="post"
                     class="inline-form"
                     onsubmit=(format!(
                        "return confirm('Reset {name} to bundled default? Current content will be saved to {name}.md.bak.')"
                     )) {
                    button type="submit" class="danger" { "Reset to bundled" }
                }
            }
        }

        p { a href="/dashboard/prompts" { "Back to the prompt list" } }
    };
    layout::authenticated_page(chrome, &title, session, &markup)
}

// ---------- reset ----------

async fn reset(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let Some(bundled) = bundled_lookup(&name) else {
        return Err(DashboardError::NotFound);
    };
    let memory = require_memory(&state)?;
    let path = workdir_path(memory, &name);

    back_up_existing(&path)?;
    atomic_write(&path, bundled.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write {}: {e}", path.display())))?;

    tracing::info!(
        actor = admin.sender_id(),
        prompt = name.as_str(),
        "dashboard: prompt override reset to bundled default"
    );

    let mut rows: Vec<PromptRow> = Vec::new();
    for (n, body) in bundled_iter() {
        let p = workdir_path(memory, n);
        rows.push(PromptRow {
            name: n,
            status: status_for(&p, body)?,
            drift: drift_for(&p, body)?,
        });
    }
    rows.sort_by_key(|r| r.name);
    let msg = format!("Reset {name} to bundled default. Previous content saved as {name}.md.bak.");
    Ok(Html(render_list(
        chrome,
        admin.session(),
        &rows,
        Some(("success", &msg)),
    )))
}
