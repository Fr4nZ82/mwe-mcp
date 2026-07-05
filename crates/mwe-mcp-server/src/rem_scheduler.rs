// SPDX-License-Identifier: AGPL-3.0-or-later
//! REM cycle scheduler for the long-lived HTTP server.
//!
//! Closes the original `rem-cycle-not-scheduled` work item (see the
//! [REM cycle](../../../wiki/design-notes/rem-cycle.md) design note). The HTTP
//! transport is the standard PWA-as-permanent-daemon deployment, so a
//! tokio interval ticker inside `cmd_serve_http` is the right home for
//! the recurring [`mwe_core::rem::run_cycle`] invocation. Operators who
//! prefer an external scheduler (systemd timer, cron) can set
//! `rem.schedule.mode: disabled` in `mwe-mcp.config.yaml` and call
//! `mwe-mcp rem run-cycle` from their cron.
//!
//! ## Design choices
//!
//! - Backends are constructed **once** at task startup and reused for
//!   every cycle; each tick rebuilds the borrow-shaped [`RemLlms`] bag
//!   from those owned handles. Avoids amortising the (LLM-specific)
//!   backend construction cost on every cycle, which would be wasteful
//!   for the local-Ollama path (model not in RAM until first call).
//! - Per-cycle failures are **soft**: they get logged and the next tick
//!   still fires. A bug in one sub-job (forge cluster detector, hub
//!   writer …) should not take the HTTP server down with it; the
//!   operator restarts after fixing config / capacity.
//! - The optional slots (`auto_promote` = `rem_promotions`, `apply` =
//!   `ingest`) are passed only when the corresponding `llm.*` slot is
//!   configured; absence means "this sub-job is off", matching the
//!   per-sub-job documentation in [`mwe_core::rem::RemLlms`].
//! - Shutdown is cooperative: the spawned task selects between the
//!   ticker and the `shutdown` future supplied by the caller. When the
//!   server's ctrl-c handler resolves, the ticker exits cleanly without
//!   waiting for the next interval.

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use mwe_core::capture_buffer;
use mwe_core::config::{LlmConfig, LlmFunction, RemScheduleConfig, RemScheduleMode};
use mwe_core::dream::{self, Cadence, FullOutcome, LightOutcome};
use mwe_core::dream_journal::{self, DreamKind, DreamTrigger};
use mwe_core::dream_light::LightPolicy;
use mwe_core::embedder::Embedder;
use mwe_core::llm::LlmBackend;
use mwe_core::rem::{RemLlms, RemPolicy};
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Owned bag of LLM backends a long-running REM task holds across
/// cycles. Built once via [`build_backends`], turned into a borrow-shape
/// [`RemLlms`] on every tick via [`OwnedRemLlms::as_borrow`].
///
/// Public because the CLI `rem run-cycle` escape hatch reuses the same
/// constructor + adapter to drive a single cycle from a child process.
pub struct OwnedRemLlms {
    /// `hub_writer` slot — required for any meaningful cycle (without
    /// it the Hub Writer sub-job runs but writes nothing, and several
    /// proposal kinds that need an apply-time LLM cannot auto-apply).
    pub hub_writer: Box<dyn LlmBackend>,
    /// `rem_dedup_semantic` slot — required for the revisor sub-job.
    pub revisor: Box<dyn LlmBackend>,
    /// `rem_promotions` slot — optional; absence disables the
    /// auto-promote sub-job.
    pub auto_promote: Option<Box<dyn LlmBackend>>,
    /// `ingest` slot — optional; the cheap (Flash-tier) backend the
    /// **light** dream runs every compile stage on (tier-per-cadence:
    /// the strong model works only at REM). Absent ⇒ the light dream
    /// degrades to whatever strong slots are configured.
    pub apply: Option<Box<dyn LlmBackend>>,
    /// `cronista` slot (strong model) — optional; drives the
    /// narrative compiler ([`mwe_core::compiler`]). Absent ⇒ the compile
    /// step is skipped (facts stay buffered/promoted but unwritten).
    pub cronista: Option<Box<dyn LlmBackend>>,
    /// `navigator` slot — optional; the recall-repair sub-job's gold-set
    /// gate replays navigation with it. Absent ⇒ the gate replays
    /// flat-only and re-file candidates never commit (conservative).
    pub navigator: Option<Box<dyn LlmBackend>>,
}

