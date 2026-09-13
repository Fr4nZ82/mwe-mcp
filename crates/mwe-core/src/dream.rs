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
//!   Conciliatore → Architetto → Cronista → Revisore) over the dirty pages.
//! - [`run_light`] — the cheap, frequent dream: promote buffered captures into
//!   `fact_index`, compile the pages that went dirty, then read those pages
//!   back and correct what they say ([`rem::judge_fresh_pages`]).
//! - [`run_full`] — the nightly / on-demand dream: a complete [`rem::run_cycle`]
//!   reorg (dedup, auto-promote, archive, **parked-comment application**),
//!   then a review of the prose already on disk, then ONE compile pass that
//!   answers both — the claims that were waiting and the placements the
//!   review asked to re-judge. Looking before writing is what makes the night
//!   self-concluding: the night puts right what the day wrote, instead of
//!   noticing it a moment too late. A second review closes the pass from
//!   above and notes what a further night should improve (founder,
//!   2026-08-26).
//!
//! The compile step is gated on the `cronista` slot: absent ⇒ it is skipped and
//! facts stay buffered/promoted but unwritten. The prose is the product of the
//! strong model, so that arm is a half-wired install showing, not a mode.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::warn;

use crate::capture_buffer;
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
#[derive(Debug, Default)]
pub struct LightOutcome {
    /// Set when the deployment's daily budget stopped this round
    /// before it began. Every other field is then empty, and the
    /// summary says this instead of reading as a quiet tick.
    pub budget_stop: Option<String>,
    /// Captures → `fact_index` promotion report.
    pub light: LightCycleReport,
    /// Narrative recompile of the pages the promotion dirtied, if it ran.
    pub compile: Option<CompileReport>,
}

/// Outcome of a full dream: the reorg cycle report, plus the compile report.
#[derive(Debug, Default)]
pub struct FullOutcome {
    /// Set when the deployment's daily budget stopped this round
    /// before it began. Every other field is then empty, and the
    /// summary says this instead of reading as a clean night.
    pub budget_stop: Option<String>,
    /// The reorg sub-jobs + parked-comment application.
    pub cycle: RemCycleReport,
    /// Narrative recompile of every page the reorg left dirty.
    pub compile: CompileReport,
    /// The closing pass over whatever was still waiting after that — empty on
    /// the usual night. See [`run_closing_pass`].
    pub closing: CompileReport,
}

/// Which dream cadence is driving a compile pass.
///
/// The distinction lets the frequent, cheap light dream run on the cheap
/// ingest tier while the nightly REM runs at full strong-model quality. Both
/// cadences place new facts with the Cartografo — the light one on the cheap
/// tier and only over what the user did not name — but the **re-open park**,
/// where the reviewer nominates carried placements for a second judgement, is
/// answered by the strong pass alone. A manual/operator compile runs at full
/// quality.
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
/// the cheap ingest-tier backend. Factored out so the policy is pinned by a
/// unit test, not buried in `run_compile`.
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
/// same one the classifier runs on. Factored out so the tier policy is pinned
/// by a unit test.
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

