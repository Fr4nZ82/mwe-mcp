// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unified dream compositions — the single definition of *what a dream does*.
//!
//! Earlier the composition "(reorg | light promotion) + narrative compile"
//! was duplicated across three surfaces — the interval scheduler, the
//! `mwe-mcp rem run-*` CLI escape hatch, and the dashboard admin button — and
//! **three of the four copies silently skipped the compile pass**. A manual
//! "run REM" therefore left the standard wikis stale until the next
//! *scheduled* tick happened to recompile them. This module is now the single
//! place those steps are composed; the scheduler, the CLI, and the dashboard
//! all delegate here, so the three dreams can never drift apart again.
//!
//! Three compositions, mirroring the three operator-facing dreams:
//!
//! - [`run_compile`] — the narrative compile only (Cartografo →
//!   Conciliatore → Cronista → Hub Writer → Reviewer) over the dirty pages.
//! - [`run_light`] — the cheap, frequent dream: promote buffered captures into
//!   `fact_index`, then compile the pages that went dirty.
//! - [`run_full`] — the nightly / on-demand dream: a complete [`rem::run_cycle`]
//!   reorg (dedup, auto-promote, archive, **parked-comment application**,
//!   hub writer) followed by a compile pass.
//!
//! The compile step is gated on the `cronista` slot: absent ⇒ it is skipped and
//! facts stay buffered/promoted but unwritten (the prose is the product of the
//! strong model, so a deployment without it simply has no narrative output).

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::warn;

use crate::compiler::{self, CompileReport};
use crate::dream_light::{self, LightCycleReport, LightPolicy};
use crate::embedder::Embedder;
use crate::llm::LlmBackend;
use crate::planner::NewFactPlacement;
use crate::rem::{self, RemCycleReport, RemLlms, RemPolicy};
use crate::wiki::WikiTree;
use crate::{compile_failures, meta_annotate, planner, reindex, reviewer};

/// Outcome of a light dream.
///
/// The promotion report, plus the compile report when the promotion produced
/// something worth recompiling and a `cronista` was wired. `compile` is `None`
/// when nothing was promoted/superseded or when the compile step was skipped
/// (no LLM bag / no `cronista`).
#[derive(Debug)]
pub struct LightOutcome {
    /// Captures → `fact_index` promotion report.
    pub light: LightCycleReport,
    /// Narrative recompile of the pages the promotion dirtied, if it ran.
    pub compile: Option<CompileReport>,
}

/// Outcome of a full dream: the reorg cycle report, plus the compile report.
#[derive(Debug)]
pub struct FullOutcome {
    /// Legacy reorg + parked-comment application + hub writer.
    pub cycle: RemCycleReport,
    /// Narrative recompile of every page the reorg left dirty.
    pub compile: CompileReport,
}

/// Which dream cadence is driving a compile pass.
///
/// The distinction lets the frequent, cheap light dream run on the cheap
/// ingest tier while the nightly REM runs at full strong-model quality; the
/// Cartografo (strong-model fact classification) stays REM-only. A
/// manual/operator compile runs at full quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// The frequent automatic light dream (promote → compile dirty pages).
    /// Every LLM stage runs on the cheap ingest-tier backend.
    Light,
    /// The nightly REM reorg, or an operator-driven compile (CLI / dashboard
    /// button). Full quality: every stage on its strong slot.
    Full,
}

/// Pick the Conciliatore backend for a cadence. The Conciliatore runs at
/// **both** cadences — it is the placement-time prevention front of semantic
/// page consolidation: a page proposed on the light path (the ingest
/// `target_page` hint) would otherwise materialise with no near-synonym check
/// at all, which is how the dogfood corpus grew three Paris pages. Tier per
/// cadence as everywhere else: Full = the configured `rem_dedup_semantic`
/// slot (`llms.revisor` — the low binary-classifier confirmer tier), Light =
/// the cheap ingest-tier backend (falling back to the revisor slot on a
/// Flash-less deployment). Factored out so the policy is pinned by a unit
/// test, not buried in `run_compile`.
fn conciliatore_backend<'a>(
    cadence: Cadence,
    strong: &'a dyn LlmBackend,
    flash: Option<&'a dyn LlmBackend>,
) -> &'a dyn LlmBackend {
    tier_backend(cadence, strong, flash)
}

