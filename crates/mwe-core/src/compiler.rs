// SPDX-License-Identifier: AGPL-3.0-or-later
//! Narrative **compiler** — Il Cronista + the Record Writer.
//!
//! The compiler is the prose stage: it consumes the [`CompilationPlan`] the
//! planner ([`crate::planner`]) produced and, page by page, turns the facts
//! assigned to each page into cohesive prose markdown. It is the second half of
//! the narrative compiler — the planner decides *where each fact lives*, the
//! compiler decides *how it reads*.
//!
//! Per dirty page, [`compile_page`] routes:
//! - a **`lista`-style page** (the ingest classifier's `page.style`) →
//!   [`compile_list_page`] (the Record Writer, **no LLM**): each atomic fact
//!   rendered deterministically as one bullet record wrapped in its ACL marker,
//!   bypassing Il Cronista.
//! - a page with **no facts** → [`compile_empty_leaf`], deterministic: its
//!   card, plus the rails this compile requires of it ([`link_targets`]). No page lists other pages,
//!   so an empty one has nothing to narrate and never reaches a model — but
//!   the links it is required to carry are still written, because the next
//!   build reads a page's links off its prose and a page that dropped them
//!   would lose them for good.
//! - everything else → [`compile_leaf_page`] (Il Cronista, **strong** model):
//!   the page's own facts woven into prose, each claim wrapped in a
//!   bare `{{f=<fact_id>}}…{{/}}` runtime ACL marker (the full
//!   `{{subject=… allow=… sender=…}}` form is export-only), other pages
//!   reachable only by `[[wikilink]]`.
//!
//! ## Degraded mode — no page stays frozen
//!
//! A Cronista reply that is unusable (transport error, or output that is not
//! parseable JSON) gets **one retry** (fresh call, strict-JSON reminder in the
//! user message). If the retry is also unusable the page falls back to a
//! **guard-only rewrite** ([`compile_degraded_leaf`]): the existing prose is
//! kept, minus the regions of facts that have left this page, and every
//! planned fact still missing a marker on disk is appended as its own marked
//! region — the standing forward-completeness-guard shape — so the markers on
//! the page are exactly the facts the index puts there (recall/redaction work,
//! offsets stamped, and the page stays movable) while the full rewrite waits
//! for the next successful compile. The degraded path **never invents content** (only canonical claim
//! text is written) and is **idempotent** (facts appended once carry markers,
//! so a re-run appends nothing). The outcome is recorded distinctly
//! ([`CompileReport::degraded`]), the page is parked on the persisted plan's
//! `force_dirty` so the next cycle retries the proper rewrite, and the
//! per-page failure ledger ([`crate::compile_failures`]) counts the streak —
//! emitting a `compile_failure_streak` notice on `wiki_events` at its
//! thresholds.
//!
//! ## Information starvation (the load-bearing invariant)
//!
//! The Cronista receives ITS OWN facts plus, for every other page, only a
//! `slug → one-line description` index — never another page's facts. It is
//! therefore structurally unable to copy a detail it was never shown, so it must
//! emit a bare `[[wikilink]]` instead of paraphrasing. That is the mechanical
//! enforcement of one-fact-one-page and what makes the prose a non-redundant
//! recall surface (see [`crate::planner`]).
//!
//! ## `fact_id` markers + recall surface
//!
//! Each ACL marker carries `f=<fact_id>` (the stable id threaded from the plan —
//! a fix vs the old engine, which lost fact identity at render time). After
//! writing a page the compiler **repoints** each fact's `fact_index` row
//! (`source_path` + byte offsets) at the compiled marker region via
//! [`fact_index::move_region`], so recall can return the compiled prose passage
//! while `fact_index.text` stays the canonical claim used for embedding/dedup.
//! Standard pages are compiler OUTPUT and are excluded from the marker reindex
//! sweep (see [`crate::reindex`]) so a reindex never overwrites the canonical
//! claim with the prose.
//!
//! ## Cross-page moves: DB-first commit point
//!
//! When a new plan reassigns a fact from page A to page B, A is rewritten
//! without the fact's marker. If the row still pointed at A at that moment, the
//! orphan sweep ([`crate::reindex`]) would read the missing marker as a forget
//! gesture and tombstone the live fact — the same race the promote machinery
//! closed for REM moves. So before any page write, [`compile_dirty_pages`]
//! **pre-points** every dirty-page fact whose row lives on a different file
//! onto its planned page as a *pending render* (NULL offsets, sweep-exempt) via
//! [`prepoint_plan_moves`]; the per-page repoint then stamps the real offsets.
//! A destination page whose Cronista fails ends in the degraded guard-append
//! above (marker on disk, offsets stamped); an infrastructure soft-fail
//! (locate/write) leaves a pending render — recall falls back to the canonical
//! claim — instead of a silent tombstone either way.
//!
//! ## Perimeter
//!
//! Only standard families reach here: the planner gathers facts only from
//! standard wikis (every wiki whose `_meta` smart flag is `false`), so the
//! plan — and thus the compiler — never sees a smart wiki. No
//! per-page smart-wiki guard is needed.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::compile_failures;
use crate::events::{self, EventKind};
use crate::fact_index::{self, FactIndexError};
use crate::llm::{CompletionRequest, LlmBackend, LlmError};
use crate::meta_annotate;
use crate::parser::{self, ParseEvent};
use crate::planner::{self, CompilationPlan, FactForPage, PagePlan};
use crate::prompts::{self, PromptError};
use crate::types::{FactId, Principal};
use crate::wiki::{WikiError, WikiTree, workdir_relative_source_path};

/// Bundled default for the Cronista prompt (compiler prose stage).
pub const BUNDLED_CRONISTA_MD: &str = include_str!("../prompts/cronista.md");

/// Bundled default for the Cronista's **nightly part**
/// (`<workdir>/prompts/cronista-night.md`).
///
/// Appended to the task half by [`compile_leaf_page`] on
/// [`crate::dream::Cadence::Full`], and only for a page that already carries
/// links: it is the brief that turns those links from an inheritance into a
/// judgement, and a page with none has nothing to judge. It rides the turn,
/// so the cacheable system half stays byte-identical across both cadences.
pub const BUNDLED_CRONISTA_NIGHT_MD: &str = include_str!("../prompts/cronista-night.md");

/// Errors raised by the compiler. Per-page LLM/parse failures are collected
/// into the report (soft); infrastructure failures bubble.
#[derive(Debug, Error)]
pub enum CompilerError {
    /// `fact_index` access failed.
    #[error("compiler fact_index: {0}")]
    FactIndex(#[from] FactIndexError),
    /// Filesystem (page write / read) failed.
    #[error("compiler wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Low-level IO.
    #[error("compiler io: {0}")]
    Io(#[from] std::io::Error),
    /// Loading a compiler prompt failed.
    #[error("compiler prompt: {0}")]
    Prompt(#[from] PromptError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, CompilerError>;

/// Outcome of [`compile_dirty_pages`].
#[derive(Debug, Default, Clone)]
pub struct CompileReport {
    /// Set when the deployment's daily budget stopped this pass before
    /// it began. Every count is then zero, and the summary says this
    /// instead of reading as a compile that found nothing to do.
    pub budget_stop: Option<String>,
    /// What the queue did on the way in: claims screened, folded as
    /// duplicates, written, superseded. Filled by
    /// [`crate::dream::run_compile`], which screens the buffer before planning
    /// and writes the placed claims before compiling — default when the
    /// compile ran without a queue (a test calling `compile_dirty_pages`
    /// directly).
    pub queue: crate::dream_light::LightCycleReport,
    /// Leaf pages (re)written as prose by Il Cronista (`prosa` / `prosa-tecnica`).
    pub leaves: usize,
    /// `lista`-style leaf pages rendered as atomic records, bypassing the
    /// strong-model Cronista.
    pub lists: usize,
    /// Pages whose render matched the existing file (skipped, no write).
    pub unchanged: usize,
    /// On-disk page files deleted by the orphan sweep — pages the plan no
    /// longer references and no live `fact_index` row points at (the
    /// deferred half of the planner's GC; see [`sweep_orphan_page_files`]).
    pub orphan_files_swept: usize,
    /// Pages compiled in **degraded mode** (`"<slug>: <reason>"`): the
    /// Cronista failed twice, so the compiler kept the existing prose and
    /// appended the missing planned facts as marked regions — a distinct
    /// outcome from both a clean compile and a failure ([the degraded
    /// mode](self)). These pages are retried for a proper rewrite next
    /// cycle (parked on the plan's `force_dirty`).
    pub degraded: Vec<String>,
    /// Per-page soft errors (`"<slug>: <error>"`).
    pub errors: Vec<String>,
    /// `(slug, served chars)` — identity cards written past
    /// [`IDENTITY_CARD_CEILING_CHARS`]. Not an error: the page is on disk and
    /// every fact is on it. It says the card will be **cut when served**, and
    /// it is the signal that re-opens the card's placement on the next cycle
    /// (`dream::park_review_bridge`) so the material that is not always-on
    /// core is moved off rather than silently cut.
    pub cards_over_budget: Vec<(String, usize)>,
    /// Pages whose prose declined a recommended rail twice, so the compiler
    /// appended it (`"<slug>: [[a]], [[b]]"`). Not an error — the rail is on
    /// the page and the navigator can walk it. It says the Cronista would not
    /// weave that neighbour into the thread, which is a **prompt** signal:
    /// a rail carrying no *why* is the weaker form of the same link.
    pub rails_appended: Vec<String>,
}

impl CompileReport {
    /// Record what a **successful** write had to say about itself: the page
    /// is on disk either way, so these are warnings beside the count, never
    /// instead of it — and a page can carry both.
    fn record_notes(&mut self, slug: &str, notes: &PageNotes) {
        if let Some(chars) = notes.over_budget_chars {
            self.cards_over_budget.push((slug.to_owned(), chars));
        }
        if !notes.rails_appended.is_empty() {
            self.rails_appended
                .push(format!("{slug}: {}", notes.rails_appended.join(", ")));
        }
    }

    /// How many pages soft-failed (for the dream journal's structured
    /// `pages_failed` count).
    #[must_use]
    pub fn pages_failed(&self) -> i64 {
        i64::try_from(self.errors.len()).unwrap_or(i64::MAX)
    }

    /// How many pages compiled in degraded mode (for the dream journal's
    /// structured `pages_degraded` count).
    #[must_use]
    pub fn pages_degraded(&self) -> i64 {
        i64::try_from(self.degraded.len()).unwrap_or(i64::MAX)
    }
}

#[derive(Debug, Deserialize)]
struct CronistaOutput {
    #[serde(rename = "mergedBody")]
    merged_body: String,
    /// One-line summary of the page — its `description:` testata field, the
    /// card a reader is shown when deciding whether to open it. On a wiki's
    /// foundation page it also becomes the wiki's `_meta` abstract.
    #[serde(default)]
    description: String,
    /// The page's dominant **writing style** — the Cronista's
    /// compile-time choice from the closed palette (`prosa` / `prosa-tecnica` /
    /// `lista`), recorded in the page's `style:` testata so recall knows how to
    /// read it. Normalised by [`normalize_style`]; absent → `prosa`.
    #[serde(default)]
    style: Option<String>,
}

/// Compile every dirty page of `plan` into prose.
///
/// `cronista` is the strong-model backend for prose pages. Per-page failures
/// are collected into the report; the run continues.
///
/// # Errors
///
/// Infrastructure failures (DB / filesystem). LLM/parse failures are soft.
pub async fn compile_dirty_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
    cronista: &dyn LlmBackend,
    cadence: crate::dream::Cadence,
    now: &str,
) -> Result<CompileReport> {
    let mut report = CompileReport::default();
    // DB-first commit point for plan moves (see the module docs): repoint every
    // cross-page-moving fact onto its planned page as a pending render BEFORE
    // any page write, so the old page's rewrite can never strand a row where
    // the orphan sweep would tombstone it.
    let prepointed = prepoint_plan_moves(pool, tree, plan).await?;
    if prepointed > 0 {
        tracing::info!(
            prepointed,
            "compiler: cross-page plan moves pre-pointed as pending renders"
        );
    }
    let (tone_cache, locale_cache) = warm_wiki_caches(pool, tree, plan).await;
    // The page index is a pure function of the plan, so it is built once per
    // run and handed to every leaf: it is the same ~3.5k tokens for all of
    // them, which is exactly what makes it the cacheable half of the Cronista
    // system prompt (see `split_cronista_prompt`). Rebuilding it per page also
    // rebuilt the same string 15-plus times for nothing.
    let page_index = build_page_index(pool, tree, plan).await;
    // Pages whose compile failed or degraded: parked on the persisted plan's
    // `force_dirty` below, so the next build retries the proper rewrite even
    // on an otherwise-idle night (the early-skip would clear the dirty set).
    let mut retry_slugs: Vec<String> = Vec::new();

    let outcomes = write_dirty_pages(
        pool,
        tree,
        plan,
        cronista,
        &tone_cache,
        &locale_cache,
        &page_index,
        cadence,
        now,
    )
    .await;

    for (slug, outcome) in outcomes {
        let slug = &slug;
        let Some(page) = plan.pages.get(slug) else {
            // Removed page: its on-disk file is handled by the orphan
            // sweep at the tail of this compile (once no row points at it).
            continue;
        };
        match outcome {
            Ok(PageOutcome::Leaf(notes)) => {
                report.leaves += 1;
                report.record_notes(slug, &notes);
                note_page_success(pool, tree, page).await;
            },
            Ok(PageOutcome::List) => {
                report.lists += 1;
                note_page_success(pool, tree, page).await;
            },
            Ok(PageOutcome::Unchanged) => {
                report.unchanged += 1;
                note_page_success(pool, tree, page).await;
            },
            Ok(PageOutcome::Degraded { reason }) => {
                tracing::warn!(
                    slug,
                    reason,
                    "compiler: page compiled DEGRADED (guard-only)"
                );
                report.degraded.push(format!("{slug}: {reason}"));
                retry_slugs.push(slug.clone());
                // A degraded append still counts as "the Cronista keeps
                // failing here" — the streak resets only on a clean rewrite.
                note_page_failure(pool, tree, page, &reason).await;
            },
            Err(e) => {
                tracing::warn!(slug, error = %e, "compiler: page failed");
                report.errors.push(format!("{slug}: {e}"));
                retry_slugs.push(slug.clone());
                note_page_failure(pool, tree, page, &e.to_string()).await;
            },
        }
    }
    // Park the failed/degraded pages for a retry: without the flag their
    // carried-over fingerprint matches the next build and the page would sit
    // frozen until its facts change. Best-effort — a plan-IO hiccup only
    // delays the retry to the next natural dirtying.
    if !retry_slugs.is_empty() {
        match planner::park_force_dirty_in_persisted_plan(tree, &retry_slugs) {
            Ok(parked) => tracing::info!(
                parked,
                "compiler: failed/degraded pages parked force_dirty for the next build"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "compiler: failed to park failed/degraded pages");
            },
        }
    }
    report.orphan_files_swept = sweep_orphan_page_files(pool, tree, plan).await;
    tracing::info!(
        leaves = report.leaves,
        lists = report.lists,
        unchanged = report.unchanged,
        orphan_files_swept = report.orphan_files_swept,
        degraded = report.degraded.len(),
        errors = report.errors.len(),
        "compiler: dirty pages compiled"
    );
    Ok(report)
}

/// The page's workdir-relative `source_path` (`wikis/<id>/<page>.md`) — the
/// failure-ledger key, same convention as `fact_index.source_path`. `None`
/// when the wiki cannot be located (the page's own compile surfaces that as
/// its soft error).
pub(crate) fn page_source_path(tree: &WikiTree, page: &PagePlan) -> Option<String> {
    let handle = tree.locate(&parse_wiki_id(&page.wiki_id)).ok()?;
    let abs = handle.abs_dir().join(&page.page_path);
    Some(workdir_relative_source_path(tree.workdir(), &abs))
}

/// Close the page's failing streak after a clean compile (best-effort — the
/// ledger is observability, never a compile failure).
async fn note_page_success(pool: &SqlitePool, tree: &WikiTree, page: &PagePlan) {
    let Some(source_path) = page_source_path(tree, page) else {
        return;
    };
    if let Err(e) = compile_failures::reset(pool, &source_path).await {
        tracing::warn!(source_path, error = %e, "compiler: failure-ledger reset failed");
    }
}

/// Record a failed/degraded page compile in the failure ledger and, when the
/// streak hits a [`compile_failures::NOTICE_THRESHOLDS`] value exactly, emit
/// one `compile_failure_streak` notice on `wiki_events` — the same channel
/// the `structure_applied` notices ride, so a persistently-failing page
/// reaches the operator instead of living only in the report dump. Once per
/// threshold per streak by construction (the count passes each value once;
/// a clean rewrite resets it). Best-effort throughout.
async fn note_page_failure(pool: &SqlitePool, tree: &WikiTree, page: &PagePlan, error: &str) {
    let Some(source_path) = page_source_path(tree, page) else {
        return;
    };
    let consecutive = match compile_failures::record_failure(pool, &source_path, error).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(source_path, error = %e, "compiler: failure-ledger record failed");
            return;
        },
    };
    if !compile_failures::NOTICE_THRESHOLDS.contains(&consecutive) {
        return;
    }
    let payload = serde_json::json!({
        "slug": page.slug,
        "source_path": source_path,
        "consecutive": consecutive,
        "last_error": error,
        "dashboard_path": "/dashboard/dream",
    });
    match events::insert_event(
        pool,
        EventKind::CompileFailureStreak,
        Some(&page.wiki_id),
        None,
        &payload,
    )
    .await
    {
        Ok(_) => tracing::warn!(
            slug = %page.slug,
            source_path,
            consecutive,
            "compiler: compile_failure_streak notice emitted"
        ),
        Err(e) => {
            tracing::warn!(source_path, error = %e, "compiler: failure-streak notice failed");
        },
    }
}

/// The deferred half of the planner's garbage collection: delete on-disk
/// page FILES the plan no longer references.
///
/// The planner drops an emptied page from the plan and the registry but
/// never touches its `.md`. Without this sweep — a live-write page whose fact
/// the Conciliatore re-routed, or a leaf whose facts all moved away — the file
/// survives with stale marker copies, and the recall navigator keeps reading
/// them. This sweep walks each plan-covered wiki (smart wikis never enter a
/// plan) and removes a concept-page file only when ALL of:
///
/// - its path is not in the plan's page set for that wiki,
/// - it is not a reserved page (`@rules.md`, any `_`-prefixed file). The
///   identity card needs no exemption: it is a plan node, so it is always in
///   the plan's page set for its wiki,
/// - **no** non-tombstoned `fact_index` row points at it
///   ([`fact_index::count_rows_at_source_path`]) — the DB-first guard: a
///   pending render or a superseded row's audit marker keeps the file.
///
/// Every step is soft (a wiki that cannot be walked is skipped, never an
/// error): cleanup must not fail a compile. Returns the number of files
/// removed.
async fn sweep_orphan_page_files(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
) -> usize {
    let mut planned: HashMap<&str, std::collections::BTreeSet<&str>> = HashMap::new();
    for page in plan.pages.values() {
        planned
            .entry(page.wiki_id.as_str())
            .or_default()
            .insert(page.page_path.as_str());
    }
    let mut swept = 0;
    for (wiki_id, pages) in &planned {
        let Ok(handle) = tree.locate(&parse_wiki_id(wiki_id)) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(handle.abs_dir()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !std::path::Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                || name.starts_with('_')
                || name == crate::wiki::RULES_FILENAME
                || pages.contains(name)
                || !entry.path().is_file()
            {
                continue;
            }
            let source_path = workdir_relative_source_path(tree.workdir(), &entry.path());
            match fact_index::count_rows_at_source_path(pool, &source_path).await {
                Ok(0) => {},
                Ok(_) => continue, // a live pointer keeps the file
                Err(e) => {
                    tracing::warn!(source_path, error = %e, "compiler: orphan sweep count failed");
                    continue;
                },
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    swept += 1;
                    tracing::info!(
                        wiki_id,
                        page = name,
                        "compiler: orphan page file swept (not in plan, no live rows)"
                    );
                },
                Err(e) => {
                    tracing::warn!(source_path, error = %e, "compiler: orphan sweep delete failed");
                },
            }
        }
    }
    swept
}

enum PageOutcome {
    /// A leaf written by Il Cronista, carrying whatever the write had to
    /// report about itself ([`PageNotes`]) — success plus warnings, never
    /// one instead of the other.
    Leaf(PageNotes),
    List,
    Unchanged,
    /// The Cronista failed twice and the page fell back to the guard-only
    /// append ([`compile_degraded_leaf`]); `reason` is the failure chain
    /// (first attempt / retry) for the report and the failure ledger.
    Degraded {
        reason: String,
    },
}

/// What a successfully written page reports about itself beyond "it
/// compiled" — warnings that ride a **success**, so neither may displace
/// the other: a card can be over its ceiling *and* have had a rail
/// appended, and the operator needs to see both.
#[derive(Debug, Default, Clone)]
struct PageNotes {
    /// Set when a written identity card came out past
    /// [`IDENTITY_CARD_CEILING_CHARS`]: the page is on disk with every fact
    /// on it, and the read path will truncate it when it serves it.
    over_budget_chars: Option<usize>,
    /// Recommended rails the writer left out of the prose, appended to the
    /// page deterministically ([`append_missing_rails`]).
    rails_appended: Vec<String>,
}

/// Pre-point every dirty-page fact whose `fact_index` row still lives on a
/// different file onto its planned page, as a **pending render** (NULL
/// offsets, sweep-exempt) — the capture commit-point pattern the promote
/// machinery already applies to REM moves, here applied to plan moves.
///
/// Runs BEFORE any page write. Once the row points at its destination, the
/// source page's rewrite (which drops the marker) can no longer be read by the
/// orphan sweep as a forget gesture (`mark_forgotten_at` is path-guarded), and
/// a destination compile that soft-fails leaves a pending render recall can
/// still serve from the canonical claim, repaired by the next compile.
///
/// Returns how many rows were pre-pointed.
async fn prepoint_plan_moves(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
) -> Result<usize> {
    let mut moved = 0;
    for slug in &plan.dirty_pages {
        let Some(page) = plan.pages.get(slug) else {
            continue;
        };
        if page.primary_facts.is_empty() {
            continue;
        }
        let Ok(handle) = tree.locate(&parse_wiki_id(&page.wiki_id)) else {
            continue; // surfaced later as the page's own soft error
        };
        let abs = handle.abs_dir().join(&page.page_path);
        let target = workdir_relative_source_path(tree.workdir(), &abs);
        for f in &page.primary_facts {
            let Some(row) = fact_index::find_by_id(pool, &f.fact_id).await? else {
                continue;
            };
            if row.deleted_at.is_some() || row.superseded_at.is_some() || row.source_path == target
            {
                continue;
            }
            // Pre-point the row to its planned page AND its wiki, so the
            // pending render (NULL offsets, stamped at compile) never leaves
            // `wiki_id` pointing at the fact's old wiki — `wiki_id` must
            // always name the wiki whose page carries the region.
            moved += usize::from(
                fact_index::move_to_wiki(pool, &f.fact_id, &page.wiki_id, &target, None, None)
                    .await?
                    > 0,
            );
        }
    }
    Ok(moved)
}

/// Write every dirty page, four at a time, and hand back what each one did.
///
/// A page is written from the facts the plan gives it and from nothing another
/// page holds, so the order they are written in cannot change what any of them
/// says — and the whole cost of writing one is the wait for the model, ~90
/// seconds on the bench's memory. One after another that is the night: 21
/// pages, 21 waits, half an hour of a machine sitting idle. Measured over five
/// real nights on 2026-09-05, writing the pages was 80% of the night (30
/// minutes of 38) against 18% for all the reorganisation passes together.
///
/// The FIRST page goes alone on purpose. The Cronista's instructions are the
/// same long block for every page of a run and ride as a cacheable prefix; the
/// first call is what puts it in the cache and the rest read it for almost
/// nothing. Start four at once and all four pay to write it.
///
/// The writers share no decision. Each takes the next page and returns its
/// outcome; the ledger writes, the report and the retry list are folded by the
/// caller, in plan order, exactly as they were when this was a loop. Nor do
/// their writes collide: the plan gives each fact to one page, so no two pages
/// touch the same row, and each row is written by a single statement that
/// takes the write lock at once rather than a transaction that upgrades to it.
#[allow(
    clippy::too_many_arguments,
    reason = "the compile's whole context, threaded through unchanged from its caller"
)]
async fn write_dirty_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
    cronista: &dyn LlmBackend,
    tone_cache: &HashMap<String, String>,
    locale_cache: &HashMap<String, String>,
    page_index: &PageIndex,
    cadence: crate::dream::Cadence,
    now: &str,
) -> Vec<(String, Result<PageOutcome>)> {
    let compile_one = async |slug: &String| -> Option<(String, Result<PageOutcome>)> {
        let page = plan.pages.get(slug)?;
        Some((
            slug.clone(),
            compile_page(
                pool,
                tree,
                plan,
                page,
                cronista,
                tone_cache,
                locale_cache,
                page_index,
                cadence,
                now,
            )
            .await,
        ))
    };
    let (first, rest) = plan
        .dirty_pages
        .split_first()
        .map_or((None, &[][..]), |(f, r)| (Some(f), r));
    let mut outcomes: Vec<(String, Result<PageOutcome>)> = Vec::new();
    if let Some(slug) = first
        && let Some(done) = compile_one(slug).await
    {
        outcomes.push(done);
    }
    if rest.is_empty() {
        return outcomes;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = parking_lot::Mutex::new(Vec::new());
    let worker = || async {
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(slug) = rest.get(i) else { break };
            if let Some(o) = compile_one(slug).await {
                done.lock().push((i, o));
            }
        }
    };
    // Boxed: four page-writing state machines live at once here, and a future
    // that carries all four inline grows every caller's frame down the stack
    // (the dashboard's dream handler is the one that notices).
    tokio::join!(
        Box::pin(worker()),
        Box::pin(worker()),
        Box::pin(worker()),
        Box::pin(worker()),
    );
    let mut got = done.into_inner();
    got.sort_by_key(|(i, _)| *i);
    outcomes.extend(got.into_iter().map(|(_, o)| o));
    outcomes
}

