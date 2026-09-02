// SPDX-License-Identifier: AGPL-3.0-or-later
//! Two guards a browser needs over the whole dashboard tree.
//!
//! [`same_origin_guard`] refuses a state-changing request that the browser
//! itself marks as coming from another site. The session cookie is
//! `SameSite=Lax`, which keeps a plain cross-site form from carrying it —
//! but `Lax` does not cover a sibling subdomain, and a cookie attribute is
//! one line of defence where two are cheap. Modern browsers say where a
//! request came from in `Sec-Fetch-Site`; older ones at least send
//! `Origin` on a POST, and that is compared against `Host`. A request that
//! carries neither (a script, a test client, a very old browser) is let
//! through: the guard refuses what is positively foreign, it does not
//! demand proof of home.
//!
//! [`security_headers`] stamps the three response headers that keep a
//! dashboard page from being framed by another site, sniffed into a
//! different type, or leaking its URL in a referrer.
//!
//! Neither touches `/mcp`, `/media` or the root OAuth endpoints: those are
//! spoken by programs, cross-site by nature, and gated by a bearer token.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Refuse a non-idempotent request the browser marks as cross-site.
pub async fn same_origin_guard(request: Request<Body>, next: Next) -> Response {
    let idempotent = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !idempotent && is_foreign(&request) {
        return (
            StatusCode::FORBIDDEN,
            "cross-site request refused: the dashboard only accepts forms it served itself",
        )
            .into_response();
    }
    next.run(request).await
}

/// `true` when the browser says this request was initiated by another site.
fn is_foreign(request: &Request<Body>) -> bool {
    let headers = request.headers();
    match headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        // `same-site` is a sibling subdomain, which is exactly the case
        // `SameSite=Lax` leaves open; `none` is the person themselves (a
        // typed URL, a bookmark).
        Some("cross-site" | "same-site") => true,
        Some(_) => false,
        None => origin_differs_from_host(headers),
    }
}

/// Without `Sec-Fetch-Site`, compare the authority `Origin` names with the
/// `Host` the request arrived at. Missing either side → not foreign.
fn origin_differs_from_host(headers: &header::HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    // `Origin: null` is what a browser sends for an opaque origin (a
    // sandboxed frame, a `data:` page): foreign by construction.
    if origin.trim() == "null" {
        return true;
    }
    let origin_authority = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .trim_end_matches('/');
    !origin_authority.eq_ignore_ascii_case(host.trim())
}

/// Add the three browser-hardening headers to every dashboard response
/// that does not already set them.
pub async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
    ] {
        headers
            .entry(name)
            .or_insert_with(|| HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/act", post(|| async { "done" }))
            .layer(axum::middleware::from_fn(same_origin_guard))
            .layer(axum::middleware::from_fn(security_headers))
    }

    async fn post_with(headers: &[(&str, &str)]) -> Response {
        let mut req = Request::builder().method(Method::POST).uri("/act");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app()
            .oneshot(req.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn a_cross_site_post_is_refused_and_a_same_origin_one_passes() {
        let refused = post_with(&[("sec-fetch-site", "cross-site")]).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let sibling = post_with(&[("sec-fetch-site", "same-site")]).await;
        assert_eq!(sibling.status(), StatusCode::FORBIDDEN);
        let same = post_with(&[("sec-fetch-site", "same-origin")]).await;
        assert_eq!(same.status(), StatusCode::OK);
        let typed = post_with(&[("sec-fetch-site", "none")]).await;
        assert_eq!(typed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn without_fetch_metadata_origin_is_compared_with_host() {
        let foreign =
            post_with(&[("origin", "https://evil.example"), ("host", "mwe.example")]).await;
        assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
        let own = post_with(&[("origin", "https://mwe.example"), ("host", "mwe.example")]).await;
        assert_eq!(own.status(), StatusCode::OK);
        let opaque = post_with(&[("origin", "null"), ("host", "mwe.example")]).await;
        assert_eq!(opaque.status(), StatusCode::FORBIDDEN);
        let bare = post_with(&[]).await;
        assert_eq!(bare.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn responses_carry_the_hardening_headers() {
        let response = post_with(&[("sec-fetch-site", "same-origin")]).await;
        let h = response.headers();
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::REFERRER_POLICY).unwrap(), "same-origin");
    }
}