impl OwnedRemLlms {
    /// Project the owned backends into a borrow-shaped [`RemLlms`] for
    /// a single [`mwe_core::rem::run_cycle`] invocation.
    #[must_use]
    pub fn as_borrow(&self) -> RemLlms<'_> {
        RemLlms {
            hub_writer: self.hub_writer.as_ref(),
            revisor: self.revisor.as_ref(),
            auto_promote: self.auto_promote.as_deref(),
            apply: self.apply.as_deref(),
            // The standard-wiki comment interpreter reuses the `ingest` slot —
            // turning a free-text correction into fact ops is the same class of
            // judgment as ingesting a message — so it borrows the same backend
            // as `apply` rather than adding a separate operator-configured slot.
            comment_applier: self.apply.as_deref(),
            cronista: self.cronista.as_deref(),
            navigator: self.navigator.as_deref(),
        }
    }
}

/// Build [`OwnedRemLlms`] from the configured `llm.*` slots.
///
/// Returns `Ok(None)` if the operator did not configure the mandatory
/// `hub_writer` or `rem_dedup_semantic` slots — the caller logs a hint
/// and skips spawning the scheduler instead of erroring out (the server
/// should still boot so the dashboard can be used to fix the config).
///
/// # Errors
///
/// Surfaces the underlying `ConfigError` when a configured slot exists
/// but cannot be materialised (unsupported backend, build failure).
pub fn build_backends(llm: &LlmConfig) -> Result<Option<OwnedRemLlms>> {
    let Some(hub) = llm.slot(LlmFunction::HubWriter) else {
        return Ok(None);
    };
    let Some(rev) = llm.slot(LlmFunction::RemDedupSemantic) else {
        return Ok(None);
    };
    let hub_writer = hub
        .build_backend(LlmFunction::HubWriter)
        .context("building rem hub_writer backend")?;
    let revisor = rev
        .build_backend(LlmFunction::RemDedupSemantic)
        .context("building rem rem_dedup_semantic backend")?;
    let auto_promote = match llm.slot(LlmFunction::RemPromotions) {
        Some(slot) => Some(
            slot.build_backend(LlmFunction::RemPromotions)
                .context("building rem rem_promotions backend")?,
        ),
        None => None,
    };
    let apply = match llm.slot(LlmFunction::Ingest) {
        Some(slot) => Some(
            slot.build_backend(LlmFunction::Ingest)
                .context("building rem ingest (apply) backend")?,
        ),
        None => None,
    };
    let cronista = match llm.slot(LlmFunction::Cronista) {
        Some(slot) => Some(
            slot.build_backend(LlmFunction::Cronista)
                .context("building rem cronista backend")?,
        ),
        None => None,
    };
    let navigator = match llm.slot(LlmFunction::Navigator) {
        Some(slot) => Some(
            slot.build_backend(LlmFunction::Navigator)
                .context("building rem navigator backend")?,
        ),
        None => None,
    };
    Ok(Some(OwnedRemLlms {
        hub_writer,
        revisor,
        auto_promote,
        apply,
        cronista,
        navigator,
    }))
}

/// Run a single **full dream** synchronously: a reorg plus the compile pass.
///
/// A complete [`mwe_core::rem::run_cycle`] reorg followed by the compile
/// pass. Used by the CLI escape hatch `mwe-mcp rem run-cycle` (closes the
/// cron-driven deployment story for `rem-cycle-not-scheduled`) and by the
/// scheduler spawn loop. Delegates to [`mwe_core::dream::run_full`] so the
/// cycle+compile composition lives in one place across the scheduler, the CLI,
/// and the dashboard.
///
/// # Errors
///
/// Surfaces infrastructure failures from the reorg or the compile pass.
pub async fn run_once(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &OwnedRemLlms,
    policy: &RemPolicy,
) -> Result<FullOutcome> {
    dream::run_full(pool, tree, embedder, &llms.as_borrow(), policy).await
}

