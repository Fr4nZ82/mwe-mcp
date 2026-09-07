// SPDX-License-Identifier: AGPL-3.0-or-later
//! No page offers a reader a link the server would refuse them.
//!
//! The operator's consoles answer a non-admin with `403`. A page that
//! links to one anyway has told somebody to go somewhere they cannot go
//! — and on a phone, where the link is the whole affordance, that is the
//! end of the road rather than a detour.
//!
//! So: render the pages a person who is not the admin actually walks
//! through, follow every `/dashboard/...` link on them with that same
//! person's session, and refuse anything that answers `4xx` / `5xx`.
//! `405` is excluded: a form `action` quoted in the page is POST-only and
//! is not a link a reader can follow.

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

/// Every distinct `/dashboard/...` target quoted in `href=` or `action=`.
fn targets(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for marker in ["href=\"", "action=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let Some(end) = rest.find('"') else { break };
            let target = &rest[..end];
            if target.starts_with("/dashboard/")
                && !target.starts_with("/dashboard/static/")
                && !out.iter().any(|t| t == target)
            {
                out.push(target.to_owned());
            }
            rest = &rest[end..];
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn a_reader_is_never_pointed_at_a_page_that_refuses_them() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = 'bob'")
        .execute(&pool)
        .await
        .unwrap();

    let mut dead: Vec<String> = Vec::new();
    for page in [
        "/home",
        "/wiki",
        "/facts",
        "/facts/sections",
        "/skills",
        "/bridges",
        "/recall-traces",
        "/settings/me",
        "/settings/2fa",
        "/chat",
        "/help",
    ] {
        let response = send(
            &app,
            Request::builder()
                .uri(page)
                .header(header::HOST, "memory.example.org")
                .header(header::COOKIE, member.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{page}");
        let html = body_string(response).await;
        for target in targets(&html) {
            let uri = target.trim_start_matches("/dashboard");
            let uri = if uri.is_empty() { "/" } else { uri };
            let response = send(
                &app,
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "memory.example.org")
                    .header(header::COOKIE, member.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            let code = response.status();
            // POST-only endpoints quoted as a form `action` are not links.
            if code.is_client_error() && code != StatusCode::METHOD_NOT_ALLOWED
                || code.is_server_error()
            {
                dead.push(format!("{page} → {target} ({code})"));
            }
        }
    }
    assert!(
        dead.is_empty(),
        "pages offering a reader somewhere they cannot go:\n{}",
        dead.join("\n")
    );
}

/// The counterpart: the operator's consoles do refuse them. Without this
/// the test above could be satisfied by opening the doors.
#[tokio::test]
async fn the_operator_consoles_still_refuse_a_reader() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = 'bob'")
        .execute(&pool)
        .await
        .unwrap();

    for uri in [
        "/tokens",
        "/users",
        "/groups",
        "/prompts",
        "/admin/llm-config",
        "/admin/usage",
        "/admin/backup",
        "/dream",
    ] {
        let response = send(
            &app,
            Request::builder()
                .uri(uri)
                .header(header::HOST, "memory.example.org")
                .header(header::COOKIE, member.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
}

/// "The facts about you" on the home page narrows by **subject** — the
/// person a fact is about — not by the wiki it happens to be filed in.
/// The two are different sets: a fact about somebody can live on a
/// group's page, and their own wiki holds facts about other people.
#[tokio::test]
async fn the_home_page_points_at_the_facts_about_the_reader() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let member = make_member(&app, &admin, "bob").await;
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = 'bob'")
        .execute(&pool)
        .await
        .unwrap();

    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::HOST, "memory.example.org")
            .header(header::COOKIE, member.as_str())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(
        html.contains("/dashboard/facts?subject=user:bob"),
        "the home page must point at the facts about the reader"
    );

    // …and the fact browser understands it.
    let response = send(
        &app,
        Request::builder()
            .uri("/facts?subject=user:bob")
            .header(header::HOST, "memory.example.org")
            .header(header::COOKIE, member.as_str())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(r#"name="subject" value="user:bob""#),
        "the filter must come back filled in, so the reader can widen it"
    );
}
