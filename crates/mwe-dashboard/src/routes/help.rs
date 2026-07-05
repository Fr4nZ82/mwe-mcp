// SPDX-License-Identifier: AGPL-3.0-or-later
//! Operative-chat **Help** page.
//!
//! The topnav "Help" button ([`crate::ui::layout`]) opens a modal whose
//! body is [`layout::help_body`]; `GET /dashboard/help` renders that same
//! body as a plain page so the surface still works without JavaScript
//! (the no-JS fallback the modal's `href` points at — the Dream-modal
//! pattern). Keeping a single [`layout::help_body`] source means the
//! modal and the fallback page cannot drift.

use axum::Router;
use axum::response::Html;
use axum::routing::get;
use maud::html;

use crate::auth::SessionUser;
use crate::error::Result;
use crate::state::DashboardState;
use crate::ui::layout;

/// Routes for the Help fallback page. Merged into the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/help", get(index))
}

/// `GET /dashboard/help` — the no-JS fallback: the same content the
/// topnav Help modal shows, rendered as a normal page. Available to
/// every authenticated user (the operative chat needs discoverability
/// for non-admins too).
async fn index(user: SessionUser) -> Result<Html<String>> {
    let body = html! {
        (layout::help_body())
        p style="margin-top:1rem" {
            a href="/dashboard/chat" class="text-phosphor hover:text-phosphor-bright" {
                "Open the chat"
            }
        }
    };
    Ok(Html(layout::authenticated_reading_page(
        "Talking to the chat",
        &user,
        &body,
    )))
}
