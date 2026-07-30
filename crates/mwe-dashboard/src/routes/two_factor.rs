// SPDX-License-Identifier: AGPL-3.0-or-later
//! TOTP two-factor: the user-facing enrollment page (authenticated) and
//! the login-time challenge (public). Engine in [`crate::twofa`];
//! enforcement gate in [`crate::auth::session`] (roadmap 28).
//!
//! ## Two trees
//!
//! - [`settings_router`] — `/dashboard/settings/2fa*`, behind the session
//!   layer: enroll (QR + confirm), disable, regenerate recovery codes.
//! - [`challenge_router`] — `/dashboard/2fa`, **public**: the second-factor
//!   step between a verified password and the session mint. The challenge
//!   is held in `pending_2fa` keyed by an opaque cookie id, so no
//!   half-authenticated JWT ever exists.
//!
//! [`begin_challenge`] is the shared entry the login and magic-link paths
//! call when the subject has 2FA enabled.

use std::time::Duration as StdDuration;

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use crate::auth::SessionUser;
use crate::auth::session::issue_session_cookie;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::twofa::{self, State2fa};
use crate::ui::{components, layout};

/// Cookie carrying the opaque pending-challenge id.
const CHALLENGE_COOKIE: &str = "mwe_2fa";
/// Pending challenge / cookie lifetime.
const CHALLENGE_TTL_MIN: i64 = 5;
/// Max second-factor attempts per challenge before it must be restarted.
const CHALLENGE_MAX_ATTEMPTS: u32 = 10;

// ---------- routers ----------

/// Authenticated enrollment + management routes.
pub fn settings_router() -> Router<DashboardState> {
    Router::new()
        .route("/settings/2fa", get(settings_page))
        .route("/settings/2fa/enroll", post(enroll))
        .route("/settings/2fa/confirm", post(confirm))
        .route("/settings/2fa/disable", post(disable))
        .route("/settings/2fa/recovery-codes", post(regen_codes))
}

/// Public login-time challenge route.
pub fn challenge_router() -> Router<DashboardState> {
    Router::new().route("/2fa", get(challenge_page).post(challenge_submit))
}

// ---------- shared challenge entry (login + magic-link) ----------

/// Begin a second-factor challenge: persist a `pending_2fa` row, drop the
/// opaque cookie, and 303 to `/dashboard/2fa`. Called by the login and
/// magic-link paths when the subject has 2FA enabled — they have a
/// verified first factor (password / single-use link) but must not get a
/// session until the second factor passes.
pub async fn begin_challenge(
    state: &DashboardState,
    jar: CookieJar,
    user_id: &str,
    is_admin: bool,
    next: Option<&str>,
) -> Result<Response> {
    let id = twofa::create_pending(&state.pool, user_id, is_admin, next, CHALLENGE_TTL_MIN).await?;
    let jar = jar.add(challenge_cookie(state, id));
    Ok((jar, Redirect::to("/dashboard/2fa")).into_response())
}

fn challenge_cookie(state: &DashboardState, value: String) -> Cookie<'static> {
    let mut c = Cookie::new(CHALLENGE_COOKIE, value);
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_path("/dashboard");
    c.set_secure(state.config.cookie_secure);
    c.set_max_age(time::Duration::minutes(CHALLENGE_TTL_MIN));
    c
}

fn clear_challenge_cookie(state: &DashboardState) -> Cookie<'static> {
    let mut c = Cookie::new(CHALLENGE_COOKIE, "");
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_path("/dashboard");
    c.set_secure(state.config.cookie_secure);
    c.set_max_age(time::Duration::seconds(0));
    c
}

// ---------- challenge (public) ----------