/// Pick a compile-stage backend by cadence (the strong model works ONLY at
/// REM). [`Cadence::Full`] uses the configured strong (Pro)
/// slot; [`Cadence::Light`] uses the cheap ingest-tier (Flash) backend — the
/// same one the classifier runs on — falling back to the strong slot only when
/// no ingest slot is configured (a Flash-less deployment). Factored out so the
/// tier policy is pinned by a unit test.
fn tier_backend<'a>(
    cadence: Cadence,
    strong: &'a dyn LlmBackend,
    flash: Option<&'a dyn LlmBackend>,
) -> &'a dyn LlmBackend {
    match cadence {
        Cadence::Full => strong,
        Cadence::Light => flash.unwrap_or(strong),
    }
}

/// Run the narrative compile pass: (incrementally) rebuild the
/// compilation plan, compile the dirty pages into prose, then run the
/// deterministic reviewer.
///
/// Returns a default (empty) [`CompileReport`] when the `cronista` slot is
/// unconfigured — facts stay buffered/promoted but unwritten. In
/// [`Cadence::Full`] each stage uses its own configured slot — the Cartografo
/// the strong `rem_promotions` slot (`llms.auto_promote`), the Conciliatore the
/// `rem_dedup_semantic` slot (`llms.revisor` — the low binary-classifier
/// confirmer tier), the Cronista its own slot, the
/// Hub Writer the `hub_writer` slot. In [`Cadence::Light`] the Cartografo does
/// not run at all — new facts are placed on their ingest-proposed page
/// ([`NewFactPlacement::Ingest`]) — while the Conciliatore, the Cronista and
/// the Hub Writer run on the cheap ingest-tier (Flash) backend via
/// [`tier_backend`]; the strong tier is REM-only.
///
/// # Errors
///
/// Surfaces planner / compiler infrastructure failures. The reviewer is
/// non-blocking: its findings are logged, never returned as an error.
pub async fn run_compile(
    pool: &SqlitePool,
    tree: &WikiTree,
    llms: &RemLlms<'_>,
    cadence: Cadence,
    now: &str,
) -> Result<CompileReport> {
    let Some(cronista_strong) = llms.cronista else {
        tracing::debug!("dream: cronista slot unconfigured — skipping narrative compilation");
        return Ok(CompileReport::default());
    };
    // Tier per cadence (the strong model works ONLY at REM). The light
    // dream runs every compile stage on the cheap ingest-tier (Flash) backend,
    // which reaches us as `llms.apply`; the nightly/operator full pass uses the
    // configured strong (Pro) slots.
    let flash = llms.apply;
    let cronista = tier_backend(cadence, cronista_strong, flash);
    let hub_writer = tier_backend(cadence, llms.hub_writer, flash);
    // Placement of NEW facts per cadence. LIGHT settles each
    // fact on the page the ingest classifier already proposed — no LLM, the
    // strong-model Cartografo is REM-only. FULL runs the Cartografo (the
    // `rem_promotions` slot); a Full pass with no strong slot configured degrades
    // to the deterministic orphan-fallback (the historical `None` behaviour).
    let placement = match cadence {
        Cadence::Light => NewFactPlacement::Ingest,
        Cadence::Full => llms.auto_promote.map_or(
            NewFactPlacement::OrphanFallback,
            NewFactPlacement::Cartografo,
        ),
    };
    // Conciliatore at BOTH cadences (placement-time near-synonym resistance,
    // see `conciliatore_backend`): the light dream runs it on the ingest tier
    // so a page born on the light path still passes the redirect check before
    // it materialises; the nightly REM runs it on its configured
    // `rem_dedup_semantic` slot (`llms.revisor`).
    let conciliatore = Some(conciliatore_backend(cadence, llms.revisor, flash));
    let plan = planner::build_wiki_plan(pool, tree, placement, conciliatore, now)
        .await
        .context("planner")?;
    let report = compiler::compile_dirty_pages(pool, tree, &plan, cronista, hub_writer, now)
        .await
        .context("compiler")?;
    // Recall-navigation: deterministic, no-LLM pass that syncs each wiki's
    // `_meta.keywords["topics"]` to the union of its facts' topics, so the
    // catalog / root index a recall navigator reads carry the topic vocabulary.
    // Best-effort like the reviewer below — a failure degrades recall, it never
    // fails the compile.
    match meta_annotate::sync_wiki_keywords(pool, tree).await {
        Ok(0) => {},
        Ok(n) => tracing::debug!(updated = n, "dream compile: synced _meta topic keywords"),
        Err(e) => warn!(error = %e, "dream compile: _meta keyword sync failed"),
    }
    // Same pass one level down: each page's testata gets the topic union of
    // the facts living on it — the per-page card an intra-wiki hop reads.
    match meta_annotate::sync_page_keywords(pool, tree).await {
        Ok(0) => {},
        Ok(n) => tracing::debug!(updated = n, "dream compile: synced page testata keywords"),
        Err(e) => warn!(error = %e, "dream compile: page keyword sync failed"),
    }
    // Enrollment context for the reviewer's cross-subject check (which wikis
    // are identity wikis + each user's groups). Best-effort like the reviewer
    // itself: a load failure only disables that one check.
    let identity = match reviewer::IdentityContext::load(pool, tree).await {
        Ok(ctx) => ctx,
        Err(e) => {
            warn!(error = %e, "dream compile: identity context load failed, skipping cross-subject check");
            reviewer::IdentityContext::default()
        },
    };
    match reviewer::review(tree, &plan, &identity) {
        Ok(r) if !r.is_clean() => {
            warn!(
                findings = r.finding_count(),
                cross_subject_bloat = r.cross_subject_bloat.len(),
                "dream compile: reviewer found issues"
            );
            park_bridge_signals(pool, tree, &plan, &r).await;
        },
        Ok(_) => {},
        Err(e) => warn!(error = %e, "dream compile: reviewer failed"),
    }
    Ok(report)
}

