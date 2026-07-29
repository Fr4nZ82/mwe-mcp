// SPDX-License-Identifier: AGPL-3.0-or-later
//! `instance.read_only` on the dashboard tree.
//!
//! Three properties, and they pull against each other, which is why they
//! are tested together:
//!
//! 1. Every request that would change memory or configuration is refused
//!    — including the public half of the tree (`/setup`,
//!    `/accept-invite`, `/reset-password`), which sits outside the
//!    session layer and would otherwise be the way in.
//! 2. Identity keeps working. Signing in and signing out are writes by
//!    nature, and reading the same page as one person and then as
//!    another is the whole reason to show an instance to anybody.
//! 3. The controls that are refused are not rendered. A button that
//!    errors in front of a stranger is worse than a button that is not
//!    there — but hiding alone would be a curtain, so (1) is asserted
//!    first and by path, not by looking at the HTML.

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
use mwe_dashboard::{DashboardConfig, DashboardState, MemoryHandles, router};

/// A dashboard that is (or is not) frozen. Memory handles are wired so
/// the routes that need a wiki tree behave as they do in production.
async fn make_app(read_only: bool) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let memory = MemoryHandles {
        tree,
        embedder: Arc::new(FakeEmbedder::new("fake-bge-m3", 8)),
        llm_config: Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        workdir: dir.path().to_path_buf(),
    };
    let state = DashboardState::new(pool, secret, blacklist, delegations)
        .with_config(DashboardConfig {
            read_only,
            ..DashboardConfig::default()
        })
        .with_memory(memory);
    (router(state), dir)
}

/// Create the admin. Must run against an **unfrozen** router: `/setup`
/// is a configuration write and a frozen instance refuses it, which is
/// itself one of the assertions below.
async fn setup_admin(app: &Router) -> String {
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

/// A frozen router sharing an already-seeded workdir, so the tests below
/// have an admin to sign in as without going through `/setup` twice.
async fn frozen_app_with_admin() -> (Router, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let memory = MemoryHandles {
        tree,
        embedder: Arc::new(FakeEmbedder::new("fake-bge-m3", 8)),
        llm_config: Arc::new(std::sync::RwLock::new(LlmConfig::default())),
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        workdir: dir.path().to_path_buf(),
    };
    let base = DashboardState::new(pool, secret, blacklist, delegations).with_memory(memory);

    // Seed through an open router, then rebuild the same state frozen.
    let open = router(base.clone());
    let cookie = setup_admin(&open).await;

    let frozen = base.with_config(DashboardConfig {
        read_only: true,
        ..DashboardConfig::default()
    });
    (router(frozen), cookie, dir)
}

async fn post(app: &Router, uri: &str, cookie: &str, body: &'static str) -> StatusCode {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .status()
}

/// Property 1, by path rather than by page: the write surface is shut,
/// on both halves of the tree.
#[tokio::test]
async fn a_frozen_instance_refuses_memory_and_configuration_writes() {
    let (app, cookie, _dir) = frozen_app_with_admin().await;

    // Memory.
    for (uri, body) in [
        ("/chat", "text=remember+that+I+moved+house"),
        ("/wiki/alice/comment/index.md", "text=note"),
        ("/wiki/alice/delete", ""),
        ("/facts/0197fa00-0000-7000-8000-000000000001/delete", ""),
        ("/dream/light", ""),
        ("/welcome", "step=1"),
    ] {
        assert_eq!(
            post(&app, uri, &cookie, body).await,
            StatusCode::FORBIDDEN,
            "POST {uri} changes memory and must be refused"
        );
    }

    // Configuration — including the *public* half, which sits outside
    // the session layer and is reached with no cookie at all.
    for (uri, body) in [
        ("/users/new", "user_id=mallory"),
        ("/tokens/issue", "sender_id=alice"),
        ("/prompts/cronista", "body=x"),
        ("/admin/backup/reset", "confirm=RESET"),
        ("/settings/require-2fa", "require_2fa_all=1"),
        ("/settings/me", "current_password=x&new_password=y"),
    ] {
        assert_eq!(
            post(&app, uri, &cookie, body).await,
            StatusCode::FORBIDDEN,
            "POST {uri} changes configuration and must be refused"
        );
    }
    for (uri, body) in [
        ("/setup", "email=m@example.com&admin_id=mallory"),
        ("/forgot-password", "email=alice@example.com"),
        ("/reset-password/deadbeef", "new_password=x"),
    ] {
        assert_eq!(
            post(&app, uri, "", body).await,
            StatusCode::FORBIDDEN,
            "POST {uri} is public and mints an identity — it must be refused too"
        );
    }
}

/// The baseline. Every one of those paths behaves differently on an
/// unfrozen instance, so the assertions above cannot be passing because
/// the routes are missing or the bodies are malformed — a `403` from the
/// guard is not the same thing as a `404` or a validation bounce.
#[tokio::test]
async fn the_same_writes_are_not_forbidden_when_the_instance_is_open() {
    let (app, _dir) = make_app(false).await;
    let cookie = setup_admin(&app).await;

    for (uri, body) in [
        ("/users/new", "user_id=mallory"),
        ("/settings/require-2fa", "require_2fa_all=1"),
        ("/settings/me", "current_password=x&new_password=y"),
        ("/dream/light", ""),
    ] {
        let status = post(&app, uri, &cookie, body).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "POST {uri} must not be forbidden on an open instance (got {status})"
        );
    }
}

