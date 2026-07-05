// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration of /dashboard/settings/me self-service
//! password change, plus the embedded admin Email (SMTP) editor.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    body_string, extract_cookie_value, extract_set_cookie, make_app, make_app_with_memory, send,
};

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
    assert!(response.status().is_redirection());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

#[tokio::test]
async fn change_password_then_old_password_no_longer_logs_in() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Successful change.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/me")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "current_password=correct-horse-battery\
                 &new_password=brand-new-pw-2026!\
                 &new_password_confirm=brand-new-pw-2026!",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Password updated"), "{html}");

    // Old password is rejected at /login.
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
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("Invalid credentials"));

    // New password is accepted.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=francesco@example.com&password=brand-new-pw-2026!",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());
}

#[tokio::test]
async fn change_password_rejects_wrong_current() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/me")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "current_password=wrong-current\
                 &new_password=brand-new-pw-2026!\
                 &new_password_confirm=brand-new-pw-2026!",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        body_string(response)
            .await
            .contains("Current password is incorrect")
    );
}

#[tokio::test]
async fn change_password_rejects_same_new() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/me")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "current_password=correct-horse-battery\
                 &new_password=correct-horse-battery\
                 &new_password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        body_string(response)
            .await
            .contains("different from the current")
    );
}

#[tokio::test]
async fn email_editor_is_embedded_in_settings_for_admin() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // The Settings page carries the Email (SMTP) section…
    let response = send(
        &app,
        Request::builder()
            .uri("/settings/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Email (SMTP) settings"), "{html}");

    // …and its save endpoint re-renders the whole page with a flash and
    // the just-saved values.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/email")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "enabled=1&smtp_host=smtp.example.com&from_address=noreply%40example.com\
                 &smtp_port=587&tls=starttls",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Email settings saved"), "{html}");
    assert!(html.contains("smtp.example.com"), "{html}");
    assert!(html.contains("Update password"), "{html}");

    // The old dedicated page is gone.
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/email")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn email_editor_hidden_without_memory_handles() {
    // `make_app` wires no MemoryHandles (no workdir), so the settings
    // page must degrade to hiding the section rather than erroring.
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/settings/me")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(!html.contains("Email (SMTP) settings"), "{html}");
}
