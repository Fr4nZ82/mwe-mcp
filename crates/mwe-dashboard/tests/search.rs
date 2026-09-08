// SPDX-License-Identifier: AGPL-3.0-or-later
//! Searching for the pages a word appears on.
//!
//! The question the surface answers is narrow — *where do these words
//! appear* — and the thing that has to hold is that the answer is drawn from
//! what the person searching may read, and from nothing else. Two people
//! typing the same word into the same deployment get two different lists, and
//! neither of them learns that the other's page exists.

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

/// Create `user_id` as a plain user and walk their invitation to a password,
/// returning their session cookie.
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

/// Drop a smart wiki under `owner`'s own wiki, which is what makes `owner`
/// its owner: a smart wiki's scope principal is derived by walking up to the
/// identity root, never declared.
fn seed_smart_wiki(tree: &WikiTree, owner: &str, wiki_id: &str, page: &str, body: &str) {
    let dir = tree.wikis_dir().join(owner).join(wiki_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        "---\n\
         wiki_id: {wiki_id}\n\
         wiki_type: wiki-tech\n\
         parent_wiki_id: {owner}\n\
         slug: {wiki_id}\n\
         title: {wiki_id}\n\
         smart: true\n\
         ---\n"
    );
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
    std::fs::write(dir.join(page), body).unwrap();
}

/// Capture one fact into `wiki_id`, on `page`, about `subject`, additionally
/// readable by `allow`.
async fn capture_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    page: &str,
    body: &str,
    subject: &str,
    allow: &[&str],
) {
    use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        subject_external: None,
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: Some(std::path::PathBuf::from(page)),
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

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> String {
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

async fn search(app: &axum::Router, cookie: &str, q: &str) -> String {
    get(app, &format!("/search?q={q}"), cookie).await
}

/// Alice's own fact, on her own page, found by a word inside it — with the
/// link that opens the page and the line the word is in.
#[tokio::test]
async fn a_word_in_your_own_fact_brings_back_its_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    capture_fact(
        &pool,
        &tree,
        "alice",
        "salute.md",
        "Il pediatra di Alice è il dottor Bianchi.",
        "user:alice",
        &[],
    )
    .await;

    let html = search(&app, &alice, "pediatra").await;
    assert!(
        html.contains(r#"href="/dashboard/wiki/alice/view/salute.md""#),
        "the hit must link to the page view: {html}"
    );
    assert!(
        html.contains("dottor Bianchi"),
        "and carry the line the word is in: {html}"
    );
    // A link that does not open is worse than no link.
    get(&app, "/wiki/alice/view/salute.md", &alice).await;

    // Half a word finds the whole one; the middle of a word finds nothing.
    let html = search(&app, &alice, "pediatr").await;
    assert!(html.contains("salute.md"), "{html}");
    let html = search(&app, &alice, "iatra").await;
    assert!(
        !html.contains("salute.md"),
        "a match inside a word is not a match: {html}"
    );
}

/// The gate that matters: a fact written for somebody else never brings back
/// its page, and the searcher is not told the page exists.
#[tokio::test]
async fn a_fact_you_may_not_read_never_brings_back_its_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    let bob = make_member(&app, &pool, &alice, "bob").await;
    seed_wiki(&tree, "alice");
    capture_fact(
        &pool,
        &tree,
        "alice",
        "salute.md",
        "Il pediatra di Alice è il dottor Bianchi.",
        "user:alice",
        &[],
    )
    .await;

    let html = search(&app, &bob, "pediatra").await;
    assert!(
        !html.contains("salute.md") && !html.contains("Bianchi"),
        "bob was shown alice's private page: {html}"
    );
    assert!(
        html.contains("Nothing you can read carries"),
        "and he should be told plainly that there is nothing: {html}"
    );
}

/// The other half of the same rule: a fact alice shared with bob is found by
/// both of them, on the same page.
#[tokio::test]
async fn a_shared_fact_is_found_by_both_of_them() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    let bob = make_member(&app, &pool, &alice, "bob").await;
    seed_wiki(&tree, "alice");
    capture_fact(
        &pool,
        &tree,
        "alice",
        "casa.md",
        "Le chiavi di casa sono dal vicino.",
        "user:alice",
        &["user:bob"],
    )
    .await;

    for (who, cookie) in [("alice", &alice), ("bob", &bob)] {
        let html = search(&app, cookie, "chiavi").await;
        assert!(
            html.contains(r#"href="/dashboard/wiki/alice/view/casa.md""#),
            "{who} lost a page shared with them: {html}"
        );
    }
}

/// A smart wiki holds no facts, so its pages are matched whole — and only for
/// somebody who may open the wiki at all.
#[tokio::test]
async fn a_smart_wikis_page_is_found_only_by_its_readers() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    let bob = make_member(&app, &pool, &alice, "bob").await;
    seed_wiki(&tree, "alice");
    seed_smart_wiki(
        &tree,
        "alice",
        "proj",
        "deploy.md",
        "# Deploy\n\nIl rilascio passa dal tunnel.\n",
    );

    let html = search(&app, &alice, "rilascio").await;
    assert!(
        html.contains(r#"href="/dashboard/wiki/proj/view/deploy.md""#),
        "the owner must find the page of their own project wiki: {html}"
    );
    get(&app, "/wiki/proj/view/deploy.md", &alice).await;

    let html = search(&app, &bob, "rilascio").await;
    assert!(
        !html.contains("deploy.md"),
        "a smart wiki bob may not open must not be searched for him: {html}"
    );
}

/// Two words are an AND, and they have to meet in the same place.
#[tokio::test]
async fn two_words_come_back_only_where_both_appear() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    capture_fact(
        &pool,
        &tree,
        "alice",
        "salute.md",
        "Il pediatra di Alice è il dottor Bianchi.",
        "user:alice",
        &[],
    )
    .await;

    let html = search(&app, &alice, "pediatra+bianchi").await;
    assert!(html.contains("salute.md"), "{html}");

    let html = search(&app, &alice, "pediatra+rossi").await;
    assert!(
        !html.contains("salute.md"),
        "one word of two is not a match: {html}"
    );
}

