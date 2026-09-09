// SPDX-License-Identifier: AGPL-3.0-or-later
//! `GET /dashboard/proposals` and `GET /dashboard/proposals/:id` — the
//! record of what the memory rearranged, read.
//!
//! Three things are asserted here, and they are the three that would be
//! expensive to get wrong:
//!
//! 1. **Who sees which rows.** A person sees the ones addressed to them
//!    and nothing else; the admin also sees the ones addressed to nobody;
//!    admin reveal lifts both to every recipient. A row outside the scope
//!    answers `404`, not `403`.
//! 2. **The page explains itself without a model.** Every row prints a
//!    sentence built from the stored note — that is what makes these
//!    pages usable on a read-only instance, where the chat that would
//!    otherwise explain them is not rendered at all.
//! 3. **A proposal is found by name, not by position.** The listing shows
//!    a page of the newest rows; the by-id page must still open one that
//!    fell behind that window, because every link to a proposal outlives
//!    it.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use sqlx::SqlitePool;

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

/// Mint a non-admin session through the invitation → accept cycle.
async fn login_as_user(app: &Router, admin_cookie: &str, user_id: &str) -> String {
    let create = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, admin_cookie)
            .body(Body::from(format!(
                "user_id={user_id}&email={user_id}@example.com&aliases="
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(create.status(), StatusCode::OK);
    let html = body_string(create).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '\'' | ')' | ','))
        .unwrap();
    let invitation_id = &after[..end];
    let accept = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "password={user_id}-pw-secret-12&password_confirm={user_id}-pw-secret-12"
            )))
            .unwrap(),
    )
    .await;
    extract_cookie_value(&extract_set_cookie(&accept, "mwe_session").expect("cookie"))
}

/// Insert one row, with the shape its kind really carries.
async fn seed(
    pool: &SqlitePool,
    proposal_id: &str,
    kind: &str,
    context: &serde_json::Value,
    status: &str,
    recipient_id: Option<&str>,
    proposed_at: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO structure_proposals \
         (proposal_id, kind, context, questions, proposed_at, timeout_at, status, recipient_id) \
         VALUES (?, ?, ?, '[]', ?, ?, ?, ?)",
    )
    .bind(proposal_id)
    .bind(kind)
    .bind(context.to_string())
    .bind(proposed_at.to_rfc3339())
    .bind((proposed_at + chrono::Duration::hours(24)).to_rfc3339())
    .bind(status)
    .bind(recipient_id)
    .execute(pool)
    .await
    .unwrap();
}

/// The note the planner writes when it invents a page.
fn page_create_context(title: &str) -> serde_json::Value {
    serde_json::json!({
        "slug": "the-kitchen",
        "wiki_id": "alice",
        "page_path": "cucina.md",
        "title": title,
        "description": "What happens in the kitchen",
        "fact_count": 5,
        "minted_at": "2026-09-08T02:00:00+00:00",
    })
}

async fn get_page(app: &Router, uri: &str, cookie: &str) -> (StatusCode, String) {
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

/// A person is shown the proposals raised about their own facts, and is
/// shown neither somebody else's nor the ones the nightly run addresses
/// to nobody. The second half is the deliberate difference from the
/// topnav badge, which does hand everybody the unaddressed bucket: a
/// receipt names pages across every wiki, and a listing of them is not a
/// reader's to browse.
#[tokio::test]
async fn a_reader_sees_their_own_rows_and_neither_anothers_nor_the_unaddressed_ones() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;
    let now = chrono::Utc::now();

    seed(
        &pool,
        "p-bob",
        "page_create",
        &page_create_context("Bob's kitchen"),
        "applied",
        Some("user:bob"),
        now,
    )
    .await;
    seed(
        &pool,
        "p-frodo",
        "page_create",
        &page_create_context("Frodo's kitchen"),
        "applied",
        Some("user:frodo"),
        now,
    )
    .await;
    seed(
        &pool,
        "p-nobody",
        "page_create",
        &page_create_context("Nobody's kitchen"),
        "applied",
        None,
        now,
    )
    .await;

    let (status, html) = get_page(&app, "/proposals", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Bob's kitchen"), "{html}");
    assert!(
        !html.contains("Frodo"),
        "another person's row leaked: {html}"
    );
    assert!(
        !html.contains("Nobody"),
        "the unaddressed bucket is not a reader's: {html}"
    );

    // The admin, without reveal, gets their own plus the unaddressed one
    // — and still not the row addressed to somebody else.
    let (status, html) = get_page(&app, "/proposals", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Nobody's kitchen"), "{html}");
    assert!(!html.contains("Bob's"), "{html}");
    assert!(!html.contains("Frodo"), "{html}");

    // With reveal, every recipient's.
    let (status, html) =
        get_page(&app, "/proposals", &format!("{admin}; mwe_admin_reveal=1")).await;
    assert_eq!(status, StatusCode::OK);
    for title in ["Bob's kitchen", "Frodo's kitchen", "Nobody's kitchen"] {
        assert!(html.contains(title), "reveal must show {title}: {html}");
    }
}

/// A row the reader may not see answers the same as a row that is not
/// there. `403` would confirm that somebody else's proposal exists at
/// that id.
#[tokio::test]
async fn a_proposal_outside_the_scope_is_not_found_rather_than_forbidden() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;
    seed(
        &pool,
        "p-frodo",
        "page_create",
        &page_create_context("Frodo's kitchen"),
        "applied",
        Some("user:frodo"),
        chrono::Utc::now(),
    )
    .await;

    let (status, _) = get_page(&app, "/proposals/p-frodo", &bob).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_page(&app, "/proposals/no-such-row", &bob).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "and so does a row that is absent"
    );

    // The baseline: the same id opens for the person it is addressed to,
    // so the 404 above is the scope and not a broken route.
    let frodo = login_as_user(&app, &admin, "frodo").await;
    let (status, html) = get_page(&app, "/proposals/p-frodo", &frodo).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Frodo's kitchen"), "{html}");
}

