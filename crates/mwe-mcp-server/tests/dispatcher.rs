// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the `mcp` dispatcher.
//!
//! Drives the same code path as a real MCP transport call, minus the
//! transport itself: the test builds a shared [`McpState`] with a
//! [`FakeEmbedder`], pins an [`IdentityProfile`], and calls
//! [`mcp::dispatch`] directly. Audit row writes are bypassed (the
//! dispatcher writes them around `dispatch`, not inside it).
//!
//! These tests prove the **wire-shape contract** of every tool that
//! does not require an external LLM: handlers backed only by
//! `mwe-core` primitives are exercised end-to-end against a fresh
//! `engine.db`.
//!
//! Tools that require Ollama / a remote LLM (`wiki_ingest_message`,
//! the LLM-driven `wiki_search` path with a deep recall stage) are
//! covered by the per-module tests in `mwe-core`; here we assert the
//! dispatcher wiring + error mapping.

use std::sync::Arc;

use mwe_core::audit::{ToolExecutionInput, record};
use mwe_core::config::{LlmConfig, LlmFunctionConfig};
use mwe_core::consumers;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::enrollment::{EnrollmentFile, GroupEntry, UserEntry, mirror_to_db};
use mwe_core::events::{self, EventKind};
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_mcp_server::mcp;
use mwe_mcp_server::mcp::state::{IdentityProfile, McpState};
use serde_json::{Value, json};

async fn fixture(
    is_admin: bool,
    consumer_id: Option<&str>,
) -> (McpState, IdentityProfile, tempfile::TempDir) {
    fixture_with_llm(is_admin, consumer_id, LlmConfig::default()).await
}

/// Variant of [`fixture`] that lets a test wire the `ingest` slot to
/// the `test-fakes` [`FakeLlmBackend`] (encoded as
/// `backend="fake", model=<canned response>`), so `wiki_ingest_message`
/// runs end-to-end without an Ollama instance.
async fn fixture_with_llm(
    is_admin: bool,
    consumer_id: Option<&str>,
    llm_config: LlmConfig,
) -> (McpState, IdentityProfile, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("db");
    let tree = WikiTree::open(dir.path()).expect("tree");
    let secret = TokenSecret::new(vec![0xCDu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
    let state = McpState {
        pool,
        tree,
        embedder,
        secret,
        blacklist,
        delegations,
        llm_config,
        recall: Arc::new(std::sync::RwLock::new(
            mwe_core::config::RecallConfig::default(),
        )),
        workdir: dir.path().to_path_buf(),
        document_policy: mwe_core::document::DocumentPolicy::default(),
        reindex_tx: None,
    };
    let identity = IdentityProfile {
        sender_id: "alice".into(),
        device_label: "test-cli".into(),
        rate_limit_id: "default".into(),
        consumer_id: consumer_id.map(str::to_owned),
        is_admin,
        consumer_class: mwe_core::jwt::ConsumerClass::Standard,
        profile: mwe_core::jwt::ConsumerProfile::Local,
    };
    (state, identity, dir)
}

async fn call(
    state: &McpState,
    identity: &IdentityProfile,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    mcp::dispatch(state, identity, name, args)
        .await
        .map_err(|e| e.to_string())
}

/// The builtin `guest` pseudo-identity (roadmap 40): every tool that
/// leaves permanent state or hands out an operator surface refuses it,
/// with one uniform wire class so a bridge can map it once.
#[tokio::test]
async fn guest_is_refused_on_the_permanent_write_surface() {
    let (state, mut identity, _dir) = fixture(false, Some("sam-bot")).await;
    identity.sender_id = "guest".into();

    for (tool, args) in [
        (
            "wiki_ingest_external",
            json!({"source": "text", "text": "doc"}),
        ),
        (
            "wiki_admin_notify",
            json!({"wiki_id": "alice", "topic": "t", "body": "b",
                   "source": {"kind": "consumer", "ref": "sam"}}),
        ),
        ("consumer_register", json!({"consumer_id": "sam-bot"})),
        ("tool_log_search", json!({})),
        ("dashboard_link", json!({"intent": "home"})),
    ] {
        let err = match call(&state, &identity, tool, args).await {
            Err(e) => e,
            Ok(v) => panic!("{tool} accepted a guest sender: {v}"),
        };
        assert!(err.contains("sender_unauthorized"), "{tool}: {err}");
        assert!(err.contains("guest"), "{tool}: {err}");
    }
}

#[tokio::test]
async fn unknown_tool_returns_not_found() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(&state, &identity, "does_not_exist", json!({}))
        .await
        .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

#[tokio::test]
async fn wiki_ingest_message_rejects_empty_text_via_dispatcher() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "   "}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("invalid_input"), "{err}");
}

#[tokio::test]
async fn wiki_ingest_message_rejects_malformed_occurred_at() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "ciao", "metadata": {"occurred_at": "yesterday at noon"}}),
    )
    .await
    .expect_err("a malformed semantic clock must be rejected, not silently ignored");
    assert!(err.contains("invalid_input"), "{err}");
    assert!(err.contains("occurred_at"), "{err}");
}

#[tokio::test]
async fn wiki_ingest_message_sender_token_mismatch() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "hello", "sender_id": "samvise"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("sender_token_mismatch"), "{err}");
}

#[tokio::test]
async fn consumer_register_then_poll_roundtrip() {
    let (state, identity, _dir) = fixture(false, Some("samvise-prod")).await;
    // A standard consumer's `consumer_register` binds its own `sender_id` as
    // the consumer's `system_user_id` (diagonal model), so that sender must be
    // an enrolled user (FK) — exactly as a real standard token's sender always
    // is (token-issue enforces enrollment).
    sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('alice', 0)")
        .execute(&state.pool)
        .await
        .expect("enroll sender");
    let out = call(
        &state,
        &identity,
        "consumer_register",
        json!({"consumer_id": "samvise-prod", "display_name": "Sam"}),
    )
    .await
    .expect("register");
    assert_eq!(out["registered"], json!(true));
    assert_eq!(out["fresh_registration"], json!(true));
    assert!(out["consumer_secret"].is_string());

    // No pending events ⇒ empty list, has_more=false.
    let poll = call(
        &state,
        &identity,
        "events_poll",
        json!({"consumer_id": "samvise-prod"}),
    )
    .await
    .expect("poll");
    assert!(poll["events"].as_array().unwrap().is_empty());
    assert_eq!(poll["has_more"], json!(false));
}

