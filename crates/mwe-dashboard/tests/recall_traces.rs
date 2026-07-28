// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin recall-traces surface integration tests.
//!
//! Exercises `/dashboard/admin/recall-traces` (the journal), the per-trace
//! viewer page (3D stage mount + the full textual trace) and its `/data`
//! JSON feed — all admin-gated, and all scoped to the reader's own recalls
//! until the admin reveal switch is on.

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
use mwe_core::recall_nav::{CandidateCard, HopTrace, OpenedPage, RequestedOpen};
use mwe_core::recall_trace::{self, RecallTrace, TraceEntryPoint, TraceHit, TraceSource};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app() -> (Router, SqlitePool, tempfile::TempDir) {
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

/// A representative trace: one flat hit, a two-entry fan, one hop with a
/// vetted-away pick and one opened page, an injected block.
fn sample_trace() -> RecallTrace {
    RecallTrace {
        version: recall_trace::TRACE_PAYLOAD_VERSION,
        consumer: Some("hermes1".to_owned()),
        turn_text: "cosa cucino stasera per gli ospiti?".to_owned(),
        intent: Some("recall".to_owned()),
        seed_mode: "classifier".to_owned(),
        topics: vec!["cucina".to_owned()],
        owners: vec!["user:galadriel".to_owned()],
        flat_hits: vec![TraceHit {
            fact_id: "0197fa00-0000-7000-8000-000000000001".to_owned(),
            wiki_id: "galadriel".to_owned(),
            source_path: "wikis/galadriel/index.md".to_owned(),
            region_start: Some(120),
            region_end: Some(180),
            text: "Galadriel è celiaca — cucinare senza glutine.".to_owned(),
            score: 0.83,
            valid_to: None,
        }],
        fresh_hits: Vec::new(),
        due_soon: Vec::new(),
        entry_points: vec![
            TraceEntryPoint {
                wiki_id: "franz".to_owned(),
                page: None,
                origin: "principal".to_owned(),
                weight: 1.0,
            },
            TraceEntryPoint {
                wiki_id: "galadriel".to_owned(),
                page: Some("index.md".to_owned()),
                origin: "rag".to_owned(),
                weight: 0.83,
            },
        ],
        hops: vec![HopTrace {
            candidates: vec![CandidateCard {
                wiki_id: "galadriel".to_owned(),
                page: Some("index.md".to_owned()),
                origin: "rag".to_owned(),
                keywords: vec!["cucina".to_owned(), "salute".to_owned()],
                summary: Some("Galadriel — identity page".to_owned()),
            }],
            requested: vec![
                RequestedOpen {
                    wiki_id: "galadriel".to_owned(),
                    page: Some("index.md".to_owned()),
                    opened: true,
                },
                RequestedOpen {
                    wiki_id: "ghost".to_owned(),
                    page: None,
                    opened: false,
                },
            ],
            done: false,
            note: Some("the guest's page holds the dietary constraints".to_owned()),
            opened: vec![OpenedPage {
                wiki_id: "galadriel".to_owned(),
                page: "index.md".to_owned(),
                chars: 420,
                excerpt: "Galadriel è celiaca…".to_owned(),
                discovered: 3,
            }],
        }],
        nav_stop: Some("done".to_owned()),
        char_budget: 8000,
        chars_collected: 420,
        truncated: false,
        injected_block: Some("- (galadriel) Galadriel è celiaca [noted 2026-07-01]".to_owned()),
        rules_block: None,
        took_ms: 2150,
    }
}

#[tokio::test]
async fn anonymous_users_are_redirected_to_login() {
    let (app, _pool, _dir) = make_app().await;
    for uri in [
        "/admin/recall-traces",
        "/admin/recall-traces/1",
        "/admin/recall-traces/1/data",
    ] {
        let response = send(
            &app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert!(
            response.status().is_redirection(),
            "{uri}: {}",
            response.status()
        );
    }
}

#[tokio::test]
async fn empty_journal_renders_and_links_nothing() {
    let (app, _pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-traces")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("No traces yet"), "{html}");
}

#[tokio::test]
async fn journal_lists_and_viewer_replays_a_recorded_trace() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    // Recorded for `alice` — the signed-in admin herself. The journal is
    // scoped to the reader, so a trace filed under any other sender would
    // not be listed here (that is
    // `journal_hides_another_users_trace_until_admin_reveal`).
    recall_trace::record_trace(&pool, TraceSource::Ingest, "alice", &sample_trace())
        .await
        .expect("record");

    // The journal row: stamp, source badge, sender, counts, view link.
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-traces")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    for needle in ["ingest", "alice", "cosa cucino stasera", "1 flat", "done"] {
        assert!(html.contains(needle), "missing `{needle}`: {html}");
    }
    let id_pos = html.find("/admin/recall-traces/").expect("view link");
    let id: i64 = html[id_pos + "/admin/recall-traces/".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("row id");

    // The viewer: 3D stage mount + every textual section.
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/admin/recall-traces/{id}"))
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    for needle in [
        "trace-stage",
        "recall-trace.js",
        "Entry-point fan",
        "Hop 1",
        "the guest's page holds the dietary constraints",
        "discarded by vetting",
        "Injected into the consumer",
        "celiaca",
        "hermes1",
    ] {
        assert!(html.contains(needle), "missing `{needle}`: {html}");
    }

    // The data feed the viewer fetches.
    let response = send(
        &app,
        Request::builder()
            .uri(format!("/admin/recall-traces/{id}/data"))
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("json");
    assert_eq!(payload["source"], "ingest");
    assert_eq!(payload["sender_id"], "alice");
    assert_eq!(payload["trace"]["hops"][0]["opened"][0]["discovered"], 3);
    assert_eq!(payload["trace"]["nav_stop"], "done");

    // Unknown id → 404.
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/recall-traces/999999")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The scoping rule, pinned from both sides: an admin does **not** see
/// another user's recall by default — not in the journal, not on the viewer,
/// and above all not through the `/data` feed, which is where the recalled
/// fact bodies actually travel. Admin reveal is the one switch that lifts it.
#[tokio::test]
async fn journal_hides_another_users_trace_until_admin_reveal() {
    let (app, pool, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Alice's own recall, and bob's — recorded in that order, so bob's is
    // the newest row and cannot be missing merely for being pruned.
    let mut mine = sample_trace();
    mine.turn_text = "che pasta compro?".to_owned();
    recall_trace::record_trace(&pool, TraceSource::Ingest, "alice", &mine)
        .await
        .expect("record alice");

    let mut theirs = sample_trace();
    theirs.turn_text = "quanto ha speso bob dal notaio?".to_owned();
    theirs.flat_hits[0].text = "Bob deve 4.000 euro al notaio.".to_owned();
    recall_trace::record_trace(&pool, TraceSource::Navigate, "bob", &theirs)
        .await
        .expect("record bob");

    let bob_id: i64 = sqlx::query_scalar("SELECT id FROM recall_traces WHERE sender_id = 'bob'")
        .fetch_one(&pool)
        .await
        .expect("bob's row id");

    // Without reveal: alice's own row is listed, bob's is not.
    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/admin/recall-traces")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(
        html.contains("che pasta compro?"),
        "the admin's own trace must still be listed: {html}"
    );
    assert!(
        !html.contains("quanto ha speso bob"),
        "another user's trace must not be listed without reveal: {html}"
    );

    // …and reaching for it by id is a 404 on both the page and the feed.
    for uri in [
        format!("/admin/recall-traces/{bob_id}"),
        format!("/admin/recall-traces/{bob_id}/data"),
    ] {
        let response = send(
            &app,
            Request::builder()
                .uri(&uri)
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{uri} must 404 without reveal"
        );
    }

    // With reveal: the row is listed and the feed serves it.
    let revealed = format!("{cookie}; mwe_admin_reveal=1");
    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/admin/recall-traces")
                .header(header::COOKIE, revealed.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(
        html.contains("quanto ha speso bob"),
        "reveal must lift the admin to the whole journal: {html}"
    );

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/admin/recall-traces/{bob_id}/data"))
            .header(header::COOKIE, revealed)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("json");
    assert_eq!(payload["sender_id"], "bob");
}
