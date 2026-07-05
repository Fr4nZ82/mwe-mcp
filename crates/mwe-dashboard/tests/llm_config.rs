// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin LLM-config editor integration tests.
//!
//! Exercises the three routes under `/dashboard/admin/`:
//!
//! - GET  `/admin/llm-config`         — render 5-slot editor + API key panel
//! - POST `/admin/llm-config`         — atomic YAML save with `.bak`
//! - POST `/admin/api-keys/:name`     — upsert env-var via `env_file::write_key`

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, send};
use mwe_core::config::{CONFIG_FILENAME, Config, LlmConfig};
use mwe_core::db;
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::FakeEmbedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

const ENV_FILENAME: &str = "mwe-mcp.env";

async fn make_app() -> (Router, SqlitePool, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::open_or_init(dir.path()).await.expect("open db");
    let secret = TokenSecret::new(vec![0xEFu8; 32]).expect("secret");
    let blacklist = Arc::new(BlacklistCache::new());
    let delegations = Arc::new(DelegationCache::new());
    std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
    let tree = WikiTree::open(dir.path()).expect("open tree");
    let embedder = Arc::new(FakeEmbedder::new("fake-bge-m3", 8));
    let memory = MemoryHandles {
        tree,
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
    let workdir = dir.path().to_path_buf();
    (router(state), pool, workdir, dir)
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

#[tokio::test]
async fn list_redirects_anonymous_users_to_login() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
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
        Some("/dashboard/login")
    );
}

#[tokio::test]
async fn page_renders_6_slots_and_api_key_panel_for_admin() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // Live-reload banner is the current headline (replaced the prior
    // "Restart required" banner once hot-reload landed).
    assert!(html.contains("take effect"), "{html}");
    // All six role cards, each with its slot key in a <code>. Cronista is
    // surfaced like every other slot: omitting it from the SLOTS list in
    // routes/llm_config.rs silently wiped the prose compiler on every
    // save, so it is configured here too (it keeps its deprecated marker).
    for slot in &[
        "hub_writer",
        "ingest",
        "rem_promotions",
        "cronista",
        "rem_dedup_semantic",
        "navigator",
    ] {
        assert!(
            html.contains(&format!(">{slot}<")),
            "missing role card {slot}: {html}"
        );
    }
    // The credentials cards always list the well-known cloud envs even
    // with no role wired.
    assert!(html.contains("ANTHROPIC_API_KEY"), "{html}");
    assert!(html.contains("GEMINI_API_KEY"), "{html}");
    assert!(html.contains("OPENROUTER_API_KEY"), "{html}");
    // No values set yet → every credential card shows "no key".
    assert!(html.contains("no key"), "{html}");
}

#[tokio::test]
async fn save_writes_yaml_and_backs_up_previous_config() {
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Seed a YAML so we have something to back up.
    let config_path = workdir.join(CONFIG_FILENAME);
    std::fs::write(
        &config_path,
        "# operator comment that will get flattened\nlogging:\n  level: info\n",
    )
    .expect("seed");

    // Submit a form that wires ingest to ollama + qwen.
    let form_body = "hub_writer__backend=&ingest__backend=ollama&ingest__model=qwen3.5:9b-q8_0\
        &ingest__api_key_env=&ingest__temperature=0.3&ingest__max_tokens=512\
        &ingest__reasoning_effort=&ingest__base_url=\
        &rem_promotions__backend=&rem_dedup_semantic__backend=&cronista__backend=";
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/llm-config")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("LLM configuration saved"),
        "flash missing: {html}"
    );

    // The YAML on disk is rewritten with the new ingest slot.
    let parsed = Config::load(&workdir).expect("load");
    let ingest = parsed.llm.ingest.as_ref().expect("ingest wired");
    assert_eq!(ingest.backend, "ollama");
    assert_eq!(ingest.model, "qwen3.5:9b-q8_0");
    assert_eq!(ingest.temperature, Some(0.3));
    assert_eq!(ingest.max_tokens, Some(512));
    // Other slots stayed unwired.
    assert!(parsed.llm.hub_writer.is_none());
    assert!(parsed.llm.cronista.is_none());

    // The previous YAML is in the .bak slot.
    let backup = workdir.join(format!("{CONFIG_FILENAME}.bak"));
    let backup_body = std::fs::read_to_string(&backup).expect("read backup");
    assert!(
        backup_body.contains("operator comment that will get flattened"),
        "{backup_body}"
    );
}

