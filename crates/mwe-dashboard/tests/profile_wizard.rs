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

/// The names the memory can reach this user by, straight out of the
/// enrolment row the roster is built from.
async fn aliases_of(pool: &sqlx::SqlitePool, user_id: &str) -> Vec<String> {
    let (json,): (Option<String>,) =
        sqlx::query_as("SELECT aliases FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("enrolment row");
    serde_json::from_str(json.as_deref().unwrap_or("[]")).expect("aliases json")
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

/// Step 1 asks the person their full name and their nickname, and those are
/// the names everybody else will use for them. They land in the enrolment
/// row's `aliases` — the only list a model resolving a name is shown — so a
/// sentence somebody else writes about "Ali" is filed as being about her.
///
/// They are written before the primer is captured, which is why this test can
/// read them back from a deployment with no model on the `ingest` slot, where
/// the capture itself refuses.
#[tokio::test]
async fn welcome_post_save_declares_the_typed_names_as_aliases() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let post = || {
        send(
            &app,
            Request::builder()
                .method("POST")
                .uri("/welcome")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "action=save&display_name=Alice+Liddell&nickname=Ali",
                ))
                .unwrap(),
        )
    };
    let response = post().await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        aliases_of(&pool, "alice").await,
        vec!["Alice Liddell", "Ali"],
        "both names, as typed"
    );

    // The wizard is still pending after the refusal, so the person fills it
    // again: the same two names must not pile up, and the id is not a name to
    // declare in the first place.
    let response = post().await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let aliases = aliases_of(&pool, "alice").await;
    assert_eq!(aliases, vec!["Alice Liddell", "Ali"], "{aliases:?}");
    assert!(
        !aliases.iter().any(|a| a.eq_ignore_ascii_case("alice")),
        "the user id is already a name of theirs: {aliases:?}"
    );
}

/// **Skip all** is the answer "do not use what I typed": the names go
/// nowhere, and the person stays reachable by their user id alone until
/// somebody declares otherwise on the Users page.
#[tokio::test]
async fn welcome_post_skip_declares_no_aliases() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "action=skip&display_name=Alice+Liddell&nickname=Ali",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    assert!(
        aliases_of(&pool, "alice").await.is_empty(),
        "a skipped wizard declares nothing"
    );
}

/// Enrol a second person straight into the roster, with the aliases given.
/// The primer's names are checked against this roster, so a test needs
/// somebody else on the deployment to collide with.
async fn enrol(pool: &sqlx::SqlitePool, user_id: &str, aliases: &[&str]) {
    sqlx::query("INSERT INTO enrollment_users (user_id, email, aliases) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(format!("{user_id}@example.com"))
        .bind(serde_json::to_string(aliases).expect("aliases json"))
        .execute(pool)
        .await
        .expect("enrol");
}

/// A name reaches one person. A nickname that is already somebody else's user
/// id is refused before anything at all is written: the wizard comes back with
/// what was typed still in it, saying whose name it is.
#[tokio::test]
async fn welcome_post_refuses_a_name_another_person_answers_to() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;
    enrol(&pool, "bob", &["Bobby"]).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "action=save&display_name=Alice+Liddell&nickname=bob&favorite_color=blu",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the wizard comes back, it does not fail the request"
    );
    let html = body_string(response).await;
    assert!(
        html.contains("already how this memory reaches bob"),
        "the refusal names whose name it is: {html}"
    );
    assert!(
        html.contains("their user id"),
        "and says it is their id rather than one of their aliases: {html}"
    );
    assert!(
        html.contains("value=\"Alice Liddell\""),
        "the form comes back filled with what was typed: {html}"
    );

    assert!(
        aliases_of(&pool, "alice").await.is_empty(),
        "a refused submission declares nothing — not even the name that was fine"
    );
    let locale: Option<String> =
        sqlx::query_scalar("SELECT locale FROM enrollment_users WHERE user_id = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("row");
    assert!(
        locale.is_none(),
        "and writes nothing else either; got {locale:?}"
    );
}

/// The same check against an alias somebody else declared, rather than their
/// id — and the person's OWN id is not a collision: it is simply a name they
/// already have, so it is skipped in silence and the rest goes through.
#[tokio::test]
async fn welcome_post_refuses_another_persons_alias_but_not_the_persons_own_id() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = setup_admin(&app).await;
    enrol(&pool, "bob", &["Bobby"]).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=save&display_name=Bobby&nickname=Ali"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("already how this memory reaches bob")
            && html.contains("a name declared for them"),
        "an alias of somebody else's is refused, and named as an alias: {html}"
    );
    assert!(aliases_of(&pool, "alice").await.is_empty());

    // Their own id is not somebody else's name. Nothing is refused; the id is
    // simply not added twice, and the other name is.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=save&display_name=Alice&nickname=Ali"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "past the name check, the wizard fails on the unwired ingest slot"
    );
    assert_eq!(
        aliases_of(&pool, "alice").await,
        vec!["Ali"],
        "the id is skipped in silence, never refused"
    );
}