/// The findings→healing bridge: persist what tonight's review (and the
/// compile-failure ledger) learned, so the NEXT cycle's engines act on it.
///
/// The reviewer runs at the dream's tail — after this cycle's refile and
/// after the plan build — so its findings can only influence cycle N+1;
/// they park on the persisted plan (the `force_dirty` pattern). All
/// nominations, never verdicts:
///
/// - each `cross_subject_bloat` fact → a **refile candidate** (the refile
///   judge still decides, and refuses what does not apply);
/// - each `cross_subject_bloat` page, each topology-anomalous page
///   (`leaf_with_children`, `hub_with_facts`), each `oversized` page, plus
///   every page failing its compile repeatedly (the ledger's streak) → a
///   **placement re-open**, so the Cartografo re-judges the carried
///   placements with the mass + identity + container signals live
///   (split-by-mass can finally fire on an old page; a fact-bearing
///   container drains and the assembly normalises it to hub). A parked
///   re-open is consumed only by a build that runs the Cartografo — a
///   light build carries it (`planner::build_wiki_plan`).
///
/// Best-effort like the review itself: a park failure only delays healing.
async fn park_bridge_signals(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &planner::CompilationPlan,
    r: &reviewer::ReviewReport,
) {
    let refile: Vec<String> = r
        .cross_subject_bloat
        .iter()
        .map(|(_, fid, _)| fid.clone())
        .collect();
    let mut reopen: std::collections::BTreeSet<String> = r
        .cross_subject_bloat
        .iter()
        .map(|(slug, _, _)| slug.clone())
        .collect();
    reopen.extend(r.leaf_with_children.iter().map(|(s, _)| s.clone()));
    reopen.extend(r.hub_with_facts.iter().map(|(s, _)| s.clone()));
    reopen.extend(r.oversized_pages.iter().map(|(s, _)| s.clone()));
    // Pages failing their compile twice in a row re-open too. Map the
    // ledger's source_path key back to a plan slug via the same helper
    // that wrote it.
    match compile_failures::persistent(pool, 2).await {
        Ok(rows) => {
            let by_path: std::collections::BTreeMap<String, &String> = plan
                .pages
                .iter()
                .filter_map(|(slug, p)| compiler::page_source_path(tree, p).map(|sp| (sp, slug)))
                .collect();
            for row in rows {
                if let Some(slug) = by_path.get(&row.source_path) {
                    reopen.insert((*slug).clone());
                }
            }
        },
        Err(e) => {
            warn!(error = %e, "dream compile: failure-ledger read failed — reopen park incomplete");
        },
    }
    let reopen: Vec<String> = reopen.into_iter().collect();
    match planner::park_bridge_signals(tree, &refile, &reopen) {
        Ok(0) => {},
        Ok(n) => tracing::info!(
            parked = n,
            refile_candidates = refile.len(),
            reopen_pages = reopen.len(),
            "dream compile: bridge signals parked for the next cycle"
        ),
        Err(e) => warn!(error = %e, "dream compile: bridge-signal park failed"),
    }
}

