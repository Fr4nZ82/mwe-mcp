// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the landing page shows, and to whom.
//!
//! Two questions live here: the sentence a fresh install opens with (the
//! model slots that have nothing behind them), and the split between what
//! a person sees on arrival and what belongs to the operator.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use sqlx::SqlitePool;

/// Set the admin up and mark their profile finished, so `/home` renders
/// instead of sending them back to the first-run wizard.
async fn login_as_admin(app: &axum::Router, pool: &SqlitePool) -> String {
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
    mark_profile_finished(pool, "alice").await;
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

/// The first-run wizard sends anyone who has not been through it back to
/// itself; these tests are about the page after that.
async fn mark_profile_finished(pool: &SqlitePool, user_id: &str) {
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("mark the profile finished");
}

/// Create `user_id` as a plain user, walk their invitation to a password,
/// and mark their profile finished. Returns their session cookie.
async fn make_member(
    app: &axum::Router,
    pool: &SqlitePool,
    admin_cookie: &str,
    user_id: &str,
) -> String {
    let response = send(
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
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .expect("terminator");
    let invitation_id = &after[..end];

    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    mark_profile_finished(pool, user_id).await;
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("member cookie"))
}

async fn home_page(app: &axum::Router, cookie: &str) -> String {
    let response = send(
        app,
        Request::builder()
            .uri("/home")
            .header(header::HOST, "memory.example.org")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_string(response).await
}

/// A fresh install has nothing behind any of the six slots, and the banner
/// is the first thing the admin reads. It counts them in words: a digit
/// beside the spelled total ("6 of the six") reads as a bug on the one
/// screen that has to be believed.
#[tokio::test]
async fn the_banner_counts_the_empty_slots_in_words() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app, &pool).await;
    let html = home_page(&app, &cookie).await;
    assert!(
        html.contains("All six model slots have no model"),
        "the banner must say all six are empty: {html}"
    );
    assert!(
        !html.contains("6 of the six"),
        "a digit against the spelled total is the defect being fixed: {html}"
    );
    // …and it still names them, which is what the admin acts on.
    assert!(html.contains("operator_chat"), "{html}");
}

/// The deployment-wide counts and the server address a consumer connects
/// to are the operator's view: they are taken across everybody's memory,
/// past the per-fragment ACL. A person opens their home page on their own
/// memory instead — which is what they came for, and all of it theirs.
#[tokio::test]
async fn a_person_opens_on_their_own_memory_not_the_whole_deployment() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app, &pool).await;
    let bob = make_member(&app, &pool, &admin, "bob").await;

    let html = home_page(&app, &bob).await;
    for operators_own in [
        "wikis with facts",
        "active facts",
        "pending proposals",
        "Connect a consumer",
        "memory.example.org/mcp",
    ] {
        assert!(
            !html.contains(operators_own),
            "a reader must not be shown `{operators_own}`: {html}"
        );
    }
    // What they do get, first thing: their own memory.
    assert!(html.contains("Your memory"), "{html}");
    assert!(html.contains("/dashboard/wiki/bob"), "{html}");
    assert!(html.contains("/dashboard/facts?subject=user:bob"), "{html}");
}

/// The same page for the admin: the operator's view is where it was.
#[tokio::test]
async fn the_admin_still_gets_the_deployment_counts_and_the_address() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app, &pool).await;

    let html = home_page(&app, &cookie).await;
    for operators_own in [
        "wikis with facts",
        "active facts",
        "pending proposals",
        "Connect a consumer",
        "https://memory.example.org/mcp",
    ] {
        assert!(
            html.contains(operators_own),
            "the admin's home page lost `{operators_own}`: {html}"
        );
    }
    // And their own memory is still on it — the split is about what is
    // added for an operator, not about taking their own memory away.
    assert!(html.contains("Your memory"), "{html}");
}
