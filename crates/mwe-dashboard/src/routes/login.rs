// SPDX-License-Identifier: AGPL-3.0-or-later
//! Password login form + handler.
//!
//! Companion to [`crate::routes::setup`] for the bootstrap path and to
//! [`crate::routes::logout`] for ending a session. The form is served
//! to anyone (no auth required), but rejects with a generic
//! `"Invalid credentials"` flash on any failure mode (unknown user,
//! wrong password, missing `user_credentials` row for a system user)
//! so the visitor cannot enumerate accounts.
//!
//! Per the JWT & session model
//! the **email** is the only login field. The admin sets every user's
//! email when inviting them (the "Add user" form), so the email lives on
//! `enrollment_users` and the handler resolves it to the canonical
//! `user_id` via `SELECT user_id … WHERE enrollment_users.email = ?`,
//! then runs the password check. There is no username fallback: login is
//! email-only, and a user with no email simply cannot sign in until the
//! admin sets one.

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use crate::auth::{password, session::issue_session_cookie};
use crate::error::Result;
use crate::routes::redirect::admin_exists;
use crate::routes::welcome::user_already_initialized;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Query string of `GET /dashboard/login`. `next` lets a deep-link flow (the
/// `webagentoauth` OAuth consent) bounce through login and return to where it
/// started; it is honoured only when it is a local `/dashboard/` path.
#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    #[serde(default)]
    pub next: Option<String>,
}

/// GET `/dashboard/login`.
///
/// If no admin exists yet, redirects to the setup wizard so a fresh
/// deployment cannot get stuck on a login form for an account that
/// does not exist.
pub async fn form(
    State(state): State<DashboardState>,
    Query(q): Query<LoginQuery>,
) -> Result<Response> {
    if !admin_exists(&state).await? {
        return Ok(Redirect::to("/dashboard/setup").into_response());
    }
    Ok(render_form(None, "", q.next.as_deref()).into_response())
}

#[derive(Debug, Deserialize)]
pub struct LoginSubmission {
    /// The login email. The only accepted identifier — there is no
    /// username fallback (see the module doc).
    pub email: String,
    pub password: String,
    /// Post-login redirect target carried through from the GET form's
    /// `?next=`. Honoured only when local (see [`safe_next`]).
    #[serde(default)]
    pub next: Option<String>,
}

/// Accept a post-login redirect target only when it is a local `/dashboard/`
/// path, so `?next=` can never be turned into an open redirect.
fn safe_next(next: Option<&str>) -> Option<String> {
    let n = next?.trim();
    if n.starts_with("/dashboard/") && !n.contains("://") {
        Some(n.to_owned())
    } else {
        None
    }
}

/// POST `/dashboard/login`. On success mints a session cookie and
/// 303-redirects to `/dashboard/home`.
pub async fn submit(
    State(state): State<DashboardState>,
    jar: CookieJar,
    axum::Form(form): axum::Form<LoginSubmission>,
) -> Result<Response> {
    let identifier = form.email.trim();
    let password = form.password.as_str();
    let generic = || {
        render_form(
            Some("Invalid credentials."),
            identifier,
            form.next.as_deref(),
        )
        .into_response()
    };

    if identifier.is_empty() || password.is_empty() {
        return Ok(generic());
    }

    // Email-only lookup. The email lives on `enrollment_users` (the row
    // born at invite, where the admin sets it) and is joined to the
    // credential that carries the password hash. `idx_enrollment_users_email`
    // is UNIQUE so this returns at most one row; `LIMIT 1` is belt-and-braces.
    // No username fallback — a user with no email cannot sign in until the
    // admin sets one. The generic flash on any miss keeps the failure mode
    // opaque (unknown email vs. credential-less account look identical).
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT c.user_id, c.password_hash, u.is_admin
           FROM enrollment_users u
           JOIN user_credentials c ON c.user_id = u.user_id
          WHERE u.email = ?
          LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(&state.pool)
    .await?;

    let Some((user_id, phc, is_admin_raw)) = row else {
        // Unknown email, or known user with no credentials (system
        // identity for a consumer token) — same opaque flash.
        return Ok(generic());
    };

    if !password::verify(password, &phc)? {
        return Ok(generic());
    }

    let is_admin = is_admin_raw != 0;

    // Second-factor gate: when the user has 2FA enabled, the password is
    // only the first factor — hand off to the challenge (which mints the
    // session once the TOTP / recovery code passes) instead of issuing a
    // session here. See `two_factor`.
    if crate::twofa::is_enabled(&state.pool, &user_id).await? {
        let next = safe_next(form.next.as_deref());
        return super::two_factor::begin_challenge(
            &state,
            jar,
            &user_id,
            is_admin,
            next.as_deref(),
        )
        .await;
    }

    let cookie = issue_session_cookie(&state, &user_id, is_admin)?;
    // A local `next` (e.g. the OAuth consent deep-link) wins; otherwise the
    // profile-wizard gate sends never-initialised users to `/welcome` instead
    // of straight home. See `welcome.rs` for the flag semantics.
    let destination = match safe_next(form.next.as_deref()) {
        Some(n) => n,
        None if user_already_initialized(&state, &user_id).await? => "/dashboard/home".to_owned(),
        None => "/dashboard/welcome".to_owned(),
    };
    Ok((jar.add(cookie), Redirect::to(&destination)).into_response())
}

fn render_form(error: Option<&str>, email: &str, next: Option<&str>) -> Html<String> {
    let body = maud::html! {
        @if let Some(msg) = error { (components::flash("error", msg)) }
        form action="/dashboard/login" method="post" {
            (components::text_field_ac("email", "Email", "email", email, true, "username"))
            (components::password_field("password", "Password", "current-password"))
            @if let Some(n) = next { input type="hidden" name="next" value=(n); }
            (components::submit("Sign in"))
        }
        p.muted {
            a href="/dashboard/forgot-password" { "Forgot your password?" }
        }
    };
    Html(layout::anonymous_page("Sign in", &body))
}
