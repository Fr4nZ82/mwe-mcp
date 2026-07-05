// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration of /dashboard/tokens: issue (smart and
//! standard), revoke, delegation edit. Verifies that the JWT `is_admin`
//! claim is derived (from the owner for a smart token, always false for a
//! standard one), that a standard token mints its bot system user on the
//! fly, and that standard issuance upserts `consumer_delegations`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app, send};

async fn login_as_admin(app: &axum::Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=francesco@example.com&admin_id=francesco&password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

async fn create_user(app: &axum::Router, cookie: &str, user_id: &str) {
    let body = format!("user_id={user_id}&email={user_id}@example.com&aliases=");
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn issue(app: &axum::Router, cookie: &str, body: &'static str) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/tokens/issue")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_string(response).await
}

#[tokio::test]
async fn issue_smart_token_for_admin_renders_token() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // The admin is a human with a login, so the diagonal-correct mono-user
    // token for them is a *smart* (Pattern A) token owned by francesco.
    let html = issue(
        &app,
        &cookie,
        "consumer_class=smart&owner_id=francesco&consumer_id=cc-laptop\
         &device_label=claude-code&rate_limit_id=default&ttl_profile=internal",
    )
    .await;
    assert!(html.contains("Token issued"), "{html}");
    // Token starts with the canonical JWT header for HS256.
    assert!(html.contains("eyJ"), "expected a JWT in the response");
    // Derived is_admin: francesco is the admin → claim must be true.
    assert!(html.contains("isAdmin"), "{html}");
    assert!(
        html.contains("smart"),
        "smart class in the claims card: {html}"
    );
}

#[tokio::test]
async fn issue_standard_token_mints_bot_and_creates_delegation_row() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "frodo").await;
    create_user(&app, &cookie, "galadriel").await;

    // No pre-created bot account: the standard path mints `samviseprod`
    // as a credential-less system user, then binds the token to it.
    let html = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=samviseprod&device_label=sam-orchestrator\
         &rate_limit_id=default&ttl_profile=exposed\
         &allowed_sender_ids=frodo&allowed_sender_ids=galadriel",
    )
    .await;
    assert!(html.contains("Token issued"), "{html}");
    assert!(
        html.contains("standard"),
        "standard class in claims: {html}"
    );
    // consumer_id (= the bot id = the sender) surfaces in the claims card
    // and the delegation table on the same page.
    assert!(
        html.contains("samviseprod"),
        "consumer_id in claims: {html}"
    );
    assert!(html.contains("frodo, galadriel") || html.contains("galadriel, frodo"));
}

#[tokio::test]
async fn smart_issue_rejects_unknown_owner() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let html = issue(
        &app,
        &cookie,
        "consumer_class=smart&owner_id=ghost&consumer_id=cc-laptop\
         &device_label=x&rate_limit_id=default&ttl_profile=internal",
    )
    .await;
    assert!(html.contains("Unknown owner"), "{html}");
}

#[tokio::test]
async fn standard_issue_rejects_empty_delegation() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let html = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=lonelyprod&device_label=x\
         &rate_limit_id=default&ttl_profile=internal",
    )
    .await;
    assert!(html.contains("at least one allowed sender"), "{html}");
    // The bot must NOT have been created when validation fails first.
    let users = send(
        &app,
        Request::builder()
            .uri("/users")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let users_html = body_string(users).await;
    assert!(
        !users_html.contains("lonelyprod"),
        "orphan system user leaked: {users_html}"
    );
}

#[tokio::test]
async fn standard_bot_id_cannot_hijack_a_human_account() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "frodo").await;

    // `francesco` is the admin — a human *with* a login. Binding a
    // standard (multi-user) token to it would leak across users.
    let html = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=francesco&device_label=x\
         &rate_limit_id=default&ttl_profile=internal&allowed_sender_ids=frodo",
    )
    .await;
    assert!(html.contains("human account"), "{html}");
}