#[tokio::test]
async fn events_poll_rejects_consumer_mismatch() {
    let (state, identity, _dir) = fixture(false, Some("samvise-prod")).await;
    consumers::register(
        &state.pool,
        &consumers::RegisterRequest {
            consumer_id: "telegram-bot",
            display_name: None,
            callback_url: None,
            kinds_subscribed: None,
            metadata: None,
            system_user_id: None,
        },
    )
    .await
    .unwrap();
    let err = call(
        &state,
        &identity,
        "events_poll",
        json!({"consumer_id": "telegram-bot"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("sender_unauthorized"), "{err}");
}

#[tokio::test]
async fn events_poll_admin_fallback_for_any_consumer() {
    let (state, identity, _dir) = fixture(true, None).await; // admin, no consumer_id
    consumers::register(
        &state.pool,
        &consumers::RegisterRequest {
            consumer_id: "telegram-bot",
            display_name: None,
            callback_url: None,
            kinds_subscribed: None,
            metadata: None,
            system_user_id: None,
        },
    )
    .await
    .unwrap();
    // Plant one event so the response is non-trivial.
    events::insert_event(
        &state.pool,
        EventKind::DedupProposed,
        None,
        None,
        &serde_json::Value::Null,
    )
    .await
    .unwrap();
    let out = call(
        &state,
        &identity,
        "events_poll",
        json!({"consumer_id": "telegram-bot"}),
    )
    .await
    .expect("poll");
    assert_eq!(out["events"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn events_ack_idempotent_across_calls() {
    let (state, identity, _dir) = fixture(true, None).await;
    consumers::register(
        &state.pool,
        &consumers::RegisterRequest {
            consumer_id: "samvise-prod",
            display_name: None,
            callback_url: None,
            kinds_subscribed: None,
            metadata: None,
            system_user_id: None,
        },
    )
    .await
    .unwrap();
    let event_id = events::insert_event(
        &state.pool,
        EventKind::StructureApplied,
        None,
        None,
        &serde_json::Value::Null,
    )
    .await
    .unwrap();
    let first = call(
        &state,
        &identity,
        "events_ack",
        json!({"consumer_id": "samvise-prod", "event_ids": [event_id]}),
    )
    .await
    .expect("ack");
    assert_eq!(first["acked"], json!(1));
    let second = call(
        &state,
        &identity,
        "events_ack",
        json!({"consumer_id": "samvise-prod", "event_ids": [event_id]}),
    )
    .await
    .expect("ack");
    assert_eq!(second["acked"], json!(1));
    assert!(second["unknown"].as_array().unwrap().is_empty());
}

// ---- structure_proposal_* removed from MCP ----
//
// The proposal tools no longer exist on the MCP surface. Structural
// changes apply directly in REM and reach the consumer as
// `structure_applied` notices over `events_poll`; the dashboard is the
// undo surface (it calls `mwe-core::proposals` directly). The
// dispatcher must surface them as `not_found`, since they're not
// registered in `schemas::all_tools()`.

#[tokio::test]
async fn structure_proposal_apply_removed_returns_not_found() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "structure_proposal_apply",
        json!({"proposal_id": "p-1", "answers": {}}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

#[tokio::test]
async fn structure_proposal_confirm_removed_returns_not_found() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "structure_proposal_confirm",
        json!({"proposal_id": "p-1"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

#[tokio::test]
async fn structure_proposal_revert_removed_returns_not_found() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "structure_proposal_revert",
        json!({"proposal_id": "p-1", "token": "any"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

#[tokio::test]
async fn structure_proposal_list_removed_returns_not_found() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(&state, &identity, "structure_proposal_list", json!({}))
        .await
        .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

#[tokio::test]
async fn wiki_read_returns_not_found_for_unknown_wiki() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(&state, &identity, "wiki_read", json!({"wiki_id": "nope"}))
        .await
        .expect_err("must reject");
    assert!(err.contains("not_found"), "{err}");
}

/// End-to-end ACL projection through `wiki_read`. Three-region page on
/// `wikis/alice/index.md`: global → owner=user:alice → allow=group:famiglia.
/// `alice` (member of `famiglia`) sees everything; `bob` (also in
/// `famiglia`) sees global + the group-allowed region but not alice's
/// owner-only region; `carol` (no group) sees only the global region.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "single end-to-end scenario with three senders"
)]
async fn wiki_read_projects_acl_per_sender() {
    let (state, alice_identity, dir) = fixture(false, None).await;

    // Seed enrollment so `enrollment::groups_for` returns the group rows
    // populated by the wiring under test.
    let enrollment = EnrollmentFile {
        version: 1,
        users: vec![
            UserEntry {
                id: "alice".into(),
                aliases: Vec::new(),
                is_admin: false,
                locale: None,
                timezone: None,
            },
            UserEntry {
                id: "bob".into(),
                aliases: Vec::new(),
                is_admin: false,
                locale: None,
                timezone: None,
            },
            UserEntry {
                id: "carol".into(),
                aliases: Vec::new(),
                is_admin: false,
                locale: None,
                timezone: None,
            },
        ],
        groups: vec![GroupEntry {
            id: "famiglia".into(),
            members: vec!["alice".into(), "bob".into()],
            scope: None,
        }],
    };
    mirror_to_db(&state.pool, &enrollment)
        .await
        .expect("mirror enrollment");

    // Plant a three-region wiki on disk. `wikis/alice` already exists
    // implicitly from `WikiTree::open`; create the `_meta.md` and the
    // `index.md` with three markers carrying distinct owners.
    let wiki_dir = dir.path().join("wikis").join("alice");
    std::fs::create_dir_all(&wiki_dir).expect("mkdir alice");
    std::fs::write(
        wiki_dir.join("_meta.md"),
        "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
         slug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
    )
    .expect("write _meta.md");
    // UUIDv7-shaped fact ids — any well-formed value works, the parser
    // only validates the marker grammar here.
    let body = "# Alice\n\n\
                {{owner=global f=01900000-0000-7000-8000-000000000001}}\n\
                Endpoint visible to everyone.\n\
                {{/}}\n\n\
                {{owner=user:alice f=01900000-0000-7000-8000-000000000002}}\n\
                Alice's private decision history.\n\
                {{/}}\n\n\
                {{allow=group:famiglia f=01900000-0000-7000-8000-000000000003}}\n\
                Shared note for the family group.\n\
                {{/}}\n";
    std::fs::write(wiki_dir.join("index.md"), body).expect("write index.md");

    // Re-open the tree so the new wiki is picked up.
    let tree = WikiTree::open(dir.path()).expect("reopen");
    let state = McpState { tree, ..state };

    // ---- alice (owner + group member) sees everything ----
    let out = call(
        &state,
        &alice_identity,
        "wiki_read",
        json!({"wiki_id": "alice"}),
    )
    .await
    .expect("alice wiki_read");
    assert_eq!(out["redacted_count"], json!(0));
    // `fully_redacted` field dropped from the wiki_read response;
    // the absence is the contract.
    assert!(out.get("fully_redacted").is_none());
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("Endpoint visible to everyone."));
    assert!(rendered.contains("Alice's private decision history."));
    assert!(rendered.contains("Shared note for the family group."));
    assert!(!rendered.contains("[redacted]"));

    // ---- bob (group member, not owner) sees global + group-allowed,
    //      misses alice's owner-only region ----
    let bob_identity = IdentityProfile {
        sender_id: "bob".into(),
        ..alice_identity.clone()
    };
    let out = call(
        &state,
        &bob_identity,
        "wiki_read",
        json!({"wiki_id": "alice"}),
    )
    .await
    .expect("bob wiki_read");
    assert_eq!(out["redacted_count"], json!(1));
    // see the alice assertion above for the rationale.
    assert!(out.get("fully_redacted").is_none());
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("Endpoint visible to everyone."));
    assert!(!rendered.contains("Alice's private decision history."));
    assert!(rendered.contains("Shared note for the family group."));
    assert!(rendered.contains("[redacted]"));

    // ---- carol (not owner, not in `famiglia`) sees only the global region ----
    let carol_identity = IdentityProfile {
        sender_id: "carol".into(),
        ..alice_identity
    };
    let out = call(
        &state,
        &carol_identity,
        "wiki_read",
        json!({"wiki_id": "alice"}),
    )
    .await
    .expect("carol wiki_read");
    assert_eq!(out["redacted_count"], json!(2));
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("Endpoint visible to everyone."));
    assert!(!rendered.contains("Alice's private decision history."));
    assert!(!rendered.contains("Shared note for the family group."));
    let redacted_occurrences = rendered.matches("[redacted]").count();
    assert_eq!(redacted_occurrences, 2);
}

/// `wiki_read` serves an arbitrary page via `path` (default `index.md`),
/// projecting the ACL of *that* page — and rejects unsafe / missing pages.
#[tokio::test]
async fn wiki_read_serves_arbitrary_page_with_per_page_acl() {
    let (state, identity, dir) = fixture(false, None).await;

    let wiki_dir = dir.path().join("wikis").join("alice");
    std::fs::create_dir_all(wiki_dir.join("recipes")).expect("mkdir recipes");
    std::fs::write(
        wiki_dir.join("_meta.md"),
        "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
         slug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
    )
    .expect("write _meta.md");
    std::fs::write(wiki_dir.join("index.md"), "# Alice\n\nLanding page.\n")
        .expect("write index.md");
    // A non-index page with one owner-only region — proves the ACL map is
    // resolved for the *page read*, not for `index.md`.
    std::fs::write(
        wiki_dir.join("recipes").join("pasta.md"),
        "# Pasta\n\nFree prose anyone can read.\n\
         {{owner=user:alice f=01900000-0000-7000-8000-0000000000aa}}\n\
         Alice's secret sauce.\n{{/}}\n",
    )
    .expect("write pasta.md");

    let tree = WikiTree::open(dir.path()).expect("reopen");
    let state = McpState { tree, ..state };

    // Default → index.md.
    let out = call(&state, &identity, "wiki_read", json!({"wiki_id": "alice"}))
        .await
        .expect("default read");
    assert_eq!(out["page"], json!("index.md"));
    assert!(
        out["content_rendered_for_sender"]
            .as_str()
            .unwrap()
            .contains("Landing page.")
    );

    // The owner (alice) reads the subpage in full.
    let out = call(
        &state,
        &identity,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "recipes/pasta.md"}),
    )
    .await
    .expect("owner subpage read");
    assert_eq!(out["page"], json!("recipes/pasta.md"));
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("Free prose anyone can read."));
    assert!(rendered.contains("Alice's secret sauce."));
    assert_eq!(out["redacted_count"], json!(0));

    // A non-owner reads the subpage: prose passes, the owner-only region is
    // redacted — i.e. the page's *own* ACL is applied, not index.md's.
    let bob = IdentityProfile {
        sender_id: "bob".into(),
        ..identity.clone()
    };
    let out = call(
        &state,
        &bob,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "recipes/pasta.md"}),
    )
    .await
    .expect("bob subpage read");
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("Free prose anyone can read."));
    assert!(!rendered.contains("Alice's secret sauce."));
    assert_eq!(out["redacted_count"], json!(1));

    // Unsafe path → invalid_input.
    let err = call(
        &state,
        &identity,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "../escape.md"}),
    )
    .await
    .expect_err("unsafe path must reject");
    assert!(err.contains("invalid_input"), "{err}");

    // Missing page → not_found.
    let err = call(
        &state,
        &identity,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "nope.md"}),
    )
    .await
    .expect_err("missing page must reject");
    assert!(err.contains("not_found"), "{err}");
}

