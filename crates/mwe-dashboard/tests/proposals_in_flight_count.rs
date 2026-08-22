// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration coverage for `GET /dashboard/proposals/in-flight-count` —
//! the JSON count the topnav badge fetches client-side.
//!
//! Verifies the JSON shape, that only `pending` rows are counted (an
//! applied change is not undone, so nothing else is still actionable),
//! and that the count is ACL-scoped to the signed-in user: everyone —
//! admins included — sees only rows addressed to them plus the
//! unaddressed/admin-fallback ones, and the admin ACL-reveal cookie
//! lifts an admin to the deployment-wide count.

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

/// Mint a non-admin session via the invitation → accept cycle, returning
/// its `mwe_session` cookie.
async fn login_as_user(app: &Router, admin_cookie: &str, user_id: &str) -> String {
    let create_resp = send(
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
    assert_eq!(create_resp.status(), StatusCode::OK);
    let html = body_string(create_resp).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '\'' | ')' | ','))
        .unwrap();
    let invitation_id = &after[..end];
    let accept_resp = send(
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
    extract_cookie_value(&extract_set_cookie(&accept_resp, "mwe_session").expect("cookie"))
}

/// Insert one `structure_proposals` row.
async fn seed_proposal(
    pool: &SqlitePool,
    proposal_id: &str,
    status: &str,
    recipient_id: Option<&str>,
) {
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO structure_proposals \
         (proposal_id, kind, context, questions, proposed_at, timeout_at, status, \
          recipient_id) \
         VALUES (?, 'wiki_promote', '{\"intent\":\"t\"}', '[]', ?, ?, ?, ?)",
    )
    .bind(proposal_id)
    .bind(now.to_rfc3339())
    .bind((now + chrono::Duration::seconds(86_400)).to_rfc3339())
    .bind(status)
    .bind(recipient_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Parse the JSON body of an in-flight-count response.
async fn fetch_count(app: &Router, cookie: &str) -> serde_json::Value {
    let response = send(
        app,
        Request::builder()
            .uri("/proposals/in-flight-count")
            .header(header::COOKIE, cookie)
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    serde_json::from_str(&body).expect("in-flight-count body is JSON")
}

#[tokio::test]
async fn in_flight_count_requires_auth() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let response = send(
        &app,
        Request::builder()
            .uri("/proposals/in-flight-count")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    // The session middleware redirects an unauthenticated GET to login.
    assert!(
        response.status().is_redirection() || response.status() == StatusCode::UNAUTHORIZED,
        "unauthenticated must not get a 200: {}",
        response.status()
    );
}

/// An admin is scoped to their **own** in-flight items by default — a
/// proposal addressed to another user does not show in the badge count
/// (it carries that user's fact text, per-fragment ACL'd). The admin
/// ACL-reveal cookie lifts the scope to the whole deployment, the same
/// posture the facts table takes.
#[tokio::test]
async fn admin_count_is_scoped_to_self_without_reveal_full_with_reveal() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;

    // pending (addressed to a stranger — hidden from the admin unless reveal)
    seed_proposal(&pool, "p-pending", "pending", Some("user:frodo")).await;
    // pending (unaddressed → admin-fallback, always counts)
    seed_proposal(&pool, "p-unaddressed", "pending", None).await;
    // applied → NOT counted: an applied change is not actionable.
    seed_proposal(&pool, "p-applied", "applied", None).await;
    // expired → NOT counted.
    seed_proposal(&pool, "p-expired", "expired", None).await;

    // Without reveal: only the unaddressed admin-fallback row counts.
    let scoped = fetch_count(&app, &admin).await;
    assert_eq!(scoped["pending"], 1, "{scoped}");

    // With the reveal cookie: the deployment-wide count.
    let revealed = fetch_count(&app, &format!("{admin}; mwe_admin_reveal=1")).await;
    assert_eq!(revealed["pending"], 2, "{revealed}");
}

#[tokio::test]
async fn non_admin_count_is_scoped_to_recipient() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bilbo = login_as_user(&app, &admin, "bilbo").await;

    // Addressed to bilbo → counts for bilbo.
    seed_proposal(&pool, "p-mine", "pending", Some("user:bilbo")).await;
    // Unaddressed (admin-fallback) → also counts for bilbo (pre-0032 rule).
    seed_proposal(&pool, "p-unaddressed", "pending", None).await;
    // Addressed to someone else → must NOT count for bilbo.
    seed_proposal(&pool, "p-frodo", "pending", Some("user:frodo")).await;

    // Bilbo: his own + the unaddressed one = 2.
    let bilbo_json = fetch_count(&app, &bilbo).await;
    assert_eq!(bilbo_json["pending"], 2, "{bilbo_json}");

    // Admin without reveal: scoped exactly like a normal user — only the
    // unaddressed admin-fallback row (frodo's and bilbo's stay hidden).
    let admin_json = fetch_count(&app, &admin).await;
    assert_eq!(admin_json["pending"], 1, "{admin_json}");

    // Admin WITH reveal: the whole deployment.
    let revealed = fetch_count(&app, &format!("{admin}; mwe_admin_reveal=1")).await;
    assert_eq!(revealed["pending"], 3, "{revealed}");
}
