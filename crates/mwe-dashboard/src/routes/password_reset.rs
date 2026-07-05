// SPDX-License-Identifier: AGPL-3.0-or-later
//! Self-service password recovery (roadmap 28) — the public
//! forgot-password / reset-password flow.
//!
//! Four routes, all **public** (mounted outside `refresh_session_layer`,
//! like `/login` and `accept-invite`): the guard is the one-shot
//! `password_resets` token, not a session.
//!
//! - GET  `/dashboard/forgot-password`  — the "enter your email" form.
//! - POST `/dashboard/forgot-password`  — mint a reset row + email the
//!   link. **Anti-enumeration**: returns the exact same confirmation
//!   page whether or not the email resolves, and the SMTP send is
//!   fire-and-forget so the response time does not leak existence. Rate
//!   limited per-email and per-IP.
//! - GET  `/dashboard/reset-password/:token` — the "choose a new
//!   password" form (or a dead-link page).
//! - POST `/dashboard/reset-password/:token` — burn the token and write
//!   the new Argon2id hash in one transaction, then send the user to
//!   `/login` (no auto-sign-in, so the next login re-runs any 2FA gate).
//!
//! The recovery email is sent only when the admin has configured and
//! enabled the [`EmailConfig`] SMTP backend (the Email section of
//! `/dashboard/settings/me`);
//! otherwise the form explains that recovery is unavailable and the POST
//! is inert.

use std::time::Duration as StdDuration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use chrono::{Duration, Utc};
use maud::html;
use mwe_core::config::EmailConfig;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::password;
use crate::error::Result;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Mount the public recovery routes.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/forgot-password", get(request_form).post(request_submit))
        .route("/reset-password/:token", get(reset_form).post(reset_submit))
}

// ---------- rate limit ----------

/// Max recovery requests per key inside [`RL_WINDOW`].
const RL_MAX: u32 = 5;
/// Fixed window for the recovery-request rate limit.
const RL_WINDOW: StdDuration = StdDuration::from_secs(15 * 60);

fn rate_ok(key: &str) -> bool {
    crate::ratelimit::check(key, RL_MAX, RL_WINDOW)
}

// ---------- request (forgot password) ----------

async fn request_form(State(state): State<DashboardState>) -> Html<String> {
    let cfg = crate::email::email_cfg(&state);
    Html(render_request_form(&cfg))
}

#[derive(Debug, Deserialize)]
pub struct RequestSubmission {
    pub email: String,
}

async fn request_submit(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<RequestSubmission>,
) -> Result<Response> {
    let addr = form.email.trim().to_owned();
    let cfg = crate::email::email_cfg(&state);

    // Evaluate BOTH rate-limit keys (each call advances its counter), then
    // combine — a short-circuit would let one axis escape accounting.
    let ip = client_ip(&headers);
    let email_ok = rate_ok(&format!("email:{}", addr.to_lowercase()));
    let ip_ok = rate_ok(&format!("ip:{ip}"));
    let within_limit = email_ok && ip_ok;

    if within_limit && cfg.is_sendable() && !addr.is_empty() {
        // The lookup mirrors login exactly: email → user_id, only for
        // users that actually have a credential row (system/bot users do
        // not). A miss is silent — same response as a hit.
        if let Some(user_id) = lookup_user_by_email(&state, &addr).await? {
            let reset_id = Uuid::now_v7().to_string();
            let now = Utc::now();
            let expires = now + Duration::minutes(state.config.reset_ttl_minutes);
            sqlx::query(
                "INSERT INTO password_resets (reset_id, user_id, created_at, expires_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&reset_id)
            .bind(&user_id)
            .bind(now.to_rfc3339())
            .bind(expires.to_rfc3339())
            .execute(&state.pool)
            .await?;

            let origin = crate::email::origin_of(cfg.public_base_url.as_deref(), &headers);
            let url = format!("{origin}/dashboard/reset-password/{reset_id}");
            tracing::info!(user = %user_id, "password-reset: link minted, emailing");

            // Fire-and-forget so the response time is constant regardless
            // of whether the email resolved (anti-enumeration), and a slow
            // SMTP relay never stalls the user's request.
            let send_cfg = cfg.clone();
            let to = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::email::send_recovery_email(&send_cfg, &to, &url).await {
                    tracing::warn!(error = %e, "password-reset: recovery email send failed");
                }
            });
        }
    } else if !within_limit {
        tracing::warn!(ip = %ip, "password-reset: request rate-limited");
    }

    // Always the same confirmation, regardless of existence / config /
    // rate-limit state.
    Ok(Html(render_request_sent()).into_response())
}