/// The page says what happened in words built from the stored note. No
/// model is called to do it — which is the whole reason a read-only
/// instance can show these pages — so a row must never render as its raw
/// JSON.
#[tokio::test]
async fn a_row_reads_as_a_sentence_and_never_as_the_stored_json() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    seed(
        &pool,
        "p-1",
        "page_create",
        &page_create_context("The kitchen"),
        "applied",
        Some("user:alice"),
        chrono::Utc::now(),
    )
    .await;

    let (status, html) = get_page(&app, "/proposals", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("A page the memory made"), "{html}");
    assert!(
        html.contains("gathered 5 facts onto a page nobody named"),
        "the listing must say what happened: {html}"
    );
    assert!(
        !html.contains("&quot;fact_count&quot;"),
        "the stored keys must not reach the page raw: {html}"
    );

    // …and the row's own page names the fields in the reader's words.
    let (status, html) = get_page(&app, "/proposals/p-1", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Facts on it"), "{html}");
    assert!(html.contains("What it is about"), "{html}");
    assert!(!html.contains("fact_count"), "{html}");
}

/// Only something still waiting can be answered, and answering is the
/// chat's job. An applied receipt offers no door, because a change of
/// shape is not undone.
#[tokio::test]
async fn the_chat_is_offered_on_a_pending_row_and_not_on_an_applied_one() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let now = chrono::Utc::now();
    seed(
        &pool,
        "p-waiting",
        "slot_conflict",
        &serde_json::json!({
            "variant": "slot_conflict",
            "slot": "the date of birth",
            "subject_id": "user:alice",
            "kept_fact_id": "0197fa00-0000-7000-8000-000000000001",
            "kept_text": "born in 1982",
            "asserted_text": "born in 1987",
            "asserted_by": "user:bob",
            "refusal": "they neither said it nor are it",
        }),
        "pending",
        Some("user:alice"),
        now,
    )
    .await;
    seed(
        &pool,
        "p-done",
        "page_create",
        &page_create_context("The kitchen"),
        "applied",
        Some("user:alice"),
        now,
    )
    .await;

    let (_, html) = get_page(&app, "/proposals", &admin).await;
    assert!(
        html.contains("/dashboard/proposals/p-waiting/open-in-chat"),
        "a pending row must offer the chat: {html}"
    );
    assert!(
        !html.contains("/dashboard/proposals/p-done/open-in-chat"),
        "an applied change is not answered: {html}"
    );
}

/// The tabs narrow the listing to one state, and the address they carry
/// is the one the Home count links to.
#[tokio::test]
async fn the_status_tabs_narrow_the_listing() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let now = chrono::Utc::now();
    seed(
        &pool,
        "p-waiting",
        "page_create",
        &page_create_context("Waiting kitchen"),
        "pending",
        Some("user:alice"),
        now,
    )
    .await;
    seed(
        &pool,
        "p-done",
        "page_create",
        &page_create_context("Done kitchen"),
        "applied",
        Some("user:alice"),
        now,
    )
    .await;

    let (status, html) = get_page(&app, "/proposals?status=pending", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Waiting kitchen"), "{html}");
    assert!(!html.contains("Done kitchen"), "{html}");

    // No filter is every state, which is where the page opens.
    let (_, html) = get_page(&app, "/proposals", &admin).await;
    assert!(
        html.contains("Waiting kitchen") && html.contains("Done kitchen"),
        "{html}"
    );

    // A value nobody wrote lands on the page rather than on an error.
    let (status, _) = get_page(&app, "/proposals?status=nonsense", &admin).await;
    assert_eq!(status, StatusCode::OK);
}

