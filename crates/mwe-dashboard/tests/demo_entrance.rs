// SPDX-License-Identifier: AGPL-3.0-or-later
//! The passwordless demo entrance (`instance.demo_identities`).
//!
//! The property that matters most is the negative one, and it is
//! asserted as a **`404`, not a `403`**: on any deployment without the
//! demo configuration the route is not mounted, so there is no door to
//! refuse at. "The button is not rendered" would be a curtain; this is a
//! door that was never cut, and the test says which one it is checking.
//!
//! The positive properties are the ones the demonstration lives on: a
//! stranger enters with one click, and switches identity with one more
//! from whatever page they are reading — because comparing the *same*
//! page as two people is the whole point.

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
use sqlx::SqlitePool;

/// Build a dashboard with an explicit posture, over a workdir whose
/// admin (and demo cast) have already been seeded through an open
/// router — `/setup` and the user-creation path are configuration
/// writes, and a frozen instance refuses both.
async fn make_app(
    read_only: bool,
    demo_identities: &[&str],
) -> (Router, SqlitePool, tempfile::TempDir) {
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
    let base =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);

    // Seed the admin so `/login` is reachable rather than redirecting to
    // the setup wizard, then insert the demo cast directly.
    let open = router(base.clone());
    let response = send(
        &open,
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
    for id in ["bob", "zoe"] {
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES (?, 0)")
            .bind(id)
            .execute(&pool)
            .await
            .expect("insert demo user");
    }

    let state = base.with_config(DashboardConfig {
        read_only,
        demo_identities: demo_identities
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into(),
        ..DashboardConfig::default()
    });
    (router(state), pool, dir)
}

async fn enter_as(
    app: &Router,
    user_id: &str,
    referer: Option<&str>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/demo/enter")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(r) = referer {
        builder = builder.header(header::REFERER, r);
    }
    send(
        app,
        builder
            .body(Body::from(format!("user_id={user_id}")))
            .unwrap(),
    )
    .await
}

/// The gate, stated the only way that means anything: with the mode off
/// the route **does not exist**, and the proof is that the server answers
/// it *identically* to a path nobody ever defined. Three configurations
/// that each look like they might enable it, and none does.
///
/// The comparison **is** the assertion, and no fixed status code would
/// do. This router bounces an unmatched request to the sign-in page
/// rather than answering a plain `404`, and a frozen deployment refuses
/// an unknown write with `403` before routing gets a say — so the
/// expected code differs per posture while the property does not. What
/// must hold is that the passwordless door is indistinguishable from a
/// wall: whatever this server says to a path nobody ever defined, it
/// says to `/demo/enter` too.
#[tokio::test]
async fn without_the_demo_configuration_the_entrance_route_does_not_exist() {
    // (read_only, identities) — the three ways to be "almost" a demo.
    for (read_only, identities) in [
        (false, &["bob", "zoe"][..]),
        (true, &[][..]),
        (false, &[][..]),
    ] {
        let (app, _pool, _dir) = make_app(read_only, identities).await;
        let response = enter_as(&app, "bob", None).await;
        let control = send(
            &app,
            Request::builder()
                .method("POST")
                .uri("/no-such-route-was-ever-defined")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("user_id=bob"))
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            control.status(),
            "read_only={read_only} identities={identities:?}: POST /demo/enter must be answered \
             exactly as a path that does not exist"
        );
        assert!(
            extract_set_cookie(&response, "mwe_session").is_none(),
            "read_only={read_only} identities={identities:?}: no session may come out"
        );
    }
}

/// …and the sign-in page of such a deployment offers a password form and
/// no buttons, so nothing hints at a door that is not there.
#[tokio::test]
async fn without_the_demo_configuration_the_login_page_is_a_password_form() {
    let (app, _pool, _dir) = make_app(false, &[]).await;
    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(html.contains(r#"name="password""#), "{html}");
    assert!(!html.contains("/dashboard/demo/enter"), "{html}");
    assert!(!html.contains("Enter as"), "{html}");
}

/// The entrance itself: three buttons, no credentials, a real session.
#[tokio::test]
async fn a_visitor_enters_with_one_click_and_no_credentials() {
    let (app, _pool, _dir) = make_app(true, &["bob", "alice", "zoe"]).await;

    let html = body_string(
        send(
            &app,
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    for label in ["Enter as Bob", "Enter as Alice", "Enter as Zoe"] {
        assert!(html.contains(label), "missing `{label}`: {html}");
    }

    let response = enter_as(&app, "bob", None).await;
    assert!(
        response.status().is_redirection(),
        "entering must mint a session: {}",
        response.status()
    );
    let cookie = extract_cookie_value(
        &extract_set_cookie(&response, "mwe_session").expect("session cookie"),
    );

    let page = body_string(
        send(
            &app,
            Request::builder()
                .uri("/home")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(page.contains("bob"), "the session must be bob's: {page}");
}

/// A passwordless door hands out the smallest thing that works. `alice`
/// is the deployment's admin; entering as her from the demo door still
/// produces a non-admin session, so the admin nav never appears.
#[tokio::test]
async fn a_demo_session_is_never_admin_even_for_an_admin_identity() {
    let (app, _pool, _dir) = make_app(true, &["bob", "alice", "zoe"]).await;
    let response = enter_as(&app, "alice", None).await;
    let cookie = extract_cookie_value(
        &extract_set_cookie(&response, "mwe_session").expect("session cookie"),
    );
    let page = body_string(
        send(
            &app,
            Request::builder()
                .uri("/home")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(
        !page.contains(r#"href="/dashboard/admin/recall-traces""#),
        "a demo session must not carry the admin role: {page}"
    );
}

/// The switcher is in the frame, and the switch lands back on the page
/// it was made from — that round trip *is* the demonstration.
#[tokio::test]
async fn the_switcher_is_on_every_page_and_returns_to_the_same_page() {
    let (app, _pool, _dir) = make_app(true, &["bob", "alice", "zoe"]).await;
    let response = enter_as(&app, "bob", None).await;
    let cookie = extract_cookie_value(
        &extract_set_cookie(&response, "mwe_session").expect("session cookie"),
    );

    // On a page that is not the sign-in screen.
    let page = body_string(
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
    assert!(page.contains("/dashboard/demo/enter"), "{page}");
    assert!(page.contains(">Zoe<"), "a one-click switch to Zoe: {page}");
    assert!(
        !page.contains(">Bob<"),
        "no button to become who you already are: {page}"
    );

    // …and switching from there comes back to the same page.
    let response = enter_as(&app, "zoe", Some("https://demo.example/dashboard/facts")).await;
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/facts")
    );
}

/// The form field carries a choice, not an authorisation: an identity
/// that is not on the configured list is refused even though the route
/// exists and the user does.
#[tokio::test]
async fn an_identity_outside_the_configured_list_is_refused() {
    let (app, _pool, _dir) = make_app(true, &["bob", "zoe"]).await;
    let response = enter_as(&app, "alice", None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(extract_set_cookie(&response, "mwe_session").is_none());
}

/// A configured identity that does not exist is an operator typo, not a
/// visitor's doing — and it mints nothing.
#[tokio::test]
async fn a_configured_identity_that_does_not_exist_mints_nothing() {
    let (app, _pool, _dir) = make_app(true, &["bob", "nobody"]).await;
    let response = enter_as(&app, "nobody", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(extract_set_cookie(&response, "mwe_session").is_none());
}
