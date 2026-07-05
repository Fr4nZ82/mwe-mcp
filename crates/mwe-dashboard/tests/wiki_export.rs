// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dashboard wiki-export route — auth + payload integration tests.
//!
//! GET `/dashboard/wiki/:id/export` is the admin-only download of the
//! wiki subtree as a portable full-marker tar (`mwe_core::export`). The
//! archive-content round-trip is covered by the `mwe_core::export` unit
//! tests; here we pin the route contract: admin gets the attachment,
//! non-admins get a 403, unknown wikis a 404.

mod common;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::types::{Principal, WikiId};
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;

async fn login_as_admin(app: &Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice&password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

/// Mint a non-admin session via the invitation cycle (same gesture as
/// the op-log revert admin-gate test).
async fn login_as_non_admin(app: &Router, admin_cookie: &str) -> String {
    let create_resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, admin_cookie)
            .body(Body::from("user_id=bilbo&email=bilbo@example.com&aliases="))
            .unwrap(),
    )
    .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let create_html = body_string(create_resp).await;
    let prefix = "/dashboard/accept-invite/";
    let start = create_html.find(prefix).expect("invitation link");
    let after = &create_html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .unwrap();
    let invitation_id = &after[..end];
    let accept_resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=bilbo-pw-secret-12&password_confirm=bilbo-pw-secret-12",
            ))
            .unwrap(),
    )
    .await;
    extract_cookie_value(&extract_set_cookie(&accept_resp, "mwe_session").expect("cookie"))
}

async fn capture_fact(pool: &SqlitePool, tree: &WikiTree, page: &str, body: &str) {
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse("alice").unwrap(),
        page: PathBuf::from(page),
        body: body.to_owned(),
        owner: "user:alice".parse::<Principal>().unwrap(),
        allow: vec![],
        sender: None,
        fact_type: None,
        topics: vec![],
        dedup_threshold: Some(1.01),
        valid_from: None,
        valid_to: None,
        style: None,
        page_description: None,
        salience: None,
    };
    let outcome = wiki_capture(tree, pool, embedder, req).await.unwrap();
    assert!(matches!(outcome.action, CaptureAction::Captured { .. }));
}

async fn body_bytes(response: Response<Body>) -> Vec<u8> {
    use http_body_util::BodyExt;
    response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec()
}

fn untar(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut archive = tar::Archive::new(bytes);
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let path = entry.path().expect("path").to_string_lossy().into_owned();
        let mut content = String::new();
        entry.read_to_string(&mut content).expect("utf-8 content");
        out.insert(path, content);
    }
    out
}

/// Admin download: 200, tar content-type, attachment disposition, and
/// the archive carries the admin's identity wiki with the captured
/// fact rewritten to a full self-describing marker.
#[tokio::test]
async fn export_serves_tar_attachment_to_admin() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    capture_fact(&pool, &tree, "index.md", "Alice likes tea").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/export")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert_eq!(content_type, "application/x-tar");
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert_eq!(disposition, "attachment; filename=\"alice-export.tar\"");

    let entries = untar(&body_bytes(response).await);
    assert!(entries.contains_key("alice/_meta.md"), "{entries:?}");
    let index = entries.get("alice/index.md").expect("index page travels");
    assert!(
        index.contains("owner=user:alice") && index.contains("Alice likes tea"),
        "captured region must travel as a full marker: {index}"
    );
}

/// The export is admin-only: a non-admin session gets a 403 before any
/// archive work happens.
#[tokio::test]
async fn export_returns_403_for_non_admin() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;
    let bilbo_cookie = login_as_non_admin(&app, &admin_cookie).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/export")
            .header(header::COOKIE, bilbo_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Unknown wiki id → 404, mirroring the wiki view's not-found policy.
#[tokio::test]
async fn export_returns_404_for_unknown_wiki() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/does-not-exist/export")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The admin wiki view advertises the export affordance.
#[tokio::test]
async fn wiki_view_renders_export_link_for_admin() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("/dashboard/wiki/alice/export"),
        "admin view must link the export route: {html}"
    );
}
