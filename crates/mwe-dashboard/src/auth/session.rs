// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dashboard session cookie — sliding-TTL JWT carrying the logged-in
//! user's identity.
//!
//! ## Shape
//!
//! The cookie is a [`mwe_core::jwt::TokenClaims`] signed with the same
//! `MWE_TOKEN_SECRET` as every other JWT this deployment issues. Only
//! the TTL and the "where it travels" differ:
//!
//! - `sender_id`     — the dashboard user.
//! - `device_label`  — hardcoded to [`SESSION_DEVICE_LABEL`] so the
//!   admin can recognise dashboard sessions in `token-list`.
//! - `rate_limit_id` — [`SESSION_RATE_LIMIT_ID`], the wider `rate_limits:`
//!   profile a person clicking through the panel is held to.
//! - `is_admin`      — the role at issuance time; trusted for the cookie
//!   lifetime (60 minutes max), revocation via [`mwe_core::jwt::revoke`].
//! - `consumer_id`   — never set on session cookies (act-as is for the
//!   MCP transport, not for browsers).
//! - `session_gen`   — the user's session generation at mint time; a
//!   cookie below the current one was signed out, wherever it is
//!   presented from ([`mwe_core::jwt::end_all_sessions`]).
//!
//! ## Sliding TTL
//!
//! [`refresh_session_layer`] is a tower middleware that runs on every
//! authenticated request. It:
//!
//! 1. Reads the cookie, calls [`mwe_core::jwt::verify`] (signature +
//!    `exp` + blacklist), and refuses any token whose `device_label` is
//!    not [`SESSION_DEVICE_LABEL`] — the same secret signs MCP bearer
//!    tokens and OAuth access tokens, and none of those is a session.
//! 2. On success, attaches a [`SessionUser`] to the request extensions
//!    so the extractor can hand it back to the handler.
//! 3. Mints a fresh JWT with the same `sender_id`+`is_admin` but a new
//!    `jti` and new `exp = now + 60min`, sets it back on the response
//!    cookie jar — the cookie is *always* refreshed when the user
//!    interacts, expires after 60min of idleness. A small keepalive ping
//!    (`/dashboard/session/keepalive`, fired by the shell on user
//!    interaction) counts as an interaction, so an active tab never
//!    lapses even on an all-client-side form.
//! 4. On any verify failure, redirects to `/dashboard/login`.
//!
//! ## Extractors
//!
//! Handlers behind [`refresh_session_layer`] take a [`SessionUser`] or
//! the stronger [`AdminUser`] in their signature. Both are pure-read
//! extractors that look up the value the middleware put in the request
//! extensions — no DB hit, no JWT decoding twice.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use mwe_core::jwt::{self, TokenClaims};

use crate::error::DashboardError;
use crate::state::DashboardState;

/// Name of the cookie that holds the session JWT.
pub const SESSION_COOKIE_NAME: &str = "mwe_session";

/// `device_label` claim baked into every session JWT.
///
/// It is what makes a JWT a session: [`verify_session`] refuses every
/// other label, and an admin listing tokens can tell dashboard sessions
/// apart from MCP tokens.
pub const SESSION_DEVICE_LABEL: &str = "dashboard-session";

/// `rate_limit_id` claim baked into every session JWT.
///
/// It names the `rate_limits:` profile in `mwe-mcp.config.yaml` whose
/// ceilings hold the session, wider than `default` because a person
/// clicking through the panel is not a consumer in a loop.
pub const SESSION_RATE_LIMIT_ID: &str = mwe_core::config::DASHBOARD_RATE_LIMIT_ID;

/// Path attribute of the session cookie — narrows it to the dashboard
/// route tree so it never travels to `/mcp/*`.
pub const SESSION_COOKIE_PATH: &str = "/dashboard";

/// Identifier of the logged-in dashboard user, distilled from the
/// session JWT and attached to the request by [`refresh_session_layer`].
#[derive(Debug, Clone)]
pub struct SessionUser {
    /// `sender_id` claim from the session JWT.
    pub sender_id: String,
    /// Whether the user has the admin role. Trusted for the 60-minute
    /// cookie lifetime.
    pub is_admin: bool,
    /// `jti` of the cookie that authenticated this request.
    ///
    /// `/dashboard/logout` blacklists it as the record that this
    /// credential was ended deliberately; what actually ends the
    /// person's sessions is their [session
    /// generation](mwe_core::jwt::end_all_sessions), which covers every
    /// device at once.
    pub session_jti: String,
}

