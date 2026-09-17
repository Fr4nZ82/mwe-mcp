// SPDX-License-Identifier: AGPL-3.0-or-later
//! Walking back through the journal.
//!
//! The engine keeps a trace for as long as `recall.trace_retention_days` says,
//! which on a running deployment is months of them; the page showed the newest
//! fifty and said nothing about the rest, so everything older than that was
//! kept and unreachable. The pages are the way back, the number rides in the
//! address so a link to one is a link to that one, and the line above the
//! table says how many there are in all and between which dates.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use mwe_core::recall_trace::{self, RecallTrace, TraceSource};
use sqlx::SqlitePool;

/// Sign in as the admin the first-run wizard creates.
async fn login_as_admin(app: &axum::Router) -> String {
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

/// `n` traces of alice's own, oldest first, each recognisable by its turn.
async fn seed_traces(pool: &SqlitePool, n: usize) {
    for i in 0..n {
        let trace = RecallTrace {
            version: recall_trace::TRACE_PAYLOAD_VERSION,
            producer: TraceSource::Ingest.as_str().to_owned(),
            turn_text: format!("turn number {i}"),
            ..RecallTrace::default()
        };
        recall_trace::record_trace(pool, TraceSource::Ingest, "alice", &trace, 30)
            .await
            .expect("record");
    }
}

/// **Every trace the memory still holds is reachable, fifty at a time.**
///
/// The first page is the newest fifty and says what the whole journal holds;
/// the second holds the rest; the number is in the address, so the second page
/// is a link somebody can keep. And a number past the end lands on the last
/// real page rather than on a blank screen — a bookmark outlives the traces it
/// pointed at.
#[tokio::test]
async fn the_journal_is_walked_page_by_page() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app).await;
    seed_traces(&pool, 120).await;

    let open = |uri: &'static str| {
        let session = session.clone();
        let app = app.clone();
        async move {
            let response = send(
                &app,
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, session)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };

    let first = open("/recall-traces").await;
    assert!(
        first.contains("<b>120</b> traces"),
        "the line above the table says what the whole journal holds: {first}"
    );
    assert!(
        first.contains("page 1 of 3"),
        "and where this page sits in it"
    );
    assert!(
        first.contains("turn number 119") && first.contains("turn number 70"),
        "the first page is the newest fifty"
    );
    assert!(
        !first.contains("turn number 69"),
        "and stops there — a page is a window"
    );
    assert!(
        first.contains("/dashboard/recall-traces?page=2"),
        "the way to the older ones is a link with the number in it"
    );

    let second = open("/recall-traces?page=2").await;
    assert!(
        second.contains("turn number 69") && second.contains("turn number 20"),
        "the second page holds the next fifty"
    );
    assert!(
        !second.contains("turn number 70"),
        "and none of the first page's"
    );
    assert!(
        second.contains("/dashboard/recall-traces?page=1"),
        "with the way back"
    );

    // The page as a browser gets it, for the screen check that goes with this
    // change: dumped only when the harness is told where to put it.
    if let Ok(dir) = std::env::var("MWE_DUMP_HTML") {
        std::fs::write(format!("{dir}/traces-page-1.html"), &first).expect("dump");
        std::fs::write(format!("{dir}/traces-page-2.html"), &second).expect("dump");
    }

    let last = open("/recall-traces?page=3").await;
    assert!(
        last.contains("turn number 0"),
        "the last page reaches the oldest trace there is"
    );

    // A link kept from when the journal was longer.
    let past_the_end = open("/recall-traces?page=40").await;
    assert!(
        past_the_end.contains("turn number 0"),
        "a page past the end lands on the last real one, not on an empty screen"
    );
}

/// **The pages are the reader's own, and so is the count.**
///
/// A person sees their own traces and nobody else's; the total above the table
/// is counted under that same scope, so it never tells them a number made of
/// traces they cannot open.
#[tokio::test]
async fn the_total_counts_only_what_this_reader_may_open() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app).await;
    seed_traces(&pool, 3).await;
    for i in 0..60 {
        let trace = RecallTrace {
            version: recall_trace::TRACE_PAYLOAD_VERSION,
            producer: TraceSource::Ingest.as_str().to_owned(),
            turn_text: format!("bob's turn {i}"),
            ..RecallTrace::default()
        };
        recall_trace::record_trace(&pool, TraceSource::Ingest, "bob", &trace, 30)
            .await
            .expect("record");
    }

    let response = send(
        &app,
        Request::builder()
            .uri("/recall-traces")
            .header(header::COOKIE, session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("<b>3</b> traces"),
        "three of her own, not sixty-three: {body}"
    );
    assert!(
        !body.contains("bob's turn"),
        "and not one of somebody else's rows"
    );
    assert!(
        !body.contains("page 1 of"),
        "three traces are one page, so there is no pager to read"
    );
}
