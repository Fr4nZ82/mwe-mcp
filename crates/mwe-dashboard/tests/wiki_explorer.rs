// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dashboard MVP — memory explorer integration tests.
//!
//! Drives the new routes (`/dashboard/wiki`, `/dashboard/wiki/:id`,
//! `/dashboard/proposals`, `/dashboard/chat`) against the live router
//! mounted with a populated [`mwe_dashboard::MemoryHandles`].

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::capture::{CaptureAction, CaptureRequest, wiki_capture};
use mwe_core::config::{LlmConfig, LlmFunction};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::llm::{
    ChatMessage, ChatResponse, CompletionUsage, FakeLlmBackend, FinishReason, ToolCall,
};
use mwe_core::types::{FactId, Principal, WikiId};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

async fn make_app_with_memory() -> (Router, SqlitePool, WikiTree, tempfile::TempDir) {
    make_app_with_overrides(mwe_dashboard::LlmBackendOverrides::default()).await
}

/// Build a dashboard router whose `MemoryHandles` carries the given
/// per-slot LLM backend overrides. The fixture used by the e2e
/// integration tests for the ingest pipeline and the agentic chat loop.
async fn make_app_with_overrides(
    overrides: mwe_dashboard::LlmBackendOverrides,
) -> (Router, SqlitePool, WikiTree, tempfile::TempDir) {
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
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        llm_overrides: overrides,
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, tree, dir)
}

fn seed_alice_wiki(tree: &WikiTree) {
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
}

async fn capture_fact(pool: &SqlitePool, tree: &WikiTree, page: &str, body: &str) -> FactId {
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse("alice").unwrap(),
        page: PathBuf::from(page),
        body: body.to_owned(),
        owner: "user:alice".parse::<Principal>().unwrap(),
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

async fn seed_pending_promote_proposal(
    pool: &SqlitePool,
    proposal_id: &str,
    source_page: &str,
    fact_ids: &[FactId],
) {
    let now = chrono::Utc::now();
    let timeout = now + chrono::Duration::hours(24);
    let fact_id_strs: Vec<String> = fact_ids.iter().map(|f| f.as_str().to_owned()).collect();
    let context = serde_json::json!({
        "source_wiki_id": "alice",
        "source_page": source_page,
        "fact_ids": fact_id_strs,
    });
    sqlx::query(
        "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
         proposed_at, timeout_at, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(proposal_id)
    .bind("wiki_promote")
    .bind(serde_json::to_string(&context).unwrap())
    .bind(r#"[{"id":"q1","text":"Move?","options":[]}]"#)
    .bind(now.to_rfc3339())
    .bind(timeout.to_rfc3339())
    .execute(pool)
    .await
    .unwrap();
}

/// Seed a pending proposal of an unshipped kind (`bundle`). Applying it
/// fails at the chassis (`KindNotYetImplemented`) — a convenient stand-in
/// for "the apply handler refused" without depending on any live kind.
async fn seed_pending_unshipped_proposal(pool: &SqlitePool, proposal_id: &str) {
    let now = chrono::Utc::now();
    let timeout = now + chrono::Duration::hours(24);
    sqlx::query(
        "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
         proposed_at, timeout_at, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(proposal_id)
    .bind("bundle")
    .bind(r#"{"intent":"test"}"#)
    .bind(r#"[{"id":"q1","text":"do it?","options":[]}]"#)
    .bind(now.to_rfc3339())
    .bind(timeout.to_rfc3339())
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_pending_dedup_proposal(
    pool: &SqlitePool,
    proposal_id: &str,
    loser: &FactId,
    winner: &FactId,
) {
    let now = chrono::Utc::now();
    let timeout = now + chrono::Duration::hours(24);
    let context = serde_json::json!({
        "loser_fact_id": loser.as_str(),
        "winner_fact_id": winner.as_str(),
    });
    sqlx::query(
        "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
         proposed_at, timeout_at, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(proposal_id)
    .bind("dedup_merge")
    .bind(serde_json::to_string(&context).unwrap())
    .bind(r#"[{"id":"q1","text":"Merge?","options":[]}]"#)
    .bind(now.to_rfc3339())
    .bind(timeout.to_rfc3339())
    .execute(pool)
    .await
    .unwrap();
}

/// Inject a `dedup_merge` proposal already in `applied_pending_confirm`
/// state (`apply_mode='auto'`, `confirm_deadline` set 7gg in the
/// future) so the pending-confirms page has something to render.
/// The loser is marked superseded via the same flip the dedup handler
/// performs at apply time, so a subsequent revert can un-do it.
async fn seed_applied_pending_confirm_dedup(
    pool: &SqlitePool,
    proposal_id: &str,
    loser: &FactId,
    winner: &FactId,
) {
    seed_pending_dedup_proposal(pool, proposal_id, loser, winner).await;
    let applied_at = chrono::Utc::now();
    let confirm_deadline = applied_at + chrono::Duration::days(7);
    let spec = serde_json::json!({
        "variant": "two_way_merge",
        "loser_fact_id": loser.as_str(),
        "winner_fact_id": winner.as_str(),
    });
    sqlx::query(
        "UPDATE structure_proposals
            SET status = 'applied_pending_confirm',
                apply_mode = 'auto',
                applied_at = ?,
                applied_by = NULL,
                answers = '{}',
                spec = ?,
                confirm_deadline = ?
          WHERE proposal_id = ?",
    )
    .bind(applied_at.to_rfc3339())
    .bind(serde_json::to_string(&spec).unwrap())
    .bind(confirm_deadline.to_rfc3339())
    .bind(proposal_id)
    .execute(pool)
    .await
    .unwrap();
    // Mirror the dedup_merge apply: mark loser as superseded by winner
    // so a revert has work to undo.
    mwe_core::fact_index::mark_superseded(pool, loser, winner)
        .await
        .unwrap();
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
    let cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));
    // Skip the first-run profile wizard so the admin lands on the
    // dashboard — a fresh setup otherwise has /home redirect to /welcome.
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

#[tokio::test]
async fn wiki_list_renders_admin_identity_wiki_post_setup() {
    // The setup wizard auto-creates the admin's identity
    // wiki, so the list page is never empty on a fresh deploy. The
    // "No wikis yet" branch is now reachable only when the wikis
    // directory is wiped after setup — covered by a dedicated test
    // when the operator manually deletes everything.
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("<code>alice</code>"),
        "admin's auto-created identity wiki must be listed: {html}",
    );
    assert!(html.contains("wiki-user"), "{html}");
    // The chat panel is rendered on every authenticated page; chat.js is
    // loaded with defer so the body is in place before hydration runs.
    assert!(html.contains("id=\"chat-panel\""), "{html}");
    assert!(html.contains("/dashboard/static/chat.js"), "{html}");
    // Top nav exposes the memory entries. The Chat tab was removed
    // because the right-side panel makes a dedicated nav entry
    // redundant (the panel is rendered on every authenticated page).
    assert!(html.contains("href=\"/dashboard/wiki\""), "{html}");
    // The Proposals tab was removed: the
    // form surface is retired and proposals are operated from the chat
    // (rendered on every authenticated page), so no nav entry for either.
    assert!(
        !html.contains("href=\"/dashboard/proposals\""),
        "Proposals tab must not appear in the top nav: {html}"
    );
    // No Chat *tab* in the top nav (a `nav_link` renders as `…>Chat</a>`).
    // We assert on the tab label, not on the bare `/dashboard/chat` path,
    // because the in-flight badge now carries a defensive fallback
    // `href="/dashboard/chat"` (it is a JS-revealed control, not a nav tab).
    assert!(
        !html.contains(">Chat</a>"),
        "Chat tab must not appear in the top nav: {html}"
    );
}

#[tokio::test]
async fn wiki_view_returns_404_for_unknown_id() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/does-not-exist")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---- dashboard editor → wiki_admin::push ----

/// The dashboard textual editor must produce a `wiki_admin_op_log`
/// row with `actor_kind = 'dashboard'` and `consumer_id IS NULL`
/// when the operator saves a page. This is the load-bearing
/// invariant — the dashboard write rides the same op-log
/// path as a smart consumer's `wiki_admin_push`, discriminated only
/// by `actor_kind`.
#[tokio::test]
async fn dashboard_editor_save_writes_op_log_row_with_actor_kind_dashboard() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Setup auto-creates `wikis/alice/` of type `wiki-user` for the
    // admin. Posting an edit on a brand-new page is
    // exactly the "operator types into a textarea" gesture this test
    // covers.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/edit/notes.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from(
                "body=%23+note+dal+cruscotto%0A%0Acontenuto+di+test%0A",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "expected redirect, got {} body={}",
        response.status(),
        body_string(response).await
    );

    // The op-log carries a row with the dashboard discriminator and
    // no consumer_id (the operator is not behind an MCP device).
    let (actor_kind, sender_id, consumer_id, op_kind, pages_affected): (
        String,
        String,
        Option<String>,
        String,
        i64,
    ) = sqlx::query_as(
        "SELECT actor_kind, sender_id, consumer_id, op_kind, pages_affected
           FROM wiki_admin_op_log WHERE wiki_id = ? ORDER BY op_id DESC LIMIT 1",
    )
    .bind("alice")
    .fetch_one(&pool)
    .await
    .expect("op log row");
    assert_eq!(actor_kind, "dashboard");
    assert_eq!(sender_id, "alice");
    assert!(
        consumer_id.is_none(),
        "dashboard writes carry no consumer_id (got {consumer_id:?})"
    );
    assert_eq!(op_kind, "push_upsert");
    assert_eq!(pages_affected, 1);

    // GET the edit form back — must surface the body that was just
    // saved, proving the round-trip lands on disk via the same
    // `atomic_write` path the smart-consumer push uses.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/edit/notes.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("note dal cruscotto"), "{html}");
}

/// `_meta.md` writes from the page editor are refused with a
/// validation error — the metadata edit surface lives on
/// `/dashboard/wiki/:id/sharing` and the two flows must not
/// conflate.
#[tokio::test]
async fn dashboard_editor_refuses_meta_md_writes() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/alice/edit/_meta.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("body=fake"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422 for _meta.md write, got {}",
        response.status()
    );
}

/// `/sharing` is the WIKI-LEVEL ACL surface, which only exists for smart
/// wikis. On a standard wiki — where access is governed per-fragment — it
/// must not be reachable: a `404`, not even discoverable, so a wiki-level
/// reproject can never flatten the per-fact granularity. The inverse guard
/// of [`dashboard_editor_forbidden_on_smart_wiki`]. Even an admin (alice
/// owns the seeded wiki) is refused — the gate is the family, not the role.
#[tokio::test]
async fn dashboard_sharing_forbidden_on_standard_wiki() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Seed a STANDARD wiki by hand (no `smart:` key → defaults to false),
    // with a valid `acl_default` so the only thing that can refuse the
    // sharing surface is the new family guard, not the owner/acl_default check.
    let dir = tree.wikis_dir().join("projstd");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        "---\n\
         wiki_id: projstd\n\
         wiki_type: wiki-tech\n\
         parent_wiki_id: null\n\
         slug: projstd\n\
         title: Project Std\n\
         acl_default: 'user:alice'\n\
         ---\n",
    )
    .unwrap();
    std::fs::write(dir.join("index.md"), "# Project\n\nBody.\n").unwrap();

    // GET the sharing form on a standard wiki → 404.
    let get = send(
        &app,
        Request::builder()
            .uri("/wiki/projstd/sharing")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        get.status(),
        StatusCode::NOT_FOUND,
        "standard-wiki sharing GET must be 404 (wiki-level ACL is smart-only)"
    );

    // POST a roster change → 404, so `reproject_wiki_acl` never runs.
    let post = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/projstd/sharing")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("shared_with_raw=group:famiglia"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        post.status(),
        StatusCode::NOT_FOUND,
        "standard-wiki sharing POST must be 404 (no wiki-level reproject)"
    );
}

/// Mint a non-admin user via the invitation cycle, returning their session
/// cookie. Mirrors the inline flow in `dashboard_revert_button_admin_only`.
async fn mint_non_admin(app: &Router, admin_cookie: &str, user_id: &str) -> String {
    let create_resp = send(
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
    assert_eq!(create_resp.status(), StatusCode::OK);
    let html = body_string(create_resp).await;
    let prefix = "/dashboard/accept-invite/";
    let start = html.find(prefix).expect("invitation link");
    let after = &html[start + prefix.len()..];
    let end = after
        .find(|c: char| {
            c.is_whitespace() || c == '"' || c == '<' || c == '\'' || c == ')' || c == ','
        })
        .unwrap();
    let invitation_id = &after[..end];
    let accept = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/accept-invite/{invitation_id}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "password=non-admin-pw-12&password_confirm=non-admin-pw-12",
            ))
            .unwrap(),
    )
    .await;
    extract_cookie_value(&extract_set_cookie(&accept, "mwe_session").expect("cookie"))
}

