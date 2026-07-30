// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only Backup console — the recovery surface (roadmap 4d).
//!
//! One page, `/admin/backup`, carrying the whole recovery story:
//!
//! - **Automatic snapshots** — editor for the `backup:` config section
//!   (mode, interval, retention, home directory), saved atomically with
//!   a `.bak` and hot-swapped into the scheduler's shared handle, plus
//!   the last automatic run's outcome from `engine_meta`.
//! - **Snapshot now** — the on-demand hot snapshot via
//!   [`mwe_core::backup::snapshot_workdir`], identical to the CLI
//!   `mwe-mcp backup --out` (which stays for cron / server-off use).
//! - **Snapshots on disk** — the snapshots home listed with provenance
//!   badges; each row offers *Restore…* and *Delete*.
//! - **Staged recovery** — restore and the memory reset never run in
//!   the live process (the pool holds `engine.db` open): they stage a
//!   one-shot marker via [`mwe_core::recovery`] that the next server
//!   boot applies under the lockfile, safety snapshot first. The page
//!   shows the pending request (cancellable) and, when the serve build
//!   wired a [`RestartHandle`](crate::state::RestartHandle), offers
//!   "Restart now" — a graceful shutdown that exits with the deliberate
//!   non-zero code so a `Restart=on-failure` unit relaunches.
//! - **Memory reset** — the danger-zone form (type `RESET` to confirm):
//!   wipes memory, keeps accounts/consumers/tokens/config/prompts.
//!
//! Every route is behind [`AdminUser`]; every POST re-renders the page
//! with a flash, the pattern of the other section editors.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::routing::{get, post};
use maud::html;
use mwe_core::backup::{
    self, AutoSnapshotReport, BackupReport, META_LAST_AUTO_REPORT, SnapshotEntry,
};
use mwe_core::config::{BackupConfig, BackupScheduleMode, CONFIG_FILENAME, Config};
use mwe_core::recovery::{
    self, META_LAST_RECOVERY, RecoveryAction, RecoveryOutcome, RecoveryRequest,
};
use mwe_core::wiki::atomic_write;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Sub-router for `/admin/backup`. Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/backup", get(page).post(run))
        .route("/admin/backup/settings", post(save_settings))
        .route("/admin/backup/restore", post(schedule_restore))
        .route("/admin/backup/delete", post(delete_snapshot))
        .route("/admin/backup/reset", post(schedule_reset))
        .route("/admin/backup/cancel", post(cancel_pending))
        .route("/admin/backup/restart", post(restart_now))
}

/// The typed phrase the memory-reset form requires.
const RESET_CONFIRM_PHRASE: &str = "RESET";

/// Everything the page render needs, assembled once per request.
struct PageCtx {
    workdir: PathBuf,
    /// The raw `backup:` section from the YAML (what the editor shows).
    cfg: BackupConfig,
    snapshots_dir: PathBuf,
    snapshots: Vec<SnapshotEntry>,
    pending: Option<RecoveryRequest>,
    last_recovery: Option<RecoveryOutcome>,
    last_auto: Option<AutoSnapshotReport>,
    restart_available: bool,
}

/// A flash line for the re-render.
struct Flash {
    kind: &'static str,
    msg: String,
}

impl Flash {
    fn success(msg: impl Into<String>) -> Self {
        Self {
            kind: "success",
            msg: msg.into(),
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            kind: "error",
            msg: msg.into(),
        }
    }
}

async fn page_ctx(state: &DashboardState) -> Result<PageCtx> {
    let workdir = workdir_of(state)?;
    let cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?
        .backup;
    let snapshots_dir = cfg.snapshots_dir(&workdir);
    let snapshots = backup::list_snapshots(&snapshots_dir)
        .map_err(|e| DashboardError::Internal(format!("listing snapshots: {e}")))?;
    let pending = recovery::pending(&workdir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "backup console: pending marker unreadable");
        None
    });
    let last_recovery = meta_json(state, META_LAST_RECOVERY).await;
    let last_auto = meta_json(state, META_LAST_AUTO_REPORT).await;
    Ok(PageCtx {
        workdir,
        cfg,
        snapshots_dir,
        snapshots,
        pending,
        last_recovery,
        last_auto,
        restart_available: state.restart.is_some(),
    })
}

