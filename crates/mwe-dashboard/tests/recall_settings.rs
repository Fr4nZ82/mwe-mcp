// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin recall-settings editor integration tests.
//!
//! Exercises `/dashboard/admin/recall-settings`:
//!
//! - GET — renders the knob table (defaults as placeholders).
//! - POST — atomic YAML save of the `recall:` section + in-place
//!   hot-swap of the shared handle (the re-rendered page and a fresh
//!   `Config::load` both see the new values).

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::config::{CONFIG_FILENAME, Config, LlmConfig};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};

async fn make_app() -> (Router, PathBuf, tempfile::TempDir) {
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
    let state = DashboardState::new(pool, secret, blacklist, delegations).with_memory(memory);
    let workdir = dir.path().to_path_buf();
    (router(state), workdir, dir)
}

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

#[tokio::test]
async fn anonymous_users_are_redirected_to_login() {
    let (app, _workdir, _dir) = make_app().await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-settings")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
}

#[tokio::test]
async fn page_renders_knobs_with_defaults_as_placeholders() {
    let (app, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-settings")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    for field in &[
        "recall_top_k",
        "recall_fresh_top_k",
        "max_hops",
        "pages_per_hop",
        "char_budget",
        "max_candidates",
        "decision_max_tokens",
        "due_soon_top_k",
        "due_soon_horizon_hours",
    ] {
        assert!(html.contains(field), "missing knob `{field}`: {html}");
    }
    // The defaults come from the live Rust policy, rendered as
    // placeholders — no override is set on a fresh deployment.
    let def = mwe_core::ingest::IngestPolicy::default();
    assert!(html.contains(&format!("placeholder=\"{}\"", def.nav.char_budget)));
}

#[tokio::test]
async fn save_persists_yaml_and_hot_swaps() {
    let (app, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/recall-settings")
            .header(header::COOKIE, cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("max_hops=4&due_soon_horizon_hours=24"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("saved and hot-reloaded"), "{html}");

    // Disk: the YAML round-trips through Config::load with the
    // overrides set and the untouched fields still None.
    assert!(workdir.join(CONFIG_FILENAME).exists());
    let cfg = Config::load(&workdir).expect("load saved config");
    assert_eq!(cfg.recall.max_hops, Some(4));
    assert_eq!(cfg.recall.due_soon_horizon_hours, Some(24));
    assert_eq!(cfg.recall.char_budget, None);

    // Hot-swap: a fresh GET renders the override as the input value
    // (the page reads the live shared handle, not the disk).
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-settings")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(html.contains("value=\"4\""), "{html}");
}

#[tokio::test]
async fn save_rejects_a_malformed_number_naming_the_field() {
    let (app, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/recall-settings")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("max_hops=abc"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(html.contains("max_hops"), "{html}");
    // Nothing was persisted.
    assert!(!workdir.join(CONFIG_FILENAME).exists());
}
