// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the read-only page viewer at
//! `/dashboard/wiki/:id/view/*path` and its inline-comment interpolation.
//!
//! Drives the new route end-to-end against a populated DB + tempdir
//! wiki tree: seeds a wiki with one or more headings, seeds
//! `wiki_briefing_items` rows with `target_cite` shapes that exercise
//! every branch of the layout policy (inline match, orphaned anchor,
//! cite without anchor, processed filter), and asserts on the HTML
//! shape.
//!
//! The comment write path is exercised on the same fixtures: GET
//! `/wiki/:id/comment/*path[?anchor=...]` renders a form,
//! POST persists a `wiki_briefing_items` row with the correct
//! `source_kind` / `kind` / `author_sender_id` / `target_cite`
//! columns, round-trips into the inline render on the next
//! GET, and rejects anonymous / non-reader / empty / oversized /
//! malformed-anchor submissions.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::config::LlmConfig;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app_with_memory() -> (Router, SqlitePool, WikiTree, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(parking_lot::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: std::sync::Arc::new(parking_lot::RwLock::new(
            std::collections::HashMap::new(),
        )),
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, tree, dir)
}

/// Drop a wiki directory at `<workdir>/wikis/alice` with the given
/// page body. Mirrors the helper in `wiki_explorer.rs` but specialised
/// for our two-heading layout fixture so each test reads cleanly.
fn seed_alice_with_page(tree: &WikiTree, page: &str, body: &str) {
    let dir = tree.wikis_dir().join("alice");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: alice\n\
                wiki_type: wiki-user\n\
                parent_wiki_id: null\n\
                slug: alice\n\
                title: Alice\n\
                acl_default: 'user:alice'\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
    let abs = dir.join(page);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

/// Insert a single row in `wiki_briefing_items`. Returns the row id so
/// the test can assert against the canonical `bi_<id>` rendering.
#[allow(clippy::too_many_arguments)]
async fn seed_briefing_item(
    pool: &SqlitePool,
    wiki_id: &str,
    target_cite: Option<&str>,
    author_sender_id: Option<&str>,
    source_kind: &str,
    body: &str,
    processed_at: Option<&str>,
    ts: Option<&str>,
) -> i64 {
    let now = ts.map_or_else(|| chrono::Utc::now().to_rfc3339(), str::to_owned);
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_briefing_items
            (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, author_sender_id, processed_at)
         VALUES (?, ?, 'user:alice', 'seed topic', ?, NULL, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(wiki_id)
    .bind(source_kind)
    .bind(body)
    .bind(&now)
    .bind(target_cite)
    .bind(author_sender_id)
    .bind(processed_at)
    .fetch_one(pool)
    .await
    .expect("seed briefing row");
    row.0
}

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

/// Find the byte offset of `needle` in `html`, panicking with a
/// useful message when missing. Inline-comment assertions need to be
/// ordering-sensitive, so we look up positions and compare them.
fn must_find(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("expected to find {needle:?} in:\n{haystack}"))
}

/// Two-heading body used by most tests. The setup is intentionally
/// minimal: a plain section with `## Section A` then `## Boundary
/// tokens` so a comment anchored to `boundary-tokens` lands cleanly.
const TWO_HEADING_BODY: &str = "# Parser\n\
                                \n\
                                Intro prose.\n\
                                \n\
                                ## Section A\n\
                                \n\
                                A body.\n\
                                \n\
                                ## Boundary tokens\n\
                                \n\
                                B body.\n";

#[tokio::test]
async fn page_view_renders_inline_comment_below_matching_heading() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Comment anchored to the second heading.
    let id = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        Some("alice"),
        "dashboard_comment",
        "Stale: switched to the new tokenizer.",
        None,
        Some("2026-05-26T13:42:00Z"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    // The comment block is rendered with the canonical bi_<id> handle
    // and carries the author attribution + the body.
    let bi_id = format!("bi_{id}");
    assert!(html.contains(&bi_id), "must surface {bi_id}: {html}");
    assert!(
        html.contains("Comment by @alice"),
        "author attribution missing: {html}"
    );
    assert!(
        html.contains("Stale: switched to the new tokenizer."),
        "comment body missing: {html}"
    );
    // Cut to the second: nine digits of precision on a note somebody
    // left this morning is a column value, not a sentence.
    assert!(
        html.contains("2026-05-26 13:42:00"),
        "comment timestamp missing or not readable"
    );
    assert!(
        !html.contains("2026-05-26T13:42:00Z"),
        "the stored form is not what the reader is shown"
    );

    // Inline ordering: the comment block must sit AFTER the "Boundary
    // tokens" heading and BEFORE the literal text of the section body
    // that follows it ("B body.").
    let pos_boundary = must_find(&html, "id=\"boundary-tokens\"");
    let pos_comment = must_find(&html, &bi_id);
    let pos_section_a = must_find(&html, "id=\"section-a\"");
    let pos_b_body = must_find(&html, "B body.");
    assert!(
        pos_boundary < pos_comment,
        "comment must render after the matching heading; \
         boundary={pos_boundary} comment={pos_comment}"
    );
    assert!(
        pos_comment < pos_b_body,
        "comment must render before the body that follows the heading; \
         comment={pos_comment} body={pos_b_body}"
    );
    // And NOT below the previous heading (`Section A`) — that would
    // be an off-by-one in the layout.
    assert!(
        pos_section_a < pos_boundary,
        "section A must render before Boundary tokens"
    );

    // No orphaned section is rendered when every comment lands inline.
    assert!(
        !html.contains("Orphaned comments"),
        "must not render the orphaned section when nothing is orphaned: {html}"
    );
}

#[tokio::test]
async fn page_view_renders_orphaned_comments_in_footer_when_anchor_missing() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Comment pointing at an anchor that does not exist in the body.
    let id = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#removed-section"),
        Some("alice"),
        "dashboard_comment",
        "This used to live under #removed-section.",
        None,
        Some("2026-05-26T13:00:00Z"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    assert!(
        html.contains("Orphaned comments"),
        "orphaned header must render: {html}"
    );
    let bi_id = format!("bi_{id}");
    let pos_header = must_find(&html, "Orphaned comments");
    let pos_comment = must_find(&html, &bi_id);
    assert!(
        pos_header < pos_comment,
        "orphan comment must render after the orphaned header; \
         header={pos_header} comment={pos_comment}"
    );
    // The anchor that could not be resolved is surfaced in the meta
    // line so the operator can fix the cite.
    assert!(
        html.contains("removed-section"),
        "missing anchor must be surfaced in the orphan meta: {html}"
    );
    // And it stays orphaned: a heading the body no longer has is a
    // different claim from a remark about the page as a whole.
    assert!(
        !html.contains("On this page"),
        "a vanished heading is orphaned, never filed as a page-level remark: {html}"
    );
}

#[tokio::test]
async fn page_view_renders_a_cite_without_anchor_under_on_this_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Path-only cite: no `#anchor`. That is a remark about the page as
    // a whole — it opens the body under its own heading, and nothing
    // about it is orphaned.
    let id = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md"),
        Some("alice"),
        "dashboard_comment",
        "Page-level note.",
        None,
        Some("2026-05-26T12:00:00Z"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    let bi_id = format!("bi_{id}");
    assert!(
        !html.contains("Orphaned comments"),
        "a cite without an anchor is not orphaned: {html}"
    );
    let pos_header = must_find(&html, "On this page");
    let pos_comment = must_find(&html, &bi_id);
    let pos_body = must_find(&html, "id=\"boundary-tokens\"");
    assert!(
        pos_header < pos_comment,
        "the page-level comment must render under its own heading; \
         header={pos_header} comment={pos_comment}"
    );
    assert!(
        pos_comment < pos_body,
        "the page-level block sits above the page body; \
         comment={pos_comment} body={pos_body}"
    );
}

/// A page written without a single heading — an identity card is one,
/// by design. Before the page-level link there was nowhere at all to
/// comment on such a page.
const HEADING_LESS_CARD: &str = "Alice, born 1985, lives in Turin.\n\n\
                                 Works as a translator; keeps early hours.\n";

#[tokio::test]
async fn a_page_without_headings_offers_the_page_level_comment_link() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "@profile.md", HEADING_LESS_CARD);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/@profile.md?mode=comment")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    assert!(
        html.contains("+ Comment on this page"),
        "a heading-less page must still offer a way in: {html}"
    );
    assert!(
        html.contains("/dashboard/wiki/alice/comment/@profile.md\""),
        "the page-level link carries no ?anchor=: {html}"
    );

    // Read mode stays clean: the page-level link is part of comment
    // mode like every other comment affordance.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/@profile.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html_read_mode = body_string(response).await;
    assert!(
        !html_read_mode.contains("+ Comment on this page"),
        "read mode must not render the page-level affordance: {html_read_mode}"
    );
}

