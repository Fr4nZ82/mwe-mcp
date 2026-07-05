// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration test for the admin **Dream** console
//! (`POST /dream/{light,compile,full}`) that replaced the old "DO REM NOW"
//! button. Verifies the route is admin/auth-gated and that, with the REM slots
//! resolvable, `full` runs a cycle synchronously and renders the report. The
//! happy path uses `FakeLlmBackend` overrides so no Ollama is needed.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use mwe_core::config::{LlmConfig, LlmFunction};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::llm::{FakeLlmBackend, LlmBackend};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, LlmBackendOverrides, MemoryHandles, router};

use common::{body_string, extract_cookie_value, extract_set_cookie, send};

/// Dashboard app whose REM-relevant slots resolve to `FakeLlmBackend`
/// (via `llm_overrides`), so `POST /dream/full` can run a full cycle
/// without a live Ollama endpoint.
async fn make_app_with_fake_rem() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let fake: Arc<dyn LlmBackend> = Arc::new(FakeLlmBackend::new("fake", "noop"));
    let overrides = LlmBackendOverrides::default()
        .with(LlmFunction::HubWriter, Arc::clone(&fake))
        .with(LlmFunction::RemDedupSemantic, Arc::clone(&fake));
    let memory = MemoryHandles {
        tree,
        embedder: Arc::new(FakeEmbedder::new("fake-bge-m3", 8)),
        llm_config: Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        llm_overrides: overrides,
        api_key_overrides: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        workdir: dir.path().to_path_buf(),
    };
    let state = DashboardState::new(pool, secret, blacklist, delegations).with_memory(memory);
    (router(state), dir)
}

/// Drive the setup wizard to create the admin + skip the profile wizard;
/// returns the session cookie `name=value` ready to re-present.
async fn create_admin(app: &Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=francesco@example.com&admin_id=francesco&password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "setup: {}",
        response.status()
    );
    let cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "welcome skip: {}",
        response.status()
    );
    cookie
}

#[tokio::test]
async fn dream_full_admin_runs_a_cycle_and_renders_report() {
    let (app, _dir) = make_app_with_fake_rem().await;
    let cookie = create_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/dream/full")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK, "dream/full status");
    let html = body_string(response).await;
    assert!(
        html.contains("Dream · Full REM"),
        "expected the Dream full-report page, got: {}",
        &html[..html.len().min(400)]
    );
}

#[tokio::test]
async fn dream_unauthenticated_is_rejected() {
    let (app, _dir) = make_app_with_fake_rem().await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/dream/full")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    // The authenticated tree redirects anonymous callers to the login
    // page — the handler (and the REM cycle) must never be reached.
    assert!(
        !response.status().is_success(),
        "unauthenticated rem-run must not succeed, got {}",
        response.status()
    );
}
