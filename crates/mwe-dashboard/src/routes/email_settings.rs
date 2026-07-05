// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only editor for `<workdir>/mwe-mcp.config.yaml > email` — the
//! SMTP backend that powers self-service password recovery (roadmap 28).
//!
//! No page of its own: the editor renders as an admin-only **section of
//! the Settings page** (`/dashboard/settings/me`, [`super::settings`]),
//! which owns the GET. This module keeps the section markup plus the two
//! POST endpoints, both behind [`AdminUser`]:
//!
//! - POST `/settings/email` — atomic save of the YAML (backup `.bak`,
//!   serialize, atomic-write), preserving every other section.
//! - POST `/settings/email/test` — send a test email to a typed address
//!   using the **saved** config, so a misconfigured backend (or a
//!   missing `MWE_SMTP_PASSWORD`) surfaces here rather than silently on
//!   the fire-and-forget recovery path.
//!
//! Both re-render the Settings page with a flash. The SMTP password is
//! never stored in the YAML; the editor only names the env-var
//! (`password_env`) that holds it — set in `mwe-mcp.env`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, EmailConfig};
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::components;

/// Sub-router for the email-settings POST endpoints. Mounted inside the
/// authenticated tree, next to the Settings page that embeds the form.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/settings/email", post(save))
        .route("/settings/email/test", post(send_test))
}

/// TLS modes: `(value, label)`.
const TLS_MODES: &[(&str, &str)] = &[
    ("starttls", "STARTTLS (port 587 — upgrade plaintext)"),
    ("implicit", "Implicit TLS / SMTPS (port 465)"),
    ("none", "None (plaintext — localhost / dev only)"),
];

// ---------- section markup ----------

/// The admin-only "Email (SMTP)" block the Settings page embeds.
pub(super) fn section(cfg: &EmailConfig) -> Markup {
    html! {
        section.email-settings {
            h2 { "Email (SMTP) settings" }
            p.muted {
                "The SMTP backend that sends self-service " strong { "password-recovery" }
                " links — the " code { "email:" } " section of " code { (CONFIG_FILENAME) }
                ". With it off (the default), the login page hides the "
                em { "Forgot your password?" } " link and the recovery route is inert."
            }

            @if cfg.enabled && !cfg.is_sendable() {
                (components::flash("error",
                    "Recovery is enabled but incomplete — set an SMTP host and a From address below."))
            }

            (email_form(cfg))

            h3 { "Send a test email" }
            p.muted {
                "Uses the " strong { "saved" } " settings above (Save first). This is how a "
                "wrong host, TLS mode, or missing " code { "MWE_SMTP_PASSWORD" }
                " surfaces — the recovery path itself is silent by design."
            }
            form action="/dashboard/settings/email/test" method="post" {
                p {
                    label for="test_to" { "Recipient" }
                    input id="test_to" name="test_to" type="email"
                        placeholder="you@example.com" required;
                }
                p { button type="submit" { "Send test email" } }
            }

            p.muted {
                "The SMTP password is never stored here — name the env-var holding it ("
                code { "password_env" } ", default " code { "MWE_SMTP_PASSWORD" }
                ") and set it in " code { "mwe-mcp.env" } "."
            }
        }
    }
}

/// The `<form>` block — split out of [`section`] to keep that function
/// within the line budget.
fn email_form(cfg: &EmailConfig) -> Markup {
    html! {
        form action="/dashboard/settings/email" method="post" {
            table.config-table {
                tbody {
                    tr {
                        td { label for="enabled" { "Enable recovery" } }
                        td {
                            input id="enabled" name="enabled" type="checkbox"
                                value="1" checked[cfg.enabled];
                        }
                        td.muted { "Master switch for the forgot-password flow." }
                    }
                    tr {
                        td { label for="smtp_host" { "SMTP host" } }
                        td {
                            input id="smtp_host" name="smtp_host" type="text"
                                value=(cfg.smtp_host.clone().unwrap_or_default())
                                placeholder="smtp.fastmail.com";
                        }
                        td.muted { "The relay hostname." }
                    }
                    tr {
                        td { label for="smtp_port" { "SMTP port" } }
                        td {
                            input id="smtp_port" name="smtp_port" type="number" min="1" max="65535"
                                value=(cfg.smtp_port.to_string()) placeholder="587";
                        }
                        td.muted { "587 = STARTTLS · 465 = implicit TLS · 25 = plaintext." }
                    }
                    tr {
                        td { label for="tls" { "TLS mode" } }
                        td {
                            select id="tls" name="tls" {
                                @for &(val, lbl) in TLS_MODES {
                                    option value=(val) selected[cfg.tls.as_str() == val] { (lbl) }
                                }
                            }
                        }
                        td.muted { "Match the port your provider expects." }
                    }
                    tr {
                        td { label for="from_address" { "From address" } }
                        td {
                            input id="from_address" name="from_address" type="email"
                                value=(cfg.from_address.clone().unwrap_or_default())
                                placeholder="noreply@example.com";
                        }
                        td.muted { "The envelope / From address recovery mail is sent from." }
                    }
                    tr {
                        td { label for="from_name" { "From name (optional)" } }
                        td {
                            input id="from_name" name="from_name" type="text"
                                value=(cfg.from_name.clone().unwrap_or_default())
                                placeholder="mwe-mcp";
                        }
                        td.muted { "Display name on the From header." }
                    }
                    tr {
                        td { label for="username" { "SMTP username (optional)" } }
                        td {
                            input id="username" name="username" type="text"
                                value=(cfg.username.clone().unwrap_or_default())
                                autocomplete="off"
                                placeholder="(blank → no SMTP auth)";
                        }
                        td.muted { "Leave blank for an unauthenticated localhost MTA." }
                    }
                    tr {
                        td { label for="password_env" { "Password env-var" } }
                        td {
                            input id="password_env" name="password_env" type="text"
                                value=(cfg.password_env) placeholder="MWE_SMTP_PASSWORD";
                        }
                        td.muted {
                            "Name of the env-var (in " code { "mwe-mcp.env" }
                            ") holding the SMTP password. Never the YAML."
                        }
                    }
                    tr {
                        td { label for="public_base_url" { "Public base URL (optional)" } }
                        td {
                            input id="public_base_url" name="public_base_url" type="text"
                                value=(cfg.public_base_url.clone().unwrap_or_default())
                                placeholder="https://mwe.example.com";
                        }
                        td.muted {
                            "Origin used to build the reset link in the email. Blank → derived "
                            "from the request host."
                        }
                    }
                }
            }
            p { button type="submit" { "Save email settings" } }
        }
    }
}