/// Run one light dream: promote captures, then compile what went dirty.
///
/// Promotes buffered captures into `fact_index`, then — when the promotion
/// produced fresh facts and a `cronista` is wired — compiles the pages that
/// went dirty so the new captures become readable prose without waiting for the
/// night.
///
/// `llms` is optional: promotion itself is deterministic (no LLM). When `None`
/// (or when the bag has no `cronista`), the compile step is skipped and only
/// the promotion runs — the "no strong model configured" path.
///
/// # Errors
///
/// Surfaces promotion or compile infrastructure failures.
pub async fn run_light(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: Option<&RemLlms<'_>>,
    policy: &LightPolicy,
) -> Result<LightOutcome> {
    let light = dream_light::run_light_cycle(pool, tree, embedder.clone(), policy)
        .await
        .context("light dream promotion")?;
    // Skip the (expensive) compile when nothing changed: a plan with no new
    // facts is a no-op, but checking here keeps the strong model untouched.
    let promoted_any = light.promoted > 0 || light.superseded > 0;
    let compile = match (promoted_any, llms) {
        (true, Some(llms)) => {
            Some(run_compile(pool, tree, llms, Cadence::Light, &Utc::now().to_rfc3339()).await?)
        },
        _ => None,
    };
    // Retirement hygiene: excise retired-fact regions from pages OUTSIDE the
    // compilation plan (`rules.md`, husks — plan pages self-clean at their
    // next compile). This is the convergent backstop behind the act-time
    // strips, covering the retire paths that run inside the proposal apply
    // chassis. Best-effort: a failure degrades disk hygiene, never the dream
    // (the residue stays fail-closed-redacted meanwhile).
    match reindex::sweep_retired_regions(pool, tree, embedder, reindex::RETIRED_SWEEP_MAX_PAGES)
        .await
    {
        Ok(sweep) if sweep.changed() => tracing::info!(
            pages_examined = sweep.pages_examined,
            regions_stripped = sweep.regions_stripped,
            rows_settled = sweep.rows_settled,
            pages_skipped_plan = sweep.pages_skipped_plan,
            warnings = sweep.warnings.len(),
            "light dream: retired-region hygiene sweep"
        ),
        Ok(_) => {},
        Err(e) => {
            tracing::warn!(error = %e, "light dream: retired-region hygiene sweep failed (non-fatal)");
        },
    }
    Ok(LightOutcome { light, compile })
}

/// Run one full dream: a complete reorg followed by a compile pass.
///
/// The [`rem::run_cycle`] reorg settles the fact set (dedup, auto-promote,
/// archive, **parked-comment application**, hub writer); the compile then
/// rewrites every page the reorg left dirty.
///
/// # Errors
///
/// Surfaces reorg or compile infrastructure failures. If the reorg fails the
/// compile is not attempted — the fact set is not in a settled state to compile
/// from.
pub async fn run_full(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    policy: &RemPolicy,
) -> Result<FullOutcome> {
    let cycle = rem::run_cycle(pool, tree, Arc::clone(&embedder), llms, policy)
        .await
        .context("rem cycle")?;
    let compile = run_compile(pool, tree, llms, Cadence::Full, &Utc::now().to_rfc3339()).await?;
    Ok(FullOutcome { cycle, compile })
}

