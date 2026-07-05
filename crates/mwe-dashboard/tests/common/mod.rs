// SPDX-License-Identifier: AGPL-3.0-or-later
//! Test helpers shared by the dashboard integration tests.
//!
//! We build a `mwe_dashboard::router` against a tempdir-backed DB and
//! drive it through `tower::ServiceExt::oneshot` — no listener, no
//! port binding, no real HTTP serialization. This keeps the tests
//! deterministic and fast.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use mwe_core::config::LlmConfig;
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

/// Build a fresh dashboard router backed by an isolated temp workdir.
/// Returns the router + the tempdir guard so the caller keeps the
/// workdir alive for the duration of the test.
#[allow(
    dead_code,
    reason = "used by sibling test files but not every one of them"
)]
pub async fn make_app() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    let state = DashboardState::new(pool, secret, blacklist, delegations);
    (router(state), dir)
}

/// Like [`make_app`] but wires a [`MemoryHandles`] bundle — needed by
/// tests that exercise paths which create / read wikis on disk (CRUD
/// auto-wiki creation, memory explorer, chat).
#[allow(dead_code, reason = "used by some sibling test files only")]
pub async fn make_app_with_memory() -> (Router, SqlitePool, WikiTree, tempfile::TempDir) {
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
        llm_overrides: mwe_dashboard::LlmBackendOverrides::default(),
        api_key_overrides: std::sync::Arc::new(std::sync::RwLock::new(
            std::collections::HashMap::new(),
        )),
        workdir: dir.path().to_path_buf(),
    };
    let state =
        DashboardState::new(pool.clone(), secret, blacklist, delegations).with_memory(memory);
    (router(state), pool, tree, dir)
}

/// Drive one request through the router and return the response.
pub async fn send(app: &Router, request: Request<Body>) -> Response<Body> {
    use tower::ServiceExt;
    app.clone().oneshot(request).await.expect("oneshot")
}

/// Collect a response body to a UTF-8 string. Panics on non-UTF-8 —
/// dashboard responses are always HTML or empty, so this is fine.
#[allow(dead_code, reason = "used by some sibling test files only")]
pub async fn body_string(response: Response<Body>) -> String {
    use http_body_util::BodyExt;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

/// Extract the value of the `Set-Cookie` header whose name matches
/// `cookie_name`. Returns the full attribute string (`name=value; ...`)
/// suitable for re-presenting as `Cookie:` on a subsequent request via
/// [`extract_cookie_value`].
#[allow(dead_code, reason = "used by some sibling test files only")]
pub fn extract_set_cookie(response: &Response<Body>, cookie_name: &str) -> Option<String> {
    let prefix = format!("{cookie_name}=");
    for h in response.headers().get_all(axum::http::header::SET_COOKIE) {
        if let Ok(s) = h.to_str()
            && s.starts_with(&prefix)
        {
            return Some(s.to_owned());
        }
    }
    None
}

/// Pull the `name=value` pair out of a `Set-Cookie` header.
#[allow(dead_code, reason = "used by some sibling test files only")]
pub fn extract_cookie_value(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .map(str::to_owned)
        .unwrap_or_default()
}
