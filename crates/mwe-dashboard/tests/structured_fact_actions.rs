// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the structured engine-direct fact actions at
//! `POST /dashboard/facts/:fact_id/acl` and `.../validity`.
//!
//! These replaced the old unapplyable ACL/validity chat-bridge: they hit
//! the engine directly (owner-or-admin gated, standard-wikis only),
//! mint a born-applied `wiki_promote` receipt, and 303-redirect the
//! operator onto that revertible receipt.
//!
//! The owner-or-admin gate's pure logic is unit-tested inside
//! `routes::facts`; here we drive the routes end-to-end against a
//! populated DB + tempdir wiki tree to assert: the engine effect (the
//! ACL / validity column moved), the receipt row exists, the redirect
//! lands on the receipt's open-in-chat page, and a smart wiki is refused
//! with 422 leaving the fact untouched.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::config::LlmConfig;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::types::{FactId, Principal, WikiId};
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
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, tree, dir)
}

/// Setup as the admin "alice", whose identity wiki `alice` owns the
/// captured facts (so owner-or-admin always passes for these fixtures).
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

/// Drop a standard `wiki-user` wiki at `<workdir>/wikis/alice`.
fn seed_alice_wiki(tree: &WikiTree) {
    let dir = tree.wikis_dir().join("alice");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: alice\n\
                wiki_type: wiki-user\n\
                parent_wiki_id: null\n\
                slug: alice\n\
                title: Alice\n\
                acl_default: 'user:alice'\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Drop a SMART wiki at `<workdir>/wikis/proj` (carries `smart: true`).
fn seed_smart_wiki(tree: &WikiTree) {
    let dir = tree.wikis_dir().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: proj\n\
                wiki_type: wiki-tech\n\
                parent_wiki_id: null\n\
                slug: proj\n\
                title: Project\n\
                acl_default: 'user:alice'\n\
                smart: true\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

async fn capture_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    page: &str,
    body: &str,
) -> FactId {
    use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: std::path::PathBuf::from(page),
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
    match outcome.action {
        CaptureAction::Captured { .. } => outcome.fact_id,
        other => panic!("expected Captured, got {other:?}"),
    }
}

#[tokio::test]
async fn acl_action_changes_engine_column_mints_receipt_and_redirects() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fid = capture_fact(&pool, &tree, "alice", "index.md", "Alice usa la bici").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/acl", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("owner=user:alice&allow=group:famiglia"))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "ACL action must redirect, got {}",
        response.status()
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        location.starts_with("/dashboard/proposals/") && location.ends_with("/open-in-chat"),
        "must land on the receipt's open-in-chat page; got {location:?}"
    );

    // Engine effect: the allow set widened.
    let row = mwe_core::fact_index::find_by_id(&pool, &fid)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        row.allow_ids,
        vec!["group:famiglia".parse::<Principal>().unwrap()],
        "allow set must move on disk/DB"
    );

    // A born-applied wiki_promote receipt exists.
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipts, 1, "exactly one receipt minted");

    // A disclosure_audit row was written (widening).
    let widened: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM disclosure_audit WHERE fact_id = ? AND widening = 1",
    )
    .bind(fid.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(widened, 1, "widening audit row");

    // A structure_applied notice event was posted.
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_events WHERE kind = 'structure_applied' AND fact_id = ?",
    )
    .bind(fid.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "structure_applied notice posted");
}

#[tokio::test]
async fn validity_action_sets_bounds_and_redirects() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fid = capture_fact(
        &pool,
        &tree,
        "alice",
        "index.md",
        "Alice lavora alla startup",
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/validity", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("valid_from=2026-01-01&valid_to=2026-06-01"))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "validity action must redirect, got {}",
        response.status()
    );

    let row = mwe_core::fact_index::find_by_id(&pool, &fid)
        .await
        .unwrap()
        .expect("row");
    assert!(
        row.valid_from
            .as_deref()
            .unwrap_or("")
            .starts_with("2026-01-01"),
        "valid_from set: {:?}",
        row.valid_from
    );
    assert!(
        row.valid_to
            .as_deref()
            .unwrap_or("")
            .starts_with("2026-06-01"),
        "valid_to set: {:?}",
        row.valid_to
    );

    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipts, 1, "validity_edit receipt minted");
}

#[tokio::test]
async fn validity_action_requires_at_least_one_bound() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fid = capture_fact(&pool, &tree, "alice", "index.md", "Alice ha un cane").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/validity", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("valid_from=&valid_to="))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "both-empty must be 422"
    );
    // Nothing was minted.
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0, "no receipt on a no-op validity submit");
}