/// Seed a STANDARD wiki `notes` owned by `user:alice` with one page that
/// carries a testata `style` sibling, so the describe editor can prove it
/// preserves siblings + body.
fn seed_notes_wiki_with_page(tree: &WikiTree) {
    let dir = tree.wikis_dir().join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        "---\nwiki_id: notes\nwiki_type: wiki-user\nparent_wiki_id: null\nslug: notes\ntitle: Notes\nacl_default: 'user:alice'\n---\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("health.md"),
        "---\nstyle: prosa\n---\n# Health\n\nDoctor visit notes.\n",
    )
    .unwrap();
}

/// The page «what goes here» (testata `description`) editor round-trips for
/// an owner: GET the form, POST a description, and the testata is rewritten
/// with the sibling field + body preserved; the page view then surfaces the
/// affordance.
#[tokio::test]
async fn dashboard_describe_round_trips_and_preserves_siblings() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await; // alice: admin + owner
    seed_notes_wiki_with_page(&tree);

    let get = send(
        &app,
        Request::builder()
            .uri("/wiki/notes/describe/health.md")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(get.status(), StatusCode::OK, "describe form GET");

    let post = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/notes/describe/health.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from(
                "description=Alice%27s+health%3A+doctors%2C+meds",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        post.status().is_redirection(),
        "POST → redirect: {}",
        post.status()
    );

    let abs = tree.wikis_dir().join("notes").join("health.md");
    assert_eq!(
        mwe_core::meta_annotate::read_page_description(&abs)
            .unwrap()
            .as_deref(),
        Some("Alice's health: doctors, meds"),
        "description must round-trip"
    );
    let raw = std::fs::read_to_string(&abs).unwrap();
    assert!(raw.contains("style: prosa"), "sibling lost: {raw}");
    assert!(raw.contains("Doctor visit notes."), "body lost: {raw}");

    let view = send(
        &app,
        Request::builder()
            .uri("/wiki/notes/view/health.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(view.status(), StatusCode::OK);
    let html = body_string(view).await;
    assert!(
        html.contains("what goes here"),
        "owner page view must surface the describe affordance: {html}"
    );
}

/// The describe editor is standard-only: a `404` on a smart wiki (GET + POST),
/// and the smart page's testata is left untouched.
#[tokio::test]
async fn dashboard_describe_forbidden_on_smart_wiki() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    let dir = tree.wikis_dir().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        "---\nwiki_id: proj\nwiki_type: wiki-tech\nparent_wiki_id: null\nslug: proj\ntitle: Project\nacl_default: 'user:alice'\nsmart: true\n---\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("index.md"),
        "---\nstyle: prosa\n---\n# P\n\nBody.\n",
    )
    .unwrap();

    let get = send(
        &app,
        Request::builder()
            .uri("/wiki/proj/describe/index.md")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(get.status(), StatusCode::NOT_FOUND, "smart describe GET");

    let post = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/proj/describe/index.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("description=hijack"))
            .unwrap(),
    )
    .await;
    assert_eq!(post.status(), StatusCode::NOT_FOUND, "smart describe POST");
    let raw = std::fs::read_to_string(dir.join("index.md")).unwrap();
    assert!(
        !raw.contains("description:"),
        "smart page must be untouched: {raw}"
    );
}

/// A non-owner non-admin is refused (`404`) on the describe editor — the gate
/// is owner-or-admin, and the page is left untouched.
#[tokio::test]
async fn dashboard_describe_refused_for_non_owner_non_admin() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let admin = login_as_admin(&app).await; // alice owns `notes`
    seed_notes_wiki_with_page(&tree);
    let bilbo = mint_non_admin(&app, &admin, "bilbo").await;

    let post = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/notes/describe/health.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, bilbo)
            .body(Body::from("description=sneaky"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        post.status(),
        StatusCode::NOT_FOUND,
        "non-owner describe POST"
    );
    let abs = tree.wikis_dir().join("notes").join("health.md");
    assert_eq!(
        mwe_core::meta_annotate::read_page_description(&abs).unwrap(),
        None,
        "non-owner must not have written a description"
    );
}

/// Describing a page that does not exist is a `404`, not a silent create.
#[tokio::test]
async fn dashboard_describe_404_on_missing_page() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_notes_wiki_with_page(&tree);

    let get = send(
        &app,
        Request::builder()
            .uri("/wiki/notes/describe/nope.md")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(get.status(), StatusCode::NOT_FOUND, "missing page describe");
}

/// The raw free-text editor is hard-forbidden on smart wikis (the
/// smart consumer is the sole writer). Even an admin gets a `404` — the
/// editor is not discoverable on a smart wiki — and the page is left
/// untouched.
#[tokio::test]
async fn dashboard_editor_forbidden_on_smart_wiki() {
    let (app, _pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Seed a SMART wiki by hand (carries `smart: true`).
    let dir = tree.wikis_dir().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("_meta.md"),
        "---\n\
         wiki_id: proj\n\
         wiki_type: wiki-tech\n\
         parent_wiki_id: null\n\
         slug: proj\n\
         title: Project\n\
         acl_default: 'user:alice'\n\
         smart: true\n\
         ---\n",
    )
    .unwrap();
    std::fs::write(dir.join("index.md"), "# Project\n\nOriginal body.\n").unwrap();

    // GET the editor form → 404 (not even discoverable).
    let get = send(
        &app,
        Request::builder()
            .uri("/wiki/proj/edit/index.md")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        get.status(),
        StatusCode::NOT_FOUND,
        "smart-wiki raw editor GET must be 404"
    );

    // POST a save → 404, and the body on disk is unchanged.
    let post = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/wiki/proj/edit/index.md")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("body=hijacked"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        post.status(),
        StatusCode::NOT_FOUND,
        "smart-wiki raw editor POST must be 404"
    );
    let on_disk = std::fs::read_to_string(dir.join("index.md")).unwrap();
    assert_eq!(
        on_disk, "# Project\n\nOriginal body.\n",
        "the refused save must leave the smart-wiki page untouched"
    );
}

