// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only prompt editor integration tests.
//!
//! Exercises the four routes under `/dashboard/prompts`:
//!
//! - GET  `/prompts`              — list with status badges.
//! - GET  `/prompts/:name`        — edit form pre-filled with the
//!   current workdir override (or bundled fallback).
//! - POST `/prompts/:name`        — atomic save with `.bak` backup.
//! - POST `/prompts/:name/reset`  — reset to bundled default.

mod common;

use std::path::PathBuf;
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
use mwe_core::prompts as core_prompts;
use mwe_core::wiki::WikiTree;
use mwe_dashboard::{DashboardState, MemoryHandles, router};
use sqlx::SqlitePool;

/// Build a router whose workdir already contains every bundled prompt,
/// like `mwe-mcp init` would leave it. Returns the workdir path along
/// with the router so the test can poke at `<workdir>/prompts/*`.
async fn make_app_with_seeded_prompts() -> (Router, SqlitePool, PathBuf, tempfile::TempDir) {
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
    // Materialise every bundled prompt under <workdir>/prompts/, the
    // same way `mwe-mcp init` does. Without this seeding the listing
    // page would only show the "bundled only" status — the seed
    // round-trip is what gives us a meaningful "MatchesDefault" /
    // "Modified" surface to test.
    core_prompts::seed_bundled_into(dir.path(), core_prompts::BUNDLED).expect("seed core");
    core_prompts::seed_bundled_into(dir.path(), mwe_dashboard::BUNDLED_PROMPTS)
        .expect("seed dashboard");
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
async fn list_lists_every_bundled_prompt_with_matches_default_after_seed() {
    let (app, _pool, _workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .uri("/prompts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    // Every shipped prompt has a row.
    for name in [
        "ingest",
        "rem-dedup",
        "rem-promotions",
        "rem-subwiki-emergence",
        "agentic-chat-panel",
    ] {
        let needle = format!("<code>{name}</code>");
        assert!(html.contains(&needle), "missing row for {name}: {html}");
    }
    // After seeding, every prompt matches the bundled default — no
    // "modified" badge anywhere.
    assert!(html.contains("matches default"), "{html}");
    assert!(
        !html.contains(">modified<"),
        "no prompt should be modified post-seed: {html}"
    );
}

#[tokio::test]
async fn list_is_admin_only() {
    let (app, _pool, _workdir, _dir) = make_app_with_seeded_prompts().await;
    // No cookie => middleware redirects to login.
    let response = send(
        &app,
        Request::builder()
            .uri("/prompts")
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
async fn edit_form_renders_current_workdir_content() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    // Plant a marker line inside the existing ingest override so we
    // can prove the editor reads from the workdir, not the bundled
    // include_str!.
    let path = workdir.join("prompts/ingest.md");
    let original = std::fs::read_to_string(&path).expect("read seeded ingest");
    let edited = original.replace("```text", "```text\nMARKER_FROM_WORKDIR_OVERRIDE_TEST_ONLY");
    std::fs::write(&path, &edited).expect("write override");

    let response = send(
        &app,
        Request::builder()
            .uri("/prompts/ingest")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("MARKER_FROM_WORKDIR_OVERRIDE_TEST_ONLY"),
        "editor must surface workdir override content: {html}"
    );
    // And the status badge has flipped to modified, given our planted
    // edit differs byte-for-byte from the bundled default.
    assert!(html.contains("modified"), "{html}");
}

#[tokio::test]
async fn save_writes_workdir_file_and_backs_up_previous_content() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    let path = workdir.join("prompts/ingest.md");
    let before = std::fs::read_to_string(&path).expect("read seeded ingest");

    // Submit an edited body — same fenced shape but with a marker
    // inside the prompt content.
    let edited = before.replace(
        "```text\n",
        "```text\n# CUSTOM_EDIT_BY_OPERATOR_TEST_HOOK\n",
    );
    let form_body = serde_urlencoded::to_string([("body", edited.as_str())]).expect("encode form");

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/prompts/ingest")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Previous content backed up"),
        "success flash should name the backup: {html}"
    );

    // The new content is on disk verbatim.
    let after = std::fs::read_to_string(&path).expect("read after save");
    assert_eq!(after, edited);
    assert!(after.contains("CUSTOM_EDIT_BY_OPERATOR_TEST_HOOK"));

    // The .bak slot holds the pre-edit body byte-for-byte.
    let backup = workdir.join("prompts/ingest.md.bak");
    let backup_bytes = std::fs::read_to_string(&backup).expect("read .bak");
    assert_eq!(backup_bytes, before);
}

#[tokio::test]
async fn save_rejects_body_missing_fenced_block() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    let path = workdir.join("prompts/ingest.md");
    let before = std::fs::read_to_string(&path).expect("read seeded ingest");

    // Submit a payload that drops the ```text fence — the loader
    // would not find a body. The save handler must refuse rather than
    // silently break boot.
    let malformed = "---\nname: ingest\n---\n\nno fenced block here.";
    let form_body = serde_urlencoded::to_string([("body", malformed)]).expect("encode form");

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/prompts/ingest")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from(form_body))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Refused to save"),
        "validation must surface in the form: {html}"
    );

    // The original workdir file is untouched.
    let after = std::fs::read_to_string(&path).expect("read after");
    assert_eq!(after, before, "save must not have run");
    // No backup created either.
    assert!(!workdir.join("prompts/ingest.md.bak").exists());
}

