// SPDX-License-Identifier: AGPL-3.0-or-later
//! SMTP sender for self-service password recovery (roadmap 28).
//!
//! The transport is built per-send from the [`EmailConfig`] section of
//! `mwe-mcp.config.yaml` (the admin edits it in the Email section of
//! `/dashboard/settings/me`).
//! rustls only — no OpenSSL (project dependency policy) — over the tokio executor.
//!
//! The SMTP password never lives in the YAML: `password_env` names an
//! env-var (default `MWE_SMTP_PASSWORD`) read from the process
//! environment here at send time, exactly like the cloud LLM keys.
//!
//! Three entry points: [`send_recovery_email`] (the forgot-password
//! flow), [`send_invitation_email`] (the admin's create-user / reinvite
//! action, when SMTP is configured), and [`send_test_email`] (the admin
//! editor's "send test email" button, which is how a misconfigured
//! backend surfaces). The recovery and invitation sends are
//! fire-and-forget at the call site — a slow relay never stalls the
//! request, and the raw link stays the source of truth if delivery fails.
//!
//! [`email_cfg`] loads the `email:` section from the workdir and
//! [`origin_of`] resolves the public origin for the links carried in
//! these messages; both are shared by every caller that mints one.

use axum::http::HeaderMap;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncTransport, Message, Tokio1Executor};
use mwe_core::config::{Config, EmailConfig};

use crate::error::DashboardError;
use crate::state::DashboardState;

/// Build an async SMTP transport from the config, resolving the password
/// from `password_env` when a username is set.
fn build_transport(
    cfg: &EmailConfig,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, DashboardError> {
    let host = cfg.smtp_host.as_deref().ok_or_else(|| {
        DashboardError::Validation("email: smtp_host is not configured".to_owned())
    })?;

    let mut builder = match cfg.tls.as_str() {
        // Implicit TLS (SMTPS, usually port 465): TLS from the first byte.
        "implicit" => AsyncSmtpTransport::<Tokio1Executor>::relay(host)
            .map_err(|e| DashboardError::Internal(format!("smtp relay: {e}")))?,
        // Plaintext — dev / localhost MTA only. No TLS negotiated.
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        // STARTTLS (the default, usually port 587): upgrade an initially
        // plaintext connection. Any unknown value falls back here.
        _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
            .map_err(|e| DashboardError::Internal(format!("smtp starttls: {e}")))?,
    };

    builder = builder.port(cfg.smtp_port);

    if let Some(user) = cfg.username.as_deref() {
        let pass = std::env::var(&cfg.password_env).unwrap_or_default();
        builder = builder.credentials(Credentials::new(user.to_owned(), pass));
    }

    Ok(builder.build())
}

/// Parse the configured `From:` mailbox (`from_name <from_address>`).
fn from_mailbox(cfg: &EmailConfig) -> Result<Mailbox, DashboardError> {
    let addr = cfg.from_address.as_deref().ok_or_else(|| {
        DashboardError::Validation("email: from_address is not configured".to_owned())
    })?;
    let raw = match cfg.from_name.as_deref() {
        Some(name) if !name.trim().is_empty() => format!("{name} <{addr}>"),
        _ => addr.to_owned(),
    };
    raw.parse::<Mailbox>().map_err(|e| {
        DashboardError::Validation(format!("email: invalid from_address `{addr}`: {e}"))
    })
}

/// Compose + send one plain-text email through the configured transport.
async fn send(
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    body: String,
) -> Result<(), DashboardError> {
    let to_mbox = to
        .parse::<Mailbox>()
        .map_err(|e| DashboardError::Validation(format!("email: invalid recipient `{to}`: {e}")))?;

    let message = Message::builder()
        .from(from_mailbox(cfg)?)
        .to(to_mbox)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .map_err(|e| DashboardError::Internal(format!("email: compose: {e}")))?;

    let transport = build_transport(cfg)?;
    transport
        .send(message)
        .await
        .map_err(|e| DashboardError::Internal(format!("email: smtp send: {e}")))?;
    Ok(())
}

/// Send the password-reset email carrying the one-shot `reset_url`.
pub async fn send_recovery_email(
    cfg: &EmailConfig,
    to: &str,
    reset_url: &str,
) -> Result<(), DashboardError> {
    let body = format!(
        "Someone (hopefully you) asked to reset the password for your mwe-mcp account.\n\n\
         Open this link to choose a new password. It expires shortly and can be used only once:\n\n\
         {reset_url}\n\n\
         If you didn't request this, you can ignore this email — your password stays unchanged.\n"
    );
    send(cfg, to, "Reset your mwe-mcp password", body).await
}

/// Send the invitation email carrying the one-shot `accept_url`.
///
/// The admin's create-user / reinvite action; the same link the dashboard
/// shows as a backup, delivered so the admin need not hand it over
/// manually.
pub async fn send_invitation_email(
    cfg: &EmailConfig,
    to: &str,
    accept_url: &str,
) -> Result<(), DashboardError> {
    let body = format!(
        "You've been invited to mwe-mcp — your personal, structured memory.\n\n\
         Open this link to set your password and sign in. It can be used once and expires soon:\n\n\
         {accept_url}\n\n\
         If you weren't expecting this, you can ignore this email.\n"
    );
    send(cfg, to, "Your mwe-mcp invitation", body).await
}

/// Send a test email to `to` so the admin can validate the SMTP backend
/// from the dashboard editor.
pub async fn send_test_email(cfg: &EmailConfig, to: &str) -> Result<(), DashboardError> {
    let body = "This is a test email from mwe-mcp.\n\n\
         If you received it, the SMTP backend in the dashboard email settings is working and \
         self-service password recovery is ready.\n"
        .to_owned();
    send(cfg, to, "mwe-mcp SMTP test", body).await
}

/// Load the `email:` config; defaults (recovery/invite off) when the
/// server has no workdir handle (identity-console-only build) or the file
/// is absent.
#[must_use]
pub fn email_cfg(state: &DashboardState) -> EmailConfig {
    state
        .memory
        .as_ref()
        .map(|m| m.workdir.clone())
        .and_then(|wd| Config::load(&wd).ok())
        .map(|c| c.email)
        .unwrap_or_default()
}

/// Public origin for the link in an email: the configured override wins,
/// else derived from the request `Host` + forwarded scheme.
#[must_use]
pub fn origin_of(configured: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(base) = configured {
        return base.trim_end_matches('/').to_owned();
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8742");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}
