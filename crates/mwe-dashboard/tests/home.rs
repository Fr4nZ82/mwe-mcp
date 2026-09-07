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