/// Read + deserialize an `engine_meta` JSON value, `None` on anything
/// missing or stale-shaped.
async fn meta_json<T: serde::de::DeserializeOwned>(state: &DashboardState, key: &str) -> Option<T> {
    let raw = mwe_core::db::meta_get(&state.pool, key).await.ok()??;
    serde_json::from_str(&raw).ok()
}

// ---------- GET ----------

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        None,
        None,
    )))
}

// ---------- POST: manual snapshot ----------

async fn run(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let workdir = workdir_of(&state)?;
    let dest = form
        .get("dest")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            DashboardError::Validation("a destination directory is required".to_owned())
        })?;
    let dest_path = PathBuf::from(&dest);

    let (flash, report, field) = match backup::snapshot_workdir(&workdir, &dest_path).await {
        Ok(report) => {
            tracing::info!(
                admin = %admin.session().sender_id,
                dest = %dest_path.display(),
                db_bytes = report.db_bytes,
                files_copied = report.files_copied,
                "backup: workdir snapshot from dashboard"
            );
            (
                Flash::success(format!("Snapshot complete: {dest}")),
                Some(report),
                None,
            )
        },
        Err(e) => {
            tracing::warn!(
                admin = %admin.session().sender_id,
                dest = %dest_path.display(),
                error = %e,
                "backup: dashboard snapshot failed"
            );
            // Keep the operator's typed path on failure; offer a fresh
            // suggestion on success (the old one is now non-empty).
            (Flash::error(e.to_string()), None, Some(dest))
        },
    };

    let ctx = page_ctx(&state).await?;
    let field = field.unwrap_or_else(|| suggested_dest(&ctx.snapshots_dir));
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &field,
        Some(&flash),
        report.as_ref(),
    )))
}

// ---------- POST: settings ----------

async fn save_settings(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let workdir = workdir_of(&state)?;
    let parsed = parse_settings(&form)?;

    // Round-trip through the raw YAML: replace only the `backup:`
    // section, `.bak` the old file, atomic-write, then hot-swap the
    // scheduler's shared handle (disk first, then swap).
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.backup = parsed.clone();
    let path = workdir.join(CONFIG_FILENAME);
    let bak = {
        let mut s = path.as_os_str().to_owned();
        s.push(".bak");
        PathBuf::from(s)
    };
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&bak, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }
    let yaml = serde_yaml::to_string(&cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;
    state.replace_backup_schedule(parsed.resolved_schedule(&workdir));

    tracing::info!(
        admin = %admin.session().sender_id,
        path = %path.display(),
        "backup settings: saved from dashboard (hot-reloaded)"
    );
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        Some(&Flash::success(
            "Backup settings saved — the scheduler reads them at its next due-check \
             (the initial delay applies from the next restart).",
        )),
        None,
    )))
}

/// Decode the settings form into a fresh [`BackupConfig`]. Empty
/// numeric fields keep the Rust defaults; an empty dir means "the
/// default sibling".
fn parse_settings(form: &HashMap<String, String>) -> Result<BackupConfig> {
    let defaults = BackupConfig::default();
    let mode = match form.get("mode").map(String::as_str) {
        Some("interval") | None => BackupScheduleMode::Interval,
        Some("disabled") => BackupScheduleMode::Disabled,
        Some(other) => {
            return Err(DashboardError::Validation(format!(
                "`mode` must be `interval` or `disabled`, got {other:?}"
            )));
        },
    };
    let num = |field: &'static str, default: u64| -> Result<u64> {
        let raw = form.get(field).map(|s| s.trim()).unwrap_or_default();
        if raw.is_empty() {
            return Ok(default);
        }
        raw.parse::<u64>().map_err(|_| {
            DashboardError::Validation(format!("`{field}` must be a non-negative integer"))
        })
    };
    let interval_secs = num("interval_secs", defaults.interval_secs)?.max(60);
    let initial_delay_secs = num("initial_delay_secs", defaults.initial_delay_secs)?;
    let retention_auto = u32::try_from(num("retention_auto", u64::from(defaults.retention_auto))?)
        .map_err(|_| DashboardError::Validation("`retention_auto` is out of range".to_owned()))?;
    let dir = form
        .get("dir")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    Ok(BackupConfig {
        mode,
        interval_secs,
        initial_delay_secs,
        dir,
        retention_auto,
    })
}

