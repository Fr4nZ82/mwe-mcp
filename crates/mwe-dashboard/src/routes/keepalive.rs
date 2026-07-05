// SPDX-License-Identifier: AGPL-3.0-or-later
//! Session keepalive ping.
//!
//! A single `204 No Content` endpoint whose only job is to pass through
//! [`refresh_session_layer`](crate::auth::session::refresh_session_layer)
//! and bump the sliding session TTL while the user is actively working —
//! without submitting anything. The dashboard's multi-step forms (the
//! welcome primer above all) advance entirely client-side, so they make
//! no request between load and final submit; a user who lingers past the
//! TTL would otherwise have the submit bounced to `/login` with their work
//! lost. The shell layout pings this on user interaction (throttled), so
//! an active tab keeps its session alive. See the
//! [JWT & session model](../../../../wiki/design-notes/jwt-and-session-model.md).

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

use crate::auth::SessionUser;
use crate::state::DashboardState;

/// Sub-router for `/session/keepalive`. Mounted inside the authenticated
/// tree, so [`refresh_session_layer`](crate::auth::session::refresh_session_layer)
/// re-issues the cookie on the way out.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/session/keepalive", get(ping))
}

/// GET `/dashboard/session/keepalive` → `204`. The [`SessionUser`]
/// extractor gates it on a live session (an expired one is redirected to
/// `/login` by the layer before reaching here); the body is empty. The
/// session-refresh middleware does the actual work — minting a fresh
/// cookie — so this handler stays a no-op.
async fn ping(_user: SessionUser) -> StatusCode {
    StatusCode::NO_CONTENT
}
