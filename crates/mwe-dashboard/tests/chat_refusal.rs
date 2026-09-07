// SPDX-License-Identifier: AGPL-3.0-or-later
//! The chat panel is told why a turn did not go through.
//!
//! `POST /dashboard/chat/agentic` is fetched by `chat.js`, which prints
//! the body's `error` in the conversation and offers `fix` as a link.
//! An HTML error page would leave it with nothing to say but a status
//! code, so a refusal has to come back as that JSON envelope.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

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

async fn post_agentic(app: &axum::Router, cookie: &str, text: &str) -> (StatusCode, String) {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, cookie)
            .body(Body::from(format!("text={text}")))
            .unwrap(),
    )
    .await;
    let status = response.status();
    (status, body_string(response).await)
}

#[tokio::test]
async fn a_refused_turn_comes_back_as_a_sentence_the_panel_can_print() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let (status, body) = post_agentic(&app, &cookie, "hello").await;

    assert!(!status.is_success(), "the turn cannot have run: {status}");
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("a refusal must be JSON, not an HTML page ({e}): {body}"));
    let message = json["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("operator-chat model slot"),
        "the panel must be handed the reason, got {message:?}"
    );
    assert!(
        !message.contains("Something went wrong"),
        "a missing slot is not an unexplained fault: {message:?}"
    );
}

#[tokio::test]
async fn an_empty_message_is_refused_in_the_same_envelope() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let (status, body) = post_agentic(&app, &cookie, "   ").await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let json: serde_json::Value = serde_json::from_str(&body).expect("JSON envelope");
    assert_eq!(json["error"], "Type a message before sending.");
}
