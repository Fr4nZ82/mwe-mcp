// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editors for the YAML config sections that previously had
//! **no dashboard surface**: the ingest timezone
//! (`recall.ingest_timezone`), the dream cadence (`rem.schedule`),
//! `logging`, and the `document` pipeline resources.
//!
//! No page of their own: like [`super::email_settings`], each renders
//! as an admin-only **section of the Settings page**
//! (`/dashboard/settings/me`, [`super::settings`]), which owns the GET.
//! This module keeps the section markup plus one POST endpoint per
//! section, all behind [`AdminUser`], all doing the same atomic save
//! (backup `.bak`, replace one section, atomic-write) and re-rendering
//! the Settings page with a flash.
//!
//! Apply semantics differ per section and each form says so: the
//! ingest timezone **hot-swaps** into the shared recall handle (next
//! ingest turn, both transports); dream cadence, logging, and the
//! document pipeline are read once at boot, so their saves apply **at
//! the next server restart** (the sibling of the Backup console's
//! `initial_delay_secs`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use mwe_core::config::{
    CONFIG_FILENAME, Config, DocumentConfig, LogFileRotation, LogLevel, LoggingConfig,
    RemScheduleConfig, RemScheduleMode,
};
use mwe_core::document::DocumentPolicy;
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::components;

/// Sub-router for the POST endpoints. Mounted inside the authenticated
/// tree, next to the Settings page that embeds the forms.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/settings/ingest-timezone", post(save_timezone))
        .route("/settings/dream-cadence", post(save_cadence))
        .route("/settings/logging", post(save_logging))
        .route("/settings/document", post(save_document))
}

/// Log levels: `(value, label)`.
const LOG_LEVELS: &[(&str, &str)] = &[
    ("info", "info — boundary events"),
    ("debug", "debug — plus internal step detail"),
];

/// File-rotation modes: `(value, label)`.
const ROTATIONS: &[(&str, &str)] = &[
    ("daily", "daily (default)"),
    ("hourly", "hourly"),
    ("never", "never — one growing file"),
    ("disabled", "disabled — stderr only, no file sink"),
];

/// The standard restart note under every boot-read section.
fn restart_note() -> Markup {
    html! {
        p.muted {
            "Saved to " code { (CONFIG_FILENAME) } " (with a " code { ".bak" }
            "); this section is read once at boot, so it applies at the "
            strong { "next server restart" } " — the "
            a href="/dashboard/admin/backup" { "Backup console" }
            " has a Restart button."
        }
    }
}

// ---------- section markup ----------

/// All four blocks, in the order the Settings page shows them.
pub(super) fn sections(cfg: &Config) -> Markup {
    html! {
        (timezone_section(cfg.recall.ingest_timezone.as_deref()))
        (cadence_section(&cfg.rem.schedule))
        (logging_section(&cfg.logging))
        (document_section(&cfg.document))
    }
}

fn timezone_section(current: Option<&str>) -> Markup {
    html! {
        section.ingest-timezone-settings {
            h2 { "Ingest timezone" }
            p.muted {
                "IANA timezone of the deployment's users (e.g. "
                code { "Europe/Rome" } ") — the " code { "recall.ingest_timezone" }
                " key. The ingest classifier stamps wall-clock times the user "
                "speaks (\"domani alle 9\") in this zone; unset, they are read "
                "as UTC. Hot: applies from the next ingest turn, no restart. "
                "The " code { "MWE_INGEST_TIMEZONE" } " env var is the "
                "fallback when this is unset."
            }
            form action="/dashboard/settings/ingest-timezone" method="post" {
                p {
                    label for="ingest_timezone" { "Timezone" }
                    input id="ingest_timezone" name="ingest_timezone" type="text"
                        value=(current.unwrap_or_default())
                        placeholder="Europe/Rome (empty = UTC)";
                }
                (components::submit("Save timezone"))
            }
        }
    }
}