/// Spawn the interval-driven REM cycle loop.
///
/// The first cycle fires after `schedule.initial_delay_secs` and every
/// `schedule.interval_secs` thereafter, until `shutdown` resolves.
/// Returns `None` when scheduling is disabled (`mode: disabled`) or
/// when the LLM bag could not be built — in both cases the operator is
/// expected to use the CLI escape hatch instead, and the caller logs a
/// hint accordingly.
///
/// `policy` is the **shared** REM policy handle (the same `Arc` the
/// dashboard state holds): the loop snapshots it at **each** cycle
/// start, so a save from the REM settings editor applies to the next
/// scheduled cycle without a restart — mirroring the recall-settings
/// hot-swap.
///
/// `shutdown` is typically wired to the same `tokio::signal::ctrl_c`
/// future the HTTP server uses for graceful shutdown, so REM stops
/// cleanly on SIGINT without waiting for the next interval.
#[must_use]
pub fn spawn<S>(
    schedule: RemScheduleConfig,
    pool: SqlitePool,
    tree: WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: Arc<OwnedRemLlms>,
    policy: Arc<RwLock<RemPolicy>>,
    shutdown: S,
) -> Option<JoinHandle<()>>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    if matches!(schedule.mode, RemScheduleMode::Disabled) {
        info!("rem scheduler: disabled by config (`rem.schedule.mode: disabled`)");
        return None;
    }
    let interval_secs = schedule.interval_secs.max(1);
    let initial_delay_secs = schedule.initial_delay_secs;
    info!(
        interval_secs,
        initial_delay_secs, "rem scheduler: enabled (interval mode)"
    );

    let handle = tokio::spawn(async move {
        tokio::pin!(shutdown);

        // Initial delay — let the server warm up. A zero-length delay
        // still goes through `tokio::time::sleep(Duration::ZERO)` to
        // keep the select! shape uniform and let an early ctrl-c be
        // honoured before the first cycle.
        let initial = tokio::time::sleep(std::time::Duration::from_secs(initial_delay_secs));
        tokio::pin!(initial);
        tokio::select! {
            () = &mut shutdown => {
                info!("rem scheduler: shutdown before first cycle");
                return;
            },
            () = &mut initial => {},
        }

        fire_once(&pool, &tree, &embedder, &llms, &snapshot_policy(&policy)).await;

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // Skip missed ticks rather than burst-running them when the
        // host was suspended — the next cycle naturally absorbs the
        // backlog, and burst-mode would defeat the operator's expectation
        // of "one cycle every N seconds".
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; we already ran a cycle
        // above, so skip it to align "interval = distance between
        // consecutive cycles" with the operator's reading of the YAML.
        ticker.tick().await;

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("rem scheduler: shutdown signal received, exiting loop");
                    return;
                },
                _ = ticker.tick() => {
                    fire_once(&pool, &tree, &embedder, &llms, &snapshot_policy(&policy)).await;
                }
            }
        }
    });
    Some(handle)
}

/// Owned snapshot of the shared policy handle, taken right before a
/// cycle fires so (a) a REM-settings save applies to the **next** cycle,
/// never a running one, and (b) the lock is not held across the cycle.
fn snapshot_policy(policy: &Arc<RwLock<RemPolicy>>) -> RemPolicy {
    policy.read().expect("rem policy rwlock poisoned").clone()
}

async fn fire_once(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llms: &OwnedRemLlms,
    policy: &RemPolicy,
) {
    let started = dream_journal::now_rfc3339();
    // One composition for the full dream (cycle + compile), shared with the CLI
    // and the dashboard via `dream::run_full`. Earlier the compile was
    // bolted on here but skipped by the CLI and the dashboard button.
    match run_once(pool, tree, Arc::clone(embedder), llms, policy).await {
        Ok(outcome) => {
            info!(
                cycle_id = outcome.cycle.cycle_id,
                duration_ms =
                    (outcome.cycle.ended_at - outcome.cycle.started_at).num_milliseconds(),
                compiled_leaves = outcome.compile.leaves,
                compiled_lists = outcome.compile.lists,
                compiled_hubs = outcome.compile.hubs,
                "rem scheduler: full dream complete (cycle + compile)"
            );
            // Nightly / interval full runs always make the journal — they are
            // meaningful and infrequent, and the admin Dream console has no
            // other window onto them.
            let (failed, degraded) = dream::journal_counts(Some(&outcome.compile));
            journal_run(
                pool,
                DreamKind::Full,
                true,
                failed,
                degraded,
                &dream::summarize_full(&outcome),
                &format!("{outcome:#?}"),
                &started,
            )
            .await;
        },
        Err(e) => {
            warn!(error = %e, "rem scheduler: full dream failed, will retry at next tick");
            journal_run(
                pool,
                DreamKind::Full,
                false,
                0,
                0,
                &format!("{e}"),
                &format!("{e:?}"),
                &started,
            )
            .await;
        },
    }
}

