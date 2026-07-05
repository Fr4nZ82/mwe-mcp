// SPDX-License-Identifier: AGPL-3.0-or-later
//! Root path (`/dashboard/`) — dispatch the visitor to setup / login /
//! home depending on the deployment state and whether they carry a
//! session cookie.

use axum::extract::State;
use axum::response::Redirect;
use axum_extra::extract::cookie::CookieJar;
use mwe_core::jwt;

use crate::auth::session::SESSION_COOKIE_NAME;
use crate::error::Result;
use crate::state::DashboardState;

/// GET `/dashboard/`. No DB write, always 303 to the right next page.
///
/// Rules:
/// 1. No admin in DB yet → `/dashboard/setup`.
/// 2. Admin exists + valid session cookie → `/dashboard/home`.
/// 3. Admin exists + no (or expired/revoked) session cookie →
///    `/dashboard/login`.
pub async fn root(State(state): State<DashboardState>, jar: CookieJar) -> Result<Redirect> {
    if !admin_exists(&state).await? {
        return Ok(Redirect::to("/dashboard/setup"));
    }

    let logged_in = match jar.get(SESSION_COOKIE_NAME) {
        Some(c) => jwt::verify(&state.secret, c.value(), &state.pool, &state.blacklist)
            .await
            .is_ok(),
        None => false,
    };

    Ok(Redirect::to(if logged_in {
        "/dashboard/home"
    } else {
        "/dashboard/login"
    }))
}

/// Shared helper used by [`root`] and by the setup-wizard guard:
/// "is there at least one row with `is_admin = 1`?".
pub async fn admin_exists(state: &DashboardState) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE is_admin = 1")
        .fetch_one(&state.pool)
        .await?;
    Ok(n > 0)
}