// ---------- POST save ----------

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let parsed = parse_form(&form)?;
    let enabled = parsed.enabled;

    // Preserve every non-email section by re-loading from disk and
    // replacing only the `email` field.
    let workdir = workdir_of(&state)?;
    let mut cfg = Config::load(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.email = parsed;

    write_config(&workdir, &cfg)?;

    tracing::info!(
        admin = %admin.session().sender_id,
        enabled,
        "email-settings: saved from dashboard"
    );

    let msg = if enabled {
        "Email settings saved. Send a test email below to confirm the backend works."
    } else {
        "Email settings saved (recovery disabled)."
    };
    let body = super::settings::render_page(&state, admin.session(), &jar, None, Some(msg)).await?;
    Ok(body.into_response())
}

// ---------- POST test ----------

async fn send_test(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let to = form
        .get("test_to")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| DashboardError::Validation("enter a recipient address".to_owned()))?;

    let workdir = workdir_of(&state)?;
    let cfg = Config::load(&workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;

    let result = crate::email::send_test_email(&cfg.email, &to).await;
    let ok = result.is_ok();

    let (error, success) = match result {
        Ok(()) => (None, Some(format!("Test email sent to {to}."))),
        Err(e) => (Some(format!("Test email failed: {e}")), None),
    };
    tracing::info!(admin = %admin.session().sender_id, to = %to, ok,
        "email-settings: test send");

    let body = super::settings::render_page(
        &state,
        admin.session(),
        &jar,
        error.as_deref(),
        success.as_deref(),
    )
    .await?;
    Ok(body.into_response())
}

// ---------- form parse + IO helpers ----------

/// Decode the flat form into an [`EmailConfig`], starting from the Rust
/// defaults so an empty field keeps the default.
fn parse_form(form: &HashMap<String, String>) -> Result<EmailConfig> {
    let val = |k: &str| {
        form.get(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };

    let d = EmailConfig::default();
    let smtp_port = match val("smtp_port") {
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| DashboardError::Validation("`smtp_port` must be 1..=65535".to_owned()))?,
        None => d.smtp_port,
    };
    let tls = match val("tls") {
        Some(t) => {
            if !["starttls", "implicit", "none"].contains(&t.as_str()) {
                return Err(DashboardError::Validation(format!(
                    "unknown TLS mode `{t}` (expected starttls / implicit / none)"
                )));
            }
            t
        },
        None => d.tls,
    };
    Ok(EmailConfig {
        // An unchecked checkbox is simply absent from the body.
        enabled: form.contains_key("enabled"),
        smtp_host: val("smtp_host"),
        smtp_port,
        tls,
        from_address: val("from_address"),
        from_name: val("from_name"),
        username: val("username"),
        password_env: val("password_env").unwrap_or(d.password_env),
        public_base_url: val("public_base_url"),
    })
}

/// Atomic write of `cfg` back to `<workdir>/mwe-mcp.config.yaml`, backing
/// up the prior file to `.bak` first. Shared shape with the embedding
/// editor's save.
fn write_config(workdir: &Path, cfg: &Config) -> Result<()> {
    let path = workdir.join(CONFIG_FILENAME);
    let backup = backup_path_for(&path);
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

fn backup_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
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
    fn empty_form_is_disabled_defaults() {
        let e = parse_form(&form(&[])).expect("parse");
        assert_eq!(e, EmailConfig::default());
        assert!(!e.enabled);
        assert_eq!(e.smtp_port, 587);
        assert_eq!(e.tls, "starttls");
        assert_eq!(e.password_env, "MWE_SMTP_PASSWORD");
    }

    #[test]
    fn checkbox_presence_enables() {
        let e = parse_form(&form(&[
            ("enabled", "1"),
            ("smtp_host", "smtp.example.com"),
            ("from_address", "noreply@example.com"),
            ("smtp_port", "465"),
            ("tls", "implicit"),
        ]))
        .expect("parse");
        assert!(e.enabled);
        assert!(e.is_sendable());
        assert_eq!(e.smtp_port, 465);
        assert_eq!(e.tls, "implicit");
    }

    #[test]
    fn unknown_tls_is_rejected() {
        assert!(matches!(
            parse_form(&form(&[("tls", "ssl-maybe")])),
            Err(DashboardError::Validation(_))
        ));
    }

    #[test]
    fn bad_port_is_rejected() {
        assert!(matches!(
            parse_form(&form(&[("smtp_port", "99999")])),
            Err(DashboardError::Validation(_))
        ));
    }

    #[test]
    fn enabled_without_host_is_not_sendable() {
        let e = parse_form(&form(&[("enabled", "1")])).expect("parse");
        assert!(e.enabled);
        assert!(!e.is_sendable());
    }
}