// ---- dashboard revert button ----

/// Tiny URL-form encoder for the integration tests — escapes the
/// characters we actually pass (space, newline, hash, ampersand,
/// equals, plus). We do not pull a percent-encoding dep just for the
/// half-dozen literals these tests need.
fn url_form_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            },
            _ => {
                let _ = write!(out, "%{b:02X}");
            },
        }
    }
    out
}

/// Drive a save through the dashboard editor — convenience wrapper
/// used by the revert tests below to produce revertable op-log
/// rows without re-typing the urlencoded body each time.
async fn dashboard_editor_save(
    app: &Router,
    cookie: &str,
    wiki_id: &str,
    page_path: &str,
    body_text: &str,
) {
    let body = format!("body={}", url_form_encode(body_text));
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/{wiki_id}/edit/{page_path}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "save must redirect, got {} (body {})",
        response.status(),
        body_string(response).await
    );
}

/// The Revert button is rendered for revertable `push_*` rows; the
/// POST handler invokes `wiki_admin::op_revert` and redirects back to
/// the op-log view with a success flash.
#[tokio::test]
async fn dashboard_revert_button_succeeds_on_revertable_row() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Two saves on `notes.md`: the second is the target we will revert.
    // (We need a second op_log row so the first save's `pre_image_json`
    // is non-NULL — that's the row whose pre-image carries the original
    // body and whose revert restores it.)
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# v1 body\n").await;
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# v2 body\n").await;

    // GET the op-log view: the page must render a Revert form for the
    // second row (the upsert).
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/op-log")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("/wiki/alice/op-log/") && html.contains("/revert"),
        "op-log view must expose a Revert POST form for revertable rows: {html}"
    );
    assert!(
        html.contains("class=\"danger\""),
        "Revert button must reuse the danger styling: {html}"
    );

    // Look up the target op_id (the second push_upsert row).
    let target_op_id: i64 = sqlx::query_scalar(
        "SELECT op_id FROM wiki_admin_op_log
          WHERE wiki_id = 'alice' AND op_kind = 'push_upsert'
          ORDER BY op_id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("target op_id");

    // POST the revert.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/alice/op-log/{target_op_id}/revert"))
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "revert POST must redirect, got {}",
        response.status()
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        location.starts_with("/dashboard/wiki/alice/op-log?flash=revert_ok")
            || location.starts_with("/wiki/alice/op-log?flash=revert_ok"),
        "redirect location must carry success flash, got {location}"
    );

    // The compensating row landed with actor_kind='system'.
    let (actor_kind, sender_id, consumer_id, op_kind): (String, String, Option<String>, String) =
        sqlx::query_as(
            "SELECT actor_kind, sender_id, consumer_id, op_kind
               FROM wiki_admin_op_log WHERE wiki_id = 'alice'
              ORDER BY op_id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("compensation row");
    assert_eq!(actor_kind, "system");
    assert_eq!(sender_id, "alice");
    assert!(consumer_id.is_none());
    assert_eq!(op_kind, "push_upsert");

    // The flash banner is rendered on the next GET.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/op-log?flash=revert_ok")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Revert applied"),
        "success flash must render: {html}"
    );
}

/// Strict conflict policy: a later op that touched the same
/// page must trip `409 op_log_target_changed_since`; the dashboard
/// translates that to a `?flash=revert_conflict` redirect.
#[tokio::test]
async fn dashboard_revert_button_returns_409_with_conflict_details_on_target_changed() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Save v1 (creates `notes.md`), then v2 (overwrites with the body
    // we'll try to revert), then v3 (an independent later edit on the
    // same page — this is the conflict).
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# v1 body\n").await;
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# v2 body\n").await;
    // The middle row is our revert target (its pre-image is "# v1 body\n").
    let target_op_id: i64 = sqlx::query_scalar(
        "SELECT op_id FROM wiki_admin_op_log
          WHERE wiki_id = 'alice' AND op_kind = 'push_upsert'
          ORDER BY op_id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# v3 body\n").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/alice/op-log/{target_op_id}/revert"))
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        location.contains("flash=revert_conflict"),
        "conflict redirect must carry flash=revert_conflict, got {location}"
    );

    // No compensating row was written.
    let system_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_admin_op_log
          WHERE wiki_id = 'alice' AND actor_kind = 'system'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        system_rows, 0,
        "strict conflict policy must NOT write a compensation row"
    );

    // The next GET surfaces the conflict banner verbatim.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/op-log?flash=revert_conflict")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Revert refused") && html.contains("strict conflict policy"),
        "conflict banner must render with the strict-policy copy: {html}"
    );
}

/// Pull rows are non-write — the op-log view must NOT render a Revert
/// button on them (the button is replaced by a muted dash with a
/// "not revertable" tooltip).
#[tokio::test]
async fn dashboard_revert_button_hidden_for_pull_rows() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    // Build a revertable history first so the table has at least one
    // pull-discriminated assertion: a dashboard save (push_upsert) +
    // a manually inserted pull row simulating an MCP `wiki_admin_pull`.
    dashboard_editor_save(&app, &cookie, "alice", "notes.md", "# body\n").await;
    sqlx::query(
        "INSERT INTO wiki_admin_op_log
            (wiki_id, sender_id, consumer_id, actor_kind, op_kind, op_mode,
             payload_hash, pages_affected, pre_image_json, ts)
         VALUES ('alice', 'alice', 'cc-laptop', 'smart_consumer', 'pull', NULL,
                 'deadbeef', 1, NULL, datetime('now'))",
    )
    .execute(&pool)
    .await
    .unwrap();

    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/op-log")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // There IS a Revert form (for the push_upsert row), but the pull
    // row is rendered with a tooltip dash. Verify the tooltip copy.
    assert!(
        html.contains("Not revertable: non-write op"),
        "pull rows must surface a non-write tooltip: {html}"
    );
    // The pull row's op_id appears in the table but NOT in a Revert
    // form action — we check that the html does not contain a revert
    // form pointing at the pull row's op_id (the last inserted).
    let pull_op_id: i64 = sqlx::query_scalar(
        "SELECT op_id FROM wiki_admin_op_log
          WHERE wiki_id = 'alice' AND op_kind = 'pull'
          ORDER BY op_id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let pull_action = format!("/wiki/alice/op-log/{pull_op_id}/revert");
    assert!(
        !html.contains(&pull_action),
        "no Revert form must point at the pull row {pull_op_id}: {html}"
    );
}

/// Non-admin sessions get a 403 from the revert POST — the form is
/// only rendered for admins anyway, but defence-in-depth verifies the
/// extractor-level gate.
#[tokio::test]
async fn dashboard_revert_button_admin_only() {
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let admin_cookie = login_as_admin(&app).await;

    // Seed a revertable row.
    dashboard_editor_save(&app, &admin_cookie, "alice", "notes.md", "# body\n").await;
    dashboard_editor_save(&app, &admin_cookie, "alice", "notes.md", "# body2\n").await;
    let target_op_id: i64 = sqlx::query_scalar(
        "SELECT op_id FROM wiki_admin_op_log
          WHERE wiki_id = 'alice' AND op_kind = 'push_upsert'
          ORDER BY op_id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Mint a non-admin user via the invitation cycle.
    let create_resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/users/new")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &admin_cookie)
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
        &app,
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
    let bilbo_cookie =
        extract_cookie_value(&extract_set_cookie(&accept_resp, "mwe_session").expect("cookie"));

    // Bilbo (non-admin) hits the revert POST → 403.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/wiki/alice/op-log/{target_op_id}/revert"))
            .header(header::COOKIE, bilbo_cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Bilbo's GET on the op-log view (if he can read at all) does
    // not show any Revert form — the cell is a muted dash.
    let response = send(
        &app,
        Request::builder()
            .uri("/wiki/alice/op-log")
            .header(header::COOKIE, bilbo_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    // Reading is allowed (no read gate on the op-log view), but no
    // Revert form is exposed.
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("/revert"),
        "non-admin must not see a Revert form: {html}"
    );
}

#[tokio::test]
async fn chat_get_renders_form() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/chat")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("name=\"text\""), "{html}");
    assert!(html.contains("action=\"/dashboard/chat\""), "{html}");
}

#[tokio::test]
async fn chat_post_empty_text_returns_inline_validation() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Type a message before sending"), "{html}");
}