fn cadence_section(cfg: &RemScheduleConfig) -> Markup {
    let def = RemScheduleConfig::default();
    let rows: &[(&str, &str, u64, u64, &str)] = &[
        (
            "interval_secs",
            "Full cycle — interval (seconds)",
            cfg.interval_secs,
            def.interval_secs,
            "Distance between nightly full REM cycles (strong-LLM reorganisation).",
        ),
        (
            "initial_delay_secs",
            "Full cycle — initial delay (seconds)",
            cfg.initial_delay_secs,
            def.initial_delay_secs,
            "Warm-up before the first full cycle after startup.",
        ),
        (
            "light_interval_secs",
            "Light dream — interval (seconds)",
            cfg.light_interval_secs,
            def.light_interval_secs,
            "Distance between light-dream runs (captures→facts promotion).",
        ),
        (
            "light_initial_delay_secs",
            "Light dream — initial delay (seconds)",
            cfg.light_initial_delay_secs,
            def.light_initial_delay_secs,
            "Warm-up before the first light dream after startup.",
        ),
    ];
    html! {
        section.dream-cadence-settings {
            h2 { "Dream cadence" }
            p.muted {
                "The " code { "rem.schedule:" } " section — when the dreams run. "
                "The behaviour knobs (what a cycle may touch) are the "
                a href="/dashboard/admin/rem-settings" { "REM settings" }
                "; the sub-jobs' model tiers are the "
                a href="/dashboard/admin/llm-config" { "LLM config" } "."
            }
            form action="/dashboard/settings/dream-cadence" method="post" {
                table.config-table {
                    tbody {
                        tr {
                            td { label for="mode" { "Mode" } }
                            td {
                                select id="mode" name="mode" {
                                    option value="interval"
                                        selected[matches!(cfg.mode, RemScheduleMode::Interval)]
                                        { "interval (on)" }
                                    option value="disabled"
                                        selected[matches!(cfg.mode, RemScheduleMode::Disabled)]
                                        { "disabled" }
                                }
                            }
                            td.muted {
                                "One switch for both schedulers: " code { "disabled" }
                                " turns off the full cycle and the light dream."
                            }
                        }
                        @for &(field, label, value, default, help) in rows {
                            tr {
                                td { label for=(field) { (label) } }
                                td {
                                    input id=(field) name=(field) type="number" min="0"
                                        value=(value) placeholder=(default);
                                }
                                td.muted { (help) }
                            }
                        }
                        tr {
                            td { label for="light_backlog_threshold" { "Light dream — backlog trigger" } }
                            td {
                                input id="light_backlog_threshold" name="light_backlog_threshold"
                                    type="number" min="0"
                                    value=(cfg.light_backlog_threshold)
                                    placeholder=(def.light_backlog_threshold);
                            }
                            td.muted {
                                "Buffered captures that fire a light dream ahead of the timer. "
                                "0 disables the early trigger."
                            }
                        }
                    }
                }
                p { button type="submit" { "Save dream cadence" } }
            }
            (restart_note())
        }
    }
}

fn logging_section(cfg: &LoggingConfig) -> Markup {
    html! {
        section.logging-settings {
            h2 { "Logging" }
            p.muted {
                "The " code { "logging:" } " section — verbosity and the rotating "
                "file sink under " code { "logs/" } ". Tracing always writes to "
                "stderr too."
            }
            form action="/dashboard/settings/logging" method="post" {
                table.config-table {
                    tbody {
                        tr {
                            td { label for="level" { "Level" } }
                            td {
                                select id="level" name="level" {
                                    @for &(val, lbl) in LOG_LEVELS {
                                        option value=(val) selected[level_value(cfg.level) == val] { (lbl) }
                                    }
                                }
                            }
                            td.muted {
                                code { "debug" } " adds dedup scores, watcher events, slow SQL — "
                                "for diagnosis, not steady state."
                            }
                        }
                        tr {
                            td { label for="file_rotation" { "File rotation" } }
                            td {
                                select id="file_rotation" name="file_rotation" {
                                    @for &(val, lbl) in ROTATIONS {
                                        option value=(val) selected[rotation_value(cfg.file_rotation) == val] { (lbl) }
                                    }
                                }
                            }
                            td.muted { "Cadence of the log-file roll." }
                        }
                        tr {
                            td { label for="file_path" { "File path" } }
                            td {
                                input id="file_path" name="file_path" type="text"
                                    value=(cfg.file_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default())
                                    placeholder="logs/mwe-mcp.log";
                            }
                            td.muted {
                                "Relative paths resolve against the workdir. Ignored when "
                                "rotation is " code { "disabled" } "."
                            }
                        }
                    }
                }
                p { button type="submit" { "Save logging settings" } }
            }
            (restart_note())
        }
    }
}

