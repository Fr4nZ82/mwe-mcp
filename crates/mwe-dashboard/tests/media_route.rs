// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dashboard media alias — auth + ACL integration tests.
//!
//! GET `/dashboard/media/:catalog_id` serves the bytes behind an
//! `{{embed=…}}` marker to the session principal, gated by the
//! per-media ACL (`media_catalog` row, `can_read` semantics — no admin
//! bypass). The byte-level mechanics (store, dedup, widening) are
//! covered by `mwe_core::media` unit tests; here we pin the route
//! contract: owner reads, an outsider gets 403 even though the page
//! marker is visible, widening opens the read, unknown ids 404,
//! malformed ids 400, and an anonymous request bounces to login.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use mwe_core::media;

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

/// Mint a non-admin session via the invitation cycle.
async fn login_as_non_admin(app: &Router, admin_cookie: &str) -> String {
    use common::body_string;
    let create_resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, admin_cookie)
            .body(Body::from("user_id=bilbo&email=bilbo@example.com&aliases="))
            .unwrap(),
    )
    .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let create_html = body_string(create_resp).await;
    let prefix = "/dashboard/accept-invite/";
    let start = create_html.find(prefix).expect("invitation link");
    let after = &create_html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .unwrap();
    let invitation_id = &after[..end];
    let accept_resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=bilbo-pw-secret-12&password_confirm=bilbo-pw-secret-12",
            ))
            .unwrap(),
    )
    .await;
    extract_cookie_value(&extract_set_cookie(&accept_resp, "mwe_session").expect("cookie"))
}

async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    use http_body_util::BodyExt;
    response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec()
}

fn media_request(catalog_id: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/media/{catalog_id}"))
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn owner_reads_outsider_denied_until_widened() {
    let (app, pool, _tree, dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app).await;
    let bilbo = login_as_non_admin(&app, &alice).await;

    let stored = media::store_media(
        &pool,
        dir.path(),
        media::NewMedia {
            bytes: b"jpegbytes".to_vec(),
            kind: media::kind::PHOTO.to_owned(),
            mime: "image/jpeg".to_owned(),
            owner: "user:alice".parse().unwrap(),
            uploaded_by_consumer: None,
            caption: None,
            description: None,
            original_filename: Some("photo.jpg".to_owned()),
        },
    )
    .await
    .expect("store");
    let cid = stored.row.catalog_id;

    // The owner streams the bytes with the right content type.
    let resp = send(&app, media_request(cid.as_str(), &alice)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/jpeg")
    );
    assert_eq!(body_bytes(resp).await, b"jpegbytes");

    // An authenticated outsider is denied at byte-serving time. The 403
    // carries the content-ACL copy ("you don't have access"), never the
    // admin-gate copy — `row_visible_to` has no admin bypass, so telling
    // the caller "admin rights required" would be a misleading hint.
    let resp = send(&app, media_request(cid.as_str(), &bilbo)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let denied_body = String::from_utf8(body_bytes(resp).await).expect("utf8 body");
    assert!(
        denied_body.contains("don't have access"),
        "403 must carry the content-ACL copy, got: {denied_body}"
    );
    assert!(
        !denied_body.contains("Admin rights required"),
        "media ACL 403 must not claim admin rights would help: {denied_body}"
    );

    // A fact embedding the media widens its read set — bilbo now reads.
    media::widen_acl(
        &pool,
        &cid,
        &"user:alice".parse().unwrap(),
        &["user:bilbo".parse().unwrap()],
        None,
    )
    .await
    .expect("widen");
    let resp = send(&app, media_request(cid.as_str(), &bilbo)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    drop(dir);
}

#[tokio::test]
async fn unknown_and_malformed_ids_and_anonymous_requests_are_rejected() {
    let (app, _pool, _tree, dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app).await;

    let resp = send(&app, media_request("c-2026-06-12-photo-999.jpg", &alice)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = send(&app, media_request("not-a-catalog-id", &alice)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // No session → the auth layer bounces to the login page.
    let resp = send(
        &app,
        Request::builder()
            .uri("/media/c-2026-06-12-photo-001.jpg")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(resp.status().is_redirection(), "{}", resp.status());
    drop(dir);
}