// ---------- POST: staged recovery ----------

async fn schedule_restore(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let workdir = workdir_of(&state)?;
    let ctx = page_ctx(&state).await?;
    let snapshot = named_snapshot(&ctx, &form)?;

    let req = RecoveryRequest {
        action: RecoveryAction::Restore {
            snapshot: snapshot.path.clone(),
        },
        requested_by: admin.session().sender_id.clone(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    let flash = match recovery::schedule(&workdir, &req) {
        Ok(()) => Flash::success(format!(
            "Restore from {} staged — it applies at the next server start, \
             after an automatic safety snapshot.",
            snapshot.name
        )),
        Err(e) => Flash::error(e.to_string()),
    };
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        Some(&flash),
        None,
    )))
}

async fn schedule_reset(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let workdir = workdir_of(&state)?;
    let typed = form.get("confirm").map(|s| s.trim()).unwrap_or_default();
    if typed != RESET_CONFIRM_PHRASE {
        return Err(DashboardError::Validation(format!(
            "type {RESET_CONFIRM_PHRASE} to confirm the memory reset"
        )));
    }
    let req = RecoveryRequest {
        action: RecoveryAction::Reset,
        requested_by: admin.session().sender_id.clone(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    let flash = match recovery::schedule(&workdir, &req) {
        Ok(()) => Flash::success(
            "Memory reset staged — it applies at the next server start, \
             after an automatic safety snapshot.",
        ),
        Err(e) => Flash::error(e.to_string()),
    };
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        Some(&flash),
        None,
    )))
}

async fn cancel_pending(
    State(state): State<DashboardState>,
    admin: AdminUser,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let workdir = workdir_of(&state)?;
    let flash = match recovery::cancel(&workdir) {
        Ok(true) => Flash::success("Pending recovery cancelled."),
        Ok(false) => Flash::error("Nothing was pending."),
        Err(e) => Flash::error(e.to_string()),
    };
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        Some(&flash),
        None,
    )))
}

async fn delete_snapshot(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let ctx = page_ctx(&state).await?;
    let snapshot = named_snapshot(&ctx, &form)?;
    // A snapshot the pending restore points at must stay until the
    // request is cancelled or applied.
    if let Some(RecoveryRequest {
        action: RecoveryAction::Restore { snapshot: staged },
        ..
    }) = &ctx.pending
        && *staged == snapshot.path
    {
        return Err(DashboardError::Validation(
            "this snapshot is the target of the pending restore — cancel that first".to_owned(),
        ));
    }
    let flash = match fs::remove_dir_all(&snapshot.path) {
        Ok(()) => {
            tracing::info!(
                admin = %admin.session().sender_id,
                snapshot = %snapshot.path.display(),
                "backup: snapshot deleted from dashboard"
            );
            Flash::success(format!("Snapshot {} deleted.", snapshot.name))
        },
        Err(e) => Flash::error(format!("deleting {}: {e}", snapshot.name)),
    };
    let ctx = page_ctx(&state).await?;
    let dest = suggested_dest(&ctx.snapshots_dir);
    Ok(Html(render(
        chrome,
        admin.session(),
        &ctx,
        &dest,
        Some(&flash),
        None,
    )))
}

/// Resolve the form's `name` against the listed snapshots — the only
/// path material a row action may reference (no free-form paths).
fn named_snapshot<'a>(
    ctx: &'a PageCtx,
    form: &HashMap<String, String>,
) -> Result<&'a SnapshotEntry> {
    let name = form
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DashboardError::Validation("a snapshot name is required".to_owned()))?;
    ctx.snapshots
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| DashboardError::Validation(format!("{name:?} is not in the snapshots home")))
}

// ---------- POST: restart ----------

