// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-use dashboard magic-link redemption (`GET /dashboard/auth/link`).
//!
//! Public (pre-session) route, mounted **outside** `refresh_session_layer`
//! alongside `/login`, `/setup`, `accept-invite`, and `cite` — same
//! posture as [`crate::routes::cite`]: the link itself is anonymous, the
//! session materialises here and access control fires on the
//! destination page.
//!
//! A `dashboard_link` MCP call mints a short-lived, session-shaped JWT
//! bound to a human subject (the act-as identity) and hands the consumer
//! a URL of the form `/dashboard/auth/link?token=<jwt>&next=<deep-link>`.
//! The consumer relays it (e.g. on Telegram). This handler:
//!
//! 1. verifies the token (signature + expiry + blacklist);
//! 2. checks it is a *dashboard-session* token, not an MCP bearer
//!    (defense-in-depth on `device_label`);
//! 3. **burns it once** via [`mwe_core::jwt::revoke_once`] (a
//!    compare-and-set on the `jti`) plus a synchronous blacklist
//!    refresh, so a replay of the same URL fails;
//! 4. mints the real sliding session cookie for the token's subject;
//! 5. 303-redirects to the validated `next` deep-link, token stripped
//!    from the address bar.
//!
//! Single-use lives on the *link* token; the session cookie it mints is
//! a separate, longer-lived sliding JWT, so refresh / back-button still
//! work after redemption.

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::html;
use mwe_core::jwt;
use serde::Deserialize;

use crate::auth::session::{SESSION_DEVICE_LABEL, issue_session_cookie};
use crate::error::Result;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Default landing when `next` is missing or fails the same-origin guard.
const DEFAULT_NEXT: &str = "/dashboard/home";

/// Mount the public redemption route.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/auth/link", get(redeem))
}

#[derive(Debug, Deserialize)]
struct RedeemQuery {
    /// The single-use link JWT minted by `dashboard_link`.
    token: String,
    /// Validated deep-link to land on after the cookie is set. The mint
    /// side folds any `chat_seed` into this path, so the handler treats
    /// it as the complete (url-decoded) destination.
    #[serde(default)]
    next: Option<String>,
}

/// `GET /dashboard/auth/link?token=…&next=…`
async fn redeem(
    State(state): State<DashboardState>,
    jar: CookieJar,
    Query(q): Query<RedeemQuery>,
) -> Result<Response> {
    // 1. Verify signature + expiry + (cached) blacklist. Any failure —
    //    bad signature, expired, or already-burned jti — renders the
    //    dead-link page rather than bouncing to /login, so a replay
    //    reads as "already used" instead of silently looping.
    let Ok(claims) = jwt::verify(&state.secret, &q.token, &state.pool, &state.blacklist).await
    else {
        return Ok(dead_link().into_response());
    };

    // 2. Defense-in-depth: only a dashboard-session token may be redeemed
    //    here. A stolen MCP bearer JWT (device_label "mcp") cannot be
    //    swapped for a browser session through this endpoint.
    if claims.device_label != SESSION_DEVICE_LABEL {
        return Ok(dead_link().into_response());
    }

    // 3. Burn the link exactly once. The plain INSERT on the jti primary
    //    key is the serialization point: of two concurrent redemptions of
    //    the same URL, only one gets `true`.
    let won = jwt::revoke_once(
        &state.pool,
        &claims.jti,
        "dashboard_link_redeemed",
        &claims.sender_id,
        claims.exp,
    )
    .await?;
    if !won {
        return Ok(dead_link().into_response());
    }
    // Make the revocation visible to the very next `verify` immediately,
    // closing the 60s blacklist-cache window so a fast replay fails.
    state.blacklist.refresh(&state.pool).await?;

    // 4. Second-factor gate. A magic link proves the consumer vouches for
    //    the user out-of-band, but it does NOT bypass 2FA when the subject
    //    has it on — otherwise anyone able to make the bot mint a link
    //    would sidestep the second factor. Hand off to the challenge,
    //    carrying the validated deep-link as the post-auth destination.
    let dest = safe_next(q.next.as_deref());
    if crate::twofa::is_enabled(&state.pool, &claims.sender_id).await? {
        return super::two_factor::begin_challenge(
            &state,
            jar,
            &claims.sender_id,
            claims.is_admin,
            Some(&dest),
        )
        .await;
    }

    // 5. No 2FA: mint the real sliding session cookie for the token's
    //    subject and redirect to the validated deep-link (token stripped).
    let cookie = issue_session_cookie(&state, &claims.sender_id, claims.is_admin)?;
    Ok((jar.add(cookie), Redirect::to(&dest)).into_response())
}

/// Same-origin redirect guard: honour `next` only when it is an absolute
/// path under `/dashboard/` (which cannot be protocol-relative `//host`
/// or carry a scheme) and free of CR/LF. Otherwise fall back to
/// [`DEFAULT_NEXT`]. Prevents this endpoint from becoming an
/// open-redirect / phishing primitive once it has minted a cookie.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with("/dashboard/") && !n.contains(['\r', '\n']) => n.to_owned(),
        _ => DEFAULT_NEXT.to_owned(),
    }
}