/// Record a finished scheduled dream run in the journal (best-effort: a write
/// failure is logged, never surfaced). The trigger is always
/// [`DreamTrigger::Scheduled`] here — the dashboard records its own
/// [`DreamTrigger::Manual`] runs. `finished_at` is stamped at call time.
/// `pages_failed` / `pages_degraded` are the compile's per-page outcome
/// counts (`dream::journal_counts`; `(0, 0)` when the run errored).
#[allow(clippy::too_many_arguments)]
async fn journal_run(
    pool: &SqlitePool,
    kind: DreamKind,
    ok: bool,
    pages_failed: i64,
    pages_degraded: i64,
    summary: &str,
    log_text: &str,
    started_at: &str,
) {
    let finished = dream_journal::now_rfc3339();
    if let Err(e) = dream_journal::record_run(
        pool,
        kind,
        DreamTrigger::Scheduled,
        ok,
        pages_failed,
        pages_degraded,
        summary,
        log_text,
        started_at,
        &finished,
    )
    .await
    {
        warn!(error = %e, "rem scheduler: failed to journal dream run");
    }
}

// ---------- compile pass ----------

/// Run one narrative compile pass: rebuild the plan, compile dirty pages, review.
///
/// The operator-driven escape hatch (CLI `mwe-mcp rem run-compile` + the
/// dashboard `/dream/compile` button), so it runs at [`Cadence::Full`] quality:
/// the strong-model Conciliatore reconciles new pages. (The automatic light
/// dream skips it.) Skips entirely when the `cronista`
/// slot is unconfigured. The Cartografo uses the `rem_promotions` slot, the
/// Conciliatore the `rem_dedup_semantic` slot, the Cronista its own slot, and
/// the Hub Writer the `hub_writer` slot.
///
/// # Errors
///
/// Surfaces planner / compiler infrastructure failures.
pub async fn run_compile_once(
    pool: &SqlitePool,
    tree: &WikiTree,
    llms: &OwnedRemLlms,
    now: &str,
) -> Result<mwe_core::compiler::CompileReport> {
    dream::run_compile(pool, tree, &llms.as_borrow(), Cadence::Full, now).await
}

// ---------- light dream ----------

/// Run a single light dream synchronously: promote, then compile what's dirty.
///
/// Promotes buffered captures into `fact_index`, then compiles the pages that
/// went dirty (when an LLM bag with a `cronista` is supplied). Used by the CLI
/// escape hatch `mwe-mcp rem run-light` and by the light-dream spawn loop.
/// Promotion is deterministic; `llms` is `None` (or a bag without `cronista`)
/// when no strong model is configured, in which case only the promotion runs.
/// Delegates to [`mwe_core::dream::run_light`].
///
/// # Errors
///
/// Surfaces infrastructure failures from the promotion or the compile pass.
pub async fn run_light_once(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: Option<&OwnedRemLlms>,
    policy: &LightPolicy,
) -> Result<LightOutcome> {
    let llms_borrow = llms.map(OwnedRemLlms::as_borrow);
    dream::run_light(pool, tree, embedder, llms_borrow.as_ref(), policy).await
}

