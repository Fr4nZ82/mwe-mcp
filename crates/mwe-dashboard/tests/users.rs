// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration of users CRUD + invitation flow:
//! admin signs in → adds a regular user → grabs the invitation link
//! from the response → second user accepts → sets their password →
//! lands on /home signed in as themselves.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app, send};

/// Bootstrap the admin and return the session cookie value ready for
/// re-presenting as `Cookie:` on subsequent requests.
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

/// Drive `/users/new` POST and return the body so the caller can pull
/// the invitation link out of it.
async fn create_user(app: &axum::Router, cookie: &str, user_id: &str) -> String {
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
    body_string(response).await
}

#[tokio::test]
async fn admin_creates_user_then_invitation_link_works() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    // Admin creates a regular user — response page contains the
    // single-use invitation link.
    let html = create_user(&app, &admin_cookie, "galadriel").await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link in response");
    // The invitation_id is the UUIDv7 substring after the prefix and
    // before the next whitespace / quote / angle bracket / period at
    // sentence end.
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .expect("invitation_id terminator");
    let invitation_id = &after[..end];
    assert!(
        invitation_id.len() >= 32,
        "got invitation_id {invitation_id:?}"
    );

    // GET the accept page — should render the password form.
    let url = format!("/accept-invite/{invitation_id}");
    let response = send(
        &app,
        Request::builder().uri(&url).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Welcome, galadriel"), "{html}");

    // POST the password — invitation consumed, session cookie minted.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(&url)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=galadriel-pw-secret&password_confirm=galadriel-pw-secret",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/home")
    );
    let galadriel_cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    // Skip the first-run profile wizard so /home renders instead of
    // redirecting back to /welcome (the return path for unfinished
    // onboarding). Skip needs no LLM.
    let skip = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &galadriel_cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    assert!(skip.status().is_redirection(), "{}", skip.status());

    // Now /home renders as galadriel — and importantly, the admin nav
    // links are NOT visible because she is not admin.
    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, &galadriel_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("galadriel"), "{html}");
    // The `Users` admin link in the top nav only appears for admins.
    assert!(
        !html.contains(">Users<"),
        "non-admin must not see Users nav link"
    );
    assert!(
        !html.contains(">Tokens<"),
        "non-admin must not see Tokens nav link"
    );

    // The invitation cannot be reused.
    let response = send(
        &app,
        Request::builder().uri(&url).body(Body::empty()).unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(html.contains("invalid, expired"), "{html}");
}

#[tokio::test]
async fn user_create_rejects_invalid_id() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    let html = create_user(&app, &admin_cookie, "Galadriel").await; // uppercase
    assert!(html.contains("must match"), "{html}");
}

#[tokio::test]
async fn user_create_rejects_duplicate_id() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    let _ = create_user(&app, &admin_cookie, "gollum").await;
    let html = create_user(&app, &admin_cookie, "gollum").await;
    assert!(html.contains("already exists"), "{html}");
}

#[tokio::test]
async fn non_admin_cannot_reach_users_page() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    // Create + activate a regular user via the invitation cycle.
    let html = create_user(&app, &admin_cookie, "bilbo").await;
    let start = html.find("/dashboard/accept-invite/").expect("link");
    let after = &html[start + "/dashboard/accept-invite/".len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .unwrap();
    let invitation_id = &after[..end];
    let url = format!("/accept-invite/{invitation_id}");
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(&url)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=bilbo-pw-secret-12&password_confirm=bilbo-pw-secret-12",
            ))
            .unwrap(),
    )
    .await;
    let bilbo_cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    // /users for a non-admin → 403 forbidden page (Maud-rendered).
    let response = send(
        &app,
        Request::builder()
            .uri("/users")
            .header(header::COOKIE, &bilbo_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_cannot_delete_self() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/users/francesco/delete")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(html.contains("admin"), "{html}");
}

/// POST `/users/new` with a fully explicit body (so a test can omit or
/// collide the email). Returns the rendered response body.
async fn create_user_raw(app: &axum::Router, cookie: &str, body: &str) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_string(response).await
}

/// Create `user_id` (email `user_id@example.com`) and consume the
/// invitation with `password`, leaving an active account.
async fn onboard_user(app: &axum::Router, admin_cookie: &str, user_id: &str, password: &str) {
    let html = create_user(app, admin_cookie, user_id).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .unwrap();
    let invitation_id = &after[..end];
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "password={password}&password_confirm={password}"
            )))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
}

/// Drive a `/login` POST with `email` + `password`; return the response.
async fn login(app: &axum::Router, email: &str, password: &str) -> axum::response::Response {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("email={email}&password={password}")))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn user_create_requires_email() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    // No email field → rejected with the actionable error, and no
    // invitation is minted.
    let html = create_user_raw(&app, &admin_cookie, "user_id=frodo&aliases=").await;
    assert!(html.contains("Enter an email"), "{html}");
    assert!(
        !html.contains("/dashboard/accept-invite/"),
        "no invitation should be minted without an email: {html}"
    );
}

#[tokio::test]
async fn user_create_rejects_duplicate_email() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;

    let _ = create_user_raw(
        &app,
        &admin_cookie,
        "user_id=sam&email=shared@example.com&aliases=",
    )
    .await;
    let html = create_user_raw(
        &app,
        &admin_cookie,
        "user_id=rosie&email=shared@example.com&aliases=",
    )
    .await;
    assert!(html.contains("already used"), "{html}");
}

#[tokio::test]
async fn login_is_email_only() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;
    onboard_user(&app, &admin_cookie, "meriadoc", "meriadoc-pw-secret-12").await;

    // By email → success (any redirect + a fresh session cookie).
    let by_email = login(&app, "meriadoc@example.com", "meriadoc-pw-secret-12").await;
    assert!(
        by_email.status().is_redirection(),
        "email login should succeed: {}",
        by_email.status()
    );
    assert!(extract_set_cookie(&by_email, "mwe_session").is_some());

    // By username slug → rejected: there is no username fallback.
    let by_slug = login(&app, "meriadoc", "meriadoc-pw-secret-12").await;
    assert_eq!(
        by_slug.status(),
        StatusCode::OK,
        "username login must not mint a session"
    );
    let html = body_string(by_slug).await;
    assert!(html.contains("Invalid credentials"), "{html}");
}

#[tokio::test]
async fn admin_can_change_user_email_and_login_follows() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;
    onboard_user(&app, &admin_cookie, "peregrin", "peregrin-pw-secret-12").await;

    // Admin edits peregrin's email.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/users/peregrin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::from("email=pippin@example.com&aliases="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // The new email logs in; the old one no longer does.
    let by_new = login(&app, "pippin@example.com", "peregrin-pw-secret-12").await;
    assert!(
        by_new.status().is_redirection(),
        "new email should log in: {}",
        by_new.status()
    );
    let by_old = login(&app, "peregrin@example.com", "peregrin-pw-secret-12").await;
    assert_eq!(
        by_old.status(),
        StatusCode::OK,
        "old email must stop working"
    );
}