// ---------- one-line outcome summaries ----------
//
// The single source of the human one-liner each dream produces, shared by the
// dashboard console (table row + topnav pill) and the server scheduler (journal
// row) so the two surfaces can never word the same outcome differently.

/// The failure tail of a compile summary: empty when every page compiled
/// clean, otherwise the failed/degraded counts — so a run that completed but
/// left pages failed or degraded **stops reading as plain ok** in the journal
/// row (the same counts land structured in `dream_runs.pages_failed` /
/// `pages_degraded` via [`journal_counts`]).
fn failure_note(c: &CompileReport) -> String {
    let (failed, degraded) = (c.errors.len(), c.degraded.len());
    if failed == 0 && degraded == 0 {
        String::new()
    } else {
        format!(" — {failed} pages FAILED · {degraded} degraded")
    }
}

/// The structured `(pages_failed, pages_degraded)` pair a dream-journal row
/// records, from the run's compile report.
///
/// `None` when the compile step did not run — an unpromoted light tick, or a
/// run that errored before compiling. One helper for the scheduler and the
/// dashboard so the counts can never be derived two different ways.
#[must_use]
pub fn journal_counts(compile: Option<&CompileReport>) -> (i64, i64) {
    compile.map_or((0, 0), |c| (c.pages_failed(), c.pages_degraded()))
}

/// One-line summary of a light dream: promotion counts, plus the compile counts
/// when the promotion dirtied pages and a `cronista` was wired.
#[must_use]
pub fn summarize_light(out: &LightOutcome) -> String {
    out.compile.as_ref().map_or_else(
        || {
            format!(
                "promoted {} · superseded {} · skip-dup {} · scanned {} — no recompilation (nothing new, or chronicler not configured)",
                out.light.promoted, out.light.superseded, out.light.skipped_dup, out.light.scanned,
            )
        },
        |c| {
            format!(
                "promoted {} · superseded {} · skip-dup {} · scanned {} — then compiled {} pages ({} lists, {} hubs, {} unchanged){}",
                out.light.promoted,
                out.light.superseded,
                out.light.skipped_dup,
                out.light.scanned,
                c.leaves,
                c.lists,
                c.hubs,
                c.unchanged,
                failure_note(c),
            )
        },
    )
}

/// One-line summary of a narrative compile pass.
#[must_use]
pub fn summarize_compile(report: &CompileReport) -> String {
    format!(
        "compiled {} pages · {} lists · {} hubs · {} unchanged{}",
        report.leaves,
        report.lists,
        report.hubs,
        report.unchanged,
        failure_note(report),
    )
}