/// A proposal is opened by name. The listing shows a page of the newest
/// rows, and a link to one older than that page has to keep working —
/// the table is never swept, so every deployment ends up with more rows
/// than the listing shows.
#[tokio::test]
async fn a_proposal_older_than_the_listing_window_still_opens() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let now = chrono::Utc::now();
    seed(
        &pool,
        "p-oldest",
        "page_create",
        &page_create_context("The oldest kitchen"),
        "applied",
        Some("user:alice"),
        now - chrono::Duration::days(400),
    )
    .await;
    for i in 0..60 {
        seed(
            &pool,
            &format!("p-newer-{i}"),
            "page_create",
            &page_create_context("A newer kitchen"),
            "applied",
            Some("user:alice"),
            now - chrono::Duration::minutes(i),
        )
        .await;
    }

    let (_, html) = get_page(&app, "/proposals", &admin).await;
    assert!(
        !html.contains("The oldest kitchen"),
        "the listing is a page of the newest rows: {html}"
    );

    let (status, html) = get_page(&app, "/proposals/p-oldest", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("The oldest kitchen"), "{html}");
}

/// An open instance is the baseline for the frozen one asserted in
/// `read_only.rs`: the pages render, and the door into the chat is
/// there.
#[tokio::test]
async fn an_open_instance_reads_its_proposals_and_offers_the_chat() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    seed(
        &pool,
        "p-waiting",
        "fact_forget",
        &serde_json::json!({
            "variant": "fact_forget",
            "fact_id": "0197fa00-0000-7000-8000-000000000001",
            "requester": "bob",
            "eligible_voters": ["alice", "carol"],
        }),
        "pending",
        Some("user:alice"),
        chrono::Utc::now(),
    )
    .await;

    let (status, html) = get_page(&app, "/proposals", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Asked by bob, who is not the one who said the fact"),
        "{html}"
    );
    assert!(
        html.contains("/dashboard/proposals/p-waiting/open-in-chat"),
        "{html}"
    );

    let (status, html) = get_page(&app, "/proposals/p-waiting", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Who may vote"), "{html}");
}

/// The people being asked to vote can read the ballot.
///
/// A forget request is addressed to whoever asked for the forget, and the
/// electorate is everybody else who can read the fact. Scoping on the
/// addressee alone hid the request from exactly the people it is a
/// question for: they were told to go and vote and shown nothing.
#[tokio::test]
async fn an_elector_reads_the_forget_request_the_requester_does_too_and_a_stranger_does_not() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;
    let carol = login_as_user(&app, &admin, "carol").await;
    let dave = login_as_user(&app, &admin, "dave").await;

    seed(
        &pool,
        "p-vote",
        "fact_forget",
        &serde_json::json!({
            "variant": "fact_forget",
            "fact_id": "0197fa00-0000-7000-8000-000000000001",
            "requester": "bob",
            "eligible_voters": ["carol"],
        }),
        "pending",
        Some("user:bob"),
        chrono::Utc::now(),
    )
    .await;

    // The requester: it is addressed to them.
    let (status, html) = get_page(&app, "/proposals", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("A request to forget"),
        "the requester: {html}"
    );

    // The elector: they are the one being asked.
    let (status, html) = get_page(&app, "/proposals", &carol).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("A request to forget"),
        "the elector must see it: {html}"
    );
    let (status, _) = get_page(&app, "/proposals/p-vote", &carol).await;
    assert_eq!(status, StatusCode::OK, "and must be able to open it");

    // Nobody else.
    let (_, html) = get_page(&app, "/proposals", &dave).await;
    assert!(
        !html.contains("A request to forget"),
        "a stranger must not: {html}"
    );
    let (status, _) = get_page(&app, "/proposals/p-vote", &dave).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The badge promises exactly what the page will show.
    let count = |cookie: String| {
        let app = app.clone();
        async move {
            let response = send(
                &app,
                Request::builder()
                    .uri("/proposals/in-flight-count")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            let body = body_string(response).await;
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["pending"]
                .as_i64()
                .unwrap()
        }
    };
    assert_eq!(count(carol).await, 1, "the elector is told there is one");
    assert_eq!(count(dave).await, 0, "and a stranger is not");
}
