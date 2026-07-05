// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTTP endpoints for the skill catalog (I family).
//!
//! Two routes, both public read (no JWT — bundled skills are public
//! documentation):
//!
//! - `GET /skills` — JSON array of bundled-skill metadata
//!   (`name`, `version`, `description`, `depends_on`, `etag`,
//!   `source: bundled`).
//! - `GET /skills/<name>.md` — markdown body of a single bundled
//!   skill, with `ETag` + `Content-Type: text/markdown; charset=utf-8`.
//!   Honours `If-None-Match` for 304 short-circuits.
//!
//! Custom skills are owner-scoped and only accessible through MCP
//! (`skill_list` / `skill_fetch`) where the JWT carries the
//! `sender_id` to filter on. The HTTP endpoints stay bundled-only
//! so unauthenticated curls cannot enumerate users' custom catalogs.

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

/// Build the `/skills` sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/", get(list_bundled_skills))
        .route("/:name", get(fetch_bundled_skill))
}

async fn list_bundled_skills() -> Response {
    let Ok(skills) = mwe_core::skills::list_bundled() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let body = json!({
        "skills": skills
            .into_iter()
            .map(|s| json!({
                "name": s.name,
                "version": s.version,
                "description": s.description,
                "depends_on": s.depends_on,
                "etag": s.etag,
                "source": { "kind": "bundled" },
            }))
            .collect::<Vec<_>>(),
    });
    let mut resp = axum::Json(body).into_response();
    // Skill metadata is stable for the lifetime of the binary —
    // long client-side caching is safe. `must-revalidate` pairs
    // with the per-file ETag so a content bump is still honoured.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300, must-revalidate"),
    );
    resp
}

async fn fetch_bundled_skill(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let stem = name.strip_suffix(".md").unwrap_or(&name);
    let Ok(hit) = mwe_core::skills::fetch_bundled(stem) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some((skill, content)) = hit else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag_val = format!("\"{}\"", skill.etag);
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
        && if_none_match.to_str().ok() == Some(etag_val.as_str())
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut resp = content.into_response();
    resp.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag_val).unwrap_or(HeaderValue::from_static("\"unknown\"")),
    );
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300, must-revalidate"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn list_returns_bundled_skill_metadata() {
        let app = router();
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let skills = parsed["skills"].as_array().expect("skills array");
        assert!(!skills.is_empty());
        let names: Vec<&str> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"core"));
        assert!(names.contains(&"smart-consumer"));
    }

    #[tokio::test]
    async fn fetch_returns_markdown_with_etag() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/core.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/markdown; charset=utf-8")
        );
        let etag = resp.headers().get(header::ETAG).cloned().expect("etag");
        let bytes = body_bytes(resp).await;
        let body = String::from_utf8(bytes).unwrap();
        assert!(body.starts_with("---\n"), "must serve frontmatter");

        // Round-trip If-None-Match → 304.
        let app2 = router();
        let resp2 = app2
            .oneshot(
                Request::builder()
                    .uri("/core.md")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp2.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn fetch_unknown_returns_404() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/does-not-exist.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn fetch_accepts_name_without_md_suffix() {
        let app = router();
        let resp = app
            .oneshot(Request::builder().uri("/core").body(Body::empty()).unwrap())
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