fn document_section(cfg: &DocumentConfig) -> Markup {
    let def = DocumentPolicy::default();
    let s = |v: Option<usize>| v.map(|x| x.to_string()).unwrap_or_default();
    let rows: &[(&str, &str, String, String, &str)] = &[
        (
            "poll_secs",
            "Worker poll (seconds)",
            cfg.poll_secs.map(|x| x.to_string()).unwrap_or_default(),
            def.poll_secs.to_string(),
            "How often the document worker checks the queue.",
        ),
        (
            "segment_target_chars",
            "Segment target (chars)",
            s(cfg.segment_target_chars),
            def.segment_target_chars.to_string(),
            "Preferred segment size the splitter aims for.",
        ),
        (
            "segment_max_chars",
            "Segment max (chars)",
            s(cfg.segment_max_chars),
            def.segment_max_chars.to_string(),
            "Hard cap per segment; a paragraph longer than this is split.",
        ),
        (
            "max_segments",
            "Max segments per document",
            s(cfg.max_segments),
            def.max_segments.to_string(),
            "Documents needing more are refused at enqueue.",
        ),
        (
            "max_facts_per_segment",
            "Max facts per segment",
            s(cfg.max_facts_per_segment),
            def.max_facts_per_segment.to_string(),
            "Extraction cap per segment (LLM output bound).",
        ),
        (
            "classify_sample_chars",
            "Classify sample (chars)",
            s(cfg.classify_sample_chars),
            def.classify_sample_chars.to_string(),
            "Head sample the classify step reads to type the document.",
        ),
        (
            "max_document_chars",
            "Max document size (chars)",
            s(cfg.max_document_chars),
            def.max_document_chars.to_string(),
            "Hard ceiling at enqueue; larger inputs are refused.",
        ),
    ];
    html! {
        section.document-settings {
            h2 { "Document pipeline" }
            p.muted {
                "The " code { "document:" } " section — resource knobs of "
                code { "wiki_ingest_external" } " (segmenting, extraction caps, "
                "worker cadence). Empty = the built-in default (the placeholder)."
            }
            form action="/dashboard/settings/document" method="post" {
                table.config-table {
                    tbody {
                        @for (field, label, value, default, help) in rows {
                            tr {
                                td { label for=(field) { (label) } }
                                td {
                                    input id=(field) name=(field) type="number" min="0"
                                        value=(value) placeholder=(default);
                                }
                                td.muted { (help) }
                            }
                        }
                        tr {
                            td { label for="merge_threshold" { "Merge threshold (0–1)" } }
                            td {
                                input id="merge_threshold" name="merge_threshold" type="number"
                                    min="0" max="1" step="0.01"
                                    value=(cfg.merge_threshold.map(|x| x.to_string()).unwrap_or_default())
                                    placeholder=(def.merge_threshold);
                            }
                            td.muted {
                                "Embedding similarity above which two extracted facts merge."
                            }
                        }
                    }
                }
                p { button type="submit" { "Save document settings" } }
            }
            (restart_note())
        }
    }
}

/// Wire value of a [`LogLevel`] (the serde `lowercase` names).
const fn level_value(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
    }
}

/// Wire value of a [`LogFileRotation`].
const fn rotation_value(r: LogFileRotation) -> &'static str {
    match r {
        LogFileRotation::Daily => "daily",
        LogFileRotation::Hourly => "hourly",
        LogFileRotation::Never => "never",
        LogFileRotation::Disabled => "disabled",
    }
}

// ---------- POST handlers ----------

async fn save_timezone(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let tz = parse_timezone(&form)?;

    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.recall.ingest_timezone = tz.clone();
    write_config(&workdir, &cfg)?;
    // Hot-swap the shared recall handle (disk first, then swap) so the
    // next ingest turn — both transports — sees the new zone.
    state.replace_recall(cfg.recall.clone());

    tracing::info!(
        admin = %admin.session().sender_id,
        timezone = tz.as_deref().unwrap_or("(unset)"),
        "server-settings: ingest timezone saved (hot-reloaded)"
    );
    let msg = tz.map_or_else(
        || "Ingest timezone unset — wall-clock times are read as UTC again.".to_owned(),
        |t| format!("Ingest timezone set to {t} — applies from the next ingest turn."),
    );
    let body =
        super::settings::render_page(&state, admin.session(), &jar, None, Some(&msg)).await?;
    Ok(body.into_response())
}

async fn save_cadence(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_cadence(&form)?;

    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.rem.schedule = parsed;
    write_config(&workdir, &cfg)?;

    tracing::info!(
        admin = %admin.session().sender_id,
        "server-settings: dream cadence saved (applies at next restart)"
    );
    let body = super::settings::render_page(
        &state,
        admin.session(),
        &jar,
        None,
        Some("Dream cadence saved — it applies at the next server restart."),
    )
    .await?;
    Ok(body.into_response())
}