#[tokio::test]
async fn a_comment_posted_through_the_page_level_link_renders_under_on_this_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "@profile.md", HEADING_LESS_CARD);

    // POST with no `?anchor=` — the form the page-level link opens.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/@profile.md")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=She+moved+to+Genoa+last+month."))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());

    let cite: Option<String> = sqlx::query_scalar(
        "SELECT target_cite FROM wiki_briefing_items WHERE wiki_id = 'alice' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("briefing row");
    assert_eq!(
        cite.as_deref(),
        Some("wiki://alice/@profile.md"),
        "a page-level comment stores a cite with no heading fragment"
    );

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/@profile.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("She moved to Genoa last month."),
        "the comment must round-trip into the page view: {html}"
    );
    assert!(
        !html.contains("Orphaned comments"),
        "a comment left on the page is not an orphan: {html}"
    );
    let pos_header = must_find(&html, "On this page");
    let pos_comment = must_find(&html, "She moved to Genoa last month.");
    assert!(
        pos_header < pos_comment,
        "the comment renders under the page-level heading; \
         header={pos_header} comment={pos_comment}"
    );
}

#[tokio::test]
async fn the_page_level_comment_form_says_it_is_about_the_whole_page() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "@profile.md", HEADING_LESS_CARD);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/comment/@profile.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("the whole page"),
        "the form must say what the comment is about: {html}"
    );
    assert!(
        !html.contains("orphaned bucket"),
        "there is no missing heading to warn about: {html}"
    );
}

#[tokio::test]
async fn a_blank_anchor_parameter_is_still_refused() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Absent means "the whole page"; present-but-blank is a malformed
    // URL for a section, and answering the other question would file
    // the remark somewhere the writer did not choose.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=Anything."))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn page_view_filters_processed_comments_out() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let pending = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        Some("alice"),
        "dashboard_comment",
        "PENDING comment body.",
        None,
        Some("2026-05-26T13:00:00Z"),
    )
    .await;
    let processed = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        Some("alice"),
        "dashboard_comment",
        "PROCESSED comment body.",
        Some("2026-05-26T13:10:00Z"),
        Some("2026-05-26T13:05:00Z"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(&format!("bi_{pending}")),
        "pending comment must render: {html}"
    );
    assert!(
        !html.contains(&format!("bi_{processed}")),
        "processed comment must NOT render: {html}"
    );
    assert!(
        html.contains("PENDING comment body."),
        "pending body must render"
    );
    assert!(
        !html.contains("PROCESSED comment body."),
        "processed body must NOT render"
    );
}

#[tokio::test]
async fn page_view_renders_no_comment_section_when_none() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("Orphaned comments"),
        "Orphaned section must not appear when no comments exist: {html}"
    );
    assert!(
        !html.contains("comment-block"),
        "no inline comment blocks should render either: {html}"
    );
    // Page body itself is still rendered.
    assert!(html.contains("id=\"boundary-tokens\""), "{html}");
}

#[tokio::test]
async fn page_view_attributes_comment_to_author_sender_id() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);
    seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        Some("alice"),
        "dashboard_comment",
        "Body 1.",
        None,
        Some("2026-05-26T13:00:00Z"),
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("@alice"),
        "author_sender_id must appear in the rendered HTML: {html}"
    );
    assert!(
        html.contains("Comment by @alice"),
        "attribution shape must read 'Comment by @<author>': {html}"
    );
}