/// Spawn the light-dream loop: drains the captures buffer into
/// `fact_index` on the **"timer + threshold"** cadence (see the
/// [REM cycle](../../../wiki/design-notes/rem-cycle.md) design note).
///
/// The loop wakes on a short poll interval and runs a light cycle when either
/// (a) `light_interval_secs` has elapsed since the last run (timer), or (b) the
/// buffered backlog has reached `light_backlog_threshold` (early trigger). It
/// shares `mode` with the full cycle — `mode: disabled` returns `None` for both.
///
/// `shutdown` is wired to the same ctrl-c future the HTTP server uses, so the
/// light dream stops cleanly on SIGINT.
#[must_use]
pub fn spawn_light<S>(
    schedule: RemScheduleConfig,
    pool: SqlitePool,
    tree: WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: Option<Arc<OwnedRemLlms>>,
    shutdown: S,
) -> Option<JoinHandle<()>>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    if matches!(schedule.mode, RemScheduleMode::Disabled) {
        info!("rem scheduler: light dream disabled by config (`rem.schedule.mode: disabled`)");
        return None;
    }
    let interval_secs = schedule.light_interval_secs.max(1);
    let initial_delay_secs = schedule.light_initial_delay_secs;
    let threshold = schedule.light_backlog_threshold;
    // Poll cadence: short enough to honour the backlog threshold promptly, but
    // never longer than the timer interval.
    let poll_secs = interval_secs.clamp(1, 60);
    info!(
        interval_secs,
        initial_delay_secs, threshold, poll_secs, "rem scheduler: light dream enabled"
    );

    let handle = tokio::spawn(async move {
        let policy = LightPolicy::default();
        tokio::pin!(shutdown);

        let initial = tokio::time::sleep(std::time::Duration::from_secs(initial_delay_secs));
        tokio::pin!(initial);
        tokio::select! {
            () = &mut shutdown => {
                info!("rem scheduler: light dream shutdown before first run");
                return;
            },
            () = &mut initial => {},
        }

        fire_light(&pool, &tree, &embedder, llms.as_deref(), &policy).await;
        let mut last_run = tokio::time::Instant::now();

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // first tick is immediate; we already ran once.

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("rem scheduler: light dream shutdown signal received, exiting loop");
                    return;
                },
                _ = ticker.tick() => {
                    let due_timer = last_run.elapsed().as_secs() >= interval_secs;
                    let due_threshold = threshold > 0 && backlog_at_least(&pool, threshold).await;
                    if due_timer || due_threshold {
                        fire_light(&pool, &tree, &embedder, llms.as_deref(), &policy).await;
                        last_run = tokio::time::Instant::now();
                    }
                }
            }
        }
    });
    Some(handle)
}

async fn backlog_at_least(pool: &SqlitePool, threshold: i64) -> bool {
    match capture_buffer::count_buffered(pool).await {
        Ok(n) => n >= threshold,
        Err(e) => {
            warn!(error = %e, "light dream: backlog probe failed, skipping early trigger");
            false
        },
    }
}