#[tokio::test]
async fn structured_actions_are_refused_on_smart_wikis() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_smart_wiki(&tree);
    let fid = capture_fact(&pool, &tree, "proj", "index.md", "Il progetto usa Rust").await;

    // ACL refused.
    let acl = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/acl", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("owner=user:alice&allow=group:famiglia"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        acl.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "smart-wiki ACL action must be refused with 422"
    );
    let html = body_string(acl).await;
    assert!(
        html.contains("smart"),
        "refusal must mention smart wikis: {html}"
    );

    // Validity refused too.
    let validity = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/validity", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("valid_from=2026-01-01&valid_to="))
            .unwrap(),
    )
    .await;
    assert_eq!(
        validity.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "smart-wiki validity action must be refused with 422"
    );

    // The fact is untouched on both surfaces.
    let row = mwe_core::fact_index::find_by_id(&pool, &fid)
        .await
        .unwrap()
        .expect("row");
    assert!(
        row.allow_ids.is_empty(),
        "ACL untouched on a refused smart-wiki action"
    );
    assert!(row.valid_from.is_none(), "validity untouched");
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM structure_proposals WHERE kind = 'wiki_promote'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0, "no receipt on a refused smart-wiki action");
}

#[tokio::test]
async fn acl_action_receipt_reverts_via_proposals_route() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fid = capture_fact(&pool, &tree, "alice", "index.md", "Alice va in montagna").await;

    // Apply an ACL change.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/acl", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("owner=user:alice&allow=group:famiglia"))
            .unwrap(),
    )
    .await;
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // Pull the proposal_id out of `/dashboard/proposals/<id>/open-in-chat`.
    let proposal_id = location
        .strip_prefix("/dashboard/proposals/")
        .and_then(|s| s.strip_suffix("/open-in-chat"))
        .expect("receipt id in redirect location")
        .to_owned();

    // Revert via the existing proposals action route — the born-applied
    // receipt is a wiki_promote variant it already handles.
    let revert = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/proposals/{proposal_id}/revert"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        revert.status().is_redirection(),
        "revert must redirect, got {}",
        revert.status()
    );

    // The ACL is restored.
    let row = mwe_core::fact_index::find_by_id(&pool, &fid)
        .await
        .unwrap()
        .expect("row");
    assert!(
        row.allow_ids.is_empty(),
        "revert must restore the prior (empty) allow set; got {:?}",
        row.allow_ids
    );
}

/// The dashboard delete owns both halves of retirement: the `deleted_at`
/// tombstone AND the excision of the region's bytes from the page
/// (`capture::wiki_forget` under the route).
#[tokio::test]
async fn delete_action_strips_the_regions_bytes_from_disk() {
    let (app, pool, tree, dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fid = capture_fact(
        &pool,
        &tree,
        "alice",
        "index.md",
        "Alice usa il monopattino",
    )
    .await;
    let page_abs = dir.path().join("wikis/alice/index.md");
    assert!(
        std::fs::read_to_string(&page_abs)
            .unwrap()
            .contains(fid.as_str()),
        "precondition: the fact is rendered on disk"
    );

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{}/delete", fid.as_str()))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "delete must redirect, got {}",
        response.status()
    );

    // DB half: tombstoned.
    let row = mwe_core::fact_index::find_by_id(&pool, &fid)
        .await
        .unwrap()
        .expect("row");
    assert!(row.deleted_at.is_some(), "fact must be tombstoned");
    // Disk half: the region's bytes left the page.
    let page = std::fs::read_to_string(&page_abs).unwrap();
    assert!(
        !page.contains(fid.as_str()) && !page.contains("Alice usa il monopattino"),
        "deleted fact's region must be excised from the page: {page}"
    );
}

/// Drop a standard `wiki-user` wiki at `<workdir>/wikis/bob`.
fn seed_bob_wiki(tree: &WikiTree) {
    let dir = tree.wikis_dir().join("bob");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: bob\n\
                wiki_type: wiki-user\n\
                parent_wiki_id: null\n\
                slug: bob\n\
                title: Bob\n\
                acl_default: 'user:bob'\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Capture a fact owned by an arbitrary principal (cross-user fixtures).
async fn capture_fact_owned(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    page: &str,
    body: &str,
    owner: &str,
) -> FactId {
    use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: std::path::PathBuf::from(page),
        body: body.to_owned(),
        owner: owner.parse::<Principal>().unwrap(),
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
    match outcome.action {
        CaptureAction::Captured { .. } => outcome.fact_id,
        other => panic!("expected Captured, got {other:?}"),
    }
}

/// `/facts` is ACL-projected: an admin sees another user's private fact
/// only with the reveal cookie set (the lens that makes the owner-or-admin
/// actions reach it). Without reveal it is filtered out.
#[tokio::test]
async fn facts_list_hides_other_users_facts_until_admin_reveal() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // alice, admin — not bob
    seed_bob_wiki(&tree);
    let _fid = capture_fact_owned(
        &pool,
        &tree,
        "bob",
        "index.md",
        "Bob private secret note",
        "user:bob",
    )
    .await;

    // Without reveal: bob's private fact is ACL-filtered out.
    let plain = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/facts")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(plain.status(), StatusCode::OK);
    let plain_html = body_string(plain).await;
    assert!(
        !plain_html.contains("Bob private secret note"),
        "admin must NOT see bob's fact on /facts without reveal"
    );

    // With reveal: the per-row ACL gate is skipped and bob's fact surfaces.
    let revealed = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/facts")
            .header(header::COOKIE, format!("{cookie}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revealed.status(), StatusCode::OK);
    let revealed_html = body_string(revealed).await;
    assert!(
        revealed_html.contains("Bob private secret note"),
        "admin WITH reveal must see bob's fact on /facts: {revealed_html}"
    );
}
