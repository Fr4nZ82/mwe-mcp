// SPDX-License-Identifier: AGPL-3.0-or-later
//! End a dashboard session — revoke the cookie's JWT, clear the
//! cookie itself, redirect to `/dashboard/login`.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use mwe_core::jwt;

use crate::auth::session::{SessionUser, clear_session_cookie};
use crate::error::Result;
use crate::state::DashboardState;

/// POST `/dashboard/logout`. Adds the current session's `jti` to
/// `token_blacklist` (so any duplicate cookie copy stops working
/// within the cache TTL), clears the browser cookie, redirects.
pub async fn handler(
    State(state): State<DashboardState>,
    jar: CookieJar,
    user: SessionUser,
) -> Result<Response> {
    // Revoke this exact session JWT — the user explicitly asked to
    // end the session, so we do not wait for natural expiry.
    let exp = chrono::Utc::now().timestamp() + state.config.session_ttl_minutes * 60;
    jwt::revoke(
        &state.pool,
        &user.session_jti,
        "logout",
        &user.sender_id,
        exp,
    )
    .await?;
    // Force the in-memory cache to pick it up before the next request.
    state.blacklist.refresh(&state.pool).await?;

    let cleared = clear_session_cookie(&state);
    Ok((jar.add(cleared), Redirect::to("/dashboard/login")).into_response())
}