impl SessionUser {
    /// Helper for admin-gated handlers — returns
    /// [`DashboardError::Forbidden`] when the user is not admin.
    pub fn require_admin(self) -> Result<AdminUser, DashboardError> {
        if self.is_admin {
            Ok(AdminUser(self))
        } else {
            Err(DashboardError::Forbidden)
        }
    }
}

/// Newtype proving an admin-gated check has passed at the handler
/// signature level.
///
/// Built either via [`SessionUser::require_admin`] or directly through
/// the [`AdminUser`] extractor (which is the canonical path for
/// `/dashboard/users`, `/groups`, `/tokens`).
#[derive(Debug, Clone)]
pub struct AdminUser(pub SessionUser);

impl AdminUser {
    /// Access the wrapped session.
    #[must_use]
    pub const fn session(&self) -> &SessionUser {
        &self.0
    }

    /// Convenience accessor for the admin's `sender_id`.
    #[must_use]
    pub fn sender_id(&self) -> &str {
        &self.0.sender_id
    }
}

/// Build a fresh session JWT for `sender_id` with the given `is_admin`
/// flag, session generation and the configured sliding TTL.
fn build_session_claims(
    state: &DashboardState,
    sender_id: &str,
    is_admin: bool,
    generation: i64,
) -> TokenClaims {
    let ttl =
        Duration::from_secs(u64::try_from(state.config.session_ttl_minutes * 60).unwrap_or(0));
    let mut claims = TokenClaims::new(sender_id, SESSION_DEVICE_LABEL, SESSION_RATE_LIMIT_ID, ttl);
    claims.is_admin = is_admin;
    claims.session_gen = Some(generation);
    claims
}

/// Build the cookie that carries `claims`. Centralised so login,
/// invitation, setup, and the sliding refresher all emit the same
/// attribute set.
fn cookie_for_claims(state: &DashboardState, token: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path(SESSION_COOKIE_PATH);
    cookie.set_secure(state.config.cookie_secure);
    cookie
}

/// Issue a fresh session JWT for somebody who has just proved who they
/// are, sign it, and return the cookie ready to be added to a
/// [`CookieJar`].
///
/// Reads the sender's [session
/// generation](mwe_core::jwt::session_generation) so the cookie is valid
/// until their next "sign out everywhere" — every sign-in path goes
/// through here, so none of them can forget to.
///
/// # Errors
///
/// The generation lookup failed, or the JWT could not be signed.
pub async fn issue_session_cookie(
    state: &DashboardState,
    sender_id: &str,
    is_admin: bool,
) -> Result<Cookie<'static>, DashboardError> {
    let generation = jwt::session_generation(&state.pool, sender_id)
        .await
        .map_err(DashboardError::Token)?;
    mint_session_cookie(state, sender_id, is_admin, generation)
}

/// Sign a session cookie carrying an already-known generation.
///
/// Two callers, and both hold the right number rather than reading it:
/// the sliding refresher carries forward the generation it has just
/// verified — re-reading it would hand a fresh, valid cookie to a session
/// that was signed out while its request was in flight — and a handler
/// that has just ended every session of the person in front of it mints
/// the one cookie that survives.
///
/// # Errors
///
/// The JWT could not be signed.
pub fn mint_session_cookie(
    state: &DashboardState,
    sender_id: &str,
    is_admin: bool,
    generation: i64,
) -> Result<Cookie<'static>, DashboardError> {
    let claims = build_session_claims(state, sender_id, is_admin, generation);
    let token = jwt::issue(&state.secret, &claims).map_err(DashboardError::Token)?;
    Ok(cookie_for_claims(state, token))
}

/// Build a cookie that clears the session — same name and path as the
/// live cookie so the browser overwrites it.
#[must_use]
pub fn clear_session_cookie(state: &DashboardState) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, "");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path(SESSION_COOKIE_PATH);
    cookie.set_secure(state.config.cookie_secure);
    cookie.set_max_age(time::Duration::seconds(0));
    cookie
}

