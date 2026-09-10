// SPDX-License-Identifier: AGPL-3.0-or-later
//! `instance.admin_reveal_locked` — the machine operator's lock on the
//! dashboard-wide ACL reveal.
//!
//! The point of these tests is that the lock is a lock and not a curtain.
//! Asserting that the Settings checkbox disappears would pass just as
//! happily against a build where the POST route still sets the cookie and
//! every reveal-aware surface still honours it, so each of the three doors
//! is tried in turn: the form, the route called directly, and a
//! hand-written `mwe_admin_reveal=1` cookie that never went through either.
//!
//! The reveal-aware surface used as the probe is the recall-traces journal
//! (cheap to seed: one row, no wiki tree, no embedder). It reads
//! `reveal::active` exactly like `/facts` and the wiki pages do — that
//! single predicate is where the *lens* lives, so one probe covers every
//! surface that widens through it.
//!
//! Two routes do **not** go through that predicate: the two exports read
//! `config.admin_reveal_locked` themselves, because what they hand over is
//! not a widened view of a page but the whole subtree as a file, and a
//! deployment that will not widen must not hand that over either. They are
//! the last two tests here, and they need a wiki tree the predicate probe
//! does without.
//!
//! Why they are pinned at all: on a shown instance every operator console
//! is readable by whoever presses a button, the freeze refuses changes and
//! not reading, and these two `GET`s are the only ones that give away more
//! than the page they are on. This switch is the whole of what stops them.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory_config, send,
};
use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::recall_nav::HopTrace;
use mwe_core::recall_trace::{self, RecallTrace, TraceSource};
use mwe_core::types::{Principal, WikiId};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardConfig, DashboardState, router};
use sqlx::SqlitePool;