#[tokio::test]
async fn save_unknown_prompt_returns_404() {
    let (app, _pool, _workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/prompts/does-not-exist")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, cookie)
            .body(Body::from("body=irrelevant"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reset_restores_bundled_default_and_backs_up_previous() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    // First, plant a hand-edited workdir override.
    let path = workdir.join("prompts/rem-dedup.md");
    let bundled = std::fs::read_to_string(&path).expect("read seeded rem-dedup");
    let edited = bundled.replace("```text\n", "```text\n# EDIT_THAT_RESET_WILL_REVERT\n");
    std::fs::write(&path, &edited).expect("plant edit");

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/prompts/rem-dedup/reset")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Reset rem-dedup to bundled default"),
        "{html}"
    );

    // The workdir file is back to the bundled bytes.
    let after = std::fs::read_to_string(&path).expect("read after reset");
    assert_eq!(after, bundled);
    assert!(!after.contains("EDIT_THAT_RESET_WILL_REVERT"));

    // The pre-reset content is preserved as .bak.
    let backup_bytes =
        std::fs::read_to_string(workdir.join("prompts/rem-dedup.md.bak")).expect("read .bak");
    assert_eq!(backup_bytes, edited);
}

/// Simulate a binary upgrade: the workdir file still carries the
/// version it was seeded with (`v1.0`), but we rewrite its
/// frontmatter to declare an older version while leaving the bundled
/// body in the binary at its current version. The drift detector
/// must (a) surface the pill in the list, (b) put the upgrade banner
/// on the edit page, (c) include "X prompt is on an older bundled
/// version" in the list header.
#[tokio::test]
async fn list_surfaces_drift_banner_when_workdir_version_lags_bundled() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;

    // Rewrite the workdir file's frontmatter to declare an older
    // version. We keep the body of the fenced block untouched so the
    // loader is happy. The seeded version is read from the file itself
    // so the test survives bundled-version bumps.
    let path = workdir.join("prompts/rem-dedup.md");
    let original = std::fs::read_to_string(&path).expect("read seeded rem-dedup");
    let bundled_version = mwe_core::prompts::parse_default_version_at_bootstrap(&original)
        .expect("seeded rem-dedup carries a version");
    let downgraded = original.replace(
        &format!("default_version_at_bootstrap: {bundled_version}"),
        "default_version_at_bootstrap: v0.9",
    );
    assert_ne!(
        downgraded, original,
        "frontmatter rewrite must change content"
    );
    std::fs::write(&path, &downgraded).expect("plant older version");

    let response = send(
        &app,
        Request::builder()
            .uri("/prompts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("1 prompt is on an older bundled version"),
        "list must announce the drift count: {html}"
    );
    assert!(
        html.contains(&format!("drift v0.9 → {bundled_version}")),
        "drift pill must show the version transition: {html}"
    );
    // And the modified counter row also got a "reset to bundled"
    // action, even though bytes-vs-bundled differ only because of
    // the frontmatter rewrite.
    assert!(
        html.contains("/dashboard/prompts/rem-dedup/reset"),
        "reset action must be visible for drifted rows: {html}"
    );
}

/// Edit form for a drifted prompt: the "Bundled default moved from
/// vN to vM" banner must appear, and the reset-section copy must
/// mention "upgrade" rather than plain "revert".
#[tokio::test]
async fn edit_form_renders_upgrade_banner_on_drift() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;
    let path = workdir.join("prompts/ingest.md");
    let original = std::fs::read_to_string(&path).expect("read seeded ingest");
    // Derive the current bundled version from the seeded file instead of
    // pinning a literal: the test hard-coded `v2.6`, so once `ingest.md` was
    // bumped to v2.7 the `.replace` silently no-op'd, no older version was
    // planted, no drift was detected, and the banner assertion failed. Reading
    // the value back keeps this green across future bundled-version bumps.
    let bundled_version = original
        .lines()
        .find_map(|l| l.trim().strip_prefix("default_version_at_bootstrap:"))
        .map(str::trim)
        .expect("seeded ingest.md carries default_version_at_bootstrap")
        .to_owned();
    let downgraded = original.replace(
        &format!("default_version_at_bootstrap: {bundled_version}"),
        "default_version_at_bootstrap: v1.9",
    );
    std::fs::write(&path, &downgraded).expect("plant older version");

    let response = send(
        &app,
        Request::builder()
            .uri("/prompts/ingest")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains("Bundled default moved from"),
        "edit page must show the upgrade banner: {html}"
    );
    assert!(html.contains("v1.9"), "{html}");
    assert!(html.contains(&bundled_version), "{html}");
    assert!(
        html.contains("Upgrades the workdir file"),
        "reset-section copy must switch to upgrade language: {html}"
    );
}

