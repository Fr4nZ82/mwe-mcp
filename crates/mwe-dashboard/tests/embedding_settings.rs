// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin embedding-settings editor integration tests.
//!
//! Exercises `/dashboard/admin/embedding`: the panel is one submit over
//! six fields, so what matters here is what a save that does not go
//! through hands back.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

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

/// A refused save hands the form back with what was typed still in it —
/// the offending field included, so the admin can see what they wrote and
/// fix that one thing instead of filling the panel in again.
#[tokio::test]
async fn a_refused_save_gives_the_form_back_with_the_typed_values() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/embedding")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "backend=bundled&model=bge-m3&base_url=http%3A%2F%2Fbox%3A11434\
                 &device=cpu&dimensions=lots&model_dir=%2Fopt%2Fmodels%2Fbge-m3",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    for kept in [
        r#"name="base_url" type="text" value="http://box:11434""#,
        // …including the one at the far end of the form.
        r#"name="model_dir" type="text" value="/opt/models/bge-m3""#,
        // …and the offending one, so the admin sees what they wrote.
        r#"name="dimensions" type="number" min="1" value="lots""#,
    ] {
        assert!(html.contains(kept), "a refused save dropped `{kept}`");
    }
    // The two dropdowns keep the choice as well, rather than snapping back
    // to what is on disk.
    let backend_option = html
        .split("<option value=\"bundled\"")
        .nth(1)
        .and_then(|rest| rest.split('>').next())
        .unwrap_or_default()
        .to_owned();
    assert!(
        backend_option.contains("selected"),
        "the chosen backend must come back selected, got `<option value=\"bundled\"{backend_option}>`"
    );
}

/// The refusal names the field, and writes nothing: the config file is
/// still not there after a save the parser turned away.
#[tokio::test]
async fn a_refused_save_writes_nothing_and_names_the_field() {
    let (app, _pool, _tree, dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/embedding")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("backend=weaviate"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(
        html.contains("weaviate"),
        "the reason must name what was refused: {html}"
    );
    assert!(
        html.contains(r#"name="model""#),
        "the reason rides on the panel itself, not on a page of its own: {html}"
    );
    assert!(
        !dir.path().join(mwe_core::config::CONFIG_FILENAME).exists(),
        "a refused save must not write the config file"
    );
}