async fn restart_now(
    State(state): State<DashboardState>,
    admin: AdminUser,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let Some(handle) = &state.restart else {
        return Err(DashboardError::Validation(
            "restart is not available on this build".to_owned(),
        ));
    };
    tracing::info!(
        admin = %admin.session().sender_id,
        "backup console: restart requested — broadcasting graceful shutdown"
    );
    handle
        .requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = handle.shutdown.send(());

    let body = html! {
        h2 { "Restarting" }
        (components::flash(
            "success",
            "The server is shutting down gracefully. Under the provisioned systemd \
             unit it relaunches within seconds — reload this page. If you run the \
             server by hand, start it again yourself.",
        ))
        @if recovery_hint(&state) {
            p.muted { "The staged recovery applies during the restart, safety snapshot first." }
        }
        p { a href="/dashboard/admin/backup" { "Back to the Backup console" } }
    };
    Ok(Html(layout::authenticated_page(
        chrome,
        "Restarting",
        admin.session(),
        &body,
    )))
}

/// Whether a recovery is pending (for the restart page's hint line).
fn recovery_hint(state: &DashboardState) -> bool {
    workdir_of(state)
        .ok()
        .and_then(|w| recovery::pending(&w).ok().flatten())
        .is_some()
}

// ---------- render ----------