/// REM-emitted items (no human author) still need a reasonable
/// attribution. The handler renders "From REM" in that case so the
/// operator does not stare at a comment without a source.
#[tokio::test]
async fn page_view_attributes_rem_comment_without_author_as_from_rem() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);
    seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        None,
        "rem",
        "REM-generated suggestion.",
        None,
        Some("2026-05-26T13:00:00Z"),
    )
    .await;
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("From REM"),
        "REM-emitted comments must surface 'From REM': {html}"
    );
    assert!(html.contains("REM-generated suggestion."), "{html}");
}

// =====================================================================
// Comment write path tests.
// =====================================================================

/// Drop a second wiki at `<workdir>/wikis/bob` belonging to bob (used
/// for the cross-user 403 test) with one heading the comment write
/// path will try to address.
fn seed_bob_with_page(tree: &WikiTree, page: &str, body: &str) {
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
    let abs = dir.join(page);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

#[tokio::test]
async fn comment_submission_inserts_briefing_row_with_correct_columns() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=Stale%3A+switched+to+the+new+tokenizer."))
            .unwrap(),
    )
    .await;
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    assert!(
        status.is_redirection(),
        "expected 302 redirect, got {status}"
    );
    assert_eq!(
        location.as_deref(),
        Some("/dashboard/wiki/alice/view/modules/parser.md"),
    );

    // Verify the row landed in the DB with the expected columns.
    let row: (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT source_kind, source_ref, kind, target_cite, author_sender_id, body, topic
           FROM wiki_briefing_items
          WHERE wiki_id = 'alice'
          ORDER BY id DESC
          LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("briefing row should exist after comment POST");
    assert_eq!(row.0, "dashboard_comment", "source_kind");
    assert_eq!(row.1, "dashboard:alice", "source_ref");
    assert_eq!(row.2.as_deref(), Some("external"), "kind");
    assert_eq!(
        row.3.as_deref(),
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        "target_cite"
    );
    assert_eq!(row.4.as_deref(), Some("alice"), "author_sender_id");
    assert_eq!(row.5, "Stale: switched to the new tokenizer.", "body");
    assert!(!row.6.is_empty(), "topic must not be blank");
    assert!(
        row.6.len() <= 200,
        "topic must respect the 200-byte schema cap (got {} bytes: {:?})",
        row.6.len(),
        row.6,
    );

    // `processed_at` starts NULL so the page view renders it inline.
    let processed: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE wiki_id = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("row");
    assert!(
        processed.is_none(),
        "processed_at must be NULL on insert; got {processed:?}"
    );
}

#[tokio::test]
async fn comment_round_trip_to_inline_render() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Step 1 — POST a fresh comment.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "body=Round-trip+from+the+write+path+into+the+inline+render.",
            ))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());

    // Step 2 — GET the read view (no `?mode=comment`) and assert the
    // comment HTML appears inline after the matching heading.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Round-trip from the write path into the inline render."),
        "body must round-trip into the inline render: {html}"
    );
    let pos_boundary = must_find(&html, "id=\"boundary-tokens\"");
    let pos_comment = must_find(&html, "Round-trip from the write path");
    let pos_b_body = must_find(&html, "B body.");
    assert!(
        pos_boundary < pos_comment,
        "comment must render after the heading; \
         boundary={pos_boundary} comment={pos_comment}"
    );
    assert!(
        pos_comment < pos_b_body,
        "comment must render before the body line that follows; \
         comment={pos_comment} body={pos_b_body}"
    );
}

#[tokio::test]
async fn comment_anonymous_redirects_to_login() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=Hello+from+nobody."))
            .unwrap(),
    )
    .await;
    // The session middleware short-circuits the unauthenticated branch
    // with a 303 / 302 redirect to /dashboard/login (the public surface).
    assert!(
        response.status().is_redirection(),
        "anonymous POST must redirect; got {}",
        response.status()
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        location.contains("/dashboard/login"),
        "Location must point at the login page; got {location:?}"
    );
}

/// A comment on a standard wiki is refused to somebody who can read nothing in
/// it — the wiki holds one fact and it is Bob's, so Alice, admin though she is,
/// has nothing there to comment on.
#[tokio::test]
async fn comment_no_read_access_returns_403() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // Alice (admin, owns "alice")
    seed_bob_with_page(&tree, "modules/private.md", TWO_HEADING_BODY);
    seed_fact_about(&pool, &tree, "bob", "modules/private.md", "user:bob", "03").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/bob/comment/modules/private.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=Sneaky+cross-user+comment."))
            .unwrap(),
    )
    .await;
    let status = response.status();
    let html = body_string(response).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-user comment must be refused; got {status} body={html}",
    );
    // Alice is admin yet denied: this gate has no admin bypass, so the 403
    // must carry the content-ACL copy and never the "Admin rights required."
    // admin-gate copy (which would wrongly imply that becoming, or being,
    // admin unlocks another user's private wiki).
    assert!(
        html.contains("don't have access"),
        "403 must carry the content-ACL copy, got: {html}"
    );
    assert!(
        !html.contains("Admin rights required"),
        "content ACL 403 must not tell an admin that admin rights would help: {html}"
    );
}

/// Reveal is a read lens, not a grant. An admin with reveal on sees another
/// user's page whole, and is still shown the "can't comment" notice instead of
/// a dead "Add comments" / "+ Comment" link — even in `?mode=comment`, because
/// the comment gate never consults reveal.
#[tokio::test]
async fn comment_affordance_hidden_when_only_reveal_opened_the_page() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // alice (admin), not bob
    seed_bob_with_page(&tree, "modules/private.md", TWO_HEADING_BODY);
    seed_fact_about(&pool, &tree, "bob", "modules/private.md", "user:bob", "04").await;

    let response = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/wiki/bob/view/modules/private.md?mode=comment")
            .header(header::COOKIE, format!("{cookie}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("You can't comment on this page"),
        "must show the can't-comment notice: {html}"
    );
    assert!(
        !html.contains("Add comments") && !html.contains("+ Comment on"),
        "must NOT offer the comment affordance when the viewer can't comment"
    );
}

#[tokio::test]
async fn comment_empty_body_returns_422() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=%20%20%20%20")) // whitespace only — trims to empty
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(
        html.contains("must not be empty"),
        "empty-body flash must mention emptiness: {html}"
    );

    // No row was inserted.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 0, "no row must be inserted on empty body");
}

