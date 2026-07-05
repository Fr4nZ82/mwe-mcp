// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration of the bootstrap flow:
//! virgin server → `/dashboard/` redirects to `/setup` → submit
//! creates admin + cookie → `/home` renders → `/logout` revokes +
//! clears → `/dashboard/` redirects to `/login`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app, send};

#[tokio::test]
async fn virgin_server_redirects_to_setup() {
    let (app, _dir) = make_app().await;

    let response = send(
        &app,
        Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert!(response.status().is_redirection(), "{}", response.status());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/setup")
    );
}

#[tokio::test]
async fn setup_get_renders_form_until_admin_exists() {
    let (app, _dir) = make_app().await;

    let response = send(
        &app,
        Request::builder()
            .uri("/setup")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Set up admin"), "{html}");
    assert!(html.contains("admin_id"));
    assert!(html.contains("email"), "{html}");
}

#[tokio::test]
async fn setup_submit_creates_admin_and_sets_session_cookie() {
    let (app, _dir) = make_app().await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(
                header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(Body::from(
                "email=francesco@example.com&admin_id=francesco&password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;

    assert!(response.status().is_redirection(), "{}", response.status());
    // Post-setup redirect lands on the LLM config page — step 1 of
    // onboarding, because the profile primer that follows needs a usable
    // ingest model. The primer itself stays gated on profile_initialized.
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/admin/llm-config")
    );
    let cookie = extract_set_cookie(&response, "mwe_session").expect("session cookie");
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("Path=/dashboard"));
    assert!(cookie.contains("SameSite=Lax"));
}

#[tokio::test]
async fn setup_after_admin_exists_redirects_to_login() {
    let (app, _dir) = make_app().await;
    create_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/setup")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/login")
    );
}

#[tokio::test]
async fn setup_rejects_mismatched_passwords() {
    let (app, _dir) = make_app().await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice&password=correcthorsebattery&password_confirm=different-pw-12",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("do not match"), "{html}");
}

#[tokio::test]
async fn setup_rejects_short_password() {
    let (app, _dir) = make_app().await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&admin_id=alice&password=short&password_confirm=short",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("at least"), "{html}");
}

#[tokio::test]
async fn login_then_home_then_logout_full_cycle() {
    let (app, _dir) = make_app().await;
    create_admin(&app).await;

    // GET /login renders the form.
    let response = send(
        &app,
        Request::builder()
            .uri("/login")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("Sign in"));

    // POST /login with the right password mints a cookie.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=francesco@example.com&password=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    let session_cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    // GET /home with the cookie returns the dashboard.
    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, &session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Signed in as"), "{html}");
    assert!(html.contains("francesco"), "{html}");
    assert!(html.contains("admin"), "{html}");

    // POST /logout revokes + clears.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/logout")
            .header(header::COOKIE, &session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/login")
    );
    let cleared = extract_set_cookie(&response, "mwe_session").expect("clear cookie");
    assert!(
        cleared.contains("Max-Age=0") || cleared.contains("mwe_session=;"),
        "{cleared}"
    );

    // After logout, the old cookie must not let us through to /home.
    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, &session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/login")
    );
}

#[tokio::test]
async fn login_with_wrong_password_shows_generic_flash() {
    let (app, _dir) = make_app().await;
    create_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=francesco@example.com&password=wrong-password-1",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("Invalid credentials"));
}

#[tokio::test]
async fn unauthenticated_home_redirects_to_login() {
    let (app, _dir) = make_app().await;
    create_admin(&app).await;

    let response = send(
        &app,
        Request::builder().uri("/home").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/login")
    );
}

/// Helper: drive the setup wizard to create the canonical admin and
/// skip the first-login profile wizard so subsequent login flows
/// redirect straight to `/dashboard/home` as the legacy tests assume.
async fn create_admin(app: &axum::Router) {
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
    assert!(
        response.status().is_redirection(),
        "create_admin failed: {}",
        response.status()
    );
    let cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    // Skip the profile wizard so the rest of the test flow expects
    // /dashboard/home redirects on login.
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    let status = response.status();
    let body = body_string(response).await;
    assert!(
        status.is_redirection(),
        "welcome skip failed: {status}: {body}"
    );
}
