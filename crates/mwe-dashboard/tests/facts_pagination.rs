// SPDX-License-Identifier: AGPL-3.0-or-later
//! Paging the fact browser, and the one thing that makes paging honest.
//!
//! A page is a window onto an ordered list, so the order has to be TOTAL: if
//! two rows can come back in either order, the window's edge falls inside the
//! tie and the second page deals it differently — a fact shows up twice, or
//! never. Every sort the browser offers has ties by the thousand (one turn
//! files several facts in the same second; a sort by kind puts a whole
//! taxonomy bucket on one value), so the ordering carries the fact's own id as
//! the last word.

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
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1 WHERE user_id = 'alice'")
        .execute(pool)
        .await
        .expect("mark the profile finished");
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

fn seed_wiki(tree: &WikiTree, wiki_id: &str) {
    let dir = tree.wikis_dir().join(wiki_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        format!(
            "---\nwiki_id: {wiki_id}\nwiki_type: wiki-user\nparent_wiki_id: null\n\
             slug: {wiki_id}\ntitle: {wiki_id}\nacl_default: 'user:{wiki_id}'\n---\n"
        ),
    )
    .unwrap();
}

/// One captured fact, about `subject`, in `wiki_id`.
async fn capture_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    body: &str,
    subject: &str,
) {
    use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        excluded: Vec::new(),
        subject_external: None,
        slot: None,
        slot_value: None,
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: Some(std::path::PathBuf::from("notes.md")),
        body: body.to_owned(),
        subject: subject.parse::<Principal>().unwrap(),
        allow: Vec::new(),
        sender: None,
        fact_type: Some("bio".to_owned()),
        topics: vec!["home".to_owned()],
        // Every body here is «fact number N», which a similarity dedup would
        // read as one claim said over and over. The browser's subject is the
        // rows, so the capture is told to keep them all.
        dedup_threshold: Some(1.01),
        valid_from: None,
        valid_to: None,
        style: None,
        page_description: None,
        salience: Some("normal".to_owned()),
    };
    let outcome = wiki_capture(tree, pool, embedder, req)
        .await
        .expect("capture");
    assert!(
        matches!(outcome.action, CaptureAction::Captured { .. }),
        "{:?}",
        outcome.action
    );
}

/// The fact ids the rendered page lists, in the order they are shown.
fn ids_on(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in html.match_indices("/dashboard/facts/") {
        let rest = &html[i + "/dashboard/facts/".len()..];
        let end = rest.find(['/', '"', '?']).unwrap_or(rest.len());
        let id = &rest[..end];
        if id.len() == 36 && !out.contains(&id.to_owned()) {
            out.push(id.to_owned());
        }
    }
    out
}

/// **Every fact is on exactly one page.**
///
/// A hundred and twenty facts captured in one go: they share a second, which
/// is the shape an import and a busy turn both have, so the default sort is
/// one long tie. The three pages must partition them — no fact on two pages,
/// none on none — and the last one must not be empty.
#[tokio::test]
async fn the_pages_partition_the_facts() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    let tree = WikiTree::open(tree.workdir()).expect("reopen");
    for i in 0..120 {
        capture_fact(
            &pool,
            &tree,
            "alice",
            &format!("fact number {i}"),
            "user:alice",
        )
        .await;
    }

    let open = |uri: String| {
        let session = session.clone();
        let app = app.clone();
        async move {
            let response = send(
                &app,
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, session)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };

    let mut seen: Vec<String> = Vec::new();
    for page in 1..=3 {
        let html = open(format!(
            "/facts?subject=user:alice&page={page}&page_size=50"
        ))
        .await;
        let ids = ids_on(&html);
        assert!(
            !ids.is_empty(),
            "page {page} of three is empty, and the last one holds twenty: {html}"
        );
        for id in ids {
            assert!(
                !seen.contains(&id),
                "fact {id} is on two pages at once — the order is not total"
            );
            seen.push(id);
        }
    }
    assert_eq!(
        seen.len(),
        120,
        "every captured fact is reachable by walking the pages"
    );

    let first = open("/facts?subject=user:alice&page=1&page_size=50".to_owned()).await;
    // The page as a browser gets it: set MWE_DUMP_HTML to a directory and the
    // first two pages are written there, for a look at the real screen.
    if let Ok(dir) = std::env::var("MWE_DUMP_HTML") {
        std::fs::write(format!("{dir}/facts-page-1.html"), &first).expect("dump");
        let second = open("/facts?subject=user:alice&page=2&page_size=50".to_owned()).await;
        std::fs::write(format!("{dir}/facts-page-2.html"), &second).expect("dump");
    }
    assert!(
        first.contains("120 facts"),
        "the pager says how many there are in all: {first}"
    );
    assert!(
        first.contains("of 3"),
        "and how many pages that is: {first}"
    );
}

