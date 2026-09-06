// SPDX-License-Identifier: AGPL-3.0-or-later
//! End the person's dashboard sessions — everywhere, not just here —
//! clear the cookie, redirect to `/dashboard/login`.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use mwe_core::jwt;

use crate::auth::session::{SessionUser, clear_session_cookie};
use crate::error::Result;
use crate::state::DashboardState;

/// POST `/dashboard/logout`. Ends **every** session this person has open
/// — the phone left on a train, the browser on a shared machine, a cookie
/// copied out of a laptop — by moving their session generation on, then
/// clears the browser cookie and redirects.
///
/// The cookie of the browser doing the signing out is blacklisted by its
/// `jti` as well. The generation already covers it; the row is the record
/// that this credential was ended deliberately, which the generation, a
/// single number, cannot carry.
pub async fn handler(
    State(state): State<DashboardState>,
    jar: CookieJar,
    user: SessionUser,
) -> Result<Response> {
    let generation = jwt::end_all_sessions(&state.pool, &user.sender_id).await?;
    tracing::info!(
        user = %user.sender_id,
        generation,
        "logout: every session of this user ended"
    );

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