#[allow(
    clippy::too_many_arguments,
    reason = "one dispatch hop: the per-run values (page index, tone cache, clock) \
              are built once by the caller and threaded, not rebuilt per page"
)]
async fn compile_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
    page: &PagePlan,
    cronista: &dyn LlmBackend,
    tone_cache: &HashMap<String, String>,
    locale_cache: &HashMap<String, String>,
    page_index: &PageIndex,
    cadence: crate::dream::Cadence,
    now: &str,
) -> Result<PageOutcome> {
    // A wiki's card rides the same dispatch: it renders as prose while it
    // still carries facts and flips to a bare overview once REM's reorg has
    // drained them onto children — which is what is meant to happen to
    // everything that lands there.
    // **No page lists other pages** (founder, 2026-08-19): an index has to be
    // maintained, while the list of pages with their cards is something the
    // engine already gets by reading the files.
    // A page with no facts is empty, and renders as its card.
    // Il Cronista a 3 stili. A leaf whose ingest-decided style (`page.style`) is
    // `lista` holds atomic-record data (a shopping list, a filmography), not
    // prose: render it deterministically as ACL-markered records (cheap, NO LLM),
    // bypassing Il Cronista. prosa / prosa-tecnica still go to the strong-model
    // Cronista below. The Cronista never emits `lista` itself (cronista.md
    // §STYLE), so `page.style` is the sole source of a record page — which is what
    // lets the testata read `lista` over a record body rather than prose.
    if style_or_default(page.style) == crate::wiki::PageStyle::Lista {
        return compile_list_page(pool, tree, page, now).await;
    }
    // A leaf with NO facts never reaches the LLM: the Cronista, handed an
    // empty YOUR FACTS list, invents colour prose from the wikilinks alone
    // (the dogfood re-run compiled Tolkien lore onto a zero-fact
    // foundation index). Render the deterministic minimal page instead.
    if page.primary_facts.is_empty() {
        return compile_empty_leaf(tree, page, &link_targets(plan, &page.slug, cadence).0, now);
    }
    // Both are per-wiki constants resolved before any page is written (see
    // `warm_wiki_caches`), so a page that arrives here always finds its own.
    let wiki_tone = tone_cache
        .get(&page.wiki_id)
        .cloned()
        .unwrap_or_else(|| resolve_tone(tree, &page.wiki_id));
    // Per PAGE, not per wiki: an agent's wiki holds pages about other people
    // too (see `tone_for_page`), and those must not be narrated as the agent's
    // own life.
    let tone = tone_for_page(&wiki_tone, page);
    let language = match locale_cache.get(&page.wiki_id) {
        Some(hit) => hit.clone(),
        None => {
            crate::locale::memory_directive_for_wiki(pool, tree, &parse_wiki_id(&page.wiki_id))
                .await
        },
    };
    compile_leaf_page(
        pool, tree, plan, page, cronista, &tone, &language, page_index, cadence, now,
    )
    .await
}

/// The wiki's `LANGUAGE` directive, resolved once per wiki per run.
///
/// The deterministic renders (list pages, fact-less leaves) never call
/// this: they write no prose, so they must not pay the scope-chain walk
/// either. Only the two LLM branches above ask.
/// Resolve the tone and the language of every wiki this run will write in,
/// once, before any page is written.
///
/// Both are per-wiki constants for a whole compile and both are expensive the
/// first time — the language walks the scope chain to the root (a tree walk
/// per hop) and then hits the DB. Memoising them lazily was enough while
/// pages were written one after another; pages that are written together
/// would each miss the same empty memo and pay for the same answer, so the
/// answers are gathered here instead.
async fn warm_wiki_caches(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut tone = HashMap::new();
    let mut locale = HashMap::new();
    for slug in &plan.dirty_pages {
        let Some(page) = plan.pages.get(slug) else {
            continue;
        };
        if tone.contains_key(&page.wiki_id) {
            continue;
        }
        tone.insert(page.wiki_id.clone(), resolve_tone(tree, &page.wiki_id));
        locale.insert(
            page.wiki_id.clone(),
            crate::locale::memory_directive_for_wiki(pool, tree, &parse_wiki_id(&page.wiki_id))
                .await,
        );
    }
    (tone, locale)
}

/// The deterministic render of a fact-less leaf — usually a foundation
/// page whose facts have not arrived yet (or have all moved away; empty
/// CONCEPT leaves are garbage-collected by the planner and never get
/// here). No LLM: with nothing to narrate, anything a model writes is
/// invention. The next compile with real facts replaces the body wholesale.
///
/// **`rails` is written too, and that is not decoration.** A page's links are
/// read back off its own prose at every build ([`crate::planner`]'s harvest),
/// so a render that drops them loses them: the next plan finds none, the graph
/// forgets the page, and a rail the REM decided is asked for again and dropped
/// again, every night. A person's identity card is the page this happens to —
/// it exists from the moment they are enrolled and carries no facts until the
/// first one lands.
///
/// They are written as a bare list, which the Cronista's own rule calls the
/// weak form of a link — and it is the only honest form here. That rule earns
/// a link its place from the sentence around it, and this page has no
/// sentences. A weak link kept beats a good one lost.
fn compile_empty_leaf(
    tree: &WikiTree,
    page: &PagePlan,
    rails: &[String],
    now: &str,
) -> Result<PageOutcome> {
    let mut body = if page.description.trim().is_empty() {
        String::new()
    } else {
        format!("_{}_", page.description.trim())
    };
    if !rails.is_empty() {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&rails.join(" · "));
    }
    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    let contents = render_page_file(
        page,
        &body,
        &page.description,
        style_or_default(page.style),
        &created,
        now,
    );
    if contents == existing {
        return Ok(PageOutcome::Unchanged);
    }
    handle.write_page(page_path, &contents)?;
    tracing::info!(
        slug = %page.slug,
        wiki_id = %page.wiki_id,
        "compiler: fact-less leaf rendered deterministically (no LLM)"
    );
    // No notes: this render has no prose to drop a rail from, and the next
    // compile with real facts replaces the page wholesale.
    Ok(PageOutcome::Leaf(PageNotes::default()))
}

// ---------- Il Cronista (leaf) ----------

#[allow(
    clippy::too_many_arguments,
    reason = "the per-run constants (tone, language directive, page index, clock) \
              are resolved once by the caller and threaded, not rebuilt per page"
)]
async fn compile_leaf_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
    page: &PagePlan,
    llm: &dyn LlmBackend,
    tone: &str,
    language_directive: &str,
    page_index: &PageIndex,
    cadence: crate::dream::Cadence,
    now: &str,
) -> Result<PageOutcome> {
    let (recommended, prior) = link_targets(plan, &page.slug, cadence);
    let (index_cached, index_task) = page_index.render_for(plan, page);
    let prompt = prompts::render(
        "cronista",
        tree.workdir(),
        BUNDLED_CRONISTA_MD,
        &[
            ("locale", language_directive),
            ("title", page.title.as_str()),
            ("slug", page.slug.as_str()),
            ("tone", tone),
            ("page_kind", page_kind(page)),
            (
                "primary_facts",
                primary_facts_text(
                    &page.primary_facts,
                    now,
                    &|r| authored_ref_resolves(tree, r),
                    &|s| successor_wikilink(plan, &page.slug, s),
                )
                .as_str(),
            ),
            ("page_index", index_cached.as_str()),
            ("page_index_task", index_task.as_str()),
            ("links", recommended_links(&recommended).as_str()),
        ],
    )?;
    // A part that fails to load is skipped with a warning — the page is still
    // written, by exactly the rules an hourly rewrite gets.
    let prompt = match night_part(tree, &prior) {
        Some(part) => splice_task_part(&prompt, &part),
        None => prompt,
    };
    let max_tokens = cronista_max_tokens(page.primary_facts.len());
    // One retry on an unusable reply (transport error OR unparseable JSON),
    // then the degraded guard-only fallback — a failing Cronista must never
    // freeze the page (see the module's degraded-mode section).
    let body = match cronista_with_retry(llm, &prompt, &page.slug, max_tokens).await {
        Ok(b) => b,
        Err(reason) => return compile_degraded_leaf(pool, tree, page, now, &reason).await,
    };
    // The rail guard, model half: the page is usable, but a rail the plan
    // declared may not have reached the prose. Costs a call only when one is
    // missing (see `cronista_relink`).
    let body = cronista_relink(llm, &prompt, &page.slug, max_tokens, body, &recommended).await;

    // The Cronista marks each fact's prose span with a lightweight `<fN>…</fN>`
    // tag (N = 1-based index into the page's facts); the load-bearing region
    // marker is rendered HERE by code, not hand-written by the LLM. Expand those
    // tags into the bare runtime `{{f=<uuid>}}…{{/}}` markers (the ACL lives in
    // the `fact_index` columns and gates by that key), then drop any orphan tag
    // the model left behind — this removes the brace/attribute miscount failure
    // mode of LLM-written markers.
    let mut merged_body = expand_and_complete_fact_markers(&body.merged_body, page);

    // Every assigned fact must end up wrapped in a marker on the page.
    let known: std::collections::BTreeSet<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();

    // The rail guard's floor, re-read off the body that will actually be
    // written rather than trusting the pre-expansion check: fact-tag
    // expansion cannot move a wikilink, and this way the two cannot drift.
    let rails_appended = missing_rails(&recommended, &merged_body);
    if !rails_appended.is_empty() {
        tracing::warn!(
            slug = %page.slug,
            rails = rails_appended.len(),
            "compiler: rails declined twice — appending (rail completeness floor)"
        );
        append_missing_rails(&mut merged_body, &rails_appended);
    }

    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    // The testata: the page's writing style prefers the ingest classifier's
    // per-page proposal (`page.style`, decided at ingest and carried through the
    // plan), falling back to the Cronista's compile-time choice (`body.style`)
    // when ingest proposed none.
    let contents = render_page_file(
        page,
        &merged_body,
        &body.description,
        style_or_default(
            page.style
                .or_else(|| crate::wiki::PageStyle::parse_lenient(body.style.as_deref())),
        ),
        &created,
        now,
    );
    let unchanged = contents == existing;
    if !unchanged {
        handle.write_page(page_path, &contents)?;
    }

    // Repoint each fact's fact_index row at the compiled marker region so recall
    // can return the prose; fact_index.text stays the canonical claim. Runs on
    // the Unchanged path too: a fact pre-pointed here as a pending render
    // (cross-page move whose marker already sat on this page) still needs its
    // offsets stamped.
    let abs = handle.abs_dir().join(page_path);
    let source_path = workdir_relative_source_path(tree.workdir(), &abs);
    repoint_facts(pool, &contents, &known, &page.wiki_id, &source_path).await?;
    if unchanged {
        return Ok(PageOutcome::Unchanged);
    }

    sync_foundation_summary(page, handle.abs_dir(), &body.description);

    Ok(PageOutcome::Leaf(PageNotes {
        over_budget_chars: card_over_budget(page, &contents),
        rails_appended,
    }))
}

/// Refresh the wiki's one-line abstract in `_meta` from the page that answers
/// *what is this wiki* — its **identity card**.
///
/// 🚨 **The abstract has no reader on the read side.** What the write side
/// does with it is its own business — nothing here promises a turn ever sees
/// it.
///
/// It keys on the page being the wiki's **identity card**, never on a file
/// name. A name can move; what the page IS cannot, and a branch keyed on a
/// name that moved stops firing silently — the abstract would go stale for
/// ever with nothing to say so.
///
/// Best-effort: a `_meta` hiccup must not fail a page that already wrote.
fn sync_foundation_summary(page: &PagePlan, abs_dir: &std::path::Path, description: &str) {
    if !page.is_identity_card() {
        return;
    }
    if let Err(e) = meta_annotate::sync_wiki_summary(abs_dir, description.trim()) {
        tracing::warn!(slug = %page.slug, error = %e, "compiler: _meta summary sync failed");
    }
}

/// An identity card past its ceiling: a **curation** failure, not a compile
/// one. The page is written as-is with every fact on it; what it reports is
/// that the read path will cut it when it serves it, and a cut drops whatever
/// sorted last. Judged here, where the numbers are still in hand.
///
/// It is a nomination, not a verdict: the slug rides the review bridge
/// (`dream::park_bridge_signals`) into the next cycle's placement re-open, and
/// the Cartografo decides what leaves the card. Nothing is dropped either
/// way — a fact that leaves gets a page of its own wiki.
fn card_over_budget(page: &PagePlan, contents: &str) -> Option<usize> {
    if page_kind(page) != "identity_card" {
        return None;
    }
    let chars = served_chars(contents);
    if chars <= IDENTITY_CARD_CEILING_CHARS {
        return None;
    }
    tracing::warn!(
        slug = %page.slug,
        chars,
        ceiling = IDENTITY_CARD_CEILING_CHARS,
        facts = page.primary_facts.len(),
        "compiler: identity card is over its ceiling and will be cut when served"
    );
    Some(chars)
}

/// How many characters of a compiled page a reader is actually **served** —
/// the quantity [`IDENTITY_CARD_CEILING_CHARS`] is a ceiling on.
///
/// The file on disk is not that quantity, and the gap is not small: the YAML
/// testata, a `{{f=<uuid>}}…{{/}}` marker pair around **every** fact (~47
/// characters each) and full `[[wiki/page|alias]]` link syntax are all
/// machinery the read path resolves away before the prose reaches a turn
/// (`ingest::identity_card` measures `plain_wikilinks` over the projected
/// page). Measuring the file made a 25-fact card that sits comfortably inside
/// its authored budget report as over it on **every compile** — and a warning
/// that fires routinely is a warning nobody reads on the day it is true.
///
/// This is the upper bound of what any reader gets: per-reader redaction only
/// removes more. Which is the right side to be on for a warning that says the
/// card *will* be cut.
fn served_chars(contents: &str) -> usize {
    let body = crate::wiki::MarkdownDoc::parse(contents)
        .map_or_else(|| contents.to_owned(), |doc| doc.body);
    // Drop every `{{…}}` run: the fact markers are the only thing that shape
    // appears in on a compiled page, and an unclosed one is machinery too.
    let mut without_markers = String::with_capacity(body.len());
    let mut rest = body.as_str();
    while let Some(open) = rest.find("{{") {
        without_markers.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            rest = "";
            break;
        };
        rest = &after[close + 2..];
    }
    without_markers.push_str(rest);
    crate::ingest::plain_wikilinks(&without_markers)
        .trim()
        .chars()
        .count()
}

/// Output budget for one Cronista page rewrite — scales with the page's
/// fact mass instead of a flat ceiling: the reply carries the WHOLE page
/// (prose + `<fN>` tags + JSON envelope), so a big page cannot fit a cap
/// sized for a normal one, and the truncated reply then reads as a
/// Cronista failure (the 48-fact prod page failed exactly this way at a
/// flat 3 000). Resource valve, not a gate — generous, bounded, and any
/// hit is warned centrally by the llm layer.
fn cronista_max_tokens(fact_count: usize) -> u32 {
    u32::try_from((2_000 + fact_count.saturating_mul(200)).clamp(3_000, 32_000)).unwrap_or(32_000)
}

/// The Cronista call ladder: one attempt, and on an unusable reply ONE
/// retry — a fresh call whose user message reminds strict JSON (no prompt
/// machinery, the system prompt is unchanged) **and twice the room**. `Err`
/// is the combined two-failure reason the degraded fallback records.
///
/// The retry doubles the ceiling because a repeat of the same call is the one
/// thing that cannot work: the failure this ladder actually meets is a reply
/// that ran out of room — truncated, or nothing but reasoning — and asking
/// again under the same limit fails the same way. A ceiling is not a spend,
/// so the doubling costs nothing on the retries that failed for another
/// reason.
/// Marker line in the rendered Cronista prompt that separates the
/// **per-run-stable** half (the rules plus the page index — identical for
/// every page of one compile run) from the **per-page** half (this page's
/// identity, facts and recommended links).
///
/// The split is what makes the stable half a cacheable prefix: it goes in
/// the system prompt with a cache breakpoint, the per-page half rides the
/// user turn. On a run of N pages only the first pays the prefix in full.
const CRONISTA_TASK_MARKER: &str = "=== PAGE TO WRITE ===";

/// Split a rendered Cronista prompt at [`CRONISTA_TASK_MARKER`].
///
/// Returns `(system, task)`. A prompt without the marker — an operator
/// override written against an older bundled body — yields `(whole,
/// None)`: the entire prompt stays in the system field and nothing is
/// marked cacheable, because a system prompt that varies per page would
/// write one cache entry per call and read none.
fn split_cronista_prompt(rendered: &str) -> (&str, Option<&str>) {
    // The marker counts only as a LINE OF ITS OWN. The standing brief names
    // it in prose ("after the `=== PAGE TO WRITE ===` line") to tell the
    // model where its page is; a plain substring search cut the prompt at
    // that mention and shipped the brief's own opening sentence as the task
    // half — with the rules, and the whole point of the split, lost.
    let at = rendered
        .match_indices(CRONISTA_TASK_MARKER)
        .find(|(i, _)| {
            let starts_line = *i == 0 || rendered[..*i].ends_with('\n');
            let rest = &rendered[i + CRONISTA_TASK_MARKER.len()..];
            starts_line && (rest.is_empty() || rest.starts_with('\n'))
        })
        .map(|(i, _)| i);
    at.map_or((rendered, None), |at| {
        (rendered[..at].trim_end(), Some(rendered[at..].trim_end()))
    })
}

/// A Cronista attempt that did not produce a usable page.
enum CronistaFailure {
    /// Worth one more try: a flaky transport, a 5xx, a rate limit, or an
    /// unparseable reply (the retry's stricter instruction exists for
    /// exactly that).
    Retryable(String),
    /// Retrying cannot help — the request itself was rejected
    /// ([`LlmError::Invalid`]), the credential is bad
    /// ([`LlmError::Auth`]), or the deployment's daily budget stopped
    /// it ([`LlmError::Budget`]). Observed live: with the API answering
    /// "credit balance too low", a whole compile run spent two calls per
    /// page to be told the same thing twice.
    Permanent(String),
}

impl CronistaFailure {
    fn message(&self) -> &str {
        match self {
            Self::Retryable(m) | Self::Permanent(m) => m,
        }
    }
}

async fn cronista_with_retry(
    llm: &dyn LlmBackend,
    prompt: &str,
    slug: &str,
    max_tokens: u32,
) -> std::result::Result<CronistaOutput, String> {
    let (system, task) = split_cronista_prompt(prompt);
    match cronista_attempt(
        llm,
        system,
        task,
        "Write the page. Return the JSON object only.",
        max_tokens,
    )
    .await
    {
        Ok(b) => Ok(b),
        Err(CronistaFailure::Permanent(err)) => {
            tracing::warn!(
                slug,
                error = %err,
                "compiler: Cronista rejected the request — no retry, straight to degraded"
            );
            Err(format!("Cronista failed (not retryable): {err}"))
        },
        Err(first) => {
            tracing::warn!(
                slug,
                error = first.message(),
                "compiler: Cronista attempt unusable — retrying once"
            );
            let retry_msg = "Write the page. Return ONLY one valid JSON object with the keys \
                             mergedBody, description, style — no code fences, no \
                             commentary, nothing before or after the object.";
            match cronista_attempt(llm, system, task, retry_msg, max_tokens.saturating_mul(2)).await
            {
                Ok(b) => Ok(b),
                Err(second) => Err(format!(
                    "Cronista failed twice: {}; retry: {}",
                    first.message(),
                    second.message()
                )),
            }
        },
    }
}

/// The rail guard's model half: one rewrite that names exactly the
/// recommended links the first draft dropped.
///
/// A **usable** page that is under-linked is not a failure — the ladder
/// above has already done its job — so this never degrades the page and
/// never costs a call unless a rail is actually missing. The second reply
/// is kept only if it carries **more** of them: a rewrite that trades one
/// dropped rail for another leaves the prose we already have.
///
/// The prompt halves are reused verbatim, so the cached system prefix
/// (see [`split_cronista_prompt`]) still engages on the second call —
/// only the short user message differs.
async fn cronista_relink(
    llm: &dyn LlmBackend,
    prompt: &str,
    slug: &str,
    max_tokens: u32,
    first: CronistaOutput,
    recommended: &[String],
) -> CronistaOutput {
    let missing = missing_rails(recommended, &first.merged_body);
    if missing.is_empty() {
        return first;
    }
    tracing::warn!(
        slug,
        missing = missing.len(),
        recommended = recommended.len(),
        "compiler: recommended rails absent from the prose — one rewrite"
    );
    let (system, task) = split_cronista_prompt(prompt);
    let msg = format!(
        "Your draft left out {} of this page's RECOMMENDED LINKS: {}. Write the page \
         again, complete, with every one of them woven into the prose where it belongs \
         and copied character-for-character. Everything else about the page is \
         unchanged — same facts, same <fN> tags, same completeness rules. Return the \
         JSON object only.",
        missing.len(),
        missing.join(", ")
    );
    match cronista_attempt(llm, system, task, &msg, max_tokens).await {
        Ok(second) => {
            if missing_rails(recommended, &second.merged_body).len() < missing.len() {
                second
            } else {
                tracing::warn!(
                    slug,
                    "compiler: the rewrite carried no more rails than the draft — keeping the draft"
                );
                first
            }
        },
        Err(e) => {
            tracing::warn!(
                slug,
                error = e.message(),
                "compiler: rail rewrite unusable — keeping the draft"
            );
            first
        },
    }
}