/// **A filter survives the walk.**
///
/// Paging with a filter on must keep the filter: a reader who narrowed to one
/// person and stepped to page two is still asking about that person, and the
/// page count they are shown is the narrowed one.
#[tokio::test]
async fn a_filter_rides_along_from_page_to_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    seed_wiki(&tree, "bob");
    let tree = WikiTree::open(tree.workdir()).expect("reopen");
    for i in 0..60 {
        capture_fact(
            &pool,
            &tree,
            "alice",
            &format!("alice fact {i}"),
            "user:alice",
        )
        .await;
    }
    for i in 0..10 {
        capture_fact(&pool, &tree, "bob", &format!("bob fact {i}"), "user:bob").await;
    }

    let response = send(
        &app,
        Request::builder()
            .uri("/facts?wiki_id=alice&page=1&page_size=50")
            .header(header::COOKIE, session.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("60 facts"),
        "the total is the narrowed one, not the whole memory: {html}"
    );
    assert!(
        html.contains("wiki_id=alice") && html.contains("page=2"),
        "and the link to the next page carries the filter: {html}"
    );
    assert!(
        !html.contains("bob fact"),
        "nothing from outside the filter is listed: {html}"
    );

    let response = send(
        &app,
        Request::builder()
            .uri("/facts?wiki_id=alice&page=2&page_size=50")
            .header(header::COOKIE, session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let second = body_string(response).await;
    assert!(
        !second.contains("bob fact"),
        "and page two is still inside it: {second}"
    );
    assert_eq!(ids_on(&second).len(), 10, "the rest of alice's sixty");
}

/// **The count follows the lens.**
///
/// A reader is told how many facts THEY can read. An admin who turns the
/// reveal on is looking at the deployment, and the number has to move with the
/// rows — a total that counted everybody's while the table listed one person's
/// would be the page contradicting itself.
#[tokio::test]
async fn the_total_moves_with_the_reveal_lens() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app, &pool).await;
    seed_wiki(&tree, "alice");
    seed_wiki(&tree, "bob");
    let tree = WikiTree::open(tree.workdir()).expect("reopen");
    for i in 0..4 {
        capture_fact(
            &pool,
            &tree,
            "alice",
            &format!("alice fact {i}"),
            "user:alice",
        )
        .await;
    }
    for i in 0..7 {
        capture_fact(&pool, &tree, "bob", &format!("bob fact {i}"), "user:bob").await;
    }

    let ask = |reveal: bool| {
        let session = session.clone();
        let app = app.clone();
        async move {
            let mut req = Request::builder().uri("/facts?page_size=50");
            req = req.header(header::COOKIE, session);
            if reveal {
                req = req.header(header::COOKIE, "mwe_admin_reveal=1");
            }
            let response = send(&app, req.body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };

    let mine = ask(false).await;
    assert!(
        mine.contains("4 facts"),
        "hers alone, and the count says so: {mine}"
    );
    assert!(!mine.contains("bob fact"), "and bob's are not listed");

    let everyones = ask(true).await;
    assert!(
        everyones.contains("11 facts"),
        "under the lens the count is the deployment's: {everyones}"
    );
    assert!(
        everyones.contains("bob fact"),
        "and the rows are too — the number and the table agree"
    );
}
