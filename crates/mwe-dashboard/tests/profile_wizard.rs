// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration coverage for the first-login profile wizard
//! (`/dashboard/welcome`).
//!
//! The wizard runs through the ingest model slot — with no model on it
//! `Save` fails 422 and only `Skip` works. These tests run against a
//! `MemoryHandles` bundle with `LlmConfig::default()` (no slots wired),
//! which is a half-wired install, not a supported way to run: they
//! exercise the refusal end-to-end. The happy-path
//! Save-with-LLM is a manual test the operator runs once Ollama is up
//! against the workhorse — there is no facility to inject a fake
//! backend through `LlmFunctionConfig::build_backend` today.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

/// Drive the setup wizard to create the canonical admin and return the
/// session cookie (without skipping the welcome wizard — these tests
/// are the ones that exercise it directly).
async fn setup_admin(app: &axum::Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice\
                 &password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

/// True when any page of `wiki_id` carries a fact marker — the shape a
/// capture leaves behind. Scanned across the whole wiki dir: there is no one
/// page a capture is guaranteed to land on.
fn wiki_carries_a_marker(tree: &mwe_core::wiki::WikiTree, wiki_id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(tree.wikis_dir().join(wiki_id)) else {
        return false;
    };
    entries.flatten().any(|e| {
        std::fs::read_to_string(e.path())
            .is_ok_and(|raw| raw.contains("{{subject=") || raw.contains("{{owner="))
    })
}

#[tokio::test]
async fn welcome_get_renders_form_with_email_pre_filled() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());
    let html = body_string(response).await;
    assert!(html.contains("Welcome"), "{html}");
    assert!(
        html.contains("alice@example.com"),
        "email must be pre-filled: {html}"
    );
    assert!(html.contains("name=\"presentati\""), "{html}");
    assert!(html.contains("name=\"birthday\""), "{html}");
    assert!(html.contains("name=\"favorite_color\""), "{html}");
    assert!(html.contains("value=\"save\""), "{html}");
    assert!(html.contains("value=\"skip\""), "{html}");
}

#[tokio::test]
async fn welcome_get_shows_no_llm_banner_when_ingest_slot_missing() {
    // The default `make_app_with_memory` wires `LlmConfig::default()`
    // which has no slots — so this test always hits the no-LLM banner
    // branch.
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_success());
    let html = body_string(response).await;
    // Named in words an operator can act on, not as a YAML path: the
    // banner has to say which slot and where to fill it.
    assert!(
        html.contains("ingest model slot has no model"),
        "the banner must name the slot: {html}"
    );
    assert!(
        html.contains("LLM config page"),
        "the banner must say where to fill it: {html}"
    );
}

#[tokio::test]
async fn welcome_post_save_fails_422_without_llm_slot() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "action=save&display_name=Alice+Liddell&favorite_color=blu",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "save without llm.ingest must hard-refuse"
    );
    let html = body_string(response).await;
    assert!(
        html.contains("ingest model slot has no model"),
        "the refusal must name the slot: {html}"
    );

    // No marker anywhere in the wiki — capture was not attempted.
    assert!(
        !wiki_carries_a_marker(&tree, "alice"),
        "failed save must not leave a partial capture"
    );

    // Flag still 0 — wizard remains pending so the operator can
    // configure the slot and try again.
    let (flag,): (i64,) =
        sqlx::query_as("SELECT profile_initialized FROM user_credentials WHERE user_id = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(flag, 0, "failed save must not flip the flag");
}

#[tokio::test]
async fn welcome_post_skip_flips_flag_without_capturing_and_without_llm() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());

    // No marker anywhere in the wiki — nothing was captured.
    assert!(
        !wiki_carries_a_marker(&tree, "alice"),
        "skip must not capture"
    );

    // Flag flipped.
    let (flag,): (i64,) =
        sqlx::query_as("SELECT profile_initialized FROM user_credentials WHERE user_id = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(flag, 1);
}

#[tokio::test]
async fn second_login_after_wizard_skip_lands_on_home() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;
    // Complete (skip) the wizard.
    let _ = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    // Log in again and check the redirect.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&password=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/home")
    );
}