/// Anonymous "this link no longer works" page. Shared shape with the
/// expired-invitation page; worded for the single-use magic link.
fn dead_link() -> Html<String> {
    let body = html! {
        (components::flash("error", "This link is invalid, expired, or has already been used."))
        p.muted {
            "Dashboard links can be opened once. Ask your assistant to send a fresh one, or "
            a href="/dashboard/login" { "sign in" }
            " with your password."
        }
    };
    Html(layout::anonymous_page("Link no longer valid", &body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use mwe_core::delegations::DelegationCache;
    use mwe_core::jwt::{BlacklistCache, TokenClaims, TokenSecret};
    use std::sync::Arc;
    use std::time::Duration;

    async fn make_state() -> (DashboardState, tempfile::TempDir) {
        // The guard goes back to the caller: a leaked temporary
        // directory is never removed by anything (see the leak that
        // filled tmpfs on the production host).
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = mwe_core::db::open_or_init(dir.path())
            .await
            .expect("open db");
        let secret = TokenSecret::new(vec![0xCDu8; 32]).expect("secret");
        let blacklist = Arc::new(BlacklistCache::new());
        let delegations = Arc::new(DelegationCache::new());
        (
            DashboardState::new(pool, secret, blacklist, delegations),
            dir,
        )
    }

    /// Mint a dashboard-session-shaped link token for `sender`.
    fn link_token(state: &DashboardState, sender: &str, ttl: Duration) -> (String, String) {
        let claims = TokenClaims::new(sender, SESSION_DEVICE_LABEL, "dashboard", ttl);
        let jti = claims.jti.clone();
        let token = jwt::issue(&state.secret, &claims).expect("issue");
        (token, jti)
    }

    fn location(resp: &Response) -> Option<String> {
        resp.headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    fn has_session_cookie(resp: &Response) -> bool {
        resp.headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|c| c.starts_with("mwe_session="))
    }

    #[tokio::test]
    async fn fresh_token_redeems_sets_cookie_and_redirects_to_next() {
        let (state, _workdir) = make_state().await;
        let (token, _) = link_token(&state, "frodo", Duration::from_secs(600));
        let resp = redeem(
            State(state.clone()),
            CookieJar::new(),
            Query(RedeemQuery {
                token,
                next: Some("/dashboard/proposals/p-1".to_owned()),
            }),
        )
        .await
        .expect("handler ok");
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&resp).as_deref(), Some("/dashboard/proposals/p-1"));
        assert!(has_session_cookie(&resp), "must set mwe_session cookie");
    }

    #[tokio::test]
    async fn replay_of_same_link_is_dead() {
        let (state, _workdir) = make_state().await;
        let (token, _) = link_token(&state, "frodo", Duration::from_secs(600));
        // First redemption wins.
        let first = redeem(
            State(state.clone()),
            CookieJar::new(),
            Query(RedeemQuery {
                token: token.clone(),
                next: Some("/dashboard/proposals/p-1".to_owned()),
            }),
        )
        .await
        .expect("first ok");
        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        // Replay loses: dead-link page, no cookie.
        let second = redeem(
            State(state.clone()),
            CookieJar::new(),
            Query(RedeemQuery {
                token,
                next: Some("/dashboard/proposals/p-1".to_owned()),
            }),
        )
        .await
        .expect("second ok");
        assert_eq!(second.status(), StatusCode::OK);
        assert!(!has_session_cookie(&second), "replay must not set a cookie");
    }

    #[tokio::test]
    async fn mcp_bearer_label_is_rejected() {
        let (state, _workdir) = make_state().await;
        let claims = TokenClaims::new("frodo", "mcp", "mcp", Duration::from_secs(600));
        let token = jwt::issue(&state.secret, &claims).expect("issue");
        let resp = redeem(
            State(state.clone()),
            CookieJar::new(),
            Query(RedeemQuery {
                token,
                next: Some("/dashboard/proposals/p-1".to_owned()),
            }),
        )
        .await
        .expect("handler ok");
        assert_eq!(resp.status(), StatusCode::OK); // dead-link page
        assert!(!has_session_cookie(&resp));
    }

    #[tokio::test]
    async fn next_outside_dashboard_falls_back_home() {
        let (state, _workdir) = make_state().await;
        let (token, _) = link_token(&state, "frodo", Duration::from_secs(600));
        let resp = redeem(
            State(state.clone()),
            CookieJar::new(),
            Query(RedeemQuery {
                token,
                next: Some("https://evil.example/phish".to_owned()),
            }),
        )
        .await
        .expect("handler ok");
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&resp).as_deref(), Some(DEFAULT_NEXT));
    }

    #[test]
    fn safe_next_guards_open_redirect() {
        assert_eq!(
            safe_next(Some("/dashboard/proposals/p-1")),
            "/dashboard/proposals/p-1"
        );
        assert_eq!(safe_next(Some("//evil.example")), DEFAULT_NEXT);
        assert_eq!(safe_next(Some("https://evil.example")), DEFAULT_NEXT);
        assert_eq!(safe_next(Some("/etc/passwd")), DEFAULT_NEXT);
        assert_eq!(safe_next(None), DEFAULT_NEXT);
    }
}
