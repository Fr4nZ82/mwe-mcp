// SPDX-License-Identifier: AGPL-3.0-or-later
//! `GET /dashboard/media/:catalog_id` — the cookie-authenticated media
//! alias backing the inline embed rendering of the wiki page preview.
//!
//! The session cookie is path-scoped to `/dashboard`, so the browser
//! never sends it to the root bearer mount (`GET /media`); the
//! `<img>`/`<video>`/`<audio>` elements [`crate::md_render`] emits
//! point here instead. Same decision sequence as the bearer mount —
//! catalog lookup → `acl::can_read` semantics (no admin bypass) → the
//! content-addressed blob — so the two mounts cannot drift on the
//! governance call (the shared pieces live in `mwe_core::media`; see
//! media pipeline).
//!
//! Error posture mirrors the bearer mount: 404 for an unknown id, 403
//! for a caller outside the row's read set (the embed key in the page
//! already disclosed existence — a 404 masquerade would protect
//! nothing). A browser `<img>` fetch shows either as a broken image.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mwe_core::media;
use mwe_core::types::CatalogId;

use crate::auth::session::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;

/// Build the media sub-router (merged into the authenticated tree).
pub fn router() -> Router<DashboardState> {
    Router::new().route("/media/:catalog_id", get(fetch))
}

async fn fetch(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(raw_id): AxumPath<String>,
    request_headers: axum::http::HeaderMap,
) -> Result<Response> {
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })?;
    let catalog_id = CatalogId::parse(&raw_id)
        .map_err(|e| DashboardError::BadRequest(format!("invalid catalog_id: {e}")))?;

    let row = media::find_by_id(&state.pool, &catalog_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("media lookup: {e}")))?
        .ok_or(DashboardError::NotFound)?;

    let groups = mwe_core::enrollment::groups_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?;
    if !media::row_visible_to(&row, &user.sender_id, &groups) {
        // ACL read denial, not an admin gate: `row_visible_to` has no
        // admin bypass, so `NoAccess` ("you don't have access") instead
        // of `Forbidden` ("admin rights required") — admin would not help.
        return Err(DashboardError::NoAccess);
    }

    // Content-addressed bytes never change for an id — the hash is a
    // perfect strong validator, so the short-lived cache below
    // revalidates for free.
    let etag = format!("\"{}\"", row.sha256);
    if let Some(if_none_match) = request_headers.get(header::IF_NONE_MATCH)
        && if_none_match.to_str().ok() == Some(etag.as_str())
    {
        return Ok(axum::http::StatusCode::NOT_MODIFIED.into_response());
    }

    let blob_abs = media::blob_path(&memory.workdir, &row.sha256);
    let file = tokio::fs::File::open(&blob_abs).await.map_err(|e| {
        tracing::warn!(
            catalog_id = %catalog_id,
            error = %e,
            "dashboard media: catalog row present but blob missing"
        );
        DashboardError::Internal("catalog row exists but its bytes are missing".into())
    })?;

    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Body::from_stream(stream).into_response();
    let headers = resp.headers_mut();
    let (content_type, disposition) = if media::inline_safe(&row.mime) {
        (
            row.mime.clone(),
            format!("inline; filename=\"{catalog_id}\""),
        )
    } else {
        (
            "application/octet-stream".to_owned(),
            format!("attachment; filename=\"{catalog_id}\""),
        )
    };
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"unknown\"")),
    );
    // Shorter-lived than the bearer mount: the dashboard runs on shared
    // browsers and a year-long cache would outlive logout; the strong
    // sha256 ETag above makes revalidation effectively free.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300, must-revalidate"),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}