/// Pick how NEW facts are placed, per cadence.
///
/// LIGHT honours every page the USER named and hands only the remainder to the
/// Cartografo on the cheap ingest tier — the half that gives the write side its
/// structure back within the hour instead of overnight. With no ingest slot
/// there is no cheap tier to run it on — a half-wired install, and what it
/// does meanwhile is the deterministic
/// half alone. FULL runs the strong Cartografo over everything, and it alone
/// answers the re-open park (see [`planner::build_wiki_plan`]); with no strong
/// slot only the identity fallback runs, which is a broken install showing.
///
/// Factored out, like [`tier_backend`] beside it, so the policy is pinned by a
/// unit test instead of being buried in `run_compile` where changing it goes
/// unnoticed by every test in the suite.
fn placement_for<'a>(
    cadence: Cadence,
    flash: Option<&'a dyn LlmBackend>,
    strong: Option<&'a dyn LlmBackend>,
) -> NewFactPlacement<'a> {
    match cadence {
        Cadence::Light => flash.map_or(
            NewFactPlacement::Ingest,
            NewFactPlacement::NamedThenCartografo,
        ),
        Cadence::Full => strong.map_or(
            NewFactPlacement::OrphanFallback,
            NewFactPlacement::Cartografo,
        ),
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
/// confirmer tier) and the Cronista its own slot. In [`Cadence::Light`] every stage runs on the cheap ingest-tier
/// (Flash) backend via [`tier_backend`], the Cartografo included
/// ([`NewFactPlacement::NamedThenCartografo`], which reaches only the facts the
/// user did not name a page for); the strong tier is REM-only.
///
/// Nothing runs at all while the deployment's daily budget is reached:
/// the report comes back with [`CompileReport::budget_stop`] set and every
/// count zero. Without that check the Cronista would be asked once per
/// page, be refused once per page, and climb the per-page failure ledger
/// into `compile_failure_streak` notices about a compiler that is working
/// perfectly well.
///
/// # Errors
///
/// Surfaces planner / compiler infrastructure failures. The reviewer is
/// non-blocking: its findings are logged, never returned as an error.
pub async fn run_compile(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    cadence: Cadence,
    now: &str,
) -> Result<CompileReport> {
    if let Some(budget_stop) = budget_stop().await {
        return Ok(CompileReport {
            budget_stop: Some(budget_stop),
            ..CompileReport::default()
        });
    }
    let placement = placement_for(cadence, llms.apply, llms.auto_promote);
    // No judge: this is the narrative compile on its own. The judge belongs to
    // a round that WRITES facts — the light dream and the night — and it reads
    // what that round wrote.
    compile_with(pool, tree, embedder, llms, cadence, placement, now, None).await
}

/// The last pass of the night: place what every earlier pass declined.
///
/// **The queue has to end the night empty** (founder, 2026-08-22: *«il REM
/// deve svuotare il buffer»*). Every other pass may answer "nothing here fits
/// this yet", because another pass comes after it. This one has nothing after
/// it, so the same answer costs the claim a whole day — and a thing said once
/// is exactly the kind of thing that would wait for weeks for four more like
/// it. So the strong Cartografo sees the leftovers with the whole forest in
/// view, told it is last, and allowed to open a page for a single fact:
/// *«la pagina risultante con una sola frase poi crescerà oppure sarà rivista
/// le notti successive»*. A one-fact page is a state of passage, and
/// `run_page_merge` is the net under it — it nominates a page of any mass.
///
/// Costs one `COUNT` on the usual night, when the nightly pass already placed
/// everything.
///
/// **One bound it does not lift:** `LightPolicy::max_promotions_per_cycle`
/// caps how many claims one pass reads, so a queue longer than that empties
/// over several nights rather than one. That cap is a guard on the embedder,
/// not a placement rule, and hitting it is reported separately from a claim
/// the model declined — the two mean different things.
///
/// # Errors
///
/// As [`run_compile`].
pub async fn run_closing_pass(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    now: &str,
) -> Result<CompileReport> {
    let Some(strong) = llms.auto_promote else {
        tracing::debug!("dream: rem_promotions slot unconfigured — no closing pass");
        return Ok(CompileReport::default());
    };
    let waiting = capture_buffer::count_buffered(pool)
        .await
        .context("closing pass: count buffered")?;
    if waiting == 0 {
        return Ok(CompileReport::default());
    }
    tracing::info!(waiting, "dream: closing pass over the claims still waiting");
    let report = compile_with(
        pool,
        tree,
        embedder,
        llms,
        Cadence::Full,
        planner::NewFactPlacement::ClosingCartografo(strong),
        now,
        // The night has judged already, in `run_cycle`.
        None,
    )
    .await?;
    // The one outcome this pass exists to prevent. Not an error — a claim is
    // never lost, and the next night tries again — but it is the thing to look
    // at, so it is said once, loudly, with the counter that says how long each
    // one has been going round.
    let over_cap = waiting.saturating_sub(i64::try_from(report.queue.scanned).unwrap_or(i64::MAX));
    if over_cap > 0 {
        tracing::info!(
            waiting,
            read = report.queue.scanned,
            over_cap,
            "closing pass: the per-pass cap held some back — they go to the next night"
        );
    }
    if report.queue.left_waiting > 0 {
        match capture_buffer::find_all_buffered(pool, 20).await {
            Ok(rows) => {
                let stuck: Vec<String> = rows
                    .iter()
                    .map(|c| format!("{} (declined {}×)", c.capture_id, c.placement_attempts))
                    .collect();
                warn!(
                    left_waiting = report.queue.left_waiting,
                    stuck = stuck.join(", "),
                    "closing pass: the queue did not empty"
                );
            },
            Err(e) => warn!(
                left_waiting = report.queue.left_waiting,
                error = %e,
                "closing pass: the queue did not empty, and the leftovers could not be read"
            ),
        }
    }
    Ok(report)
}

/// **The hour reads back the pages it just wrote onto.**
///
/// The prose exists by the time this runs, so it is the first moment the
/// claims of the round can be read the way a person would read them: woven
/// into the page, beside the facts that were already there. That is the whole
/// of what the classifier could not do, having seen one turn and nothing
/// around it.
///
/// What the judge corrects is corrected in the ROW, within the hour: the
/// errand carries the day it was true of, the request something on the page
/// answered is finished, the role that runs out is a state with an end.
/// Everything that reads validity is then right — what is coming up, a dated
/// question, the assistant asking again for something already done — instead
/// of being right tomorrow morning. The PROSE catches up at the next round,
/// because validity is part of the page fingerprint and a judged page comes
/// back dirty.
///
/// It runs on the `rem_dedup_semantic` slot, the one this round already asks
/// its other confirmations of, and it asks three of the night's questions about
/// a fact and none of the night's two about the page ([`rem::JudgementDepth`]).
/// What model is in that slot is the operator's to choose.
///
/// Never fatal: the compile is what the hour owes, and a page this pass could
/// not read is still unjudged, so tonight reads it.
async fn read_back_what_the_round_wrote(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    placed: &[planner::FactForPage],
    rem_policy: &RemPolicy,
    now: &str,
) -> rem::PageJudgementReport {
    let fresh: Vec<crate::types::FactId> = placed.iter().map(|f| f.fact_id.clone()).collect();
    match rem::judge_fresh_pages(
        pool,
        tree,
        embedder,
        llms.revisor,
        &format!("light-{now}"),
        rem_policy,
        &fresh,
    )
    .await
    {
        Ok(judged) => {
            tracing::info!(
                pages = judged.pages_read,
                changed = judged.changed.len(),
                refused = judged.refused.len(),
                left = judged.pages_left,
                "light dream: the hour read back the pages it wrote onto"
            );
            judged
        },
        Err(e) => {
            tracing::warn!(error = %e, "light dream: page judgement skipped");
            rem::PageJudgementReport::default()
        },
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one compile: what to place, who places it, at which cadence, and whether the hour judges what it just wrote"
)]
async fn compile_with(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    cadence: Cadence,
    placement: planner::NewFactPlacement<'_>,
    now: &str,
    judging: Option<&RemPolicy>,
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
    // Placement of NEW facts per cadence.
    //
    // LIGHT honours every page the USER named — a list, a container asked for
    // by name — deterministically, then hands the remainder to the Cartografo
    // on the cheap ingest tier. The second half is what gives the write side
    // its structure back: since the classifier stopped proposing a page for
    // prose, a fact with no name of its own had nowhere to go but the wiki's
    // buffer, and the strong Cartografo only ever looked at it the next night
    // — by which time the light build had already settled it there, so the
    // carry-over kept it. With no ingest slot wired there is no cheap tier to
    // run it on: a half-wired install, and the light pass does the
    // deterministic half alone until somebody fixes it.
    //
    // FULL runs the strong Cartografo (the `rem_promotions` slot) over
    // everything, and it alone answers the re-open park; a Full pass with no
    // strong slot configured leaves only the deterministic identity fallback
    // (the historical `None` behaviour).
    tracing::debug!(
        cadence = ?cadence,
        placement = placement.label(),
        "dream compile: placement policy"
    );
    // Conciliatore at BOTH cadences (placement-time near-synonym resistance,
    // see `conciliatore_backend`): the light dream runs it on the ingest tier
    // so a page born on the light path still passes the redirect check before
    // it materialises; the nightly REM runs it on its configured
    // `rem_dedup_semantic` slot (`llms.revisor`).
    let conciliatore = Some(conciliatore_backend(cadence, llms.revisor, flash));
    // The queue, screened: the claims waiting in `capture_buffer` minus the
    // duplicates. They are not memories yet — the plan below decides where each
    // one goes, and `materialise` writes the rows once it has (founder,
    // 2026-08-18: first the address, then the row and the prose together).
    let mut queue = dream_light::screen_queue(pool, tree, &LightPolicy::default())
        .await
        .context("light dream: screen queue")?;
    let plan = planner::build_wiki_plan(pool, tree, placement, conciliatore, now, &queue.for_plan)
        .await
        .context("planner")?;
    // Between the plan and the prose: each placed claim becomes a `fact_index`
    // row addressed to the page it landed on, moments before the compile writes
    // that page. A claim the plan could not place keeps waiting.
    dream_light::materialise(pool, tree, &embedder, &mut queue, &plan, now)
        .await
        .context("light dream: materialise")?;
    let mut report = compiler::compile_dirty_pages(pool, tree, &plan, cronista, cadence, now)
        .await
        .context("compiler")?;
    report.queue = queue.report;
    if cadence == Cadence::Light
        && let Some(rem_policy) = judging
    {
        report.judged = read_back_what_the_round_wrote(
            pool,
            tree,
            embedder,
            llms,
            &queue.for_plan,
            rem_policy,
            now,
        )
        .await;
    }
    // Deterministic, no-LLM pass that syncs each wiki's
    // `_meta.keywords["topics"]` to the union of its facts' topics, and each
    // page's testata to the topics of the facts on it. The **page** cards are
    // what a turn reaches; the wiki-level union is write-side vocabulary. No
    // reader is shown a wiki or a list of them.
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
    // The eagle's flight: the review that closes the pass, looking down at
    // the structure this build has just made. What it finds is for a further
    // night — the shape a night of moving facts leaves can be sound and still
    // want straightening, and saying so is the point of looking from up here.
    // Tonight's own healing already happened, at the head of the pass.
    //
    // The over-budget cards are the compiler's own finding, not the
    // reviewer's — it measures a card at the moment it writes it, where the
    // served length is still in hand. They travel the same bridge because
    // they ask for the same thing: put this page's placements back in front
    // of the Cartografo.
    let over_budget: Vec<String> = report
        .cards_over_budget
        .iter()
        .map(|(slug, _)| slug.clone())
        .collect();
    match reviewer::review(tree, &plan, &identity, now) {
        Ok(r) if !r.is_clean() || !over_budget.is_empty() => {
            if !r.is_clean() {
                warn!(
                    findings = r.finding_count(),
                    cross_subject_bloat = r.cross_subject_bloat.len(),
                    spent_card_facts = r.spent_card_facts.len(),
                    "dream compile: reviewer found issues"
                );
            }
            park_bridge_signals(pool, tree, &plan, &r, &over_budget).await;
        },
        Ok(_) => {},
        Err(e) => {
            warn!(error = %e, "dream compile: reviewer failed");
            // A card past its ceiling is cut on every turn until something
            // moves material off it, so the re-open is parked even when the
            // review that usually carries it could not run.
            if !over_budget.is_empty() {
                park_bridge_signals(
                    pool,
                    tree,
                    &plan,
                    &reviewer::ReviewReport::default(),
                    &over_budget,
                )
                .await;
            }
        },
    }
    Ok(report)
}

/// The findings→healing bridge: persist what a review (and the
/// compile-failure ledger) learned, as nominations on the plan.
///
/// **Two reviews park here, and they are two different jobs.**
/// [`review_before_placing`] runs at the head of the night, so what it finds
/// is healed by the build a moment later — the night puts right what the day
/// wrote. The review at the compile's tail is the eagle's flight: it looks
/// down at the structure the night has just built and notes what a further
/// pass should improve — moving facts can leave a shape that is sound but
/// crooked, and the honest answer is *«good enough for now, and tomorrow
/// night I straighten it»* (founder, 2026-08-26). Its findings are for the
/// next cycle by design, not for want of a chance to act.
///
/// Either way the park is on the persisted plan (the `force_dirty` pattern),
/// and either way these are nominations, never verdicts:
///
/// - each `cross_subject_bloat` fact → a **refile candidate** (the refile
///   judge still decides, and refuses what does not apply);
/// - each `cross_subject_bloat` page, each `oversized` page, every identity
///   card the compiler wrote past its ceiling, every card still carrying a
///   fact that has stopped holding, plus every page failing its compile
///   repeatedly (the ledger's streak) → a **placement re-open**, so
///   the Cartografo re-judges the carried placements with the mass +
///   identity + container signals live (split-by-mass can finally fire on an
///   old page; a fact-bearing container drains and is garbage-collected once
///   empty; an over-budget card sheds what is not always-on core; a card
///   whose fact lines now carry `SPENT=` sheds what has stopped being true).
///   A parked re-open is consumed only by a build that runs the Cartografo —
///   a light build carries it (`planner::build_wiki_plan`).
///
/// Best-effort like the review itself: a park failure only delays healing.
async fn park_bridge_signals(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &planner::CompilationPlan,
    r: &reviewer::ReviewReport,
    cards_over_budget: &[String],
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
    reopen.extend(r.oversized_pages.iter().map(|(s, _)| s.clone()));
    // An identity card past its ceiling. It is the one page a re-open cannot
    // heal by splitting — a card is never split, because recall serves it
    // whole — so what the re-open buys here is the other repair: the
    // Cartografo re-judges every placement on it against the one criterion
    // for what a card holds, and moves off what is not always-on core. The
    // alternative is not "it stays whole": it is already being CUT when
    // served, in an order nobody chose.
    reopen.extend(cards_over_budget.iter().cloned());
    // A card fact that has stopped holding. Same repair, different reason:
    // the always-on core is what a consumer is handed on EVERY turn, so a
    // spent claim there is read as true until something moves it. The
    // Cartografo re-homes it onto a topic page, where it stays readable as
    // what happened — a card is where a fact stops belonging, never where it
    // stops existing.
    reopen.extend(r.spent_card_facts.iter().map(|(slug, _, _)| slug.clone()));
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

/// Review what is already written and park what needs re-judging, **before**
/// the night's one build places anything.
///
/// The night's corrections and its new claims are the same job and want the
/// same judgement: a claim arriving tonight may belong on a page this review
/// is about to re-open, and a page re-opened after the build was settled is a
/// page that waits a day. So the reviewer reads the plan and the prose the
/// last compile left on disk — both are on disk before this night starts —
/// and its findings park on that plan, where [`run_compile`]'s strong
/// Cartografo consumes them in the same pass.
///
/// The compiler's own over-budget finding is not here and cannot be: a card
/// is measured past its ceiling **as it is written**, so that one is produced
/// by the compile and parks for the next night's review, which is this one.
///
/// Best-effort throughout, like the review at the compile's tail: a night
/// that cannot review still places its claims.
async fn review_before_placing(pool: &SqlitePool, tree: &WikiTree, now: &str) {
    let plan = match planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        // Nothing compiled yet: there is no prose to review and no carried
        // placement to re-judge.
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "dream: pre-placement review skipped — plan load failed");
            return;
        },
    };
    let identity = match reviewer::IdentityContext::load(pool, tree).await {
        Ok(ctx) => ctx,
        Err(e) => {
            warn!(error = %e, "dream: pre-placement review without identity context");
            reviewer::IdentityContext::default()
        },
    };
    match reviewer::review(tree, &plan, &identity, now) {
        Ok(r) if !r.is_clean() => {
            warn!(
                findings = r.finding_count(),
                cross_subject_bloat = r.cross_subject_bloat.len(),
                spent_card_facts = r.spent_card_facts.len(),
                "dream: pre-placement review found issues — healing them in tonight's build"
            );
            park_bridge_signals(pool, tree, &plan, &r, &[]).await;
        },
        Ok(_) => {},
        Err(e) => warn!(error = %e, "dream: pre-placement review failed"),
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
/// the promotion runs, so a half-wired install surfaces as pages that never
/// compile rather than as a panic.
///
/// After the prose, the hour **reads back the pages it wrote onto**
/// ([`crate::rem::judge_fresh_pages`]): an errand that is spent by bedtime
/// gets its end, a request another fact on the page finished gets closed, a
/// role filed as who somebody IS is refiled as what they are doing.
/// `rem_policy` is where that pass reads its cap, and it is the same handle
/// the REM settings panel swaps in place, so a change takes effect on the next
/// hour.
///
/// Nothing runs at all while the deployment's daily budget is reached:
/// the outcome comes back with [`LightOutcome::budget_stop`] set and every
/// report empty, and the summary says that instead of a quiet tick.
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
    rem_policy: &RemPolicy,
) -> Result<LightOutcome> {
    // Hygiene first, and before the budget gate: the kept write outcomes of
    // re-delivered turns expire on a clock, and the write path only prunes
    // them when somebody writes. On a deployment nobody is talking to — or one
    // that has spent its budget — this is the only thing that empties the
    // table, so it must not be behind a stop that means "do not spend".
    let expired_replies = crate::ingest_replay::prune(pool, Utc::now()).await;
    if expired_replies > 0 {
        tracing::info!(expired_replies, "light dream: expired turn outcomes swept");
    }
    if let Some(budget_stop) = budget_stop().await {
        return Ok(LightOutcome {
            budget_stop: Some(budget_stop),
            ..LightOutcome::default()
        });
    }
    // Skip the (expensive) compile when the queue is empty: a plan with no new
    // claims is a no-op, and checking here keeps the strong model untouched.
    // The count is the whole gate now — the promotion itself lives inside the
    // compile, because a claim becomes a fact only once its page is decided.
    let waiting = usize::try_from(capture_buffer::count_buffered(pool).await?).unwrap_or(0);
    let compile = match (waiting > 0, llms) {
        (true, Some(llms)) => {
            let placement = placement_for(Cadence::Light, llms.apply, llms.auto_promote);
            Some(
                compile_with(
                    pool,
                    tree,
                    embedder.clone(),
                    llms,
                    Cadence::Light,
                    placement,
                    &Utc::now().to_rfc3339(),
                    Some(rem_policy),
                )
                .await?,
            )
        },
        _ => None,
    };
    // No prose writer configured: drain the queue anyway, deterministically.
    // Each claim lands on the page the user's own turn named, or on the orphan
    // fallback's, with its render pending — because the alternative is a queue
    // that grows for ever while the recall fresh slot, a ranked top-K, quietly
    // stops offering the older half of it.
    let light = match &compile {
        Some(c) => c.queue.clone(),
        None if waiting > 0 => dream_light::drain_deterministically(
            pool,
            tree,
            &embedder,
            policy,
            &Utc::now().to_rfc3339(),
        )
        .await
        .context("light dream: deterministic drain")?,
        None => LightCycleReport::default(),
    };
    // Retirement hygiene: excise retired-fact regions from pages OUTSIDE the
    // compilation plan (`@rules.md`, husks — plan pages self-clean at their
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
    Ok(LightOutcome {
        budget_stop: None,
        light,
        compile,
    })
}