/// Verify whatever cookie the request carries and return the claims
/// on success. Surfaces every flavour of failure as
/// [`DashboardError::Unauthenticated`] so the middleware can collapse
/// them into a single redirect.
async fn verify_session(
    state: &DashboardState,
    jar: &CookieJar,
) -> Result<TokenClaims, DashboardError> {
    let cookie = jar
        .get(SESSION_COOKIE_NAME)
        .ok_or(DashboardError::Unauthenticated)?;
    let claims = jwt::verify(&state.secret, cookie.value(), &state.pool, &state.blacklist)
        .await
        .map_err(|_| DashboardError::Unauthenticated)?;
    // Every JWT this deployment issues is signed with the same secret: MCP
    // bearer tokens (a year long, carrying the owner's admin flag), OAuth
    // access tokens, magic links. Only the ones minted *for a browser* are
    // a session — an MCP token pasted into the cookie would otherwise walk
    // into the panel past the second factor, which only the login and the
    // magic-link paths ask for.
    if claims.device_label != SESSION_DEVICE_LABEL {
        return Err(DashboardError::Unauthenticated);
    }
    // "Sign out everywhere" is one number: a cookie minted before the
    // sender last ended their sessions names a lower generation and is
    // over, wherever it is being presented from. A cookie with no number
    // at all reads as generation 0, which is where a user who has never
    // ended their sessions still is.
    //
    // A failed lookup refuses the session, like every other step of this
    // function: whether a credential is still good is not a question to
    // answer optimistically when the answer is unavailable.
    let current = jwt::session_generation(&state.pool, &claims.sender_id)
        .await
        .map_err(|_| DashboardError::Unauthenticated)?;
    if claims.session_gen.unwrap_or(0) < current {
        return Err(DashboardError::Unauthenticated);
    }
    Ok(claims)
}

/// Tower middleware: gate the wrapped routes on a valid session and
/// refresh the cookie on every successful interaction.
pub async fn refresh_session_layer(
    State(state): State<DashboardState>,
    jar: CookieJar,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Ok(claims) = verify_session(&state, &jar).await else {
        return Redirect::to("/dashboard/login").into_response();
    };

    let user = SessionUser {
        sender_id: claims.sender_id.clone(),
        is_admin: claims.is_admin,
        session_jti: claims.jti.clone(),
    };
    let generation = claims.session_gen.unwrap_or(0);
    request.extensions_mut().insert(user.clone());

    // 2FA enforcement: a user obliged to have 2FA but not yet
    // enrolled is trapped on the setup page — every other page redirects
    // there until they enroll. The setup/logout/keepalive routes are
    // exempt so they can actually complete it. Fail open on a DB error so
    // a transient hiccup never locks the whole dashboard out.
    if !enforcement_exempt(request.uri().path()) {
        match crate::twofa::needs_enrollment_trap(&state.pool, &user.sender_id).await {
            Ok(true) => return Redirect::to("/dashboard/settings/2fa").into_response(),
            Ok(false) => {},
            Err(e) => {
                tracing::error!(error = %e, "2fa enforcement check failed (allowing through)");
            },
        }
    }

    let response = next.run(request).await;

    // A handler that has decided about the session cookie has the last
    // word. The refresher's `Set-Cookie` is appended *after* the
    // handler's, and a browser keeps the last one it is given: without
    // this, signing out handed the browser a cleared cookie and then a
    // brand-new valid one, and the person stayed signed in.
    if sets_session_cookie(&response) {
        return response;
    }

    // Carried forward, not re-read: a session signed out while this
    // request was in flight must not be handed a fresh cookie by the
    // response to it.
    match mint_session_cookie(&state, &user.sender_id, user.is_admin, generation) {
        Ok(fresh) => (jar.add(fresh), response).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "failed to mint refreshed session cookie");
            response
        },
    }
}

/// Does this response already set the session cookie itself?
fn sets_session_cookie(response: &Response) -> bool {
    let prefix = format!("{SESSION_COOKIE_NAME}=");
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with(&prefix))
}

