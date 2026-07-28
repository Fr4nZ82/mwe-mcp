// SPDX-License-Identifier: AGPL-3.0-or-later
//! Self-service "change your own password" page, plus the admin-only
//! sections.
//!
//! `/dashboard/settings/me` — reachable by every signed-in user
//! (admin or regular). Requires the current password as a confirmation
//! step so an attacker who stole a session cookie cannot quietly lock
//! the legitimate user out.
//!
//! For an admin the page also carries the deployment-wide require-2FA
//! toggle, the **Admin reveal** checkbox — the single, dashboard-wide
//! ACL-bypass switch (see [`crate::reveal`]) — the Email (SMTP) editor
//! ([`super::email_settings`]), and the server-settings sections for
//! the YAML config that has no page of its own — ingest timezone,
//! dream cadence, logging, document pipeline
//! ([`super::server_settings`]; both modules own their POST endpoints).
//! `POST /dashboard/settings/reveal` flips the cookie and returns here.

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use maud::{Markup, html};
use mwe_core::config::Config;
use serde::Deserialize;

use crate::auth::SessionUser;
use crate::auth::password;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Where the reveal toggle redirects back to.
const SETTINGS_RETURN_TO: &str = "/dashboard/settings/me";

pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/settings/me", get(page).post(submit))
        .route("/settings/reveal", post(crate::reveal::toggle))
        .route("/settings/require-2fa", post(toggle_global_require))
}

/// Load the deployment-wide require-2FA toggle (admins only — non-admins
/// never see the section).
async fn admin_global_require(state: &DashboardState, user: &SessionUser) -> Result<bool> {
    if user.is_admin {
        crate::twofa::global_required(&state.pool).await
    } else {
        Ok(false)
    }
}

