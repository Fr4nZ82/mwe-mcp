// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the `/cite/:bi_id` citation resolver route.
//!
//! Drives the public resolver route end-to-end against a populated DB:
//! seeds `wiki_briefing_items` rows with various `target_cite` shapes,
//! exercises the redirect path, the 404 branches, the prefix-tolerance
//! on the path param, and the auth posture (anonymous on `/cite/`,
//! auth-gated on the destination).

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, send};
use mwe_core::config::LlmConfig;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app_with_memory() -> (Router, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree,
        embedder,
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, dir)
}

/// Seed one row in `wiki_briefing_items` with the given `target_cite`
/// and return its `id` (the i64 primary key, which the resolver
/// accepts in either `bi_<id>` or bare `<id>` form).
async fn seed_briefing_row(pool: &SqlitePool, wiki_id: &str, target_cite: Option<&str>) -> i64 {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_briefing_items
            (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, processed_at)
         VALUES (?, 'rem', 'rem:test_seed', 'seed topic', 'seed body', NULL, ?, ?, NULL)
         RETURNING id",
    )
    .bind(wiki_id)
    .bind(&now)
    .bind(target_cite)
    .fetch_one(pool)
    .await
    .expect("seed briefing row");
    row.0
}

fn redirect_location(response: &axum::http::Response<Body>) -> String {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn cite_resolver_redirects_to_wiki_page_and_anchor_when_target_cite_present() {
    let (app, pool, _dir) = make_app_with_memory().await;
    let id = seed_briefing_row(
        &pool,
        "alice",
        Some("wiki://alice/modules/auth.md#mfa-flow"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "expected 302, got {}",
        response.status()
    );
    let location = redirect_location(&response);
    assert_eq!(
        location, "/dashboard/wiki/alice/view/modules/auth.md#mfa-flow",
        "redirect must include the wiki_id, page path, and anchor"
    );
}

#[tokio::test]
async fn cite_resolver_redirects_without_anchor_when_only_path_in_target_cite() {
    let (app, pool, _dir) = make_app_with_memory().await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/decisions/recovery.md")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    let location = redirect_location(&response);
    assert_eq!(
        location, "/dashboard/wiki/alice/view/decisions/recovery.md",
        "redirect must omit the `#anchor` suffix when target_cite has no anchor"
    );
    assert!(
        !location.contains('#'),
        "no anchor → no `#` in the destination URL: {location}"
    );
}

#[tokio::test]
async fn cite_resolver_returns_404_for_unknown_bi_id() {
    let (app, _pool, _dir) = make_app_with_memory().await;

    let response = send(
        &app,
        Request::builder()
            .uri("/cite/bi_999999")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cite_resolver_returns_404_when_target_cite_is_null() {
    let (app, pool, _dir) = make_app_with_memory().await;
    // A briefing item with no target_cite (the default for REM-emitted
    // items today) — the resolver has nothing to redirect to.
    let id = seed_briefing_row(&pool, "alice", None).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The body should not leak the row's existence to a probing caller;
    // the standard "Page not found." page is rendered (see
    // `DashboardError::user_facing_message`).
    let html = body_string(response).await;
    assert!(
        html.contains("Page not found"),
        "404 page must render the generic not-found copy: {html}"
    );
}

#[tokio::test]
async fn cite_resolver_returns_404_for_malformed_target_cite() {
    let (app, pool, _dir) = make_app_with_memory().await;
    // Direct INSERT bypasses `notify_append`'s validation so we can
    // simulate a corrupt row (the only way a malformed string reaches
    // the resolver in practice). The resolver must defensively 404
    // rather than 500 on `parse_cite` failure.
    let id = seed_briefing_row(&pool, "alice", Some("https://not-a-cite")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cite_resolver_accepts_bi_prefix_and_bare_integer() {
    let (app, pool, _dir) = make_app_with_memory().await;
    let id = seed_briefing_row(
        &pool,
        "alice",
        Some("wiki://alice/notes/idx.md#section-one"),
    )
    .await;

    // `bi_<id>` form (the canonical public shape returned by the API).
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "bi_-prefixed must resolve"
    );
    assert_eq!(
        redirect_location(&response),
        "/dashboard/wiki/alice/view/notes/idx.md#section-one"
    );

    // Bare integer form (hand-typed by an operator, or legacy callers).
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "bare integer must resolve too, got {}",
        response.status()
    );
    assert_eq!(
        redirect_location(&response),
        "/dashboard/wiki/alice/view/notes/idx.md#section-one"
    );

    // Garbage neither in bi_ nor integer shape → 404.
    let response = send(
        &app,
        Request::builder()
            .uri("/cite/not-a-number")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cite_resolver_is_anonymous_accessible() {
    // The resolver is public: an unauthenticated request (no Cookie
    // header at all) must redirect when the row resolves cleanly. Auth
    // enforcement lives on the destination `/dashboard/wiki/...` page,
    // not on the resolver.
    let (app, pool, _dir) = make_app_with_memory().await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/index.md#welcome")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            // NB: no `Cookie` header at all — anonymous caller.
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "anonymous caller must reach the resolver; got {}",
        response.status()
    );
    assert_eq!(
        redirect_location(&response),
        "/dashboard/wiki/alice/view/index.md#welcome"
    );

    // And the same route is reachable via the discoverable
    // `/dashboard/cite/:bi_id` alias the dashboard router mounts in
    // its public tree (no session middleware on that branch either).
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
}