/// The sentence a round says when it does not run, or `None` when the
/// deployment may spend.
///
/// Checked once, before the first sub-job, because the alternative is
/// ugly and misleading: every LLM stage would take the refusal
/// separately, log itself as "unavailable — skipped", and the run of
/// failures would end the cycle as an infrastructure fault. A night that
/// is not run because the operator set a daily budget is not a night
/// that failed, and it must not read as one anywhere.
///
/// **The whole round, not the paid half of it.** A deployment whose
/// slots are split between a metered provider and a local model could in
/// principle run the free stages, and it deliberately does not: the
/// stages settle the fact set for the ones behind them, which is why
/// `run_full` already refuses to compile from a reorg that failed. Half
/// a round leaves the memory mid-reorganisation for a whole day to save
/// work the operator asked to pause.
async fn budget_stop() -> Option<String> {
    let guard = crate::budget::global()?;
    let state = guard.state().await.ok()?;
    if !state.stopped() {
        return None;
    }
    let message = format!("skipped: {}", state.stop_message());
    tracing::info!(
        day = %state.day,
        spent = state.spent,
        "dream: the daily budget is spent — this round is skipped"
    );
    Some(message)
}

/// Run one full dream: settle the facts, read what is written, then write once.
///
/// The [`rem::run_cycle`] reorg settles the fact set (dedup, auto-promote,
/// archive, **parked-comment application**). Then [`review_before_placing`]
/// reads the prose already on disk while tonight's claims stay in the queue,
/// so the single compile that follows is handed the whole night's work at
/// once: place what is waiting, and re-home what the review says sits wrong.
/// The compile rewrites every page either of them left dirty.
///
/// Nothing runs at all while the deployment's daily budget is reached:
/// the outcome comes back with [`FullOutcome::budget_stop`] set and every
/// report empty, and the summary says that instead of a clean night.
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
    if let Some(budget_stop) = budget_stop().await {
        return Ok(FullOutcome {
            budget_stop: Some(budget_stop),
            ..FullOutcome::default()
        });
    }
    let cycle = rem::run_cycle(pool, tree, Arc::clone(&embedder), llms, policy)
        .await
        .context("rem cycle")?;
    let now = Utc::now().to_rfc3339();
    // Look before writing. The claims that came in today stay in the queue
    // while the night reads the prose already on disk, so the single build
    // below is handed BOTH jobs at once: place what is waiting, and re-judge
    // the pages this review says were placed wrong. Reviewing after the build
    // instead would find what the build had just settled, and the finding
    // would keep until tomorrow.
    review_before_placing(pool, tree, &now).await;
    let compile = run_compile(pool, tree, embedder.clone(), llms, Cadence::Full, &now).await?;
    // Last, and it must be last: it is the pass that answers for whatever the
    // others left, so nothing may run behind it and put something back.
    let closing = run_closing_pass(pool, tree, embedder, llms, &now).await?;
    Ok(FullOutcome {
        budget_stop: None,
        cycle,
        compile,
        closing,
    })
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
    if let Some(stop) = &out.budget_stop {
        return stop.clone();
    }
    out.compile.as_ref().map_or_else(
        || {
            format!(
                "promoted {} · superseded {} · skip-dup {} · scanned {} — no recompilation (nothing new, or chronicler not configured)",
                out.light.promoted, out.light.superseded, out.light.skipped_dup, out.light.scanned,
            )
        },
        |c| {
            format!(
                "promoted {} · superseded {} · skip-dup {} · scanned {}{} — then compiled {} pages ({} lists, {} unchanged){}",
                out.light.promoted,
                out.light.superseded,
                out.light.skipped_dup,
                out.light.scanned,
                judgement_note(&c.judged),
                c.leaves,
                c.lists,
                c.unchanged,
                failure_note(c),
            )
        },
    )
}