/// The right-side chat panel embeds the elements `chat.js` queries
/// at hydration — the resize handle (drag to resize), the messages
/// container (hydrated from localStorage), and the form that intercepts
/// submits and POSTs with `Accept: application/json`. This is the
/// contract between Maud and the client script; if any id disappears
/// the JS silently no-ops and the panel breaks without warning, so we
/// pin the contract here.
#[tokio::test]
async fn chat_panel_embeds_elements_required_by_chat_js() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The script is loaded with defer so it runs after the body parse.
    assert!(html.contains("/dashboard/static/chat.js"), "{html}");
    assert!(html.contains("class=\"has-chat-panel\""), "{html}");
    // Every id chat.js looks up must be present.
    for id in [
        "chat-panel",
        "chat-panel-resize",
        "chat-panel-messages",
        "chat-panel-form",
        "chat-panel-text",
    ] {
        let needle = format!("id=\"{id}\"");
        assert!(html.contains(&needle), "missing {id}: {html}");
    }
}

/// The agentic endpoint surfaces the missing `llm.hub_writer`
/// slot as a 422 with an italian actionable message. Same UX contract
/// the wizard relies on for the `llm.ingest` slot — operators should
/// see a clear "go wire the slot in the YAML" notice, not a generic
/// 500.
#[tokio::test]
async fn chat_agentic_returns_422_when_hub_writer_slot_missing() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text=ciao"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_string(response).await;
    assert!(
        body.contains("llm.hub_writer`"),
        "validation body should name the missing slot: {body}"
    );
}

/// The agentic endpoint refuses empty submissions before
/// touching the LLM. Matches the older `/dashboard/chat` validation
/// contract so the panel JS sees a consistent shape across endpoints.
#[tokio::test]
async fn chat_agentic_empty_text_returns_validation_error() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// The chat endpoint negotiates response shape on `Accept:
/// application/json`. The right-side panel uses this to receive a JSON
/// envelope on every turn (`response_html` is what it appends to its
/// scroll area, `user_text` is what it stores in localStorage). The
/// negotiation must work on the validation branch too, otherwise the
/// JS surfaces a misleading "HTTP 500" instead of the engine's
/// actionable error.
#[tokio::test]
async fn chat_post_empty_text_returns_json_when_client_asks_for_json() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, cookie)
            .body(Body::from("text="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = body_string(response).await;
    assert!(
        ct.contains("application/json"),
        "content-type {ct}; body {body}"
    );
    assert!(body.contains("\"error\""), "{body}");
}

#[tokio::test]
async fn home_page_lists_memory_section() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("Browse wikis"), "{html}");
    assert!(html.contains("Browse facts"), "{html}");
    // The proposals link now points at the chat (the form surface is
    // retired), not the removed page.
    assert!(
        html.contains("Review pending changes in the chat"),
        "{html}"
    );
    assert!(
        !html.contains("href=\"/dashboard/proposals\""),
        "Home must not link to the retired proposals form page: {html}"
    );
    // "Open the chat" link removed: chat lives in the right-side panel.
    assert!(
        !html.contains("Open the chat"),
        "Home memory section must not link to the dedicated chat page: {html}"
    );
    assert!(html.contains("MCP calls (24h)"), "{html}");
}

// ---- Proposal action routes ----
//
// The proposals questionnaire / tray FORM surface is retired: the GET
// `/dashboard/proposals` and `/dashboard/proposals/pending-confirms`
// pages are gone, and the page tests that asserted their HTML went with
// them. What remains here are the action routes — POST `apply` /
// `confirm` / `revert` and GET `open-in-chat` — kept mounted as bridge
// endpoints. Each POST now performs its chassis action and 303-redirects
// to `/dashboard/chat` (the single operational surface) instead of
// rendering a page, so these tests assert the redirect + the resulting
// DB / on-disk state rather than flash HTML.

/// Assert a response is the 303 redirect to the chat surface that the
/// retired-form action routes return on both success and classified error.
fn assert_redirects_to_chat(response: &axum::http::Response<Body>) {
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "action route must 303-redirect to the chat"
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok());
    assert_eq!(location, Some("/dashboard/chat"));
}

#[tokio::test]
async fn proposals_dedup_apply_then_revert_redirects_and_round_trips() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let loser = capture_fact(&pool, &tree, "index.md", "Bob pesa 80").await;
    let winner = capture_fact(&pool, &tree, "index.md", "Bob ora pesa 80").await;
    seed_pending_dedup_proposal(&pool, "p-dup", &loser, &winner).await;

    // Apply via the dashboard (POST with empty body) → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-dup/apply")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // The loser is now superseded — the chassis ran behind the redirect.
    let loser_row = mwe_core::fact_index::find_by_id(&pool, &loser)
        .await
        .unwrap()
        .unwrap();
    assert!(loser_row.superseded_at.is_some());

    // Revert → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-dup/revert")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // The loser is active again.
    let loser_row = mwe_core::fact_index::find_by_id(&pool, &loser)
        .await
        .unwrap()
        .unwrap();
    assert!(loser_row.superseded_at.is_none());
}

#[tokio::test]
async fn proposals_apply_unknown_id_still_redirects_to_chat() {
    // The action route does not surface a page on error any more — a
    // failed apply (unknown id) is logged and the operator is handed
    // back to the chat just like a success, where they can inspect state
    // with the read tools.
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-missing/apply")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("target_page=elsewhere.md"))
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);
}

// ---- confirm/revert action routes (form tray retired) ----

#[tokio::test]
async fn pending_confirms_confirm_route_promotes_to_applied_and_redirects() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let loser = capture_fact(&pool, &tree, "index.md", "Alice ha un cane").await;
    let winner = capture_fact(&pool, &tree, "index.md", "Alice possiede un cane").await;
    seed_applied_pending_confirm_dedup(&pool, "p-confirm-me", &loser, &winner).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-confirm-me/confirm")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    let (status, apply_mode, confirmed_by): (String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT status, apply_mode, confirmed_by FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind("p-confirm-me")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "applied");
    assert_eq!(
        apply_mode.as_deref(),
        Some("auto"),
        "confirm must not rewrite apply_mode",
    );
    assert_eq!(confirmed_by.as_deref(), Some("alice"));
}

#[tokio::test]
async fn pending_confirms_revert_route_unwinds_dedup_merge_and_redirects() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let loser = capture_fact(&pool, &tree, "index.md", "Alice ha un cane").await;
    let winner = capture_fact(&pool, &tree, "index.md", "Alice possiede un cane").await;
    seed_applied_pending_confirm_dedup(&pool, "p-revert-me", &loser, &winner).await;

    // POST revert with no body — the action route auto-detects
    // `applied_pending_confirm` and uses the caller path.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-revert-me/revert")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // Loser is un-superseded — the dedup_merge inverse actually ran.
    let loser_row = mwe_core::fact_index::find_by_id(&pool, &loser)
        .await
        .unwrap()
        .unwrap();
    assert!(loser_row.superseded_at.is_none());

    let (status, triggered_by): (String, Option<String>) = sqlx::query_as(
        "SELECT status, revert_triggered_by FROM structure_proposals WHERE proposal_id = ?",
    )
    .bind("p-revert-me")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "reverted");
    assert_eq!(triggered_by.as_deref(), Some("user"));
}

#[tokio::test]
async fn proposals_apply_failure_still_redirects_and_leaves_row_pending() {
    // Applying an unshipped-kind proposal fails at the chassis
    // (`KindNotYetImplemented`). The action route no longer renders that
    // error as a flash; it logs it and 303-redirects, leaving the row
    // `pending` so the operator can retry conversationally.
    let (app, pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_pending_unshipped_proposal(&pool, "p-forge").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-forge/apply")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);
    // Row still pending — the failed apply did not flip its status.
    let status: String =
        sqlx::query_scalar("SELECT status FROM structure_proposals WHERE proposal_id = ?")
            .bind("p-forge")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn proposals_promote_to_subwiki_round_trips_via_action_routes() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let f1 = capture_fact(&pool, &tree, "giardinaggio.md", "Note A").await;
    let f2 = capture_fact(&pool, &tree, "giardinaggio.md", "Note B").await;
    seed_pending_promote_proposal(&pool, "p-sub", "giardinaggio.md", &[f1.clone(), f2.clone()])
        .await;

    // Apply via the action route: variant=file_to_subwiki, no explicit
    // slug (so it derives from the source page stem) → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-sub/apply")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("variant=file_to_subwiki"))
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // New sub-wiki exists on disk; source file is gone.
    let new_dir = tree.wikis_dir().join("alice").join("giardinaggio");
    assert!(new_dir.exists());
    assert!(new_dir.join("_meta.md").exists());
    assert!(new_dir.join("index.md").exists());
    let source_after = tree.wikis_dir().join("alice").join("giardinaggio.md");
    assert!(!source_after.exists());
    // fact_index rows now point at the sub-wiki.
    for fid in [&f1, &f2] {
        let row = mwe_core::fact_index::find_by_id(&pool, fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "alice-giardinaggio");
    }

    // Revert via the action route → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-sub/revert")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // Source file back, sub-wiki gone, fact_index rows restored.
    assert!(source_after.exists());
    assert!(!new_dir.exists());
    for fid in [&f1, &f2] {
        let row = mwe_core::fact_index::find_by_id(&pool, fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "alice");
        assert_eq!(row.source_path, "wikis/alice/giardinaggio.md");
    }
}

#[tokio::test]
async fn proposals_apply_then_revert_round_trips_via_action_routes() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fact = capture_fact(&pool, &tree, "index.md", "Movable text").await;
    seed_pending_promote_proposal(&pool, "p-1", "index.md", std::slice::from_ref(&fact)).await;

    // Apply via the action route → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-1/apply")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("target_page=giardinaggio.md"))
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // The fact moved on disk.
    let target_contents =
        std::fs::read_to_string(tree.wikis_dir().join("alice").join("giardinaggio.md")).unwrap();
    assert!(
        target_contents.contains(&format!("f={fact}")),
        "{target_contents}"
    );

    // Revert via the action route → 303 to the chat.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/proposals/p-1/revert")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_redirects_to_chat(&response);

    // The fact is back on the source page.
    let source_after =
        std::fs::read_to_string(tree.wikis_dir().join("alice").join("index.md")).unwrap();
    assert!(
        source_after.contains(&format!("f={fact}")),
        "{source_after}"
    );
}

