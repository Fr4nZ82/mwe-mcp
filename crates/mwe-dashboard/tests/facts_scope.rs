// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the fact browser is scoped to when somebody opens it.
//!
//! A person arriving from the top bar is asking what the memory holds
//! about *them*; an admin opening the same page is looking at the
//! deployment. Both questions have an answer here, and the widening from
//! the first to the second has to survive the pager, so it rides in the
//! query string rather than in a flag on the session.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::types::{Principal, WikiId};
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;

async fn login_as_admin(app: &axum::Router, pool: &SqlitePool) -> String {
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
    mark_profile_finished(pool, "alice").await;
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

async fn mark_profile_finished(pool: &SqlitePool, user_id: &str) {
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("mark the profile finished");
}

/// Create `user_id` as a plain user and walk their invitation to a
/// password, returning their session cookie.
async fn make_member(
    app: &axum::Router,
    pool: &SqlitePool,
    admin_cookie: &str,
    user_id: &str,
) -> String {
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
    mark_profile_finished(pool, user_id).await;
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("member cookie"))
}

/// Drop a standard `wiki-user` wiki at `<workdir>/wikis/<wiki_id>`.
fn seed_wiki(tree: &WikiTree, wiki_id: &str) {
    let dir = tree.wikis_dir().join(wiki_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        "---\n\
         wiki_id: {wiki_id}\n\
         wiki_type: wiki-user\n\
         parent_wiki_id: null\n\
         slug: {wiki_id}\n\
         title: {wiki_id}\n\
         acl_default: 'user:{wiki_id}'\n\
         ---\n"
    );
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Capture one fact into `wiki_id`, about `subject`, additionally readable
/// by `allow`.
async fn capture_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    body: &str,
    subject: &str,
    allow: &[&str],
) {
    use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        subject_external: None,
        slot: None,
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: Some(std::path::PathBuf::from("notes.md")),
        body: body.to_owned(),
        subject: subject.parse::<Principal>().unwrap(),
        allow: allow
            .iter()
            .map(|a| a.parse::<Principal>().unwrap())
            .collect(),
        sender: None,
        fact_type: None,
        topics: vec![],
        dedup_threshold: Some(1.01),
        valid_from: None,
        valid_to: None,
        style: None,
        page_description: None,
        salience: None,
    };
    let outcome = wiki_capture(tree, pool, embedder, req).await.unwrap();
    assert!(
        matches!(outcome.action, CaptureAction::Captured { .. }),
        "{:?}",
        outcome.action
    );
}

async fn facts_page(app: &axum::Router, cookie: &str, uri: &str) -> String {
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
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    body_string(response).await
}

const ABOUT_BOB: &str = "Bob keeps his bike in the hallway";
const ABOUT_CAROL: &str = "Carol swims on Tuesdays";

/// Seed two facts both alice and bob may read: one about bob, one about
/// carol. Neither is about alice, so an admin narrowed to themselves would
/// see nothing at all — which is what the admin test rests on.
async fn seed_two_facts(
    app: &axum::Router,
    pool: &SqlitePool,
    tree: &WikiTree,
) -> (String, String) {
    let admin = login_as_admin(app, pool).await;
    let bob = make_member(app, pool, &admin, "bob").await;
    seed_wiki(tree, "bob");
    capture_fact(pool, tree, "bob", ABOUT_BOB, "user:bob", &["user:alice"]).await;
    capture_fact(
        pool,
        tree,
        "bob",
        ABOUT_CAROL,
        "user:carol",
        &["user:bob", "user:alice"],
    )
    .await;
    (admin, bob)
}

/// Opening Facts from the top bar is the question people arrive with:
/// what does this thing know about me. The list is narrowed to them —
/// not to their wiki, which is a different set — and the About field
/// shows the narrowing, so it can be seen and cleared.
#[tokio::test]
async fn a_person_opens_facts_on_the_facts_about_them() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let (_admin, bob) = seed_two_facts(&app, &pool, &tree).await;

    let html = facts_page(&app, &bob, "/facts").await;
    assert!(html.contains(ABOUT_BOB), "{html}");
    assert!(
        !html.contains(ABOUT_CAROL),
        "a bare arrival is scoped to the reader, not to everything they may read: {html}"
    );
    assert!(
        html.contains(r#"name="subject" value="user:bob""#),
        "the About field must show the narrowing: {html}"
    );
    assert!(
        html.contains(r#"href="/dashboard/facts?subject=""#),
        "and the widening must be one click away: {html}"
    );
    // The pager carries the narrowing, so page two is still about them.
    assert!(html.contains("subject=user%3Abob"), "{html}");
}

/// The widening link is an empty `About`, which is a query string — and a
/// query string is taken literally, so the second click is not undone by
/// the rule that made the first one narrow.
#[tokio::test]
async fn the_widening_link_shows_everything_the_person_can_read() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let (_admin, bob) = seed_two_facts(&app, &pool, &tree).await;

    let html = facts_page(&app, &bob, "/facts?subject=").await;
    assert!(html.contains(ABOUT_BOB), "{html}");
    assert!(
        html.contains(ABOUT_CAROL),
        "the wider list is every fact the reader may read: {html}"
    );
    assert!(
        html.contains(r#"name="subject" value=""#),
        "the About field comes back empty: {html}"
    );
}

/// The console is the admin's view of the deployment: it opens on all of
/// it, narrowed to nobody.
#[tokio::test]
async fn the_admin_opens_facts_on_the_whole_deployment() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let (admin, _bob) = seed_two_facts(&app, &pool, &tree).await;

    let html = facts_page(&app, &admin, "/facts").await;
    // Neither fact is about alice: narrowing her to herself would empty
    // the page.
    assert!(html.contains(ABOUT_BOB), "{html}");
    assert!(html.contains(ABOUT_CAROL), "{html}");
    assert!(
        !html.contains(r#"name="subject" value="user:alice""#),
        "an admin is never narrowed to themselves: {html}"
    );
    assert!(
        !html.contains("Everything I can read"),
        "and is offered no widening, because nothing was narrowed: {html}"
    );
}