/// One-line summary of a narrative compile pass.
#[must_use]
pub fn summarize_compile(report: &CompileReport) -> String {
    if let Some(stop) = &report.budget_stop {
        return stop.clone();
    }
    format!(
        "compiled {} pages · {} lists · {} unchanged{}",
        report.leaves,
        report.lists,
        report.unchanged,
        failure_note(report),
    )
}

/// One-line summary of a full dream: the reorg cycle counts plus the compile
/// counts.
#[must_use]
pub fn summarize_full(out: &FullOutcome) -> String {
    if let Some(stop) = &out.budget_stop {
        return stop.clone();
    }
    let husks = if out.cycle.husk_gc.removed.is_empty() {
        String::new()
    } else {
        format!(" · husk-gc {}", out.cycle.husk_gc.removed.len())
    };
    // Named only on a night that decided one, like husk-gc: a rail is the
    // rarest thing the cycle does and the line must not carry a zero for it.
    let rails = if out.cycle.rail_writer.written.is_empty() {
        String::new()
    } else {
        format!(" · rails {}", out.cycle.rail_writer.written.len())
    };
    let judged = judgement_note(&out.cycle.page_judge);
    format!(
        "cycle {} · dedup {} · auto-promote {} · comments applied {}{husks}{rails}{judged} — then compiled {} pages ({} lists){}{}",
        out.cycle.cycle_id,
        out.cycle.revisor.applied.len(),
        out.cycle.auto_promote.applied.len(),
        out.cycle.briefing_processor.facts_corrected
            + out.cycle.briefing_processor.facts_added
            + out.cycle.briefing_processor.facts_removed
            + out.cycle.briefing_processor.facts_moved,
        out.compile.leaves,
        out.compile.lists,
        failure_note(&out.compile),
        closing_note(&out.closing),
    )
}

