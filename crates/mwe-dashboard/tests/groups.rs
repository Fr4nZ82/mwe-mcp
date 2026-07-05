// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end integration of groups CRUD: create, edit membership,
//! reject invalid ids, refuse duplicate ids, delete.

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

#[tokio::test]
async fn admin_creates_group_with_members() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "gandalf").await;
    create_user(&app, &cookie, "aragorn").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from(
                "group_id=fellowship&members=gandalf&members=aragorn&scope=Quests, shared decisions",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Created group fellowship"), "{html}");
    assert!(html.contains("gandalf, aragorn") || html.contains("aragorn, gandalf"));
}

#[tokio::test]
async fn group_create_rejects_id_clashing_with_user() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "shire").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("group_id=shire&scope="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("clashes with an existing user"), "{html}");
}

#[tokio::test]
async fn group_create_rejects_degree_sign() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("group_id=a°b&scope="))
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(html.contains("no `°`"), "{html}");
}

#[tokio::test]
async fn group_create_rejects_unknown_member() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("group_id=ghosts&members=nobody&scope="))
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(html.contains("Unknown user id"), "{html}");
}

#[tokio::test]
async fn group_edit_and_delete_roundtrip() {
    let (app, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    create_user(&app, &cookie, "gimli").await;
    create_user(&app, &cookie, "legolas").await;

    // Create.
    let _ = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("group_id=duo&members=gimli&scope="))
            .unwrap(),
    )
    .await;

    // Edit — add legolas.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/duo")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .body(Body::from("members=gimli&members=legolas&scope=Outings"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Updated group duo"), "{html}");

    // Delete.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/groups/duo/delete")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());

    // The list now omits `duo`.
    let response = send(
        &app,
        Request::builder()
            .uri("/groups")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(
        !html.contains("duo"),
        "deleted group must not appear: {html}"
    );
}

#[tokio::test]
async fn non_admin_cannot_reach_groups_page() {
    let (app, _dir) = make_app().await;
    let admin_cookie = login_as_admin(&app).await;
    create_user(&app, &admin_cookie, "merry").await;
    // Activate merry via accept-invite.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &admin_cookie)
            .body(Body::from(
                "user_id=pippin&email=pippin@example.com&aliases=",
            ))
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    let start = html.find("/dashboard/accept-invite/").unwrap();
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
                "password=pippin-pw-secret-12&password_confirm=pippin-pw-secret-12",
            ))
            .unwrap(),
    )
    .await;
    let pippin_cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    let response = send(
        &app,
        Request::builder()
            .uri("/groups")
            .header(header::COOKIE, &pippin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