/// Regression: `wiki_read` must NOT leak the page frontmatter (testata).
/// The testata's `description` / `keywords.topics` are card metadata derived
/// from the page's facts; they carry no ACL markers, so before the fix
/// `render_for_sender` passed them through verbatim and a reader who could
/// not see a private region still read its topic words in the frontmatter.
#[tokio::test]
async fn wiki_read_strips_frontmatter_so_card_topics_never_leak() {
    let (state, identity, dir) = fixture(false, None).await;

    let wiki_dir = dir.path().join("wikis").join("alice");
    std::fs::create_dir_all(&wiki_dir).expect("mkdir alice");
    std::fs::write(
        wiki_dir.join("_meta.md"),
        "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
         slug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
    )
    .expect("write _meta.md");
    // A page whose testata summarises a PRIVATE fact: the topic word
    // "celiachia" and the description sit in the frontmatter, the fact body
    // is an owner-only region.
    std::fs::write(
        wiki_dir.join("salute.md"),
        "---\ntitle: Salute\ndescription: \"Note di salute di Alice\"\n\
         keywords:\n  topics: celiachia, intolleranze\n---\n\n\
         Alice {{owner=user:alice f=01900000-0000-7000-8000-0000000000bb}}\
         è celiaca{{/}} dal 2020.\n",
    )
    .expect("write salute.md");

    let tree = WikiTree::open(dir.path()).expect("reopen");
    let state = McpState { tree, ..state };

    // A non-owner reads the page: the private region is redacted AND the
    // frontmatter (description + topic words) never reaches the reader.
    let bob = IdentityProfile {
        sender_id: "bob".into(),
        ..identity.clone()
    };
    let out = call(
        &state,
        &bob,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "salute.md"}),
    )
    .await
    .expect("bob read");
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    // Body scaffolding survives; the private fact is redacted.
    assert!(rendered.contains("Alice"), "body prose lost: {rendered}");
    assert!(
        rendered.contains("dal 2020."),
        "body prose lost: {rendered}"
    );
    assert!(
        !rendered.contains("è celiaca"),
        "private region leaked: {rendered}"
    );
    assert_eq!(out["redacted_count"], json!(1));
    // The leak that was the bug: NO frontmatter content reaches the reader.
    assert!(
        !rendered.contains("celiachia"),
        "topic word leaked: {rendered}"
    );
    assert!(
        !rendered.contains("intolleranze"),
        "topic word leaked: {rendered}"
    );
    assert!(
        !rendered.contains("Note di salute"),
        "description leaked: {rendered}"
    );
    assert!(
        !rendered.contains("---"),
        "frontmatter fence leaked: {rendered}"
    );

    // The owner reads the same page: the fact is visible, but the raw
    // frontmatter is still stripped (title/owner ride the structured JSON).
    let out = call(
        &state,
        &identity,
        "wiki_read",
        json!({"wiki_id": "alice", "path": "salute.md"}),
    )
    .await
    .expect("alice read");
    let rendered = out["content_rendered_for_sender"].as_str().unwrap();
    assert!(rendered.contains("è celiaca"), "owner must see the fact");
    assert!(
        !rendered.contains("keywords:"),
        "frontmatter leaked to owner: {rendered}"
    );
    assert!(
        !rendered.contains("celiachia"),
        "frontmatter leaked to owner: {rendered}"
    );
}