// ---------------------------------------------------------------------------
// E2E with FakeLlmBackend
//
// Until this section landed, the dashboard could not be tested
// end-to-end with a deterministic LLM: handlers built a fresh
// OllamaBackend on every request, so the test fixture had no seam to
// plant a fake. This was acknowledged as a deferral; the agentic loop
// forced the issue (no test could
// drive the loop without a network round-trip). MemoryHandles now
// carries optional per-slot LlmBackend overrides, so these tests
// plant a FakeLlmBackend and drive the full handler stack with no
// Ollama on the wire.
// ---------------------------------------------------------------------------

/// The chat panel (`POST /dashboard/chat/agentic`) drives the
/// `hub_writer` slot through an agentic loop: ask the model, dispatch
/// any tool calls it produces, feed the results back, repeat until
/// the model returns a textual reply. This test wires a fake backend
/// whose script is (`tool_call` `wiki_recall`) → (final assistant
/// text), posts a turn, and asserts:
///
/// - the JSON response has one trace entry naming `wiki_recall`;
/// - `final_message` is the second scripted turn;
/// - the loop ran exactly two iterations (one tool call + one final
///   reply) and did not hit the budget ceiling.
#[tokio::test]
async fn chat_agentic_loop_dispatches_tool_then_returns_final_message() {
    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        // Turn 1: model asks to invoke wiki_recall.
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_0".into(),
                    name: "wiki_recall".into(),
                    arguments: serde_json::json!({ "query": "libri" }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        // Turn 2: model emits the textual reply.
        ChatResponse {
            message: ChatMessage::assistant("Non ho trovato fatti su libri nella tua memoria."),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let (app, _pool, _tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text=cerca+libri"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");

    assert_eq!(turn["user_text"], "cerca libri");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 1, "exactly one tool call: {body}");
    assert_eq!(trace[0]["name"], "wiki_recall");
    assert_eq!(trace[0]["is_error"], false);
    assert!(
        trace[0]["result"]
            .as_str()
            .unwrap_or("")
            .contains("\"hits\""),
        "tool result should carry the recall hits envelope: {body}"
    );
    assert_eq!(
        turn["final_message"],
        "Non ho trovato fatti su libri nella tua memoria."
    );
    assert_eq!(turn["iterations"], 2);
    assert_eq!(turn["budget_exhausted"], false);
}

/// `process_submission` (the welcome-wizard primer path + the no-JS
/// `/dashboard/chat` form) ingests through `wiki_ingest_message`
/// which calls the `ingest` LLM slot. With a fake backend that
/// returns a parseable `capture` plan, the end-to-end run produces a
/// real fact in `fact_index` and on disk — the closing of the
/// deferral called out earlier.
///
/// We seed Alice's identity wiki, plant a fake backend that returns
/// the ingest plan as its `complete` response, and POST the form.
/// Assertions:
///
/// - the JSON envelope has `intent=capture`;
/// - `fact_index` has one active row for `wiki_id=alice`;
/// - the source markdown carries an `owner=` marker emitted by the
///   capture pipeline.
#[tokio::test]
async fn chat_ingest_e2e_captures_fact_with_fake_backend() {
    use mwe_core::fact_index;
    // `requested_container: true` takes the live direct-write path so the
    // fact lands in `fact_index` immediately. Every non-smart wiki is
    // a standard wiki, so a plain capture into `alice`
    // would buffer for the nightly compiler instead.
    let plan = serde_json::json!({
        "intent": "capture",
        "suggested_seed": "ho salvato",
        "context_snippet": "",
        "target_wiki_id": "alice",
        "target_page": "index.md",
        "owner_id": "user:alice",
        "allow_ids": [],
        "fact_type": "bio",
        "topics": ["intro"],
        "body": "Alice si presenta come tester della pipeline ingest.",
        "requested_container": true,
        "needs_disambig": false,
        "disambig_candidates": []
    });
    let fake = FakeLlmBackend::new("fake-ingest", plan.to_string());

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::Ingest, Arc::new(fake));
    let (app, pool, tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;
    // Make sure Alice's wiki exists on disk so the capture pipeline
    // has a target page to write to (login_as_admin only sets up the
    // identity row + db tables — setup auto-creates the admin's
    // identity wiki, so this is a guard only).
    if !tree.wikis_dir().join("alice").exists() {
        seed_alice_wiki(&tree);
    }

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text=presento+me+stessa"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let resp: serde_json::Value = serde_json::from_str(&body).expect("ingest response is JSON");
    let inner: serde_json::Value =
        serde_json::from_str(resp["response_html"].as_str().unwrap_or(""))
            .unwrap_or(serde_json::Value::Null);
    // The response_html is HTML, not JSON — parse via string match
    // instead of structure.
    let html = resp["response_html"].as_str().unwrap_or("");
    assert!(html.contains("capture"), "intent capture in {html}");
    let _ = inner; // silence unused

    let count = fact_index::count_active_in_wiki(&pool, "alice")
        .await
        .expect("count");
    assert_eq!(count, 1, "exactly one fact captured (body: {body})");
}

/// The agentic chat panel can drive a structure-proposal apply
/// end-to-end. We seed a pending `dedup_merge` proposal (no LLM
/// needed by its kind handler), script a fake `hub_writer` that
/// produces (`tool_call` `structure_proposal_get`) → (`tool_call`
/// `structure_proposal_apply`) → (final assistant message), and post
/// a confirmation message to `/dashboard/chat/agentic`. We assert:
///
/// - both tool calls appear in the trace, in order;
/// - the apply tool result carries the `revert_token` produced by the
///   chassis (proof the apply really happened);
/// - the proposal row in the database has transitioned to
///   `applied`.
#[tokio::test]
async fn chat_agentic_loop_applies_dedup_proposal_end_to_end() {
    let dedup_get_call = ToolCall {
        id: "call_0".into(),
        name: "structure_proposal_get".into(),
        arguments: serde_json::json!({ "proposal_id": "p-dup" }),
        thought_signature: None,
    };
    let dedup_apply_call = ToolCall {
        id: "call_1".into(),
        name: "structure_proposal_apply".into(),
        arguments: serde_json::json!({
            "proposal_id": "p-dup",
            "answers": {}
        }),
        thought_signature: None,
    };
    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![dedup_get_call],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![dedup_apply_call],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage::assistant(
                "Proposta applicata. Puoi revocare entro 7 giorni se cambi idea.",
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let (app, pool, tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let loser = capture_fact(&pool, &tree, "index.md", "Alice pesa 62 kg").await;
    let winner = capture_fact(&pool, &tree, "index.md", "Alice pesa adesso 62 kg").await;
    seed_pending_dedup_proposal(&pool, "p-dup", &loser, &winner).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(
                "text=sì,+applica+la+proposta+p-dup+a+conferma+esplicita",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 2, "exactly two tool calls: {body}");
    assert_eq!(trace[0]["name"], "structure_proposal_get");
    assert_eq!(trace[1]["name"], "structure_proposal_apply");
    assert_eq!(trace[1]["is_error"], false);
    let apply_result_str = trace[1]["result"].as_str().unwrap_or("");
    let apply_result: serde_json::Value =
        serde_json::from_str(apply_result_str).expect("apply result is JSON");
    assert!(
        apply_result["applied"]["revert_token"].is_string(),
        "apply outcome should carry a revert_token: {apply_result_str}"
    );
    assert_eq!(
        turn["final_message"],
        "Proposta applicata. Puoi revocare entro 7 giorni se cambi idea."
    );

    // Database state: the proposal row is now `applied`.
    let status: (String,) =
        sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
            .bind("p-dup")
            .fetch_one(&pool)
            .await
            .expect("query proposal row");
    assert_eq!(status.0, "applied");
}

/// Script the fake `hub_writer` for the batch flow: one
/// `wiki_facts_for` call, then one `wiki_forget` per fact, then a
/// final assistant message. Extracted so the test function itself
/// stays short.
fn batch_forget_chat_script(facts: &[FactId]) -> Vec<ChatResponse> {
    let mut script = Vec::with_capacity(facts.len() + 2);
    script.push(ChatResponse {
        message: ChatMessage {
            role: mwe_core::llm::Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_0".into(),
                name: "wiki_facts_for".into(),
                arguments: serde_json::json!({ "wiki_id": "alice", "limit": 10 }),
                thought_signature: None,
            }],
            tool_call_id: None,
        },
        finish_reason: FinishReason::EndOfTurn,
        usage: CompletionUsage::default(),
    });
    for (i, fact_id) in facts.iter().enumerate() {
        script.push(ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call_{}", i + 1),
                    name: "wiki_forget".into(),
                    arguments: serde_json::json!({
                        "fact_id": fact_id.as_str(),
                        "reason": "user_request"
                    }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        });
    }
    script.push(ChatResponse {
        message: ChatMessage::assistant(format!("Ho tombstonato {} fatti su libri.", facts.len())),
        finish_reason: FinishReason::EndOfTurn,
        usage: CompletionUsage::default(),
    });
    script
}

/// The chat panel can run a batch fact deletion end-to-end.
/// Seed three facts on Alice's wiki, script a fake `hub_writer` that
/// (a) lists them via `wiki_facts_for`, (b) tombstones each one in
/// turn via `wiki_forget`, (c) produces a final summary. Assert:
///
/// - the trace has 1 list call followed by 3 forget calls in order;
/// - every forget result is `tombstoned: true`;
/// - `fact_index::count_active_in_wiki` returns 0 after the loop.
#[tokio::test]
async fn chat_agentic_batch_forgets_three_facts_end_to_end() {
    use mwe_core::fact_index;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("pool");
    std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
    let tree = WikiTree::open(dir.path()).unwrap();
    seed_alice_wiki(&tree);
    let f1 = capture_fact(&pool, &tree, "index.md", "primo fatto su libri").await;
    let f2 = capture_fact(&pool, &tree, "index.md", "secondo fatto su libri").await;
    let f3 = capture_fact(&pool, &tree, "index.md", "terzo fatto su libri").await;

    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback")
        .with_chat_script(batch_forget_chat_script(&[f1, f2, f3]));
    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        llm_overrides: overrides,
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    let app = router(state);
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(
                "text=cancella+tutti+i+fatti+su+libri+nella+mia+wiki",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 4, "1 list + 3 forget expected: {body}");
    assert_eq!(trace[0]["name"], "wiki_facts_for");
    assert_eq!(trace[1]["name"], "wiki_forget");
    assert_eq!(trace[2]["name"], "wiki_forget");
    assert_eq!(trace[3]["name"], "wiki_forget");
    for entry in trace.iter().skip(1).take(3) {
        let result_str = entry["result"].as_str().unwrap_or("");
        let result: serde_json::Value =
            serde_json::from_str(result_str).expect("forget result is JSON");
        assert_eq!(
            result["forgot"]["tombstoned"], true,
            "forget must tombstone: {result_str}"
        );
    }
    let active = fact_index::count_active_in_wiki(&pool, "alice")
        .await
        .expect("count");
    assert_eq!(active, 0, "all three facts tombstoned");
}

/// Script the fake `hub_writer` for the single-fact-correction
/// flow: one `wiki_recall`, then one `wiki_supersede` against
/// `old_fact`, then a final assistant message. Extracted so the test
/// function itself stays under the clippy line cap.
fn supersede_correction_chat_script(old_fact: &FactId, new_body: &str) -> Vec<ChatResponse> {
    vec![
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_0".into(),
                    name: "wiki_recall".into(),
                    arguments: serde_json::json!({ "query": "martedì alle 14" }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "wiki_supersede".into(),
                    arguments: serde_json::json!({
                        "old_fact_id": old_fact.as_str(),
                        "new_body": new_body
                    }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage::assistant(
                "Ho corretto il fatto: ora dice mercoledì alle 14 invece di martedì.",
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]
}

/// The chat panel can correct a single fact end-to-end. We
/// seed Alice's wiki with one fact ("ho deciso martedì alle 14"),
/// script a fake `hub_writer` that (a) finds the fact via `wiki_recall`,
/// (b) replaces it via `wiki_supersede` with the corrected body, then
/// (c) emits a final summary. Assert:
///
/// - the trace has `wiki_recall` then `wiki_supersede` in order;
/// - the supersede tool result carries the new `fact_id`;
/// - the original fact is tombstoned with `superseded_by` pointing at
///   the new row, and the new row carries the corrected body;
/// - `fact_index::count_active_in_wiki` still returns 1 (the old row
///   is gone, the new one is active).
#[tokio::test]
async fn chat_agentic_supersedes_single_fact_with_corrected_body_end_to_end() {
    use mwe_core::fact_index;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("pool");
    std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
    let tree = WikiTree::open(dir.path()).unwrap();
    seed_alice_wiki(&tree);
    let old_fact = capture_fact(&pool, &tree, "index.md", "ho deciso martedì alle 14").await;
    // Give the old fact a validity window so the test pins the
    // carry-over: a body correction must not reopen/close the claim.
    sqlx::query("UPDATE fact_index SET valid_from = ?, valid_to = ? WHERE fact_id = ?")
        .bind("2026-06-01")
        .bind("2026-06-30")
        .bind(old_fact.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(
        supersede_correction_chat_script(&old_fact, "ho deciso mercoledì alle 14"),
    );

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        llm_overrides: overrides,
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    let app = router(state);
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(
                "text=correggi+il+fatto+su+martedì+sostituendolo+con+mercoledì",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 2, "expected recall + supersede: {body}");
    assert_eq!(trace[0]["name"], "wiki_recall");
    assert_eq!(trace[1]["name"], "wiki_supersede");
    assert_eq!(trace[1]["is_error"], false);

    let supersede_result_str = trace[1]["result"].as_str().unwrap_or("");
    let supersede_result: serde_json::Value =
        serde_json::from_str(supersede_result_str).expect("supersede result is JSON");
    let new_fact_id = supersede_result["superseded"]["new_fact_id"]
        .as_str()
        .expect("new_fact_id minted")
        .to_owned();
    assert_ne!(new_fact_id, old_fact.as_str());

    // Old row is tombstoned via the supersede chain.
    let old_row = fact_index::find_by_id(&pool, &old_fact)
        .await
        .unwrap()
        .expect("old row still present");
    assert!(
        old_row.superseded_at.is_some(),
        "old fact must carry superseded_at: {old_row:?}"
    );
    assert_eq!(
        old_row.superseded_by.as_ref().map(FactId::as_str),
        Some(new_fact_id.as_str())
    );

    // New row exists with the corrected body and inherited owner.
    let new_fact_typed = FactId::parse(&new_fact_id).expect("new fact id parses");
    let new_row = fact_index::find_by_id(&pool, &new_fact_typed)
        .await
        .unwrap()
        .expect("new row must exist");
    assert_eq!(new_row.text, "ho deciso mercoledì alle 14");
    assert_eq!(new_row.wiki_id, "alice");
    assert!(new_row.superseded_at.is_none() && new_row.deleted_at.is_none());
    // The validity window carried over like the rest of the metadata.
    assert_eq!(new_row.valid_from.as_deref(), Some("2026-06-01"));
    assert_eq!(new_row.valid_to.as_deref(), Some("2026-06-30"));

    // Active count is unchanged (1 in, 1 out, 1 in).
    let active = fact_index::count_active_in_wiki(&pool, "alice")
        .await
        .expect("count");
    assert_eq!(active, 1, "supersede should keep one active fact");
}

/// The dispatcher refuses to supersede a fact that is already
/// tombstoned (or itself superseded). This prevents the chat from
/// silently chaining replacements on a stale candidate; the LLM should
/// recall again or surface the error to the user.
#[tokio::test]
async fn chat_agentic_supersede_refuses_already_tombstoned_fact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("pool");
    std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
    let tree = WikiTree::open(dir.path()).unwrap();
    seed_alice_wiki(&tree);
    let old_fact = capture_fact(&pool, &tree, "index.md", "fatto da cancellare").await;
    // Tombstone the fact before the chat tries to supersede it.
    mwe_core::capture::wiki_forget(
        &tree,
        &pool,
        Arc::new(FakeEmbedder::new("fake-bge-m3", 8)),
        &old_fact,
        "user_request",
    )
    .await
    .unwrap();

    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_0".into(),
                    name: "wiki_supersede".into(),
                    arguments: serde_json::json!({
                        "old_fact_id": old_fact.as_str(),
                        "new_body": "non importa"
                    }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage::assistant(
                "Il fatto non è più attivo, riprova con un altro candidato.",
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        llm_overrides: overrides,
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    let app = router(state);
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text=sostituisci+quel+fatto"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0]["name"], "wiki_supersede");
    assert_eq!(
        trace[0]["is_error"], true,
        "supersede must surface as error: {body}"
    );
    let err_str = trace[0]["result"].as_str().unwrap_or("");
    assert!(
        err_str.contains("already superseded or tombstoned"),
        "error should explain why: {err_str}"
    );
}

/// Seed a sub-wiki directory under an existing root wiki. Used by the
/// hierarchical-move integration test to set up a scope-change target.
fn seed_subwiki_under(tree: &WikiTree, parent_slug: &str, parent_id: &str, slug: &str) {
    let dir = tree.wikis_dir().join(parent_slug).join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        "---\nwiki_id: {parent_id}-{slug}\nwiki_type: wiki-cliente\nparent_wiki_id: {parent_id}\nslug: {slug}\ntitle: {slug}\nacl_default: 'user:alice'\n---\n",
    );
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Script the fake `hub_writer` for the hierarchical move flow:
/// a single `wiki_change_scope` call with the supplied source + parent,
/// then a final assistant message. Extracted so the test functions
/// themselves stay under the clippy line cap.
fn change_scope_chat_script(source_id: &str, new_parent: Option<&str>) -> Vec<ChatResponse> {
    let mut args = serde_json::json!({ "source_wiki_id": source_id });
    if let Some(p) = new_parent {
        args["new_parent_wiki_id"] = serde_json::Value::String(p.to_owned());
    }
    vec![
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_0".into(),
                    name: "wiki_change_scope".into(),
                    arguments: args,
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage::assistant(new_parent.map_or_else(
                || "Ho promosso la wiki a radice.".to_owned(),
                |p| format!("Ho spostato la wiki sotto `{p}`."),
            )),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]
}

/// The chat panel can move a wiki under a different parent
/// end-to-end. Seed Alice's wiki with a `acmecorp` sub-wiki holding
/// one fact, plus a `lavoro` sibling root. Script a fake `hub_writer`
/// that calls `wiki_change_scope` to move `alice-acmecorp` under
/// `lavoro`, then emits a final summary. Assert:
///
/// - the trace has a single `wiki_change_scope` call;
/// - the tool result reports `facts_rebased = 1`;
/// - the directory has moved on disk (sub-wiki is gone from its old
///   location, present under the new parent);
/// - `_meta.md.parent_wiki_id` of the moved wiki now points at the
///   new parent;
/// - the fact's `source_path` was rebased to the new location;
/// - `wiki_id` stayed stable (the spec invariant).
#[tokio::test]
async fn chat_agentic_changes_wiki_scope_under_new_parent_end_to_end() {
    use mwe_core::fact_index;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("pool");
    std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
    let tree = WikiTree::open(dir.path()).unwrap();
    seed_alice_wiki(&tree);
    // Second top-level wiki Alice can move things under.
    let lavoro_meta = "---\n\
                       wiki_id: lavoro\n\
                       wiki_type: wiki-user\n\
                       parent_wiki_id: null\n\
                       slug: lavoro\n\
                       title: Lavoro\n\
                       acl_default: 'user:alice'\n\
                       ---\n";
    let lavoro_dir = tree.wikis_dir().join("lavoro");
    std::fs::create_dir_all(&lavoro_dir).unwrap();
    std::fs::write(lavoro_dir.join("_meta.md"), lavoro_meta).unwrap();
    seed_subwiki_under(&tree, "alice", "alice", "acmecorp");

    // Capture a fact in the sub-wiki being moved, so we can verify the
    // source_path is rebased after the scope change.
    let acme_fact = {
        let req = mwe_core::capture::CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse("alice-acmecorp").unwrap(),
            page: PathBuf::from("intro.md"),
            body: "ACME è un cliente storico".to_owned(),
            owner: "user:alice".parse::<Principal>().unwrap(),
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
        let embedder: Arc<dyn mwe_core::embedder::Embedder> =
            Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
        let out = mwe_core::capture::wiki_capture(&tree, &pool, embedder, req)
            .await
            .unwrap();
        out.fact_id
    };

    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback")
        .with_chat_script(change_scope_chat_script("alice-acmecorp", Some("lavoro")));

    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree: tree.clone(),
        embedder,
        llm_config: std::sync::Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        llm_overrides: overrides,
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    let app = router(state);
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/chat/agentic")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("text=sposta+la+wiki+acmecorp+dentro+lavoro"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let turn: serde_json::Value = serde_json::from_str(&body).expect("agentic turn is JSON");
    let trace = turn["trace"].as_array().expect("trace is array");
    assert_eq!(trace.len(), 1, "single change_scope expected: {body}");
    assert_eq!(trace[0]["name"], "wiki_change_scope");
    assert_eq!(trace[0]["is_error"], false);

    let result_str = trace[0]["result"].as_str().unwrap_or("");
    let result: serde_json::Value = serde_json::from_str(result_str).expect("scope result is JSON");
    assert_eq!(result["scope_changed"]["facts_rebased"], 1);
    assert_eq!(result["scope_changed"]["new_parent_wiki_id"], "lavoro");

    // Directory has moved on disk.
    assert!(!tree.wikis_dir().join("alice/acmecorp").exists());
    assert!(tree.wikis_dir().join("lavoro/acmecorp").exists());

    // Moved wiki's parent_wiki_id now points at lavoro.
    let moved_meta =
        std::fs::read_to_string(tree.wikis_dir().join("lavoro/acmecorp/_meta.md")).unwrap();
    assert!(moved_meta.contains("parent_wiki_id: lavoro"));

    // Fact's source_path is rebased; wiki_id stayed stable.
    let row = fact_index::find_by_id(&pool, &acme_fact)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.source_path, "wikis/lavoro/acmecorp/intro.md");
    assert_eq!(row.wiki_id, "alice-acmecorp");
}

/// `GET /dashboard/proposals/:id/open-in-chat` runs the
/// agentic loop server-side with a primer that asks the model to
/// summarise the proposal (no apply), and renders a landing page
/// whose `window.__mweChatPrimer` carries the resulting turn. The
/// chat panel's JS picks it up on hydrate so the user lands on a
/// fresh page already showing the proposal summary in the panel.
/// This is the NON-emergence (review/apply) primer branch — a
/// `dedup_merge` proposal that is still pending.
#[tokio::test]
async fn open_in_chat_primes_panel_with_proposal_summary() {
    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        ChatResponse {
            message: ChatMessage {
                role: mwe_core::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_0".into(),
                    name: "structure_proposal_get".into(),
                    arguments: serde_json::json!({ "proposal_id": "p-dup" }),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
        ChatResponse {
            message: ChatMessage::assistant(
                "Questa proposta unisce due fatti duplicati. Confermi l'applicazione?",
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);
    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let (app, pool, tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let loser = capture_fact(&pool, &tree, "index.md", "Bob pesa 80").await;
    let winner = capture_fact(&pool, &tree, "index.md", "Bob ora pesa 80").await;
    seed_pending_dedup_proposal(&pool, "p-dup", &loser, &winner).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/proposals/p-dup/open-in-chat")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The landing page advertises the proposal id and primes the panel.
    assert!(html.contains("<code>p-dup</code>"), "{html}");
    assert!(
        html.contains("window.__mweChatPrimer ="),
        "panel primer script must be present: {html}"
    );
    assert!(
        html.contains("structure_proposal_get"),
        "primer trace should include the get tool: {html}"
    );
    assert!(
        html.contains("Confermi l'applicazione"),
        "primer final_message should be present: {html}"
    );

    // The proposal must still be `pending` — open-in-chat is a
    // read-only briefing, not an apply.
    let status: (String,) =
        sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
            .bind("p-dup")
            .fetch_one(&pool)
            .await
            .expect("query proposal row");
    assert_eq!(status.0, "pending", "open-in-chat must not apply");
}

/// The in-flight badge's data endpoint
/// (`GET /dashboard/proposals/in-flight/chat-turn`) runs the overview
/// primer through the agentic loop and returns the turn as **JSON** — not
/// an HTML landing page — so `chat.js` can render it inline in the panel
/// (with a spinner while it loads). Asserts the JSON shape the panel
/// consumes.
#[tokio::test]
async fn in_flight_chat_turn_returns_overview_json() {
    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        ChatResponse {
            message: ChatMessage::assistant("Hai 1 proposta in sospeso da rivedere."),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);
    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let (app, _pool, _tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/proposals/in-flight/chat-turn")
            .header(header::COOKIE, cookie)
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    // It is the JSON turn the panel consumes, not an HTML landing page.
    assert!(
        !body.contains("<html") && !body.contains("window.__mweChatPrimer"),
        "endpoint must return JSON, not a landing page: {body}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("AgenticTurn JSON");
    assert_eq!(
        v["final_message"], "Hai 1 proposta in sospeso da rivedere.",
        "{v}"
    );
    assert!(v["trace"].is_array(), "turn carries a trace array: {v}");
}

// ---------------- /dashboard/facts ----------------

/// Seed a memory wiki on disk owned by `user:alice` under the given id.
/// Keeps the test fixture symmetric across multi-wiki scenarios — the
/// dashboard user is always `alice`, so every test wiki it browses must
/// be readable by that user.
fn seed_wiki_for_alice(tree: &WikiTree, wiki_id: &str) {
    let dir = tree.wikis_dir().join(wiki_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        "---\n\
         wiki_id: {wiki_id}\n\
         wiki_type: wiki-user\n\
         parent_wiki_id: null\n\
         slug: {wiki_id}\n\
         title: {wiki_id}\n\
         acl_default: 'user:alice'\n\
         ---\n"
    );
    std::fs::write(dir.join("_meta.md"), meta).unwrap();
}

/// Capture a fact into an arbitrary wiki owned by `user:alice`.
async fn capture_fact_in(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    page: &str,
    body: &str,
) -> FactId {
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let req = CaptureRequest {
        authored_refs: Vec::new(),
        wiki_id: WikiId::parse(wiki_id).unwrap(),
        page: PathBuf::from(page),
        body: body.to_owned(),
        owner: "user:alice".parse::<Principal>().unwrap(),
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
async fn facts_page_renders_empty_state() {
    let (app, _pool, _tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/facts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("No facts to show"), "{html}");
    // Filter form is always rendered even on the empty branch.
    assert!(html.contains("name=\"wiki_id\""), "{html}");
}

#[tokio::test]
async fn facts_page_lists_captured_facts_for_connected_user() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let _f1 = capture_fact(&pool, &tree, "index.md", "fact body uno").await;
    let _f2 = capture_fact(&pool, &tree, "index.md", "fact body due").await;
    let _f3 = capture_fact(&pool, &tree, "index.md", "fact body tre").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/facts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    for needle in ["fact body uno", "fact body due", "fact body tre"] {
        assert!(html.contains(needle), "missing {needle}: {html}");
    }
    // Filter form + action links are present.
    assert!(
        html.contains("action=\"/dashboard/facts\""),
        "filter form action: {html}"
    );
    assert!(html.contains(">wiki</a>"), "wiki action link: {html}");
    assert!(html.contains(">edit</a>"), "edit action link: {html}");
    // Pagination scaffold is in the page even on a single short page.
    assert!(html.contains("previous"), "{html}");
    assert!(html.contains("next"), "{html}");
}

#[tokio::test]
async fn facts_page_respects_wiki_id_filter() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    // Two memory wikis, both readable by alice. Two facts each.
    seed_alice_wiki(&tree);
    seed_wiki_for_alice(&tree, "alice-giardinaggio");
    capture_fact(&pool, &tree, "index.md", "alice main A").await;
    capture_fact(&pool, &tree, "index.md", "alice main B").await;
    capture_fact_in(&pool, &tree, "alice-giardinaggio", "index.md", "giardino A").await;
    capture_fact_in(&pool, &tree, "alice-giardinaggio", "index.md", "giardino B").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/facts?wiki_id=alice")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;

    assert!(html.contains("alice main A"), "{html}");
    assert!(html.contains("alice main B"), "{html}");
    assert!(
        !html.contains("giardino A") && !html.contains("giardino B"),
        "wiki_id filter should drop alice-giardinaggio rows: {html}"
    );
}

/// `GET /dashboard/facts/:fact_id/edit` renders the edit form
/// pre-populated with the current `fact_index` row. ACL + validity are
/// the structured engine-direct sub-forms (posting to `/facts/:id/acl`
/// and `/facts/:id/validity`); body / topics / `fact_type` ride the
/// form-to-chat bridge. The current validity bounds are surfaced too.
#[tokio::test]
async fn facts_edit_form_pre_populates_from_fact_index_row() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fact = capture_fact(&pool, &tree, "index.md", "Alice usa la bici a Milano").await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/facts/{fact}/edit"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The supersede (body/topics/fact_type) form targets the chat-bridge
    // submit handler.
    assert!(
        html.contains(&format!("action=\"/dashboard/facts/{fact}/edit/submit\"")),
        "supersede form action: {html}"
    );
    // The two structured sub-forms post to the engine-direct routes.
    assert!(
        html.contains(&format!("action=\"/dashboard/facts/{fact}/acl\"")),
        "ACL sub-form action: {html}"
    );
    assert!(
        html.contains(&format!("action=\"/dashboard/facts/{fact}/validity\"")),
        "validity sub-form action: {html}"
    );
    // Structured ACL fields (owner + allow) and validity date inputs.
    assert!(html.contains("name=\"owner\""), "{html}");
    assert!(html.contains("name=\"allow\""), "{html}");
    assert!(html.contains("name=\"valid_from\""), "{html}");
    assert!(html.contains("name=\"valid_to\""), "{html}");
    // Supersade fields still on the chat bridge.
    assert!(html.contains("name=\"topics\""), "{html}");
    assert!(html.contains("name=\"fact_type\""), "{html}");
    assert!(html.contains("name=\"body\""), "{html}");
    // The current body + the validity rows are surfaced in the summary.
    assert!(html.contains("Alice usa la bici a Milano"), "{html}");
    assert!(html.contains("valid_from"), "validity surfaced: {html}");
    // The form-to-chat bridge expectation is surfaced in copy.
    assert!(
        html.contains("explicit") && html.contains("agentic chat"),
        "user notice about explicit confirmation via chat: {html}"
    );
    // ACL / validity must NOT be advertised as chat-bridge fields anymore.
    assert!(
        !html.contains("name=\"acl\"") && !html.contains("name=\"acl_allow\""),
        "the old chat-bridge ACL fields must be gone: {html}"
    );
}

/// `POST /dashboard/facts/:fact_id/edit/submit` composes a textual
/// instruction from the supersede delta (topics / `fact_type` / body) and
/// routes it through the agentic loop. The response is a landing page
/// that primes `window.__mweChatPrimer` with the run; the composed
/// instruction is the `user_text` of that primer so the chat panel shows
/// exactly what the bridge said on the user's behalf.
#[tokio::test]
async fn facts_edit_submit_composes_message_and_primes_chat_panel() {
    let fake = mwe_core::llm::FakeLlmBackend::new("fake-hub", "fallback").with_chat_script(vec![
        ChatResponse {
            message: ChatMessage::assistant(
                "Ricevuta la richiesta di modifica del fact. Confermi l'applicazione?",
            ),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        },
    ]);
    let overrides =
        mwe_dashboard::LlmBackendOverrides::default().with(LlmFunction::HubWriter, Arc::new(fake));
    let (app, pool, tree, _dir) = make_app_with_overrides(overrides).await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fact = capture_fact(&pool, &tree, "index.md", "Alice usa la bici a Milano").await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri(format!("/facts/{fact}/edit/submit"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            // Supersade delta: a topics change (ACL / validity left the
            // bridge for the structured routes).
            .body(Body::from("topics=mobilita%2C+milano&fact_type=&body="))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The landing page primes the chat panel with the agentic turn.
    assert!(
        html.contains("window.__mweChatPrimer ="),
        "panel primer script must be present: {html}"
    );
    assert!(html.contains(fact.as_str()), "{html}");
    // The composed instruction follows the metadata-only macro-case
    // ("Edit fact … set `topics` …."). We anchor on the
    // deterministic substrings the mapper emits.
    assert!(
        html.contains("Edit fact") && html.contains("set `topics`"),
        "primer user_text should follow the deterministic mapper template: {html}"
    );
    // ACL is not a chat-bridge instruction (it rides the structured route).
    assert!(
        !html.contains("set `acl`"),
        "ACL must not ride the chat bridge: {html}"
    );
    // The final_message of the scripted agentic turn surfaces in the
    // primer payload.
    assert!(
        html.contains("Confermi"),
        "primer final_message should ask for confirmation: {html}"
    );
}

/// The legacy `GET /dashboard/facts/:fact_id/open-in-chat`
/// route (vestigial pre-bridge) is dropped. The dashboard router
/// has no handler for it; axum's default behaviour for an unknown
/// nested path is `404 Not Found`.
#[tokio::test]
async fn facts_legacy_open_in_chat_route_is_gone() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fact = capture_fact(&pool, &tree, "index.md", "Alice usa la bici a Milano").await;

    let response = send(
        &app,
        Request::builder()
            .uri(format!("/facts/{fact}/open-in-chat"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "legacy open-in-chat path must not be served anymore",
    );
}

/// The per-row "Modifica" deep-link on
/// `/dashboard/facts` points at the new edit form, not at the
/// (removed) legacy primer page.
#[tokio::test]
async fn facts_index_deep_link_points_at_edit_form() {
    let (app, pool, tree, _dir) = make_app_with_memory().await;
    let cookie = login_as_admin(&app).await;
    seed_alice_wiki(&tree);
    let fact = capture_fact(&pool, &tree, "index.md", "Alice usa la bici a Milano").await;

    let response = send(
        &app,
        Request::builder()
            .uri("/facts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains(">edit</a>"), "{html}");
    assert!(
        html.contains(&format!("/dashboard/facts/{fact}/edit")),
        "row deep-link must target the new /edit form: {html}"
    );
    // The *per-fact* legacy primer page is gone; assert on that exact
    // path rather than the bare `/open-in-chat` substring (the
    // single-proposal born-applied receipt still uses
    // `/dashboard/proposals/:id/open-in-chat`, so a substring match would
    // be ambiguous).
    assert!(
        !html.contains(&format!("/dashboard/facts/{fact}/open-in-chat")),
        "no row should still link at the removed per-fact open-in-chat page: {html}"
    );
}