/// The whole raw YAML config for the embedded admin editors (SMTP +
/// the server-settings sections). `None` for non-admins, and when the
/// server runs without memory handles (no workdir to read the YAML
/// from — e.g. auth-only test routers).
fn admin_config(state: &DashboardState, user: &SessionUser) -> Result<Option<Config>> {
    let Some(workdir) = state.memory.as_ref().map(|m| &m.workdir) else {
        return Ok(None);
    };
    if !user.is_admin {
        return Ok(None);
    }
    let cfg = Config::load_raw(workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    Ok(Some(cfg))
}

/// Build the whole Settings page for `user`, loading every piece that
/// depends on runtime state (reveal cookie, global require-2FA, email
/// config). Shared with [`super::email_settings`], whose POST handlers
/// re-render this page with a flash.
pub(super) async fn render_page(
    state: &DashboardState,
    user: &SessionUser,
    jar: &CookieJar,
    error: Option<&str>,
    success: Option<&str>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(state);
    let reveal_on = crate::reveal::active(state, user, jar);
    let global_2fa = admin_global_require(state, user).await?;
    let cfg = admin_config(state, user)?;
    Ok(Html(render(
        chrome,
        user,
        reveal_on,
        state.config.admin_reveal_locked,
        global_2fa,
        cfg.as_ref(),
        error,
        success,
    )))
}

async fn page(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<Html<String>> {
    render_page(&state, &user, &jar, None, None).await
}

#[derive(Debug, Deserialize)]
pub struct GlobalRequireSubmission {
    /// Checkbox — present only when checked.
    #[serde(default)]
    pub require_2fa_all: Option<String>,
}

/// Admin-only: flip the deployment-wide "require 2FA for all non-system
/// users" toggle.
async fn toggle_global_require(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<GlobalRequireSubmission>,
) -> Result<Response> {
    let admin = user.require_admin()?;
    let on = form.require_2fa_all.is_some();
    crate::twofa::set_global_required(&state.pool, on).await?;
    tracing::info!(actor = %admin.sender_id(), enabled = on, "settings: global require-2FA toggled");
    let msg = if on {
        "Two-factor is now required for all users — they'll set it up at next sign-in."
    } else {
        "Two-factor is no longer required deployment-wide."
    };
    let body = render_page(&state, admin.session(), &jar, None, Some(msg)).await?;
    Ok(body.into_response())
}

#[derive(Debug, Deserialize)]
pub struct PasswordChangeSubmission {
    pub current_password: String,
    pub new_password: String,
    pub new_password_confirm: String,
}

async fn submit(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<PasswordChangeSubmission>,
) -> Result<Response> {
    // `render_page` reflects the reveal cookie's current state — a
    // password change leaves it untouched, so the admin checkbox does
    // not flip on re-render.
    let reject = async |msg: &str| -> Result<Response> {
        let body = render_page(&state, &user, &jar, Some(msg), None).await?;
        Ok(body.into_response())
    };

    let row: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM user_credentials WHERE user_id = ?")
            .bind(&user.sender_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(current_phc) = row else {
        // User exists in enrollment_users but has no credentials row.
        // System identities (bots) fall here; they cannot use the
        // self-service password change.
        return reject("Your account has no password (system identity?).").await;
    };

    if !password::verify(&form.current_password, &current_phc)? {
        return reject("Current password is incorrect.").await;
    }

    if form.new_password.len() < state.config.min_password_len {
        return reject(&format!(
            "New password must be at least {} characters.",
            state.config.min_password_len
        ))
        .await;
    }
    if form.new_password != form.new_password_confirm {
        return reject("The two new passwords do not match.").await;
    }
    if form.new_password == form.current_password {
        return reject("Pick a password different from the current one.").await;
    }

    let new_phc = password::hash(&form.new_password)?;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE user_credentials
            SET password_hash = ?, hashed_at = ?, must_change = 0
          WHERE user_id = ?",
    )
    .bind(&new_phc)
    .bind(&now)
    .bind(&user.sender_id)
    .execute(&state.pool)
    .await?;

    tracing::info!(user = %user.sender_id, "self-service password change");

    let body = render_page(&state, &user, &jar, None, Some("Password updated.")).await?;
    Ok(body.into_response())
}

/// The admin-only Admin reveal section: a short explainer + the
/// dashboard-wide ACL-bypass checkbox (and its banner when active).
///
/// `locked` is `instance.admin_reveal_locked` from the on-disk config:
/// the switch belongs to whoever runs the machine, so the explainer says
/// so whether or not it is currently exercised, and the control itself
/// gives way to [`crate::reveal::checkbox`]'s locked notice.
fn reveal_section(reveal_on: bool, locked: bool) -> Markup {
    html! {
        section.admin-reveal-settings {
            h2 { "Admin reveal" }
            @if reveal_on { (crate::reveal::banner()) }
            p.muted {
                "When on, the whole dashboard bypasses the per-user ACL projection: "
                "wiki pages show every fragment (highlighted), the "
                a href="/dashboard/facts" { "facts table" }
                " lists every user's facts, so the owner-or-admin ACL / validity / "
                "delete actions reach them, and the "
                a href="/dashboard/admin/recall-traces" { "recall traces" }
                " journal widens from your own recalls to everybody's. It never "
                "affects the MCP tool surface, and it is honoured only for admins. "
                "Leave it off unless you are actively supervising."
            }
            p.muted {
                "This switch can be taken away from the dashboard: setting "
                code { "instance.admin_reveal_locked" }
                " in " code { "mwe-mcp.config.yaml" }
                " turns reveal off for good, and no page here can undo it — only "
                "somebody with access to the machine can. Use it wherever "
                "administering the panel and running the server are not the same "
                "person: the admin keeps the deployment, the members keep their "
                "private fragments."
            }
            (crate::reveal::checkbox(reveal_on, locked, SETTINGS_RETURN_TO))
        }
    }
}

/// Admin-only deployment-wide require-2FA section.
fn global_require_section(on: bool) -> Markup {
    html! {
        section.admin-2fa-settings {
            h2 { "Require two-factor for everyone" }
            p.muted {
                "When on, every non-system user must set up two-factor authentication "
                "before they can use the dashboard — enforced at their next sign-in. "
                "System / bot identities (no password) are exempt. Turn this on before "
                "exposing the dashboard to a public user base."
            }
            form action="/dashboard/settings/require-2fa" method="post" {
                p {
                    label for="require_2fa_all" {
                        input id="require_2fa_all" name="require_2fa_all" type="checkbox"
                            value="1" checked[on];
                        " Require two-factor for all users"
                    }
                }
                (components::submit("Save"))
            }
        }
    }
}

fn render(
    chrome: layout::Chrome,
    user: &SessionUser,
    reveal_on: bool,
    reveal_locked: bool,
    global_2fa: bool,
    cfg: Option<&Config>,
    error: Option<&str>,
    success: Option<&str>,
) -> String {
    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        @if let Some(msg) = success {
            (components::flash("success", msg))
        }
        p.muted {
            "You are signed in as " strong { (user.sender_id) } "."
            @if !chrome.read_only {
                " Changing your password keeps you signed in on this session — "
                "other sessions remain valid until they expire or you revoke them."
            }
        }
        // A frozen deployment stores no new credential and mounts no 2FA
        // routes, so both are gone; the page stays because it is also
        // where an admin reads the reveal posture.
        @if chrome.read_only {
            (crate::read_only::notice())
        } @else {
            form action="/dashboard/settings/me" method="post" {
                // Hidden username anchor for browser password managers
                // (Chrome / Safari) so the new password gets associated
                // with the right account. Not submitted as a meaningful
                // field — the route reads the signed-in user from the
                // session cookie regardless.
                input
                    type="text"
                    name="username"
                    autocomplete="username"
                    value=(user.sender_id)
                    hidden;
                (components::password_field("current_password", "Current password", "current-password"))
                (components::password_field("new_password", "New password", "new-password"))
                (components::password_field("new_password_confirm", "Confirm new password", "new-password"))
                (components::submit("Update password"))
            }

            section.twofa-link {
                h2 { "Two-factor authentication" }
                p.muted {
                    "Add a one-time code from an authenticator app to your sign-in."
                }
                p { a href="/dashboard/settings/2fa" { "Manage two-factor authentication →" } }
            }
        }

        @if user.is_admin {
            @if !chrome.read_only {
                (global_require_section(global_2fa))
            }
            (reveal_section(reveal_on, reveal_locked))
            @if let Some(cfg) = cfg && !chrome.read_only {
                (super::email_settings::section(&cfg.email))
                (super::server_settings::sections(cfg))
            }
        }
    };
    layout::authenticated_reading_page(chrome, "Settings", user, &body)
}