/// After a reset, the workdir file is rewritten from the bundled body
/// (which carries the current bundled version), so the drift pill
/// must disappear from the next list rendering.
#[tokio::test]
async fn reset_clears_drift_pill() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;
    let path = workdir.join("prompts/rem-promotions.md");
    let original = std::fs::read_to_string(&path).expect("read seeded rem-promotions");
    let downgraded = original.replace(
        "default_version_at_bootstrap: v1.0",
        "default_version_at_bootstrap: v0.8",
    );
    std::fs::write(&path, &downgraded).expect("plant older version");

    // Reset to bundled.
    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/prompts/rem-promotions/reset")
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("drift v0.8"),
        "drift pill must be gone after reset: {html}"
    );

    // Belt-and-suspenders: the next list page also has no drift
    // surface for rem-promotions.
    let response = send(
        &app,
        Request::builder()
            .uri("/prompts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        !html.contains("on an older bundled version"),
        "list must show no drift after reset: {html}"
    );
}

/// A workdir file whose frontmatter has been hand-edited to drop the
/// `default_version_at_bootstrap` key entirely still drifts against
/// the bundled body — the parser reads `None`, the comparator says
/// "(unset)" vs `vN.M`, and the operator sees the upgrade banner.
#[tokio::test]
async fn drift_pill_appears_when_workdir_lacks_version_key() {
    let (app, _pool, workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;
    let path = workdir.join("prompts/rem-dedup.md");
    let original = std::fs::read_to_string(&path).expect("read seeded rem-dedup");
    let bundled_version = mwe_core::prompts::parse_default_version_at_bootstrap(&original)
        .expect("seeded rem-dedup carries a version");
    let wiped = original
        .lines()
        .filter(|l| !l.starts_with("default_version_at_bootstrap:"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, &wiped).expect("plant wiped frontmatter");

    let response = send(
        &app,
        Request::builder()
            .uri("/prompts")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_string(response).await;
    assert!(
        html.contains(&format!("drift (unset) → {bundled_version}")),
        "missing version key must read as (unset) drift: {html}"
    );
}

#[tokio::test]
async fn admin_nav_links_to_prompts() {
    let (app, pool, _workdir, _dir) = make_app_with_seeded_prompts().await;
    let cookie = login_as_admin(&app).await;
    // Mark the first-run profile wizard done so /home renders the
    // dashboard instead of redirecting back to /welcome.
    sqlx::query("UPDATE user_credentials SET profile_initialized = 1")
        .execute(&pool)
        .await
        .unwrap();
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
    // Top nav admin block carries the link.
    assert!(html.contains("href=\"/dashboard/prompts\""), "{html}");
    // Home page admin actions list it too.
    assert!(html.contains("Edit operational prompts"), "{html}");
}
