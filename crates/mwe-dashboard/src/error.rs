// SPDX-License-Identifier: AGPL-3.0-or-later
//! Error type for dashboard handlers.
//!
//! Every handler returns `Result<T, DashboardError>`; `DashboardError`
//! implements [`axum::response::IntoResponse`] so it converts to an
//! HTML page with the right status code automatically. Keeping the
//! variants narrow (instead of a single `anyhow::Error`) lets us
//! render targeted error pages instead of leaking sqlx messages to
//! the user.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use thiserror::Error;

/// The page that lifts a refusal, offered next to the sentence that
/// explains it.
///
/// Only ever set to a page the reader can actually open: an operator
/// console handed to somebody who is not an admin is a 403, which is a
/// worse answer than no link at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    /// Where to go.
    pub href: &'static str,
    /// What the link says.
    pub label: &'static str,
}

/// Handler-level error variants. The discriminants drive both the
/// HTTP status and the user-facing copy.
#[derive(Debug, Error)]
pub enum DashboardError {
    /// Generic 400 for malformed form bodies, bad path params, etc.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// User-facing validation message (e.g. "Password is too short").
    /// Renders as 422 to distinguish from "broken request" 400s.
    #[error("{0}")]
    Validation(String),

    /// Authenticated session is missing or expired — handlers reached
    /// through the auth middleware never produce this; it is reserved
    /// for explicit checks inside handlers.
    #[error("not authenticated")]
    Unauthenticated,

    /// Authenticated but lacking admin rights.
    #[error("admin role required")]
    Forbidden,

    /// Authenticated, but outside the read set of the requested content
    /// (per-fragment ACL on media; on a wiki, whichever question its family
    /// asks — [`mwe_core::wiki_admin::wiki_readable_by`]).
    /// Distinct from [`Self::Forbidden`] because **admin does not bypass**
    /// a content read ACL — see the media route and that gate — so the
    /// "Admin rights required." copy would be an actively misleading hint.
    #[error("no read access")]
    NoAccess,

    /// Resource not found (404).
    #[error("not found")]
    NotFound,

    /// Unhandled sqlx failure — bubbled as 500 with a generic message
    /// to the user; the actual error is logged at `error!` level by
    /// the response converter.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// JWT issue/verify/blacklist failure surfaced from `mwe-core`.
    #[error("token error: {0}")]
    Token(#[from] mwe_core::jwt::TokenError),

    /// Enrollment validator returned a hard error (id regex, dangling
    /// member, ...). Rendered as 422 with the validator's own message
    /// so the form can show it inline.
    #[error("enrollment error: {0}")]
    Enrollment(#[from] mwe_core::enrollment::EnrollmentError),

    /// Password hashing/verification machinery.
    #[error("password error: {0}")]
    Password(String),

    /// A model call the deployment refused for a reason the reader can
    /// act on: the daily budget stopped it, the provider is refusing
    /// more calls for now, the model could not be reached, or its key
    /// was rejected.
    ///
    /// Distinct from [`Self::Internal`] because nothing is broken — the
    /// generic "something went wrong, check the logs" is both false and
    /// useless here, and the person reading it can usually clear the
    /// condition themselves. `message` is written for them, and `fix`
    /// names the page that lifts it when one exists.
    #[error("unavailable: {message}")]
    Unavailable {
        /// The sentence shown to the reader.
        message: String,
        /// Where they go to lift it, when there is such a page.
        fix: Option<Fix>,
    },

    /// Catch-all for unrecoverable failures we did not predict.
    /// Rendered as 500 with a generic message.
    #[error("internal: {0}")]
    Internal(String),
}

impl DashboardError {
    const fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Validation(_) | Self::Enrollment(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::NoAccess => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Db(_) | Self::Token(_) | Self::Password(_) | Self::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            },
        }
    }

    /// User-facing message. Internal errors get a generic line; user
    /// errors get their own `Display` so they show validation hints.
    fn user_facing_message(&self) -> String {
        match self {
            Self::BadRequest(m) | Self::Validation(m) | Self::Password(m) => m.clone(),
            Self::Unauthenticated => "Authentication required.".to_owned(),
            Self::Forbidden => "Admin rights required.".to_owned(),
            Self::NoAccess => "You don't have access to this item.".to_owned(),
            Self::NotFound => "Page not found.".to_owned(),
            Self::Enrollment(e) => e.to_string(),
            Self::Unavailable { message, .. } => message.clone(),
            Self::Db(_) | Self::Token(_) | Self::Internal(_) => {
                "Something went wrong on the server. Check the server logs.".to_owned()
            },
        }
    }

    /// The page that lifts this refusal, when there is one.
    const fn fix(&self) -> Option<Fix> {
        match self {
            Self::Unavailable { fix, .. } => *fix,
            _ => None,
        }
    }

    fn log(&self, status: StatusCode) {
        if status.is_server_error() {
            // Server-side failures get the full chain in the logs so
            // an operator can debug from the terminal where mwe-mcp
            // serve was launched.
            tracing::error!(error = %self, "dashboard handler failed");
        } else {
            tracing::debug!(error = %self, status = status.as_u16(), "dashboard handler rejected");
        }
    }

    /// The same refusal as [`Self::into_response`], as the JSON envelope
    /// `{ error, fix: { href, label } }`.
    ///
    /// The chat panel posts with `Accept: application/json` and prints
    /// `error` in the conversation; handing it the HTML error page
    /// instead would leave it with nothing to say but the status code.
    #[must_use]
    pub fn into_json_response(self) -> Response {
        let status = self.status();
        self.log(status);
        let fix = self.fix();
        let body = serde_json::json!({
            "error": self.user_facing_message(),
            "fix": fix.map(|f| serde_json::json!({ "href": f.href, "label": f.label })),
        });
        (status, axum::Json(body)).into_response()
    }
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> Response {
        let status = self.status();
        self.log(status);
        let body = crate::ui::layout::error_page(status, &self.user_facing_message(), self.fix());
        (status, Html(body)).into_response()
    }
}

/// Shorthand for handler return types.
pub type Result<T> = std::result::Result<T, DashboardError>;
