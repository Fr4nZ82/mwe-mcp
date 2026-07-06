// SPDX-License-Identifier: AGPL-3.0-or-later
//! Link fabric on the memory explorer's rendered page — the human
//! surface of the canonical wikilink grammar
//! (the recall-pipeline design note §Link grammar) plus the
//! region → source-fact click-through:
//!
//! - canonical `[[wiki_id]]` / `[[wiki_id/page-slug]]` wikilinks render
//!   as in-dashboard navigation, the `|display` alias as the label, and
//!   a target that does not resolve against the tree stays literal
//!   prose — never a broken link;
//! - every readable fact region carries a small superscript anchor to
//!   its fact record (`/dashboard/facts/:id/edit`), a redacted region
//!   does not, and the admin reveal offers the anchor on revealed
//!   regions too.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app_with_memory, send};
use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::types::{FactId, Principal, WikiId};
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;

/// Run the setup wizard (admin `alice`) + skip the welcome primer, and
/// hand back the session cookie. Mirrors the sibling explorer tests.
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
    let cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));
    let skip = send(
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
    assert!(skip.status().is_redirection(), "{}", skip.status());
    cookie
}

/// Seed a second top-level wiki `bob` so bare `[[bob]]` wiki hops have a
/// live target in the tree.
fn seed_bob_wiki(tree: &WikiTree) {
    let dir = tree.wikis_dir().join("bob");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: bob\n\
                wiki_type: wiki-user\n\
                parent_wiki_id: null\n\
                slug: bob\n\
                title: Bob\n\
                acl_default: 'user:bob'\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Capture one fact on alice's wiki via the direct capture path (writes
/// the `{{f=…}}` region to the page AND the `fact_index` row).
async fn capture_fact(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &str,
    body: &str,
    owner: &str,
) -> FactId {
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse("alice").unwrap(),
        page: PathBuf::from(page),
        body: body.to_owned(),
        owner: owner.parse::<Principal>().unwrap(),
        allow: vec![],
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
    match outcome.action {
        CaptureAction::Captured { .. } => outcome.fact_id,
        other => panic!("expected Captured, got {other:?}"),
    }
}

#[tokio::test]
async fn page_view_linkifies_canonical_wikilinks_and_leaves_dangling_literal() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // auto-creates wiki `alice`
    seed_bob_wiki(&tree);
    let alice_dir = tree.wikis_dir().join("alice");
    std::fs::write(alice_dir.join("notes.md"), "# Notes\n\nTarget page.\n").unwrap();
    std::fs::write(
        alice_dir.join("links.md"),
        "# Links\n\nA wiki hop [[bob]], a page hop [[alice/notes]], an aliased \
         [[alice/notes|My Notes]], a ghost [[ghost]], a missing [[alice/missing]] \
         and the mutant [[famiglia_bruno_battaglia/referto_oculistica]].\n",
    )
    .unwrap();

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/links.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    // Both canonical forms navigate in-dashboard.
    assert!(
        html.contains(r#"<a class="wikilink" href="/dashboard/wiki/bob">bob</a>"#),
        "wiki hop must linkify: {html}"
    );
    assert!(
        html.contains(
            r#"<a class="wikilink" href="/dashboard/wiki/alice/view/notes.md">alice/notes</a>"#
        ),
        "page hop must linkify: {html}"
    );
    // The alias renders as the label.
    assert!(
        html.contains(
            r#"<a class="wikilink" href="/dashboard/wiki/alice/view/notes.md">My Notes</a>"#
        ),
        "alias must render as the label: {html}"
    );
    // Dangling targets stay literal prose — never a broken link.
    assert!(html.contains("[[ghost]]"), "{html}");
    assert!(html.contains("[[alice/missing]]"), "{html}");
    assert!(
        html.contains("[[famiglia_bruno_battaglia/referto_oculistica]]"),
        "the mutant grammar must stay literal: {html}"
    );
    assert!(
        !html.contains(r#"href="/dashboard/wiki/ghost"#),
        "no link may point at an unknown wiki: {html}"
    );
}

#[tokio::test]
async fn page_view_readable_region_carries_fact_anchor_and_redacted_one_does_not() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // admin `alice`

    // Two promoted facts on the same page: one alice can read (her own),
    // one she cannot (bob's — no allow, no sender shortcut).
    let readable = capture_fact(&pool, &tree, "notes.md", "Alice pesa 72 kg.", "user:alice").await;
    let hidden = capture_fact(
        &pool,
        &tree,
        "notes.md",
        "Bob ha un segreto sulla dieta.",
        "user:bob",
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/notes.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    // The readable region carries the superscript anchor to its record…
    assert!(
        html.contains(&format!(
            r#"<sup class="fact-ref"><a href="/dashboard/facts/{readable}/edit""#
        )),
        "readable region must carry its fact anchor: {html}"
    );
    // …the redacted one is a fact-less placeholder: no anchor, no id.
    assert!(html.contains("[redacted]"), "{html}");
    assert!(
        !html.contains(hidden.as_str()),
        "a redacted region must not leak its fact id: {html}"
    );

    // Admin reveal: the hidden region is shown (highlighted) and now
    // carries its own anchor too — the supervision lens is one click
    // from the record of anything it shows.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/notes.md")
            .header(header::COOKIE, format!("{cookie}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(&format!(
            r#"<sup class="fact-ref"><a href="/dashboard/facts/{readable}/edit""#
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<sup class="fact-ref"><a href="/dashboard/facts/{hidden}/edit""#
        )),
        "reveal must offer the anchor on the revealed region: {html}"
    );
    assert!(
        html.contains(r#"<span class="acl-revealed">"#)
            || html.contains(r#"<div class="acl-revealed">"#),
        "the revealed region keeps its highlight: {html}"
    );
}
