// SPDX-License-Identifier: AGPL-3.0-or-later
//! The links this server sends by email are built on the address the
//! operator declared, and on nothing else — least of all the `Host`
//! header of the request asking for them.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, make_app_with_memory, send};
use sqlx::SqlitePool;

/// An SMTP backend good enough for `is_sendable`, plus the public
/// address when one is given. The relay does not have to exist: the send
/// is fire-and-forget, and what this file is about is whether a link is
/// minted at all.
fn write_config(workdir: &std::path::Path, public_base_url: Option<&str>) {
    let smtp = "email:\n  enabled: true\n  smtp_host: smtp.example\n  \
                from_address: memory@example\n";
    let yaml = public_base_url.map_or_else(
        || smtp.to_owned(),
        |url| format!("{smtp}public_base_url: '{url}'\n"),
    );
    std::fs::write(workdir.join("mwe-mcp.config.yaml"), yaml).expect("write config");
}

async fn seed_user(pool: &SqlitePool) {
    sqlx::query("INSERT INTO enrollment_users (user_id, email, is_admin) VALUES ('frodo', 'frodo@example.com', 0)")
        .execute(pool)
        .await
        .expect("user");
    sqlx::query(
        "INSERT INTO user_credentials (user_id, password_hash, hashed_at)
         VALUES ('frodo', 'x', '2026-01-01T00:00:00+00:00')",
    )
    .execute(pool)
    .await
    .expect("credentials");
}

/// A reset link is minted only when a row lands in `password_resets`, so
/// counting them says whether one was built.
async fn reset_rows(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM password_resets")
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn ask_for_a_reset(app: &axum::Router) -> StatusCode {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/forgot-password")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            // The header no link may be built from: it is attacker-chosen
            // on any deployment reachable from outside.
            .header(header::HOST, "attacker.example")
            .header("x-forwarded-proto", "https")
            .body(Body::from("email=frodo@example.com"))
            .unwrap(),
    )
    .await;
    response.status()
}

/// With no declared address there is no link to send, and the `Host`
/// header does not become one: nothing is minted at all.
#[tokio::test]
async fn without_a_public_address_no_recovery_link_is_built_from_the_host_header() {
    let (app, pool, _tree, dir) = make_app_with_memory().await;
    write_config(dir.path(), None);
    seed_user(&pool).await;

    let status = ask_for_a_reset(&app).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the answer never says what happened"
    );
    assert_eq!(
        reset_rows(&pool).await,
        0,
        "no reset link may be minted when the only address available is the \
         one the requester put in the `Host` header"
    );
}

/// With one declared, the link is minted — the refusal above is about the
/// missing address, not about recovery being off.
#[tokio::test]
async fn with_a_public_address_the_recovery_link_is_minted() {
    let (app, pool, _tree, dir) = make_app_with_memory().await;
    write_config(dir.path(), Some("https://memory.example"));
    seed_user(&pool).await;

    let status = ask_for_a_reset(&app).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reset_rows(&pool).await,
        1,
        "the declared address is what the link is built on"
    );
}

/// And the form says so up front, rather than promising a message that
/// cannot be sent.
#[tokio::test]
async fn the_recovery_form_says_recovery_is_unavailable_without_an_address() {
    let (app, _pool, _tree, dir) = make_app_with_memory().await;
    write_config(dir.path(), None);

    let response = send(
        &app,
        Request::builder()
            .uri("/forgot-password")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(
        html.contains("not configured on this server"),
        "the form must not offer to send a link it cannot build: {html}"
    );

    write_config(dir.path(), Some("https://memory.example"));
    let response = send(
        &app,
        Request::builder()
            .uri("/forgot-password")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(
        html.contains("Send reset link"),
        "with an address the form offers recovery: {html}"
    );
}
