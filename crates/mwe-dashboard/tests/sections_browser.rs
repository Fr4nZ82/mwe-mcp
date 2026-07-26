// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Sections tab of the memory browser — the smart-family half of
//! `/dashboard/facts`, listing `wiki_sections` instead of `fact_index`.
//!
//! Drives the live router: the tab bar on both halves, the listing and
//! its filters, the per-wiki ACL (a section is readable because its
//! **wiki** is), and the read-only posture.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::config::LlmConfig;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::sections::{self, NewSection, SmartWikiRow};
use mwe_core::types::Principal;
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app() -> (Router, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let memory = MemoryHandles {
        tree,
        embedder: Arc::new(FakeEmbedder::new("fake-bge-m3", 8)),
        llm_config: Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        workdir: dir.path().to_path_buf(),
    };
    let state = DashboardState::new(
        pool.clone(),
        secret,
        Arc::new(BlacklistCache::new()),
        Arc::new(DelegationCache::new()),
    )
    .with_memory(memory);
    (router(state), pool, dir)
}

/// Run the setup wizard as `alice` and return her session cookie.
async fn login_as_admin(app: &Router) -> String {
    let setup = send(
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
    assert!(setup.status().is_redirection(), "{}", setup.status());
    let cookie = extract_cookie_value(&extract_set_cookie(&setup, "mwe_session").expect("cookie"));
    let skip = send(
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
    assert!(skip.status().is_redirection(), "{}", skip.status());
    cookie
}

/// Register a smart wiki and index one page of sections into it.
async fn seed_sections(pool: &SqlitePool, wiki_id: &str, owner: &str, shared_with: Vec<Principal>) {
    sections::upsert_smart_wiki(
        pool,
        &SmartWikiRow {
            wiki_id: wiki_id.to_owned(),
            owner_id: owner.parse().expect("owner principal"),
            shared_with,
            project_id: Some("abc123".to_owned()),
            wiki_type: "project".to_owned(),
        },
    )
    .await
    .expect("register smart wiki");

    let page = format!("wikis/{wiki_id}/auth.md");
    sections::replace_page_sections(
        pool,
        &page,
        &[
            NewSection {
                wiki_id: wiki_id.to_owned(),
                source_path: page.clone(),
                section_ord: 0,
                heading_path: Some("Auth".to_owned()),
                text: format!("{wiki_id} rotates its JWT signing key nightly."),
                embedding: vec![0.1; 8],
            },
            NewSection {
                wiki_id: wiki_id.to_owned(),
                source_path: page.clone(),
                section_ord: 1,
                heading_path: Some("Auth > MFA".to_owned()),
                text: format!("{wiki_id} stores MFA recovery codes hashed."),
                embedding: vec![0.2; 8],
            },
        ],
    )
    .await
    .expect("index sections");
}

async fn get(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
    let response = send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = response.status();
    (status, body_string(response).await)
}

#[tokio::test]
async fn both_halves_of_the_memory_browser_carry_the_tab_bar() {
    let (app, _pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let (status, facts) = get(&app, "/facts", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        facts.contains("/dashboard/facts/sections"),
        "the Facts page must link to the Sections tab: {facts}"
    );

    let (status, sections_page) = get(&app, "/facts/sections", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        sections_page.contains("/dashboard/facts"),
        "the Sections page must link back to Facts: {sections_page}"
    );
}

#[tokio::test]
async fn sections_page_lists_the_owner_s_indexed_sections() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    seed_sections(&pool, "alice-proj", "user:alice", Vec::new()).await;

    let (status, html) = get(&app, "/facts/sections", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("alice-proj"), "{html}");
    assert!(html.contains("rotates its JWT signing key"), "{html}");
    assert!(html.contains("MFA recovery codes"), "{html}");
    // The heading chain is surfaced so the operator can place the block.
    assert!(html.contains("Auth &gt; MFA"), "{html}");
    // Each row deep-links to its page in the wiki viewer — the actionable
    // surface, since a section itself is read-only.
    assert!(
        html.contains("/dashboard/wiki/alice-proj/view/auth.md"),
        "row must link to the page it came from: {html}"
    );
    // Read-only: none of the fact verbs are offered here.
    assert!(
        !html.contains("/facts/sections/"),
        "no per-row actions: {html}"
    );
}

#[tokio::test]
async fn sections_page_honours_the_per_wiki_acl() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    // Alice owns one project; Bob owns another she was never granted.
    seed_sections(&pool, "alice-proj", "user:alice", Vec::new()).await;
    seed_sections(&pool, "bob-proj", "user:bob", Vec::new()).await;

    let (status, html) = get(&app, "/facts/sections", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("alice-proj"), "{html}");
    assert!(
        !html.contains("bob-proj"),
        "another owner's sections must not leak: {html}"
    );
}

#[tokio::test]
async fn a_shared_wiki_becomes_visible_through_the_registry_row_alone() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    seed_sections(&pool, "bob-proj", "user:bob", Vec::new()).await;

    let (_, before) = get(&app, "/facts/sections", &cookie).await;
    assert!(!before.contains("bob-proj"), "{before}");

    // Bob shares the wiki with alice. This is a ONE-ROW write — the
    // sections are untouched — and it is enough to open the read window.
    sections::upsert_smart_wiki(
        &pool,
        &SmartWikiRow {
            wiki_id: "bob-proj".to_owned(),
            owner_id: "user:bob".parse().unwrap(),
            shared_with: vec![Principal::User("alice".to_owned())],
            project_id: Some("abc123".to_owned()),
            wiki_type: "project".to_owned(),
        },
    )
    .await
    .expect("share");

    let (_, after) = get(&app, "/facts/sections", &cookie).await;
    assert!(
        after.contains("bob-proj"),
        "a grantee must see the shared wiki's sections: {after}"
    );
}

#[tokio::test]
async fn filters_narrow_the_listing_and_survive_in_the_pager_links() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    seed_sections(&pool, "alice-proj", "user:alice", Vec::new()).await;

    let (status, html) = get(&app, "/facts/sections?text=MFA", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("MFA recovery codes"), "{html}");
    assert!(
        !html.contains("rotates its JWT signing key"),
        "the text filter must exclude the other section: {html}"
    );

    // A filter that matches nothing renders the empty state, not an error.
    let (status, empty) = get(&app, "/facts/sections?text=nothing-matches", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.contains("No sections to show"), "{empty}");
}

#[tokio::test]
async fn an_account_with_no_readable_smart_wiki_gets_the_empty_state() {
    let (app, _pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let (status, html) = get(&app, "/facts/sections", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("No smart wiki is readable"), "{html}");
}

#[tokio::test]
async fn the_sections_page_requires_a_session() {
    let (app, _pool, _dir) = make_app().await;
    let response = send(
        &app,
        Request::builder()
            .uri("/facts/sections")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "anonymous access must bounce to the login page, got {}",
        response.status()
    );
}