#[tokio::test]
async fn comment_body_too_long_returns_422() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // 4097 ASCII bytes — one over the COMMENT_BODY_MAX_BYTES ceiling.
    let big = "a".repeat(4097);
    let body = format!("body={big}");
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(
        html.contains("must not exceed"),
        "oversize body flash must mention the cap: {html}"
    );

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 0, "no row must be inserted on oversize body");
}

#[tokio::test]
async fn comment_malformed_anchor_returns_422() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // Uppercase chars + spaces are forbidden — must be 422.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=Boundary%20Tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=valid+body"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id = 'alice'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 0, "no row must be inserted on malformed anchor");
}

#[tokio::test]
async fn comment_mode_query_param_shows_add_comment_buttons() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // With `?mode=comment` → "+ Comment" buttons appear next to every
    // heading and the "Stop commenting" toggle replaces the "Add
    // comments" link.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md?mode=comment")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html_comment_mode = body_string(response).await;
    assert!(
        html_comment_mode.contains("+ Comment on"),
        "comment-mode view must surface the per-heading affordance: {html_comment_mode}"
    );
    assert!(
        html_comment_mode.contains("Stop commenting"),
        "comment-mode view must surface the toggle to leave comment mode: {html_comment_mode}"
    );
    // The form links target the comment route on each anchor we know
    // about in the fixture body.
    assert!(
        html_comment_mode
            .contains("/dashboard/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens"),
        "must include a comment link addressing the Boundary tokens slug: {html_comment_mode}"
    );

    // Without the query param the read mode is clean — no "+ Comment"
    // buttons, but the "Add comments" toggle is visible.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html_read_mode = body_string(response).await;
    assert!(
        !html_read_mode.contains("+ Comment on"),
        "read mode must NOT render the per-heading + Comment affordance: {html_read_mode}"
    );
    assert!(
        html_read_mode.contains("Add comments"),
        "read mode must surface the toggle to enter comment mode: {html_read_mode}"
    );
    assert!(
        !html_read_mode.contains("Stop commenting"),
        "read mode must NOT surface the leave-comment-mode toggle: {html_read_mode}"
    );
}

/// End-to-end closing-the-loop scenario:
/// (a) the operator drops a comment on a page through the dashboard
///     write path, seeding a pending `wiki_briefing_items`
///     row whose `target_cite` lands on the matching heading;
/// (b) the page view renders the comment inline because
///     `processed_at IS NULL`;
/// (c) a `wiki_admin::push` carrying `mark_processed=[bi_<N>]`
///     flips `processed_at` atomically with the page write — this is
///     the write the nightly comment pass makes once it has acted on the
///     comment;
/// (d) the next page view no longer shows the comment because the
///     SQL filter on `processed_at IS NULL` filters it out.
#[tokio::test]
async fn comment_round_trip_then_mark_processed_removes_it_from_inline_view() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    // (a) Dashboard write path lands the comment row.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "body=Stale%3A+update+to+the+new+tokenizer+per+the+async-runtime+decision.",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "comment POST must redirect: {}",
        response.status()
    );
    let bi: i64 = sqlx::query_scalar("SELECT id FROM wiki_briefing_items WHERE wiki_id = 'alice'")
        .fetch_one(&pool)
        .await
        .expect("briefing row");

    // (b) Inline render: the comment shows up under the matching heading.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(&format!("bi_{bi}")),
        "pending comment must render before mark_processed: {html}"
    );
    assert!(
        html.contains("Stale: update to the new tokenizer"),
        "comment body must render: {html}"
    );

    // (c) The item is marked processed. On a person's own wiki that is the
    //     nightly comment pass's own write (`comment_apply`), which is the
    //     road a comment on a standard wiki actually travels; the smart
    //     consumer's road, `wiki_admin_push` carrying `mark_processed`, is
    //     covered where it is reachable, on a smart wiki.
    sqlx::query("UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ?")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(bi)
        .execute(&pool)
        .await
        .expect("mark the comment processed");

    // (d) Next page view filters the (now processed) comment out.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/modules/parser.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html_after = body_string(response).await;
    assert!(
        !html_after.contains(&format!("bi_{bi}")),
        "processed comment must NOT render after mark_processed: {html_after}"
    );
    assert!(
        !html_after.contains("Stale: update to the new tokenizer"),
        "comment body must NOT render after mark_processed: {html_after}"
    );
}

#[tokio::test]
async fn comment_form_get_renders_context() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The page surfaces the heading text + the slug + the wiki / page
    // identifiers so the operator knows exactly what they are
    // commenting on.
    assert!(html.contains("Boundary tokens"), "heading text: {html}");
    assert!(html.contains("boundary-tokens"), "slug: {html}");
    assert!(html.contains("modules/parser.md"), "page path: {html}");
    assert!(html.contains("alice"), "wiki id: {html}");
    // The form posts to the matching POST endpoint and asks for a body.
    assert!(
        html.contains("name=\"body\""),
        "form must include the body textarea: {html}"
    );
    assert!(
        html.contains(
            "action=\"/dashboard/wiki/alice/comment/modules/parser.md?anchor=boundary-tokens\""
        ),
        "form action must point at the matching POST endpoint: {html}"
    );
}

/// The mark-as-read endpoint refuses a comment on a NARRATIVE
/// wiki: those are applied by the REM dream as fact ops, never mark-passive
/// drained out from under it. The row must stay pending after the 400.
#[tokio::test]
async fn submit_process_refuses_standard_wiki_comment() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "modules/parser.md", TWO_HEADING_BODY);

    let bi_id = seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/modules/parser.md#boundary-tokens"),
        Some("alice"),
        "dashboard_comment",
        "the date is wrong",
        None,
        None,
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/alice/briefing-items/bi_{bi_id}/process"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "standard-wiki comment Submit must be refused with 400"
    );

    // The row stays pending — the dream still has it to apply.
    let processed: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
            .bind(bi_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        processed.is_none(),
        "refused Submit must leave the standard-wiki comment pending; got {processed:?}"
    );
}