#[allow(
    clippy::too_many_lines,
    reason = "one linear page template — splitting it would scatter the console's layout"
)]
fn render(
    chrome: layout::Chrome,
    session: &SessionUser,
    ctx: &PageCtx,
    dest_value: &str,
    flash: Option<&Flash>,
    manual_report: Option<&BackupReport>,
) -> String {
    let defaults = BackupConfig::default();
    let body = html! {
        h2 { "Backup & recovery" }

        @if let Some(f) = flash {
            (components::flash(f.kind, &f.msg))
        }

        // ---- pending staged recovery ----
        @if let Some(p) = &ctx.pending {
            div.flash.flash-error {
                strong { "Staged recovery pending: " (p.action.label()) "." }
                " Requested by " code { (p.requested_by) } " at " (p.requested_at)
                ". It applies at the next server start — an automatic safety "
                "snapshot is taken first, and the marker is consumed even if "
                "the action is refused."
            }
            div {
                form action="/dashboard/admin/backup/cancel" method="post" class="inline-form" {
                    button type="submit" { "Cancel pending recovery" }
                }
                " "
                @if ctx.restart_available {
                    form action="/dashboard/admin/backup/restart" method="post" class="inline-form"
                        onsubmit="return confirm('Restart the server now and apply the staged recovery?')" {
                        button type="submit" class="danger" { "Restart now and apply" }
                    }
                }
            }
        }

        // ---- last recovery outcome ----
        @if let Some(last) = &ctx.last_recovery {
            @let kind = if last.ok { "success" } else { "error" };
            p.flash.(format!("flash-{kind}")) {
                strong { "Last recovery — " (last.action) ": " (if last.ok { "applied" } else { "refused" }) "." }
                " " (last.detail) " (" (last.finished_at) ")"
                @if let Some(s) = &last.safety_snapshot {
                    br; "Safety snapshot: " code { (s) }
                }
            }
        }

        // ---- automatic snapshots ----
        h3 { "Automatic snapshots" }
        p.muted {
            "The scheduler takes a hot snapshot of the whole workdir into the "
            "snapshots home on the configured interval, prunes the oldest "
            code { "auto-*" } " snapshots beyond the retention, and stamps its "
            "last run in the engine DB — a restart never re-fires a snapshot "
            "inside the interval. Backing the " code { "backup:" } " section of "
            code { (CONFIG_FILENAME) } "."
        }
        @if let Some(auto) = &ctx.last_auto {
            @let kind = if auto.ok { "success" } else { "error" };
            p.flash.(format!("flash-{kind}")) {
                strong { "Last automatic snapshot: " (if auto.ok { "ok" } else { "FAILED" }) "." }
                " " (auto.detail) " (" (auto.at) ") — " code { (auto.dest) }
            }
        }
        form action="/dashboard/admin/backup/settings" method="post" {
            table.config-table {
                tbody {
                    tr {
                        td { label for="mode" { "Mode" } }
                        td {
                            select id="mode" name="mode" {
                                option value="interval" selected[ctx.cfg.mode == BackupScheduleMode::Interval] { "interval (on)" }
                                option value="disabled" selected[ctx.cfg.mode == BackupScheduleMode::Disabled] { "disabled" }
                            }
                        }
                        td.muted { "Automatic snapshots on/off. Manual snapshots always work." }
                    }
                    tr {
                        td { label for="interval_secs" { "Interval (seconds)" } }
                        td {
                            input id="interval_secs" name="interval_secs" type="number" min="60"
                                value=(ctx.cfg.interval_secs)
                                placeholder=(defaults.interval_secs);
                        }
                        td.muted { "Distance between automatic snapshots. Default 86400 (daily)." }
                    }
                    tr {
                        td { label for="retention_auto" { "Retention (auto snapshots)" } }
                        td {
                            input id="retention_auto" name="retention_auto" type="number" min="0"
                                value=(ctx.cfg.retention_auto)
                                placeholder=(defaults.retention_auto);
                        }
                        td.muted {
                            "How many " code { "auto-*" } " snapshots to keep; older ones are "
                            "pruned after each run. 0 keeps everything. Manual and safety "
                            "snapshots are never pruned."
                        }
                    }
                    tr {
                        td { label for="dir" { "Snapshots home" } }
                        td {
                            input id="dir" name="dir" type="text"
                                value=(ctx.cfg.dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default())
                                placeholder=(backup::default_snapshots_dir(&ctx.workdir).display());
                        }
                        td.muted {
                            "Must be outside the workdir. Empty = the sibling default. "
                            "Snapshots contain secrets and the cleartext memory wiki — "
                            "keep it owner-only."
                        }
                    }
                    tr {
                        td { label for="initial_delay_secs" { "Initial delay (seconds)" } }
                        td {
                            input id="initial_delay_secs" name="initial_delay_secs" type="number" min="0"
                                value=(ctx.cfg.initial_delay_secs)
                                placeholder=(defaults.initial_delay_secs);
                        }
                        td.muted { "Warm-up before the first due-check; applies from the next restart." }
                    }
                }
            }
            p { button type="submit" { "Save backup settings" } }
        }

        // ---- manual snapshot ----
        h3 { "Snapshot now" }
        p.muted {
            "A hot, consistent point-in-time copy of " code { "engine.db" }
            " plus the markdown tree, config and " code { "mwe-mcp.env" }
            " — safe next to the live server. Identical to the CLI "
            code { "mwe-mcp backup --out" } "."
        }
        @if let Some(report) = manual_report {
            table.config-table {
                tbody {
                    tr { td { "engine.db copy" } td { (human_bytes(report.db_bytes)) } }
                    tr { td { "Files copied" } td { (report.files_copied) } }
                    tr { td { "Bytes copied" } td { (human_bytes(report.bytes_copied)) } }
                    @if !report.skipped.is_empty() {
                        tr {
                            td { "Left out (unreadable)" }
                            td.flash-error {
                                @for p in &report.skipped {
                                    code { (p.display().to_string()) } " "
                                }
                            }
                        }
                    }
                }
            }
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

        // ---- snapshots on disk ----
        h3 { "Snapshots on disk" }
        @if ctx.snapshots.is_empty() {
            p.muted { "No snapshots in " code { (ctx.snapshots_dir.display()) } " yet." }
        } @else {
            p.muted {
                "Contents of " code { (ctx.snapshots_dir.display()) } ". "
                strong { "Restore" } " stages the workdir to be replaced by the "
                "chosen snapshot at the next server start (safety snapshot "
                "first, cancellable until then)."
            }
            table.config-table {
                thead { tr {
                    th { "Snapshot" }
                    th { "Kind" }
                    th { "Taken" }
                    th { "Size" }
                    th { "Files" }
                    th { "Actions" }
                } }
                tbody {
                    @for s in &ctx.snapshots {
                        tr {
                            td { code { (s.name) } }
                            td { (s.kind.label()) }
                            td { (s.taken_at.map(fmt_time).unwrap_or_default()) }
                            td { (human_bytes(s.bytes)) }
                            // Size cannot tell a snapshot from a husk —
                            // the DB is written first and is most of the
                            // weight, so a run that captured nothing but
                            // the database still looks the right size.
                            // The count is what shows it.
                            td {
                                (s.files)
                                @if s.files <= 1 {
                                    " " span.flash-error { "DB only — no tree" }
                                }
                            }
                            td {
                                form action="/dashboard/admin/backup/restore" method="post" class="inline-form"
                                    onsubmit="return confirm('Stage a restore from this snapshot? It replaces the whole workdir at the next server start.')" {
                                    input type="hidden" name="name" value=(s.name);
                                    button type="submit" { "Restore…" }
                                }
                                " "
                                form action="/dashboard/admin/backup/delete" method="post" class="inline-form"
                                    onsubmit="return confirm('Delete this snapshot from disk?')" {
                                    input type="hidden" name="name" value=(s.name);
                                    button type="submit" class="danger" { "Delete" }
                                }
                            }
                        }
                    }
                }
            }
        }

        // ---- memory reset ----
        h3 { "Memory reset" }
        p.flash.flash-error {
            strong { "Danger zone." } " Wipes every memory: facts, wikis, media, "
            "captures, proposals, recall history, the training spool. Keeps the "
            "installation: accounts, enrollment, consumers and their tokens, "
            "2FA, OAuth state, custom skills, config, env and prompt overrides. "
            "Identity wikis are re-scaffolded empty and every user goes through "
            "the welcome wizard again. Staged like a restore: it applies at the "
            "next server start, after an automatic safety snapshot."
        }
        form action="/dashboard/admin/backup/reset" method="post" {
            (components::text_field(
                "confirm",
                &format!("Type {RESET_CONFIRM_PHRASE} to confirm"),
                "text",
                "",
                true,
            ))
            p { button type="submit" class="danger" { "Stage memory reset" } }
        }
    };
    layout::authenticated_page(chrome, "Backup", session, &body)
}

/// A suggested manual destination inside the snapshots home:
/// `manual-<timestamp>` — guaranteed fresh (so empty) and outside the
/// workdir, and listed with the right provenance badge afterwards.
fn suggested_dest(snapshots_dir: &Path) -> String {
    snapshots_dir
        .join(backup::snapshot_name("manual"))
        .to_string_lossy()
        .into_owned()
}

/// Compact display time for the listing.
fn fmt_time(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%d %H:%M UTC")
        .to_string()
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

/// Workdir root — only present on the full `mwe-mcp serve` build, where
/// the memory handles carry the workdir.
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
    fn suggested_dest_lives_in_the_snapshots_home() {
        let home = Path::new("/srv/mwe/work-snapshots");
        let s = suggested_dest(home);
        let dest = Path::new(&s);
        assert_eq!(dest.parent(), Some(home), "{s}");
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(name.starts_with("manual-"), "got: {s}");
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn parse_settings_defaults_and_overrides() {
        let empty = HashMap::new();
        let cfg = parse_settings(&empty).unwrap();
        assert_eq!(cfg, BackupConfig::default());

        let mut form = HashMap::new();
        form.insert("mode".to_owned(), "disabled".to_owned());
        form.insert("interval_secs".to_owned(), "3600".to_owned());
        form.insert("retention_auto".to_owned(), "3".to_owned());
        form.insert("dir".to_owned(), " /mnt/backups ".to_owned());
        let cfg = parse_settings(&form).unwrap();
        assert_eq!(cfg.mode, BackupScheduleMode::Disabled);
        assert_eq!(cfg.interval_secs, 3600);
        assert_eq!(cfg.retention_auto, 3);
        assert_eq!(cfg.dir.as_deref(), Some(Path::new("/mnt/backups")));

        let mut bad = HashMap::new();
        bad.insert("interval_secs".to_owned(), "sometimes".to_owned());
        assert!(parse_settings(&bad).is_err());

        // The interval floor keeps a typo from hammering VACUUM INTO.
        let mut tiny = HashMap::new();
        tiny.insert("interval_secs".to_owned(), "1".to_owned());
        assert_eq!(parse_settings(&tiny).unwrap().interval_secs, 60);
    }
}