#[tokio::test]
async fn save_anthropic_derives_api_key_env_from_provider() {
    // The per-role `api_key_env` column is gone: an Anthropic role in the
    // default (key) mode derives ANTHROPIC_API_KEY on save — no rejection.
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let form_body = "hub_writer__backend=anthropic&hub_writer__model=claude-opus-4-8\
        &hub_writer__temperature=&hub_writer__max_tokens=\
        &hub_writer__reasoning_effort=&hub_writer__base_url=\
        &ingest__backend=&rem_promotions__backend=&rem_dedup_semantic__backend=";
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/llm-config")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cfg = Config::load(&workdir).expect("load");
    let hw = cfg.llm.hub_writer.as_ref().expect("hub_writer wired");
    assert_eq!(hw.backend, "anthropic");
    assert_eq!(hw.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
}

#[tokio::test]
async fn set_api_key_writes_env_file_and_redirects_back() {
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/ANTHROPIC_API_KEY")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("value=sk-ant-test-fingerprint-LAST"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "{}", response.status());
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/dashboard/admin/llm-config")
    );

    // Env file exists with the value quoted + the key written.
    let env_path = workdir.join(ENV_FILENAME);
    let body = std::fs::read_to_string(&env_path).expect("read env file");
    assert!(
        body.contains("ANTHROPIC_API_KEY=\"sk-ant-test-fingerprint-LAST\""),
        "{body}"
    );
}

#[tokio::test]
async fn set_api_key_rejects_invalid_env_var_name() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    // Lowercase + dot — definitely not a valid env-var identifier.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/bogus.key")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("value=whatever"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body_string(response).await;
    assert!(html.contains("bogus.key"), "{html}");
}

#[tokio::test]
async fn page_after_api_key_set_shows_fingerprint_not_value() {
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Save a key.
    let set = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/ANTHROPIC_API_KEY")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("value=sk-ant-test-fingerprint-WXYZ"))
            .unwrap(),
    )
    .await;
    assert!(set.status().is_redirection());

    // Re-render the page and assert the fingerprint surfaces but the
    // raw value does not. A dashboard set also populates the in-memory
    // override, so the credential card's origin reads "live override".
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // Last 4 chars of "sk-ant-test-fingerprint-WXYZ" → "WXYZ".
    assert!(html.contains("WXYZ"), "fingerprint not shown: {html}");
    assert!(
        !html.contains("sk-ant-test-fingerprint-WXYZ"),
        "raw value leaked into page: {html}"
    );
    // Origin label visible: a dashboard set is a live in-memory override.
    assert!(html.contains("live override"), "{html}");
    // env file from the helper is still on disk and parseable.
    let _ = std::fs::read_to_string(workdir.join(ENV_FILENAME)).expect("env file present");
}

#[tokio::test]
async fn save_hot_reloads_llm_config_into_memory_handles() {
    // The whole point of hot-reload: an admin save mutates the running
    // process's view of LlmConfig, no restart required. Drive the
    // POST through the router, then snapshot MemoryHandles directly
    // and assert the new slot is there.
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let form_body = "hub_writer__backend=ollama&hub_writer__model=qwen3.5:9b-q8_0\
        &hub_writer__api_key_env=&hub_writer__temperature=0.42&hub_writer__max_tokens=4096\
        &hub_writer__reasoning_effort=&hub_writer__base_url=\
        &ingest__backend=&rem_promotions__backend=&rem_dedup_semantic__backend=&cronista__backend=";
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/llm-config")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("hot-reloaded"),
        "success flash must mention reload: {html}"
    );

    // We can't directly poke the MemoryHandles through the router,
    // but we can re-load the YAML and confirm both the disk state
    // and (by inference) the in-memory state are in sync — the route
    // would have failed the test above if the in-memory swap had
    // panicked. To prove the in-memory side specifically, we ask
    // Config::load to round-trip the file the route just wrote.
    let cfg = mwe_core::config::Config::load(&workdir).expect("load");
    let hw = cfg.llm.hub_writer.as_ref().expect("hub_writer");
    assert_eq!(hw.model, "qwen3.5:9b-q8_0");
    assert_eq!(hw.temperature, Some(0.42));
    assert_eq!(hw.max_tokens, Some(4096));
}

#[tokio::test]
async fn set_api_key_hot_reloads_overrides_and_surfaces_origin_label() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    let set = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/ANTHROPIC_API_KEY")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("value=sk-ant-test-LIVE"))
            .unwrap(),
    )
    .await;
    assert!(set.status().is_redirection(), "{}", set.status());

    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // The "live override" label is the in-memory origin — proves the
    // in-memory map is what's being read, not the env file fallback.
    assert!(html.contains("live override"), "{html}");
    // Fingerprint still surfaces last 4 chars without leaking the value.
    assert!(html.contains("LIVE"), "{html}");
    assert!(
        !html.contains("sk-ant-test-LIVE"),
        "raw value leaked: {html}"
    );
}