/// The page view renders the body but never the **testata** (the
/// frontmatter card): its owner-tier `keywords`/`description` must not leak
/// into the dashboard, exactly as `wiki_read` strips it for a consumer
/// (the card boundary, dashboard half).
#[tokio::test]
async fn page_view_strips_the_testata_so_card_topics_never_leak() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    // A page whose testata carries owner-tier card topics + a description.
    let page = "---\n\
                title: Health\n\
                keywords:\n  topics: celiachia, gravidanza\n\
                description: Sensitive health notes.\n\
                ---\n\
                \n\
                # Health notes\n\
                \n\
                Visible body prose.\n";
    seed_alice_with_page(&tree, "health.md", page);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/health.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    // The body renders...
    assert!(
        html.contains("Visible body prose."),
        "body must render: {html}"
    );
    // ...but the frontmatter card never leaks into the page.
    assert!(!html.contains("celiachia"), "card topic leaked: {html}");
    assert!(!html.contains("gravidanza"), "card topic leaked: {html}");
    assert!(
        !html.contains("Sensitive health notes."),
        "card description leaked: {html}"
    );
}

// ---------- admin ACL-reveal toggle ----------
//
// The dashboard-only operator override (CookieJar `mwe_admin_reveal` +
// server-side admin gate) renders memory-wiki pages through
// `render::render_admin_reveal_segments`, showing — highlighted — fragments the
// viewer could not read. The MCP tool surface is unaffected (it always
// honours the ACL); these tests lock the dashboard wiring end-to-end.

/// A page Alice (the admin/sender) cannot fully read: a block region
/// owned by `user:bob` with no `allow`, anchored by a heading + prose so
/// the non-reveal view shows `[redacted]` rather than the total-redaction
/// callout. The `f=` is a valid `UUIDv7`; no `fact_index` row is seeded so
/// the inline marker attributes gate it.
const BOB_OWNED_PAGE: &str = "# Secret page\n\
                              \n\
                              Intro prose.\n\
                              \n\
                              {{subject=user:bob f=018f1234-5678-7abc-9def-000000000001}}\n\
                              TOPSECRET body line\n\
                              {{/}}\n";

#[tokio::test]
async fn page_view_redacts_for_admin_without_the_reveal_cookie() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "secret.md", BOB_OWNED_PAGE);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/secret.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    assert!(html.contains("[redacted]"), "expected redaction: {html}");
    assert!(
        !html.contains("TOPSECRET body line"),
        "private body leaked without reveal: {html}"
    );
    assert!(
        !html.contains("acl-revealed"),
        "no highlight expected without reveal: {html}"
    );
    assert!(
        !html.contains("ACL bypass active"),
        "no banner expected without reveal: {html}"
    );
}

#[tokio::test]
async fn page_view_reveals_for_admin_with_the_reveal_cookie() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "secret.md", BOB_OWNED_PAGE);

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/secret.md")
            .header(header::COOKIE, format!("{session}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    assert!(
        html.contains("TOPSECRET body line"),
        "revealed body missing: {html}"
    );
    assert!(
        html.contains(r#"<div class="acl-revealed">"#),
        "revealed fragment must be highlighted: {html}"
    );
    assert!(
        html.contains("ACL bypass active"),
        "reveal banner missing: {html}"
    );
    assert!(
        !html.contains("[redacted]"),
        "nothing should be redacted in reveal mode: {html}"
    );
}

/// Retirement hygiene on the dashboard render: a **retired** region whose
/// bytes still sit on disk redacts fail-closed in the normal view (the
/// per-user render uses the ACTIVE ACL map — the row is dropped, the bare
/// marker has no inline attributes, nobody reads it, not even the fact's
/// own subject), while the admin reveal keeps the FULL map so the operator
/// still sees the residue — un-highlighted, because by its last-known ACL
/// the viewer could read it.
#[tokio::test]
async fn page_view_redacts_a_retired_region_but_reveal_still_shows_it() {
    use mwe_core::fact_index::{self, NewFact};
    use mwe_core::types::FactId;

    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app).await;

    // A bare runtime marker (`{{f=…}}`, no inline attributes): its gate
    // lives in the DB row alone.
    let fact_id = FactId::parse("018f1234-5678-7abc-9def-000000000042").unwrap();
    let page = format!(
        "# Recap\n\nIntro prose.\n\n{{{{f={}}}}}\nRETIREDBODY line\n{{{{/}}}}\n",
        fact_id.as_str()
    );
    seed_alice_with_page(&tree, "retired.md", &page);
    // The row is owned by the viewer herself — with the FULL map she would
    // read it; the active map must drop it regardless.
    fact_index::insert(
        &pool,
        &NewFact {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/retired.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "RETIREDBODY line".to_owned(),
            embedding: vec![0.0; 8],
            subject_id: "user:alice".parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: Some("user:alice".parse().unwrap()),
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        },
    )
    .await
    .expect("seed fact");
    let successor = FactId::parse("018f1234-5678-7abc-9def-000000000043").unwrap();
    fact_index::mark_superseded(&pool, &fact_id, &successor, chrono::Utc::now())
        .await
        .expect("retire the fact");

    // Normal view: fail-closed — the superseded region is redacted even
    // for its own subject.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/retired.md")
            .header(header::COOKIE, &session)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("RETIREDBODY line"),
        "retired region must not render to its last-known audience: {html}"
    );
    assert!(html.contains("[redacted]"), "expected redaction: {html}");

    // Admin reveal: the FULL map keeps the retired row, so the residue is
    // visible to the supervision lens — and NOT highlighted (the viewer
    // could read it by its last-known ACL).
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/retired.md")
            .header(header::COOKIE, format!("{session}; mwe_admin_reveal=1"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("RETIREDBODY line"),
        "reveal must still show the retired residue: {html}"
    );
    assert!(!html.contains("[redacted]"), "reveal never redacts: {html}");
    assert!(
        !html.contains("acl-revealed"),
        "the full map gates by the last-known ACL, so the subject's own \
         region is not highlighted: {html}"
    );
}

