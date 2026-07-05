// SPDX-License-Identifier: AGPL-3.0-or-later
//! Static asset handler for the dashboard.
//!
//! Serves the compiled CSS, the PWA manifest, the service worker, and
//! the icons from inside the binary via [`rust_embed`]. Anything under
//! the `assets/` folder at compile time lands at
//! `/dashboard/static/<path>`.
//!
//! Pass 1 ships a single hand-written `app.css` so the layout has a
//! style baseline; passes 2+ may add `manifest.json`, `sw.js`, and
//! the icons referenced by the [dashboard](../../../wiki/design-notes/dashboard.md).

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

use crate::state::DashboardState;

/// Embedded asset bundle. The path is relative to this crate's
/// `Cargo.toml`; see [`crates/mwe-dashboard/assets/`](./assets/).
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// Router fragment that adds the `/static/*path` handler to whatever
/// it is merged into.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/static/*path", get(serve_asset))
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    if let Some(file) = Assets::get(&path) {
        let mime = mime_for(&path);
        let mut response = Response::new(Body::from(file.data));
        if let Ok(value) = HeaderValue::from_str(mime) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        response
    } else {
        (StatusCode::NOT_FOUND, "asset not found").into_response()
    }
}

fn mime_for(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}