/// Accents and capitals are folded on both sides, so a person typing on a
/// keyboard that makes accents awkward still finds the page.
#[tokio::test]
async fn the_accent_and_the_capital_make_no_difference() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    capture_fact(
        &pool,
        &tree,
        "alice",
        "citta.md",
        "Alice vive in città da marzo.",
        "user:alice",
        &[],
    )
    .await;

    for query in ["citta", "CITTA", "citt%C3%A0"] {
        let html = search(&app, &alice, query).await;
        assert!(html.contains("citta.md"), "`{query}` found nothing: {html}");
    }
}

/// An empty box is the box as it opens, not a search that found nothing.
#[tokio::test]
async fn an_empty_query_asks_nothing_and_says_so() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;

    let html = get(&app, "/search", &alice).await;
    assert!(
        html.contains("Type a word above and press Search."),
        "{html}"
    );
    assert!(
        !html.contains("Nothing you can read carries"),
        "an unasked question has no negative answer: {html}"
    );
}

/// The way in: the box is in the top bar of every page, for everybody.
#[tokio::test]
async fn the_top_bar_carries_the_search_box() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;
    let bob = make_member(&app, &pool, &alice, "bob").await;

    for (who, cookie) in [("alice", &alice), ("bob", &bob)] {
        let html = get(&app, "/home", cookie).await;
        assert!(
            html.contains(r#"action="/dashboard/search""#) && html.contains(r#"name="q""#),
            "{who} has no search box in the top bar: {html}"
        );
    }
}

/// The guide page is one press away, the way every other screen's is.
#[tokio::test]
async fn the_screen_points_at_its_guide_page() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let alice = login_as_admin(&app, &pool).await;

    let html = get(&app, "/search", &alice).await;
    assert!(
        html.contains(r#"href="/dashboard/guide/user/search""#),
        "{html}"
    );
    let page = get(&app, "/guide/user/search", &alice).await;
    assert!(page.contains("Search</h1>"), "{page}");
}