#[tokio::test]
async fn toggle_reveal_sets_then_clears_the_cookie() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let session = login_as_admin(&app).await;

    // Turn it on.
    let on = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/reveal")
            .header(header::COOKIE, session.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("on=1&return_to=/dashboard/wiki/alice"))
            .unwrap(),
    )
    .await;
    assert!(on.status().is_redirection(), "{}", on.status());
    let set = extract_set_cookie(&on, "mwe_admin_reveal").expect("reveal cookie set");
    assert!(
        set.contains("mwe_admin_reveal=1"),
        "cookie not set on: {set}"
    );

    // Turn it off (checkbox unticked → `on` absent).
    let off = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/reveal")
            .header(header::COOKIE, session)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("return_to=/dashboard/wiki/alice"))
            .unwrap(),
    )
    .await;
    assert!(off.status().is_redirection(), "{}", off.status());
    let cleared = extract_set_cookie(&off, "mwe_admin_reveal").expect("reveal cookie cleared");
    // Cleared cookie carries an empty value + an immediate expiry.
    assert!(
        cleared.contains("mwe_admin_reveal=;") || cleared.contains("mwe_admin_reveal= "),
        "cookie not cleared: {cleared}"
    );
}

// ---------- "Mark as read" on a pending comment ----------

/// Drop a **smart** wiki (`casa`, child of `alice`) with one page.
fn seed_smart_casa_with_page(tree: &WikiTree, page: &str, body: &str) {
    let dir = tree.wikis_dir().join("casa");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: casa\n\
                wiki_type: wiki-tech\n\
                parent_wiki_id: alice\n\
                slug: casa\n\
                title: Casa\n\
                acl_default: 'user:alice'\n\
                smart: true\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
    std::fs::write(dir.join(page), body).unwrap();
}

/// The route that clears a comment had no button anywhere: it was
/// reachable only by crafting the request. On a smart wiki — the only
/// family it accepts — every pending comment now carries one.
#[tokio::test]
async fn a_smart_wiki_comment_offers_the_control_that_clears_it() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "index.md", "# Alice\n");
    seed_smart_casa_with_page(
        &tree,
        "impianti.md",
        "# Impianti\n\n## Boiler\n\nServiced.\n",
    );
    let bi_id = seed_briefing_item(
        &pool,
        "casa",
        Some("wiki://casa/impianti.md#boiler"),
        Some("alice"),
        "user",
        "The service is due in October.",
        None,
        None,
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/casa/view/impianti.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(&format!(
            "/dashboard/wiki/casa/briefing-items/bi_{bi_id}/process"
        )),
        "the comment must carry the form that clears it: {html}"
    );
    assert!(html.contains("Mark as read"), "{html}");
}

/// A comment on a standard wiki is read by the nightly cycle, which
/// changes the facts from it. `POST …/process` refuses one with a 400,
/// so the page must not offer the button that would ask for it.
#[tokio::test]
async fn a_standard_wiki_comment_offers_no_control_the_route_would_refuse() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "cucina.md", "# Cucina\n\n## Colazione\n\nCoffee.\n");
    seed_briefing_item(
        &pool,
        "alice",
        Some("wiki://alice/cucina.md#colazione"),
        Some("alice"),
        "user",
        "Bob takes it without sugar.",
        None,
        None,
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/view/cucina.md")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("briefing-items/"),
        "a standard-wiki comment must not offer a control the route refuses: {html}"
    );
    assert!(!html.contains("Mark as read"), "{html}");
}

/// Clearing a comment lands the reader back on the page they were
/// reading, not on the wiki index.
#[tokio::test]
async fn clearing_a_comment_returns_to_the_page_it_was_on() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "index.md", "# Alice\n");
    seed_smart_casa_with_page(
        &tree,
        "impianti.md",
        "# Impianti\n\n## Boiler\n\nServiced.\n",
    );
    let bi_id = seed_briefing_item(
        &pool,
        "casa",
        Some("wiki://casa/impianti.md#boiler"),
        Some("alice"),
        "user",
        "The service is due in October.",
        None,
        None,
    )
    .await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/casa/briefing-items/bi_{bi_id}/process"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/wiki/casa/view/impianti.md"),
        "must land back on the page the comment was anchored to"
    );

    let processed: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
            .bind(bi_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(processed.is_some(), "the row must be drained");
}

/// A comment means two different things depending on the wiki it is
/// left on, and the page that takes it says which. Getting this wrong
/// tells a person their note is going to a consumer that will never see
/// it — or that a consumer will read what the nightly pass will.
#[tokio::test]
async fn the_comment_form_says_who_will_read_it() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "cucina.md", "# Cucina\n\n## Colazione\n\nCoffee.\n");
    seed_smart_casa_with_page(
        &tree,
        "impianti.md",
        "# Impianti\n\n## Boiler\n\nServiced.\n",
    );

    for (uri, expected, refused) in [
        (
            "/wiki/alice/comment/cucina.md?anchor=colazione",
            "The nightly pass reads it and changes the facts you point at",
            "consumer that writes this wiki",
        ),
        (
            "/wiki/casa/comment/impianti.md?anchor=boiler",
            "The consumer that writes this wiki finds it",
            "nightly pass",
        ),
    ] {
        let response = send(
            &app,
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let html = body_string(response).await;
        assert!(html.contains(expected), "{uri} does not say `{expected}`");
        assert!(
            !html.contains(refused),
            "{uri} also claims `{refused}`, which is the other family's promise"
        );
    }
}

// ---------- a topic wiki, which belongs to nobody ----------

/// Drop the shape the nightly grouping raises: a `wiki-tech` wiki with no
/// parent, named for its subject, so no principal answers for it.
fn seed_giardinaggio_with_page(tree: &WikiTree, page: &str, body: &str) {
    let dir = tree.wikis_dir().join("giardinaggio");
    std::fs::create_dir_all(&dir).unwrap();
    let meta = "---\n\
                wiki_id: giardinaggio\n\
                wiki_type: wiki-tech\n\
                parent_wiki_id: null\n\
                slug: giardinaggio\n\
                title: Giardinaggio\n\
                ---\n";
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
    std::fs::write(dir.join(page), body).unwrap();
}