async fn challenge_page(State(state): State<DashboardState>, jar: CookieJar) -> Result<Response> {
    let Some(id) = jar.get(CHALLENGE_COOKIE).map(|c| c.value().to_owned()) else {
        return Ok(Redirect::to("/dashboard/login").into_response());
    };
    match twofa::peek_pending(&state.pool, &id).await? {
        Some(_) => Ok(Html(render_challenge(None)).into_response()),
        None => Ok(Html(render_challenge_expired()).into_response()),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChallengeSubmission {
    /// A 6-digit TOTP code or a recovery code; we try TOTP first.
    pub code: String,
}

async fn challenge_submit(
    State(state): State<DashboardState>,
    jar: CookieJar,
    axum::Form(form): axum::Form<ChallengeSubmission>,
) -> Result<Response> {
    let Some(id) = jar.get(CHALLENGE_COOKIE).map(|c| c.value().to_owned()) else {
        return Ok(Redirect::to("/dashboard/login").into_response());
    };
    let Some(pending) = twofa::peek_pending(&state.pool, &id).await? else {
        return Ok(Html(render_challenge_expired()).into_response());
    };

    // Throttle guesses per challenge — a 6-digit code with ±1 skew is a
    // small space, so an unbounded retry loop would be brute-forceable.
    if !crate::ratelimit::check(
        &format!("2fa-challenge:{id}"),
        CHALLENGE_MAX_ATTEMPTS,
        StdDuration::from_secs(15 * 60),
    ) {
        return Ok(Html(render_challenge(Some(
            "Too many attempts. Sign in again to restart.",
        )))
        .into_response());
    }

    let code = form.code.trim();
    let account = account_label(&state, &pending.user_id).await?;
    let totp_ok = twofa::load_secret(&state.pool, &state.secret, &pending.user_id)
        .await?
        .is_some_and(|secret| twofa::verify_code(&secret, &account, code));
    let accepted =
        totp_ok || twofa::consume_recovery_code(&state.pool, &pending.user_id, code).await?;

    if !accepted {
        return Ok(Html(render_challenge(Some("Invalid code. Try again."))).into_response());
    }

    // Second factor passed: burn the challenge, mint the real session,
    // clear the challenge cookie.
    twofa::delete_pending(&state.pool, &id).await?;
    let session = issue_session_cookie(&state, &pending.user_id, pending.is_admin)?;
    let jar = jar.add(session).add(clear_challenge_cookie(&state));
    let dest = pending.next.as_deref().unwrap_or("/dashboard/home");
    tracing::info!(user = %pending.user_id, "2fa: challenge passed, session minted");
    Ok((jar, Redirect::to(dest)).into_response())
}

// ---------- settings (authenticated) ----------

async fn settings_page(
    State(state): State<DashboardState>,
    user: SessionUser,
) -> Result<Html<String>> {
    render_settings(&state, &user, None).await
}

/// Begin enrollment: mint a secret, store it unconfirmed, show the QR +
/// the confirm form.
async fn enroll(State(state): State<DashboardState>, user: SessionUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let secret = twofa::generate_secret_bytes();
    twofa::begin_enrollment(&state.pool, &state.secret, &user.sender_id, &secret).await?;
    let account = account_label(&state, &user.sender_id).await?;
    let url = twofa::provisioning_url(&secret, &account)?;
    let qr = twofa::qr_svg(&url)?;
    let body = html! {
        h2 { "Set up two-factor authentication" }
        (enroll_panel(&url, &qr, None))
    };
    Ok(Html(layout::authenticated_reading_page(
        chrome,
        "Two-factor authentication",
        &user,
        &body,
    )))
}

#[derive(Debug, Deserialize)]
pub struct ConfirmSubmission {
    pub code: String,
}

/// Confirm enrollment: verify a live code against the pending secret,
/// activate, and show the one-time recovery codes.
async fn confirm(
    State(state): State<DashboardState>,
    user: SessionUser,
    axum::Form(form): axum::Form<ConfirmSubmission>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    if twofa::state_of(&state.pool, &user.sender_id).await? != State2fa::Pending {
        // Nothing pending — re-render the status page.
        return render_settings(
            &state,
            &user,
            Some(("error", "No pending enrollment — start again.")),
        )
        .await;
    }
    let account = account_label(&state, &user.sender_id).await?;
    let secret = twofa::load_secret(&state.pool, &state.secret, &user.sender_id)
        .await?
        .ok_or_else(|| DashboardError::Internal("pending 2fa secret missing".to_owned()))?;

    if !twofa::verify_code(&secret, &account, form.code.trim()) {
        // Re-show the QR with an error so the user can retry.
        let url = twofa::provisioning_url(&secret, &account)?;
        let qr = twofa::qr_svg(&url)?;
        let body = html! {
            h2 { "Set up two-factor authentication" }
            (enroll_panel(&url, &qr, Some("That code didn't match. Try the current one.")))
        };
        return Ok(Html(layout::authenticated_reading_page(
            chrome,
            "Two-factor authentication",
            &user,
            &body,
        )));
    }

    let codes = twofa::generate_recovery_codes()?;
    twofa::confirm_enrollment(&state.pool, &user.sender_id, &codes).await?;
    tracing::info!(user = %user.sender_id, "2fa: enrollment confirmed");

    let body = html! {
        (components::flash("success", "Two-factor authentication is now on."))
        (recovery_codes_panel(&codes))
        p { a href="/dashboard/settings/2fa" { "← Back to two-factor settings" } }
    };
    Ok(Html(layout::authenticated_reading_page(
        chrome,
        "Recovery codes",
        &user,
        &body,
    )))
}

async fn disable(State(state): State<DashboardState>, user: SessionUser) -> Result<Html<String>> {
    // An obliged user cannot turn 2FA off — they would only be trapped on
    // the enrollment page again. Refuse with a clear message.
    if twofa::is_obliged(&state.pool, &user.sender_id).await? {
        return render_settings(
            &state,
            &user,
            Some((
                "error",
                "Your administrator requires two-factor authentication; it can't be turned off.",
            )),
        )
        .await;
    }
    twofa::disable(&state.pool, &user.sender_id).await?;
    tracing::info!(user = %user.sender_id, "2fa: disabled by user");
    render_settings(
        &state,
        &user,
        Some(("success", "Two-factor authentication turned off.")),
    )
    .await
}

async fn regen_codes(
    State(state): State<DashboardState>,
    user: SessionUser,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    if !twofa::is_enabled(&state.pool, &user.sender_id).await? {
        return render_settings(&state, &user, Some(("error", "Enable two-factor first."))).await;
    }
    let codes = twofa::generate_recovery_codes()?;
    // Reuse confirm_enrollment's code-replacement under the hood: it both
    // re-stamps enabled=1 and swaps the recovery set, which is exactly a
    // regenerate for an already-active user.
    twofa::confirm_enrollment(&state.pool, &user.sender_id, &codes).await?;
    tracing::info!(user = %user.sender_id, "2fa: recovery codes regenerated");
    let body = html! {
        (components::flash("success", "New recovery codes generated. The old ones no longer work."))
        (recovery_codes_panel(&codes))
        p { a href="/dashboard/settings/2fa" { "← Back to two-factor settings" } }
    };
    Ok(Html(layout::authenticated_reading_page(
        chrome,
        "Recovery codes",
        &user,
        &body,
    )))
}

// ---------- render ----------

async fn render_settings(
    state: &DashboardState,
    user: &SessionUser,
    flash: Option<(&str, &str)>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(state);
    let st = twofa::state_of(&state.pool, &user.sender_id).await?;
    let obliged = twofa::is_obliged(&state.pool, &user.sender_id).await?;
    let unused = if st == State2fa::Enabled {
        twofa::unused_recovery_count(&state.pool, &user.sender_id).await?
    } else {
        0
    };

    let body = html! {
        @if let Some((kind, msg)) = flash { (components::flash(kind, msg)) }
        h2 { "Two-factor authentication" }
        p.muted {
            "Protect your sign-in with a time-based code from an authenticator app "
            "(Aegis, Google Authenticator, 1Password, …)."
        }
        @if obliged {
            (components::flash("info", "Your administrator requires two-factor authentication on this account."))
        }

        @match st {
            State2fa::Enabled => {
                (components::flash("success", "Two-factor authentication is ON."))
                p { "Unused recovery codes: " strong { (unused) } "." }
                form action="/dashboard/settings/2fa/recovery-codes" method="post" {
                    (components::submit("Generate new recovery codes"))
                }
                @if !obliged {
                    form action="/dashboard/settings/2fa/disable" method="post" {
                        (components::submit("Turn off two-factor"))
                    }
                }
            },
            State2fa::Pending => {
                p { "You started setting up two-factor but didn't finish." }
                form action="/dashboard/settings/2fa/enroll" method="post" {
                    (components::submit("Continue setup"))
                }
            },
            State2fa::None => {
                form action="/dashboard/settings/2fa/enroll" method="post" {
                    (components::submit("Set up two-factor"))
                }
            },
        }
    };
    Ok(Html(layout::authenticated_reading_page(
        chrome,
        "Two-factor authentication",
        user,
        &body,
    )))
}

/// The QR + manual-secret + confirm-code panel shown during enrollment.
fn enroll_panel(url: &str, qr_svg: &str, error: Option<&str>) -> Markup {
    let secret_param = url
        .split_once("secret=")
        .and_then(|(_, rest)| rest.split('&').next())
        .unwrap_or_default();
    html! {
        @if let Some(msg) = error { (components::flash("error", msg)) }
        p { "Scan this QR code with your authenticator app:" }
        div.qr { (PreEscaped(qr_svg.to_owned())) }
        p.muted {
            "Can't scan? Enter this key manually: " code { (secret_param) }
        }
        h3 { "Confirm" }
        p { "Enter the 6-digit code your app shows now to finish:" }
        form action="/dashboard/settings/2fa/confirm" method="post" {
            p {
                label for="code" { "Authentication code" }
                input id="code" name="code" type="text" inputmode="numeric"
                    autocomplete="one-time-code" pattern="[0-9 ]*" required;
            }
            (components::submit("Verify and turn on"))
        }
    }
}

/// The one-time recovery-codes panel.
fn recovery_codes_panel(codes: &[String]) -> Markup {
    html! {
        h3 { "Save your recovery codes" }
        p.muted {
            "Each code works once if you lose your authenticator. Store them somewhere "
            "safe — they won't be shown again."
        }
        ul.recovery-codes {
            @for c in codes { li { code { (c) } } }
        }
    }
}

fn render_challenge(error: Option<&str>) -> String {
    let body = html! {
        h1 { "Two-factor authentication" }
        @if let Some(msg) = error { (components::flash("error", msg)) }
        p.muted { "Enter the 6-digit code from your authenticator app." }
        form action="/dashboard/2fa" method="post" {
            p {
                label for="code" { "Authentication code" }
                input id="code" name="code" type="text" inputmode="numeric"
                    autocomplete="one-time-code" pattern="[0-9 A-Za-z-]*" autofocus required;
            }
            (components::submit("Verify"))
        }
        p.muted { "Lost your device? Enter one of your recovery codes above instead." }
        p.muted { a href="/dashboard/login" { "← Back to sign in" } }
    };
    layout::anonymous_page("Two-factor authentication", &body)
}

fn render_challenge_expired() -> String {
    let body = html! {
        (components::flash("error", "This sign-in attempt expired. Please sign in again."))
        p.muted { a href="/dashboard/login" { "← Back to sign in" } }
    };
    layout::anonymous_page("Sign in again", &body)
}

// ---------- helpers ----------

/// The label shown in the authenticator app (`issuer:account`). Prefer
/// the login email; fall back to the user id.
async fn account_label(state: &DashboardState, user_id: &str) -> Result<String> {
    let email: Option<String> =
        sqlx::query_scalar("SELECT email FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();
    Ok(email.unwrap_or_else(|| user_id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use mwe_core::delegations::DelegationCache;
    use mwe_core::jwt::{BlacklistCache, TokenSecret};
    use std::sync::Arc;

    async fn make_state() -> (DashboardState, tempfile::TempDir) {
        // The guard goes back to the caller: a leaked temporary
        // directory is never removed by anything (see the leak that
        // filled tmpfs on the production host).
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = mwe_core::db::open_or_init(dir.path()).await.expect("db");
        let secret = TokenSecret::new(vec![0xCDu8; 32]).expect("secret");
        let blacklist = Arc::new(BlacklistCache::new());
        let delegations = Arc::new(DelegationCache::new());
        (
            DashboardState::new(pool, secret, blacklist, delegations),
            dir,
        )
    }

    async fn enrolled_user(state: &DashboardState, user: &str) -> Vec<String> {
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES (?, 0)")
            .bind(user)
            .execute(&state.pool)
            .await
            .expect("insert user");
        let secret = twofa::generate_secret_bytes();
        twofa::begin_enrollment(&state.pool, &state.secret, user, &secret)
            .await
            .expect("begin");
        let codes = twofa::generate_recovery_codes().expect("codes");
        twofa::confirm_enrollment(&state.pool, user, &codes)
            .await
            .expect("confirm");
        codes
    }

    fn has_session_cookie(resp: &Response) -> bool {
        resp.headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|c| c.starts_with("mwe_session="))
    }

    #[tokio::test]
    async fn challenge_passes_with_recovery_code_and_mints_session() {
        let (state, _workdir) = make_state().await;
        let codes = enrolled_user(&state, "frodo").await;
        let id = twofa::create_pending(&state.pool, "frodo", false, None, 5)
            .await
            .expect("pending");
        let jar = CookieJar::new().add(Cookie::new(CHALLENGE_COOKIE, id.clone()));

        let resp = challenge_submit(
            State(state.clone()),
            jar,
            axum::Form(ChallengeSubmission {
                code: codes[0].clone(),
            }),
        )
        .await
        .expect("handler");

        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(has_session_cookie(&resp), "session cookie must be set");
        // Challenge burned; recovery code spent.
        assert!(
            twofa::peek_pending(&state.pool, &id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !twofa::consume_recovery_code(&state.pool, "frodo", &codes[0])
                .await
                .unwrap(),
            "recovery code must be single-use"
        );
    }

    #[tokio::test]
    async fn wrong_code_mints_no_session_and_keeps_challenge() {
        let (state, _workdir) = make_state().await;
        let _ = enrolled_user(&state, "frodo").await;
        let id = twofa::create_pending(&state.pool, "frodo", false, None, 5)
            .await
            .expect("pending");
        let jar = CookieJar::new().add(Cookie::new(CHALLENGE_COOKIE, id.clone()));

        let resp = challenge_submit(
            State(state.clone()),
            jar,
            axum::Form(ChallengeSubmission {
                code: "000000".to_owned(),
            }),
        )
        .await
        .expect("handler");

        assert_eq!(resp.status(), StatusCode::OK); // re-rendered form
        assert!(!has_session_cookie(&resp));
        assert!(
            twofa::peek_pending(&state.pool, &id)
                .await
                .unwrap()
                .is_some(),
            "a failed attempt must leave the challenge alive"
        );
    }

    #[tokio::test]
    async fn no_cookie_redirects_to_login() {
        let (state, _workdir) = make_state().await;
        let resp = challenge_page(State(state), CookieJar::new())
            .await
            .expect("handler");
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }
}