async fn save_logging(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_logging(&form)?;

    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.logging = parsed;
    write_config(&workdir, &cfg)?;

    tracing::info!(
        admin = %admin.session().sender_id,
        "server-settings: logging saved (applies at next restart)"
    );
    let body = super::settings::render_page(
        &state,
        admin.session(),
        &jar,
        None,
        Some("Logging settings saved — they apply at the next server restart."),
    )
    .await?;
    Ok(body.into_response())
}

async fn save_document(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_document(&form)?;

    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load_raw(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.document = parsed;
    write_config(&workdir, &cfg)?;

    tracing::info!(
        admin = %admin.session().sender_id,
        "server-settings: document pipeline saved (applies at next restart)"
    );
    let body = super::settings::render_page(
        &state,
        admin.session(),
        &jar,
        None,
        Some("Document-pipeline settings saved — they apply at the next server restart."),
    )
    .await?;
    Ok(body.into_response())
}

// ---------- form parsing ----------

/// `ingest_timezone` field: trimmed; empty → unset. Light sanity only —
/// the engine passes the string to the classifier prompt verbatim, so
/// the gate is "obviously not a timezone", not full IANA validation.
fn parse_timezone(form: &HashMap<String, String>) -> Result<Option<String>> {
    let raw = form
        .get("ingest_timezone")
        .map(|s| s.trim())
        .unwrap_or_default();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > 64 || raw.chars().any(char::is_whitespace) {
        return Err(DashboardError::Validation(
            "`ingest_timezone` must be a single IANA name like Europe/Rome".to_owned(),
        ));
    }
    Ok(Some(raw.to_owned()))
}

/// Decode the cadence form into a fresh [`RemScheduleConfig`]. Empty
/// numeric fields keep the Rust defaults (the placeholders).
fn parse_cadence(form: &HashMap<String, String>) -> Result<RemScheduleConfig> {
    let def = RemScheduleConfig::default();
    let mode = match form.get("mode").map(String::as_str) {
        Some("interval") | None => RemScheduleMode::Interval,
        Some("disabled") => RemScheduleMode::Disabled,
        Some(other) => {
            return Err(DashboardError::Validation(format!(
                "`mode` must be `interval` or `disabled`, got {other:?}"
            )));
        },
    };
    let backlog = match form
        .get("light_backlog_threshold")
        .map(|s| s.trim())
        .unwrap_or_default()
    {
        "" => def.light_backlog_threshold,
        raw => raw.parse::<i64>().ok().filter(|n| *n >= 0).ok_or_else(|| {
            DashboardError::Validation(
                "`light_backlog_threshold` must be a non-negative integer".to_owned(),
            )
        })?,
    };
    Ok(RemScheduleConfig {
        mode,
        interval_secs: parse_u64_or(form, "interval_secs", def.interval_secs)?,
        initial_delay_secs: parse_u64_or(form, "initial_delay_secs", def.initial_delay_secs)?,
        light_interval_secs: parse_u64_or(form, "light_interval_secs", def.light_interval_secs)?,
        light_initial_delay_secs: parse_u64_or(
            form,
            "light_initial_delay_secs",
            def.light_initial_delay_secs,
        )?,
        light_backlog_threshold: backlog,
    })
}

/// Decode the logging form into a fresh [`LoggingConfig`].
fn parse_logging(form: &HashMap<String, String>) -> Result<LoggingConfig> {
    let level = match form.get("level").map(String::as_str) {
        Some("info") | None => LogLevel::Info,
        Some("debug") => LogLevel::Debug,
        Some(other) => {
            return Err(DashboardError::Validation(format!(
                "`level` must be `info` or `debug`, got {other:?}"
            )));
        },
    };
    let file_rotation = match form.get("file_rotation").map(String::as_str) {
        Some("daily") | None => LogFileRotation::Daily,
        Some("hourly") => LogFileRotation::Hourly,
        Some("never") => LogFileRotation::Never,
        Some("disabled") => LogFileRotation::Disabled,
        Some(other) => {
            return Err(DashboardError::Validation(format!(
                "unknown `file_rotation` {other:?} (daily / hourly / never / disabled)"
            )));
        },
    };
    let file_path = form
        .get("file_path")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    Ok(LoggingConfig {
        level,
        file_rotation,
        file_path,
    })
}

/// Decode the document form into a fresh [`DocumentConfig`]. Empty
/// fields stay `None` (keep the built-in default).
fn parse_document(form: &HashMap<String, String>) -> Result<DocumentConfig> {
    let merge_threshold = match form
        .get("merge_threshold")
        .map(|s| s.trim())
        .unwrap_or_default()
    {
        "" => None,
        raw => Some(
            raw.parse::<f32>()
                .ok()
                .filter(|v| (0.0..=1.0).contains(v))
                .ok_or_else(|| {
                    DashboardError::Validation(
                        "`merge_threshold` must be a number between 0 and 1".to_owned(),
                    )
                })?,
        ),
    };
    Ok(DocumentConfig {
        poll_secs: parse_opt(form, "poll_secs")?,
        segment_target_chars: parse_opt(form, "segment_target_chars")?,
        segment_max_chars: parse_opt(form, "segment_max_chars")?,
        max_segments: parse_opt(form, "max_segments")?,
        max_facts_per_segment: parse_opt(form, "max_facts_per_segment")?,
        classify_sample_chars: parse_opt(form, "classify_sample_chars")?,
        merge_threshold,
        max_document_chars: parse_opt(form, "max_document_chars")?,
    })
}

/// Numeric field, empty → the given default.
fn parse_u64_or(form: &HashMap<String, String>, field: &'static str, default: u64) -> Result<u64> {
    let raw = form.get(field).map(|s| s.trim()).unwrap_or_default();
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<u64>().map_err(|_| {
        DashboardError::Validation(format!("`{field}` must be a non-negative integer"))
    })
}

/// Numeric field, empty → `None` (keep the built-in default).
fn parse_opt<T: std::str::FromStr>(
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

// ---------- IO helpers ----------

/// Atomic write of `cfg` back to `<workdir>/mwe-mcp.config.yaml`,
/// backing up the prior file to `.bak` first. Same shape as the email
/// and REM editors.
fn write_config(workdir: &Path, cfg: &Config) -> Result<()> {
    let path = workdir.join(CONFIG_FILENAME);
    let backup = {
        let mut s = path.as_os_str().to_owned();
        s.push(".bak");
        PathBuf::from(s)
    };
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&backup, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }
    let yaml = serde_yaml::to_string(cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;
    Ok(())
}

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

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn timezone_empty_unsets_and_garbage_rejected() {
        assert_eq!(parse_timezone(&form(&[])).unwrap(), None);
        assert_eq!(
            parse_timezone(&form(&[("ingest_timezone", " Europe/Rome ")])).unwrap(),
            Some("Europe/Rome".to_owned())
        );
        assert!(parse_timezone(&form(&[("ingest_timezone", "not a zone")])).is_err());
    }

    #[test]
    fn cadence_defaults_and_overrides() {
        let c = parse_cadence(&form(&[])).unwrap();
        assert_eq!(c, RemScheduleConfig::default());

        let c = parse_cadence(&form(&[
            ("mode", "disabled"),
            ("interval_secs", "43200"),
            ("light_backlog_threshold", "0"),
        ]))
        .unwrap();
        assert_eq!(c.mode, RemScheduleMode::Disabled);
        assert_eq!(c.interval_secs, 43_200);
        assert_eq!(c.light_backlog_threshold, 0);
        assert_eq!(
            c.light_interval_secs,
            RemScheduleConfig::default().light_interval_secs
        );

        assert!(parse_cadence(&form(&[("light_backlog_threshold", "-3")])).is_err());
        assert!(parse_cadence(&form(&[("mode", "cron")])).is_err());
    }

    #[test]
    fn logging_parses_enums_and_rejects_unknown() {
        let l = parse_logging(&form(&[])).unwrap();
        assert_eq!(l, LoggingConfig::default());

        let l = parse_logging(&form(&[
            ("level", "debug"),
            ("file_rotation", "disabled"),
            ("file_path", "/var/log/mwe.log"),
        ]))
        .unwrap();
        assert!(matches!(l.level, LogLevel::Debug));
        assert!(matches!(l.file_rotation, LogFileRotation::Disabled));
        assert_eq!(l.file_path.as_deref(), Some(Path::new("/var/log/mwe.log")));

        assert!(parse_logging(&form(&[("level", "chatty")])).is_err());
        assert!(parse_logging(&form(&[("file_rotation", "weekly")])).is_err());
    }

    #[test]
    fn document_empty_keeps_defaults_and_threshold_bounded() {
        let d = parse_document(&form(&[])).unwrap();
        assert_eq!(d, DocumentConfig::default());

        let d = parse_document(&form(&[
            ("max_segments", "100"),
            ("merge_threshold", "0.85"),
        ]))
        .unwrap();
        assert_eq!(d.max_segments, Some(100));
        assert_eq!(d.merge_threshold, Some(0.85));

        assert!(parse_document(&form(&[("merge_threshold", "1.5")])).is_err());
        assert!(parse_document(&form(&[("poll_secs", "sometimes")])).is_err());
    }
}