/// A wiki's home is a reader's view of it, not its shape.
///
/// The door stays open — a standard wiki is structure and hides from nobody —
/// but what is listed behind it answers per fact. A page name says what is on
/// the page before anybody opens it, and a count of everything says how much
/// there is that you are not being shown.
#[tokio::test]
async fn a_wiki_home_lists_the_pages_its_reader_reads_and_counts_their_facts() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;
    let bob_cookie = login_as_user(&app, &admin_cookie, "bob").await;
    seed_alice_with_page(&tree, "shared.md", "# Shared\n\nprose\n");
    seed_alice_with_page(&tree, "hers.md", "# Hers\n\nprose\n");
    // One fact bob is in the audience of, one he is not.
    seed_fact_about(&pool, &tree, "alice", "shared.md", "user:alice", "a1").await;
    sqlx::query("UPDATE fact_index SET allow_ids = '[\"user:bob\"]' WHERE source_path = ?")
        .bind("wikis/alice/shared.md")
        .execute(&pool)
        .await
        .expect("share the first one with bob");
    seed_fact_about(&pool, &tree, "alice", "hers.md", "user:alice", "a2").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice")
            .header(header::COOKIE, bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "the door is open");
    let html = body_string(response).await;
    assert!(
        html.contains("shared.md"),
        "a page bob reads a fact of is listed: {html}"
    );
    assert!(
        !html.contains("hers.md"),
        "a page whose only fact is alice's is not his to be offered: {html}"
    );
    assert!(
        html.contains("1 that still hold"),
        "the count is the facts bob may read, not the wiki's total: {html}"
    );
}

/// A page nobody's facts are on still has a name its reader may know.
///
/// The listing asks «may this reader be told this page exists», and the
/// product answers that in one place. A page carrying no active fact keeps
/// nothing from anybody — an `@rules.md` of plain prose is exactly that — so
/// it is listed. What is withheld is a page whose facts are all out of reach.
#[tokio::test]
async fn a_wiki_home_lists_a_page_that_holds_no_fact_at_all() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;
    let bob_cookie = login_as_user(&app, &admin_cookie, "bob").await;
    seed_alice_with_page(&tree, "shared.md", "# Shared\n\nprose\n");
    seed_alice_with_page(&tree, "hers.md", "# Hers\n\nprose\n");
    seed_fact_about(&pool, &tree, "alice", "shared.md", "user:alice", "b1").await;
    sqlx::query("UPDATE fact_index SET allow_ids = '[\"user:bob\"]' WHERE source_path = ?")
        .bind("wikis/alice/shared.md")
        .execute(&pool)
        .await
        .expect("share the first one with bob");
    seed_fact_about(&pool, &tree, "alice", "hers.md", "user:alice", "b2").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice")
            .header(header::COOKIE, bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("shared.md"),
        "a page he reads a fact of: {html}"
    );
    assert!(
        !html.contains("hers.md"),
        "a page whose only fact is alice's stays out: {html}"
    );
    assert!(
        html.contains("@rules.md"),
        "a page of plain prose withholds nothing and is listed: {html}"
    );
}

/// A smart wiki holds no facts at all, so there is nothing per page to ask:
/// its gate is the roster, and past it the whole shelf shows.
///
/// Asking the per-page question there would have shown its own owner an empty
/// wiki — every page gone and no facts — which is what a per-fact rule does to
/// a family that has no facts by construction.
#[tokio::test]
async fn a_smart_wikis_owner_sees_its_pages_although_it_holds_no_facts() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;
    seed_alice_with_page(&tree, "appunti.md", "# Appunti\n\nprose\n");
    let caller = mwe_core::wiki_admin::AdminCaller {
        sender_id: "alice".into(),
        consumer_id: Some("cc-alice".into()),
        consumer_class: mwe_core::jwt::ConsumerClass::Smart,
    };
    let created = mwe_core::wiki_admin::push(
        &pool,
        &tree,
        &caller,
        mwe_core::wiki_admin::PushRequest {
            mode: mwe_core::wiki_admin::PushMode::Create,
            wiki_id: None,
            parent_wiki_id: Some(mwe_core::types::WikiId::parse("alice").unwrap()),
            slug: Some("progetto".into()),
            title: Some("Progetto".into()),
            wiki_type: Some("wiki-companion".into()),
            smart: true,
            project_id: None,
            description: None,
            pages: vec![mwe_core::wiki_admin::PushPage {
                path: "decisioni.md".into(),
                content: "# Decisioni\n\nwhat we settled\n".into(),
            }],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        },
    )
    .await
    .expect("forge the smart wiki");

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/wiki/{}", created.wiki_id.as_str()))
            .header(header::COOKIE, admin_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("decisioni.md"),
        "its owner must see the pages of their own smart wiki: {html}"
    );
}

/// A nested wiki's pages live under its person's directory while its id is
/// `<user>-<slug>`, so a source path glued together from the id names a file
/// that is not there — and every per-page question about it answers about
/// nothing. The handle builds the path.
#[tokio::test]
async fn a_nested_wikis_listing_and_count_agree() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;
    let bob_cookie = login_as_user(&app, &admin_cookie, "bob").await;
    seed_alice_with_page(&tree, "appunti.md", "# Appunti\n\nprose\n");
    // A standard wiki nested under alice: `wikis/alice/ricette/`, id
    // `alice-ricette`. Its facts are bob's to read.
    let dir = tree.wikis_dir().join("alice").join("ricette");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        "---\nwiki_id: alice-ricette\nwiki_type: wiki-topic\nparent_wiki_id: alice\n\
         slug: ricette\ntitle: Ricette\nacl_default: 'global'\n---\n",
    )
    .unwrap();
    std::fs::write(dir.join("dolci.md"), "# Dolci\n\nprose\n").unwrap();
    seed_fact_about(&pool, &tree, "alice-ricette", "dolci.md", "user:bob", "b3").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice-ricette")
            .header(header::COOKIE, bob_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("dolci.md"),
        "a page of a NESTED wiki whose fact is his: {html}"
    );
    assert!(
        html.contains("1 that still hold"),
        "and the count agrees with the list: {html}"
    );
}

/// One indexed fact about `subject`, so the wiki's derived visibility has
/// something to answer with.
///
/// The source path comes from the wiki's own handle, never from its id: a
/// nested wiki lives under its person's directory (`wikis/<user>/<slug>/`)
/// while its id is `<user>-<slug>`, so a path glued together from the id
/// names a file that is not there — and every per-page question then answers
/// about nothing, which is a test passing without testing.
async fn seed_fact_about(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    page: &str,
    subject: &str,
    tail: &str,
) {
    use mwe_core::fact_index::{self, NewFact};
    use mwe_core::types::{FactId, WikiId};

    let handle = tree
        .locate(&WikiId::parse(wiki_id).expect("wiki id"))
        .expect("the wiki must be on disk before a fact is filed on one of its pages");

    fact_index::insert(
        pool,
        &NewFact {
            subject_external: None,
            slot: None,
            slot_value: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&format!("018f1234-5678-7abc-9def-0000000005{tail}")).unwrap(),
            wiki_id: wiki_id.to_owned(),
            source_path: handle.source_path(std::path::Path::new(page)),
            region_start: None,
            region_end: None,
            text: "The roses are pruned in February.".to_owned(),
            embedding: vec![0.0; 8],
            subject_id: subject.parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: Some(subject.parse().unwrap()),
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            salience: None,
            source_ref: None,
        },
    )
    .await
    .expect("seed fact");
}