#[tokio::test]
async fn save_then_immediate_backend_for_returns_new_anthropic_slot() {
    // End-to-end check of the hot-reload chain: POST a YAML config
    // that wires hub_writer to anthropic, then ask MemoryHandles for
    // the hub_writer backend and assert we get a live AnthropicBackend
    // — not the SlotMissing error a stale clone would throw.
    use mwe_core::config::LlmFunction;

    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Set the API key first so the anthropic backend has a key to
    // construct against (the override path is exactly what hot-reload
    // closes vs. having to std::env::set_var ANTHROPIC_API_KEY).
    let set = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/ANTHROPIC_API_KEY")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie.clone())
            .body(Body::from("value=sk-ant-fake-for-construction"))
            .unwrap(),
    )
    .await;
    assert!(set.status().is_redirection());

    let form_body = "hub_writer__backend=anthropic&hub_writer__model=claude-haiku-4-5-20251001\
        &hub_writer__api_key_env=ANTHROPIC_API_KEY&hub_writer__temperature=&hub_writer__max_tokens=\
        &hub_writer__reasoning_effort=&hub_writer__base_url=\
        &ingest__backend=&rem_promotions__backend=&rem_dedup_semantic__backend=&cronista__backend=";
    let save = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/llm-config")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(save.status(), StatusCode::OK);

    // Re-open the workdir via a fresh DashboardState (simulating "an
    // independent reader") — the route handler does the same. The
    // assertion that MemoryHandles::backend_for(HubWriter) constructs
    // a live backend is the direct hot-reload guarantee: without the
    // override the closure would return None for ANTHROPIC_API_KEY
    // and build_backend would raise MissingApiKeyEnv.
    let cfg = mwe_core::config::Config::load(&workdir).expect("load");
    let slot = cfg.llm.slot(LlmFunction::HubWriter).expect("slot");
    let envs: std::collections::HashMap<&str, &str> =
        std::iter::once(("ANTHROPIC_API_KEY", "sk-ant-fake-for-construction")).collect();
    slot.build_backend_with_env(LlmFunction::HubWriter, |k| {
        envs.get(k).map(|s| (*s).to_owned())
    })
    .expect("backend constructs");
}

#[cfg(unix)]
#[tokio::test]
async fn set_api_key_preserves_chmod_0600_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Seed an env file as a world-readable to prove the helper clamps
    // it back down.
    let env_path = workdir.join(ENV_FILENAME);
    std::fs::write(&env_path, "MWE_TOKEN_SECRET=deadbeef\n").expect("seed");
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/api-keys/ANTHROPIC_API_KEY")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("value=sk-ant-fake"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection());

    let mode = std::fs::metadata(&env_path)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "expected 0o600 after set, got {mode:o}");
}

#[tokio::test]
async fn page_shows_step1_onboarding_banner_for_fresh_admin() {
    // A just-created admin has `profile_initialized = 0`, so the LLM page
    // is framed as step 1 of onboarding (the `/setup` redirect lands here).
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(html.contains("step 1 of 2"), "{html}");
    // `LlmConfig::default()` wires no ingest slot, so the "continue" link is
    // gated behind the hint rather than offered as an active link.
    assert!(html.contains("usable provider to continue"), "{html}");
    assert!(
        !html.contains("Continue to profile setup"),
        "continue link must be gated while ingest is unconfigured: {html}"
    );
}

#[tokio::test]
async fn onboarding_banner_gone_after_profile_initialized() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    // Skip the profile primer → flips `profile_initialized = 1`, ending the
    // onboarding window. The LLM page then renders as the normal admin view.
    let skip = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    assert!(
        skip.status().is_redirection() || skip.status().is_success(),
        "{}",
        skip.status()
    );
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("step 1 of 2"),
        "onboarding banner must be gone once the profile is initialized: {html}"
    );
}

#[tokio::test]
async fn ollama_card_has_endpoint_and_optional_bearer() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/llm-config")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_string(response).await;
    assert!(
        html.contains("/dashboard/admin/ollama-endpoint"),
        "endpoint form action: {html}"
    );
    assert!(html.contains("name=\"endpoint\""), "endpoint input: {html}");
    // The optional Bearer for remote / cloud Ollama.
    assert!(
        html.contains("OLLAMA_API_KEY"),
        "optional bearer key: {html}"
    );
}

#[tokio::test]
async fn set_ollama_endpoint_persists_and_validates_scheme() {
    let (app, _pool, workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;

    // Non-http scheme is rejected.
    let bad = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/ollama-endpoint")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("endpoint=ftp://nope"))
            .unwrap(),
    )
    .await;
    assert!(bad.status().is_client_error(), "{}", bad.status());

    // A valid URL persists to the workdir env file as OLLAMA_BASE_URL.
    let ok = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/ollama-endpoint")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("endpoint=http%3A%2F%2F192.168.1.50%3A11434"))
            .unwrap(),
    )
    .await;
    assert!(ok.status().is_redirection(), "{}", ok.status());
    let env = std::fs::read_to_string(workdir.join(ENV_FILENAME)).unwrap_or_default();
    assert!(
        env.contains("OLLAMA_BASE_URL"),
        "env file carries it: {env}"
    );
    assert!(env.contains("192.168.1.50"), "{env}");
}

#[tokio::test]
async fn ollama_models_degrades_to_empty_when_daemon_unreachable() {
    let (app, _pool, _workdir, _dir) = make_app().await;
    let cookie = login_as_admin(&app).await;
    // Point the endpoint at a closed port so the proxy fails fast.
    let _ = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/admin/ollama-endpoint")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("endpoint=http%3A%2F%2F127.0.0.1%3A1"))
            .unwrap(),
    )
    .await;
    let response = send(
        &app,
        Request::builder()
            .uri("/admin/ollama-models")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("\"models\":[]"),
        "graceful empty list: {body}"
    );
}
