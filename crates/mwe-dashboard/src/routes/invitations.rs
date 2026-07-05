// SPDX-License-Identifier: AGPL-3.0-or-later
//! Invitation acceptance: `/dashboard/accept-invite/:invitation_id`
//! ([dashboard](../../../../wiki/design-notes/dashboard.md) +
//! [engine DB and migrations](../../../../wiki/design-notes/engine-db-and-migrations.md)).
//!
//! Publicly reachable: anyone with the URL can land here. The guard
//! is the invitation itself — `UUIDv7` random, single-use, 24h TTL.
//! Successful POST writes `user_credentials`, marks the invitation
//! consumed, and mints the session cookie so the new user lands on
//! `/dashboard/home` already signed in.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use chrono::Utc;
use maud::html;
use serde::Deserialize;

use crate::auth::{password, session::issue_session_cookie};
use crate::error::Result;
use crate::state::DashboardState;
use crate::ui::{components, layout};

pub fn router() -> Router<DashboardState> {
    Router::new().route("/accept-invite/:invitation_id", get(form).post(submit))
}

#[derive(Debug)]
struct LiveInvitation {
    user_id: String,
    is_admin: bool,
}

async fn lookup_live_invitation(
    state: &DashboardState,
    invitation_id: &str,
) -> Result<Option<LiveInvitation>> {
    let now = Utc::now().to_rfc3339();
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT i.user_id, u.is_admin
           FROM user_invitations i
           JOIN enrollment_users u ON u.user_id = i.user_id
          WHERE i.invitation_id = ?
            AND i.consumed_at IS NULL
            AND i.expires_at > ?",
    )
    .bind(invitation_id)
    .bind(&now)
    .fetch_optional(&state.pool)
    .await?;

    Ok(row.map(|(user_id, is_admin)| LiveInvitation {
        user_id,
        is_admin: is_admin != 0,
    }))
}

async fn form(
    State(state): State<DashboardState>,
    Path(invitation_id): Path<String>,
) -> Result<Html<String>> {
    let html = lookup_live_invitation(&state, &invitation_id)
        .await?
        .map_or_else(render_dead_invitation, |invitation| {
            render_form(&invitation, &invitation_id, None, &state)
        });
    Ok(Html(html))
}

#[derive(Debug, Deserialize)]
pub struct AcceptSubmission {
    pub password: String,
    pub password_confirm: String,
}

async fn submit(
    State(state): State<DashboardState>,
    jar: axum_extra::extract::cookie::CookieJar,
    Path(invitation_id): Path<String>,
    axum::Form(form): axum::Form<AcceptSubmission>,
) -> Result<Response> {
    let Some(invitation) = lookup_live_invitation(&state, &invitation_id).await? else {
        return Ok(Html(render_dead_invitation()).into_response());
    };

    let password = form.password.as_str();
    let confirm = form.password_confirm.as_str();

    if password.len() < state.config.min_password_len {
        let msg = format!(
            "Password must be at least {} characters.",
            state.config.min_password_len
        );
        return Ok(
            Html(render_form(&invitation, &invitation_id, Some(&msg), &state)).into_response(),
        );
    }
    if password != confirm {
        return Ok(Html(render_form(
            &invitation,
            &invitation_id,
            Some("The two passwords do not match."),
            &state,
        ))
        .into_response());
    }

    // Mutual exclusion, agent side (migration 0050): never grant a dashboard
    // login to a consumer-agent identity. `validate_token_identity` bars the
    // other direction (a credentialed human can't bind a standard token); this is
    // the defense-in-depth at the credential-creation end — an identity is either
    // a human with a login or a bot's credential-less system user, never both.
    if let Err(msg) = mwe_core::enrollment::reject_if_agent(&state.pool, &invitation.user_id).await
    {
        tracing::warn!(user = %invitation.user_id, "invite acceptance blocked (is_agent): {msg}");
        return Ok(
            Html(render_form(&invitation, &invitation_id, Some(&msg), &state)).into_response(),
        );
    }

    let phc = password::hash(password)?;
    let now = Utc::now().to_rfc3339();

    let mut tx = state.pool.begin().await?;
    // Use INSERT OR REPLACE so a re-invitation flow (admin cancels and
    // re-issues for the same user) just overwrites the prior hash.
    sqlx::query(
        "INSERT OR REPLACE INTO user_credentials
            (user_id, password_hash, hashed_at, must_change)
         VALUES (?, ?, ?, 0)",
    )
    .bind(&invitation.user_id)
    .bind(&phc)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE user_invitations SET consumed_at = ?
          WHERE invitation_id = ?
            AND consumed_at IS NULL",
    )
    .bind(&now)
    .bind(&invitation_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(user = %invitation.user_id, "invitation consumed; credentials set");

    let cookie = issue_session_cookie(&state, &invitation.user_id, invitation.is_admin)?;
    Ok((jar.add(cookie), Redirect::to("/dashboard/home")).into_response())
}

fn render_form(
    invitation: &LiveInvitation,
    invitation_id: &str,
    error: Option<&str>,
    state: &DashboardState,
) -> String {
    let title = format!("Welcome, {}", invitation.user_id);
    let body = html! {
        p {
            "You were invited to mwe-mcp as " strong { (invitation.user_id) } ". "
            "Choose your password below — the admin will never see it. The link is "
            "single-use and expires after the invitation TTL."
        }
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        form action=(format!("/dashboard/accept-invite/{invitation_id}")) method="post" {
            // Hidden username anchor so the browser password manager
            // associates the new password with this account (Chrome /
            // Safari). Not read server-side — the route trusts the
            // single-use invitation id.
            input type="text" name="username" autocomplete="username" value=(invitation.user_id) hidden;
            (components::password_field("password", "Choose a password", "new-password"))
            (components::password_field("password_confirm", "Confirm password", "new-password"))
            p.help.muted { "Minimum " (state.config.min_password_len) " characters." }
            (components::submit("Set password and sign in"))
        }
    };
    layout::anonymous_page(&title, &body)
}

fn render_dead_invitation() -> String {
    let body = html! {
        p { "This invitation link is invalid, expired, or already used." }
        p { "Ask the admin to share a fresh one — they can regenerate it "
            "from " code { "/dashboard/users" } "." }
        p { a href="/dashboard/login" { "Back to sign in" } }
    };
    layout::anonymous_page("Invitation unavailable", &body)
}