#[tokio::test]
async fn wiki_search_runs_against_empty_corpus() {
    let (state, identity, _dir) = fixture(false, None).await;
    let out = call(&state, &identity, "wiki_search", json!({"query": "x"}))
        .await
        .expect("search");
    assert!(out["results"].as_array().unwrap().is_empty());
    assert_eq!(out["total"], json!(0));
}

/// `wiki_navigate` with no `navigator` LLM slot wired (the fixture default)
/// degrades to flat-only — never an error, `navigator_available: false`, an
/// empty navigated path, and the flat hits (empty corpus here).
#[tokio::test]
async fn wiki_navigate_degrades_to_flat_only_without_a_navigator() {
    let (state, identity, _dir) = fixture(false, None).await;
    let out = call(
        &state,
        &identity,
        "wiki_navigate",
        json!({"query": "anything"}),
    )
    .await
    .expect("navigate runs");
    assert_eq!(out["navigator_available"], json!(false));
    assert_eq!(out["navigated"], json!([]));
    assert_eq!(out["hops"], json!(0));
    assert!(out["flat"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn tool_log_search_non_admin_scoped_to_own_sender() {
    let (state, identity, _dir) = fixture(false, None).await;
    // Plant rows for two senders.
    for sender in ["alice", "samvise"] {
        record(
            &state.pool,
            &ToolExecutionInput {
                tool_name: "wiki_search",
                sender_id: sender,
                device_label: "x",
                rate_limit_id: None,
                args_hash: None,
                result_summary: None,
                latency_ms: 1,
                cost_estimate: None,
                error: None,
            },
        )
        .await
        .unwrap();
    }
    let out = call(&state, &identity, "tool_log_search", json!({}))
        .await
        .expect("search");
    let entries = out["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["sender_id"], json!("alice"));
}

#[tokio::test]
async fn tool_log_search_admin_sees_every_sender() {
    let (state, identity, _dir) = fixture(true, None).await;
    for sender in ["alice", "samvise"] {
        record(
            &state.pool,
            &ToolExecutionInput {
                tool_name: "wiki_search",
                sender_id: sender,
                device_label: "x",
                rate_limit_id: None,
                args_hash: None,
                result_summary: None,
                latency_ms: 1,
                cost_estimate: None,
                error: None,
            },
        )
        .await
        .unwrap();
    }
    let out = call(&state, &identity, "tool_log_search", json!({}))
        .await
        .expect("search");
    assert_eq!(out["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn wiki_lint_runs_clean_on_empty_tree() {
    let (state, identity, _dir) = fixture(false, None).await;
    let out = call(&state, &identity, "wiki_lint", json!({}))
        .await
        .expect("lint");
    assert_eq!(out["summary"]["total"], json!(0));
}

#[tokio::test]
async fn wiki_ingest_external_inline_enqueues_idempotent_job() {
    // Enqueue itself never calls the LLM (the worker does), but the
    // handler refuses without a configured ingest slot — wire the fake.
    let (state, identity, _dir) = fixture_with_llm(false, None, fake_ingest_llm_config("{}")).await;
    let out = call(
        &state,
        &identity,
        "wiki_ingest_external",
        json!({"source": {"type": "inline", "content": "# hello world\n"}}),
    )
    .await
    .expect("ingest");
    assert_eq!(out["status"], json!("queued"));
    let job_id = out["job_id"].as_str().expect("job_id").to_owned();
    // Same text, same owner → the idempotency hit returns the prior job.
    let again = call(
        &state,
        &identity,
        "wiki_ingest_external",
        json!({"source": {"type": "inline", "content": "# hello world\n"}}),
    )
    .await
    .expect("ingest again");
    assert_eq!(again["status"], json!("existing"));
    assert_eq!(again["job_id"], json!(job_id));
}

#[tokio::test]
async fn wiki_ingest_external_rejects_unknown_disposition() {
    let (state, identity, _dir) = fixture_with_llm(false, None, fake_ingest_llm_config("{}")).await;
    let err = call(
        &state,
        &identity,
        "wiki_ingest_external",
        json!({"source": {"type": "inline", "content": "x"}, "disposition": "shred"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("invalid_input"), "{err}");
}

#[tokio::test]
async fn wiki_ingest_external_rejects_file_source() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "wiki_ingest_external",
        json!({"source": {"type": "file", "path": "/etc/passwd"}}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("not_implemented_phase_c"), "{err}");
}

#[tokio::test]
async fn dashboard_link_home_returns_signed_url() {
    let (state, identity, _dir) = fixture(false, None).await;
    let out = call(
        &state,
        &identity,
        "dashboard_link",
        json!({"intent": "home"}),
    )
    .await
    .expect("link");
    let url = out["url"].as_str().unwrap();
    // 0032: the link now targets the single-use redemption endpoint,
    // carrying the deep-link in `next` (url-encoded).
    assert!(url.starts_with("/dashboard/auth/link?token="), "{url}");
    assert!(url.contains("&next=%2Fdashboard%2Fhome"), "{url}");
    assert!(out["token_expires_at"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn dashboard_link_admin_only_intents_gated() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "dashboard_link",
        json!({"intent": "settings"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("sender_unauthorized"), "{err}");
    let (state, identity, _dir) = fixture(true, None).await;
    let out = call(
        &state,
        &identity,
        "dashboard_link",
        json!({"intent": "settings"}),
    )
    .await
    .expect("ok");
    let url = out["url"].as_str().unwrap();
    assert!(url.starts_with("/dashboard/auth/link?token="), "{url}");
    assert!(url.contains("&next=%2Fdashboard%2Fsettings"), "{url}");
}

#[tokio::test]
async fn dashboard_link_modify_wiki_requires_context_wiki_id() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "dashboard_link",
        json!({"intent": "modify_wiki"}),
    )
    .await
    .expect_err("must reject");
    assert!(err.contains("context.wiki_id"), "{err}");
}

// ---- wiki_ingest_message surfaces a `pending_attention` block ----
//
// When at least one `structure_proposals` row is in flight (`pending`
// or `applied_pending_confirm`), the response from `wiki_ingest_message`
// carries an extra `pending_attention` block the consumer agent uses
// to nudge the user toward the dashboard. The default wire shape stays
// quiet (no block) when the count is zero, so existing consumers do not
// see noise on every turn.

/// Build an [`LlmConfig`] whose `ingest` slot points at the
/// `test-fakes`-only `"fake"` backend with the given canned response.
fn fake_ingest_llm_config(canned_response: &str) -> LlmConfig {
    LlmConfig {
        ingest: Some(LlmFunctionConfig {
            backend: "fake".into(),
            model: canned_response.into(),
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        }),
        ..LlmConfig::default()
    }
}

/// Seed a `structure_proposals` row with the given lifecycle status.
async fn seed_proposal_row(state: &McpState, proposal_id: &str, status: &str) {
    let now = chrono::Utc::now();
    let timeout = now + chrono::Duration::hours(24);
    sqlx::query(
        "INSERT INTO structure_proposals (proposal_id, kind, context, questions, \
         proposed_at, timeout_at, status) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(proposal_id)
    .bind("dedup_merge")
    .bind(r#"{"intent":"test"}"#)
    .bind(r#"[{"id":"q1","text":"do it?","options":[]}]"#)
    .bind(now.to_rfc3339())
    .bind(timeout.to_rfc3339())
    .bind(status)
    .execute(&state.pool)
    .await
    .expect("seed proposal");
}

#[tokio::test]
async fn wiki_ingest_message_omits_pending_attention_when_no_proposals_in_flight() {
    let (state, identity, _dir) =
        fixture_with_llm(false, None, fake_ingest_llm_config(r#"{"intent":"skip"}"#)).await;
    let out = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "hello there"}),
    )
    .await
    .expect("ingest must succeed with fake llm");
    assert_eq!(out["intent_classified"], json!("skip"));
    assert!(
        out.get("pending_attention").is_none(),
        "no in-flight proposals ⇒ block must be absent, got: {out}",
    );
}

/// Guest wire shape (roadmap 40): the turn succeeds, the `rules` channel
/// carries the reserved-behaviour directive, nothing is filed, and the
/// governance blocks stay absent even with a proposal in flight — the
/// canned CAPTURE plan proves the classifier's answer is never consulted.
#[tokio::test]
async fn wiki_ingest_message_guest_turn_is_ephemeral_on_the_wire() {
    let (state, mut identity, _dir) = fixture_with_llm(
        false,
        Some("sam-bot"),
        fake_ingest_llm_config(
            r#"{"intent":"capture","extractions":[{"target_wiki_id":"alice",
                "target_page":"note.md","owner_id":"user:alice","fact_type":"other",
                "body":"must never file","topics":[]}]}"#,
        ),
    )
    .await;
    identity.sender_id = "guest".into();
    seed_proposal_row(&state, "p-guest-1", "pending").await;

    let out = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "a che ora chiude la farmacia?"}),
    )
    .await
    .expect("guest ingest succeeds");
    assert_eq!(out["intent_classified"], json!("skip"));
    assert_eq!(out["llm_used"], json!(false), "classifier skipped");
    assert_eq!(out["capture_id"], json!(null), "nothing filed");
    let rules = out["rules"].as_str().expect("guest directive present");
    assert!(rules.contains("UNIDENTIFIED SPEAKER"), "got: {rules}");
    assert!(
        out.get("pending_attention").is_none(),
        "governance nudges are for enrolled members, got: {out}",
    );
    assert!(out.get("pending_votes").is_none(), "got: {out}");
}

// ---------- wiki_admin_push.mark_processed ----------

/// Smart-consumer fixture: extends [`fixture`] with `consumer_class =
/// Smart`, seeds the bundled `wiki_type` registry + an identity wiki
/// for `alice` so a downstream `wiki_admin_push` can use the chassis
/// directly. Returns the freshly created smart wiki id and the
/// underlying state for arg construction.
async fn smart_fixture_with_smart_wiki() -> (
    McpState,
    IdentityProfile,
    tempfile::TempDir,
    mwe_core::types::WikiId,
) {
    use mwe_core::types::WikiId;
    use mwe_core::wiki::{IdentityKind, create_identity_wiki};
    use mwe_core::wiki_admin::{ActorKind, AdminCaller, PushMode, PushPage, PushRequest, push};

    let (state, mut identity, dir) = fixture(false, Some("cc-laptop")).await;
    identity.consumer_class = mwe_core::jwt::ConsumerClass::Smart;
    // Alice identity wiki — the chassis needs it to create a
    // smart-wiki on `create`.
    let alice_id = WikiId::parse("alice").unwrap();
    create_identity_wiki(&state.tree, &alice_id, "Alice", IdentityKind::User)
        .expect("alice identity wiki");

    // Drive the core push to create a real smart-wiki the smart
    // consumer can target. Skipping MCP for the setup keeps the test
    // body focused on `mark_processed` wire behaviour rather than on
    // wiki forging.
    let caller = AdminCaller {
        sender_id: "alice".into(),
        consumer_id: Some("cc-laptop".into()),
        consumer_class: mwe_core::jwt::ConsumerClass::Smart,
    };
    let req = PushRequest {
        mode: PushMode::Create,
        wiki_id: None,
        parent_wiki_id: Some(alice_id.clone()),
        slug: Some("lnprint".into()),
        title: Some("lnprint companion".into()),
        wiki_type: Some("wiki-companion".into()),
        smart: true,
        project_id: None,
        pages: vec![PushPage {
            path: "index.md".into(),
            content: "# lnprint\n".into(),
        }],
        deletes: Vec::new(),
        mark_processed: Vec::new(),
        expected_op_log_head: None,
    };
    let resp = push(
        &state.pool,
        &state.tree,
        &caller,
        ActorKind::SmartConsumer,
        req,
    )
    .await
    .expect("seed smart wiki");
    (state, identity, dir, resp.wiki_id)
}

async fn seed_briefing_row(pool: &sqlx::SqlitePool, wiki_id: &str, body: &str) -> i64 {
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_briefing_items
            (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, author_sender_id, processed_at)
         VALUES (?, 'dashboard_comment', 'dashboard:alice', 'seed', ?, NULL, ?, NULL, 'alice', NULL)
         RETURNING id",
    )
    .bind(wiki_id)
    .bind(body)
    .bind(&now)
    .fetch_one(pool)
    .await
    .expect("seed briefing");
    row.0
}

#[tokio::test]
async fn wiki_admin_push_accepts_mark_processed_field_and_surfaces_marked_in_output() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;
    let bi = seed_briefing_row(&state.pool, wiki_id.as_str(), "address me").await;

    let out = call(
        &state,
        &identity,
        "wiki_admin_push",
        json!({
            "mode": "upsert",
            "wiki_id": wiki_id.as_str(),
            "pages": [{"path": "index.md", "content": "# lnprint v2\n"}],
            "mark_processed": [format!("bi_{bi}")],
        }),
    )
    .await
    .expect("push with mark_processed must succeed");

    assert_eq!(
        out["marked_processed"],
        json!([format!("bi_{bi}")]),
        "wire response must echo the marked ids"
    );
    let processed_at: Option<String> =
        sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
            .bind(bi)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(
        processed_at.is_some(),
        "row must be flipped to processed by the dispatcher path"
    );
}

#[tokio::test]
async fn wiki_admin_push_queues_section_indexing_when_reindex_channel_is_wired() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let state = McpState {
        reindex_tx: Some(tx),
        ..state
    };

    let out = call(
        &state,
        &identity,
        "wiki_admin_push",
        json!({
            "mode": "upsert",
            "wiki_id": wiki_id.as_str(),
            "pages": [{"path": "notes.md", "content": "# Notes\n"}],
        }),
    )
    .await
    .expect("push must succeed");

    assert_eq!(
        out["section_indexing"],
        json!("queued"),
        "with a wired reindex channel the ack must not embed inline"
    );
    match rx.try_recv().expect("one queued change") {
        mwe_core::watcher::WatchedChange::Touched(p) => {
            assert!(p.ends_with("notes.md"), "queued path: {}", p.display());
        },
        other => panic!("expected Touched, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "exactly one page was pushed");
}

#[tokio::test]
async fn wiki_admin_push_indexes_inline_without_reindex_channel() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(
        &state,
        &identity,
        "wiki_admin_push",
        json!({
            "mode": "upsert",
            "wiki_id": wiki_id.as_str(),
            "pages": [{"path": "notes.md", "content": "# Notes\n"}],
        }),
    )
    .await
    .expect("push must succeed");
    assert_eq!(
        out["section_indexing"],
        json!("inline"),
        "no channel (tests, degraded boot) → synchronous indexing as before"
    );
}

#[tokio::test]
async fn wiki_admin_push_returns_unknown_briefing_item_id_error_class() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;

    let err = call(
        &state,
        &identity,
        "wiki_admin_push",
        json!({
            "mode": "upsert",
            "wiki_id": wiki_id.as_str(),
            "pages": [{"path": "index.md", "content": "# never written\n"}],
            "mark_processed": ["bi_99999"],
        }),
    )
    .await
    .expect_err("unknown bi_id must abort with a wire error");
    assert!(
        err.contains("unknown_briefing_item_id"),
        "expected the wire class, got: {err}"
    );
}

#[tokio::test]
async fn structure_proposal_in_flight_surfaces_warning_in_wiki_ingest_message_response() {
    let (state, identity, _dir) =
        fixture_with_llm(false, None, fake_ingest_llm_config(r#"{"intent":"skip"}"#)).await;

    // Seed three in-flight rows (2 pending + 1 applied_pending_confirm)
    // plus terminal rows that must not be counted.
    seed_proposal_row(&state, "p-pending-1", "pending").await;
    seed_proposal_row(&state, "p-pending-2", "pending").await;
    seed_proposal_row(&state, "p-apc-1", "applied_pending_confirm").await;
    seed_proposal_row(&state, "p-applied", "applied").await;
    seed_proposal_row(&state, "p-reverted", "reverted").await;
    seed_proposal_row(&state, "p-expired", "expired").await;

    let out = call(
        &state,
        &identity,
        "wiki_ingest_message",
        json!({"text": "anything"}),
    )
    .await
    .expect("ingest must succeed with fake llm");

    let block = out
        .get("pending_attention")
        .expect("pending_attention block must be present when count > 0");
    // The seeded rows carry a NULL recipient (the admin-fallback bucket),
    // so they count for any caller; only the `note` reflects the 0032
    // recipient scoping now.
    assert_eq!(block["pending_count"], json!(2));
    assert_eq!(block["applied_pending_confirm_count"], json!(1));
    assert_eq!(block["dashboard_path"], json!("/dashboard/proposals"));
    assert_eq!(block["note"], json!("scoped_to_recipient"));
}

// ===== K family =====

#[tokio::test]
async fn smart_bootstrap_rejects_standard_token_with_smart_class_wire_error() {
    let (state, identity, _dir) = fixture(false, None).await;
    // Default fixture is Standard class; bootstrap must refuse.
    let err = call(&state, &identity, "smart_bootstrap", json!({}))
        .await
        .expect_err("standard token must be refused");
    assert!(
        err.contains("requires_consumer_class_smart"),
        "expected smart-class wire code, got: {err}"
    );
}

#[tokio::test]
async fn smart_bootstrap_surfaces_caller_owned_smart_wikis() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(&state, &identity, "smart_bootstrap", json!({}))
        .await
        .expect("smart bootstrap should succeed for a smart caller");
    assert_eq!(out["caller_sender_id"], json!("alice"));
    let wikis = out["smart_wikis"].as_array().expect("smart_wikis");
    assert_eq!(wikis.len(), 1);
    assert_eq!(wikis[0]["wiki_id"], json!(wiki_id.as_str()));
    assert_eq!(wikis[0]["wiki_type"], json!("wiki-companion"));
    // Briefing buckets present even with zero rows.
    let counts = &wikis[0]["briefing_counts"];
    assert_eq!(counts["total"], json!(0));
}

#[tokio::test]
async fn smart_bootstrap_omits_first_connect_without_a_project_id() {
    let (state, identity, _dir, _wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(&state, &identity, "smart_bootstrap", json!({}))
        .await
        .expect("smart bootstrap");
    // A transversal session asks nothing about a project and is told
    // nothing about onboarding one.
    assert_eq!(out["first_connect"], json!(null));
}

#[tokio::test]
async fn smart_bootstrap_volunteers_first_connect_for_an_unknown_project() {
    let (state, identity, _dir, _wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(
        &state,
        &identity,
        "smart_bootstrap",
        json!({ "project_id": "18a486b5c823a33f" }),
    )
    .await
    .expect("smart bootstrap");
    let fc = &out["first_connect"];
    assert_eq!(fc["project_id"], json!("18a486b5c823a33f"));
    assert_eq!(fc["wiki_found"], json!(false));
    assert_eq!(fc["wiki_id"], json!(null));
    let hint = fc["hint"].as_str().expect("hint volunteered on the wire");
    assert!(
        hint.contains("smart-onboarding"),
        "the hint must name the skill carrying the procedure: {hint}"
    );
}

#[tokio::test]
async fn wiki_admin_pull_shape_mode_returns_counters_and_no_content() {
    let (state, identity, _dir, wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(
        &state,
        &identity,
        "wiki_admin_pull",
        json!({ "wiki_id": wiki_id.as_str(), "shape": true }),
    )
    .await
    .expect("shape pull");
    assert_eq!(out["shape_summary"]["pages"], json!(1));
    assert_eq!(out["shape_summary"]["pages_needing_repair"], json!(0));
    let page = &out["pages"][0];
    assert_eq!(page["path"], json!("index.md"));
    assert!(page["content"].is_null(), "shape mode must not ship bytes");
    assert_eq!(page["shape"]["needs_repair"], json!(false));
    assert_eq!(page["shape"]["oversize_blocks"], json!(0));
    assert!(page["shape"]["sections"].as_u64().is_some());
}

#[tokio::test]
async fn recall_core_global_rejects_standard_token_with_smart_class_wire_error() {
    let (state, identity, _dir) = fixture(false, None).await;
    let err = call(
        &state,
        &identity,
        "recall_core_global",
        json!({"query": "anything"}),
    )
    .await
    .expect_err("standard token must be refused");
    assert!(
        err.contains("requires_consumer_class_smart"),
        "expected smart-class wire code, got: {err}"
    );
}

#[tokio::test]
async fn recall_core_global_returns_empty_hits_with_filter_echo_for_smart_caller() {
    let (state, identity, _dir, _wiki_id) = smart_fixture_with_smart_wiki().await;
    let out = call(
        &state,
        &identity,
        "recall_core_global",
        json!({"query": "anything"}),
    )
    .await
    .expect("smart caller, empty index, should succeed with no hits");
    assert_eq!(out["query"], json!("anything"));
    assert_eq!(out["filter_applied"]["owner_user"], json!("alice"));
    let excluded = out["filter_applied"]["excluded_wiki_types"]
        .as_array()
        .expect("excluded_wiki_types array");
    assert!(
        excluded.iter().any(|v| v == &json!("wiki-companion")),
        "excluded stems must include the smart-family stem we just registered: {excluded:?}"
    );
    let hits = out["hits"].as_array().expect("hits array");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn recall_core_global_rejects_empty_query_with_invalid_input() {
    let (state, identity, _dir, _wiki_id) = smart_fixture_with_smart_wiki().await;
    let err = call(
        &state,
        &identity,
        "recall_core_global",
        json!({"query": "   "}),
    )
    .await
    .expect_err("empty query must surface invalid_input");
    assert!(err.contains("invalid_input"), "got: {err}");
}

// ===== L family — wiki_forget (authority-routed) =====

/// Seed the dispatcher fixture for the `wiki_forget` routing tests: a
/// `famiglia` group = {alice, bob, carol} enrolled, with a `famiglia`
/// group-wiki on disk so a group-owned fact's audience resolves.
async fn forget_fixture() -> (McpState, IdentityProfile, tempfile::TempDir) {
    let (state, identity, dir) = fixture(false, None).await;
    mirror_to_db(
        &state.pool,
        &EnrollmentFile {
            version: 1,
            users: vec![
                UserEntry {
                    id: "alice".into(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                },
                UserEntry {
                    id: "bob".into(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                },
                UserEntry {
                    id: "carol".into(),
                    aliases: Vec::new(),
                    is_admin: false,
                    locale: None,
                    timezone: None,
                },
            ],
            groups: vec![GroupEntry {
                id: "famiglia".into(),
                members: vec!["alice".into(), "bob".into(), "carol".into()],
                scope: None,
            }],
        },
    )
    .await
    .expect("mirror enrollment");

    // A `famiglia` group-wiki on disk so a `group:famiglia`-owned fact has a
    // home wiki the redaction/audience path can resolve.
    let fam_dir = dir.path().join("wikis").join("famiglia");
    std::fs::create_dir_all(&fam_dir).expect("mkdir famiglia");
    std::fs::write(
        fam_dir.join("_meta.md"),
        "---\nwiki_id: famiglia\nwiki_type: wiki-group\nparent_wiki_id: null\n\
         slug: famiglia\ntitle: famiglia\nacl_default: 'group:famiglia'\n---\n",
    )
    .expect("write _meta.md");
    let tree = WikiTree::open(dir.path()).expect("reopen");
    let state = McpState { tree, ..state };
    (state, identity, dir)
}

/// Insert one fact on `famiglia/vacanze.md` with explicit owner/allow/sender,
/// returning its id string. `uuid_tail` makes each test's fact distinct.
async fn insert_forget_fact(
    pool: &sqlx::SqlitePool,
    uuid_tail: &str,
    owner: &str,
    allow: &[&str],
    sender: Option<&str>,
) -> String {
    use mwe_core::fact_index::{self, NewFact};
    let fact_id =
        mwe_core::types::FactId::parse(&format!("0190a0c8-0000-7000-8000-0000000000{uuid_tail}"))
            .expect("fact id");
    fact_index::insert(
        pool,
        &NewFact {
            authored_refs: Vec::new(),
            fact_id: fact_id.clone(),
            wiki_id: "famiglia".to_owned(),
            source_path: "wikis/famiglia/vacanze.md".to_owned(),
            region_start: Some(0),
            region_end: Some(32),
            text: "shared family fact".to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            owner_id: owner.parse().unwrap(),
            allow_ids: allow.iter().map(|a| a.parse().unwrap()).collect(),
            sender_id: sender.map(|s| s.parse().unwrap()),
            fact_type: None,
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        },
    )
    .await
    .expect("insert fact");
    fact_id.as_str().to_owned()
}

async fn is_forgotten(pool: &sqlx::SqlitePool, fact_id: &str) -> bool {
    let id = mwe_core::types::FactId::parse(fact_id).unwrap();
    mwe_core::fact_index::find_by_id(pool, &id)
        .await
        .expect("find")
        .expect("row")
        .deleted_at
        .is_some()
}

/// The author (sender) forgets their own fact → tombstoned immediately.
#[tokio::test]
async fn wiki_forget_author_tombstones_own_fact() {
    let (state, identity, _dir) = forget_fixture().await;
    // alice is the sender → direct delete.
    let fid = insert_forget_fact(
        &state.pool,
        "01",
        "user:alice",
        &["group:famiglia"],
        Some("user:alice"),
    )
    .await;
    let out = call(&state, &identity, "wiki_forget", json!({"fact_id": fid}))
        .await
        .expect("author forgets own fact");
    assert_eq!(out["outcome"], json!("forgotten"));
    assert_eq!(out["fact_id"], json!(fid));
    assert!(
        is_forgotten(&state.pool, &fid).await,
        "fact must be tombstoned"
    );

    // Forgetting it again is an idempotent success, not an error.
    let again = call(&state, &identity, "wiki_forget", json!({"fact_id": fid}))
        .await
        .expect("idempotent re-forget");
    assert_eq!(again["outcome"], json!("already_forgotten"));
}

/// A non-author owner (alice owns the fact bob authored) is **not** allowed to
/// open a vote from the consumer MCP — the vote is dashboard-only. The tool
/// steers them to the dashboard and opens **no** proposal in the background.
#[tokio::test]
async fn wiki_forget_non_author_owner_is_pointed_to_dashboard() {
    let (state, identity, _dir) = forget_fixture().await;
    // owner=alice, sender=bob, allow=famiglia → alice is owner-not-author.
    let fid = insert_forget_fact(
        &state.pool,
        "02",
        "user:alice",
        &["group:famiglia"],
        Some("user:bob"),
    )
    .await;
    let out = call(&state, &identity, "wiki_forget", json!({"fact_id": fid}))
        .await
        .expect("owner steered to dashboard");
    assert_eq!(out["outcome"], json!("request_from_dashboard"));
    assert!(out["detail"].is_string(), "carries a steer for the agent");
    assert!(
        !is_forgotten(&state.pool, &fid).await,
        "the fact is untouched"
    );
    // Crucially: NO vote was started in the background by the agent.
    let proposals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM structure_proposals WHERE kind = 'fact_forget'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(proposals, 0, "no fact_forget proposal opened from MCP");
}

/// An unrelated caller (neither author, owner, nor owning-group member) is
/// refused with `sender_unauthorized`.
#[tokio::test]
async fn wiki_forget_unrelated_caller_is_refused() {
    let (state, base, _dir) = forget_fixture().await;
    // owner=bob (a user, not a group), sender=carol → alice is in neither
    // role. The fixture identity is already alice, but spell it out for clarity.
    let fid = insert_forget_fact(&state.pool, "03", "user:bob", &[], Some("user:carol")).await;
    let alice = IdentityProfile {
        sender_id: "alice".into(),
        ..base
    };
    let err = call(&state, &alice, "wiki_forget", json!({"fact_id": fid}))
        .await
        .expect_err("unrelated caller must be refused");
    assert!(err.contains("sender_unauthorized"), "{err}");
    assert!(
        !is_forgotten(&state.pool, &fid).await,
        "a refused forget leaves the fact untouched"
    );
}

/// An unknown fact id is `not_found`; a malformed id is `invalid_input`.
#[tokio::test]
async fn wiki_forget_unknown_and_malformed_fact_id() {
    let (state, identity, _dir) = forget_fixture().await;
    let err = call(
        &state,
        &identity,
        "wiki_forget",
        json!({"fact_id": "0190a0c8-0000-7000-8000-0000000000ff"}),
    )
    .await
    .expect_err("unknown fact must be not_found");
    assert!(err.contains("not_found"), "{err}");

    let err = call(
        &state,
        &identity,
        "wiki_forget",
        json!({"fact_id": "not-a-uuid"}),
    )
    .await
    .expect_err("malformed id must be invalid_input");
    assert!(err.contains("invalid_input"), "{err}");
}

/// Bulk self-delete, scope "all": tombstones every fact the caller authored,
/// never another author's.
#[tokio::test]
async fn wiki_forget_bulk_all_clears_only_callers_facts() {
    let (state, identity, _dir) = forget_fixture().await;
    // alice (the fixture identity) authored two facts; bob authored one.
    let a1 = insert_forget_fact(
        &state.pool,
        "11",
        "user:alice",
        &["group:famiglia"],
        Some("user:alice"),
    )
    .await;
    let a2 = insert_forget_fact(
        &state.pool,
        "12",
        "user:alice",
        &["group:famiglia"],
        Some("user:alice"),
    )
    .await;
    let b1 = insert_forget_fact(
        &state.pool,
        "13",
        "user:bob",
        &["group:famiglia"],
        Some("user:bob"),
    )
    .await;

    let out = call(
        &state,
        &identity,
        "wiki_forget_bulk",
        json!({"scope": "all"}),
    )
    .await
    .expect("bulk all");
    assert_eq!(out["outcome"], json!("forgotten_bulk"));
    assert_eq!(out["forgotten"], json!(2), "both alice facts, not bob's");
    assert!(is_forgotten(&state.pool, &a1).await);
    assert!(is_forgotten(&state.pool, &a2).await);
    assert!(
        !is_forgotten(&state.pool, &b1).await,
        "another author's fact is never reached"
    );
}

/// Bulk self-delete, scope "page": bounded to one page; bad/missing args are
/// `invalid_input`.
#[tokio::test]
async fn wiki_forget_bulk_page_scope_and_validation() {
    let (state, identity, _dir) = forget_fixture().await;
    // alice's fact on the fixture page famiglia/vacanze.md.
    let a = insert_forget_fact(&state.pool, "21", "user:alice", &[], Some("user:alice")).await;

    let out = call(
        &state,
        &identity,
        "wiki_forget_bulk",
        json!({"scope": "page", "wiki_id": "famiglia", "page": "vacanze.md"}),
    )
    .await
    .expect("bulk page");
    assert_eq!(out["forgotten"], json!(1));
    assert!(is_forgotten(&state.pool, &a).await);

    // scope "page" without the ids → invalid_input.
    let err = call(
        &state,
        &identity,
        "wiki_forget_bulk",
        json!({"scope": "page"}),
    )
    .await
    .expect_err("page needs wiki_id + page");
    assert!(err.contains("invalid_input"), "{err}");

    // an unknown scope → invalid_input.
    let err = call(
        &state,
        &identity,
        "wiki_forget_bulk",
        json!({"scope": "everything"}),
    )
    .await
    .expect_err("bad scope");
    assert!(err.contains("invalid_input"), "{err}");
}
