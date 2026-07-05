// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTTP endpoints under `/connect/*` — operator-facing onboarding
//! surface.
//!
//! Today this module ships the **hook bundle** sub-tree:
//!
//! - `GET /connect/hooks` — JSON list of consumers for which a hook
//!   template is bundled (`{ consumers: [{ name, label, status }] }`).
//! - `GET /connect/hooks/<consumer>.json` — the full JSON envelope
//!   for that consumer: `consumer`, `consumer_label`,
//!   `target_file`, `installation`, `policy`, `tools_referenced`,
//!   and the `snippet` block the operator copies into their
//!   consumer's settings (when applicable — Codex returns
//!   `snippet: null` until `OpenAI` ships a hook surface).
//!
//! Both routes are public read (no JWT). The hook templates are
//! configuration boilerplate; nothing about them is sensitive. The
//! dashboard `/connect` page will link to these endpoints
//! so the operator can copy-paste straight from the browser.
//!
//! See the [tool reference](../../../wiki/protocol/tool-reference.md)
//! for the design rationale and the explicit constraint that the MCP
//! protocol has no way for the server to register hooks on the
//! client — mwe-mcp can only *recommend* them.

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Bundled hook templates, one per consumer. Order is preserved in
/// the `list` response so the dashboard renders consumers in a
/// stable sequence.
const HOOK_TEMPLATES: &[(&str, &str)] = &[
    (
        "claude-code",
        include_str!("hooks_templates/claude-code.json"),
    ),
    ("codex", include_str!("hooks_templates/codex.json")),
];

/// Build the `/connect` sub-router.
pub fn router() -> Router {
    Router::new()
        .route("/hooks", get(list_hook_templates))
        .route("/hooks/:consumer", get(fetch_hook_template))
}

fn lookup_template(name: &str) -> Option<&'static str> {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    HOOK_TEMPLATES
        .iter()
        .find(|(consumer, _)| *consumer == stem)
        .map(|(_, body)| *body)
}

fn etag_for(body: &str) -> String {
    let hash = Sha256::digest(body.as_bytes());
    format!("\"{}\"", hex::encode(&hash[..16]))
}

async fn list_hook_templates() -> Response {
    let mut consumers = Vec::with_capacity(HOOK_TEMPLATES.len());
    for (name, body) in HOOK_TEMPLATES {
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let label = parsed
            .get("consumer_label")
            .and_then(Value::as_str)
            .unwrap_or(name);
        let status = parsed
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("ready");
        consumers.push(json!({
            "name": name,
            "label": label,
            "status": status,
            "url": format!("/connect/hooks/{name}.json"),
        }));
    }
    let body = json!({ "consumers": consumers });
    let mut resp = axum::Json(body).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300, must-revalidate"),
    );
    resp
}

async fn fetch_hook_template(Path(consumer): Path<String>, headers: HeaderMap) -> Response {
    let Some(body) = lookup_template(&consumer) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag_val = etag_for(body);
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
        && if_none_match.to_str().ok() == Some(etag_val.as_str())
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut resp = body.to_owned().into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag_val).unwrap_or(HeaderValue::from_static("\"unknown\"")),
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

    #[test]
    fn every_bundled_template_is_valid_json_with_required_keys() {
        for (name, body) in HOOK_TEMPLATES {
            let parsed: Value =
                serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                parsed.get("consumer").and_then(Value::as_str) == Some(*name),
                "{name}: consumer field must match filename stem"
            );
            assert!(
                parsed.get("milestone").is_some(),
                "{name}: must declare milestone"
            );
            assert!(
                parsed.get("spec_reference").is_some(),
                "{name}: must reference the planning spec"
            );
            assert!(
                parsed.get("note").is_some()
                    || parsed.get("policy").is_some()
                    || parsed.get("installation").is_some(),
                "{name}: must include human-readable guidance"
            );
        }
    }

    #[tokio::test]
    async fn list_returns_bundled_consumers_in_order() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hooks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let parsed: Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        let consumers = parsed["consumers"].as_array().expect("consumers array");
        let names: Vec<&str> = consumers
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["claude-code", "codex"]);
        let urls: Vec<&str> = consumers
            .iter()
            .map(|c| c["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "/connect/hooks/claude-code.json",
                "/connect/hooks/codex.json",
            ]
        );
    }

    #[tokio::test]
    async fn fetch_returns_claude_code_envelope_with_snippet_block() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hooks/claude-code.json")
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
            Some("application/json; charset=utf-8")
        );
        let etag = resp.headers().get(header::ETAG).cloned().expect("etag");
        let parsed: Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["consumer"], "claude-code");
        let snippet = parsed["snippet"].as_object().expect("snippet object");
        assert!(snippet.contains_key("hooks"));
        let hooks = snippet["hooks"].as_object().expect("hooks object");
        // The OAuth bridge ships only a token-less SessionStart nudge; per-prompt
        // recall is model-driven, so there is no UserPromptSubmit hook.
        assert!(
            hooks.contains_key("SessionStart"),
            "claude-code must surface the SessionStart nudge"
        );
        assert!(
            !hooks.contains_key("UserPromptSubmit"),
            "per-prompt recall is model-driven — no UserPromptSubmit hook"
        );

        // 304 round-trip via If-None-Match.
        let app2 = router();
        let resp2 = app2
            .oneshot(
                Request::builder()
                    .uri("/hooks/claude-code.json")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp2.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn fetch_returns_codex_envelope_without_snippet() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hooks/codex.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let parsed: Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(parsed["consumer"], "codex");
        assert_eq!(parsed["status"], "no_native_hook_mechanism_yet");
        assert!(parsed["snippet"].is_null(), "codex snippet must be null");
        assert!(parsed["fallback_pattern"].is_string());
    }

    #[tokio::test]
    async fn fetch_accepts_consumer_without_json_suffix() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hooks/claude-code")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn fetch_unknown_consumer_returns_404() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/hooks/does-not-exist.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