/// What the page judge did, for the one-line summary.
///
/// Empty on a pass that read its pages and agreed with all of them — the
/// ordinary outcome, and a zero on the line would bury the passes that decided
/// something. **The pages the cap left out are always named**, because a page
/// nobody read is the one thing here a person may want to act on: it stays
/// unjudged and the next pass (the next hour's, or tonight's) reads it.
fn judgement_note(report: &rem::PageJudgementReport) -> String {
    match (report.changed.len(), report.pages_left) {
        (0, 0) => String::new(),
        (0, left) => format!(" · {left} pages not read back, oldest first"),
        (n, 0) => format!(" · judged {n}"),
        (n, left) => format!(" · judged {n} ({left} pages not read back, oldest first)"),
    }
}

/// What the closing pass did, for the one-line summary — empty on the usual
/// night, when the queue was already empty when it looked.
///
/// A claim it could not place is named here because it is the outcome the pass
/// exists to prevent: a reader of the journal row must not have to go looking
/// for it.
fn closing_note(c: &CompileReport) -> String {
    let q = &c.queue;
    if q.scanned == 0 {
        return String::new();
    }
    let left = if q.left_waiting == 0 {
        " · queue empty".to_owned()
    } else {
        format!(" · {} STILL WAITING", q.left_waiting)
    };
    format!(
        " — closing pass: {} waiting, {} placed{left}",
        q.scanned, q.promoted
    )
}

#[cfg(test)]
mod tests {

    /// The night's review parks its findings BEFORE the night places
    /// anything, so the build that follows heals them.
    ///
    /// This is the ordering the whole pass turns on (founder, 2026-08-26):
    /// review what is already written while tonight's claims wait, then do
    /// the work once. Reviewing at the build's tail instead would leave the
    /// Cartografo that could act on a finding already finished, and the card
    /// wrong for a day.
    ///
    /// What the test pins is that the park is on the plan when the build
    /// would read it: a spent fact on a card, no compile in between.
    #[tokio::test]
    async fn the_review_parks_before_the_night_places_anything() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let id = crate::types::WikiId::parse("frodo").unwrap();
        crate::wiki::create_identity_wiki(&tree, &id, "Frodo", crate::wiki::IdentityKind::User)
            .expect("wiki");
        let tree = WikiTree::open(dir.path()).expect("reopen");

