// SPDX-License-Identifier: AGPL-3.0-or-later
//! Static asset handler for the dashboard.
//!
//! Serves the compiled CSS, the page scripts, the fonts and the SVG
//! marks from inside the binary via [`rust_embed`]. Anything under the
//! `assets/` folder at compile time lands at `/dashboard/static/<path>`,
//! with its content type decided by extension.
//!
//! `assets/tailwind.css` is **built, never hand-edited**: the sources are
//! `tailwind/app.css` + `tailwind/tokens.css` and the Tailwind CLI compiles
//! them into it.

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

/// The URL a page uses for an embedded asset, carrying a fingerprint of the
/// file's content: `/dashboard/static/<path>?v=1a2b3c4d`.
///
/// A browser, and any cache sitting in front of the server, keys a stored copy
/// on the whole URL, query included. Taking the query from the bytes ties the
/// two together: a build that changes a file gives it a URL nobody has a copy
/// of, so it is fetched at once, and a file that did not change keeps its URL
/// and is served from the copy already held. A path that is not in the bundle
/// falls back to the crate version, so a mistyped name still renders a page —
/// the route answers `404` for it either way.
#[must_use]
pub fn asset_url(path: &str) -> String {
    let value = fingerprint(path).unwrap_or_else(|| crate::VERSION.to_string());
    format!("/dashboard/static/{path}?v={value}")
}

/// Eight lowercase hex characters standing for an embedded file's content:
/// the first four bytes of its sha256. `None` when the path is not in the
/// bundle. Four bytes stay readable at a glance and are far more than the
/// handful of files one page names need in order to differ.
fn fingerprint(path: &str) -> Option<String> {
    let hash = Assets::get(path)?.metadata.sha256_hash();
    Some(hex::encode(&hash[..4]))
}

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
        // The URL asked for carries the fingerprint of these very bytes, so
        // the answer never changes under it and a browser may hold on to it
        // without ever checking back. Public: every visitor gets the same
        // file and it holds nothing of theirs.
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The value an asset URL carries after `?v=`.
    fn query_value(url: &str) -> String {
        let (_, value) = url.split_once("?v=").expect("an asset URL carries ?v=");
        value.to_string()
    }

    #[test]
    fn an_asset_url_carries_the_fingerprint_of_its_content_not_the_version() {
        let url = asset_url("tailwind.css");
        assert!(
            url.starts_with("/dashboard/static/tailwind.css?v="),
            "{url}"
        );
        let value = query_value(&url);
        assert_eq!(value.len(), 8, "eight hex characters, got {value}");
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only, got {value}"
        );
        assert_ne!(
            value,
            crate::VERSION,
            "the query is the content's fingerprint, not the build's version"
        );
    }

    #[test]
    fn two_assets_get_two_fingerprints() {
        assert_ne!(
            query_value(&asset_url("tailwind.css")),
            query_value(&asset_url("ui.js")),
            "two different files must not share a URL"
        );
    }

    #[test]
    fn an_unknown_path_falls_back_to_the_crate_version() {
        assert_eq!(
            asset_url("no-such-file.js"),
            format!("/dashboard/static/no-such-file.js?v={}", crate::VERSION)
        );
    }

    #[tokio::test]
    async fn a_served_asset_may_be_kept_for_a_year() {
        let response = serve_asset(Path("tailwind.css".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("public, max-age=31536000, immutable")
        );
    }

    #[tokio::test]
    async fn a_missing_asset_is_not_cached() {
        let response = serve_asset(Path("no-such-file.js".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get(header::CACHE_CONTROL).is_none());
    }
}
