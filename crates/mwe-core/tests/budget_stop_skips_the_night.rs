// SPDX-License-Identifier: AGPL-3.0-or-later
//! The nightly work under a reached daily budget.
//!
//! A test binary of its own because the budget guard is installed into a
//! process-wide `OnceLock`, the same idiom as the usage ledger and the
//! OAuth store: a test that installs a *stopped* guard would stop every
//! other test sharing the binary.
//!
//! What is asserted here is the behaviour the operator sees, and its
//! opposite. A round that cannot spend must come back **saying it was
//! skipped** — not as an infrastructure failure, and not as a clean night
//! that happened to find nothing to do. Those are the two wrong answers,
//! and both are plausible enough to have been written.

#![cfg(feature = "test-fakes")]

use std::sync::Arc;

use mwe_core::budget::{BudgetGuard, install_global};
use mwe_core::config::{BudgetConfig, LlmPricingConfig, ModelPrice};
use mwe_core::dream;
use mwe_core::dream_light::LightPolicy;
use mwe_core::embedder::{Embedder, FakeEmbedder};
use mwe_core::llm::FakeLlmBackend;
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;
use tempfile::TempDir;

/// A price list where one output token per million costs one unit, so a
/// seeded row's cost is easy to reason about.
fn pricing() -> LlmPricingConfig {
    LlmPricingConfig {
        currency: Some("EUR".to_owned()),
        models: vec![ModelPrice {
            model: "spendy".to_owned(),
            input: 1.0,
            cached_input: None,
            cache_write: None,
            output: 1.0,
        }],
        extra: serde_yaml::Mapping::new(),
    }
}

/// Seed a metered call worth `tokens / 1_000_000` into today's ledger.
async fn spend(pool: &SqlitePool, tokens: i64) {
    sqlx::query(
        "INSERT INTO llm_usage
           (ts, slot, backend, model, kind, billing, source, tag,
            prompt_tokens, completion_tokens, cached_prompt_tokens,
            cache_write_tokens, latency_ms, error)
         VALUES (?, 'cronista', 'anthropic', 'spendy', 'complete', 'api', 'serve', NULL,
                 0, ?, 0, 0, 10, NULL)",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(tokens)
    .execute(pool)
    .await
    .expect("seed usage");
}

async fn workdir() -> (TempDir, SqlitePool, WikiTree) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = mwe_core::db::open_or_init(dir.path()).await.expect("db");
    let tree = WikiTree::open(dir.path()).expect("tree");
    (dir, pool, tree)
}

#[tokio::test]
async fn a_reached_spend_cap_skips_the_nightly_work_and_says_so() {
    let (_dir, pool, tree) = workdir().await;
    // 2M output tokens at 1.00 per million = 2.00, against a 1.00 budget.
    spend(&pool, 2_000_000).await;
    install_global(Arc::new(BudgetGuard::new(
        pool.clone(),
        BudgetConfig {
            daily_limit: Some(1.0),
            warn_at_percent: 80,
            extra: serde_yaml::Mapping::new(),
        },
        pricing(),
    )));

    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("bge-m3", 8));
    // A bag whose every model answers instantly: the point is that none
    // of them is reached, so a backend that would happily reply is the
    // sharper fixture.
    let willing = FakeLlmBackend::new("spendy", "{}");
    let llms = mwe_core::rem::RemLlms {
        revisor: &willing,
        auto_promote: Some(&willing),
        apply: Some(&willing),
        comment_applier: Some(&willing),
        cronista: Some(&willing),
        navigator: Some(&willing),
    };

    // The light tick, fully wired: it would promote and compile, and it
    // does not start.
    let light = dream::run_light(
        &pool,
        &tree,
        Arc::clone(&embedder),
        Some(&llms),
        &LightPolicy::default(),
    )
    .await
    .expect("a daily budget is not an infrastructure failure");
    let stop = light
        .budget_stop
        .clone()
        .expect("the round names why it did not run");
    assert!(stop.starts_with("skipped: "), "{stop}");
    assert!(stop.contains("daily budget"), "{stop}");
    assert_eq!(
        dream::summarize_light(&light),
        stop,
        "the one-line summary the journal and the console both read must say it too"
    );
    assert!(
        !dream::summarize_light(&light).contains("promoted 0"),
        "a skipped round must not read as a tick that found nothing: {}",
        dream::summarize_light(&light)
    );

    // The operator-driven compile is the other door onto the same
    // spending, and without this it would ask the Cronista once per page,
    // be refused once per page, and climb the per-page failure ledger.
    let compile = dream::run_compile(
        &pool,
        &tree,
        embedder,
        &llms,
        dream::Cadence::Full,
        &chrono::Utc::now().to_rfc3339(),
    )
    .await
    .expect("a daily budget is not an infrastructure failure");
    assert!(compile.budget_stop.is_some());
    assert_eq!(compile.leaves, 0);
    assert!(
        compile.errors.is_empty(),
        "a stop is not a page that failed to compile: {:?}",
        compile.errors
    );
    assert_eq!(compile.pages_failed(), 0, "and the journal must agree");
}