        // A card carrying a claim the engine has closed.
        let mut spent = planner::FactForPage {
            topics: Vec::new(),
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: crate::types::FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d01").unwrap(),
            text: "Frodo wears a wrist brace.".to_owned(),
            fact_type: Some("state".to_owned()),
            subject: "user:frodo".parse().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: "frodo".to_owned(),
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            successor_fact_id: None,
            target_page: None,
            style: None,
            salience: Some("high".to_owned()),
        };
        spent.decay_reason = Some("superseded".to_owned());

        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "frodo".to_owned(),
            planner::PagePlan {
                title: "Frodo".to_owned(),
                description: String::new(),
                style: None,
                primary_facts: vec![spent],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "frodo".to_owned(),
                page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
                slug: "frodo".to_owned(),
            },
        );
        let plan = planner::CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: vec!["frodo".to_owned()],
            generated_at: "2026-08-25T00:00:00Z".to_owned(),
            fact_count: 1,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        planner::save_plan(&tree, &plan).expect("save");

        review_before_placing(&pool, &tree, "2026-09-01T00:00:00Z").await;

        let back = planner::load_previous_plan(&tree)
            .expect("read")
            .expect("plan");
        assert_eq!(
            back.reopen_pages,
            vec!["frodo".to_owned()],
            "the card must be in front of the Cartografo BEFORE tonight's build runs"
        );
        drop(dir);
    }

    /// A card past its ceiling re-opens its own placement, even on a night
    /// the reviewer finds nothing else wrong.
    ///
    /// A card grows past its ceiling on a wiki that is otherwise in good
    /// order — that is the ordinary way it happens — so the bridge cannot key
    /// on the review being dirty. What it keys on is the compiler's own
    /// measurement, taken when the card was written, and the re-open is what
    /// carries it to the Cartografo.
    #[tokio::test]
    async fn an_over_budget_card_reopens_its_placement_on_a_clean_night() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db");
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");

        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "franz".to_owned(),
            planner::PagePlan {
                title: "Franz".to_owned(),
                description: String::new(),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "franz".to_owned(),
                page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
                slug: "franz".to_owned(),
            },
        );
        let plan = planner::CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: vec!["franz".to_owned()],
            generated_at: "2026-08-25T00:00:00Z".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        planner::save_plan(&tree, &plan).expect("save");

        // A clean review, and one card the compiler measured over its ceiling.
        park_bridge_signals(
            &pool,
            &tree,
            &plan,
            &reviewer::ReviewReport::default(),
            &["franz".to_owned()],
        )
        .await;

        let back = planner::load_previous_plan(&tree)
            .expect("read")
            .expect("plan");
        assert_eq!(
            back.reopen_pages,
            vec!["franz".to_owned()],
            "the card's placement must be put back in front of the Cartografo"
        );
        drop(dir);
    }

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

    fn bag(rev: &FakeLlmBackend) -> RemLlms<'_> {
        RemLlms {
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
        let rev = FakeLlmBackend::new("rev", "noop");
        let report = run_compile(
            &pool,
            &tree,
            Arc::new(FakeEmbedder::new("fake", 4)),
            &bag(&rev),
            Cadence::Full,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile must succeed on an empty workdir");
        assert_eq!(report.leaves, 0);
    }

    /// A workdir with one enrolled person, her wiki on disk, and nothing else.
    async fn setup_alice() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).expect("wiki dir");
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .expect("meta");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .expect("enrol");
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    /// One claim nobody named a page for and nobody would place on the
    /// identity card — the shape that waits.
    async fn buffer_one(pool: &SqlitePool, body: &str) -> crate::types::FactId {
        capture_buffer::buffer_capture(
            pool,
            crate::capture::CaptureRequest {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: crate::types::WikiId::parse("alice").unwrap(),
                page: None,
                body: body.to_owned(),
                subject: "user:alice".parse::<crate::types::Principal>().unwrap(),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".to_owned()),
                topics: Vec::new(),
                dedup_threshold: None,
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            },
            None,
        )
        .await
        .expect("buffer")
        .capture_id
    }

    /// A backend that answers by what it was asked.
    ///
    /// One light round calls the cheap slot for three different jobs — where a
    /// claim goes, whether its page is a near-synonym of another, and the
    /// prose — and a test about a FOURTH call has to let those three succeed.
    /// Each entry is a word the asking prompt contains and the answer to give
    /// when it does; an unmatched prompt gets the near-synonym check's «no».
    struct ByPrompt(Vec<(&'static str, String)>);

    #[async_trait::async_trait]
    impl crate::llm::LlmBackend for ByPrompt {
        fn model_id(&self) -> &'static str {
            "by-prompt"
        }

        async fn complete(
            &self,
            request: crate::llm::CompletionRequest,
        ) -> std::result::Result<crate::llm::CompletionResponse, crate::llm::LlmError> {
            let asked = format!("{}{}", request.system.unwrap_or_default(), request.prompt);
            let text = self
                .0
                .iter()
                .find(|(marker, _)| asked.contains(marker))
                .map_or_else(
                    || "{\"same\":false}".to_owned(),
                    |(_, answer)| answer.clone(),
                );
            Ok(crate::llm::CompletionResponse {
                text,
                finish_reason: crate::llm::FinishReason::EndOfTurn,
                usage: crate::llm::CompletionUsage::default(),
            })
        }
    }

    /// One claim whose page the turn itself named — the shape the light round
    /// places without asking anybody.
    async fn buffer_on_page(pool: &SqlitePool, body: &str, page: &str) -> crate::types::FactId {
        capture_buffer::buffer_capture(
            pool,
            crate::capture::CaptureRequest {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: crate::types::WikiId::parse("alice").unwrap(),
                page: Some(std::path::PathBuf::from(page)),
                body: body.to_owned(),
                subject: "user:alice".parse::<crate::types::Principal>().unwrap(),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".to_owned()),
                topics: Vec::new(),
                dedup_threshold: None,
                valid_from: Some("2026-03-14T18:00:00Z".to_owned()),
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            },
            None,
        )
        .await
        .expect("buffer")
        .capture_id
    }

    /// A fact from an earlier round, on a page that therefore exists, and
    /// already carrying the end of its own day.
    async fn plant_ended_fact(
        pool: &SqlitePool,
        tree: &WikiTree,
        page: &str,
        body: &str,
    ) -> crate::types::FactId {
        crate::capture::wiki_capture(
            tree,
            pool,
            Arc::new(FakeEmbedder::new("fake", 4)),
            crate::capture::CaptureRequest {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                wiki_id: crate::types::WikiId::parse("alice").unwrap(),
                page: Some(std::path::PathBuf::from(page)),
                body: body.to_owned(),
                subject: "user:alice".parse::<crate::types::Principal>().unwrap(),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("episode".to_owned()),
                topics: Vec::new(),
                dedup_threshold: Some(0.999),
                valid_from: Some("2026-03-01T21:00:00Z".to_owned()),
                valid_to: Some("2026-03-01T23:59:59Z".to_owned()),
                style: None,
                page_description: None,
                salience: None,
            },
        )
        .await
        .expect("a fact from an earlier round")
        .fact_id
    }

    /// **The hour reads back the page it just wrote onto.**
    ///
    /// «Ha annaffiato le piante» is written with no end, because the turn that
    /// carried it said nothing about one — and a fact with no end is a
    /// standing state, so the memory goes on treating a watering from March as
    /// true today. The night would catch it; this is the same reading, run on
    /// the page the round has just compiled, so the household does not live
    /// with it until the night.
    ///
    /// The page is the unit: the model gets the prose with both facts marked,
    /// and answers by marker. The older fact here already ends, so the verdict
    /// asking to end it again is refused BY NAME rather than applied — which
    /// is what the receipt shows a person who wonders what the judge did.
    #[tokio::test]
    async fn the_light_round_reads_back_the_page_it_wrote_onto() {
        let (_dir, tree, pool) = setup_alice().await;
        let older = plant_ended_fact(&pool, &tree, "casa.md", "Ha chiuso a chiave").await;
        let fresh = buffer_on_page(&pool, "Ha annaffiato le piante", "casa.md").await;

        // The cheap slot does the whole light compile — placing, the
        // near-synonym check, the prose — so it answers by what it was asked.
        let flash = ByPrompt(vec![
            (
                "assignments",
                // Onto the page the earlier fact already opened: recognising a
                // home, not inventing one, so no birth floor stands in the way.
                format!(
                    "{{\"assignments\":[{{\"fact_id\":\"{fresh}\",\"page_slug\":\"casa\"}}],\"new_pages\":[]}}"
                ),
            ),
            (
                "mergedBody",
                "{\"mergedBody\":\"<f1>Ha chiuso a chiave.</f1> <f2>Ha annaffiato le \
                 piante.</f2>\",\"description\":\"La casa di Alice\"}"
                    .to_owned(),
            ),
        ]);
        // Both markers are asked to end. One can, one cannot.
        let judge = FakeLlmBackend::new(
            "judge",
            "{\"verdicts\":{\"f1\":{\"verdict\":\"end\",\"valid_to\":\"2026-03-14\"},\
              \"f2\":{\"verdict\":\"end\",\"valid_to\":\"2026-03-14\"}}}",
        );
        // The hour judges on the `rem_dedup_semantic` slot — here the only
        // handle that is not the cheap one doing the compile, so the test can
        // tell the judge's call from the round's other three.
        let llms = RemLlms {
            revisor: &judge,
            auto_promote: None,
            apply: Some(&flash),
            comment_applier: None,
            cronista: Some(&flash),
            navigator: None,
        };

        let out = run_light(
            &pool,
            &tree,
            Arc::new(FakeEmbedder::new("fake", 4)),
            Some(&llms),
            &LightPolicy::default(),
            &RemPolicy::default(),
        )
        .await
        .expect("light round");

        let compile = out.compile.as_ref().expect("the round compiled");
        assert_eq!(compile.judged.pages_read, 1, "one page, one call");
        assert_eq!(
            compile.judged.changed,
            vec![format!("{fresh} · end")],
            "the fresh fact got its day: {:?}",
            compile.judged
        );
        assert!(
            compile
                .judged
                .refused
                .iter()
                .any(|r| r.contains("already ends")),
            "and the one that already ends was refused by name: {:?}",
            compile.judged.refused
        );
        let row = crate::fact_index::find_by_id(&pool, &fresh)
            .await
            .unwrap()
            .expect("the claim became a fact");
        assert!(
            row.valid_to
                .as_deref()
                .is_some_and(|v| v.starts_with("2026-03-14")),
            "within the hour, not tomorrow morning: {:?}",
            row.valid_to
        );
        assert_eq!(
            crate::fact_index::find_by_id(&pool, &older)
                .await
                .unwrap()
                .expect("row")
                .valid_to
                .as_deref()
                .map(|v| v[..10].to_owned()),
            Some("2026-03-01".to_owned()),
            "the older fact keeps the end it had"
        );

        // And the model read a PAGE: prose, with the facts marked.
        let asked = judge.last_prompt().expect("the judge was asked");
        assert!(
            asked.contains("<f1>") && asked.contains("<f2>"),
            "both facts are nameable: {asked}"
        );
        assert!(
            asked.contains("Ha annaffiato le piante"),
            "including the one this round wrote: {asked}"
        );
    }

    /// **The queue ends the night empty**, and a page for a single fact is how
    /// it does.
    ///
    /// The claim here is the shape that otherwise waits for weeks: nobody
    /// named a page for it, its salience does not reserve the identity card,
    /// and the hourly pass will not coin a page under the birth floor. The
    /// closing pass is the only one allowed to open a page for one fact, and
    /// this is the assertion that it does.
    #[tokio::test]
    async fn the_closing_pass_opens_a_page_for_the_claim_nothing_else_would_place() {
        let (_dir, tree, pool) = setup_alice().await;
        let id = buffer_one(&pool, "Alice ha cominciato il nuoto il martedì.").await;
        assert_eq!(capture_buffer::count_buffered(&pool).await.unwrap(), 1);

        let cartografo = FakeLlmBackend::new(
            "pro",
            format!(
                "{{\"assignments\":[{{\"fact_id\":\"{id}\",\"page_slug\":\"nuoto\"}}],\
                  \"new_pages\":[{{\"slug\":\"nuoto\",\"title\":\"Nuoto\",\"description\":\"Il nuoto di Alice\"}}]}}"
            ),
        );
        let cronista = FakeLlmBackend::new(
            "pro",
            "{\"mergedBody\":\"<f1>Alice ha cominciato il nuoto il martedì.</f1>\",\"description\":\"Il nuoto di Alice\"}",
        );
        let rev = FakeLlmBackend::new("rev", "{\"same\": false}");
        let llms = RemLlms {
            revisor: &rev,
            auto_promote: Some(&cartografo),
            apply: None,
            comment_applier: None,
            cronista: Some(&cronista),
            navigator: None,
        };

        let report = run_closing_pass(
            &pool,
            &tree,
            Arc::new(FakeEmbedder::new("fake", 4)),
            &llms,
            "2026-08-23T02:00:00Z",
        )
        .await
        .expect("closing pass");

        assert_eq!(report.queue.scanned, 1);
        assert_eq!(report.queue.promoted, 1, "{report:?}");
        assert_eq!(report.queue.left_waiting, 0, "{report:?}");
        assert_eq!(
            capture_buffer::count_buffered(&pool).await.unwrap(),
            0,
            "the buffer ends the night empty"
        );

        let row = crate::fact_index::find_by_id(&pool, &id)
            .await
            .unwrap()
            .expect("the claim became a fact");
        assert_eq!(
            row.source_path, "wikis/alice/nuoto.md",
            "on the page the closing pass opened for it, not on the card"
        );
        assert!(
            tree.workdir().join("wikis/alice/nuoto.md").exists(),
            "and the page was written"
        );

        // And the model was told which pass it is — the whole difference
        // between this call and the nightly one is that sentence.
        let shown = cartografo.last_system_prompt().expect("cartografo ran");
        assert!(shown.contains("YOU ARE THE LAST PASS"), "{shown}");
        assert!(
            !shown.contains("LEAVING A FACT UNPLACED IS AN ANSWER"),
            "the pass with nothing after it is not offered that answer: {shown}"
        );
    }

    /// On the usual night the queue is already empty when the closing pass
    /// looks, and it costs one `COUNT` — no plan, no model, no compile.
    #[tokio::test]
    async fn an_empty_queue_costs_the_closing_pass_one_count() {
        let (_dir, tree, pool) = setup_alice().await;
        let cartografo = FakeLlmBackend::new("pro", "{}");
        let cronista = FakeLlmBackend::new("pro", "{}");
        let rev = FakeLlmBackend::new("rev", "{}");
        let llms = RemLlms {
            revisor: &rev,
            auto_promote: Some(&cartografo),
            apply: None,
            comment_applier: None,
            cronista: Some(&cronista),
            navigator: None,
        };
        let report = run_closing_pass(
            &pool,
            &tree,
            Arc::new(FakeEmbedder::new("fake", 4)),
            &llms,
            "2026-08-23T02:00:00Z",
        )
        .await
        .expect("closing pass");
        assert_eq!(report.queue.scanned, 0);
        assert!(
            cartografo.last_system_prompt().is_none(),
            "no claim waiting means no call"
        );
        assert!(cronista.last_system_prompt().is_none());
    }

    /// What the journal row says. Silent when the queue was already empty,
    /// and loud when it did not empty — that is the outcome the pass exists
    /// to prevent, and a reader must not have to go looking for it.
    #[test]
    fn the_closing_pass_reports_itself_only_when_it_had_work() {
        let mut c = CompileReport::default();
        assert_eq!(closing_note(&c), "");

        c.queue.scanned = 3;
        c.queue.promoted = 3;
        assert_eq!(
            closing_note(&c),
            " — closing pass: 3 waiting, 3 placed · queue empty"
        );

        c.queue.promoted = 2;
        c.queue.left_waiting = 1;
        assert_eq!(
            closing_note(&c),
            " — closing pass: 3 waiting, 2 placed · 1 STILL WAITING"
        );
    }

    /// The Conciliatore runs at BOTH cadences (placement-time near-synonym
    /// resistance — a light-path page must pass the redirect check before it
    /// materialises), on the cadence's tier: the configured
    /// `rem_dedup_semantic` (revisor) slot at REM, the ingest tier at light.
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

    /// Placement per cadence, on the shipped policy rather than on an argument
    /// a test wrote by hand.
    ///
    /// The light dream PLACES: it honours the page the user named and hands
    /// the remainder to the Cartografo on the cheap ingest tier. Before this
    /// it settled only what the classifier had already named, which — since
    /// the classifier stopped naming a page for prose — meant every prose fact
    /// waited in the buffer until the next REM.
    ///
    /// Both degradations are part of the policy: no ingest slot ⇒ the light
    /// pass keeps the deterministic half alone; no strong slot ⇒ the full pass
    /// falls back to the identity fallback.
    #[test]
    fn light_places_with_the_cheap_cartografo_and_full_with_the_strong_one() {
        let strong = FakeLlmBackend::new("pro", "x");
        let flash = FakeLlmBackend::new("flash", "x");
        assert_eq!(
            placement_for(Cadence::Light, Some(&flash), Some(&strong)).label(),
            "named-then-cartografo",
            "the light dream places the facts the user did not name a page for"
        );
        assert_eq!(
            placement_for(Cadence::Full, Some(&flash), Some(&strong)).label(),
            "cartografo",
            "REM classifies everything with the strong slot"
        );
        assert_eq!(
            placement_for(Cadence::Light, None, Some(&strong)).label(),
            "ingest",
            "no ingest slot ⇒ no cheap tier to run it on"
        );
        assert_eq!(
            placement_for(Cadence::Full, Some(&flash), None).label(),
            "identity fallback",
            "a Full pass never borrows the cheap tier"
        );
    }

    /// The re-open park is the STRONG pass's work queue and nothing else may
    /// clear it. The light cadence now runs a Cartografo too, so "runs the
    /// Cartografo" stopped being the same question as "may answer a
    /// nomination" — this pins them apart, which is the whole hazard of
    /// letting the light pass place (a cheap hourly build reversed a
    /// considered cross-wiki move within three hours on 2026-07-04).
    #[test]
    fn only_the_strong_pass_may_consume_the_reopen_park() {
        let strong = FakeLlmBackend::new("pro", "x");
        let flash = FakeLlmBackend::new("flash", "x");
        let light = placement_for(Cadence::Light, Some(&flash), Some(&strong));
        let full = placement_for(Cadence::Full, Some(&flash), Some(&strong));
        assert!(light.runs_cartografo(), "the light pass does place facts");
        assert!(full.runs_cartografo());
        assert!(
            matches!(full, NewFactPlacement::Cartografo(_)),
            "only this variant consumes the park in build_wiki_plan"
        );
        assert!(
            !matches!(light, NewFactPlacement::Cartografo(_)),
            "the light pass must carry the park forward untouched"
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

        report.errors.push("famiglia_carol: boom".to_owned());
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
        let outcome = run_light(
            &pool,
            &tree,
            embedder,
            None,
            &LightPolicy::default(),
            &RemPolicy::default(),
        )
        .await
        .expect("light must succeed on an empty workdir");
        assert_eq!(outcome.light.scanned, 0);
        assert!(outcome.compile.is_none());
    }

    /// **A deployment nobody is talking to keeps no kept turn.**
    ///
    /// The buffer that stops a re-delivered turn being decided twice is
    /// emptied on the way into a write, and an idle install does not write:
    /// without a sweep on the clock the last turn of the day sat there until
    /// the next one, whenever that came. It runs before the budget gate, too —
    /// a deployment that has stopped spending has not stopped keeping.
    #[tokio::test]
    async fn the_light_round_sweeps_kept_turns_past_their_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embedder::FakeEmbedder::new("fake", 4));
        sqlx::query(
            "INSERT INTO ingest_replies \
               (sender_id, consumer_id, author, turn_hash, created_at, expires_at, reply) \
             VALUES ('alice', '', 'user', 'deadbeef', ?, ?, '{}')",
        )
        .bind((Utc::now() - chrono::Duration::hours(2)).to_rfc3339())
        .bind((Utc::now() - chrono::Duration::hours(1)).to_rfc3339())
        .execute(&pool)
        .await
        .expect("a turn kept this morning");

        run_light(
            &pool,
            &tree,
            embedder,
            None,
            &LightPolicy::default(),
            &RemPolicy::default(),
        )
        .await
        .expect("light round");

        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_replies")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(left, 0, "the round swept it by the clock");
        drop(dir);
    }
}