/// Sign a fresh non-admin in, through the invite the admin issues.
async fn login_as_user(app: &Router, admin_cookie: &str, user_id: &str) -> String {
    let create = send(
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
    assert_eq!(create.status(), StatusCode::OK);
    let html = body_string(create).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '\'' | ')' | ','))
        .unwrap();
    let invitation_id = &after[..end];
    let accept = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "password={user_id}-pw-secret-12&password_confirm={user_id}-pw-secret-12"
            )))
            .unwrap(),
    )
    .await;
    extract_cookie_value(&extract_set_cookie(&accept, "mwe_session").expect("cookie"))
}

/// A wiki nobody owns is read the way every standard wiki is read: per fact.
/// The reader of one of its facts opens the page and is offered the comment
/// affordance; a stranger does not find it at all.
///
/// Asking who owns the wiki instead is the other behaviour, and it cannot be
/// answered here: nobody does. That question was a server error on every page
/// view of every wiki the nightly grouping had raised.
#[tokio::test]
async fn a_page_of_a_wiki_nobody_owns_opens_for_the_reader_of_one_of_its_facts() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;
    let carol = login_as_user(&app, &admin, "carol").await;

    seed_giardinaggio_with_page(&tree, "rose.md", TWO_HEADING_BODY);
    seed_fact_about(&pool, &tree, "giardinaggio", "rose.md", "user:bob", "01").await;

    let seen = send(
        &app,
        Request::builder()
            .uri("/wiki/giardinaggio/view/rose.md")
            .header(header::COOKIE, &bob)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        seen.status(),
        StatusCode::OK,
        "the reader of a fact in this wiki opens its page"
    );
    let html = body_string(seen).await;
    assert!(
        html.contains("Add comments"),
        "a reader who may read here may comment here: {html}"
    );

    let hidden = send(
        &app,
        Request::builder()
            .uri("/wiki/giardinaggio/view/rose.md")
            .header(header::COOKIE, &carol)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        hidden.status(),
        StatusCode::NOT_FOUND,
        "a reader who can read nothing in this wiki does not find it"
    );
}

/// **You comment where you read, and «where» is the page.**
///
/// Reading one page of a wiki is not reading another. Bob reads a fact on one
/// page of this wiki and nothing on the next, and the night turns a comment
/// into changes to the facts of the page it names — so the affordance is on
/// the first page and not on the second, and the endpoint refuses him there
/// too, so the two agree.
#[tokio::test]
async fn commenting_is_offered_on_the_page_you_read_and_refused_on_the_one_you_do_not() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;

    // Two pages in one wiki: one fact is Bob's, the other is not his to see.
    seed_giardinaggio_with_page(&tree, "rose.md", TWO_HEADING_BODY);
    seed_giardinaggio_with_page(&tree, "conti.md", TWO_HEADING_BODY);
    seed_fact_about(&pool, &tree, "giardinaggio", "rose.md", "user:bob", "03").await;
    seed_fact_about(&pool, &tree, "giardinaggio", "conti.md", "user:carol", "04").await;

    let his = send(
        &app,
        Request::builder()
            .uri("/wiki/giardinaggio/view/rose.md")
            .header(header::COOKIE, &bob)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(his.status(), StatusCode::OK);
    assert!(
        body_string(his).await.contains("Add comments"),
        "he may comment on the page he reads"
    );

    let not_his = send(
        &app,
        Request::builder()
            .uri("/wiki/giardinaggio/view/conti.md")
            .header(header::COOKIE, &bob)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(not_his).await;
    assert!(
        !html.contains("Add comments"),
        "and not on the page he reads nothing of: {html}"
    );

    // And the endpoint says the same thing, so the two cannot drift.
    let refused = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/giardinaggio/comment/conti.md")
            .header(header::COOKIE, &bob)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("body=change+this"))
            .unwrap(),
    )
    .await;
    assert!(
        refused.status().is_client_error(),
        "the endpoint refuses what the page did not offer, got {}",
        refused.status()
    );
}

/// **A page's text is not editable from anywhere.**
///
/// The route that handed somebody a page's raw body is gone, GET and POST:
/// a page shows each fact only to the people that fact is for, and a box
/// holding the raw text hands whoever opens it every fact on the page,
/// whoever it is about. The admin is not an exception — being able to run
/// the panel is not being everybody's audience.
#[tokio::test]
async fn a_pages_text_cannot_be_opened_for_editing_by_anybody() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await;
    let bob = login_as_user(&app, &admin, "bob").await;

    seed_giardinaggio_with_page(&tree, "rose.md", TWO_HEADING_BODY);
    seed_fact_about(&pool, &tree, "giardinaggio", "rose.md", "user:bob", "02").await;

    for (who, cookie) in [("the admin", &admin), ("a reader", &bob)] {
        let form = send(
            &app,
            Request::builder()
                .uri("/wiki/giardinaggio/edit/rose.md")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            form.status(),
            StatusCode::NOT_FOUND,
            "{who} finds no editor: there is no such route"
        );

        let saved = send(
            &app,
            Request::builder()
                .method("POST")
                .uri("/wiki/giardinaggio/edit/rose.md")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("body=%23%20Rose%0A"))
                .unwrap(),
        )
        .await;
        assert_eq!(
            saved.status(),
            StatusCode::NOT_FOUND,
            "and nothing to post to either, for {who}"
        );
    }

    let on_disk =
        std::fs::read_to_string(tree.wikis_dir().join("giardinaggio").join("rose.md")).unwrap();
    assert_eq!(
        on_disk, TWO_HEADING_BODY,
        "and the page on disk is byte for byte what it was"
    );
}