#[tokio::test]
async fn standard_bot_id_rejects_unwiki_safe_id() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "frodo").await;

    // Underscore is rejected by the enrollment id grammar itself (every
    // enrollable id is a valid wiki id — identity-and-acl.md §1.6), so the
    // bot path bounces it at the first guard, before the wiki-id parse
    // fallback ever runs.
    let html = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=sam_prod&device_label=x\
         &rate_limit_id=default&ttl_profile=internal&allowed_sender_ids=frodo",
    )
    .await;
    assert!(html.contains("lowercase letters and digits"), "{html}");
}

#[tokio::test]
async fn smart_issue_rejects_missing_consumer_id() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "alice").await;

    let html = issue(
        &app,
        &cookie,
        "consumer_class=smart&owner_id=alice&device_label=Claude+Code+laptop\
         &rate_limit_id=default&ttl_profile=internal",
    )
    .await;
    assert!(html.contains("requires a device id"), "{html}");
}

#[tokio::test]
async fn revoke_inserts_into_blacklist_and_refreshes_cache() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Make up a jti — the dashboard accepts any string, it is just the
    // key of `token_blacklist`. Real callers pass the jti they read
    // from the issued JWT.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/tokens/revoke")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "jti=01900000-0000-7000-8000-000000000000&reason=manual",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Revoked"), "{html}");
    assert!(html.contains("01900000-0000-7000-8000-000000000000"));
}

#[tokio::test]
async fn revoke_rejects_empty_reason() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/tokens/revoke")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("jti=anything&reason="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn delegation_edit_updates_allowed_senders() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "frodo").await;
    create_user(&app, &cookie, "bilbo").await;

    // Seed: issue a standard token so the delegation row exists.
    let _ = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=samviseprod&device_label=x\
         &rate_limit_id=default&ttl_profile=internal&allowed_sender_ids=frodo",
    )
    .await;

    // Now edit: replace allowed list with {bilbo}.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/tokens/delegation/samviseprod")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("allowed_sender_ids=bilbo"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());

    // Landing page now shows bilbo, not frodo.
    let response = send(
        &app,
        Request::builder()
            .uri("/tokens")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    // Find the delegations table row for `samviseprod` and check the
    // cell that lists allowed senders. The form below also lists every
    // user as a checkbox label, so we cannot just grep for "frodo".
    let row_marker = "<code>samviseprod</code>";
    let row_start = html.find(row_marker).expect("delegation row");
    let after_consumer_cell = &html[row_start..];
    let allowed_cell_start = after_consumer_cell
        .find("</td><td>")
        .expect("allowed senders cell");
    let allowed_cell_inner_start = allowed_cell_start + "</td><td>".len();
    let allowed_cell_end = after_consumer_cell[allowed_cell_inner_start..]
        .find("</td>")
        .expect("end of cell");
    let allowed_cell =
        &after_consumer_cell[allowed_cell_inner_start..allowed_cell_inner_start + allowed_cell_end];
    assert!(allowed_cell.contains("bilbo"), "cell={allowed_cell:?}");
    assert!(
        !allowed_cell.contains("frodo"),
        "frodo should have been removed from delegation list, cell={allowed_cell:?}"
    );
}

/// The builtin `guest` pseudo-identity is delegable without being an
/// enrolled user — granting it from the delegation editor is the guest
/// feature's enable switch (roadmap 40).
#[tokio::test]
async fn delegation_edit_accepts_builtin_guest() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "frodo").await;

    let _ = issue(
        &app,
        &cookie,
        "consumer_class=standard&consumer_id=samviseprod&device_label=x\
         &rate_limit_id=default&ttl_profile=internal&allowed_sender_ids=frodo",
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/tokens/delegation/samviseprod")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "allowed_sender_ids=frodo&allowed_sender_ids=guest",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "guest must be delegable: {}",
        response.status()
    );

    let response = send(
        &app,
        Request::builder()
            .uri("/tokens")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let allowed = body_string(response).await;
    let row_start = allowed.find("<code>samviseprod</code>").expect("row");
    assert!(
        allowed[row_start..row_start + 400].contains("guest"),
        "guest should appear in the allowed-senders cell"
    );
}