/// Property 2: identity survives the freeze. Signing in and out are the
/// only writes a frozen instance still accepts, because switching who
/// you are looking as is the point of showing it.
#[tokio::test]
async fn identity_still_works_on_a_frozen_instance() {
    let (app, cookie, _dir) = frozen_app_with_admin().await;

    // The session minted before the freeze still reads pages.
    let response = send(
        &app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Signing out works…
    assert!(
        !matches!(
            post(&app, "/logout", &cookie, "").await,
            StatusCode::FORBIDDEN
        ),
        "logout must not be refused"
    );

    // …and signing back in mints a fresh session.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=alice@example.com&password=correct-horse-battery",
            ))
            .unwrap(),
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "login must work on a frozen instance: {}",
        response.status()
    );
    assert!(extract_set_cookie(&response, "mwe_session").is_some());
}

/// Property 3: a frozen instance shows the **whole** product and lets
/// none of it fire.
///
/// This one inverted. The consoles used to be left unmounted, so that a
/// page of dead controls could not invite a stranger to try; the cost
/// was that the instance we show to strangers hid the half of the
/// product that answers "what is this thing". A memory server is an
/// operator's tool as much as a reader's.
///
/// So the pages are mounted and linked, the guard refuses them by path,
/// and `read-only.js` renders their controls inert. The order still
/// matters and it is still the same order: the door is shut first
/// (asserted by property 1), and only then is the handle taken off.
///
/// The chat panel stays out, because it is not a page — it is a widget
/// in the frame whose only purpose is to capture memory on every turn.
#[tokio::test]
async fn a_frozen_instance_shows_every_console_and_arms_none_of_them() {
    let (app, cookie, _dir) = frozen_app_with_admin().await;
    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/home")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;

    assert!(
        html.contains("Read-only instance"),
        "the frame must say so: {html}"
    );
    assert!(
        !html.contains("chat-panel-form"),
        "the chat panel writes memory and must not be rendered: {html}"
    );
    // The frame ships the inert-controls script, and hands it the
    // server's own allow-list rather than a copy.
    assert!(
        html.contains("/dashboard/static/read-only.js"),
        "the frozen frame must load the inert-controls script: {html}"
    );
    assert!(
        html.contains("window.__mweLiveWrites=") && html.contains("/dashboard/settings/reveal"),
        "the script must be handed the live-write list: {html}"
    );
    for linked in [
        r#"href="/dashboard/users""#,
        r#"href="/dashboard/tokens""#,
        r#"href="/dashboard/prompts""#,
        r#"href="/dashboard/dream""#,
        r#"href="/dashboard/admin/backup""#,
        r#"href="/dashboard/recall-traces""#,
        r#"href="/dashboard/wiki""#,
    ] {
        assert!(
            html.contains(linked),
            "a shown instance must link the whole product, missing {linked}: {html}"
        );
    }

    // The consoles are reachable, not merely linked…
    for uri in [
        "/users",
        "/tokens",
        "/prompts",
        "/dream",
        "/admin/backup",
        "/admin/llm-config",
        "/welcome",
    ] {
        let response = send(
            &app,
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "GET {uri} must render on a shown instance"
        );
    }

    // …and every one of them still refuses to be written to. Property 1
    // walks the write surface in full; this is the half that would break
    // first if somebody "simplified" the mounting above.
    for uri in ["/users/new", "/tokens", "/admin/backup"] {
        let response = send(
            &app,
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, cookie.clone())
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("x=1"))
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "POST {uri} must still be refused on a shown instance"
        );
    }
}

/// An open instance keeps all of it — the mode is opt-in and the default
/// install is untouched.
#[tokio::test]
async fn an_open_instance_keeps_its_consoles_and_its_chat_panel() {
    let (app, _dir) = make_app(false).await;
    let cookie = setup_admin(&app).await;
    // `/facts`, not `/home`: on an open instance a freshly-created admin
    // has not answered the profile wizard yet, so `/home` redirects to
    // `/welcome` and there is no page to inspect. (That redirect is
    // exactly what the frozen build skips, since it mounts no wizard.)
    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/facts")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(!html.contains("Read-only instance"), "{html}");
    assert!(html.contains("chat-panel-form"), "{html}");
    assert!(html.contains(r#"href="/dashboard/users""#), "{html}");

    let response = send(
        &app,
        Request::builder()
            .uri("/tokens")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}