/// One-line summary of a full dream: the reorg cycle counts plus the compile
/// counts.
#[must_use]
pub fn summarize_full(out: &FullOutcome) -> String {
    let husks = if out.cycle.husk_gc.removed.is_empty() {
        String::new()
    } else {
        format!(" · husk-gc {}", out.cycle.husk_gc.removed.len())
    };
    format!(
        "cycle {} · dedup {} · auto-promote {} · comments applied {}{husks} — then compiled {} pages ({} lists, {} hubs){}",
        out.cycle.cycle_id,
        out.cycle.revisor.applied.len(),
        out.cycle.auto_promote.applied.len(),
        out.cycle.briefing_processor.facts_corrected
            + out.cycle.briefing_processor.facts_added
            + out.cycle.briefing_processor.facts_removed
            + out.cycle.briefing_processor.facts_moved,
        out.compile.leaves,
        out.compile.lists,
        out.compile.hubs,
        failure_note(&out.compile),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;

    async fn setup() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    fn bag<'a>(hub: &'a FakeLlmBackend, rev: &'a FakeLlmBackend) -> RemLlms<'a> {
        RemLlms {
            hub_writer: hub,
            revisor: rev,
            auto_promote: None,
            apply: None,
            comment_applier: None,
            cronista: None,
            navigator: None,
        }
    }

    /// The compile step is gated on the `cronista` slot: absent, [`run_compile`]
    /// returns an empty report instead of running the planner/Cronista. This is
    /// the gate the scheduler, the CLI, and the dashboard all rely on, so it is
    /// pinned here at the single composition site.
    #[tokio::test]
    async fn run_compile_is_a_clean_noop_without_cronista() {
        let (_dir, tree, pool) = setup().await;
        let hub = FakeLlmBackend::new("hub", "# index\n");
        let rev = FakeLlmBackend::new("rev", "noop");
        let report = run_compile(
            &pool,
            &tree,
            &bag(&hub, &rev),
            Cadence::Full,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile must succeed on an empty workdir");
        assert_eq!(report.leaves, 0);
        assert_eq!(report.hubs, 0);
    }

    /// The Conciliatore runs at BOTH cadences (placement-time near-synonym
    /// resistance — a light-path page must pass the redirect check before it
    /// materialises), on the cadence's tier: the configured
    /// `rem_dedup_semantic` (revisor) slot at REM, the ingest tier at light,
    /// falling back to the revisor slot on a Flash-less deployment.
    #[test]
    fn conciliatore_runs_at_both_cadences_on_the_cadence_tier() {
        let strong = FakeLlmBackend::new("pro", "x");
        let flash = FakeLlmBackend::new("flash", "x");
        assert_eq!(
            conciliatore_backend(Cadence::Full, &strong, Some(&flash)).model_id(),
            "pro"
        );
        assert_eq!(
            conciliatore_backend(Cadence::Light, &strong, Some(&flash)).model_id(),
            "flash"
        );
        assert_eq!(
            conciliatore_backend(Cadence::Light, &strong, None).model_id(),
            "pro"
        );
    }

    /// Tier per cadence (strong model ONLY at REM): the full pass uses
    /// the strong slot; the light dream uses the Flash (ingest-tier) backend,
    /// and falls back to the strong slot only when no ingest slot is wired.
    #[test]
    fn tier_backend_uses_flash_only_in_light() {
        let strong = FakeLlmBackend::new("pro", "x");
        let flash = FakeLlmBackend::new("flash", "x");
        // Full → always the strong slot, even when a Flash backend exists.
        assert_eq!(
            tier_backend(Cadence::Full, &strong, Some(&flash)).model_id(),
            "pro"
        );
        // Light → the Flash backend when present.
        assert_eq!(
            tier_backend(Cadence::Light, &strong, Some(&flash)).model_id(),
            "flash"
        );
        // Light with no ingest slot → falls back to the strong slot.
        assert_eq!(
            tier_backend(Cadence::Light, &strong, None).model_id(),
            "pro"
        );
    }

    /// A compile that left pages failed or degraded must stop reading as
    /// plain ok: the one-line summary carries the counts, and
    /// [`journal_counts`] hands the same numbers to the journal row's
    /// structured columns. A clean report keeps the summary clean.
    #[test]
    fn summaries_and_journal_counts_surface_failed_and_degraded_pages() {
        let mut report = CompileReport {
            leaves: 3,
            ..CompileReport::default()
        };
        assert!(
            !summarize_compile(&report).contains("FAILED"),
            "clean compile → no failure note: {}",
            summarize_compile(&report)
        );
        assert_eq!(journal_counts(Some(&report)), (0, 0));
        assert_eq!(journal_counts(None), (0, 0));

        report
            .errors
            .push("famiglia_bruno_battaglia: boom".to_owned());
        report.degraded.push("salute: degraded append".to_owned());
        let summary = summarize_compile(&report);
        assert!(
            summary.contains("1 pages FAILED · 1 degraded"),
            "failure note present: {summary}"
        );
        assert_eq!(journal_counts(Some(&report)), (1, 1));
    }

    /// Nothing buffered ⇒ promotion is a no-op ⇒ the compile is skipped
    /// (`compile == None`), so the strong model is never touched on an idle
    /// light dream.
    #[tokio::test]
    async fn run_light_promotes_nothing_and_skips_compile_on_empty_workdir() {
        let (_dir, tree, pool) = setup().await;
        let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", 4));
        let outcome = run_light(&pool, &tree, embedder, None, &LightPolicy::default())
            .await
            .expect("light must succeed on an empty workdir");
        assert_eq!(outcome.light.scanned, 0);
        assert!(outcome.compile.is_none());
    }
}
