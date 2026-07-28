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
//! single predicate is where the lock lives, which is the whole reason one
//! probe is enough.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::recall_nav::HopTrace;
use mwe_core::recall_trace::{self, RecallTrace, TraceSource};
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
    recall_trace::record_trace(pool, TraceSource::Ingest, "bob", &trace)
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
        "/admin/recall-traces",
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
            .uri(format!("/admin/recall-traces/{bob_id}/data"))
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
    let html = get_body(&app, "/admin/recall-traces", &forged).await;
    assert!(
        !html.contains(BOBS_TURN),
        "locked: a forged cookie must not widen the journal: {html}"
    );

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/admin/recall-traces/{bob_id}/data"))
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