/// A dashboard whose on-disk config either locks admin reveal or does not.
async fn make_app(admin_reveal_locked: bool) -> (Router, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let state = DashboardState::new(pool.clone(), secret, blacklist, delegations).with_config(
        DashboardConfig {
            admin_reveal_locked,
            ..DashboardConfig::default()
        },
    );
    (router(state), pool, dir)
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

/// The marker the probe looks for: a recall filed under `bob`, which alice
/// may only see through reveal.
const BOBS_TURN: &str = "quanto ha speso bob dal notaio?";

/// Seed one recall trace owned by somebody other than the signed-in admin.
async fn seed_bobs_trace(pool: &SqlitePool) -> i64 {
    let trace = RecallTrace {
        version: recall_trace::TRACE_PAYLOAD_VERSION,
        turn_text: BOBS_TURN.to_owned(),
        seed_mode: "classifier".to_owned(),
        hops: Vec::<HopTrace>::new(),
        ..RecallTrace::default()
    };
    recall_trace::record_trace(
        pool,
        TraceSource::Ingest,
        "bob",
        &trace,
        recall_trace::DEFAULT_TRACE_RETENTION_DAYS,
    )
    .await
    .expect("record bob's trace");
    sqlx::query_scalar("SELECT id FROM recall_traces WHERE sender_id = 'bob'")
        .fetch_one(pool)
        .await
        .expect("bob's row id")
}

/// GET a path with `cookie` and return the body.
async fn get_body(app: &Router, uri: &str, cookie: &str) -> String {
    body_string(
        send(
            app,
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await
}

/// Baseline. Without the lock, a hand-written reveal cookie is enough to
/// widen the journal — so the locked assertions below cannot pass for some
/// unrelated reason (a wrong cookie name, an empty journal, a broken probe).
#[tokio::test]
async fn a_hand_written_reveal_cookie_works_when_the_server_does_not_lock_it() {
    let (app, pool, _dir) = make_app(false).await;
    let cookie = login_as_admin(&app).await;
    let bob_id = seed_bobs_trace(&pool).await;

    let html = get_body(
        &app,
        "/recall-traces",
        &format!("{cookie}; mwe_admin_reveal=1"),
    )
    .await;
    assert!(
        html.contains(BOBS_TURN),
        "unlocked: the forged cookie must reveal bob's trace: {html}"
    );

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/recall-traces/{bob_id}/data"))
            .header(header::COOKIE, format!("{cookie}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "unlocked: the feed must serve bob's trace under reveal"
    );
}

/// The lock, tried from all three directions.
#[tokio::test]
async fn a_locked_reveal_cannot_be_switched_on_by_form_route_or_cookie() {
    let (app, pool, _dir) = make_app(true).await;
    let cookie = login_as_admin(&app).await;
    let bob_id = seed_bobs_trace(&pool).await;

    // 1. The route, called directly with a well-formed body — no form, no
    //    JavaScript, no checkbox involved.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/reveal")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("on=1&return_to=/dashboard/settings/me"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "locked: POST /settings/reveal must refuse"
    );
    assert!(
        extract_set_cookie(&response, "mwe_admin_reveal").is_none(),
        "locked: the refusal must not hand back a reveal cookie"
    );

    // 2. The cookie, written by hand — the shortcut that skips the route
    //    entirely. This is the assertion that matters: a build that only
    //    hid the checkbox would fail here.
    let forged = format!("{cookie}; mwe_admin_reveal=1");
    let html = get_body(&app, "/recall-traces", &forged).await;
    assert!(
        !html.contains(BOBS_TURN),
        "locked: a forged cookie must not widen the journal: {html}"
    );

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/recall-traces/{bob_id}/data"))
            .header(header::COOKIE, forged.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "locked: the feed must stay closed to a forged cookie"
    );

    // 3. Only now the presentation: the checkbox is gone and the page says
    //    who took it away, rather than offering a control that 403s.
    let settings = get_body(&app, "/settings/me", &forged).await;
    assert!(
        !settings.contains(r#"action="/dashboard/settings/reveal""#),
        "locked: the toggle form must not be rendered: {settings}"
    );
    assert!(
        settings.contains("Locked by the server"),
        "locked: Settings must explain the lock: {settings}"
    );
    assert!(
        settings.contains("instance.admin_reveal_locked"),
        "locked: Settings must name the config key that lifts it: {settings}"
    );
}

/// Unlocked deployments keep the control — the lock is opt-in, and the
/// default install is unchanged.
#[tokio::test]
async fn an_unlocked_deployment_still_offers_the_toggle() {
    let (app, _pool, _dir) = make_app(false).await;
    let cookie = login_as_admin(&app).await;

    let settings = get_body(&app, "/settings/me", &cookie).await;
    assert!(
        settings.contains(r#"action="/dashboard/settings/reveal""#),
        "unlocked: the toggle form must be rendered: {settings}"
    );
    assert!(
        !settings.contains("Locked by the server"),
        "unlocked: no lock notice: {settings}"
    );
    assert!(
        settings.contains("instance.admin_reveal_locked"),
        "the explainer must mention the lock even when it is not engaged: {settings}"
    );

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/reveal")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("on=1&return_to=/dashboard/settings/me"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    let set = extract_set_cookie(&response, "mwe_admin_reveal").expect("reveal cookie");
    assert!(set.contains("mwe_admin_reveal=1"), "{set}");
}

// ---------------------------------------------------------------------
// The two exports, which read the switch directly rather than through
// `reveal::active`.
// ---------------------------------------------------------------------

/// Put one fact on a page of alice's wiki, so the two exports have
/// something to refuse to hand over. Without it a passing test could be
/// passing because the archive is empty.
async fn capture_fact(pool: &SqlitePool, tree: &WikiTree) {
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        subject_external: None,
        slot: None,
        slot_value: None,
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse("alice").expect("wiki id"),
        page: Some(std::path::PathBuf::from("cucina.md")),
        body: "Alice likes tea".to_owned(),
        subject: "user:alice".parse::<Principal>().expect("principal"),
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
    let outcome = wiki_capture(tree, pool, embedder, req)
        .await
        .expect("capture");
    assert!(matches!(outcome.action, CaptureAction::Captured { .. }));
}

/// GET `uri` as the signed-in admin and return the status.
async fn get_status(app: &Router, uri: &str, cookie: &str) -> StatusCode {
    send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .status()
}

/// **The wiki subtree is refused at the route, not hidden on the page.**
///
/// Both halves are asserted here rather than left to the reader: unlocked
/// the same request from the same admin comes back `200`, so the refusal
/// is the lock's doing and not a broken fixture, an absent wiki or a
/// session that never authenticated.
#[tokio::test]
async fn the_wiki_export_is_refused_while_reveal_is_locked() {
    for (locked, expected) in [(false, StatusCode::OK), (true, StatusCode::FORBIDDEN)] {
        let (app, pool, tree, _dir) = make_app_with_memory_config(DashboardConfig {
            admin_reveal_locked: locked,
            ..DashboardConfig::default()
        })
        .await;
        let cookie = login_as_admin(&app).await;
        capture_fact(&pool, &tree).await;

        assert_eq!(
            get_status(&app, "/wiki/alice/export", &cookie).await,
            expected,
            "admin_reveal_locked = {locked}"
        );
    }
}

/// **And so is everything the memory holds about one person.**
///
/// Same shape, and the same reason for asserting the unlocked half: this
/// route answers `403` under the lock and an attachment without it, so a
/// build that broke the download entirely could not pass as a build that
/// locks it.
#[tokio::test]
async fn the_person_export_is_refused_while_reveal_is_locked() {
    for (locked, expected) in [(false, StatusCode::OK), (true, StatusCode::FORBIDDEN)] {
        let (app, pool, tree, _dir) = make_app_with_memory_config(DashboardConfig {
            admin_reveal_locked: locked,
            ..DashboardConfig::default()
        })
        .await;
        let cookie = login_as_admin(&app).await;
        capture_fact(&pool, &tree).await;

        assert_eq!(
            get_status(&app, "/users/alice/export", &cookie).await,
            expected,
            "admin_reveal_locked = {locked}"
        );
    }
}
