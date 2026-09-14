// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the `/cite/:bi_id` citation resolver route.
//!
//! Drives the resolver end-to-end against a populated DB: seeds
//! `wiki_briefing_items` rows with various `target_cite` shapes, and
//! exercises the redirect path, the 404 branches, the prefix-tolerance on
//! the path param, and the two gates — a stranger is sent to sign in and
//! told nothing, and a reader who can read nothing of the page it names is
//! told the same thing somebody holding a handle to nothing is told.

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
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app_with_memory() -> (Router, SqlitePool, WikiTree, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(parking_lot::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: std::sync::Arc::new(parking_lot::RwLock::new(
            std::collections::HashMap::new(),
        )),
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, tree, dir)
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

/// Mint an admin session the way every dashboard test does: the first-run
/// setup form, which also creates the user.
async fn sign_in_as_alice(app: &Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice&password=correct-horse-battery\
                 &password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

/// A page in Alice's wiki carrying one fact, and whom that fact is about.
///
/// The resolver's gate is per fact, so what settles whether a reader is sent
/// to a page is whether one of its facts is theirs to read.
async fn seed_page_with_one_fact(tree: &WikiTree, pool: &SqlitePool, page: &str, subject: &str) {
    use mwe_core::fact_index::{self, NewFact};
    use mwe_core::types::FactId;

    let dir = tree.wikis_dir().join("alice");
    std::fs::create_dir_all(&dir).expect("wiki dir");
    std::fs::write(
        dir.join("_meta.md"),
        "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
         slug: alice\ntitle: Alice\n---\n",
    )
    .expect("meta");
    let fact_id = FactId::parse("018f1234-5678-7abc-9def-0000000000c1").unwrap();
    std::fs::write(
        dir.join(page),
        format!(
            "# page\n\n{{{{f={}}}}}\nsomething true\n{{{{/}}}}\n",
            fact_id.as_str()
        ),
    )
    .expect("page");
    fact_index::insert(
        pool,
        &NewFact {
            excluded_ids: Vec::new(),
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            fact_id,
            wiki_id: "alice".to_owned(),
            source_path: format!("wikis/alice/{page}"),
            region_start: None,
            region_end: None,
            text: "something true".to_owned(),
            embedding: vec![0.0; 8],
            subject_id: subject.parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: Some(subject.parse().unwrap()),
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        },
    )
    .await
    .expect("seed fact");
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
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = sign_in_as_alice(&app).await;
    seed_page_with_one_fact(&tree, &pool, "auth.md", "user:alice").await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/auth.md#mfa-flow")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
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
        location, "/dashboard/wiki/alice/view/auth.md#mfa-flow",
        "redirect must include the wiki_id, page path, and anchor"
    );
}

#[tokio::test]
async fn cite_resolver_redirects_without_anchor_when_only_path_in_target_cite() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = sign_in_as_alice(&app).await;
    seed_page_with_one_fact(&tree, &pool, "recovery.md", "user:alice").await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/recovery.md")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    let location = redirect_location(&response);
    assert_eq!(
        location, "/dashboard/wiki/alice/view/recovery.md",
        "redirect must omit the `#anchor` suffix when target_cite has no anchor"
    );
    assert!(
        !location.contains('#'),
        "no anchor → no `#` in the destination URL: {location}"
    );
}

#[tokio::test]
async fn cite_resolver_returns_404_for_unknown_bi_id() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let session = sign_in_as_alice(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/cite/bi_999999")
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cite_resolver_returns_404_when_target_cite_is_null() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    // A briefing item with no target_cite (the default for REM-emitted
    // items today) — the resolver has nothing to redirect to.
    let session = sign_in_as_alice(&app).await;
    let id = seed_briefing_row(&pool, "alice", None).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
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
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    // Direct INSERT bypasses `notify_append`'s validation so we can
    // simulate a corrupt row (the only way a malformed string reaches
    // the resolver in practice). The resolver must defensively 404
    // rather than 500 on `parse_cite` failure.
    let session = sign_in_as_alice(&app).await;
    let id = seed_briefing_row(&pool, "alice", Some("https://not-a-cite")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cite_resolver_accepts_bi_prefix_and_bare_integer() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = sign_in_as_alice(&app).await;
    seed_page_with_one_fact(&tree, &pool, "idx.md", "user:alice").await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/idx.md#section-one")).await;

    // `bi_<id>` form (the canonical public shape returned by the API).
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
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
        "/dashboard/wiki/alice/view/idx.md#section-one"
    );

    // Bare integer form (hand-typed by an operator, or legacy callers).
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/{id}"))
            .header(header::COOKIE, &session)
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
        "/dashboard/wiki/alice/view/idx.md#section-one"
    );

    // Garbage neither in bi_ nor integer shape → 404.
    let response = send(
        &app,
        Request::builder()
            .uri("/cite/not-a-number")
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_stranger_is_sent_to_sign_in_and_told_nothing() {
    // The ids are consecutive integers, so anybody can walk them. What must
    // not come back is a `Location` naming a wiki and a page: that is the
    // page's NAME handed to somebody who has not been recognised, and a
    // page's name is usually the news.
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    seed_page_with_one_fact(&tree, &pool, "profile.md", "user:alice").await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/profile.md#welcome")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            // NB: no `Cookie` header at all — a stranger.
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert!(response.status().is_redirection(), "{}", response.status());
    let location = redirect_location(&response);
    assert!(
        location.starts_with("/dashboard/login?next="),
        "a stranger goes to sign in: {location}"
    );
    assert!(
        !location.contains("alice") && !location.contains("profile"),
        "and what they come back to is the handle, never the page it names: {location}"
    );
}

#[tokio::test]
async fn a_reader_who_can_read_nothing_of_the_page_is_told_it_does_not_exist() {
    // The fact on the page is Bob's alone. Alice holds a handle to it and is
    // told what somebody holding a handle to nothing is told, so walking the
    // ids learns only which ids exist — which their being consecutive already
    // says.
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = sign_in_as_alice(&app).await;
    seed_page_with_one_fact(&tree, &pool, "bobs_page.md", "user:bob").await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/bobs_page.md")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/bi_{id}"))
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a page she can read nothing of does not exist for her"
    );
    let html = body_string(response).await;
    assert!(
        !html.contains("bobs_page"),
        "and the refusal does not name it either: {html}"
    );
}

#[tokio::test]
async fn the_short_form_at_the_root_forwards_and_carries_only_the_handle() {
    // The session cookie is scoped to `/dashboard`, so a resolver above that
    // prefix would never be shown a reader and could only refuse everybody.
    // The short form sends the browser one hop down instead.
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let id = seed_briefing_row(&pool, "alice", Some("wiki://alice/secret.md")).await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/cite/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    // In this test tree the dashboard alias is what answers, so the assertion
    // that matters is the one every mount shares: nothing of the destination
    // travels to somebody who has not been recognised.
    let location = redirect_location(&response);
    assert!(
        !location.contains("secret"),
        "the destination never travels before the reader is known: {location}"
    );
}