/// Paths reachable while a user is trapped by 2FA enforcement: the setup
/// flow itself (so they can enroll), plus logout and the keepalive ping.
/// Tolerates an optional `/dashboard` prefix so it works whether or not
/// the nest has stripped it before this layer runs.
fn enforcement_exempt(path: &str) -> bool {
    let p = path.strip_prefix("/dashboard").unwrap_or(path);
    p.starts_with("/settings/2fa") || p == "/logout" || p.starts_with("/session/")
}

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or(DashboardError::Unauthenticated)
    }
}

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for AdminUser {
    type Rejection = DashboardError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = SessionUser::from_request_parts(parts, state).await?;
        user.require_admin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mwe_core::delegations::DelegationCache;
    use mwe_core::jwt::{BlacklistCache, TokenSecret};
    use std::sync::Arc;

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

    /// Enrol a user so the rows keyed on `enrollment_users` (the session
    /// generation among them) have their referent.
    async fn enrol(state: &DashboardState, user_id: &str) {
        sqlx::query("INSERT OR IGNORE INTO enrollment_users (user_id) VALUES (?)")
            .bind(user_id)
            .execute(&state.pool)
            .await
            .expect("enrol");
    }

    /// Issuing then verifying a session cookie roundtrips the
    /// `sender_id` and the admin flag.
    #[tokio::test]
    async fn issue_then_verify_roundtrips_claims() {
        let (state, _workdir) = make_state().await;
        let cookie = issue_session_cookie(&state, "frodo", true)
            .await
            .expect("issue");

        let jar = CookieJar::new().add(cookie);
        let claims = verify_session(&state, &jar).await.expect("verify");

        assert_eq!(claims.sender_id, "frodo");
        assert!(claims.is_admin);
        assert_eq!(claims.device_label, SESSION_DEVICE_LABEL);
        assert_eq!(claims.rate_limit_id, SESSION_RATE_LIMIT_ID);
        assert!(claims.consumer_id.is_none());
    }

    /// A revoked session jti fails verify — the dashboard cookie
    /// rides on the same blacklist as MCP tokens.
    #[tokio::test]
    async fn revoked_session_fails_verify() {
        let (state, _workdir) = make_state().await;
        let cookie = issue_session_cookie(&state, "frodo", true)
            .await
            .expect("issue");
        let jar = CookieJar::new().add(cookie.clone());

        let claims = verify_session(&state, &jar).await.expect("first verify");
        mwe_core::jwt::revoke(&state.pool, &claims.jti, "logout", "frodo", claims.exp)
            .await
            .expect("revoke");
        state
            .blacklist
            .refresh(&state.pool)
            .await
            .expect("blacklist refresh");

        let err = verify_session(&state, &jar).await.expect_err("must reject");
        assert!(matches!(err, DashboardError::Unauthenticated));
    }

    /// A session minted before the user ended their sessions is over,
    /// wherever it is presented from — that is what makes signing out on
    /// one device reach the others. A session minted after it is fine.
    #[tokio::test]
    async fn a_session_from_before_the_last_sign_out_is_refused() {
        let (state, _workdir) = make_state().await;
        enrol(&state, "frodo").await;
        let old = issue_session_cookie(&state, "frodo", true)
            .await
            .expect("issue");
        let jar = CookieJar::new().add(old);
        verify_session(&state, &jar).await.expect("live before");

        mwe_core::jwt::end_all_sessions(&state.pool, "frodo")
            .await
            .expect("end all");

        let err = verify_session(&state, &jar).await.expect_err("must reject");
        assert!(matches!(err, DashboardError::Unauthenticated));

        let fresh = issue_session_cookie(&state, "frodo", true)
            .await
            .expect("issue");
        let jar = CookieJar::new().add(fresh);
        verify_session(&state, &jar)
            .await
            .expect("a session opened afterwards is not touched");
    }

    /// Another person's sign-out is not this person's: the generation is
    /// per user.
    #[tokio::test]
    async fn one_users_sign_out_leaves_another_users_session_alone() {
        let (state, _workdir) = make_state().await;
        enrol(&state, "frodo").await;
        enrol(&state, "samvise").await;
        let cookie = issue_session_cookie(&state, "frodo", false)
            .await
            .expect("issue");
        let jar = CookieJar::new().add(cookie);

        mwe_core::jwt::end_all_sessions(&state.pool, "samvise")
            .await
            .expect("end all");

        verify_session(&state, &jar).await.expect("still live");
    }

    /// A token that was not minted as a browser session — an MCP bearer
    /// token here, the same secret and a valid signature — is not a
    /// session, however it reached the cookie jar.
    #[tokio::test]
    async fn a_non_session_token_in_the_cookie_is_rejected() {
        let (state, _workdir) = make_state().await;
        let mut claims = TokenClaims::new("frodo", "mcp", "default", Duration::from_secs(3600));
        claims.is_admin = true;
        let token = jwt::issue(&state.secret, &claims).expect("issue");
        let jar = CookieJar::new().add(cookie_for_claims(&state, token));

        let err = verify_session(&state, &jar).await.expect_err("must reject");
        assert!(matches!(err, DashboardError::Unauthenticated));
    }

    /// `require_admin` enforces the role at the type-system level.
    #[test]
    fn require_admin_promotes_session_user() {
        let admin = SessionUser {
            sender_id: "frodo".into(),
            is_admin: true,
            session_jti: "j".into(),
        };
        assert!(admin.require_admin().is_ok());

        let regular = SessionUser {
            sender_id: "samvise".into(),
            is_admin: false,
            session_jti: "j".into(),
        };
        let err = regular.require_admin().expect_err("must reject");
        assert!(matches!(err, DashboardError::Forbidden));
    }
}
