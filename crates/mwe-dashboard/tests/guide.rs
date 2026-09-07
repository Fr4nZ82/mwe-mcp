// SPDX-License-Identifier: AGPL-3.0-or-later
//! The guide is read inside the dashboard, from the copy the binary
//! carries.
//!
//! `GET /dashboard/guide` is the map and `GET /dashboard/guide/<path>` a
//! page. The reader who is not the admin gets the map with the operator's
//! half left out — the map itself, answered `200`, not a refusal — and
//! `403` on an operator page reached by address.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};

async fn login_as_admin(app: &axum::Router) -> String {
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

/// Create `user_id` as a plain user and walk their invitation to a
/// password, returning their session cookie.
async fn make_member(app: &axum::Router, admin_cookie: &str, user_id: &str) -> String {
    let response = send(
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
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .expect("terminator");
    let invitation_id = &after[..end];

    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=correct-horse-battery&password_confirm=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("member cookie"))
}

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, String) {
    let response = send(
        app,
        Request::builder()
            .uri(uri)
            .header(header::HOST, "memory.example.org")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = response.status();
    (status, body_string(response).await)
}

/// Every `/dashboard/guide...` target quoted in an `href` on the page.
fn guide_links(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("href=\"") {
        rest = &rest[i + 6..];
        let Some(end) = rest.find('"') else { break };
        let target = &rest[..end];
        if target.starts_with("/dashboard/guide") && !out.iter().any(|t| t == target) {
            out.push(target.to_owned());
        }
        rest = &rest[end..];
    }
    out
}

#[tokio::test]
async fn the_map_and_a_page_are_served_from_the_binary() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;

    let (status, html) = get(&app, "/guide", &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("The mwe-mcp guide"), "{html}");
    // A relative markdown link became the route that serves it.
    assert!(
        html.contains("href=\"/dashboard/guide/user/your-facts\""),
        "{html}"
    );

    let (status, html) = get(&app, "/guide/user/your-facts", &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Your facts</h1>"), "{html}");
    assert!(html.contains("wiki-page-view"), "{html}");
}

/// The documents that are not the guide are read where they live.
#[tokio::test]
async fn a_link_out_of_the_guide_points_at_the_repository() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;

    let (status, html) = get(&app, "/guide", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("https://github.com/Fr4nZ82/mwe-mcp/blob/main/INSTALL.md"),
        "{html}"
    );
}

/// The chosen shape: the reader gets the map, minus the operator's half.
/// Not a refusal, and not the whole listing with twenty links that would
/// each answer `403`.
#[tokio::test]
async fn a_reader_gets_the_map_without_the_operators_half() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;

    let (status, readers_map) = get(&app, "/guide", &member).await;
    assert_eq!(status, StatusCode::OK, "a reader is not refused the map");
    assert!(
        !readers_map.contains("/dashboard/guide/operator/"),
        "the reader's map lists an operator page:\n{readers_map}"
    );
    assert!(
        readers_map.contains("href=\"/dashboard/guide/user/the-chat\""),
        "the reader's map lost their own half:\n{readers_map}"
    );

    let (status, admins_map) = get(&app, "/guide", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        admins_map.contains("href=\"/dashboard/guide/operator/tokens\""),
        "the admin's map lost the operator's half:\n{admins_map}"
    );
}

#[tokio::test]
async fn an_operator_page_answers_a_reader_but_not_the_admin_with_a_refusal() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;

    let (status, _) = get(&app, "/guide/operator/tokens", &member).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, html) = get(&app, "/guide/operator/tokens", &admin).await;
    assert_eq!(status, StatusCode::OK, "{html}");
}

#[tokio::test]
async fn a_page_that_is_not_there_is_a_404() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;

    for uri in [
        "/guide/user/there-is-no-such-page",
        "/guide/operator/there-is-no-such-page",
        // The map has one address, `/dashboard/guide`.
        "/guide/README",
    ] {
        let (status, _) = get(&app, uri, &admin).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// A page path names a file inside `docs/` and nothing else. `rust_embed`
/// resolves a relative path against the filesystem when the binary is
/// built without optimisations, so a `.` or `..` segment that reached it
/// would step around the check that keeps the operator's half the
/// admin's.
#[tokio::test]
async fn a_path_that_walks_out_of_the_guide_reaches_nothing() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;

    for uri in [
        "/guide/./operator/tokens",
        "/guide/user/../operator/tokens",
        "/guide/operator/../operator/tokens",
    ] {
        let (status, _) = get(&app, uri, &member).await;
        assert_ne!(status, StatusCode::OK, "{uri} reached an operator page");
    }
}

/// Walk the guide as a reader: every page they are offered opens, and
/// none of them hands them a link into the operator's half.
#[tokio::test]
async fn every_page_a_reader_is_offered_opens_for_them() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;

    let mut queue = vec!["/dashboard/guide".to_owned()];
    let mut seen: Vec<String> = Vec::new();
    while let Some(target) = queue.pop() {
        if seen.contains(&target) {
            continue;
        }
        seen.push(target.clone());
        let uri = target.trim_start_matches("/dashboard");
        let (status, html) = get(&app, uri, &member).await;
        assert_eq!(status, StatusCode::OK, "{target}");
        for link in guide_links(&html) {
            assert!(
                !link.starts_with("/dashboard/guide/operator/"),
                "{target} offers a reader {link}, which refuses them"
            );
            queue.push(link);
        }
    }
    assert!(seen.len() > 10, "the walk found almost nothing: {seen:?}");
}

/// The way in: everybody has "Guide" in the top bar.
#[tokio::test]
async fn the_top_bar_offers_the_guide() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1")
        .execute(&pool)
        .await
        .unwrap();

    for cookie in [&admin, &member] {
        let (status, html) = get(&app, "/home", cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("href=\"/dashboard/guide\""), "{html}");
        assert!(html.contains(">Guide<"), "{html}");
    }
}

/// The "?" beside a screen's title opens the guide page for that screen,
/// and is absent where the page would refuse the reader.
#[tokio::test]
async fn a_screen_points_at_its_own_page_of_the_guide() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = 'bob'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, html) = get(&app, "/facts", &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("href=\"/dashboard/guide/user/your-facts\""),
        "{html}"
    );

    let (status, html) = get(&app, "/tokens", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("href=\"/dashboard/guide/operator/tokens\""),
        "{html}"
    );

    // The guide page for that screen carries no mark back to itself, and
    // prints its name once: the layout's title is the page's own heading.
    let (status, html) = get(&app, "/guide/operator/tokens", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !html.contains("href=\"/dashboard/guide/operator/tokens\""),
        "the guide page points at itself:\n{html}"
    );
    assert_eq!(html.matches("Tokens</h1>").count(), 1, "{html}");
    assert!(!html.contains("<h1 id="), "{html}");
}
