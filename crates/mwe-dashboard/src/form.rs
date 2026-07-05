// SPDX-License-Identifier: AGPL-3.0-or-later
//! [`HtmlForm`] — an `axum::Form` replacement that handles repeated
//! keys (e.g. `members=alice&members=bob`) by deserializing them into
//! a `Vec<String>`.
//!
//! `axum::Form` uses `serde_urlencoded` which deliberately rejects
//! repeated keys. The dashboard's group + delegation forms need
//! repeated keys for checkbox lists, so we ship our own tiny extractor
//! around `serde_html_form` instead of teaching the whole codebase to
//! squash arrays into comma-separated strings.
//!
//! Drop-in usage:
//!
//! ```ignore
//! async fn handler(HtmlForm(body): HtmlForm<MyForm>) -> ... { ... }
//! ```

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;

use crate::error::DashboardError;

/// Form-data extractor that accepts repeated keys.
#[derive(Debug)]
pub struct HtmlForm<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequest<S> for HtmlForm<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = DashboardError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await.map_err(|e| {
            DashboardError::BadRequest(format!(
                "reading form body: {} {}",
                StatusCode::BAD_REQUEST,
                e
            ))
        })?;
        let parsed: T = serde_html_form::from_bytes(&bytes)
            .map_err(|e| DashboardError::BadRequest(format!("malformed form body: {e}")))?;
        Ok(Self(parsed))
    }
}