/// Render the Cronista's `<fN>` tags into runtime markers, then make sure
/// every assigned fact ended up wrapped in one.
///
/// The Cronista marks each fact's prose span with a lightweight `<fN>…</fN>`
/// tag (N = 1-based index into the page's facts); the load-bearing region
/// marker is rendered HERE by code, never hand-written by the LLM. The tags
/// expand into the bare runtime `{{f=<uuid>}}…{{/}}` form (the ACL lives in
/// the `fact_index` columns and gates by that key) and any orphan tag the
/// model left behind is dropped — which removes the brace/attribute
/// miscount failure mode of LLM-written markers.
///
/// **Forward completeness guard**: a fact the Cronista did not tag (omitted,
/// or tagged with a number the expander could not resolve) is appended as
/// its own marked region under a thematic break, so nothing is silently
/// dropped and no non-global fact loses its protective ACL marker (the
/// `missing_acl_markers` the reviewer flags). A later full recompile can
/// weave the appended facts back into the prose. The rail guard's floor is
/// the same discipline applied to links ([`append_missing_rails`]), and its
/// rule holds here too: this is code, it does not know the page's language,
/// so it writes a separator and never a sentence.
///
/// The guard reads MARKERS and not meaning, so it cannot tell an omitted
/// fact from one whose content the prose already carries untagged — the
/// second comes back appended beside its own paraphrase, and the page says
/// the same thing twice. The break is what keeps that from reading as the
/// writer's own closing paragraph; the cure is upstream, in the Cronista's
/// obligation to tag every fact.
fn expand_and_complete_fact_markers(raw_body: &str, page: &PagePlan) -> String {
    let mut body = strip_orphan_fact_tags(&expand_fact_tags(raw_body, &page.primary_facts));
    // What actually made it onto the page as a marker.
    let emitted: std::collections::BTreeSet<String> = parser::parse(&body)
        .events
        .into_iter()
        .filter_map(|ev| match ev {
            ParseEvent::Region { attrs, .. } => attrs.fact_id.map(|f| f.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let missing: Vec<&FactForPage> = page
        .primary_facts
        .iter()
        .filter(|f| !emitted.contains(f.fact_id.as_str()))
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            slug = %page.slug,
            missing = missing.len(),
            fact_ids = %missing
                .iter()
                .map(|f| f.fact_id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            "compiler: facts without a marker after tag expansion — appending (forward completeness guard)"
        );
        // Skipped on an empty body, where a leading `---` would be read as
        // frontmatter rather than as a break.
        if !body.trim().is_empty() {
            body.push_str("\n\n---\n");
        }
        for f in missing {
            let region = crate::capture::render_marker(&f.fact_id, &f.text.replace('\n', " "));
            body.push_str("\n\n");
            body.push_str(&region);
        }
    }
    body
}

/// The rail guard's floor: append whatever the prose still will not carry.
///
/// Deterministic and last — it runs after [`cronista_relink`] has asked
/// twice. An appended rail is the **weaker** form of a link: the page's
/// thesis is that a relation explained in narrative is what makes recall
/// accurate, and a bare edge carries the label without the why. It is
/// written anyway because the alternative is worse — a neighbour reachable
/// from nowhere — and it is [reported](CompileReport::rails_appended) so
/// that how often the writer declines a rail stays visible instead of
/// becoming the silent 33 % this guard exists to end.
///
/// The links go on their own line, unlabelled and unadorned: prose in the
/// page's language would be invented here (this is code, and it does not
/// know the language), and dressing the appendix as authored text would
/// hide exactly what the report is trying to show.
fn append_missing_rails(body: &mut String, rails: &[String]) {
    if rails.is_empty() {
        return;
    }
    body.push_str("\n\n");
    body.push_str(&rails.join(" · "));
    body.push('\n');
}

/// One Cronista call + parse. `Err` is the human-readable failure — a
/// transport/backend error or an unparseable reply — that the caller's
/// retry/degraded ladder consumes; both failure classes are handled
/// identically (a flaky call must not cost more than a retry). A reply
/// that hit the `max_tokens` ceiling names the cap in its error instead
/// of the generic parse failure — truncation must never read as model
/// flakiness.
async fn cronista_attempt(
    llm: &dyn LlmBackend,
    system_prompt: &str,
    task: Option<&str>,
    user_msg: &str,
    max_tokens: u32,
) -> std::result::Result<CronistaOutput, CronistaFailure> {
    // With a split prompt the per-page half leads the user turn and the
    // instruction closes it; without one (an override with no marker) the
    // user turn is the bare instruction, as it always was.
    let user = task.map_or_else(|| user_msg.to_owned(), |t| format!("{t}\n\n{user_msg}"));
    let request = CompletionRequest::new(user)
        .with_system(system_prompt)
        .with_temperature(0.4)
        .with_max_tokens(max_tokens);
    // Only a split prompt has a system half that repeats verbatim across
    // the run; marking an unsplit one would buy a cache write per page and
    // never a read.
    let request = if task.is_some() {
        request.with_cached_system()
    } else {
        request
    };
    match llm.complete(request).await {
        Ok(r) => parse_cronista(&r.text).ok_or_else(|| {
            CronistaFailure::Retryable(match r.finish_reason {
                crate::llm::FinishReason::MaxTokens => format!(
                    "Cronista reply truncated at the max_tokens cap ({max_tokens}) — unparseable JSON"
                ),
                _ => "Cronista output was not parseable JSON".to_owned(),
            })
        }),
        Err(e) => {
            let msg = format!("Cronista LLM failed: {e}");
            match e {
                LlmError::Invalid(_) | LlmError::Auth(_) | LlmError::Budget(_) => {
                    Err(CronistaFailure::Permanent(msg))
                },
                _ => Err(CronistaFailure::Retryable(msg)),
            }
        },
    }
}

/// Drop every marked region whose fact is not among `planned`, returning the
/// remaining text and how many were cut.
///
/// A region carries one fact's prose between its markers, so cutting it takes
/// the fact's bytes and nothing else. Prose outside the markers — the
/// scaffolding the rest of the page is read against — is untouched.
fn cut_departed_regions(
    existing: &str,
    planned: &std::collections::BTreeSet<&str>,
) -> (String, usize) {
    let mut drop_spans: Vec<(usize, usize)> = Vec::new();
    for ev in parser::parse(existing).events {
        let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
        else {
            continue;
        };
        // A region with no `fact_id` belongs to no fact and is left alone.
        if let Some(fid) = attrs.fact_id
            && !planned.contains(fid.as_str())
        {
            drop_spans.push((start, end));
        }
    }
    if drop_spans.is_empty() {
        return (existing.to_owned(), 0);
    }
    let cut = drop_spans.len();
    let mut out = String::with_capacity(existing.len());
    let mut cursor = 0usize;
    for (start, end) in drop_spans {
        out.push_str(&existing[cursor..start]);
        cursor = end;
    }
    out.push_str(&existing[cursor..]);
    // Cutting a region leaves the blank lines that framed it back to back.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    (out, cut)
}

/// The **guard-only rewrite** — the degraded fallback when the Cronista
/// failed twice. Never invents content and leaves the page better than
/// frozen:
///
/// - the existing on-disk page (prose, frontmatter, markers) is kept as it
///   is, except for the regions of facts that have **left this page**: those
///   are cut. A fact that moved away keeps its region here otherwise, and
///   nothing ever removes it — the clean path rewrites the page whole, and
///   this path is the only other writer. The page then has more markers than
///   the index has rows for it, which every structural move refuses
///   (`marker set diverged from fact_index`), so the page can never be
///   moved again and only a successful Cronista can free it. Cutting the
///   region invents nothing: the bytes belong to the fact, and the fact is
///   somewhere else;
/// - every planned fact with **no marker on the page yet** is appended as
///   its own marked region (canonical claim text, the exact shape of the
///   forward completeness guard), so every fact reaches disk with a marker
///   and recall/redaction work;
/// - a page that never compiled (no file yet) is born as its testata plus
///   the marked regions — still zero invention.
///
/// **Idempotent**: appended facts now carry markers on disk, so a second
/// degraded pass finds nothing missing and writes nothing — no duplicate
/// regions. The repoint runs regardless (offsets stamped for appended
/// regions AND for pre-pointed pending renders whose marker already sits on
/// the page). The outcome is [`PageOutcome::Degraded`] even when nothing was
/// appended: the Cronista still failed, the page still awaits its proper
/// rewrite (the caller parks it `force_dirty` and counts the failure
/// streak). A later successful compile rewrites the whole page and
/// supersedes the appended tail.
async fn compile_degraded_leaf(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &PagePlan,
    now: &str,
    reason: &str,
) -> Result<PageOutcome> {
    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let on_file = handle.read_page(page_path).unwrap_or_default();

    let planned: std::collections::BTreeSet<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();
    // Cut the regions of facts this page no longer holds, then read what is
    // left: keeping them is what strands the page (see the doc above).
    let (existing, cut) = cut_departed_regions(&on_file, &planned);
    // Which planned facts already have a marker on the page (a prior
    // compile's region, or a prior degraded append).
    let on_disk: std::collections::BTreeSet<String> = parser::parse(&existing)
        .events
        .into_iter()
        .filter_map(|ev| match ev {
            ParseEvent::Region { attrs, .. } => attrs.fact_id.map(|f| f.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let missing: Vec<&FactForPage> = page
        .primary_facts
        .iter()
        .filter(|f| !on_disk.contains(f.fact_id.as_str()))
        .collect();

    let appended = missing.len();
    let contents = if missing.is_empty() {
        existing.clone()
    } else {
        let regions = missing
            .iter()
            .map(|f| crate::capture::render_marker(&f.fact_id, &f.text.replace('\n', " ")))
            .collect::<Vec<_>>()
            .join("\n\n");
        if existing.trim().is_empty() {
            // Never-compiled page: a minimal real file (testata from the
            // plan) whose body is the marked regions. No LLM, no invention.
            render_page_file(
                page,
                &regions,
                &page.description,
                style_or_default(page.style),
                &preserved_created(&existing, now),
                now,
            )
        } else {
            // Append: the prose that is left and the frontmatter stay as
            // they are (the next clean compile refreshes the testata).
            let mut out = existing.trim_end().to_owned();
            out.push_str("\n\n");
            out.push_str(&regions);
            out.push('\n');
            out
        }
    };

    // Against what is ON DISK, not against the cut text: a pass that only
    // cut a departed region still has a page to write.
    if contents != on_file {
        handle.write_page(page_path, &contents)?;
    }

    // Stamp offsets for every planned fact whose marker is on the page —
    // the appended regions and any pre-pointed pending render alike.
    let known: std::collections::BTreeSet<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();
    let abs = handle.abs_dir().join(page_path);
    let source_path = workdir_relative_source_path(tree.workdir(), &abs);
    repoint_facts(pool, &contents, &known, &page.wiki_id, &source_path).await?;

    tracing::warn!(
        slug = %page.slug,
        appended,
        cut,
        reason,
        "compiler: degraded guard-only rewrite (departed regions cut, missing facts appended)"
    );
    Ok(PageOutcome::Degraded {
        reason: format!(
            "{reason} — degraded rewrite: {cut} departed region(s) cut, {appended} missing fact \
             region(s) appended"
        ),
    })
}

async fn repoint_facts(
    pool: &SqlitePool,
    contents: &str,
    known: &std::collections::BTreeSet<&str>,
    wiki_id: &str,
    source_path: &str,
) -> Result<()> {
    for ev in parser::parse(contents).events {
        if let ParseEvent::Region {
            start, end, attrs, ..
        } = ev
            && let Some(fid) = attrs.fact_id
            && known.contains(fid.as_str())
        {
            let s = i64::try_from(start).ok();
            let e = i64::try_from(end).ok();
            // Move the row to THIS page: `move_to_wiki` sets `wiki_id`
            // alongside `source_path`/offsets so the invariant `wiki_id ==
            // wiki-of(source_path)` holds — a fact's home wiki is always the
            // wiki whose page physically carries it. (Plain `move_region`
            // would leave a stale `wiki_id` behind when the narrative
            // compiler renders a fact onto a page in another wiki, which is
            // what produced the wiki_id/source_path divergence.)
            fact_index::move_to_wiki(pool, &fid, wiki_id, source_path, s, e).await?;
        }
    }
    Ok(())
}

/// Expand the Cronista's lightweight `<fN>…</fN>` span tags into the bare
/// runtime `{{f=<uuid>}}…{{/}}` markers. `N` is the 1-based index
/// into `facts` (the order they were handed to the Cronista); the marker is
/// rendered HERE from `facts`, never copied by the LLM — which is what removes
/// the brace/attribute miscount failure mode of LLM-written markers. Only the
/// FIRST occurrence of each `N` is rendered; a duplicate, an out-of-range `N`,
/// or an unclosed open is unwrapped to plain text (the forward-completeness
/// guard then backfills any fact that never produced a marker).
fn expand_fact_tags(body: &str, facts: &[FactForPage]) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    let mut rendered: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut pos = 0;
    while let Some(rel) = body[pos..].find("<f") {
        let open = pos + rel;
        let n_start = open + 2;
        let n_len = body[n_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        // Require the exact shape `<fN>` — digits then `>`.
        if n_len == 0 || !body[n_start + n_len..].starts_with('>') {
            out.push_str(&body[pos..n_start]); // keep `<f`, scan on
            pos = n_start;
            continue;
        }
        let n: usize = body[n_start..n_start + n_len].parse().unwrap_or(0);
        let span_start = n_start + n_len + 1; // past `>`
        let close = format!("</f{n}>");
        let Some(crel) = body[span_start..].find(&close) else {
            // Unclosed open: drop the `<fN>` token, keep the rest as prose.
            out.push_str(&body[pos..open]);
            pos = span_start;
            continue;
        };
        let close_at = span_start + crel;
        out.push_str(&body[pos..open]); // prose before the tag
        let span = &body[span_start..close_at];
        if (1..=facts.len()).contains(&n) && rendered.insert(n) {
            let f = &facts[n - 1];
            // Embed-completeness guard: the canonical claim may carry
            // `{{embed=…}}` markers; the Cronista's rewritten span must
            // not silently drop them — re-append by code whatever the
            // model ate. The whole body is checked too: a marker the
            // model kept in adjacent prose must not be appended a second
            // time inside the span.
            let span_repaired = restore_missing_embeds(span, &f.text, body);
            out.push_str(&crate::capture::render_marker(&f.fact_id, &span_repaired));
        } else {
            // Out of range, or a duplicate tag → keep the prose, drop the tags.
            out.push_str(span);
        }
        pos = close_at + close.len();
    }
    out.push_str(&body[pos..]);
    out
}

/// Re-append any valid `{{embed=…}}` marker present in the fact's
/// canonical text but missing from the Cronista's rewritten span — and
/// from the rest of the page body (`full_body`): a marker the model
/// moved into adjacent prose is still on the page and must not be
/// duplicated. The marker is a load-bearing key — prose rewrites may
/// rephrase the words but never sever the media link.
fn restore_missing_embeds(span: &str, canonical_text: &str, full_body: &str) -> String {
    let canonical = parser::collect_embeds(canonical_text);
    if canonical.is_empty() {
        return span.to_owned();
    }
    let present = parser::collect_embeds(span);
    let mut out = span.to_owned();
    for cid in canonical {
        if !present.contains(&cid)
            && !full_body.contains(&crate::capture::render_embed_marker(&cid))
        {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(&crate::capture::render_embed_marker(&cid));
        }
    }
    out
}

/// Remove any leftover `<fN>` / `</fN>` tag tokens the expander didn't consume
/// (a stray close, or an open whose `N` was not a real fact) so no fact tag
/// survives into the published page.
fn strip_orphan_fact_tags(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut pos = 0;
    while let Some(rel) = body[pos..].find('<') {
        let lt = pos + rel;
        out.push_str(&body[pos..lt]);
        let mut c = lt + 1;
        if body[c..].starts_with('/') {
            c += 1;
        }
        if body[c..].starts_with('f') {
            let d_start = c + 1;
            let d_len = body[d_start..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if d_len > 0 && body[d_start + d_len..].starts_with('>') {
                pos = d_start + d_len + 1; // drop the whole `<fN>` / `</fN>` token
                continue;
            }
        }
        out.push('<'); // not a fact tag — keep the `<`
        pos = lt + 1;
    }
    out.push_str(&body[pos..]);
    out
}

/// The body of a `lista` page: one bullet record per fact, each wrapped in its
/// bare `{{f=…}}…{{/}}` region marker, with the done-cue **inside** the marker
/// so redaction hides a closure together with the fact it describes.
///
/// One function, two callers, and that is the point: the ingest turn refreshes
/// a list the moment it changes ([`refresh_list_page`]) and the compile
/// refreshes one that something else changed. Two renderers would drift, and
/// the drift would show as a page that flips shape depending on who touched it
/// last.
fn list_records<'a>(
    facts: impl Iterator<Item = (&'a FactId, &'a str, Option<&'a str>, Option<&'a str>)>,
) -> String {
    facts
        .map(|(fact_id, text, decay_reason, valid_to)| {
            let mut line = text.replace('\n', " ");
            if let Some(cue) = closure_cue(decay_reason, valid_to) {
                line.push_str(&cue);
            }
            format!("- {}", crate::capture::render_marker(fact_id, &line))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rewrite a `lista` page **now**, from the facts as they stand.
///
/// Founder, 2026-08-18: *«il classificatore si occupa delle liste, sia di
/// crearle che di aggiungere/togliere/modificare elementi. Il dream light no,
/// perché avviene dopo un'ora»* — and the reason, in his words: *«gestire una
/// lista è semplice, non è come scrivere la prosa, per cui aspettiamo un'ora e
/// bufferizziamo i fatti in modo da risparmiare»*. Waiting buys nothing here:
/// there is no model to call, no page to choose, nothing to judge. So a list
/// is brought up to date in the turn that changed it.
///
/// Adding an item already wrote it live (the capture path appends its marker).
/// The two gestures that change a record rather than add one do not, and both
/// come through here: `fact_index::close_validity` stamps the row and leaves
/// the file alone, so *«ho comprato il latte»* showed no `✓` until the next
/// compile, up to an hour later; and REPLACING an entry
/// ([`crate::capture::list_entry_superseded`]) cuts the old record's bytes out
/// from under its bullet and appends the new one at the end of the file. Both
/// leave the page a poor copy of the list until it is rebuilt, which costs
/// nothing here — so it is rebuilt in the turn that changed it.
///
/// Returns `Ok(false)` and touches nothing when the page is not `lista`-styled
/// — the caller does not have to know what kind of page a fact sits on.
///
/// The compile still renders list pages, and that is deliberate: a fact on a
/// list can also change from the dashboard, the comment channel, a forget
/// vote or REM's dedup, none of which run a turn. They all reconcile through
/// the plan's dirty set. In the ordinary case the compile finds the page
/// already correct and writes nothing.
///
/// # Errors
///
/// DB or filesystem failures. A page that cannot be parsed is left alone.
pub async fn refresh_list_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    source_path: &str,
) -> Result<bool> {
    let handle = tree.locate(&parse_wiki_id(wiki_id))?;
    let Some(rel) = source_path.strip_prefix(&format!("{}/", handle.rel_dir_posix())) else {
        return Ok(false);
    };
    let page_path = std::path::Path::new(rel);
    let Ok(existing) = handle.read_page(page_path) else {
        return Ok(false);
    };
    let Some(doc) = crate::wiki::MarkdownDoc::parse(&existing) else {
        return Ok(false);
    };
    // Only a `lista` page renders as records; anything else is the Cronista's
    // and is not this function's to touch.
    if crate::meta_annotate::parse_page_card(&existing).style != Some(crate::wiki::PageStyle::Lista)
    {
        return Ok(false);
    }

    let rows = fact_index::find_active_by_source_path(pool, source_path).await?;
    let body = list_records(rows.iter().map(|r| {
        (
            &r.fact_id,
            r.text.as_str(),
            r.decay_reason.as_deref(),
            r.valid_to.as_deref(),
        )
    }));
    // The testata is preserved verbatim — title, style, description, keywords
    // are not this function's to decide; only the records are rebuilt.
    let contents = format!("---\n{}\n---\n\n{body}\n", doc.frontmatter.trim_end());
    if contents == existing {
        return Ok(false);
    }
    handle.write_page(page_path, &contents)?;
    // The records moved, so every offset on this page is stale. Same repoint
    // the compile does after rendering one.
    let known: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.fact_id.as_str()).collect();
    repoint_facts(pool, &contents, &known, wiki_id, source_path).await?;
    Ok(true)
}

// ---------- the Record Writer (lista) ----------

/// Render a `lista`-style leaf as atomic records, **no LLM**.
///
/// A page whose ingest-decided style is `lista` holds atomic records
/// (a shopping list, a filmography) — data scanned/looked-up at a stroke, not
/// prose to be "understood". The facts are already atomic, so the compiler
/// renders each one **deterministically** as one bullet record wrapped in its
/// bare `{{f=…}}…{{/}}` region marker (the ACL gates it from the DB by that
/// key), bypassing the strong-model Cronista (cheap). One record per fact
/// means every fact keeps its protective per-fragment region with no
/// completeness guard needed, and recall still repoints onto the rendered
/// region.
async fn compile_list_page(
    pool: &SqlitePool,
    tree: &WikiTree,
    page: &PagePlan,
    now: &str,
) -> Result<PageOutcome> {
    // One bullet record per fact, each wrapped in its region marker rendered
    // HERE by code, never by an LLM: the marker carries the ACL. A record
    // is a single line, so any newline in the claim collapses to a space. A
    // closed record carries its done-cue INSIDE the marker, so redaction hides
    // the closure together with the fact it describes.
    let body = list_records(page.primary_facts.iter().map(|f| {
        (
            &f.fact_id,
            f.text.as_str(),
            f.decay_reason.as_deref(),
            f.valid_to.as_deref(),
        )
    }));

    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    // Testata: the style is `lista` (the ingest classifier's
    // per-page choice that routed us here); the description is the plan's
    // ingest-proposed page description — there is no Cronista on this path to emit one.
    let contents = render_page_file(
        page,
        &body,
        &page.description,
        style_or_default(page.style),
        &created,
        now,
    );
    let unchanged = contents == existing;
    if !unchanged {
        handle.write_page(page_path, &contents)?;
    }

    // Repoint each fact's fact_index row onto its compiled record region so recall
    // returns the rendered line; fact_index.text stays the canonical claim. Runs
    // on the Unchanged path too: a fact pre-pointed here as a pending render
    // (cross-page move whose record already sat on this page) still needs its
    // offsets stamped.
    let known: std::collections::BTreeSet<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();
    let abs = handle.abs_dir().join(page_path);
    let source_path = workdir_relative_source_path(tree.workdir(), &abs);
    repoint_facts(pool, &contents, &known, &page.wiki_id, &source_path).await?;
    if unchanged {
        return Ok(PageOutcome::Unchanged);
    }

    // Recall navigation: a record page that IS its wiki's identity card
    // (a `lista`-styled card) still owns the wiki's abstract.
    sync_foundation_summary(page, handle.abs_dir(), &page.description);

    Ok(PageOutcome::List)
}

/// The lista record's closure cue — the Record Writer's deterministic
/// counterpart of the Cronista's phrased closure ("comprato il 7 giugno").
///
/// Language-free on purpose: this path has no LLM to match the user's
/// language, so the cue is a glyph plus the closure date — `· ✓ <date>` for a
/// spent intention ([`fact_index::decay::COMPLETED`]), `· ✗ <date>` for a
/// retracted or contradicted one (the `item · done` shape the lista style is
/// described with at ingest). Keyed strictly on `decay_reason`: a window with
/// a future or merely-expired `valid_to` and no explicit closure gets no cue,
/// and an open record renders without one.
///
/// Takes the two fields rather than a plan row, so a record read straight from
/// `fact_index` — the turn's own refresh of a list — renders identically to
/// one carried on the compile plan.
fn closure_cue(decay_reason: Option<&str>, valid_to: Option<&str>) -> Option<String> {
    let reason = decay_reason?;
    let glyph = if reason == fact_index::decay::COMPLETED {
        '✓'
    } else {
        '✗'
    };
    // The date part of the closure instant (ISO-8601 `YYYY-MM-DD…`); a
    // malformed or missing `valid_to` degrades to the bare glyph.
    let date = valid_to
        .and_then(|t| t.get(..10))
        .map_or_else(String::new, |d| format!(" {d}"));
    Some(format!(" · {glyph}{date}"))
}

// ---------- helpers ----------

/// The bare id of a principal (`user:franz` / `group:famiglia` → `franz` /
/// `famiglia`) for the human-readable `(audience: …)` hint.
const fn principal_name(p: &Principal) -> &str {
    match p {
        Principal::User(id) | Principal::Group(id) => id.as_str(),
    }
}

/// Compact audience hint for a fact's `{primary_facts}` line — the per-fact
/// ACL projected for the Cronista. **Empty** for a *public* fact (`global` on
/// any axis: its substance is safe to weave into the page's default-visibility
/// connective prose); otherwise `(audience: <names>)` naming the read-set
/// (`subject ∪ allow ∪ sender`, sorted + deduped) so the Cronista keeps that
/// fact's substance **inside its own `<fN>` span** rather than leaking it into
/// the untagged prose every reader of the page sees (prompt FACT TAGS rule).
/// A forgotten author ([`crate::gdpr::removed_sender`]) is left out of the
/// names: it is an identity nobody holds, so it is nobody to name.
/// A one-way projection: the DB ACL stays authoritative and the marker is
/// still rendered by code from the fact — the hint is never parsed back.
fn audience_hint(subject: &Principal, allow: &[Principal], sender: Option<&Principal>) -> String {
    if crate::acl::is_public(subject, allow, sender) {
        return String::new();
    }
    let mut names: Vec<&str> = std::iter::once(subject)
        .chain(allow.iter())
        .chain(sender)
        .filter(|p| !crate::gdpr::is_removed(p))
        .map(principal_name)
        .collect();
    names.sort_unstable();
    names.dedup();
    format!(" (audience: {})", names.join(", "))
}

fn primary_facts_text(
    facts: &[FactForPage],
    now: &str,
    ref_alive: &dyn Fn(&str) -> bool,
    succ_home: &dyn Fn(&FactId) -> Option<String>,
) -> String {
    if facts.is_empty() {
        return "(no facts — write only a brief introduction)".to_owned();
    }
    // A NUMBERED list — the Cronista wraps each fact's prose span in `<fN>…</fN>`
    // by this 1-based number. We deliberately DO NOT show subject / allow / sender /
    // fact_id: the ACL is load-bearing and is rendered by code (see
    // [`expand_fact_tags`]), never copied by the LLM — so the model cannot drop
    // an `allow=` or miscount the braces of a marker it does not write.
    //
    // A fact that carries a validity window (`valid_from`/`valid_to`)
    // gets a compact `(validity: …)` hint appended to its line. This is a one-way
    // projection of `fact_index.valid_*` — the prompt tells the Cronista to weave
    // a readable validity cue into the prose; the DB stays authoritative and the
    // rendered cue is never parsed back.
    facts
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let ft = f.fact_type.as_deref().unwrap_or("other").to_uppercase();
            let validity = validity_hint(
                f.valid_from.as_deref(),
                f.valid_to.as_deref(),
                f.decay_reason.as_deref(),
                now,
            );
            // Succession pointer (structural aftercare): a closed fact whose
            // successor has a resolvable home elsewhere gets a `(current: …)`
            // hint, so the prose keeps the history AND points the reader one
            // hop from the current truth — instead of a well-written obituary
            // with no forward rail. Resolver semantics (`succ_home`): `None`
            // when the successor is unplaced or lives on this same page (the
            // Cronista already narrates both facts side by side there).
            let succession = f
                .successor_fact_id
                .as_ref()
                .and_then(succ_home)
                .map_or_else(String::new, |link| format!(" (current: {link})"));
            // Provenance breadcrumbs: when the fact's turn authored a
            // project page, the full detail already lives there. Surface the
            // `[[…]]` link(s) so the Cronista writes a terse reference instead
            // of restating the body (the prompt carries the instruction). One-way
            // projection from `fact_index.authored_refs`, never parsed back —
            // and existence-vetted through `ref_alive`: a ref whose target page
            // was removed (an absorbed dossier stub) stays in the DB as audit
            // provenance but never reaches prose as a dead rail.
            let live_refs: Vec<&str> = f
                .authored_refs
                .iter()
                .map(String::as_str)
                .filter(|r| ref_alive(r))
                .collect();
            let provenance = if live_refs.is_empty() {
                String::new()
            } else {
                format!(" (detail at: {})", live_refs.join(" "))
            };
            // Audience hint (per-fact ACL projection): a fact readable by less
            // than everyone gets a trailing `(audience: …)` so the Cronista
            // keeps its substance inside its own `<fN>` span — the untagged
            // connective prose is the page's default-visibility narrative and
            // must not paraphrase a restricted fact (prompt FACT TAGS). A
            // public fact carries no hint and weaves freely.
            let audience = audience_hint(&f.subject, &f.allow, f.sender.as_ref());
            format!(
                "{}. [{ft}] {}{audience}{validity}{succession}{provenance}",
                i + 1,
                f.text.replace('\n', " ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compact validity hint for a fact's `{primary_facts}` line. Empty
/// when the fact has no validity window; otherwise the raw ISO bounds in a
/// `(validity: …)` envelope the Cronista phrases naturally.
///
/// The one subtlety is the **open-ended** case (`valid_from` set, `valid_to`
/// `None`). A `valid_from` that is *not in the future* is the record/freshness
/// timestamp — the day we learned the fact — NOT a biographical onset. Narrating
/// it ("known as Sméagol since June 2026", "lives in Ferrara since today") is
/// false: an identity/durable fact has no onset to announce. So for an
/// already-in-effect open-ended fact we hand the Cronista a dateless
/// `(validity: open-ended)` and it weaves no start cue (cronista rule: an
/// open-ended start needs none). A **future** `valid_from` ("da lunedì cambio
/// ufficio") IS a genuine onset the user announced → keep the dated form so the
/// Cronista can phrase it.
///
/// A closed window may also carry its WHY (`decay_reason`:
/// `completed` / `retracted` / `contradicted`) — appended inside the
/// envelope so the Cronista can phrase the closure accurately ("bought
/// on…", "abandoned", "replaced by…") instead of a generic "until".
fn validity_hint(
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    decay_reason: Option<&str>,
    now: &str,
) -> String {
    let why = decay_reason.map_or_else(String::new, |r| format!(", closed: {r}"));
    match (valid_from, valid_to) {
        (None, None) => String::new(),
        (Some(from), Some(to)) => format!(" (validity: from {from} until {to}{why})"),
        (Some(from), None) if is_future(from, now) => {
            format!(" (validity: from {from}, open-ended)")
        },
        (Some(_), None) => " (validity: open-ended)".to_owned(),
        (None, Some(to)) => format!(" (validity: until {to}{why})"),
    }
}

/// `true` when `from` is strictly after `now` (both ISO-8601). On a parse
/// failure — vanishingly rare, the values come from `fact_index` and the ingest
/// prompt resolves them to `…T00:00:00Z` — default to NOT future, so a durable
/// fact's record date is never narrated as an onset (the conservative side of
/// the dogfood fix).
fn is_future(from: &str, now: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(from),
        chrono::DateTime::parse_from_rfc3339(now),
    ) {
        (Ok(f), Ok(n)) => f > n,
        _ => false,
    }
}

/// The canonical wikilink for one planned page — `[[wiki_id/page-slug]]`,
/// always a page (the grammar is on [`crate::recall::WikiLink`]).
///
/// The slug is the page **file's** stem, never the plan slug alone (which
/// would read as a hop to a wiki that does not exist). Every link the
/// compiler feeds the Cronista goes through here, so the prose only ever
/// sees resolvable rails.
fn plan_page_wikilink(page: &PagePlan) -> String {
    let stem = page
        .page_path
        .strip_suffix(".md")
        .unwrap_or(&page.page_path);
    format!("[[{}/{stem}]]", page.wiki_id)
}

/// Resolve a successor fact's home page to its canonical wikilink — the
/// `(current: …)` half of the succession hint in [`primary_facts_text`].
///
/// Placement-vetted by construction: the successor must be **planned** on
/// some page (cross-wiki allowed — the plan is forest-wide), so the rail
/// always lands on a compiled page; an unplaced successor yields `None`
/// and the hint is dropped (dead-rail discipline, like `ref_alive`).
/// `None` also when the successor lives on `current_slug` itself: the
/// Cronista already narrates predecessor and successor side by side
/// there, and a self-link would be noise.
fn successor_wikilink(
    plan: &CompilationPlan,
    current_slug: &str,
    successor: &FactId,
) -> Option<String> {
    let (slug, home) = plan.pages.iter().find(|(_, p)| {
        p.primary_facts
            .iter()
            .any(|f| f.fact_id.as_str() == successor.as_str())
    })?;
    if slug == current_slug {
        return None;
    }
    Some(plan_page_wikilink(home))
}

/// The link rail every leaf is shown: one line per page in the plan,
/// `- [[wikilink]]: description`.
///
/// It deliberately includes the page being written. Excluding it made the
/// block differ by one line for every call, which is precisely what a
/// prompt cache cannot absorb — and this block is ~3.5k tokens, the bulk
/// of the Cronista's input. Included, the block is one string per run,
/// built once and reused verbatim, and the prompt carries the one rule
/// that costs: never link a page to itself.
/// How many pages a plan may hold before the Cronista stops being shown the
/// **whole** index and gets a per-page selection instead.
///
/// The number is the point where the arithmetic flips, not a guess at a
/// corpus size. The whole index is byte-identical for every page of a run, so
/// it rides the cached prefix: the first call pays it, the rest pay roughly a
/// tenth. A per-page selection is different per call, so it is paid in full,
/// every time. A selection of `S` lines therefore beats a cached index of `B`
/// lines only while `S < B/10` — which is what makes the switch a ceiling
/// rather than a replacement, and which
/// [`CARD_INDEX_SELECTION_PAGES`] clears with room to spare.
///
/// ⚠️ **The tenth is one supplier's discount written as if it were
/// universal**, and it has never been checked against the backends actually
/// configured. If the real discount is a quarter, this belongs at 160, not
/// 400. Verify it before hanging another decision on it.
///
/// Below it nothing changes; the whole index is both cheaper and complete.
/// Above it the whole index stops fitting a call at all, and what the Cronista
/// is shown becomes a selection ([`crate::candidates`]).
pub const CARD_INDEX_CACHE_CEILING_PAGES: usize = 400;

/// How many cards a selection carries once the ceiling is passed —
/// [`crate::candidates::SELECTION_PAGES`], because what the selection *is* is
/// that module's business and only the ceiling is this one's.
pub const CARD_INDEX_SELECTION_PAGES: usize = crate::candidates::SELECTION_PAGES;

/// What the Cronista is shown of the rest of the memory.
///
/// Two shapes, chosen once per run by [`build_page_index`]:
///
/// - [`Self::Whole`] — every page, one string built once, identical for every
///   call, living in the **cacheable** half of the prompt.
/// - [`Self::Selected`] — above [`CARD_INDEX_CACHE_CEILING_PAGES`], a slice
///   composed by [`crate::candidates`]. It is different per call, so it moves
///   to the **task** half: left in the system half it would write one cache
///   entry per page and read none, which is strictly worse than not caching at
///   all.
enum PageIndex {
    Whole(String),
    /// Every page of the plan, with the traits a selection keys on.
    Selected(crate::candidates::CandidatePool),
}

/// Build the index for one run: whole below the ceiling, a candidate pool
/// above it.
///
/// The card vectors come from `page_card`, which the reindex pipeline fills —
/// the compiler has no embedder and deliberately does not grow one. A card
/// that was never embedded simply does not rank, which makes the offer
/// smaller, never wrong.
async fn build_page_index(pool: &SqlitePool, tree: &WikiTree, plan: &CompilationPlan) -> PageIndex {
    if plan.pages.len() <= CARD_INDEX_CACHE_CEILING_PAGES {
        return PageIndex::Whole(page_index_block(plan));
    }
    let by_source_path: BTreeMap<String, String> = plan
        .pages
        .iter()
        .filter_map(|(slug, p)| Some((plan_page_source_path(tree, p)?, slug.clone())))
        .collect();
    let candidates = crate::candidates::CandidatePool::load(pool, &by_source_path).await;
    tracing::info!(
        pages = plan.pages.len(),
        ceiling = CARD_INDEX_CACHE_CEILING_PAGES,
        embedded = candidates.embedded(),
        "compiler: page index over its cache ceiling — composing candidates per page"
    );
    PageIndex::Selected(candidates)
}

/// The workdir-relative path of a planned page — the `page_card` key.
fn plan_page_source_path(tree: &WikiTree, p: &PagePlan) -> Option<String> {
    let handle = tree
        .locate(&crate::types::WikiId::parse(&p.wiki_id).ok()?)
        .ok()?;
    Some(crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &handle.abs_dir().join(&p.page_path),
    ))
}

impl PageIndex {
    /// `(cacheable_half, task_half)` for one page.
    fn render_for(&self, plan: &CompilationPlan, page: &PagePlan) -> (String, String) {
        match self {
            Self::Whole(all) => (all.clone(), String::new()),
            Self::Selected(candidates) => (
                "(listed with your page below — a selection, not every page of the memory)"
                    .to_owned(),
                format!(
                    "OTHER PAGES you may [[wikilink]] (same rules as above). \
                     Each line says WHY it is here: `near` = its card resembles \
                     yours; `same-people` = it holds facts about, or told by, the \
                     same people as yours; `same-turn` = it holds facts said in \
                     the same conversation as yours; `same-wiki` = it simply \
                     lives beside yours; `far` = it resembles yours in NOTHING, \
                     and those are the ones a search from this page would never \
                     reach:\n{}",
                    Self::selection_lines(plan, page, candidates)
                ),
            ),
        }
    }

    /// The selection for one page, each line carrying the source that chose
    /// it.
    ///
    /// The fallback ordering handed to [`crate::candidates::CandidatePool::pick`]
    /// is this page's **own wiki, biggest first**. It is consulted only for a
    /// page with no card vector, and that is not the rare case: every page
    /// created in the run being compiled is one, and so is every page after an
    /// embedder failure. It must not be the plan's `BTreeMap` order, which is
    /// alphabetical and would re-introduce here the exact ordering this whole
    /// mechanism exists to abolish. Fact mass is the signal that survives with
    /// no vector at all: a page carrying fifty facts is a likelier destination
    /// than an empty one, and it is *importance*, the axis the founder allowed
    /// where similarity is unavailable (2026-08-09).
    fn selection_lines(
        plan: &CompilationPlan,
        page: &PagePlan,
        candidates: &crate::candidates::CandidatePool,
    ) -> String {
        let mut home: Vec<&PagePlan> = plan
            .pages
            .values()
            .filter(|p| p.wiki_id == page.wiki_id && p.slug != page.slug)
            .collect();
        home.sort_by_key(|p| std::cmp::Reverse(p.primary_facts.len()));
        let home: Vec<String> = home.into_iter().map(|p| p.slug.clone()).collect();

        let ask = candidates.ask_for([page.slug.as_str()]);
        let exclude: std::collections::BTreeSet<String> =
            std::iter::once(page.slug.clone()).collect();
        // Rendered in the order they were picked. Sorting the slice by slug
        // afterwards would hand the model an alphabetical list again, which is
        // the ordering this exists to get rid of; and where a list is cut, the
        // order IS the selection.
        let lines: Vec<String> = candidates
            .pick(&ask, &exclude, CARD_INDEX_SELECTION_PAGES, &home)
            .into_iter()
            .filter_map(|c| {
                let p = plan.pages.get(&c.key)?;
                Some(format!(
                    "- [{}] {}: {}",
                    c.source.tag(),
                    plan_page_wikilink(p),
                    p.description
                ))
            })
            .collect();
        if lines.is_empty() {
            "(no other pages)".to_owned()
        } else {
            lines.join("\n")
        }
    }
}

fn page_index_block(plan: &CompilationPlan) -> String {
    let lines: Vec<String> = plan
        .compilation_order
        .iter()
        .filter_map(|s| {
            let p = plan.pages.get(s)?;
            Some(format!("- {}: {}", plan_page_wikilink(p), p.description))
        })
        .collect();
    if lines.is_empty() {
        "(no pages)".to_owned()
    } else {
        lines.join("\n")
    }
}

/// Every link the plan holds for one page, as canonical wikilinks.
///
/// The plan's `link_graph` is not a wish list: it is what **this page**
/// already says, plus what the REM decided it should say (`planner`, step 9).
/// Which of them this compile may re-judge is [`link_targets`]'s question,
/// not this one's. A page that somebody else links to is handed nothing on
/// that account either way: a link puts no obligation on the page it points
/// at (founder, 2026-08-23).
///
/// **An identity card is never an OBLIGATION.** A mandatory link with no place
/// in the narrative is a sentence the page has to grow to host it, and a card
/// is the destination least likely to have earned one: a reader who arrives
/// with that person in the turn was served the card whole already
/// ([`crate::recall::turn_subjects`]), and the funnel marks a served card
/// visited, so it is never walked to on top of that.
///
/// **Whether to OFFER one is a different question, and the answer is yes.** It
/// stays in the page index, where linking it is a choice the prose earns: the
/// card of somebody this turn is not about is served to nobody, so a link to
/// it is a real door — and every link's clause becomes a recall key
/// ([`crate::link_key`]) whether or not anybody ever walks it. What is fenced
/// here is only what a page MUST point at. A card's OWN rails are untouched:
/// this is about what may be pointed AT, never about what a card may point at
/// from.
fn recommended_link_targets(plan: &CompilationPlan, slug: &str) -> Vec<String> {
    plan.link_graph
        .get(slug)
        .map(|ls| {
            ls.iter()
                // The graph stores plan slugs; a slug whose page vanished
                // from the plan would be a dead rail — skip it.
                .filter_map(|l| plan.pages.get(l))
                .filter(|p| p.page_path != crate::wiki::PROFILE_FILENAME)
                .map(plan_page_wikilink)
                .collect()
        })
        .unwrap_or_default()
}

/// Open the task half with `part`: after the marker line, ahead of the page.
///
/// The rendered prompt becomes three pieces — the standing brief, the part,
/// the page — and each boundary is forced:
///
/// - it cannot ride the **cached** half, because it carries this page's own
///   links and would write a cache entry per page while reading none, which is
///   worse than not caching at all (the same trap `{page_index_task}` avoids);
/// - it cannot follow the **page** either, because the brief opens by telling
///   the model its page is at the very end.
///
/// So it opens the task half, which is also where it belongs on its own terms:
/// a part is an instruction about how to write this page, and an instruction
/// comes before the thing it governs — the same order, for the same reason, as
/// the parts that open an `ingest` turn.
fn splice_task_part(prompt: &str, part: &str) -> String {
    let (system, task) = split_cronista_prompt(prompt);
    let Some(task) = task else {
        // An operator override with no marker is one undivided document sent
        // as the system prompt: there is no task half to open, so the part
        // rides the end rather than being dropped.
        return format!("{prompt}\n\n{part}");
    };
    let (marker, page) = task.split_once('\n').unwrap_or((task, ""));
    format!("{system}\n\n{marker}\n\n{part}\n\n{page}")
}

/// Render the `cronista-night` part for a page, or `None` when it is not wanted.
///
/// `None` on an empty `prior` — a page with no links of its own is asked
/// nothing and pays nothing for a brief about judging them — and `None` again
/// when the part cannot be read, which leaves the page written by the rules an
/// hourly rewrite gets rather than not written at all.
fn night_part(tree: &WikiTree, prior: &[String]) -> Option<String> {
    if prior.is_empty() {
        return None;
    }
    match prompts::render(
        "cronista-night",
        tree.workdir(),
        BUNDLED_CRONISTA_NIGHT_MD,
        &[("prior_links", prior.join(", ").as_str())],
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "compiler: nightly link brief unread — writing without it");
            None
        },
    }
}

/// A page's links, split by whether **this** compile may re-judge them.
///
/// Returns `(required, prior)`. `required` is what the page must say and what
/// [`missing_rails`] enforces; `prior` is what it said last time, offered to
/// the night for re-judgement through the `cronista-night` part.
///
/// **The split is by cadence, not by a record of who wrote each link**, and it
/// cannot be anything else: the plan reads a page's links off its own prose
/// ([`crate::planner::build_compilation_plan`], step 9), and prose does not say
/// which pass wrote a sentence. What the engine does know is which pass is
/// running now — and that is the axis the problem has. The hourly pass runs on
/// the cheap tier, so left free to re-judge it would spend the day undoing the
/// night; left binding at both cadences (which is what shipped before this
/// split) its sketch becomes an obligation the strong model cannot lift. So:
///
/// - [`Cadence::Light`] — everything is required. The hourly pass writes what
///   the page has and adds to it; it never takes one away.
/// - [`Cadence::Full`] — only the rails the REM parked earlier tonight are
///   required, because [`crate::rem::run_rail_writer`] runs *before* this
///   compile and a compile free to discard its choice would undo it in the
///   minute it was made. Everything else is `prior`: the night meets it as
///   ordinary prose and may keep it, replace it, or let it go.
fn link_targets(
    plan: &CompilationPlan,
    slug: &str,
    cadence: crate::dream::Cadence,
) -> (Vec<String>, Vec<String>) {
    let all = recommended_link_targets(plan, slug);
    if matches!(cadence, crate::dream::Cadence::Light) {
        return (all, Vec::new());
    }
    // Rendered through `plan_page_wikilink` like `all` is, so the two sides
    // compare as strings and cannot disagree about what a link looks like.
    let parked: std::collections::BTreeSet<String> = plan
        .authored_rails
        .iter()
        .filter(|(from, _)| from == slug)
        .filter_map(|(_, to)| plan.pages.get(to))
        .map(plan_page_wikilink)
        .collect();
    all.into_iter().partition(|l| parked.contains(l))
}

fn recommended_links(targets: &[String]) -> String {
    if targets.is_empty() {
        "none specific".to_owned()
    } else {
        targets.join(", ")
    }
}

/// The address a wikilink resolves to — `(wiki_id, page stem)`, the `.md`
/// suffix stripped and the `|display` alias already gone.
///
/// One page, one key, whichever side wrote the link: the recommendation
/// and the prose are compared through the same parser
/// ([`crate::recall::extract_wikilinks`], which is also what the recall
/// funnel harvests rails with) so the two cannot disagree about what a
/// link is. A bare `[[wiki_id]]` has no page and yields `None` — it names
/// a map, which is not a rail.
fn link_address(link: &crate::recall::WikiLink) -> Option<(String, String)> {
    let page = link.page.as_deref()?;
    let stem = page.strip_suffix(".md").unwrap_or(page);
    (!stem.is_empty()).then(|| (link.wiki_id.clone(), stem.to_owned()))
}

/// Which recommended rails never reached the prose, in the order they were
/// recommended.
///
/// The compiler hands the Cronista a list of links it must weave in and,
/// until this guard, checked nothing: a third of the required links never
/// reached the page text on the corpus this was measured on. A rail
/// that does not land is not a cosmetic loss — the navigator harvests its
/// candidates from the **prose**, so the neighbour simply cannot be walked
/// to from here.
fn missing_rails(recommended: &[String], body: &str) -> Vec<String> {
    let written: std::collections::BTreeSet<(String, String)> =
        crate::recall::extract_wikilinks(body)
            .iter()
            .filter_map(link_address)
            .collect();
    recommended
        .iter()
        .filter(|rail| {
            crate::recall::extract_wikilinks(rail)
                .first()
                .and_then(link_address)
                .is_some_and(|addr| !written.contains(&addr))
        })
        .cloned()
        .collect()
}

/// Preserve `created:` across recompiles by reading it back from the prior file.
fn preserved_created(existing: &str, now: &str) -> String {
    for line in existing.lines() {
        if let Some(rest) = line.trim().strip_prefix("created:") {
            let v = rest.trim().trim_matches(['"', '\'']);
            if !v.is_empty() {
                return v.to_owned();
            }
        }
    }
    // Date portion of `now` (YYYY-MM-DD) when no prior file.
    now.split('T').next().unwrap_or(now).to_owned()
}

/// Render the full page file (frontmatter + body). The **testata**
/// carries the per-page `style` (closed palette) and the free-text `description`
/// — the generic/per-page level of the two-level header (the specialized `_meta`
/// level is added later). `description` is omitted when
/// empty; `style` always defaults to `prosa`.
fn render_page_file(
    page: &PagePlan,
    body: &str,
    description: &str,
    style: crate::wiki::PageStyle,
    created: &str,
    now: &str,
) -> String {
    use std::fmt::Write as _;
    let date = now.split('T').next().unwrap_or(now);
    let title = page.title.replace('"', "'");
    let mut fm = String::with_capacity(body.len() + 256);
    fm.push_str("---\n");
    let _ = writeln!(fm, "title: \"{title}\"");
    let _ = writeln!(fm, "created: {created}");
    let _ = writeln!(fm, "updated: {date}");
    // No `page_type:` line: what kind of page this is is its file name.
    let _ = writeln!(fm, "style: {style}");
    let desc = description.replace(['"', '\n'], " ");
    let desc = desc.trim();
    if !desc.is_empty() {
        let _ = writeln!(fm, "description: \"{desc}\"");
    }
    fm.push_str("---\n\n");
    fm.push_str(body.trim());
    fm.push('\n');
    fm
}

/// The style a page is written in when nobody proposed one.
///
/// Prose: a compiled standard page is prose by default. The tag is how recall
/// reads the page back **and** how much room the night gives it before it
/// considers a split ([`crate::rem::mass_floor_for_style`]) — never a gate
/// that refuses anything. A style is one of three, fixed by type
/// ([`crate::wiki::PageStyle`]), so there is nothing to coerce.
pub(crate) fn style_or_default(style: Option<crate::wiki::PageStyle>) -> crate::wiki::PageStyle {
    style.unwrap_or(crate::wiki::PageStyle::Prosa)
}

/// The autobiography voice: the wiki's subject is an agent and it is writing
/// about itself. Legended in `cronista.md` under TONE.
const AGENT_TONE: &str = "agent-autobiography-first-person";

/// The voice of a person's own wiki, and the fallback for a page inside an
/// agent's wiki whose subject is somebody else.
const IDENTITY_TONE: &str = "narrative-first-person-when-sender-equals-subject";

/// Resolve the prose tone of a page's wiki from its `wiki_type`.
///
/// The known actor / root wiki types map straight to a fixed tone; every
/// other wiki type (topic wikis, content wikis) falls back to a
/// neutral narrative tone. Cached per wiki within a compile run — which is why
/// the per-page narrowing lives in [`tone_for_page`] and not here.
fn resolve_tone(tree: &WikiTree, wiki_id: &str) -> String {
    let Ok(handle) = tree.locate(&parse_wiki_id(wiki_id)) else {
        return "narrative".to_owned();
    };
    // An agent's own wiki is a `wiki-user` like a human's — the agent IS an
    // enrolled user — so the type alone would give it a human's voice, and its
    // self-facts ("the agent helped the user with…") would compile into a service
    // log written about it in the third person. It is an autobiography: the
    // subject writes it, so the voice is first person. Checked before the type
    // because it is the more specific claim about the same wiki.
    if handle.meta().is_agent {
        return AGENT_TONE.to_owned();
    }
    match handle.meta().wiki_type.as_str() {
        "wiki-user" => IDENTITY_TONE,
        "wiki-group" => "shared",
        _ => "narrative",
    }
    .to_owned()
}

/// Narrow the agent wiki's first-person voice to the pages that are actually
/// **about** the agent.
///
/// A wiki is one container, not one subject. An agent's wiki accumulates pages
/// whose subject is somebody else — misrouted before the agent-wiki guard was
/// live (the live deployment carries ~30% such residue: whole topic pages about
/// a user's pregnancy sitting in the assistant's wiki), and the residue does not
/// disappear the day the guard starts working. Compiling those in the first
/// person would have the assistant narrate a user's life as its own — a far
/// worse failure than the third-person log the voice exists to fix.
///
/// So the voice follows the page's dominant subject: the autobiography tone
/// only when most of the page's facts are owned by the agent itself. An
/// identity wiki's id is its principal's id, which is the whole test. Pages of
/// every other wiki are untouched.
/// Hard ceiling on an identity card's compiled body, in characters.
///
/// The founder's number (2026-08-03), and it is the **same** one the read
/// path enforces as `IngestPolicy::max_sender_identity_chars` — deliberately,
/// so a card written inside its authored bound is never cut when served. The
/// Cronista is told to aim well under it (~1800); this is the failsafe, and it
/// firing means the curation upstream did not happen.
pub const IDENTITY_CARD_CEILING_CHARS: usize = 2_500;

/// Which brief the Cronista should apply to this page.
///
/// `identity_card` only for a foundation node sitting on its **reserved**
/// page: the page type alone is not enough, because a `Person` node names the
/// actor and could in principle be planned elsewhere, and a brief that told
/// the model "this is served on every turn" about an ordinary page would be a
/// lie that costs prose.
fn page_kind(page: &PagePlan) -> &'static str {
    if page.is_identity_card() {
        "identity_card"
    } else {
        "leaf"
    }
}

fn tone_for_page(wiki_tone: &str, page: &PagePlan) -> String {
    if wiki_tone != AGENT_TONE {
        return wiki_tone.to_owned();
    }
    let agent = Principal::User(page.wiki_id.clone());
    let mine = page
        .primary_facts
        .iter()
        .filter(|f| f.subject == agent)
        .count();
    if mine * 2 > page.primary_facts.len() {
        AGENT_TONE.to_owned()
    } else {
        IDENTITY_TONE.to_owned()
    }
}

fn parse_wiki_id(s: &str) -> crate::types::WikiId {
    // The plan's wiki_id strings came from real wikis; if a parse ever fails
    // (corrupt plan) the subsequent locate() returns NotFound, surfaced as a
    // soft per-page error.
    crate::types::WikiId::parse(s).unwrap_or_else(|_| crate::types::WikiId::parse("root").unwrap())
}

/// Existence vetting for one `[[…]]` `authored_ref` before it reaches the
/// Cronista's `(detail at: …)` hint: the canonical link grammar resolved
/// against the live tree — `[[wiki_id]]` needs the wiki, `[[wiki_id/slug]]`
/// needs the page file too. A ref whose target vanished (an absorbed
/// dossier stub, a renamed page) is filtered from the projection — a dead
/// rail must never be woven into prose — while the DB row keeps it as
/// audit provenance.
fn authored_ref_resolves(tree: &WikiTree, r: &str) -> bool {
    let Some(inner) = r
        .trim()
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    else {
        return false;
    };
    let target = inner.split('|').next().unwrap_or(inner).trim();
    let (wiki_id, page) = match target.split_once('/') {
        Some((w, p)) => (w.trim(), Some(p.trim())),
        None => (target, None),
    };
    let Ok(parsed) = crate::types::WikiId::parse(wiki_id) else {
        return false;
    };
    let Ok(handle) = tree.locate(&parsed) else {
        return false;
    };
    match page {
        None | Some("") => true,
        Some(slug) => {
            let rel = std::path::PathBuf::from(format!("{slug}.md"));
            // Existence checked the way a case-insensitive filesystem
            // resolves: byte-exact first, else the unique case-insensitive
            // match — same resolution the recall navigator applies to page
            // hops.
            crate::wiki::is_safe_page_path(&rel)
                && crate::wiki::resolve_page_case_insensitive(handle.abs_dir(), &rel).is_some()
        },
    }
}

fn parse_cronista(raw: &str) -> Option<CronistaOutput> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<CronistaOutput>(&raw[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dream::Cadence;
    use crate::llm::FakeLlmBackend;
    use crate::planner::FactForPage;
    use crate::types::{FactId, Principal};
    use std::collections::BTreeMap;

    async fn setup() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(wikis.join("alice/cucina.md"), "# alice\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    /// An agent's own wiki gets the autobiography voice, and it wins over the
    /// type: the wiki IS a `wiki-user` (the agent is an enrolled user), so
    /// reading the type alone would compile its self-facts into a third-person
    /// dossier about it — "the agent helped the user with…" — instead of its own
    /// memory of the episode.
    #[tokio::test]
    async fn resolve_tone_gives_an_agent_wiki_the_first_person_voice() {
        let (dir, _tree, _pool) = setup().await;
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("hermes1")).unwrap();
        std::fs::write(
            wikis.join("hermes1/_meta.md"),
            "---\nwiki_id: hermes1\nwiki_type: wiki-user\nslug: hermes1\ntitle: Hermes\n\
             acl_default: 'user:hermes1'\nis_agent: true\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");

        assert_eq!(resolve_tone(&tree, "hermes1"), AGENT_TONE);
        assert_eq!(
            resolve_tone(&tree, "alice"),
            IDENTITY_TONE,
            "a human's wiki keeps the ordinary voice"
        );
    }

    /// …but only on the pages that are about it. An agent's wiki carries pages
    /// whose subject is somebody else — residue misrouted before the
    /// agent-wiki guard went live, ~30% of the live assistant's wiki — and
    /// compiling those in the first person would have the assistant narrate a
    /// user's pregnancy as its own life.
    #[test]
    fn tone_for_page_keeps_the_first_person_off_another_subjects_page() {
        let mut mine = page_with_subjects("hermes1", &["user:hermes1", "user:hermes1"]);
        assert_eq!(tone_for_page(AGENT_TONE, &mine), AGENT_TONE);

        // One stray fact does not flip a page that is mostly the agent's.
        mine.primary_facts
            .push(ffp_with_subject(9, "Carol parte lunedì", "user:carol"));
        assert_eq!(tone_for_page(AGENT_TONE, &mine), AGENT_TONE);

        let hers = page_with_subjects("hermes1", &["user:carol", "user:carol"]);
        assert_eq!(
            tone_for_page(AGENT_TONE, &hers),
            IDENTITY_TONE,
            "a page about someone else keeps the ordinary voice"
        );

        // A human's wiki is untouched by the narrowing.
        let plain = page_with_subjects("alice", &["user:alice"]);
        assert_eq!(tone_for_page(IDENTITY_TONE, &plain), IDENTITY_TONE);
    }

    /// The card brief must switch on the **page**, not on the type alone: a
    /// `Person` node planned anywhere but its reserved page is an ordinary
    /// leaf, and telling the model "this is served on every turn" about it
    /// would be a lie that costs prose on a page nobody serves.
    #[test]
    fn only_a_person_node_on_its_reserved_page_is_an_identity_card() {
        let mut page = page_with_subjects("alice", &["user:alice"]);

        page.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        assert_eq!(page_kind(&page), "identity_card");

        assert_eq!(page_kind(&page), "identity_card");

        // Right type, wrong page.
        page.page_path = "viaggi.md".to_owned();
        assert_eq!(page_kind(&page), "leaf");
    }

    /// The serve-time failsafe and the compile-time ceiling must be the same
    /// number, or a card written inside its authored bound would still be cut.
    #[test]
    fn the_card_ceiling_matches_what_the_read_path_will_serve() {
        assert_eq!(
            IDENTITY_CARD_CEILING_CHARS,
            crate::ingest::IngestPolicy::default().max_sender_identity_chars
        );
    }

    /// …and it is measured on the same quantity, not on the file.
    ///
    /// The testata, the `{{f=…}}` marker pair around every fact and the link
    /// syntax are machinery the read path resolves away. Counting them made a
    /// card that sits inside its authored budget report as over it on every
    /// compile, and a warning that fires routinely is one nobody reads on the
    /// day it is true.
    #[test]
    fn the_card_budget_measures_what_is_served_not_what_is_on_disk() {
        let uuid = "018f1234-5678-7abc-9def-0123456789ab";
        let file = format!(
            "---\ntitle: Alice\ntopics: [salute, casa]\nwiki_id: alice\n---\n\n\
             # Alice\n\n{{{{f={uuid}}}}}Alice cura l'orto{{{{/}}}} \
             insieme a [[famiglia/giardino|suo marito]].\n"
        );
        let served = served_chars(&file);
        assert_eq!(
            served,
            "# Alice\n\nAlice cura l'orto insieme a suo marito."
                .chars()
                .count(),
            "the testata, the markers and the link syntax are not served"
        );
        assert!(
            served < file.chars().count() / 2,
            "and the gap is not small: {served} of {}",
            file.chars().count()
        );
    }

    /// A `PagePlan` carrying one fact per subject string, for the tone tests.
    fn page_with_subjects(wiki_id: &str, subjects: &[&str]) -> PagePlan {
        PagePlan {
            slug: "pagina".to_owned(),
            title: "Pagina".to_owned(),
            description: String::new(),
            style: None,
            primary_facts: subjects
                .iter()
                .enumerate()
                .map(|(i, o)| ffp_with_subject(u8::try_from(i).unwrap_or(0), "un fatto", o))
                .collect(),
            outgoing_links: Vec::new(),
            pending_links: Vec::new(),
            wiki_id: wiki_id.to_owned(),
            page_path: "pagina.md".to_owned(),
        }
    }

    fn ffp_with_subject(seed: u8, text: &str, subject: &str) -> FactForPage {
        FactForPage {
            subject: subject.parse::<Principal>().unwrap(),
            ..ffp(seed, text)
        }
    }

    fn ffp(id_seed: u8, text: &str) -> FactForPage {
        FactForPage {
            topics: Vec::new(),
            subject_external: None,
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_seed:02x}"))
                .unwrap(),
            text: text.to_owned(),
            fact_type: Some("bio".to_owned()),
            subject: "user:alice".parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: "alice".to_owned(),
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            successor_fact_id: None,
            target_page: None,
            style: None,
            salience: None,
        }
    }

    /// What the completeness guard appends is marked off from the prose.
    ///
    /// The guard knows about MARKERS, never about meaning: a fact the Cronista
    /// wrote into its prose and forgot to tag comes back appended verbatim, so
    /// the page states it twice — which is how a page ends up looking as if it
    /// repeats its own opening on purpose. Dropping the append instead would
    /// trade a visible repetition for a fact with no ACL marker, so the append
    /// stays; the break is what stops it reading as the writer's own closing
    /// paragraph. The cure for the repetition itself is upstream, in the
    /// prompt: tag every fact.
    #[test]
    fn an_appended_fact_is_marked_off_from_the_prose_it_was_left_out_of() {
        let page = PagePlan {
            primary_facts: vec![
                ffp(1, "The hybrid estate was bought for 12,400."),
                ffp(2, "The scrappage incentive ran out on 30 April."),
            ],
            ..page_with_subjects("alice", &[])
        };
        // The writer tagged the first fact and left the second untagged.
        let out = expand_and_complete_fact_markers(
            "<f1>The hybrid estate was bought for 12,400.</f1>",
            &page,
        );
        let (prose, appendix) = out
            .split_once("\n\n---\n")
            .expect("the appended fact is marked off from the prose");
        assert!(
            prose.contains("12,400"),
            "the tagged fact stays where the writer put it: {prose}"
        );
        assert!(
            appendix.contains("30 April"),
            "and the untagged one lands after the break: {appendix}"
        );

        // An empty body is the one place a leading `---` would be read as
        // frontmatter rather than as a break.
        let from_nothing = expand_and_complete_fact_markers("", &page);
        assert!(
            !from_nothing.trim_start().starts_with("---"),
            "a page with no prose never opens on a separator: {from_nothing}"
        );
        assert!(
            from_nothing.contains("30 April") && from_nothing.contains("12,400"),
            "and both facts are still appended: {from_nothing}"
        );
    }

    #[test]
    fn primary_facts_text_appends_provenance_hint_only_when_present() {
        // A fact whose turn authored a project page gets a
        // `(detail at: [[…]])` suffix so the Cronista links instead of
        // duplicating; a pure-standard fact gets none.
        let plain = ffp(1, "Alice è celiaca.");
        let mut linked = ffp(2, "Ha rifatto il login del progetto.");
        linked.authored_refs = vec!["[[acme/auth]]".to_owned(), "[[acme/session]]".to_owned()];

        let out = primary_facts_text(
            &[plain, linked],
            "2026-06-21T00:00:00+00:00",
            &|_| true,
            &|_| None,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            !lines[0].contains("detail at"),
            "plain fact must carry no provenance hint: {}",
            lines[0]
        );
        assert!(
            lines[1].ends_with("(detail at: [[acme/auth]] [[acme/session]])"),
            "linked fact must carry the space-joined wikilinks: {}",
            lines[1]
        );
    }

    #[test]
    fn primary_facts_text_appends_audience_hint_only_for_restricted_facts() {
        // A public fact (global on any axis) weaves freely — no hint. A fact
        // readable by less than everyone carries `(audience: …)` naming the
        // read-set (subject ∪ allow ∪ sender, sorted + deduped) so the Cronista
        // keeps its substance inside its own <fN> span, out of the
        // default-visibility connective prose.
        let mut public = ffp(1, "La biblioteca apre alle 9.");
        public.subject = Principal::global();
        let restricted = ffp(2, "Alice ha un appuntamento in ospedale.");
        // ffp defaults to subject=user:alice, allow=[], sender=None.
        let mut shared = ffp(3, "Nota di famiglia su Gollum.");
        shared.subject = "user:gollum".parse::<Principal>().unwrap();
        shared.allow = vec!["group:famiglia".parse::<Principal>().unwrap()];
        shared.sender = Some("user:galadriel".parse::<Principal>().unwrap());

        let out = primary_facts_text(
            &[public, restricted, shared],
            "2026-06-21T00:00:00+00:00",
            &|_| true,
            &|_| None,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            !lines[0].contains("(audience:"),
            "a public fact carries no audience hint: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("(audience: alice)"),
            "a subject-only fact names its subject: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("(audience: famiglia, galadriel, gollum)"),
            "the read-set is subject ∪ allow ∪ sender, sorted + deduped: {}",
            lines[2]
        );
    }

    #[test]
    fn cronista_max_tokens_scales_with_fact_mass() {
        assert_eq!(cronista_max_tokens(0), 3_000); // floor
        assert_eq!(cronista_max_tokens(5), 3_000); // 2000+1000 → still floor
        assert_eq!(cronista_max_tokens(48), 11_600); // the live prod page
        assert_eq!(cronista_max_tokens(1_000), 32_000); // ceiling
    }

    #[test]
    fn primary_facts_text_filters_dead_authored_refs_from_the_hint() {
        // A ref whose target page vanished (an absorbed dossier stub) must
        // not be woven into prose: it is filtered from the `(detail at: …)`
        // hint — dropped entirely when nothing survives — while the DB row
        // keeps it as audit provenance.
        let mut linked = ffp(2, "Ha rifatto il login del progetto.");
        linked.authored_refs = vec!["[[acme/auth]]".to_owned(), "[[ghost/gone]]".to_owned()];
        let out = primary_facts_text(
            std::slice::from_ref(&linked),
            "2026-06-21T00:00:00+00:00",
            &|r| r == "[[acme/auth]]",
            &|_| None,
        );
        assert!(
            out.ends_with("(detail at: [[acme/auth]])"),
            "only the live ref survives: {out}"
        );

        let mut all_dead = ffp(3, "Nota orfana.");
        all_dead.authored_refs = vec!["[[ghost/gone]]".to_owned()];
        let out = primary_facts_text(
            std::slice::from_ref(&all_dead),
            "2026-06-21T00:00:00+00:00",
            &|_| false,
            &|_| None,
        );
        assert!(
            !out.contains("detail at"),
            "an all-dead ref list must drop the hint entirely: {out}"
        );
    }

    #[test]
    fn primary_facts_text_appends_succession_hint_via_resolver() {
        // Structural aftercare: a closed fact whose successor resolves to a
        // home page gets a `(current: [[…]])` hint so the prose can point
        // one hop from the obituary to today's truth; an unresolvable
        // successor (unplaced, or homed on this same page) drops the hint —
        // the same dead-rail discipline as `ref_alive`.
        let mut closed = ffp(4, "Nome candidato: Sirio.");
        closed.valid_to = Some("2026-06-20T00:00:00Z".to_owned());
        closed.decay_reason = Some("contradicted".to_owned());
        let succ = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dee").unwrap();
        closed.successor_fact_id = Some(succ.clone());

        let out = primary_facts_text(
            std::slice::from_ref(&closed),
            "2026-06-21T00:00:00+00:00",
            &|_| true,
            &|s| (s == &succ).then(|| "[[morgana/nomi]]".to_owned()),
        );
        assert!(
            out.contains("(current: [[morgana/nomi]])"),
            "a resolved successor appends the hint: {out}"
        );

        let out = primary_facts_text(
            std::slice::from_ref(&closed),
            "2026-06-21T00:00:00+00:00",
            &|_| true,
            &|_| None,
        );
        assert!(
            !out.contains("current:"),
            "an unresolvable successor drops the hint entirely: {out}"
        );
    }

    #[test]
    fn successor_wikilink_resolves_within_the_plan() {
        // The resolver half: plan-vetted (the successor must be placed on
        // some page — cross-wiki allowed), self-page suppressed (the
        // Cronista already narrates both facts side by side there).
        fn leaf(slug: &str, wiki: &str, page_path: &str, fact: FactForPage) -> PagePlan {
            PagePlan {
                slug: slug.to_owned(),
                title: slug.to_owned(),
                description: String::new(),
                style: None,
                primary_facts: vec![fact],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: wiki.to_owned(),
                page_path: page_path.to_owned(),
            }
        }
        let old = ffp(1, "meal prep v1");
        let succ = ffp(2, "meal prep v2");
        let succ_id = succ.fact_id.clone();
        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "vecchia".to_owned(),
            leaf("vecchia", "morgana", "vecchia.md", old),
        );
        pages.insert(
            "meal_prep".to_owned(),
            leaf("meal_prep", "hermes1", "meal_prep.md", succ),
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 2,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        assert_eq!(
            successor_wikilink(&plan, "vecchia", &succ_id).as_deref(),
            Some("[[hermes1/meal_prep]]"),
            "a successor placed on another page resolves cross-wiki"
        );
        assert_eq!(
            successor_wikilink(&plan, "meal_prep", &succ_id),
            None,
            "a successor homed on the page being compiled yields no link"
        );
        let phantom = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99").unwrap();
        assert_eq!(
            successor_wikilink(&plan, "vecchia", &phantom),
            None,
            "an unplaced successor yields no link (dead-rail discipline)"
        );
    }

    #[tokio::test]
    async fn authored_ref_resolves_vets_against_the_live_tree() {
        let (_dir, tree, _pool) = setup().await;
        // Wiki hop: the wiki exists. Page hop: the file must exist too.
        assert!(authored_ref_resolves(&tree, "[[alice]]"));
        assert!(authored_ref_resolves(&tree, "[[alice/cucina]]"));
        assert!(!authored_ref_resolves(&tree, "[[alice/missing]]"));
        assert!(!authored_ref_resolves(&tree, "[[ghost]]"));
        assert!(!authored_ref_resolves(&tree, "[[ghost/page]]"));
        // Mutant shapes never resolve: no brackets, traversal, empty.
        assert!(!authored_ref_resolves(&tree, "alice/cucina"));
        assert!(!authored_ref_resolves(&tree, "[[alice/../secret]]"));
        assert!(!authored_ref_resolves(&tree, "[[]]"));
    }

    async fn plant_fact_at(
        pool: &SqlitePool,
        fid: &FactId,
        subject: &str,
        text: &str,
        source_path: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) {
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: source_path.to_owned(),
                region_start: start,
                region_end: end,
                text: text.to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: subject.parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    async fn plant_fact(pool: &SqlitePool, fid: &FactId, subject: &str, text: &str) {
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: text.to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: subject.parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no
                // classifier placement proposal to carry.
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    /// The wiki's abstract comes from the wiki's OWN page — its card — and
    /// from nowhere else. What decides that is the page's file name.
    #[tokio::test]
    async fn only_a_wikis_own_page_writes_its_abstract() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        plant_fact_at(
            &pool,
            &fid,
            "user:alice",
            "Alice loves pasta",
            "wikis/alice/appunti_vari.md",
            None,
            None,
        )
        .await;
        let mut plan = leaf_plan(&fid);
        let card = plan.pages.get_mut("alice").expect("page");
        card.page_path = crate::wiki::PROFILE_FILENAME.to_owned();
        let body = "{\"mergedBody\":\"Chi è Alice.\",\"description\":\"d\"}".to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile");

        let meta = std::fs::read_to_string(dir.path().join("wikis/alice/_meta.md")).unwrap();
        assert!(
            meta.contains("summary: d"),
            "the card's description became the wiki's abstract: {meta}"
        );
        drop(dir);
    }

    fn leaf_plan(fid: &FactId) -> CompilationPlan {
        let mut pages = BTreeMap::new();
        pages.insert(
            "alice".to_owned(),
            PagePlan {
                slug: "alice".to_owned(),
                title: "Alice".to_owned(),
                description: "Alice".to_owned(),
                style: None,
                primary_facts: vec![FactForPage {
                    topics: Vec::new(),
                    subject_external: None,
                    authored_refs: Vec::new(),
                    fact_id: fid.clone(),
                    text: "Alice loves pasta".to_owned(),
                    fact_type: Some("preference".to_owned()),
                    subject: "user:alice".parse::<Principal>().unwrap(),
                    allow: Vec::new(),
                    sender: None,
                    source_wiki_id: "alice".to_owned(),
                    valid_from: None,
                    valid_to: None,
                    decay_reason: None,
                    successor_fact_id: None,
                    target_page: None,
                    style: None,
                    salience: None,
                }],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "cucina.md".to_owned(),
            },
        );
        CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["alice".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 1,
            dirty_pages: vec!["alice".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cronista_writes_prose_with_marker_and_repoints_fact() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        // Plant a promoted fact with no offsets — a pending render.
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no
                // classifier placement proposal to carry.
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        // The Cronista tags the claim's span with the lightweight `<f1>…</f1>`
        // tag; the code renders the bare runtime `{{f=<fid>}}…{{/}}` marker.
        let body =
            "{\"mergedBody\":\"A proposito di pasta. <f1>Alice ama la pasta.</f1>\",\"description\":\"d\"}"
                .to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let plan = leaf_plan(&fid);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(report.leaves, 1);

        // The compiled page exists with the marker.
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(page.contains("Alice ama la pasta."));
        assert!(page.contains(&format!("f={fid}")));
        // The fact_index row was repointed onto the compiled page.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/cucina.md");
        assert!(row.region_start.is_some(), "offsets repointed");
        assert_eq!(
            row.text, "Alice loves pasta",
            "canonical claim text preserved"
        );
        // No wiki abstract: this page is `cucina.md`, an ordinary page, and
        // only a wiki's OWN page carries its abstract. What a page is IS its
        // file name, so the two cannot disagree. The abstract has its own
        // test below.
        let meta = std::fs::read_to_string(dir.path().join("wikis/alice/_meta.md")).unwrap();
        assert!(
            !meta.contains("summary: d"),
            "an ordinary page does not speak for its wiki: {meta}"
        );
        drop(dir);
    }

    /// The cacheable split, end to end. The system half must carry the
    /// standing brief and the page index and **nothing that identifies the
    /// page** — a title in the system prompt makes every call's prefix
    /// unique, which silently costs a cache write per page and earns no
    /// read. The per-page half must ride the user turn instead.
    #[tokio::test]
    async fn cronista_sends_the_stable_brief_as_system_and_the_page_as_user() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99").unwrap();
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
        let body = "{\"mergedBody\":\"Pasta. <f1>Alice ama la pasta.</f1>\",\"description\":\"d\"}"
            .to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let plan = leaf_plan(&fid);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile");

        let system = cronista.last_system_prompt().expect("system prompt sent");
        let user = cronista.last_prompt().expect("user prompt sent");
        assert!(
            system.contains("ONE FACT, ONE PAGE"),
            "the standing brief stays in the cacheable half: {system}"
        );
        assert!(
            system.contains("OTHER PAGES"),
            "the page index stays in the cacheable half"
        );
        assert!(
            !system.lines().any(|l| l.trim() == CRONISTA_TASK_MARKER),
            "the separator line opens the user half — the brief may name it in \
             prose, but never carry it as a line of its own: {system}"
        );
        assert!(
            !system.contains("Alice loves pasta"),
            "this page's facts must not sit in the shared prefix: {system}"
        );
        assert!(
            user.starts_with(CRONISTA_TASK_MARKER),
            "the per-page half leads the user turn: {user}"
        );
        assert!(
            user.contains("Alice loves pasta"),
            "the page's own facts ride the user turn: {user}"
        );
        assert!(
            user.contains("Return the JSON object only"),
            "the write instruction closes the user turn: {user}"
        );
        drop(dir);
    }

    /// The page's language is the one its owner declared, and the directive is
    /// the last instruction the writer reads before the page — on the nightly
    /// pass too, which is the one that puts something between them.
    ///
    /// One compiled page out of forty-six came back written end to end in
    /// Italian on an all-English corpus whose three people were all declared
    /// `en-GB` — its own title, description and facts stayed English, so
    /// nothing was mis-derived and the directive was served correctly. The
    /// writer went with the language of the worked examples it had just read.
    /// Which is why the position matters and why it is asserted per cadence:
    /// on a full compile `splice_task_part` opens the task half with the
    /// `cronista-night` part, whose own examples would otherwise be the last
    /// prose before the page. Nothing here was covered at all — the compiler
    /// had no test that the language reached the model.
    #[tokio::test]
    async fn the_page_is_written_in_the_language_its_owner_declared() {
        for (locale, expected) in [
            ("it-IT", "Respond in Italian"),
            ("en-GB", "Respond in English"),
        ] {
            for cadence in [Cadence::Light, Cadence::Full] {
                let (dir, tree, pool) = setup().await;
                sqlx::query(
                    "INSERT INTO enrollment_users (user_id, locale, is_admin) VALUES ('alice', ?1, 0)",
                )
                .bind(locale)
                .execute(&pool)
                .await
                .unwrap();
                let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99").unwrap();
                fact_index::insert(
                    &pool,
                    &crate::fact_index::NewFact {
                        subject_external: None,
                        slot: None,
                        slot_value: None,
                        authored_refs: Vec::new(),
                        fact_id: fid.clone(),
                        wiki_id: "alice".to_owned(),
                        source_path: "wikis/alice/appunti_vari.md".to_owned(),
                        region_start: None,
                        region_end: None,
                        text: "Alice loves pasta".to_owned(),
                        embedding: vec![0.1, 0.2],
                        subject_id: "user:alice".parse::<Principal>().unwrap(),
                        allow_ids: Vec::new(),
                        sender_id: None,
                        fact_type: Some("preference".to_owned()),
                        topics: Vec::new(),
                        valid_from: None,
                        valid_to: None,
                        target_page: None,
                        style: None,
                        salience: None,
                        source_ref: None,
                    },
                )
                .await
                .unwrap();
                // A neighbour the page already links to: on the full cadence
                // that is a `prior` link, which is what makes `night_part`
                // render and splice ahead of the page.
                let mut plan = leaf_plan(&fid);
                let neighbour = PagePlan {
                    slug: "spesa".to_owned(),
                    title: "Spesa".to_owned(),
                    description: "the list".to_owned(),
                    style: None,
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: "alice".to_owned(),
                    page_path: "spesa.md".to_owned(),
                };
                plan.pages.insert("spesa".to_owned(), neighbour);
                plan.link_graph
                    .insert("alice".to_owned(), vec!["spesa".to_owned()]);

                let cronista = FakeLlmBackend::new(
                    "fake",
                    "{\"mergedBody\":\"P. <f1>Alice loves pasta.</f1>\",\"description\":\"d\"}",
                );
                compile_dirty_pages(
                    &pool,
                    &tree,
                    &plan,
                    &cronista,
                    cadence,
                    "2026-05-31T00:00:00Z",
                )
                .await
                .expect("compile");

                let user = cronista.last_prompt().expect("user prompt sent");
                assert!(
                    user.contains(expected),
                    "{cadence:?}: the writer was not told to write in the language {locale} \
                     declares: {user}"
                );
                let language_at = user.find("LANGUAGE:").expect("the LANGUAGE directive");
                let page_at = user.find("\nPAGE: ").expect("the page it governs");
                assert!(
                    language_at < page_at,
                    "{cadence:?}: the directive must come before the page"
                );
                // Nothing instructional between the two. On the full cadence
                // the night part is present and must sit BEFORE the directive,
                // which is the whole point of asserting per cadence.
                let between = &user[language_at..page_at];
                assert!(
                    !between.contains("LINKS THIS PAGE CARRIED LAST TIME"),
                    "{cadence:?}: the night part sits between the directive and the page: {between}"
                );
                if matches!(cadence, Cadence::Full) {
                    assert!(
                        user.contains("LINKS THIS PAGE CARRIED LAST TIME"),
                        "the full cadence must actually splice the night part, else this \
                         case proves nothing: {user}"
                    );
                }
                drop(dir);
            }
        }
    }

    /// Every worked example the writer is shown is in the language the
    /// directive names by default.
    ///
    /// The brief's samples are the only prose in it that has a language, and
    /// a model writing prose reaches for the prose it has just read. They are
    /// English, like the rest of the repository's surface, and the directive
    /// is what moves a memory into another one — not the examples.
    #[test]
    fn the_brief_shows_its_worked_examples_in_english() {
        for sample in [
            "**Creatinine** — 2.53 mg/dL on 12 May 2026",
            "The colour I like best is teal.",
            "\"I helped…\", \"I tend to…\"",
        ] {
            assert!(
                BUNDLED_CRONISTA_MD.contains(sample),
                "a worked example the writer copies its voice from is gone: {sample}"
            );
        }
    }

    /// A prompt with no marker — an operator override written against an
    /// older bundled body — keeps everything in the system prompt, and
    /// (asserted in the unit test below) marks nothing cacheable.
    #[test]
    fn split_cronista_prompt_degrades_without_the_marker() {
        let (system, task) = split_cronista_prompt("brief\n\n=== PAGE TO WRITE ===\nPAGE: x");
        assert_eq!(system, "brief");
        assert_eq!(task, Some("=== PAGE TO WRITE ===\nPAGE: x"));

        let (system, task) = split_cronista_prompt("an override with no marker at all");
        assert_eq!(system, "an override with no marker at all");
        assert_eq!(task, None, "no marker ⇒ no split ⇒ no cache hint");

        // The brief names the marker in prose so the model knows where its
        // page is. Only the standalone line may cut the prompt — cutting at
        // the mention would ship the rules as if they were the task.
        let (system, task) = split_cronista_prompt(
            "read on after the `=== PAGE TO WRITE ===` line\nrules\n=== PAGE TO WRITE ===\nPAGE: x",
        );
        assert_eq!(
            system, "read on after the `=== PAGE TO WRITE ===` line\nrules",
            "an in-prose mention is not a separator"
        );
        assert_eq!(task, Some("=== PAGE TO WRITE ===\nPAGE: x"));
    }

    #[tokio::test]
    async fn cronista_testata_records_style_and_description_in_frontmatter() {
        // The compiler writes the per-page testata — the Cronista's
        // compile-time `style` choice (normalised to the closed palette) and its
        // free-text page description.
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no
                // classifier placement proposal to carry.
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let body = "{\"mergedBody\":\"<f1>Alice ama la pasta.</f1>\",\"description\":\"Cosa piace ad Alice\",\"style\":\"prosa-tecnica\"}".to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let plan = leaf_plan(&fid);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-06T00:00:00Z",
        )
        .await
        .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("\nstyle: prosa-tecnica\n"),
            "the Cronista's style is recorded in the testata: {page}"
        );
        assert!(
            page.contains("description: \"Cosa piace ad Alice\""),
            "the Cronista's description is recorded in the testata: {page}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn ingest_plan_style_wins_over_cronista_in_testata() {
        // When the ingest classifier proposed a per-page `style` (carried
        // on the plan as `PagePlan.style`), the testata uses it; the Cronista's
        // compile-time style is the fallback only. Here ingest says
        // `prosa-tecnica` and the Cronista says `prosa` → the plan wins.
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let body = "{\"mergedBody\":\"<f1>Alice ama la pasta.</f1>\",\"description\":\"Cosa piace ad Alice\",\"style\":\"prosa\"}".to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let mut plan = leaf_plan(&fid);
        // The ingest classifier proposed `prosa-tecnica` for this page.
        plan.pages.get_mut("alice").unwrap().style = Some(crate::wiki::PageStyle::ProsaTecnica);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-06T00:00:00Z",
        )
        .await
        .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("\nstyle: prosa-tecnica\n"),
            "the ingest plan's style wins over the Cronista's: {page}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn lista_style_renders_records_bypassing_the_cronista() {
        // A leaf whose ingest-decided style is `lista` is rendered as
        // atomic bullet records — each wrapped in its ACL marker — WITHOUT
        // calling Il Cronista (the strong prose model): the `lista` testata
        // matches a record body.
        let (dir, tree, pool) = setup().await;
        let f1 = ffp(0x11, "latte");
        let f2 = ffp(0x12, "forbici");
        plant_fact(&pool, &f1.fact_id, "user:alice", "latte").await;
        plant_fact(&pool, &f2.fact_id, "user:alice", "forbici").await;

        let mut pages = BTreeMap::new();
        pages.insert(
            "spesa".to_owned(),
            PagePlan {
                slug: "spesa".to_owned(),
                title: "Spesa".to_owned(),
                description: "La lista della spesa".to_owned(),
                style: Some(crate::wiki::PageStyle::Lista),
                primary_facts: vec![f1.clone(), f2.clone()],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "spesa.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["spesa".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 2,
            dirty_pages: vec!["spesa".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        // If the Cronista were (wrongly) invoked, this distinctive prose would
        // land on the page. The record path must bypass it entirely.
        let cronista = FakeLlmBackend::new(
            "fake",
            "{\"mergedBody\":\"PROSE_FROM_CRONISTA\",\"description\":\"d\"}",
        );
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-07T00:00:00Z",
        )
        .await
        .expect("compile");

        assert_eq!(report.lists, 1, "counted as a list page");
        assert_eq!(report.leaves, 0, "the Cronista leaf path was not taken");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md")).unwrap();
        assert!(
            !page.contains("PROSE_FROM_CRONISTA"),
            "the Cronista was bypassed: {page}"
        );
        assert!(
            page.contains("\nstyle: lista\n"),
            "the testata style is lista: {page}"
        );
        // Each fact is a bullet record wrapped in its bare region marker
        // (the ACL gates from the DB by key, not from inline attributes).
        assert!(page.contains("- {{f="), "bullet record + marker: {page}");
        assert!(
            !page.contains("subject=") && !page.contains("owner="),
            "no inline ACL on disk: {page}"
        );
        assert!(page.contains("latte{{/}}"), "latte record present: {page}");
        assert!(
            page.contains(&format!("f={}", f1.fact_id))
                && page.contains(&format!("f={}", f2.fact_id)),
            "both facts carry their marker: {page}"
        );

        // The fact_index rows were repointed onto the compiled records.
        let row = fact_index::find_by_id(&pool, &f1.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/spesa.md");
        assert!(row.region_start.is_some(), "offsets repointed");
        assert_eq!(row.text, "latte", "canonical claim text preserved");
        drop(dir);
    }

    #[tokio::test]
    async fn fact_less_leaf_renders_deterministically_without_the_llm() {
        // A foundation page with no facts yet must NOT reach the Cronista:
        // handed an empty fact list, the model invents colour prose from
        // wikilinks alone. The deterministic render is the description
        // page description under a normal testata, idempotent across compiles.
        let (dir, tree, pool) = setup().await;
        let mut pages = BTreeMap::new();
        pages.insert(
            "alice".to_owned(),
            PagePlan {
                slug: "alice".to_owned(),
                title: "Alice".to_owned(),
                description: "Identity wiki for alice.".to_owned(),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "cucina.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["alice".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: vec!["alice".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        // If the Cronista were (wrongly) invoked, this prose would land.
        let cronista = FakeLlmBackend::new(
            "fake",
            "{\"mergedBody\":\"INVENTED_LORE\",\"description\":\"d\"}",
        );
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(report.leaves, 1, "rendered, counted as a leaf");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            !page.contains("INVENTED_LORE"),
            "no LLM on the fact-less path: {page}"
        );
        assert!(
            page.contains("_Identity wiki for alice._"),
            "the description is the whole body: {page}"
        );
        assert!(!page.contains("{{f="), "no markers without facts: {page}");
        assert!(
            !page.contains("[["),
            "and no rails were required of it: {page}"
        );

        // Idempotent: the same compile re-run is a no-op.
        let report2 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile 2");
        assert_eq!(report2.unchanged, 1, "second render matches byte-for-byte");
        drop(dir);
    }

    /// **A page with no facts still writes the links it is required to
    /// carry.**
    ///
    /// A page's links are read back off its own prose at every build, so a
    /// render that drops them loses them: the next plan finds none, the graph
    /// forgets the page, and a rail the REM decided is asked for again and
    /// dropped again, every night. A person's identity card is exactly the
    /// page this happens to — it exists from the moment they are enrolled and
    /// carries no facts until the first one lands.
    #[tokio::test]
    async fn a_fact_less_page_still_writes_the_rails_the_plan_requires() {
        let (dir, tree, pool) = setup().await;
        let mut pages = BTreeMap::new();
        for (slug, path, facts) in [
            ("alice", "@profile.md", Vec::new()),
            ("cucina", "cucina.md", vec![ffp(0x41, "alice cooks")]),
        ] {
            pages.insert(
                slug.to_owned(),
                PagePlan {
                    slug: slug.to_owned(),
                    title: slug.to_owned(),
                    description: format!("{slug} desc"),
                    style: None,
                    primary_facts: facts,
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: "alice".to_owned(),
                    page_path: path.to_owned(),
                },
            );
        }
        let mut link_graph = BTreeMap::new();
        link_graph.insert("alice".to_owned(), vec!["cucina".to_owned()]);
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: vec!["alice".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: vec!["alice".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let cronista =
            FakeLlmBackend::new("fake", "{\"mergedBody\":\"NOPE\",\"description\":\"d\"}");
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-08-23T00:00:00Z",
        )
        .await
        .expect("compile");

        let page =
            std::fs::read_to_string(dir.path().join("wikis/alice/@profile.md")).expect("written");
        assert!(
            !page.contains("NOPE"),
            "still no model on this path: {page}"
        );
        assert!(page.contains("_alice desc_"), "{page}");
        assert!(
            page.contains("[[alice/cucina]]"),
            "the rail is on the page, so the next build can read it back: {page}"
        );
        // And it reads back as the same edge, through the same two functions
        // the plan's harvest uses — which is the whole point of writing it.
        let read_back: Vec<String> = crate::recall::extract_wikilinks(&page)
            .into_iter()
            .filter_map(|l| {
                l.page
                    .map(|pg| crate::planner::plan_slug_for_page(&l.wiki_id, &pg))
            })
            .collect();
        assert_eq!(read_back, vec!["cucina".to_owned()], "{page}");
        drop(dir);
    }

    /// The orphan-file sweep: a page file the plan no longer references
    /// and no live row points at is deleted; a file with a live pointer,
    /// a reserved name, or a plan entry survives.
    #[tokio::test]
    async fn orphan_page_files_are_swept_unless_pointed_at_or_reserved() {
        let (dir, tree, pool) = setup().await;
        let f1 = ffp(0x31, "latte");
        plant_fact(&pool, &f1.fact_id, "user:alice", "latte").await;

        let mut pages = BTreeMap::new();
        pages.insert(
            "spesa".to_owned(),
            PagePlan {
                slug: "spesa".to_owned(),
                title: "Spesa".to_owned(),
                description: "La lista della spesa".to_owned(),
                style: Some(crate::wiki::PageStyle::Lista),
                primary_facts: vec![f1.clone()],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "spesa.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["spesa".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 1,
            dirty_pages: vec!["spesa".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        let alice_dir = dir.path().join("wikis/alice");
        // A zombie: not in the plan, stale marker copy, no row points at it.
        std::fs::write(
            alice_dir.join("registro_spesa.md"),
            "{{f=0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77}}stale copy{{/}}\n",
        )
        .unwrap();
        // Not in the plan but a LIVE row points at it → protected.
        let f2 = ffp(0x32, "vecchia nota");
        plant_fact_at(
            &pool,
            &f2.fact_id,
            "user:alice",
            "vecchia nota",
            "wikis/alice/vecchie_note.md",
            Some(0),
            Some(10),
        )
        .await;
        std::fs::write(
            alice_dir.join("vecchie_note.md"),
            format!("{{{{f={}}}}}vecchia nota{{{{/}}}}\n", f2.fact_id),
        )
        .unwrap();
        // Reserved names survive even with no rows.
        std::fs::write(alice_dir.join("@rules.md"), "# Rules\n").unwrap();
        // The scaffolding page `setup` seeds is not part of this count.
        std::fs::remove_file(alice_dir.join("cucina.md")).ok();
        // `index.md` is not reserved on the sweep side: a file the plan does
        // not know about, with no fact pointing at it, is swept like any other.
        std::fs::write(alice_dir.join("index.md"), "# Alice\n\n- [[alice/spesa]]\n").unwrap();

        let cronista = FakeLlmBackend::new("fake", "unused — lista path");
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");

        assert_eq!(
            report.orphan_files_swept, 2,
            "the zombie and the leftover index went"
        );
        assert!(
            !alice_dir.join("registro_spesa.md").exists(),
            "the zombie file is gone"
        );
        assert!(
            alice_dir.join("vecchie_note.md").exists(),
            "a live pointer keeps the file"
        );
        assert!(
            alice_dir.join("@rules.md").exists(),
            "reserved names survive"
        );
        assert!(alice_dir.join("spesa.md").exists(), "plan pages survive");
        assert!(
            !alice_dir.join("index.md").exists(),
            "a leftover map file is swept like any other orphan"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn lista_closed_records_carry_the_done_cue_inside_the_marker() {
        // A `lista` record whose fact carries a `decay_reason` renders with a
        // language-free done-cue — `· ✓ <date>` for completed, `· ✗` for
        // retracted/contradicted — INSIDE its region marker, so redaction
        // hides the closure together with the fact. An open record and a
        // window without an explicit closure render with no cue at all.
        let (dir, tree, pool) = setup().await;
        let open = ffp(0x21, "forbici");
        let mut bought = ffp(0x22, "latte");
        bought.decay_reason = Some("completed".to_owned());
        bought.valid_to = Some("2026-06-07T18:30:00Z".to_owned());
        let mut dropped = ffp(0x23, "pannelli per la serra");
        dropped.decay_reason = Some("retracted".to_owned());
        // No valid_to (defensive): the cue degrades to the bare glyph.
        let mut expired = ffp(0x24, "torta per sabato");
        // A past end WITHOUT a decay_reason is expiry, not a closure → no cue.
        expired.valid_to = Some("2026-06-01T00:00:00Z".to_owned());
        for f in [&open, &bought, &dropped, &expired] {
            plant_fact(&pool, &f.fact_id, "user:alice", &f.text).await;
        }

        let mut pages = BTreeMap::new();
        pages.insert(
            "spesa".to_owned(),
            PagePlan {
                slug: "spesa".to_owned(),
                title: "Spesa".to_owned(),
                description: "La lista della spesa".to_owned(),
                style: Some(crate::wiki::PageStyle::Lista),
                primary_facts: vec![open.clone(), bought.clone(), dropped, expired],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "spesa.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["spesa".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 4,
            dirty_pages: vec!["spesa".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        let cronista = FakeLlmBackend::new("fake", "unused — lista path has no LLM");
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md")).unwrap();
        assert!(
            page.contains("latte · ✓ 2026-06-07{{/}}"),
            "completed record: glyph + closure date, inside the marker: {page}"
        );
        assert!(
            page.contains("pannelli per la serra · ✗{{/}}"),
            "retracted record without a valid_to: bare glyph: {page}"
        );
        assert!(
            page.contains("forbici{{/}}"),
            "open record untouched: {page}"
        );
        assert!(
            page.contains("torta per sabato{{/}}"),
            "expiry without an explicit closure gets no cue: {page}"
        );

        // The cue lives only in the render; the canonical claim is untouched.
        let row = fact_index::find_by_id(&pool, &bought.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.text, "latte", "canonical claim text preserved");
        drop(dir);
    }

    /// A list entry is the bare item with its values, and the page shows it
    /// as written.
    ///
    /// Founder, 2026-09-11: *«no frasi, una lista è una lista, ma può avere
    /// valori … dev'essere schematico, è ciò che contraddistingue le liste»*.
    /// So `latte 2` renders as `- latte 2` and nothing dresses it back up into
    /// a sentence; a second value carried on ` · ` survives whole, and a
    /// closure appends its own ` · ✓` after it without colliding with the one
    /// already there.
    #[tokio::test]
    async fn a_list_entry_renders_as_the_bare_item_with_its_values() {
        let (dir, tree, pool) = setup().await;
        let milk = ffp(0x31, "latte 2");
        let water = ffp(0x32, "acqua 2 casse");
        let bread = ffp(0x33, "pane senza glutine");
        let mut yoghurt = ffp(0x34, "yogurt · scade 20/09");
        yoghurt.decay_reason = Some("completed".to_owned());
        yoghurt.valid_to = Some("2026-06-07T18:30:00Z".to_owned());
        for f in [&milk, &water, &bread, &yoghurt] {
            plant_fact(&pool, &f.fact_id, "user:alice", &f.text).await;
        }

        let mut pages = BTreeMap::new();
        pages.insert(
            "spesa".to_owned(),
            PagePlan {
                slug: "spesa".to_owned(),
                title: "Spesa".to_owned(),
                description: "La lista della spesa".to_owned(),
                style: Some(crate::wiki::PageStyle::Lista),
                primary_facts: vec![milk.clone(), water.clone(), bread.clone(), yoghurt.clone()],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "spesa.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["spesa".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 4,
            dirty_pages: vec!["spesa".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        let cronista = FakeLlmBackend::new("fake", "unused — lista path has no LLM");
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md")).unwrap();
        for (fact, rendered) in [
            (&milk, "latte 2"),
            (&water, "acqua 2 casse"),
            (&bread, "pane senza glutine"),
            // The closure appends its own ` · ✓` after the value the entry
            // already carries — two middle dots, one row, nothing merged.
            (&yoghurt, "yogurt · scade 20/09 · ✓ 2026-06-07"),
        ] {
            let line = format!(
                "- {}",
                crate::capture::render_marker(&fact.fact_id, rendered)
            );
            assert!(
                page.contains(&line),
                "the list reads as written: expected `{line}` in:\n{page}"
            );
        }
        drop(dir);
    }

    /// **A style is one of three, and nothing else can be one** (founder,
    /// 2026-08-19). A value that is not one of the three never becomes a style
    /// at all — it is refused, never coerced — and a page with no style
    /// compiles as prose.
    #[test]
    fn a_style_is_one_of_three_or_nothing() {
        use crate::wiki::PageStyle;
        assert_eq!(PageStyle::parse("prosa"), Some(PageStyle::Prosa));
        assert_eq!(
            PageStyle::parse(" Prosa-Tecnica "),
            Some(PageStyle::ProsaTecnica),
            "trimmed and case-folded — the same value written loosely"
        );
        assert_eq!(PageStyle::parse("lista"), Some(PageStyle::Lista));
        assert_eq!(
            PageStyle::parse("bullets"),
            None,
            "not one of the three is NOT a style — it is dropped, never coerced"
        );
        assert_eq!(PageStyle::parse(""), None);
        assert_eq!(PageStyle::parse_lenient(None), None);

        // The wire form round-trips: what is written is what parses back.
        for s in [PageStyle::Prosa, PageStyle::ProsaTecnica, PageStyle::Lista] {
            assert_eq!(PageStyle::parse(s.as_str()), Some(s));
        }

        // A page nobody described compiles as prose.
        assert_eq!(style_or_default(None), PageStyle::Prosa);
        assert_eq!(style_or_default(Some(PageStyle::Lista)), PageStyle::Lista);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // inline 2-fact page fixture reads top-to-bottom
    async fn cronista_omission_recovered_by_forward_completeness_guard() {
        let (dir, tree, pool) = setup().await;
        let fid1 = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d01").unwrap();
        let fid2 = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d02").unwrap();
        // Both facts are promoted with no offsets — pending renders. fid2 is a
        // non-global (group) fact — exactly the `missing_acl_markers` case.
        plant_fact(&pool, &fid1, "user:alice", "Alice loves pasta").await;
        plant_fact(
            &pool,
            &fid2,
            "group:famiglia",
            "Matteo has homework on Monday",
        )
        .await;

        // The Cronista tags fid1's span (`<f1>`) ONLY — it never tags fid2.
        let body =
            "{\"mergedBody\":\"A proposito di pasta. <f1>Alice ama la pasta.</f1>\",\"description\":\"d\"}"
                .to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);

        // Plan: both facts assigned to alice's leaf page.
        let mut pages = BTreeMap::new();
        pages.insert(
            "alice".to_owned(),
            PagePlan {
                slug: "alice".to_owned(),
                title: "Alice".to_owned(),
                description: "Alice".to_owned(),
                style: None,
                primary_facts: vec![
                    FactForPage {
                        topics: Vec::new(),
                        subject_external: None,
                        authored_refs: Vec::new(),
                        fact_id: fid1.clone(),
                        text: "Alice loves pasta".to_owned(),
                        fact_type: Some("preference".to_owned()),
                        subject: "user:alice".parse::<Principal>().unwrap(),
                        allow: Vec::new(),
                        sender: None,
                        source_wiki_id: "alice".to_owned(),
                        valid_from: None,
                        valid_to: None,
                        decay_reason: None,
                        successor_fact_id: None,
                        target_page: None,
                        style: None,
                        salience: None,
                    },
                    FactForPage {
                        topics: Vec::new(),
                        subject_external: None,
                        authored_refs: Vec::new(),
                        fact_id: fid2.clone(),
                        text: "Matteo has homework on Monday".to_owned(),
                        fact_type: Some("plan".to_owned()),
                        subject: "group:famiglia".parse::<Principal>().unwrap(),
                        allow: Vec::new(),
                        sender: None,
                        source_wiki_id: "alice".to_owned(),
                        valid_from: None,
                        valid_to: None,
                        decay_reason: None,
                        successor_fact_id: None,
                        target_page: None,
                        style: None,
                        salience: None,
                    },
                ],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "cucina.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["alice".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 2,
            dirty_pages: vec!["alice".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-01T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(report.leaves, 1);

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        // The fact the Cronista DID emit.
        assert!(page.contains(&format!("f={fid1}")), "emitted fact present");
        // The OMITTED non-global fact was recovered by the forward guard,
        // wrapped in its protective region marker (no silent drop — the
        // ACL gates from the DB by that key, so the bare marker is the
        // whole invariant).
        assert!(
            page.contains(&format!("f={fid2}")),
            "omitted fact appended by the forward completeness guard"
        );
        assert!(
            !page.contains("subject=") && !page.contains("owner="),
            "bare runtime markers — no inline ACL on disk"
        );
        assert!(
            page.contains("Matteo has homework on Monday"),
            "appended fact body present"
        );
        // Both facts repointed onto the compiled page.
        for fid in [&fid1, &fid2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(
                row.source_path, "wikis/alice/cucina.md",
                "fact {fid} repointed onto the compiled page"
            );
            assert!(row.region_start.is_some(), "offsets repointed for {fid}");
        }
        drop(dir);
    }

    fn concept_leaf_plan(f: FactForPage, slug: &str, style: Option<&str>) -> CompilationPlan {
        let mut pages = BTreeMap::new();
        pages.insert(
            slug.to_owned(),
            PagePlan {
                slug: slug.to_owned(),
                title: slug.to_owned(),
                description: "d".to_owned(),
                style: crate::wiki::PageStyle::parse_lenient(style),
                primary_facts: vec![f],
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: format!("{slug}.md"),
            },
        );
        CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec![slug.to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 1,
            dirty_pages: vec![slug.to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        }
    }

    #[tokio::test]
    async fn plan_move_prepoints_the_row_before_the_source_rewrite_can_strand_it() {
        // The dogfood loss: a plan reassigns a fact from page A
        // to page B, A is rewritten without the marker while the row still
        // points at A, and the orphan sweep tombstones the live fact. The
        // compiler must repoint the row DB-first — BEFORE any page write.
        // With the degraded mode, a destination whose Cronista fails (twice)
        // now ends in the guard-only append: the pre-pointed row is stamped
        // onto the appended region — never a tombstone either way.
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x21, "Matteo plays karate on Mondays");
        // The row lives on the OLD page (a prior compile), with real offsets.
        plant_fact_at(
            &pool,
            &f.fact_id,
            "user:alice",
            "Matteo plays karate on Mondays",
            "wikis/alice/old.md",
            Some(10),
            Some(80),
        )
        .await;

        // The plan reassigns the fact to a new page whose Cronista FAILS
        // (the fake returns the same unparseable reply on the retry too).
        let plan = concept_leaf_plan(f.clone(), "karate", None);
        let cronista = FakeLlmBackend::new("fake", "NOT JSON");
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");
        assert!(
            report.errors.is_empty(),
            "no hard failure: {:?}",
            report.errors
        );
        assert_eq!(
            report.degraded.len(),
            1,
            "destination page degraded to the guard-only append: {:?}",
            report.degraded
        );

        // Pre-pointed DB-first onto the destination, then stamped by the
        // degraded append — NOT left stranded on old.md.
        let row = fact_index::find_by_id(&pool, &f.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/karate.md");
        assert!(
            row.region_start.is_some(),
            "the degraded append put the marker on disk and stamped offsets"
        );
        assert!(row.deleted_at.is_none(), "the fact survives");

        // The loss chain is closed: the old page's sweep (path-guarded)
        // can no longer read the rewrite as a forget gesture.
        let touched = fact_index::mark_forgotten_at(
            &pool,
            &f.fact_id,
            "wikis/alice/old.md",
            crate::reindex::REASON_MARKER_REMOVED,
        )
        .await
        .unwrap();
        assert_eq!(touched, 0, "a moved row is not an orphan of the old page");
        drop(dir);
    }

    #[tokio::test]
    async fn plan_move_lands_offsets_when_the_destination_compiles() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x22, "Matteo plays karate on Mondays");
        plant_fact_at(
            &pool,
            &f.fact_id,
            "user:alice",
            "Matteo plays karate on Mondays",
            "wikis/alice/old.md",
            Some(10),
            Some(80),
        )
        .await;

        let plan = concept_leaf_plan(f.clone(), "karate", None);
        let body =
            "{\"mergedBody\":\"<f1>Matteo fa karate il lunedì.</f1>\",\"description\":\"d\"}";
        let cronista = FakeLlmBackend::new("fake", body);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(report.leaves, 1);

        let row = fact_index::find_by_id(&pool, &f.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/karate.md");
        assert!(
            row.region_start.is_some(),
            "destination compile stamped the real offsets"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn compile_moves_wiki_id_to_the_page_wiki_not_just_source_path() {
        // Invariant: a fact homed in wiki `alice` that the plan renders onto a
        // page in wiki `bob` ends with wiki_id == "bob" — `repoint_facts` uses
        // `move_to_wiki`, so `wiki_id` always names the wiki whose page carries
        // the region (no wiki_id/source_path divergence).
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        for w in ["alice", "bob"] {
            std::fs::create_dir_all(wikis.join(w)).unwrap();
            std::fs::write(
                wikis.join(format!("{w}/_meta.md")),
                format!(
                    "---\nwiki_id: {w}\nwiki_type: wiki-user\nslug: {w}\ntitle: {w}\nacl_default: 'user:{w}'\n---\n"
                ),
            )
            .unwrap();
            std::fs::write(wikis.join(format!("{w}/cucina.md")), format!("# {w}\n")).unwrap();
        }
        let tree = WikiTree::open(dir.path()).expect("tree");

        let f = ffp(0x42, "Matteo fa karate il lunedì");
        plant_fact_at(
            &pool,
            &f.fact_id,
            "user:alice",
            "Matteo fa karate il lunedì",
            "wikis/alice/old.md",
            Some(10),
            Some(60),
        )
        .await;
        assert_eq!(
            fact_index::find_by_id(&pool, &f.fact_id)
                .await
                .unwrap()
                .unwrap()
                .wiki_id,
            "alice"
        );

        // Same leaf plan, but the destination page lives in wiki `bob`.
        let mut plan = concept_leaf_plan(f.clone(), "karate", None);
        plan.pages.get_mut("karate").unwrap().wiki_id = "bob".to_owned();

        let body =
            "{\"mergedBody\":\"<f1>Matteo fa karate il lunedì.</f1>\",\"description\":\"d\"}";
        let cronista = FakeLlmBackend::new("fake", body);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile");

        let row = fact_index::find_by_id(&pool, &f.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.source_path, "wikis/bob/karate.md",
            "rendered onto bob's page"
        );
        assert_eq!(row.wiki_id, "bob", "wiki_id followed the page's wiki");
        drop(dir);
    }

    #[tokio::test]
    async fn unchanged_page_still_repoints_pending_renders() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x23, "latte");
        // Journal row, NULL offsets (fresh promote).
        plant_fact(&pool, &f.fact_id, "user:alice", "latte").await;
        let plan = concept_leaf_plan(f.clone(), "spesa", Some("lista"));
        let cronista = FakeLlmBackend::new("fake", "unused — lista path has no LLM");

        // First compile writes the record page and stamps offsets.
        let r1 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile 1");
        assert_eq!(r1.lists, 1);

        // Knock the row back to a pending render elsewhere (the state a
        // pre-point leaves when the destination content already sits on disk).
        fact_index::move_region(&pool, &f.fact_id, "wikis/alice/appunti_vari.md", None, None)
            .await
            .unwrap();

        // The identical plan renders byte-identical content → Unchanged, but
        // the repoint must still stamp the offsets.
        let r2 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("compile 2");
        assert_eq!(r2.unchanged, 1);
        let row = fact_index::find_by_id(&pool, &f.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/spesa.md");
        assert!(
            row.region_start.is_some(),
            "an unchanged page still stamps offsets on its pending renders"
        );
        drop(dir);
    }

    #[test]
    fn primary_facts_text_is_numbered() {
        let f = ffp(1, "Alice runs daily");
        let txt = primary_facts_text(
            std::slice::from_ref(&f),
            "2026-06-08T12:00:00Z",
            &|_| true,
            &|_| None,
        );
        assert!(
            txt.contains("1. [BIO] Alice runs daily"),
            "facts are presented numbered + typed: {txt}"
        );
    }

    #[test]
    fn bundled_cronista_prompt_carries_the_identity_card_guard() {
        // The belt-guard behind the planner's identity-page discipline: when
        // writing a user's identity card, another subject is NAMED and left
        // there, never woven into the prose with their details. One sentence
        // by design — plan placement is the load-bearing channel (the
        // Cronista only ever sees the facts the plan gave the page).
        assert!(
            BUNDLED_CRONISTA_MD.contains("never weave ANOTHER subject's detail"),
            "identity-index reference-distance guard present"
        );
        assert!(
            BUNDLED_CRONISTA_MD.contains("the page carries one subject"),
            "one-subject framing present"
        );
    }

    #[test]
    fn primary_facts_text_withholds_acl_from_the_cronista() {
        // Under the <fN> contract the Cronista does not write markers, so the
        // ACL (subject / allow / sender / fact_id) is deliberately NOT shown to it —
        // the code renders the marker (expand_fact_tags). Withholding it is what
        // removes the brace/attribute miscount failure mode of LLM-written markers.
        let mut f = ffp(2, "Frodo works only the afternoon tomorrow");
        f.subject = "user:frodo".parse::<Principal>().unwrap();
        f.allow = vec!["group:famiglia".parse::<Principal>().unwrap()];
        f.sender = Some("user:galadriel".parse::<Principal>().unwrap());
        let txt = primary_facts_text(
            std::slice::from_ref(&f),
            "2026-06-08T12:00:00Z",
            &|_| true,
            &|_| None,
        );
        assert!(
            !txt.contains("subject=") && !txt.contains("owner="),
            "subject must NOT reach the prompt: {txt}"
        );
        assert!(
            !txt.contains("allow="),
            "allow must NOT reach the prompt: {txt}"
        );
        assert!(
            !txt.contains("sender="),
            "sender must NOT reach the prompt: {txt}"
        );
        assert!(
            !txt.contains("f="),
            "fact_id must NOT reach the prompt: {txt}"
        );
    }

    #[test]
    fn primary_facts_text_appends_validity_hint_only_when_present() {
        // A fact with a validity window gets a `(validity: …)` hint the
        // Cronista phrases into prose; a durable fact (both bounds None) gets none.
        // NOTE: today every narrative-compiled fact has NULL validity (the
        // buffer→promote path drops it), so this path is exercised at the unit
        // level — it lights up end-to-end once that gap is threaded.
        let now = "2026-06-08T12:00:00Z";
        let mut closed = ffp(1, "dentist appointment");
        closed.valid_from = Some("2026-06-07T17:00:00Z".to_owned());
        closed.valid_to = Some("2026-06-07T18:00:00Z".to_owned());

        let mut horizon = ffp(2, "milan trip this week");
        horizon.valid_to = Some("2026-06-13T00:00:00Z".to_owned());

        let durable = ffp(3, "lives in Lisbon");

        // open-ended with a PAST/record `valid_from` (the day we learned it, not
        // an onset) → the date is suppressed, never narrated as "since <date>".
        let mut recorded = ffp(4, "known as Smeagol");
        recorded.valid_from = Some("2026-06-08T00:00:00Z".to_owned());

        // open-ended with a FUTURE `valid_from` (an announced onset) → date kept.
        let mut future = ffp(5, "moves office from Monday");
        future.valid_from = Some("2026-06-15T00:00:00Z".to_owned());

        let txt = primary_facts_text(
            &[closed, horizon, durable, recorded, future],
            now,
            &|_| true,
            &|_| None,
        );
        assert!(
            txt.contains("(validity: from 2026-06-07T17:00:00Z until 2026-06-07T18:00:00Z)"),
            "closed window renders both bounds: {txt}"
        );
        assert!(
            txt.contains("(validity: until 2026-06-13T00:00:00Z)"),
            "open-start horizon renders the end bound: {txt}"
        );
        assert!(
            txt.contains("3. [BIO] lives in Lisbon (audience: alice)\n"),
            "a durable fact carries NO validity suffix (audience hint aside): {txt}"
        );
        assert!(
            txt.contains("4. [BIO] known as Smeagol (audience: alice) (validity: open-ended)"),
            "a past/record open-ended start is dateless — no false onset: {txt}"
        );
        assert!(
            !txt.contains("known as Smeagol (validity: from"),
            "the record date must NOT be narrated as an onset: {txt}"
        );
        assert!(
            txt.contains(
                "5. [BIO] moves office from Monday (audience: alice) (validity: from 2026-06-15T00:00:00Z, open-ended)"
            ),
            "a FUTURE open-ended start keeps its dated onset: {txt}"
        );
    }

    #[test]
    fn validity_hint_covers_all_four_bound_combinations() {
        let now = "2026-06-08T12:00:00Z";
        assert_eq!(validity_hint(None, None, None, now), "");
        assert_eq!(
            validity_hint(
                Some("2026-06-06T00:00:00Z"),
                Some("2026-06-11T00:00:00Z"),
                None,
                now
            ),
            " (validity: from 2026-06-06T00:00:00Z until 2026-06-11T00:00:00Z)"
        );
        // open-ended + past start (record date) → dateless, no false onset.
        assert_eq!(
            validity_hint(Some("2026-06-06T00:00:00Z"), None, None, now),
            " (validity: open-ended)"
        );
        // open-ended + future start (announced onset) → keeps the dated form.
        assert_eq!(
            validity_hint(Some("2026-06-15T00:00:00Z"), None, None, now),
            " (validity: from 2026-06-15T00:00:00Z, open-ended)"
        );
        assert_eq!(
            validity_hint(None, Some("2026-06-11T00:00:00Z"), None, now),
            " (validity: until 2026-06-11T00:00:00Z)"
        );
    }

    #[test]
    fn validity_hint_appends_the_decay_reason_on_closed_windows() {
        // A closed window carrying its WHY shows it inside the envelope, so
        // the Cronista can phrase "bought"/"abandoned" instead of a generic
        // "until"; an open window never shows a reason (nothing closed it).
        let now = "2026-06-08T12:00:00Z";
        assert_eq!(
            validity_hint(
                Some("2026-06-06T00:00:00Z"),
                Some("2026-06-07T00:00:00Z"),
                Some("completed"),
                now
            ),
            " (validity: from 2026-06-06T00:00:00Z until 2026-06-07T00:00:00Z, closed: completed)"
        );
        assert_eq!(
            validity_hint(None, Some("2026-06-07T00:00:00Z"), Some("retracted"), now),
            " (validity: until 2026-06-07T00:00:00Z, closed: retracted)"
        );
        // Open-ended shapes ignore a (nonsensical) reason.
        assert_eq!(
            validity_hint(Some("2026-06-06T00:00:00Z"), None, Some("completed"), now),
            " (validity: open-ended)"
        );
    }

    /// The index carries descriptions and never another page's facts —
    /// that starvation is the whole mechanism. It also carries the
    /// page being written: one string per run is what makes the system
    /// half of the Cronista prompt a cacheable prefix, and the body pays
    /// for it with an explicit never-link-to-itself rule.
    /// Fixture: `n` leaf pages in one wiki, plus one in another.
    fn selection_plan() -> CompilationPlan {
        let mut pages = BTreeMap::new();
        for (slug, wiki) in [
            ("cucina", "alice"),
            ("orto", "alice"),
            ("auto", "alice"),
            ("garage", "bob"),
        ] {
            pages.insert(
                slug.to_owned(),
                PagePlan {
                    slug: slug.to_owned(),
                    title: slug.to_owned(),
                    description: format!("{slug} desc"),
                    style: None,
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: wiki.to_owned(),
                    page_path: format!("{slug}.md"),
                },
            );
        }
        CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec![
                "auto".to_owned(),
                "cucina".to_owned(),
                "garage".to_owned(),
                "orto".to_owned(),
            ],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        }
    }

    /// Below the ceiling the whole index rides the cacheable half and the
    /// task half stays empty — the prefix a run reuses must not move.
    #[test]
    fn the_whole_index_leaves_the_task_half_empty() {
        let plan = selection_plan();
        let page = plan.pages.get("cucina").expect("page");
        let (cached, task) = PageIndex::Whole(page_index_block(&plan)).render_for(&plan, page);
        assert!(cached.contains("[[alice/orto]]: orto desc"));
        assert!(
            task.is_empty(),
            "nothing moves to the per-page half while the whole index is cached"
        );
    }

    /// A candidate pool over [`selection_plan`], keyed the way the real
    /// [`build_page_index`] keys one.
    async fn selection_pool(
        pool: &SqlitePool,
        plan: &CompilationPlan,
    ) -> crate::candidates::CandidatePool {
        let by_source_path: BTreeMap<String, String> = plan
            .pages
            .values()
            .map(|p| {
                (
                    format!("wikis/{}/{}", p.wiki_id, p.page_path),
                    p.slug.clone(),
                )
            })
            .collect();
        crate::candidates::CandidatePool::load(pool, &by_source_path).await
    }

    /// Above the ceiling the lines move to the task half — leaving them in the
    /// cacheable one would write a cache entry per page and read none — and
    /// each one says why it is offered.
    #[tokio::test]
    async fn the_card_selection_moves_to_the_task_half_and_says_why() {
        let (dir, _tree, pool) = setup().await;
        let plan = selection_plan();
        let mut candidates = selection_pool(&pool, &plan).await;
        candidates.set_embedding("cucina", vec![1.0, 0.0, 0.0]);
        // `orto` points nearly the same way as `cucina`; `auto` is orthogonal;
        // `garage` points away.
        candidates.set_embedding("orto", vec![0.9, 0.1, 0.0]);
        candidates.set_embedding("auto", vec![0.0, 1.0, 0.0]);
        candidates.set_embedding("garage", vec![-1.0, 0.0, 0.0]);
        let index = PageIndex::Selected(candidates);
        let page = plan.pages.get("cucina").expect("page");
        let (cached, task) = index.render_for(&plan, page);

        assert!(
            !cached.contains("[[alice/orto]]"),
            "no page line may stay in the cacheable half: {cached}"
        );
        let orto = task.find("[[alice/orto]]").expect("nearest page offered");
        let auto = task
            .find("[[alice/auto]]")
            .expect("orthogonal page offered");
        let garage = task.find("[[bob/garage]]").expect("far page offered");
        assert!(
            orto < auto && auto < garage,
            "the near source lists what it picked nearest first: {task}"
        );
        assert!(
            task.contains("- [near] [[alice/orto]]"),
            "every line names the source that chose it: {task}"
        );
        assert!(
            !task.contains("[[alice/cucina]]"),
            "a page is never offered itself"
        );
        drop(dir);
    }

    /// The two sources that need no vector at all — and they are the reason
    /// the selection is not a similarity ranking: neither page here resembles
    /// `cucina`, and without them neither could ever be linked from it.
    #[tokio::test]
    async fn a_page_is_offered_for_its_people_and_its_turn_not_only_its_words() {
        let (dir, _tree, pool) = setup().await;
        // A memory big enough that nearness alone fills its quota: the two
        // pages below must arrive on their OWN source or not at all.
        let mut plan = selection_plan();
        for i in 0..14 {
            let slug = format!("vicina{i:02}");
            plan.pages.insert(
                slug.clone(),
                PagePlan {
                    title: slug.clone(),
                    description: format!("{slug} desc"),
                    style: None,
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: "alice".to_owned(),
                    page_path: format!("{slug}.md"),
                    slug,
                },
            );
        }
        // `cucina` and `auto` were said by the same person; `cucina` and
        // `garage` were said in the same turn. `orto` shares neither and is
        // the nearest page by card.
        plant_fact_at(
            &pool,
            &ffp(0x51, "x").fact_id,
            "user:bob",
            "bob cooks",
            "wikis/alice/cucina.md",
            None,
            None,
        )
        .await;
        plant_fact_at(
            &pool,
            &ffp(0x52, "x").fact_id,
            "user:bob",
            "bob drives",
            "wikis/alice/auto.md",
            None,
            None,
        )
        .await;
        plant_fact_at(
            &pool,
            &ffp(0x53, "x").fact_id,
            "user:carol",
            "carol parks",
            "wikis/bob/garage.md",
            None,
            None,
        )
        .await;
        plant_fact_at(
            &pool,
            &ffp(0x54, "x").fact_id,
            "user:dave",
            "dave digs",
            "wikis/alice/orto.md",
            None,
            None,
        )
        .await;
        // One turn produced `cucina`'s fact and `garage`'s.
        for id in [&ffp(0x51, "x").fact_id, &ffp(0x53, "x").fact_id] {
            sqlx::query(
                "INSERT INTO capture_buffer
                   (capture_id, body, subject_id, status, captured_at, origin_message_hash)
                 VALUES (?, 'x', 'user:alice', 'promoted', '2026-08-23T00:00:00Z', 'turn-1')",
            )
            .bind(id.as_str())
            .execute(&pool)
            .await
            .expect("buffer row");
        }

        let mut candidates = selection_pool(&pool, &plan).await;
        candidates.set_embedding("cucina", vec![1.0, 0.0, 0.0]);
        candidates.set_embedding("orto", vec![0.99, 0.01, 0.0]);
        // The two that must arrive on their own source point away from
        // `cucina`, so nearness will never reach them.
        candidates.set_embedding("auto", vec![0.0, 1.0, 0.0]);
        candidates.set_embedding("garage", vec![-1.0, 0.0, 0.0]);
        for i in 0_u8..14 {
            let t = f32::from(i) / 100.0;
            candidates.set_embedding(&format!("vicina{i:02}"), vec![1.0 - t, t, 0.0]);
        }
        let page = plan.pages.get("cucina").expect("page");
        let (_, task) = PageIndex::Selected(candidates).render_for(&plan, page);

        assert!(
            task.contains("- [same-people] [[alice/auto]]"),
            "the page about the same person is offered as such: {task}"
        );
        assert!(
            task.contains("- [same-turn] [[bob/garage]]"),
            "the page from the same conversation is offered as such: {task}"
        );
        drop(dir);
    }

    /// A page the table has no vector for — new this run, no description, an
    /// embedder that failed — is topped up from its own wiki's pages, which is
    /// where its links most often go and costs no arithmetic.
    #[tokio::test]
    async fn a_page_with_no_card_vector_falls_back_to_its_own_wiki() {
        let (dir, _tree, pool) = setup().await;
        let plan = selection_plan();
        let index = PageIndex::Selected(selection_pool(&pool, &plan).await);
        let page = plan.pages.get("cucina").expect("page");
        let (_, task) = index.render_for(&plan, page);
        assert!(
            task.contains("- [same-wiki] [[alice/orto]]"),
            "same wiki is offered, and says so: {task}"
        );
        assert!(task.contains("[[alice/auto]]"));
        assert!(
            !task.contains("[[bob/garage]]"),
            "another wiki's page is not the fallback neighbourhood"
        );
        drop(dir);
    }

    #[test]
    fn page_index_includes_self_and_shows_only_descriptions() {
        let mut pages = BTreeMap::new();
        for s in ["alice", "bob"] {
            pages.insert(
                s.to_owned(),
                PagePlan {
                    slug: s.to_owned(),
                    title: s.to_owned(),
                    description: format!("{s} desc"),
                    style: None,
                    primary_facts: vec![ffp(2, "secret bob fact")],
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: s.to_owned(),
                    page_path: "hobbies.md".to_owned(),
                },
            );
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["alice".to_owned(), "bob".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let idx = page_index_block(&plan);
        assert!(
            idx.contains("[[bob/hobbies]]: bob desc"),
            "shows other page description"
        );
        assert!(
            idx.contains("[[alice/hobbies]]: alice desc"),
            "includes the page being written — the block is one per run"
        );
        assert!(
            !idx.contains("secret bob fact"),
            "NEVER another page's facts"
        );
    }

    /// The card of somebody this turn is not about reaches the reader by no
    /// other route, so the index offers it and the prose decides. What a page
    /// may never be REQUIRED to point at is a separate fence, and it lives in
    /// [`recommended_link_targets`].
    #[test]
    fn the_page_index_offers_an_identity_card_like_any_other_page() {
        let mut pages = BTreeMap::new();
        for (slug, path) in [
            ("alice", "hobbies.md"),
            ("bob", crate::wiki::PROFILE_FILENAME),
        ] {
            pages.insert(
                slug.to_owned(),
                PagePlan {
                    slug: slug.to_owned(),
                    title: slug.to_owned(),
                    description: format!("{slug} desc"),
                    style: None,
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    pending_links: Vec::new(),
                    wiki_id: slug.to_owned(),
                    page_path: path.to_owned(),
                },
            );
        }
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["alice".to_owned(), "bob".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let idx = page_index_block(&plan);
        assert!(
            idx.contains("[[alice/hobbies]]"),
            "an ordinary page is offered"
        );
        assert!(
            idx.contains("[[bob/@profile]]"),
            "and so is a card — choosing it is the prose's call: {idx}"
        );
    }

    /// The compile emitters only feed the prose-writing prompts canonical
    /// links (`[[wiki_id]]` / `[[wiki_id/page-slug]]`) — a bare plan slug
    /// would read as a wiki hop to a wiki that does not exist (a dead rail
    /// for the recall navigator and the dashboard click-through).
    #[test]
    fn plan_links_are_canonical_wiki_qualified_forms() {
        let leaf = |slug: &str, wiki_id: &str, page_path: &str| PagePlan {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: format!("{slug} desc"),
            style: None,
            primary_facts: Vec::new(),
            outgoing_links: Vec::new(),
            pending_links: Vec::new(),
            wiki_id: wiki_id.to_owned(),
            page_path: page_path.to_owned(),
        };
        // A leaf page links as `[[wiki_id/stem]]` …
        assert_eq!(
            plan_page_wikilink(&leaf("ricette_freezer", "morgana", "ricette_freezer.md")),
            "[[morgana/ricette_freezer]]"
        );
        // … even when the page lives in a sub-wiki whose id differs from
        // the plan slug (the underscored-mutant class this kills).
        assert_eq!(
            plan_page_wikilink(&leaf(
                "referto_oculistica",
                "famiglia-carol",
                "referto_oculistica.md"
            )),
            "[[famiglia-carol/referto_oculistica]]"
        );
        // The starvation index and the recommended links both ride the
        // same helper.
        let mut pages = BTreeMap::new();
        pages.insert("hub".to_owned(), leaf("hub", "famiglia", "ricette.md"));
        pages.insert(
            "salute_carol".to_owned(),
            leaf("salute_carol", "famiglia-carol", "salute_carol.md"),
        );
        let mut link_graph = BTreeMap::new();
        link_graph.insert(
            "hub".to_owned(),
            vec!["salute_carol".to_owned(), "vanished".to_owned()],
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: vec!["hub".to_owned(), "salute_carol".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };
        let idx = page_index_block(&plan);
        assert!(
            idx.contains("- [[famiglia-carol/salute_carol]]: salute_carol desc"),
            "{idx}"
        );
        let targets = recommended_link_targets(&plan, "hub");
        assert_eq!(
            recommended_links(&targets),
            "[[famiglia-carol/salute_carol]]",
            "graph slugs resolve through the plan; a vanished slug is skipped"
        );
    }

    // ---------- degraded mode + failure surfacing ----------

    use crate::llm::{CompletionResponse, CompletionUsage, FinishReason, LlmError};

    /// A Cronista whose backend refuses the request outright, counting
    /// attempts. Models the live failure — the API answering "credit balance
    /// too low" (a 400 ⇒ [`LlmError::Invalid`]) — where a retry buys the same
    /// refusal a second time and the count is what proves it does not.
    struct RefusingCronista {
        error: fn(String) -> LlmError,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RefusingCronista {
        const fn new(error: fn(String) -> LlmError) -> Self {
            Self {
                error,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for RefusingCronista {
        fn model_id(&self) -> &'static str {
            "refusing-cronista"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::llm::Result<CompletionResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err((self.error)("HTTP 400: credit balance too low".to_owned()))
        }
    }

    /// A rejected request is not flakiness: retrying buys the same refusal
    /// at the same price. One attempt, then straight to the degraded page.
    #[tokio::test]
    async fn cronista_does_not_retry_a_rejected_request() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5daa").unwrap();
        plant_degraded_fact(&pool, &fid).await;
        let plan = leaf_plan(&fid);

        for make in [
            LlmError::Invalid as fn(String) -> LlmError,
            LlmError::Auth as fn(String) -> LlmError,
        ] {
            let cronista = RefusingCronista::new(make);
            let report = compile_dirty_pages(
                &pool,
                &tree,
                &plan,
                &cronista,
                Cadence::Light,
                "2026-05-31T00:00:00Z",
            )
            .await
            .expect("compile");
            assert_eq!(
                cronista.calls(),
                1,
                "a permanent rejection must cost exactly one call, not two"
            );
            assert_eq!(
                report.degraded.len(),
                1,
                "the page still degrades, not freezes"
            );
            assert!(
                report.degraded[0].contains("not retryable"),
                "the report names why it did not retry: {:?}",
                report.degraded[0]
            );
        }
        drop(dir);
    }

    /// The other half of the contract: a flaky transport IS worth one more
    /// try, so the retry ladder must stay in place for it.
    #[tokio::test]
    async fn cronista_still_retries_a_transport_failure() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dbb").unwrap();
        plant_degraded_fact(&pool, &fid).await;
        let cronista = RefusingCronista::new(LlmError::Transport as fn(String) -> LlmError);
        let plan = leaf_plan(&fid);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.calls(), 2, "transport flakiness earns one retry");
        drop(dir);
    }

    /// Minimal fact for the failure-path tests: the page needs one fact to
    /// reach the Cronista at all (a fact-less leaf renders without an LLM).
    async fn plant_degraded_fact(pool: &SqlitePool, fid: &FactId) {
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                subject_external: None,
                slot: None,
                slot_value: None,
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    /// Scripted Cronista: pops one `Result<reply, transport-error>` per
    /// `complete` call. Panics when over-called, so a test pins the exact
    /// number of attempts (first call + at most one retry per page).
    struct ScriptedCronista(
        std::sync::Mutex<std::collections::VecDeque<std::result::Result<String, String>>>,
    );

    impl ScriptedCronista {
        fn new(script: Vec<std::result::Result<&str, &str>>) -> Self {
            Self(std::sync::Mutex::new(
                script
                    .into_iter()
                    .map(|r| r.map(str::to_owned).map_err(str::to_owned))
                    .collect(),
            ))
        }

        fn remaining(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for ScriptedCronista {
        fn model_id(&self) -> &'static str {
            "scripted-cronista"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::llm::Result<CompletionResponse> {
            let next = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedCronista over-called: script exhausted");
            match next {
                Ok(text) => Ok(CompletionResponse {
                    text,
                    finish_reason: FinishReason::EndOfTurn,
                    usage: CompletionUsage::default(),
                }),
                Err(msg) => Err(LlmError::Transport(msg)),
            }
        }
    }

    const GOOD_CRONISTA: &str =
        "{\"mergedBody\":\"Su Alice. <f1>Alice ama la pasta.</f1>\",\"description\":\"d\"}";

    // ---------- the rail guard ----------

    /// A one-fact leaf the plan gives one rail to, pointing at a neighbour
    /// that exists in the plan but is NOT dirty (so the scripted Cronista is
    /// called for `cucina` alone and every call in a test's script is
    /// accounted for).
    fn leaf_plan_with_rail(f: FactForPage, slug: &str, neighbour: &str) -> CompilationPlan {
        let mut plan = concept_leaf_plan(f, slug, None);
        plan.pages.insert(
            neighbour.to_owned(),
            PagePlan {
                slug: neighbour.to_owned(),
                title: neighbour.to_owned(),
                description: "il vicino".to_owned(),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: format!("{neighbour}.md"),
            },
        );
        plan.link_graph
            .insert(slug.to_owned(), vec![neighbour.to_owned()]);
        plan.compilation_order.push(neighbour.to_owned());
        plan
    }

    /// The guard reads the PROSE, not the plan — the whole point of the
    /// defect it closes. A rail counts as landed when the page text carries
    /// it, in any form the link grammar allows: the `|display` alias is
    /// presentation, and a `.md` suffix addresses the same page.
    /// A card is offered, never required.
    ///
    /// Recall serves the speaker's own card before it has read the turn, and
    /// the cards of whoever the turn names besides — so a rail to one buys a
    /// door into a room the reader is already in. It is not free either: a
    /// mandatory link the narrative has no place for is a sentence the page
    /// grows to host it, and that sentence carries the link, which is how the
    /// next build harvests it and requires it again.
    ///
    /// A card's own rails are a different question and stay: this filters
    /// what a page must point AT.
    #[test]
    fn an_identity_card_is_never_a_mandatory_rail() {
        let card = |slug: &str| planner::PagePlan {
            title: slug.to_owned(),
            description: String::new(),
            style: None,
            primary_facts: Vec::new(),
            outgoing_links: Vec::new(),
            pending_links: Vec::new(),
            wiki_id: slug.to_owned(),
            page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
            slug: slug.to_owned(),
        };
        let mut leaf = card("impegni_franz");
        leaf.page_path = "impegni_franz.md".to_owned();
        leaf.wiki_id = "franz".to_owned();
        leaf.outgoing_links = vec!["franz".to_owned(), "cucina_franz".to_owned()];
        let mut topic = card("cucina_franz");
        topic.page_path = "cucina_franz.md".to_owned();
        topic.wiki_id = "franz".to_owned();

        let mut pages = std::collections::BTreeMap::new();
        for p in [leaf, topic, card("franz")] {
            pages.insert(p.slug.clone(), p);
        }
        let mut link_graph = std::collections::BTreeMap::new();
        link_graph.insert(
            "impegni_franz".to_owned(),
            vec!["cucina_franz".to_owned(), "franz".to_owned()],
        );
        let plan = planner::CompilationPlan {
            pages,
            link_graph,
            merged_pages: Vec::new(),
            compilation_order: vec!["impegni_franz".to_owned()],
            generated_at: "2026-08-26T00:00:00Z".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        assert_eq!(
            recommended_link_targets(&plan, "impegni_franz"),
            vec!["[[franz/cucina_franz]]".to_owned()],
            "the topic page is required, the card is not"
        );
    }

    #[test]
    fn missing_rails_reads_the_prose_and_normalises_the_address() {
        let recommended = vec![
            "[[alice/spesa]]".to_owned(),
            "[[alice/cucina]]".to_owned(),
            "[[bob/hobbies]]".to_owned(),
        ];
        let body = "Fa la [[alice/spesa|spesa]] il sabato, e cucina in [[alice/cucina.md]].";
        assert_eq!(
            missing_rails(&recommended, body),
            vec!["[[bob/hobbies]]".to_owned()],
            "an aliased link and a .md-suffixed one both land; only bob's is missing"
        );
        // A bare wiki link names a MAP, which no reader may open, so it can
        // never stand in for the rail to a page of that wiki.
        assert_eq!(
            missing_rails(&["[[bob/hobbies]]".to_owned()], "Ne parla con [[bob]]."),
            vec!["[[bob/hobbies]]".to_owned()],
        );
        assert!(missing_rails(&[], "nessun binario").is_empty());
    }

    /// A page whose prose already carries every recommended rail costs ONE
    /// call: the guard is free unless something is actually missing.
    #[tokio::test]
    async fn a_page_that_carries_its_rails_costs_no_extra_call() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x51, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = leaf_plan_with_rail(f, "cucina", "spesa");

        let cronista = ScriptedCronista::new(vec![Ok(
            "{\"mergedBody\":\"<f1>Alice ama la pasta</f1>, che compra in [[alice/spesa]].\",\
              \"description\":\"d\"}",
        )]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.remaining(), 0, "exactly one call");
        assert_eq!(report.leaves, 1);
        assert!(report.rails_appended.is_empty(), "nothing to append");
        drop(dir);
    }

    /// A dropped rail buys ONE rewrite, and a rewrite that carries it wins:
    /// the rail ends up woven into the prose, not appended, and the page has
    /// nothing to report.
    #[tokio::test]
    async fn a_dropped_rail_costs_one_rewrite_that_weaves_it_in() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x52, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = leaf_plan_with_rail(f, "cucina", "spesa");

        let cronista = ScriptedCronista::new(vec![
            Ok("{\"mergedBody\":\"<f1>Alice ama la pasta</f1>.\",\"description\":\"d\"}"),
            Ok(
                "{\"mergedBody\":\"<f1>Alice ama la pasta</f1>, che compra in [[alice/spesa]].\",\
                 \"description\":\"d\"}",
            ),
        ]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.remaining(), 0, "one rewrite, no more");
        assert_eq!(report.leaves, 1);
        assert!(
            report.rails_appended.is_empty(),
            "the rewrite carried it, so nothing was appended: {:?}",
            report.rails_appended
        );

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("che compra in [[alice/spesa]]"),
            "the rail is woven into the thread: {page}"
        );
        drop(dir);
    }

    /// Declined twice: the FIRST draft's prose is kept (a rewrite that
    /// carries no more rails has bought nothing), the rail is appended bare
    /// so the neighbour is reachable at all, and the report says so — the
    /// silent 33 % is what this guard exists to end.
    #[tokio::test]
    async fn a_rail_declined_twice_is_appended_bare_and_reported() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x53, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = leaf_plan_with_rail(f, "cucina", "spesa");

        let cronista = ScriptedCronista::new(vec![
            Ok(
                "{\"mergedBody\":\"<f1>Alice ama la pasta</f1>. Prima stesura.\",\
                 \"description\":\"d\"}",
            ),
            Ok(
                "{\"mergedBody\":\"<f1>Alice ama la pasta</f1>. Seconda stesura.\",\
                 \"description\":\"d\"}",
            ),
        ]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.remaining(), 0, "asked twice, never a third time");
        assert_eq!(report.leaves, 1, "an under-linked page is still a success");
        assert_eq!(
            report.rails_appended,
            vec!["cucina: [[alice/spesa]]".to_owned()],
            "the gap is reported, not swallowed"
        );

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("Prima stesura"),
            "the rewrite carried no more rails, so the first draft's prose stands: {page}"
        );
        assert!(
            !page.contains("Seconda stesura"),
            "the second draft bought nothing and must not replace the first: {page}"
        );
        assert!(
            page.trim_end().ends_with("[[alice/spesa]]"),
            "the rail is on the page as its own trailing line: {page}"
        );
        drop(dir);
    }

    /// The split is by CADENCE, and the hourly pass keeps the old contract.
    ///
    /// A page carrying a link of its own plus a rail the REM parked earlier
    /// tonight: the hourly compile is handed BOTH as mandatory — it writes
    /// what the page says and adds to it, and never takes one away — while
    /// the night is handed only the parked rail. That one is minutes old and
    /// is not this compile's to undo; what the prose already carried is a
    /// question, and answering it is the night's job.
    #[test]
    fn the_night_inherits_a_link_as_a_question_and_the_hour_as_an_order() {
        let f = ffp(0x71, "Frodo cammina fino al mulino");
        let mut plan = leaf_plan_with_rail(f, "cammini", "mulino");
        plan.pages.insert(
            "acqua".to_owned(),
            PagePlan {
                slug: "acqua".to_owned(),
                title: "acqua".to_owned(),
                description: "il vicino deciso stanotte".to_owned(),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                pending_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "acqua.md".to_owned(),
            },
        );
        plan.link_graph.insert(
            "cammini".to_owned(),
            vec!["mulino".to_owned(), "acqua".to_owned()],
        );
        plan.authored_rails = vec![("cammini".to_owned(), "acqua".to_owned())];

        assert_eq!(
            link_targets(&plan, "cammini", Cadence::Light),
            (
                vec!["[[alice/mulino]]".to_owned(), "[[alice/acqua]]".to_owned()],
                Vec::new()
            ),
            "the hourly pass may add a link, never remove one"
        );
        assert_eq!(
            link_targets(&plan, "cammini", Cadence::Full),
            (
                vec!["[[alice/acqua]]".to_owned()],
                vec!["[[alice/mulino]]".to_owned()]
            ),
            "tonight's parked rail binds; what the prose already said is offered"
        );
    }

    /// At night, a link the page carried and the model let go STAYS gone.
    ///
    /// This is the whole point. The same reply at the hourly cadence buys a
    /// rewrite and, failing that, a bare appended rail
    /// (`a_dropped_rail_costs_one_rewrite_that_weaves_it_in` beside this).
    /// Here it costs one call and nothing else: the guard enforces only what
    /// the compile was told it MUST say, and at night that is the parked
    /// rails alone. Otherwise the night could never take a link away, and
    /// every sketch the cheap tier ever wrote would be permanent.
    #[tokio::test]
    async fn the_night_may_let_a_link_go_and_nothing_puts_it_back() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x72, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = leaf_plan_with_rail(f, "cucina", "spesa");

        let cronista = ScriptedCronista::new(vec![Ok(
            "{\"mergedBody\":\"<f1>Alice ama la pasta</f1>.\",\"description\":\"d\"}",
        )]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Full,
            "2026-08-26T00:00:00Z",
        )
        .await
        .expect("compile");

        assert_eq!(cronista.remaining(), 0, "one call — no rewrite is bought");
        assert_eq!(report.leaves, 1);
        assert!(
            report.rails_appended.is_empty(),
            "an offered link that was let go is a decision, not a gap: {:?}",
            report.rails_appended
        );
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            !page.contains("[[alice/spesa]]"),
            "nothing put the link back on the page: {page}"
        );
        drop(dir);
    }

    /// What the night may NOT drop is the rail the REM parked minutes ago.
    ///
    /// `crate::rem::run_rail_writer` runs before the compile, so a compile
    /// free to discard its choice would undo the night's own decision inside
    /// the same night. A parked rail is handed over as mandatory at both
    /// cadences, and the guard still appends it.
    #[tokio::test]
    async fn a_rail_parked_tonight_still_binds_tonight() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x73, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let mut plan = leaf_plan_with_rail(f, "cucina", "spesa");
        plan.authored_rails = vec![("cucina".to_owned(), "spesa".to_owned())];

        let cronista = ScriptedCronista::new(vec![
            Ok("{\"mergedBody\":\"<f1>Alice ama la pasta</f1>.\",\"description\":\"d\"}"),
            Ok("{\"mergedBody\":\"<f1>Alice ama la pasta</f1>.\",\"description\":\"d\"}"),
        ]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Full,
            "2026-08-26T00:00:00Z",
        )
        .await
        .expect("compile");

        assert_eq!(cronista.remaining(), 0, "declined twice, so both calls ran");
        assert_eq!(
            report.rails_appended,
            vec!["cucina: [[alice/spesa]]".to_owned()],
            "the night cannot discard a decision the same night took"
        );
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("[[alice/spesa]]"),
            "the parked rail is on the page: {page}"
        );
        drop(dir);
    }

    /// The nightly brief is rendered for a page that has links to judge, and
    /// asked of nothing when it has none — a page with no links of its own
    /// would be paying for a brief about judging them.
    #[test]
    fn the_nightly_brief_carries_the_links_it_asks_about() {
        let dir = tempfile::tempdir().expect("tmp");
        let tree = WikiTree::open(dir.path()).expect("tree");

        assert!(
            night_part(&tree, &[]).is_none(),
            "a page with no links of its own is asked nothing"
        );
        let rendered = night_part(&tree, &["[[alice/spesa]]".to_owned()])
            .expect("the part renders from the bundled default");
        assert!(
            rendered.contains("[[alice/spesa]]"),
            "the brief names the link it is asking about: {rendered}"
        );
        assert!(
            !rendered.contains("{prior_links}"),
            "the placeholder is substituted, not shipped: {rendered}"
        );
        drop(dir);
    }

    /// Three pieces, in this order: the standing brief, the part, the page.
    ///
    /// Both boundaries are forced. The part carries this page's own links, so
    /// in the cached half it would write an entry per page and read none; and
    /// it cannot follow the page either, because the brief opens by telling the
    /// model its page is at the very end. What is left is the head of the task
    /// half — which is also where an instruction about writing a page belongs.
    #[test]
    fn the_part_opens_the_task_half_and_the_cached_prefix_is_untouched() {
        let rendered = format!(
            "standing brief\n\n{CRONISTA_TASK_MARKER}\nPAGE: \"Cucina\"\n\nYOUR FACTS: 1. pasta"
        );
        let spliced = splice_task_part(&rendered, "NIGHT BRIEF");

        let (system, task) = split_cronista_prompt(&spliced);
        assert_eq!(
            system, "standing brief",
            "the cacheable prefix is byte-identical whichever cadence runs: {system}"
        );
        let task = task.expect("the marker still cuts the prompt");
        let brief = task
            .find("NIGHT BRIEF")
            .expect("the part is in the task half");
        let page = task.find("PAGE:").expect("the page is in the task half");
        assert!(
            brief < page,
            "the instruction comes before the page it governs: {task}"
        );
        assert!(
            task.starts_with(CRONISTA_TASK_MARKER),
            "the marker still leads the task half: {task}"
        );
    }

    /// An operator override written against an older bundled body carries no
    /// marker: it is one undivided document. There is no task half to open, so
    /// the part rides its end rather than being silently dropped — the night
    /// still gets its brief.
    #[test]
    fn a_markerless_override_still_receives_the_part() {
        let spliced = splice_task_part("an override with no marker at all", "NIGHT BRIEF");
        assert!(spliced.starts_with("an override with no marker at all"));
        assert!(spliced.ends_with("NIGHT BRIEF"), "{spliced}");
    }

    async fn streak_notices(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM wiki_events WHERE kind = 'compile_failure_streak'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// One unusable Cronista reply costs one retry, not the page: the second
    /// attempt succeeds and the compile is CLEAN (no degradation recorded,
    /// no failure-ledger row).
    #[tokio::test]
    async fn cronista_parse_failure_retries_once_then_compiles_clean() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x31, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = concept_leaf_plan(f.clone(), "cucina", None);

        let cronista = ScriptedCronista::new(vec![Ok("NOT JSON"), Ok(GOOD_CRONISTA)]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.remaining(), 0, "exactly one retry happened");
        assert_eq!(report.leaves, 1, "clean compile after the retry");
        assert!(report.degraded.is_empty(), "no degradation recorded");
        assert!(report.errors.is_empty());

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(page.contains("Alice ama la pasta."), "woven prose: {page}");
        assert!(
            compile_failures::get(&pool, "wikis/alice/cucina.md")
                .await
                .unwrap()
                .is_none(),
            "a clean compile leaves no failure-ledger row"
        );
        drop(dir);
    }

    /// Retry also unusable ⇒ the guard-only degraded rewrite: every planned
    /// fact reaches disk as a marked region (offsets stamped), the outcome is
    /// recorded distinctly, the failure ledger opens a streak, and the page
    /// is parked on the persisted plan's `force_dirty` for the next build.
    #[tokio::test]
    async fn cronista_double_failure_degrades_to_marked_append() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x32, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = concept_leaf_plan(f.clone(), "cucina", None);
        // Persist the plan (as build_wiki_plan does before every compile) so
        // the force_dirty parking has a plan file to mutate.
        planner::save_plan(&tree, &plan).expect("persist plan");

        let cronista = ScriptedCronista::new(vec![Ok("NOT JSON"), Err("boom: 500")]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");
        assert_eq!(cronista.remaining(), 0);
        assert_eq!(report.leaves, 0);
        assert_eq!(report.degraded.len(), 1, "degraded outcome recorded");
        assert!(
            report.degraded[0].starts_with("cucina: "),
            "degraded entry names the page: {:?}",
            report.degraded
        );
        assert!(report.errors.is_empty(), "degraded ≠ failed");

        // The fact reached disk as a bare marked region — no invention, no
        // inline ACL — and its row was repointed with real offsets.
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains(&format!("f={}", f.fact_id)),
            "marker on disk: {page}"
        );
        assert!(page.contains("Alice loves pasta"), "canonical claim text");
        assert!(
            !page.contains("subject=") && !page.contains("owner="),
            "bare runtime marker"
        );
        let row = fact_index::find_by_id(&pool, &f.fact_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/cucina.md");
        assert!(row.region_start.is_some(), "offsets stamped");

        // Failure ledger: streak opened at 1 (no notice yet — threshold is 2).
        let streak = compile_failures::get(&pool, "wikis/alice/cucina.md")
            .await
            .unwrap()
            .expect("ledger row");
        assert_eq!(streak.consecutive, 1);
        assert_eq!(streak_notices(&pool).await, 0);

        // Parked for a retry: the persisted plan's force_dirty carries the
        // slug, so the next build recompiles it even with zero new facts.
        let persisted = planner::load_previous_plan(&tree).unwrap().unwrap();
        assert!(
            persisted.force_dirty.contains(&"cucina".to_owned()),
            "degraded page parked force_dirty: {:?}",
            persisted.force_dirty
        );
        drop(dir);
    }

    /// A fact that left the page takes its region with it, even when the
    /// Cronista is down.
    ///
    /// Left behind, the marker outlives the row that pointed at it: the page
    /// then carries more markers than the index has rows for it, and every
    /// structural move refuses it (`marker set diverged from fact_index`).
    /// Since only a successful Cronista rewrites the page whole, a page in
    /// that state is stranded for as long as the failure lasts — and the
    /// grouping that would have carried it into a new wiki is refused every
    /// night.
    #[tokio::test]
    async fn a_degraded_rewrite_cuts_the_regions_of_facts_that_left() {
        let (dir, tree, pool) = setup().await;
        let stays = ffp(0x41, "Alice loves pasta");
        let leaves = ffp(0x42, "Alice bought a bicycle");
        plant_fact(&pool, &stays.fact_id, "user:alice", "Alice loves pasta").await;
        plant_fact(
            &pool,
            &leaves.fact_id,
            "user:alice",
            "Alice bought a bicycle",
        )
        .await;
        let cronista = FakeLlmBackend::new("fake", "NOT JSON");

        // Both facts on the page, written by a degraded pass (the Cronista is
        // down from the start): two markers on disk.
        let mut plan = concept_leaf_plan(stays.clone(), "cucina", None);
        plan.pages
            .get_mut("cucina")
            .unwrap()
            .primary_facts
            .push(leaves.clone());
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile 1");
        let page = dir.path().join("wikis/alice/cucina.md");
        let after_1 = std::fs::read_to_string(&page).unwrap();
        assert_eq!(after_1.matches(&format!("f={}", leaves.fact_id)).count(), 1);

        // The second fact is refiled elsewhere: the plan no longer gives it to
        // this page. The Cronista is still down.
        let plan = concept_leaf_plan(stays.clone(), "cucina", None);
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T01:00:00Z",
        )
        .await
        .expect("compile 2");
        let after_2 = std::fs::read_to_string(&page).unwrap();
        assert_eq!(
            after_2.matches(&format!("f={}", leaves.fact_id)).count(),
            0,
            "the departed fact's region is gone: {after_2}"
        );
        assert!(
            !after_2.contains("bicycle"),
            "and so is the prose that carried it: {after_2}"
        );
        assert_eq!(
            after_2.matches(&format!("f={}", stays.fact_id)).count(),
            1,
            "the fact that stayed keeps its one region: {after_2}"
        );
        drop(dir);
    }

    /// The retry is not a repeat: it asks for twice the room.
    ///
    /// The failure this ladder actually meets is a reply that ran out of room —
    /// truncated, or nothing but reasoning and no answer at all — and the room
    /// to think is derived from what the caller asks for. Retrying under the
    /// same ceiling fails the same way, and the page ends up written the
    /// degraded way for want of a bigger number.
    #[tokio::test]
    async fn the_cronista_retry_asks_for_twice_the_room() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x51, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = concept_leaf_plan(f, "cucina", None);
        // Unusable both times, so the whole ladder runs.
        let cronista = FakeLlmBackend::new("fake", "NOT JSON");

        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile");

        let asked = cronista.max_tokens_seen();
        assert_eq!(asked.len(), 2, "one attempt and one retry: {asked:?}");
        let first = asked[0].expect("the Cronista names its ceiling");
        assert_eq!(
            asked[1],
            Some(first * 2),
            "the retry asks for twice the first ceiling: {asked:?}",
        );
        drop(dir);
    }

    /// The degraded append is idempotent (a re-run appends nothing — no
    /// duplicate regions) and the failure notice fires exactly once when the
    /// streak hits the threshold, not on every further failing cycle.
    #[tokio::test]
    async fn degraded_append_is_idempotent_and_notice_fires_once_at_threshold() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x33, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = concept_leaf_plan(f.clone(), "cucina", None);
        // Always-unparseable Cronista: both the attempt and the retry fail on
        // every run.
        let cronista = FakeLlmBackend::new("fake", "NOT JSON");

        // Run 1: degraded append (streak 1, no notice).
        let r1 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile 1");
        assert_eq!(r1.degraded.len(), 1);
        let after_1 = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert_eq!(
            after_1.matches(&format!("f={}", f.fact_id)).count(),
            1,
            "one marker after the first degraded pass"
        );

        // Run 2: idempotent (byte-identical page, still exactly one marker),
        // streak 2 ⇒ the notice fires.
        let r2 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T01:00:00Z",
        )
        .await
        .expect("compile 2");
        assert_eq!(r2.degraded.len(), 1, "still degraded, never settles clean");
        let after_2 = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert_eq!(after_2, after_1, "second degraded pass is a no-op on disk");
        assert_eq!(
            after_2.matches(&format!("f={}", f.fact_id)).count(),
            1,
            "no duplicate region on the re-run"
        );
        let streak = compile_failures::get(&pool, "wikis/alice/cucina.md")
            .await
            .unwrap()
            .expect("ledger row");
        assert_eq!(streak.consecutive, 2);
        assert_eq!(streak_notices(&pool).await, 1, "notice at exactly 2");

        // Run 3: streak 3 — between thresholds, NO second notice.
        compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T02:00:00Z",
        )
        .await
        .expect("compile 3");
        assert_eq!(
            compile_failures::get(&pool, "wikis/alice/cucina.md")
                .await
                .unwrap()
                .unwrap()
                .consecutive,
            3
        );
        assert_eq!(
            streak_notices(&pool).await,
            1,
            "once per streak threshold, not every cycle"
        );

        // The notice row carries the ledger context for the operator.
        let (wiki_id, payload): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT wiki_id, payload FROM wiki_events WHERE kind = 'compile_failure_streak'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(wiki_id.as_deref(), Some("alice"));
        let payload = payload.expect("payload");
        assert!(payload.contains("\"consecutive\":2"), "{payload}");
        assert!(payload.contains("wikis/alice/cucina.md"), "{payload}");
        drop(dir);
    }

    /// A later successful compile supersedes the degraded tail with the real
    /// rewrite and RESETS the failure ledger (only a clean full rewrite ends
    /// the streak).
    #[tokio::test]
    async fn clean_compile_resets_the_failure_ledger() {
        let (dir, tree, pool) = setup().await;
        let f = ffp(0x34, "Alice loves pasta");
        plant_fact(&pool, &f.fact_id, "user:alice", "Alice loves pasta").await;
        let plan = concept_leaf_plan(f.clone(), "cucina", None);

        // Run 1 fails twice ⇒ degraded (streak 1); run 2 succeeds first try.
        let cronista = ScriptedCronista::new(vec![
            Ok("NOT JSON"),
            Ok("STILL NOT JSON"),
            Ok(GOOD_CRONISTA),
        ]);
        let r1 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("compile 1");
        assert_eq!(r1.degraded.len(), 1);
        assert!(
            compile_failures::get(&pool, "wikis/alice/cucina.md")
                .await
                .unwrap()
                .is_some()
        );

        let r2 = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T01:00:00Z",
        )
        .await
        .expect("compile 2");
        assert_eq!(cronista.remaining(), 0);
        assert_eq!(r2.leaves, 1, "the proper rewrite landed");
        assert!(r2.degraded.is_empty());
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(
            page.contains("Alice ama la pasta."),
            "clean rewrite superseded the degraded tail: {page}"
        );
        assert!(
            compile_failures::get(&pool, "wikis/alice/cucina.md")
                .await
                .unwrap()
                .is_none(),
            "clean full rewrite ends the streak"
        );
        drop(dir);
    }

    /// A transport error on one page's Cronista is caught by the per-page
    /// path (retry → degraded) and never aborts the compile pass: the other
    /// dirty pages still compile.
    #[tokio::test]
    async fn transport_error_on_one_page_does_not_abort_the_pass() {
        let (dir, tree, pool) = setup().await;
        let prose = ffp(0x35, "Alice loves pasta");
        let record = ffp(0x36, "latte");
        plant_fact(&pool, &prose.fact_id, "user:alice", "Alice loves pasta").await;
        plant_fact(&pool, &record.fact_id, "user:alice", "latte").await;

        let mut pages = BTreeMap::new();
        let mut prose_page = concept_leaf_plan(prose.clone(), "cucina", None)
            .pages
            .remove("cucina")
            .unwrap();
        prose_page.slug = "cucina".to_owned();
        pages.insert("cucina".to_owned(), prose_page);
        let lista_page = concept_leaf_plan(record.clone(), "spesa", Some("lista"))
            .pages
            .remove("spesa")
            .unwrap();
        pages.insert("spesa".to_owned(), lista_page);
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: BTreeMap::new(),
            compilation_order: vec!["cucina".to_owned(), "spesa".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 2,
            dirty_pages: vec!["cucina".to_owned(), "spesa".to_owned()],
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
            authored_rails: Vec::new(),
        };

        // Both Cronista attempts die on transport; the lista page needs no LLM.
        let cronista = ScriptedCronista::new(vec![Err("connection refused"), Err("timeout")]);
        let report = compile_dirty_pages(
            &pool,
            &tree,
            &plan,
            &cronista,
            Cadence::Light,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("one flaky page must not abort the pass");
        assert_eq!(report.degraded.len(), 1, "the flaky page degraded");
        assert_eq!(report.lists, 1, "the other page still compiled");
        assert!(report.errors.is_empty());

        // Both facts are on disk with markers.
        let cucina = std::fs::read_to_string(dir.path().join("wikis/alice/cucina.md")).unwrap();
        assert!(cucina.contains(&format!("f={}", prose.fact_id)));
        let spesa = std::fs::read_to_string(dir.path().join("wikis/alice/spesa.md")).unwrap();
        assert!(spesa.contains(&format!("f={}", record.fact_id)));
        drop(dir);
    }
}