async fn fire_light(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llms: Option<&OwnedRemLlms>,
    policy: &LightPolicy,
) {
    let started = dream_journal::now_rfc3339();
    // Promotion + (when a `cronista` is wired) the incremental compile of the
    // pages the promotion dirtied — one composition, shared via
    // `dream::run_light` (formerly inline here). Cost-guarded inside
    // `run_light`: the compile is skipped entirely when nothing was promoted.
    match run_light_once(pool, tree, Arc::clone(embedder), llms, policy).await {
        Ok(outcome) => {
            // A scheduled light tick that scanned nothing is a no-op the loop
            // runs constantly — journaling it would flood the bounded history
            // and bury the meaningful runs. Record only the ticks that did
            // work, matching the condition that gates the info! log.
            if outcome.light.scanned > 0 {
                info!(
                    scanned = outcome.light.scanned,
                    promoted = outcome.light.promoted,
                    skipped_dup = outcome.light.skipped_dup,
                    superseded = outcome.light.superseded,
                    errors = outcome.light.errors.len(),
                    compiled = outcome.compile.is_some(),
                    "rem scheduler: light dream complete"
                );
                let (failed, degraded) = dream::journal_counts(outcome.compile.as_ref());
                journal_run(
                    pool,
                    DreamKind::Light,
                    true,
                    failed,
                    degraded,
                    &dream::summarize_light(&outcome),
                    &format!("{outcome:#?}"),
                    &started,
                )
                .await;
            }
        },
        Err(e) => {
            warn!(error = %e, "rem scheduler: light dream failed, will retry at next poll");
            journal_run(
                pool,
                DreamKind::Light,
                false,
                0,
                0,
                &format!("{e}"),
                &format!("{e:?}"),
                &started,
            )
            .await;
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use mwe_core::db;
    use mwe_core::embedder::FakeEmbedder;
    use mwe_core::llm::FakeLlmBackend;
    use tokio::sync::oneshot;

    fn fake_llms() -> OwnedRemLlms {
        OwnedRemLlms {
            hub_writer: Box::new(FakeLlmBackend::new("fake", "noop")),
            revisor: Box::new(FakeLlmBackend::new("fake", "noop")),
            auto_promote: None,
            apply: None,
            cronista: None,
            navigator: None,
        }
    }

    #[tokio::test]
    async fn spawn_returns_none_when_mode_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let llms = fake_llms();
        let (_tx, rx) = oneshot::channel::<()>();
        let handle = spawn(
            RemScheduleConfig {
                mode: RemScheduleMode::Disabled,
                interval_secs: 1,
                initial_delay_secs: 0,
                ..RemScheduleConfig::default()
            },
            pool,
            tree,
            embedder,
            Arc::new(llms),
            Arc::new(RwLock::new(RemPolicy::default())),
            async move {
                let _ = rx.await;
            },
        );
        assert!(handle.is_none(), "disabled mode must not spawn");
    }

    #[tokio::test]
    async fn run_once_completes_on_empty_workdir_with_fake_llms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let llms = fake_llms();
        let policy = RemPolicy::default();
        let report = run_once(&pool, &tree, embedder, &llms, &policy)
            .await
            .expect("run_once must succeed on an empty workdir");
        assert!(report.cycle.ended_at >= report.cycle.started_at);
        // Empty corpus → no candidates, no proposals; sub-job counters
        // all stay at zero. The contract under test is that `run_cycle`
        // does not error on the empty path — which is the surface the
        // scheduler hits on every tick that finds nothing to do.
        assert_eq!(report.cycle.revisor.pairs_examined, 0);
    }

    #[tokio::test]
    async fn spawn_light_returns_none_when_mode_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let (_tx, rx) = oneshot::channel::<()>();
        let handle = spawn_light(
            RemScheduleConfig {
                mode: RemScheduleMode::Disabled,
                ..RemScheduleConfig::default()
            },
            pool,
            tree,
            embedder,
            None,
            async move {
                let _ = rx.await;
            },
        );
        assert!(
            handle.is_none(),
            "disabled mode must not spawn the light dream"
        );
    }

    #[tokio::test]
    async fn run_light_once_completes_on_empty_workdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let policy = LightPolicy::default();
        let report = run_light_once(&pool, &tree, embedder, None, &policy)
            .await
            .expect("light cycle must succeed on an empty workdir");
        assert_eq!(report.light.scanned, 0);
        assert_eq!(report.light.promoted, 0);
    }

    #[tokio::test]
    async fn run_compile_once_is_a_clean_noop_without_cronista() {
        // fake_llms has no cronista slot ⇒ the compile pass skips and reports
        // nothing, even though the planner stage runs. Proves the compile wiring
        // composes (planner + compiler + reviewer) and degrades gracefully.
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let llms = fake_llms();
        let report = run_compile_once(&pool, &tree, &llms, "2026-05-31T00:00:00Z")
            .await
            .expect("compile pass must succeed on an empty workdir");
        assert_eq!(report.leaves, 0);
        assert_eq!(report.hubs, 0);
    }

    #[tokio::test]
    async fn spawn_exits_cleanly_on_shutdown_before_first_cycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let llms = fake_llms();
        // Long initial delay + immediate shutdown ⇒ the spawn loop
        // must select on the shutdown branch before ever firing a
        // cycle. Bounds-checks the `tokio::select!` shape: a
        // misordered select would either fire the cycle or block on
        // the timer forever.
        let (tx, rx) = oneshot::channel::<()>();
        let handle = spawn(
            RemScheduleConfig {
                mode: RemScheduleMode::Interval,
                interval_secs: 3_600,
                initial_delay_secs: 3_600,
                ..RemScheduleConfig::default()
            },
            pool,
            tree,
            embedder,
            Arc::new(llms),
            Arc::new(RwLock::new(RemPolicy::default())),
            async move {
                let _ = rx.await;
            },
        )
        .expect("must spawn in interval mode");
        // Drop a beat so the task is parked on its first select! arm.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = tx.send(());
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("scheduler must exit on shutdown")
            .expect("scheduler join");
    }
}