// ---------- reset (choose new password) ----------

async fn reset_form(
    State(state): State<DashboardState>,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    Ok(Html(match lookup_live_reset(&state, &token).await? {
        Some(_) => render_reset_form(&token, None, state.config.min_password_len),
        None => render_dead_link(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct ResetSubmission {
    pub password: String,
    pub password_confirm: String,
}

async fn reset_submit(
    State(state): State<DashboardState>,
    Path(token): Path<String>,
    axum::Form(form): axum::Form<ResetSubmission>,
) -> Result<Response> {
    // 1. Token must be live. (Re-checked atomically at burn time below.)
    if lookup_live_reset(&state, &token).await?.is_none() {
        return Ok(Html(render_dead_link()).into_response());
    }

    // 2. Validate the new password BEFORE burning, so a too-short or
    //    mismatched attempt leaves the token usable for a retry.
    let pw = form.password.as_str();
    let min = state.config.min_password_len;
    if pw.len() < min {
        let msg = format!("Password must be at least {min} characters.");
        return Ok(Html(render_reset_form(&token, Some(&msg), min)).into_response());
    }
    if pw != form.password_confirm {
        return Ok(Html(render_reset_form(
            &token,
            Some("The two passwords do not match."),
            min,
        ))
        .into_response());
    }

    // 3. Burn-once + set the hash in one transaction. The conditional
    //    UPDATE (consumed_at IS NULL AND not expired) is the
    //    serialization point: of two concurrent submits only one updates
    //    a row.
    let phc = password::hash(pw)?;
    let now = Utc::now().to_rfc3339();
    let mut tx = state.pool.begin().await?;

    let burn = sqlx::query(
        "UPDATE password_resets SET consumed_at = ?
          WHERE reset_id = ? AND consumed_at IS NULL AND expires_at > ?",
    )
    .bind(&now)
    .bind(&token)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    if burn.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(Html(render_dead_link()).into_response());
    }

    let user_id: String =
        sqlx::query_scalar("SELECT user_id FROM password_resets WHERE reset_id = ?")
            .bind(&token)
            .fetch_one(&mut *tx)
            .await?;

    sqlx::query(
        "INSERT OR REPLACE INTO user_credentials
            (user_id, password_hash, hashed_at, must_change)
         VALUES (?, ?, ?, 0)",
    )
    .bind(&user_id)
    .bind(&phc)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    tracing::info!(user = %user_id, "password-reset: password updated via recovery link");

    // No auto-sign-in: send them to /login so the next sign-in runs any
    // 2FA gate the user has enabled.
    Ok(Html(render_reset_done()).into_response())
}

// ---------- DB + request helpers ----------

/// Resolve a login email to its `user_id`, only for users that have a
/// credential row (a `must_change`-style bot/system user without one is
/// not recoverable). Same shape as the login resolver.
async fn lookup_user_by_email(state: &DashboardState, email: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT u.user_id
           FROM enrollment_users u
           JOIN user_credentials c ON c.user_id = u.user_id
          WHERE u.email = ?
          LIMIT 1",
    )
    .bind(email)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Return the reset's `user_id` iff the token is still usable (unconsumed
/// and unexpired).
async fn lookup_live_reset(state: &DashboardState, token: &str) -> Result<Option<String>> {
    let now = Utc::now().to_rfc3339();
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT user_id FROM password_resets
          WHERE reset_id = ? AND consumed_at IS NULL AND expires_at > ?",
    )
    .bind(token)
    .bind(&now)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Best-effort client IP from the proxy headers Cloudflare / nginx set,
/// for rate limiting. Falls back to a shared `unknown` bucket on a direct
/// localhost hit (no header) — acceptable, the email-key axis still bites.
fn client_ip(headers: &HeaderMap) -> String {
    for h in ["cf-connecting-ip", "x-real-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok())
            && let Some(first) = v.split(',').next()
            && !first.trim().is_empty()
        {
            return first.trim().to_owned();
        }
    }
    "unknown".to_owned()
}

// ---------- render ----------

fn render_request_form(cfg: &EmailConfig) -> String {
    let body = html! {
        h1 { "Forgot your password?" }
        @if cfg.is_sendable() {
            p.muted {
                "Enter your account email and we'll send you a link to choose a new "
                "password. The link expires shortly and can be used once."
            }
            form action="/dashboard/forgot-password" method="post" {
                (components::text_field_ac("email", "Email", "email", "", true, "username"))
                (components::submit("Send reset link"))
            }
        } @else {
            (components::flash("info",
                "Password recovery by email is not configured on this server."))
            p.muted { "Ask the admin for a fresh invitation link to reset your password." }
        }
        p.muted { a href="/dashboard/login" { "← Back to sign in" } }
    };
    layout::anonymous_page("Forgot your password?", &body)
}

/// The constant confirmation shown after every POST — never reveals
/// whether the email existed.
fn render_request_sent() -> String {
    let body = html! {
        h1 { "Check your email" }
        p {
            "If an account exists for that address, we've sent a link to reset its "
            "password. The link expires shortly and can be used once."
        }
        p.muted { "Didn't get it? Check your spam folder, or ask the admin for an invitation link." }
        p.muted { a href="/dashboard/login" { "← Back to sign in" } }
    };
    layout::anonymous_page("Check your email", &body)
}

fn render_reset_form(token: &str, error: Option<&str>, min_len: usize) -> String {
    let body = html! {
        h1 { "Choose a new password" }
        @if let Some(msg) = error { (components::flash("error", msg)) }
        form action=(format!("/dashboard/reset-password/{token}")) method="post" {
            // Hidden anchor so the browser password manager files the new
            // password against this account. Not read server-side.
            input type="text" name="username" autocomplete="username" hidden;
            (components::password_field("password", "New password", "new-password"))
            (components::password_field("password_confirm", "Confirm new password", "new-password"))
            p.help.muted { "Minimum " (min_len) " characters." }
            (components::submit("Set new password"))
        }
    };
    layout::anonymous_page("Choose a new password", &body)
}

fn render_reset_done() -> String {
    let body = html! {
        (components::flash("success", "Your password has been updated."))
        p { "You can now " a href="/dashboard/login" { "sign in" } " with your new password." }
    };
    layout::anonymous_page("Password updated", &body)
}

fn render_dead_link() -> String {
    let body = html! {
        (components::flash("error", "This reset link is invalid, expired, or already used."))
        p.muted {
            "Reset links can be opened once and expire quickly. "
            a href="/dashboard/forgot-password" { "Request a new one" }
            " or " a href="/dashboard/login" { "sign in" } "."
        }
    };
    layout::anonymous_page("Link no longer valid", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password;
    use mwe_core::delegations::DelegationCache;
    use mwe_core::jwt::{BlacklistCache, TokenSecret};
    use std::sync::Arc;

    async fn make_state() -> DashboardState {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = mwe_core::db::open_or_init(dir.path()).await.expect("db");
        let secret = TokenSecret::new(vec![0xCDu8; 32]).expect("secret");
        let blacklist = Arc::new(BlacklistCache::new());
        let delegations = Arc::new(DelegationCache::new());
        DashboardState::new(pool, secret, blacklist, delegations)
    }

    async fn seed_user(state: &DashboardState, user: &str, email: &str, password: &str) {
        sqlx::query("INSERT INTO enrollment_users (user_id, email, is_admin) VALUES (?, ?, 0)")
            .bind(user)
            .bind(email)
            .execute(&state.pool)
            .await
            .expect("insert user");
        let phc = password::hash(password).expect("hash");
        sqlx::query(
            "INSERT INTO user_credentials (user_id, password_hash, hashed_at) VALUES (?, ?, ?)",
        )
        .bind(user)
        .bind(&phc)
        .bind(Utc::now().to_rfc3339())
        .execute(&state.pool)
        .await
        .expect("insert cred");
    }

    async fn mint_reset(state: &DashboardState, user: &str) -> String {
        let id = Uuid::now_v7().to_string();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO password_resets (reset_id, user_id, created_at, expires_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user)
        .bind(now.to_rfc3339())
        .bind((now + Duration::minutes(30)).to_rfc3339())
        .execute(&state.pool)
        .await
        .expect("insert reset");
        id
    }

    async fn current_hash(state: &DashboardState, user: &str) -> String {
        sqlx::query_scalar("SELECT password_hash FROM user_credentials WHERE user_id = ?")
            .bind(user)
            .fetch_one(&state.pool)
            .await
            .expect("hash")
    }

    #[tokio::test]
    async fn reset_sets_new_password_and_burns_token() {
        let state = make_state().await;
        seed_user(&state, "frodo", "frodo@example.com", "old-password-1").await;
        let before = current_hash(&state, "frodo").await;
        let token = mint_reset(&state, "frodo").await;

        let resp = reset_submit(
            State(state.clone()),
            Path(token.clone()),
            axum::Form(ResetSubmission {
                password: "brand-new-password-2".to_owned(),
                password_confirm: "brand-new-password-2".to_owned(),
            }),
        )
        .await
        .expect("handler");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The stored hash changed and verifies against the new password.
        let after = current_hash(&state, "frodo").await;
        assert_ne!(before, after, "hash must change");
        assert!(password::verify("brand-new-password-2", &after).unwrap());

        // The token is now consumed — a replay does nothing.
        let before_replay = after.clone();
        let _ = reset_submit(
            State(state.clone()),
            Path(token),
            axum::Form(ResetSubmission {
                password: "yet-another-pass-3".to_owned(),
                password_confirm: "yet-another-pass-3".to_owned(),
            }),
        )
        .await
        .expect("handler");
        assert_eq!(
            current_hash(&state, "frodo").await,
            before_replay,
            "a burned token must not change the password again"
        );
    }

    #[tokio::test]
    async fn short_password_does_not_burn_token() {
        let state = make_state().await;
        seed_user(&state, "sam", "sam@example.com", "old-password-1").await;
        let token = mint_reset(&state, "sam").await;
        let _ = reset_submit(
            State(state.clone()),
            Path(token.clone()),
            axum::Form(ResetSubmission {
                password: "short".to_owned(),
                password_confirm: "short".to_owned(),
            }),
        )
        .await
        .expect("handler");
        // Token still live → a proper attempt still works.
        assert!(lookup_live_reset(&state, &token).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn request_is_constant_response_regardless_of_existence() {
        let state = make_state().await;
        seed_user(&state, "frodo", "frodo@example.com", "old-password-1").await;
        let known = request_submit(
            State(state.clone()),
            HeaderMap::new(),
            axum::Form(RequestSubmission {
                email: "frodo@example.com".to_owned(),
            }),
        )
        .await
        .expect("handler");
        let unknown = request_submit(
            State(state.clone()),
            HeaderMap::new(),
            axum::Form(RequestSubmission {
                email: "nobody@example.com".to_owned(),
            }),
        )
        .await
        .expect("handler");
        assert_eq!(known.status(), unknown.status());
        assert_eq!(known.status(), axum::http::StatusCode::OK);
    }
}
