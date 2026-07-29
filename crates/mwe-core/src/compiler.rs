// SPDX-License-Identifier: AGPL-3.0-or-later
//! Narrative **compiler** — Il Cronista + the Hub Writer.
//!
//! The compiler is the prose stage: it consumes the [`CompilationPlan`] the
//! planner ([`crate::planner`]) produced and, page by page, turns the facts
//! assigned to each page into cohesive prose markdown. It is the second half of
//! the narrative compiler — the planner decides *where each fact lives*, the
//! compiler decides *how it reads*.
//!
//! Per dirty page, [`compile_page`] routes:
//! - a **hub** (0 facts, ≥1 children, `concept_hub`/`group_theme`) →
//!   [`compile_hub_page`] (the Hub Writer, cheap model): an overview that cites
//!   every child `[[wikilink]]`, never facts.
//! - a **`lista`-style leaf** (the ingest classifier's `page.style`) →
//!   [`compile_list_page`] (the Record Writer, **no LLM**): each atomic fact
//!   rendered deterministically as one bullet record wrapped in its ACL marker,
//!   bypassing Il Cronista.
//! - everything else → [`compile_leaf_page`] (Il Cronista, **strong** model):
//!   the page's own facts woven into prose, each claim wrapped in a
//!   bare `{{f=<fact_id>}}…{{/}}` runtime ACL marker (the full
//!   `{{owner=… allow=… sender=…}}` form is export-only), other pages
//!   reachable only by `[[wikilink]]`.
//!
//! ## Degraded mode — no page stays frozen
//!
//! A Cronista reply that is unusable (transport error, or output that is not
//! parseable JSON) gets **one retry** (fresh call, strict-JSON reminder in the
//! user message). If the retry is also unusable the page falls back to a
//! **guard-only rewrite** ([`compile_degraded_leaf`]): the existing prose is
//! kept byte-for-byte and every planned fact still missing a marker on disk is
//! appended as its own marked region — the standing forward-completeness-guard
//! shape — so every fact reaches disk with a marker (recall/redaction work,
//! offsets stamped) while the full rewrite waits for the next successful
//! compile. The degraded path **never invents content** (only canonical claim
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

use std::collections::HashMap;

use serde::Deserialize;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::compile_failures;
use crate::events::{self, EventKind};
use crate::fact_index::{self, FactIndexError};
use crate::llm::{CompletionRequest, LlmBackend, LlmError};
use crate::meta_annotate;
use crate::parser::{self, ParseEvent};
use crate::planner::{self, CompilationPlan, FactForPage, PagePlan, PageType};
use crate::prompts::{self, PromptError};
use crate::types::{FactId, Principal};
use crate::wiki::{WikiError, WikiTree, workdir_relative_source_path};

/// Bundled default for the Cronista prompt (compiler prose stage).
pub const BUNDLED_CRONISTA_MD: &str = include_str!("../prompts/cronista.md");

/// A wiki's overview page path — its foundation (person / `group_theme`) page.
/// Concept pages use `<slug>.md`, so this uniquely marks the page whose
/// description becomes the wiki's `_meta` abstract.
const INDEX_PAGE: &str = "index.md";

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
    /// Leaf pages (re)written as prose by Il Cronista (`prosa` / `prosa-tecnica`).
    pub leaves: usize,
    /// Hub pages (re)written.
    pub hubs: usize,
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
}

impl CompileReport {
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
    /// One-line summary of the page; for a wiki's `index.md` overview page this
    /// becomes the wiki's `_meta` abstract. Also the page's `description:`
    /// testata field.
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
/// `cronista` is the strong-model backend for leaves; `hub` the cheap backend
/// for hubs. Per-page failures are collected into the report; the run continues.
///
/// # Errors
///
/// Infrastructure failures (DB / filesystem). LLM/parse failures are soft.
pub async fn compile_dirty_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
    plan: &CompilationPlan,
    cronista: &dyn LlmBackend,
    hub: &dyn LlmBackend,
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
    let mut tone_cache: HashMap<String, String> = HashMap::new();
    // Sibling of the tone memo, and memoised for the same reason: resolving a
    // wiki's language walks the scope chain to the root wiki (a `tree.walk()`
    // per hop) and then hits the DB. Both are per-wiki constants for the whole
    // run, and a compile touches many pages of the same wiki.
    let mut locale_cache: HashMap<String, String> = HashMap::new();
    // The page index is a pure function of the plan, so it is built once per
    // run and handed to every leaf: it is the same ~3.5k tokens for all of
    // them, which is exactly what makes it the cacheable half of the Cronista
    // system prompt (see `split_cronista_prompt`). Rebuilding it per page also
    // rebuilt the same string 15-plus times for nothing.
    let page_index = page_index_block(plan);
    // Pages whose compile failed or degraded: parked on the persisted plan's
    // `force_dirty` below, so the next build retries the proper rewrite even
    // on an otherwise-idle night (the early-skip would clear the dirty set).
    let mut retry_slugs: Vec<String> = Vec::new();
    for slug in &plan.dirty_pages {
        let Some(page) = plan.pages.get(slug) else {
            // Removed page: its on-disk file is handled by the orphan
            // sweep at the tail of this compile (once no row points at it).
            continue;
        };
        match compile_page(
            pool,
            tree,
            plan,
            page,
            cronista,
            hub,
            &mut tone_cache,
            &mut locale_cache,
            &page_index,
            now,
        )
        .await
        {
            Ok(PageOutcome::Leaf) => {
                report.leaves += 1;
                note_page_success(pool, tree, page).await;
            },
            Ok(PageOutcome::Hub) => {
                report.hubs += 1;
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
        hubs = report.hubs,
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
/// never touched its `.md` — a live-write page whose fact the Conciliatore
/// re-routed, or a leaf whose facts all moved away, survived as a zombie
/// file with stale marker copies that the recall navigator kept reading
/// (the dogfood re-run's duplicated registry twin). This sweep walks each
/// plan-covered wiki (smart wikis never enter a plan) and removes a
/// concept-page file only when ALL of:
///
/// - its path is not in the plan's page set for that wiki,
/// - it is not a reserved page (`index.md`, `rules.md`, any `_`-prefixed
///   file),
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
                || name == "index.md"
                || name == "rules.md"
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
    Leaf,
    Hub,
    List,
    Unchanged,
    /// The Cronista failed twice and the page fell back to the guard-only
    /// append ([`compile_degraded_leaf`]); `reason` is the failure chain
    /// (first attempt / retry) for the report and the failure ledger.
    Degraded {
        reason: String,
    },
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
    hub: &dyn LlmBackend,
    tone_cache: &mut HashMap<String, String>,
    locale_cache: &mut HashMap<String, String>,
    page_index: &str,
    now: &str,
) -> Result<PageOutcome> {
    // An emerged/topic-wiki index rides the same dispatch: it renders as
    // prose while it still carries facts and flips to the hub overview once
    // its facts have moved down onto children.
    let is_hub = page.primary_facts.is_empty()
        && !page.child_leaves.is_empty()
        && matches!(
            page.page_type,
            PageType::ConceptHub | PageType::GroupTheme | PageType::EmergedIndex
        );
    if is_hub {
        let language = cached_language_directive(pool, tree, &page.wiki_id, locale_cache).await;
        return compile_hub_page(tree, plan, page, hub, &language, now).await;
    }
    // Il Cronista a 3 stili. A leaf whose ingest-decided style (`page.style`) is
    // `lista` holds atomic-record data (a shopping list, a filmography), not
    // prose: render it deterministically as ACL-markered records (cheap, NO LLM),
    // bypassing Il Cronista. prosa / prosa-tecnica still go to the strong-model
    // Cronista below. The Cronista never emits `lista` itself (cronista.md
    // §STYLE), so `page.style` is the sole source of a record page — which is what
    // lets the testata read `lista` over a record body rather than prose.
    if normalize_style(page.style.as_deref()) == "lista" {
        return compile_list_page(pool, tree, page, now).await;
    }
    // A leaf with NO facts never reaches the LLM: the Cronista, handed an
    // empty YOUR FACTS list, invents colour prose from the wikilinks alone
    // (the dogfood re-run compiled Tolkien lore onto a zero-fact
    // foundation index). Render the deterministic minimal page instead.
    if page.primary_facts.is_empty() {
        return compile_empty_leaf(tree, page, now);
    }
    let wiki_tone = tone_cache
        .entry(page.wiki_id.clone())
        .or_insert_with(|| resolve_tone(tree, &page.wiki_id));
    // Per PAGE, not per wiki: an agent's wiki holds pages about other people
    // too (see `tone_for_page`), and those must not be narrated as the agent's
    // own life.
    let tone = tone_for_page(wiki_tone, page);
    let language = cached_language_directive(pool, tree, &page.wiki_id, locale_cache).await;
    compile_leaf_page(
        pool, tree, plan, page, cronista, &tone, &language, page_index, now,
    )
    .await
}

/// The wiki's `LANGUAGE` directive, resolved once per wiki per run.
///
/// The deterministic renders (list pages, fact-less leaves) never call
/// this: they write no prose, so they must not pay the scope-chain walk
/// either. Only the two LLM branches above ask.
async fn cached_language_directive(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &str,
    cache: &mut HashMap<String, String>,
) -> String {
    if let Some(hit) = cache.get(wiki_id) {
        return hit.clone();
    }
    let directive =
        crate::locale::memory_directive_for_wiki(pool, tree, &parse_wiki_id(wiki_id)).await;
    cache.insert(wiki_id.to_owned(), directive.clone());
    directive
}

/// The deterministic render of a fact-less leaf — usually a foundation
/// page whose facts have not arrived yet (or have all moved away; empty
/// CONCEPT leaves are garbage-collected by the planner and never get
/// here). No LLM: with nothing to narrate, anything a model writes is
/// invention. The body is just the page's one-liner description; the
/// next compile with real facts replaces it wholesale.
fn compile_empty_leaf(tree: &WikiTree, page: &PagePlan, now: &str) -> Result<PageOutcome> {
    let body = if page.description.trim().is_empty() {
        String::new()
    } else {
        format!("_{}_", page.description.trim())
    };
    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    let contents = render_page_file(
        page,
        &body,
        &page.description,
        normalize_style(page.style.as_deref()),
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
    Ok(PageOutcome::Leaf)
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
    page_index: &str,
    now: &str,
) -> Result<PageOutcome> {
    let prompt = prompts::render(
        "cronista",
        tree.workdir(),
        BUNDLED_CRONISTA_MD,
        &[
            ("locale", language_directive),
            ("title", page.title.as_str()),
            ("slug", page.slug.as_str()),
            ("parent_hub", page.parent_hub.as_deref().unwrap_or("—")),
            ("tone", tone),
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
            ("page_index", page_index),
            ("links", recommended_links(plan, &page.slug).as_str()),
        ],
    )?;
    // One retry on an unusable reply (transport error OR unparseable JSON),
    // then the degraded guard-only fallback — a failing Cronista must never
    // freeze the page (see the module's degraded-mode section).
    let body = match cronista_with_retry(
        llm,
        &prompt,
        &page.slug,
        cronista_max_tokens(page.primary_facts.len()),
    )
    .await
    {
        Ok(b) => b,
        Err(reason) => return compile_degraded_leaf(pool, tree, page, now, &reason).await,
    };

    // The Cronista marks each fact's prose span with a lightweight `<fN>…</fN>`
    // tag (N = 1-based index into the page's facts); the load-bearing region
    // marker is rendered HERE by code, not hand-written by the LLM. Expand those
    // tags into the bare runtime `{{f=<uuid>}}…{{/}}` markers (the ACL lives in
    // the `fact_index` columns and gates by that key), then drop any orphan tag
    // the model left behind — this removes the brace/attribute miscount failure
    // mode of LLM-written markers.
    let mut merged_body =
        strip_orphan_fact_tags(&expand_fact_tags(&body.merged_body, &page.primary_facts));

    // Every assigned fact must end up wrapped in a marker on the page.
    let known: std::collections::BTreeSet<&str> = page
        .primary_facts
        .iter()
        .map(|f| f.fact_id.as_str())
        .collect();
    // What actually made it onto the page as a marker.
    let emitted: std::collections::BTreeSet<String> = parser::parse(&merged_body)
        .events
        .into_iter()
        .filter_map(|ev| match ev {
            ParseEvent::Region { attrs, .. } => attrs.fact_id.map(|f| f.as_str().to_owned()),
            _ => None,
        })
        .collect();
    // Forward completeness guard: every assigned fact must have produced a
    // marker. If the Cronista didn't tag one (omitted it, or used a tag the
    // expander could not resolve), append it deterministically as its own marked
    // region so nothing is silently dropped and no non-global fact loses its
    // protective ACL marker (the `missing_acl_markers` the reviewer flags). A
    // later full recompile can weave the appended facts back into the prose.
    let missing: Vec<&FactForPage> = page
        .primary_facts
        .iter()
        .filter(|f| !emitted.contains(f.fact_id.as_str()))
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            slug = %page.slug,
            missing = missing.len(),
            "compiler: facts without a marker after tag expansion — appending (forward completeness guard)"
        );
        for f in missing {
            let region = crate::capture::render_marker(&f.fact_id, &f.text.replace('\n', " "));
            merged_body.push_str("\n\n");
            merged_body.push_str(&region);
        }
    }

    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    // The testata: the page's writing style prefers the ingest classifier's
    // per-page proposal (`page.style`, decided at ingest and carried through the
    // plan), falling back to the Cronista's compile-time choice (`body.style`)
    // when ingest proposed none. Its fresh `description` is the page's «what goes
    // in here» one-liner.
    let contents = render_page_file(
        page,
        &merged_body,
        &body.description,
        normalize_style(page.style.as_deref().or(body.style.as_deref())),
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

    // Recall navigation: a wiki's `index.md` overview page carries the
    // wiki's one-line abstract. Persist the Cronista's fresh `description` into the
    // wiki's `_meta` summary so the catalog / root index can show it. Best-effort —
    // a `_meta` hiccup must not fail the page that already wrote.
    if page.page_path == INDEX_PAGE
        && let Err(e) = meta_annotate::sync_wiki_summary(handle.abs_dir(), body.description.trim())
    {
        tracing::warn!(slug = %page.slug, error = %e, "compiler: _meta summary sync failed");
    }

    Ok(PageOutcome::Leaf)
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
/// machinery, the system prompt is unchanged). `Err` is the combined
/// two-failure reason the degraded fallback records.
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
/// None)`: the entire prompt stays in the system field exactly as before
/// and nothing is marked cacheable, because a system prompt that varies
/// per page would write one cache entry per call and read none.
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
    /// ([`LlmError::Invalid`]) or the credential is bad
    /// ([`LlmError::Auth`]). Observed live: with the API answering
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
            match cronista_attempt(llm, system, task, retry_msg, max_tokens).await {
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
                LlmError::Invalid(_) | LlmError::Auth(_) => Err(CronistaFailure::Permanent(msg)),
                _ => Err(CronistaFailure::Retryable(msg)),
            }
        },
    }
}

/// The **guard-only rewrite** — the degraded fallback when the Cronista
/// failed twice. Never invents content and leaves the page better than
/// frozen:
///
/// - the existing on-disk page (prose, frontmatter, markers) is kept
///   **byte-for-byte**;
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
    let existing = handle.read_page(page_path).unwrap_or_default();

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
                normalize_style(page.style.as_deref()),
                &preserved_created(&existing, now),
                now,
            )
        } else {
            // Append-only: existing prose and frontmatter stay
            // byte-for-byte (the next clean compile refreshes the testata).
            let mut out = existing.trim_end().to_owned();
            out.push_str("\n\n");
            out.push_str(&regions);
            out.push('\n');
            out
        }
    };

    if contents != existing {
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
        reason,
        "compiler: degraded guard-only rewrite (existing prose kept, missing facts appended)"
    );
    Ok(PageOutcome::Degraded {
        reason: format!("{reason} — degraded append of {appended} missing fact region(s)"),
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
            // `{{embed=…}}` markers (the media pipeline); the Cronista's
            // rewritten span must not silently drop them — re-append by
            // code whatever the model ate. The whole body is checked
            // too: a marker the model kept in adjacent prose must not
            // be appended a second time inside the span.
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
    // HERE by code (never an LLM — the ACL is load-bearing, CLAUDE §11). A record
    // is a single line, so any newline in the claim collapses to a space. A
    // closed record carries its done-cue INSIDE the marker, so redaction hides
    // the closure together with the fact it describes.
    let body = page
        .primary_facts
        .iter()
        .map(|f| {
            let mut line = f.text.replace('\n', " ");
            if let Some(cue) = record_closure_cue(f) {
                line.push_str(&cue);
            }
            let record = crate::capture::render_marker(&f.fact_id, &line);
            format!("- {record}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    // Testata: the style is `lista` (the ingest classifier's
    // per-page choice that routed us here); the description is the plan's
    // ingest-proposed one-liner — there is no Cronista on this path to emit one.
    let contents = render_page_file(
        page,
        &body,
        &page.description,
        normalize_style(page.style.as_deref()),
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

    // Recall navigation: if this list is a wiki's `index.md` overview,
    // persist its plan description as the wiki's `_meta` abstract.
    if page.page_path == INDEX_PAGE
        && let Err(e) = meta_annotate::sync_wiki_summary(handle.abs_dir(), page.description.trim())
    {
        tracing::warn!(slug = %page.slug, error = %e, "compiler: _meta summary sync failed");
    }

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
/// and an open record renders exactly as before.
fn record_closure_cue(f: &FactForPage) -> Option<String> {
    let reason = f.decay_reason.as_deref()?;
    let glyph = if reason == fact_index::decay::COMPLETED {
        '✓'
    } else {
        '✗'
    };
    // The date part of the closure instant (ISO-8601 `YYYY-MM-DD…`); a
    // malformed or missing `valid_to` degrades to the bare glyph.
    let date = f
        .valid_to
        .as_deref()
        .and_then(|t| t.get(..10))
        .map_or_else(String::new, |d| format!(" {d}"));
    Some(format!(" · {glyph}{date}"))
}

// ---------- the Hub Writer (hub) ----------

async fn compile_hub_page(
    tree: &WikiTree,
    plan: &CompilationPlan,
    page: &PagePlan,
    llm: &dyn LlmBackend,
    language_directive: &str,
    now: &str,
) -> Result<PageOutcome> {
    let children = page
        .child_leaves
        .iter()
        .filter_map(|s| {
            plan.pages
                .get(s)
                .map(|c| format!("- {}", plan_page_wikilink(c)))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let snippet = page
        .child_leaves
        .iter()
        .filter_map(|s| {
            plan.pages
                .get(s)
                .map(|c| format!("- {}: {}", c.slug, c.description))
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Reuse the existing Hub Writer prompt (regenerate-index), fed from the plan.
    // A hub of an agent's wiki is a page of that agent's autobiography like any
    // other, so it carries the same first-person directive the REM regenerator
    // passes — resolved from the wiki, not from the page (a hub has no subject
    // of its own). An unresolvable wiki simply gets the default voice.
    let subject = tree
        .locate(&parse_wiki_id(&page.wiki_id))
        .map_or("", |h| crate::wiki::subject_directive(h.meta()));
    let prompt = prompts::render(
        "regenerate-index",
        tree.workdir(),
        crate::rem::BUNDLED_REGENERATE_INDEX_MD,
        &[
            ("locale", language_directive),
            ("title", page.title.as_str()),
            ("wiki_type", "hub"),
            ("wiki_id", page.slug.as_str()),
            ("subject", subject),
            ("children", children.as_str()),
            ("snippet", snippet.as_str()),
        ],
    )?;
    let resp = llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.2)
                .with_max_tokens(2_000),
        )
        .await;
    let prose = match resp {
        Ok(r) => r.text,
        Err(e) => return Err(soft(&format!("Hub Writer LLM failed: {e}"))),
    };

    let handle = tree.locate(&parse_wiki_id(&page.wiki_id))?;
    let page_path = std::path::Path::new(&page.page_path);
    let existing = handle.read_page(page_path).unwrap_or_default();
    let created = preserved_created(&existing, now);
    // The testata: a hub is an overview/navigation page, always
    // `prosa`; its `description` is the plan's one-liner (the Hub Writer emits
    // prose, not a one-liner).
    let contents = render_page_file(
        page,
        prose.trim(),
        &page.description,
        "prosa",
        &created,
        now,
    );
    if contents == existing {
        return Ok(PageOutcome::Unchanged);
    }
    handle.write_page(page_path, &contents)?;

    // Recall navigation: for a hub wiki's `index.md`, the plan's
    // one-line description is the best abstract available (the Hub Writer emits
    // prose, not a one-liner). Persist it to the wiki's `_meta` summary.
    if page.page_path == INDEX_PAGE
        && let Err(e) = meta_annotate::sync_wiki_summary(handle.abs_dir(), page.description.trim())
    {
        tracing::warn!(slug = %page.slug, error = %e, "compiler: _meta summary sync failed");
    }

    Ok(PageOutcome::Hub)
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
/// (`owner ∪ allow ∪ sender`, sorted + deduped) so the Cronista keeps that
/// fact's substance **inside its own `<fN>` span** rather than leaking it into
/// the untagged prose every reader of the page sees (prompt FACT TAGS rule).
/// A one-way projection: the DB ACL stays authoritative and the marker is
/// still rendered by code from the fact — the hint is never parsed back.
fn audience_hint(owner: &Principal, allow: &[Principal], sender: Option<&Principal>) -> String {
    if crate::acl::is_public(owner, allow, sender) {
        return String::new();
    }
    let mut names: Vec<&str> = std::iter::once(owner)
        .chain(allow.iter())
        .chain(sender)
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
    // by this 1-based number. We deliberately DO NOT show owner / allow / sender /
    // fact_id: the ACL is load-bearing and is rendered by code (see
    // [`expand_fact_tags`]), never copied by the LLM — so the model cannot drop
    // an `allow=` or miscount the braces of a marker it no longer writes.
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
            let audience = audience_hint(&f.owner, &f.allow, f.sender.as_ref());
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
/// Cronista can phrase it. (The dated open-ended hint was previously polluting
/// identity prose with record dates.)
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

/// The canonical wikilink for one planned page, per the link grammar
/// (recall-pipeline.md §Link grammar):
/// `[[wiki_id/page-slug]]` for a page hop (the slug is the page file's
/// stem — never the plan slug alone, which would read as a wiki hop to a
/// wiki that does not exist), collapsing to the bare `[[wiki_id]]` wiki
/// hop when the page is the wiki's own `index.md` overview. Every link
/// the compiler feeds the Cronista / Hub Writer goes through here so the
/// prose only ever sees resolvable rails.
fn plan_page_wikilink(page: &PagePlan) -> String {
    if page.page_path == INDEX_PAGE {
        return format!("[[{}]]", page.wiki_id);
    }
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
    (slug != current_slug).then(|| plan_page_wikilink(home))
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
fn page_index_block(plan: &CompilationPlan) -> String {
    let lines: Vec<String> = plan
        .compilation_order
        .iter()
        .filter_map(|s| {
            plan.pages
                .get(s)
                .map(|p| format!("- {}: {}", plan_page_wikilink(p), p.description))
        })
        .collect();
    if lines.is_empty() {
        "(no pages)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn recommended_links(plan: &CompilationPlan, slug: &str) -> String {
    plan.link_graph
        .get(slug)
        .map(|ls| {
            ls.iter()
                // The graph stores plan slugs; a slug whose page vanished
                // from the plan would be a dead rail — skip it.
                .filter_map(|l| plan.pages.get(l).map(plan_page_wikilink))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none specific".to_owned())
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
    style: &str,
    created: &str,
    now: &str,
) -> String {
    use std::fmt::Write as _;
    let date = now.split('T').next().unwrap_or(now);
    let title = page.title.replace('"', "'");
    let ptype = planner::page_type_tag(page.page_type);
    let mut fm = String::with_capacity(body.len() + 256);
    fm.push_str("---\n");
    let _ = writeln!(fm, "title: \"{title}\"");
    let _ = writeln!(fm, "created: {created}");
    let _ = writeln!(fm, "updated: {date}");
    let _ = writeln!(fm, "page_type: {ptype}");
    let _ = writeln!(fm, "style: {style}");
    let desc = description.replace(['"', '\n'], " ");
    let desc = desc.trim();
    if !desc.is_empty() {
        let _ = writeln!(fm, "description: \"{desc}\"");
    }
    if let Some(h) = &page.parent_hub {
        let _ = writeln!(fm, "parent_hub: {h}");
    }
    fm.push_str("---\n\n");
    fm.push_str(body.trim());
    fm.push('\n');
    fm
}

/// Coerce a Cronista-emitted style into the closed palette
/// (`prosa` / `prosa-tecnica` / `lista`). An absent / unrecognised value falls
/// back to `prosa`: a compiled standard page is prose by default, and the tag is
/// a recall read-hint, never a hard gate.
pub(crate) fn normalize_style(style: Option<&str>) -> &'static str {
    match style.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("prosa-tecnica") => "prosa-tecnica",
        Some("lista") => "lista",
        _ => "prosa",
    }
}

/// The autobiography voice: the wiki's subject is an agent and it is writing
/// about itself. Legended in `cronista.md` under TONE.
const AGENT_TONE: &str = "agent-autobiography-first-person";

/// The voice of a person's own wiki, and the fallback for a page inside an
/// agent's wiki whose subject is somebody else.
const IDENTITY_TONE: &str = "narrative-first-person-when-sender-equals-owner";

/// Resolve the prose tone of a page's wiki from its `wiki_type`.
///
/// The known actor / root wiki types map straight to a fixed tone; every
/// other wiki type (emergent topic wikis, content wikis) falls back to a
/// neutral narrative tone. Cached per wiki within a compile run — which is why
/// the per-page narrowing lives in [`tone_for_page`] and not here.
fn resolve_tone(tree: &WikiTree, wiki_id: &str) -> String {
    let Ok(handle) = tree.locate(&parse_wiki_id(wiki_id)) else {
        return "narrative".to_owned();
    };
    // An agent's own wiki is a `wiki-user` like a human's — the agent IS an
    // enrolled user — so the type alone would give it a human's voice, and its
    // self-facts ("l'agente ha aiutato l'utente…") would compile into a service
    // log written about it in the third person. It is an autobiography: the
    // subject writes it, so the voice is first person. Checked before the type
    // because it is the more specific claim about the same wiki.
    if handle.meta().is_agent {
        return AGENT_TONE.to_owned();
    }
    match handle.meta().wiki_type.as_str() {
        "wiki-user" => IDENTITY_TONE,
        "wiki-group" => "shared",
        "wiki-root" => "telegraphic",
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
fn tone_for_page(wiki_tone: &str, page: &PagePlan) -> String {
    if wiki_tone != AGENT_TONE {
        return wiki_tone.to_owned();
    }
    let agent = Principal::User(page.wiki_id.clone());
    let mine = page
        .primary_facts
        .iter()
        .filter(|f| f.owner == agent)
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
            // Obsidian-style existence check: byte-exact first, else the
            // unique case-insensitive match — same resolution the recall
            // navigator applies to page hops.
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

fn soft(msg: &str) -> CompilerError {
    CompilerError::Io(std::io::Error::other(msg.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        std::fs::write(wikis.join("alice/index.md"), "# alice\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        (dir, tree, pool)
    }

    /// An agent's own wiki gets the autobiography voice, and it wins over the
    /// type: the wiki IS a `wiki-user` (the agent is an enrolled user), so
    /// reading the type alone would compile its self-facts into a third-person
    /// dossier about it — "l'agente ha aiutato l'utente…" — instead of its own
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
        let mut mine = page_with_owners("hermes1", &["user:hermes1", "user:hermes1"]);
        assert_eq!(tone_for_page(AGENT_TONE, &mine), AGENT_TONE);

        // One stray fact does not flip a page that is mostly the agent's.
        mine.primary_facts
            .push(ffp_owned(9, "Carol parte lunedì", "user:carol"));
        assert_eq!(tone_for_page(AGENT_TONE, &mine), AGENT_TONE);

        let hers = page_with_owners("hermes1", &["user:carol", "user:carol"]);
        assert_eq!(
            tone_for_page(AGENT_TONE, &hers),
            IDENTITY_TONE,
            "a page about someone else keeps the ordinary voice"
        );

        // A human's wiki is untouched by the narrowing.
        let plain = page_with_owners("alice", &["user:alice"]);
        assert_eq!(tone_for_page(IDENTITY_TONE, &plain), IDENTITY_TONE);
    }

    /// A `PagePlan` carrying one fact per owner string, for the tone tests.
    fn page_with_owners(wiki_id: &str, owners: &[&str]) -> PagePlan {
        PagePlan {
            slug: "pagina".to_owned(),
            title: "Pagina".to_owned(),
            description: String::new(),
            style: None,
            page_type: PageType::ConceptLeaf,
            owner_scope: None,
            parent_hub: None,
            child_leaves: Vec::new(),
            primary_facts: owners
                .iter()
                .enumerate()
                .map(|(i, o)| ffp_owned(u8::try_from(i).unwrap_or(0), "un fatto", o))
                .collect(),
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
            wiki_id: wiki_id.to_owned(),
            page_path: "pagina.md".to_owned(),
        }
    }

    fn ffp_owned(seed: u8, text: &str, owner: &str) -> FactForPage {
        FactForPage {
            owner: owner.parse::<Principal>().unwrap(),
            ..ffp(seed, text)
        }
    }

    fn ffp(id_seed: u8, text: &str) -> FactForPage {
        FactForPage {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_seed:02x}"))
                .unwrap(),
            text: text.to_owned(),
            fact_type: Some("bio".to_owned()),
            owner: "user:alice".parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: "alice".to_owned(),
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            successor_fact_id: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
        }
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
        // read-set (owner ∪ allow ∪ sender, sorted + deduped) so the Cronista
        // keeps its substance inside its own <fN> span, out of the
        // default-visibility connective prose.
        let mut public = ffp(1, "La biblioteca apre alle 9.");
        public.owner = Principal::global();
        let restricted = ffp(2, "Alice ha un appuntamento in ospedale.");
        // ffp defaults to owner=user:alice, allow=[], sender=None.
        let mut shared = ffp(3, "Nota di famiglia su Gollum.");
        shared.owner = "user:gollum".parse::<Principal>().unwrap();
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
            "an owner-only fact names its owner: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("(audience: famiglia, galadriel, gollum)"),
            "the read-set is owner ∪ allow ∪ sender, sorted + deduped: {}",
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
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![fact],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
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
        assert!(authored_ref_resolves(&tree, "[[alice/index]]"));
        assert!(!authored_ref_resolves(&tree, "[[alice/missing]]"));
        assert!(!authored_ref_resolves(&tree, "[[ghost]]"));
        assert!(!authored_ref_resolves(&tree, "[[ghost/page]]"));
        // Mutant shapes never resolve: no brackets, traversal, empty.
        assert!(!authored_ref_resolves(&tree, "alice/index"));
        assert!(!authored_ref_resolves(&tree, "[[alice/../secret]]"));
        assert!(!authored_ref_resolves(&tree, "[[]]"));
    }

    async fn plant_fact_at(
        pool: &SqlitePool,
        fid: &FactId,
        owner: &str,
        text: &str,
        source_path: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) {
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: source_path.to_owned(),
                region_start: start,
                region_end: end,
                text: text.to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: owner.parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
    }

    async fn plant_fact(pool: &SqlitePool, fid: &FactId, owner: &str, text: &str) {
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: text.to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: owner.parse::<Principal>().unwrap(),
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
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
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
                page_type: PageType::Person,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![FactForPage {
                    authored_refs: Vec::new(),
                    fact_id: fid.clone(),
                    text: "Alice loves pasta".to_owned(),
                    fact_type: Some("preference".to_owned()),
                    owner: "user:alice".parse::<Principal>().unwrap(),
                    allow: Vec::new(),
                    sender: None,
                    source_wiki_id: "alice".to_owned(),
                    valid_from: None,
                    valid_to: None,
                    decay_reason: None,
                    successor_fact_id: None,
                    target_page: None,
                    style: None,
                    page_description: None,
                    salience: None,
                }],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "index.md".to_owned(),
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
        }
    }

    #[tokio::test]
    async fn cronista_writes_prose_with_marker_and_repoints_fact() {
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        // Plant the fact in fact_index pointing at the buffer journal.
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: "user:alice".parse::<Principal>().unwrap(),
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
                page_description: None,
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let plan = leaf_plan(&fid);
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-05-31T00:00:00Z")
                .await
                .expect("compile");
        assert_eq!(report.leaves, 1);

        // The compiled page exists with the marker.
        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
        assert!(page.contains("Alice ama la pasta."));
        assert!(page.contains(&format!("f={fid}")));
        // The fact_index row was repointed off the journal onto the compiled page.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/index.md");
        assert!(row.region_start.is_some(), "offsets repointed");
        assert_eq!(
            row.text, "Alice loves pasta",
            "canonical claim text preserved"
        );
        // The index page's Cronista `description` became the wiki's
        // `_meta` abstract (what the catalog / root index surface).
        let meta = std::fs::read_to_string(dir.path().join("wikis/alice/_meta.md")).unwrap();
        assert!(
            meta.contains("summary: d"),
            "wiki abstract persisted to _meta: {meta}"
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
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
        let body = "{\"mergedBody\":\"Pasta. <f1>Alice ama la pasta.</f1>\",\"description\":\"d\"}"
            .to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let plan = leaf_plan(&fid);
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-05-31T00:00:00Z")
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

    /// A prompt with no marker — an operator override predating v1.14 —
    /// must behave exactly as before: everything in the system prompt, and
    /// (asserted in the unit test below) nothing marked cacheable.
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
        // free-text `description` one-liner.
        let (dir, tree, pool) = setup().await;
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: "user:alice".parse::<Principal>().unwrap(),
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
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let body = "{\"mergedBody\":\"<f1>Alice ama la pasta.</f1>\",\"description\":\"Cosa piace ad Alice\",\"style\":\"prosa-tecnica\"}".to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let plan = leaf_plan(&fid);
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-06T00:00:00Z")
            .await
            .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
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
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let body = "{\"mergedBody\":\"<f1>Alice ama la pasta.</f1>\",\"description\":\"Cosa piace ad Alice\",\"style\":\"prosa\"}".to_owned();
        let cronista = FakeLlmBackend::new("fake", &body);
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let mut plan = leaf_plan(&fid);
        // The ingest classifier proposed `prosa-tecnica` for this page.
        plan.pages.get_mut("alice").unwrap().style = Some("prosa-tecnica".to_owned());
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-06T00:00:00Z")
            .await
            .expect("compile");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
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
                style: Some("lista".to_owned()),
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![f1.clone(), f2.clone()],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
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
        };

        // If the Cronista were (wrongly) invoked, this distinctive prose would
        // land on the page. The record path must bypass it entirely.
        let cronista = FakeLlmBackend::new(
            "fake",
            "{\"mergedBody\":\"PROSE_FROM_CRONISTA\",\"description\":\"d\"}",
        );
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-07T00:00:00Z")
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
        assert!(!page.contains("owner="), "no inline ACL on disk: {page}");
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
        // one-liner under a normal testata, idempotent across compiles.
        let (dir, tree, pool) = setup().await;
        let mut pages = BTreeMap::new();
        pages.insert(
            "alice".to_owned(),
            PagePlan {
                slug: "alice".to_owned(),
                title: "Alice".to_owned(),
                description: "Identity wiki for alice.".to_owned(),
                style: None,
                page_type: PageType::Person,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "index.md".to_owned(),
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
        };

        // If the Cronista were (wrongly) invoked, this prose would land.
        let cronista = FakeLlmBackend::new(
            "fake",
            "{\"mergedBody\":\"INVENTED_LORE\",\"description\":\"d\"}",
        );
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
                .await
                .expect("compile");
        assert_eq!(report.leaves, 1, "rendered, counted as a leaf");

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
        assert!(
            !page.contains("INVENTED_LORE"),
            "no LLM on the fact-less path: {page}"
        );
        assert!(
            page.contains("_Identity wiki for alice._"),
            "the description is the whole body: {page}"
        );
        assert!(!page.contains("{{f="), "no markers without facts: {page}");

        // Idempotent: the same compile re-run is a no-op.
        let report2 =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
                .await
                .expect("compile 2");
        assert_eq!(report2.unchanged, 1, "second render matches byte-for-byte");
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
                style: Some("lista".to_owned()),
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![f1.clone()],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
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
        std::fs::write(alice_dir.join("rules.md"), "# Rules\n").unwrap();

        let cronista = FakeLlmBackend::new("fake", "unused — lista path");
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
                .await
                .expect("compile");

        assert_eq!(report.orphan_files_swept, 1, "exactly the zombie went");
        assert!(
            !alice_dir.join("registro_spesa.md").exists(),
            "the zombie file is gone"
        );
        assert!(
            alice_dir.join("vecchie_note.md").exists(),
            "a live pointer keeps the file"
        );
        assert!(
            alice_dir.join("rules.md").exists(),
            "reserved names survive"
        );
        assert!(alice_dir.join("spesa.md").exists(), "plan pages survive");
        drop(dir);
    }

    #[tokio::test]
    async fn lista_closed_records_carry_the_done_cue_inside_the_marker() {
        // A `lista` record whose fact carries a `decay_reason` renders with a
        // language-free done-cue — `· ✓ <date>` for completed, `· ✗` for
        // retracted/contradicted — INSIDE its region marker, so redaction
        // hides the closure together with the fact. An open record and a
        // window without an explicit closure render exactly as before.
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
                style: Some("lista".to_owned()),
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![open.clone(), bought.clone(), dropped, expired],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
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
        };

        let cronista = FakeLlmBackend::new("fake", "unused — lista path has no LLM");
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
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

    #[test]
    fn normalize_style_coerces_to_closed_palette() {
        assert_eq!(normalize_style(None), "prosa", "absent → default prosa");
        assert_eq!(normalize_style(Some("prosa")), "prosa");
        assert_eq!(
            normalize_style(Some(" Prosa-Tecnica ")),
            "prosa-tecnica",
            "trimmed + case-folded"
        );
        assert_eq!(normalize_style(Some("lista")), "lista");
        assert_eq!(
            normalize_style(Some("bullets")),
            "prosa",
            "unrecognised → default prosa, never a hard reject"
        );
        assert_eq!(normalize_style(Some("")), "prosa");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // inline 2-fact page fixture reads top-to-bottom
    async fn cronista_omission_recovered_by_forward_completeness_guard() {
        let (dir, tree, pool) = setup().await;
        let fid1 = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d01").unwrap();
        let fid2 = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d02").unwrap();
        // Both facts are promoted (pointing at the buffer journal). fid2 is a
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");

        // Plan: both facts assigned to alice's leaf page.
        let mut pages = BTreeMap::new();
        pages.insert(
            "alice".to_owned(),
            PagePlan {
                slug: "alice".to_owned(),
                title: "Alice".to_owned(),
                description: "Alice".to_owned(),
                style: None,
                page_type: PageType::Person,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![
                    FactForPage {
                        authored_refs: Vec::new(),
                        fact_id: fid1.clone(),
                        text: "Alice loves pasta".to_owned(),
                        fact_type: Some("preference".to_owned()),
                        owner: "user:alice".parse::<Principal>().unwrap(),
                        allow: Vec::new(),
                        sender: None,
                        source_wiki_id: "alice".to_owned(),
                        valid_from: None,
                        valid_to: None,
                        decay_reason: None,
                        successor_fact_id: None,
                        target_page: None,
                        style: None,
                        page_description: None,
                        salience: None,
                    },
                    FactForPage {
                        authored_refs: Vec::new(),
                        fact_id: fid2.clone(),
                        text: "Matteo has homework on Monday".to_owned(),
                        fact_type: Some("plan".to_owned()),
                        owner: "group:famiglia".parse::<Principal>().unwrap(),
                        allow: Vec::new(),
                        sender: None,
                        source_wiki_id: "alice".to_owned(),
                        valid_from: None,
                        valid_to: None,
                        decay_reason: None,
                        successor_fact_id: None,
                        target_page: None,
                        style: None,
                        page_description: None,
                        salience: None,
                    },
                ],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "index.md".to_owned(),
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
        };
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-01T00:00:00Z")
                .await
                .expect("compile");
        assert_eq!(report.leaves, 1);

        let page = std::fs::read_to_string(dir.path().join("wikis/alice/index.md")).unwrap();
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
            !page.contains("owner="),
            "bare runtime markers — no inline ACL on disk"
        );
        assert!(
            page.contains("Matteo has homework on Monday"),
            "appended fact body present"
        );
        // Both facts repointed off the journal onto the compiled page.
        for fid in [&fid1, &fid2] {
            let row = fact_index::find_by_id(&pool, fid).await.unwrap().unwrap();
            assert_eq!(
                row.source_path, "wikis/alice/index.md",
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
                style: style.map(str::to_owned),
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: vec![f],
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
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
        }
    }

    #[tokio::test]
    async fn plan_move_prepoints_the_row_before_the_source_rewrite_can_strand_it() {
        // The dogfood loss (roadmap 10c): a plan reassigns a fact from page A
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
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
            std::fs::write(wikis.join(format!("{w}/index.md")), format!("# {w}\n")).unwrap();
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");

        // First compile writes the record page and stamps offsets.
        let r1 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
            .await
            .expect("compile 1");
        assert_eq!(r1.lists, 1);

        // Knock the row back to a pending render elsewhere (the state a
        // pre-point leaves when the destination content already sits on disk).
        fact_index::move_region(&pool, &f.fact_id, "wikis/alice/_captures.md", None, None)
            .await
            .unwrap();

        // The identical plan renders byte-identical content → Unchanged, but
        // the repoint must still stamp the offsets.
        let r2 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-06-11T00:00:00Z")
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
    fn bundled_cronista_prompt_carries_the_identity_index_guard() {
        // The belt-guard behind the planner's identity-page discipline: when
        // writing a user's identity index, another subject's detail is
        // referenced with its [[wikilink]], never woven into the prose. One
        // sentence by design — plan placement is the load-bearing channel
        // (the Cronista only ever sees the facts the plan gave the page).
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
        // Under the <fN> contract the Cronista no longer writes markers, so the
        // ACL (owner / allow / sender / fact_id) is deliberately NOT shown to it —
        // the code renders the marker (expand_fact_tags). Withholding it is what
        // removes the brace/attribute miscount failure mode of LLM-written markers.
        let mut f = ffp(2, "Frodo works only the afternoon tomorrow");
        f.owner = "user:frodo".parse::<Principal>().unwrap();
        f.allow = vec!["group:famiglia".parse::<Principal>().unwrap()];
        f.sender = Some("user:galadriel".parse::<Principal>().unwrap());
        let txt = primary_facts_text(
            std::slice::from_ref(&f),
            "2026-06-08T12:00:00Z",
            &|_| true,
            &|_| None,
        );
        assert!(
            !txt.contains("owner="),
            "owner must NOT reach the prompt: {txt}"
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
    /// that starvation is the whole mechanism. It now also carries the
    /// page being written: one string per run is what makes the system
    /// half of the Cronista prompt a cacheable prefix, and the body pays
    /// for it with an explicit never-link-to-itself rule.
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
                    page_type: PageType::Person,
                    owner_scope: None,
                    parent_hub: None,
                    child_leaves: Vec::new(),
                    primary_facts: vec![ffp(2, "secret bob fact")],
                    outgoing_links: Vec::new(),
                    incoming_links: Vec::new(),
                    wiki_id: s.to_owned(),
                    page_path: "index.md".to_owned(),
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
        };
        let idx = page_index_block(&plan);
        assert!(
            idx.contains("[[bob]]: bob desc"),
            "shows other page description"
        );
        assert!(
            idx.contains("[[alice]]: alice desc"),
            "includes the page being written — the block is one per run"
        );
        assert!(
            !idx.contains("secret bob fact"),
            "NEVER another page's facts"
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
            page_type: PageType::ConceptLeaf,
            owner_scope: None,
            parent_hub: None,
            child_leaves: Vec::new(),
            primary_facts: Vec::new(),
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
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
                "famiglia-bruno-battaglia",
                "referto_oculistica.md"
            )),
            "[[famiglia-bruno-battaglia/referto_oculistica]]"
        );
        // A wiki's own index.md collapses to the bare wiki hop.
        assert_eq!(
            plan_page_wikilink(&leaf("famiglia", "famiglia", "index.md")),
            "[[famiglia]]"
        );

        // The starvation index and the recommended links both ride the
        // same helper.
        let mut pages = BTreeMap::new();
        pages.insert("hub".to_owned(), leaf("hub", "famiglia", "index.md"));
        pages.insert(
            "salute_bruno".to_owned(),
            leaf(
                "salute_bruno",
                "famiglia-bruno-battaglia",
                "salute_bruno.md",
            ),
        );
        let mut link_graph = BTreeMap::new();
        link_graph.insert(
            "hub".to_owned(),
            vec!["salute_bruno".to_owned(), "vanished".to_owned()],
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph,
            compilation_order: vec!["hub".to_owned(), "salute_bruno".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        let idx = page_index_block(&plan);
        assert!(
            idx.contains("- [[famiglia-bruno-battaglia/salute_bruno]]: salute_bruno desc"),
            "{idx}"
        );
        let links = recommended_links(&plan, "hub");
        assert_eq!(
            links, "[[famiglia-bruno-battaglia/salute_bruno]]",
            "graph slugs resolve through the plan; a vanished slug is skipped"
        );
    }

    // ---------- degraded mode + failure surfacing ----------

    use crate::llm::{CompletionResponse, CompletionUsage, FinishReason, LlmError};

    /// A Cronista whose backend refuses the request outright, counting
    /// attempts. Models the live failure: with the API answering "credit
    /// balance too low" (a 400 ⇒ [`LlmError::Invalid`]) every page used to
    /// buy the same refusal twice.
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let plan = leaf_plan(&fid);

        for make in [
            LlmError::Invalid as fn(String) -> LlmError,
            LlmError::Auth as fn(String) -> LlmError,
        ] {
            let cronista = RefusingCronista::new(make);
            let report =
                compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-05-31T00:00:00Z")
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let plan = leaf_plan(&fid);
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-05-31T00:00:00Z")
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
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice loves pasta".to_owned(),
                embedding: vec![0.1, 0.2],
                owner_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("preference".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: None,
                style: None,
                page_description: None,
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T00:00:00Z")
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T00:00:00Z")
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
        assert!(!page.contains("owner="), "bare runtime marker");
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");

        // Run 1: degraded append (streak 1, no notice).
        let r1 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T00:00:00Z")
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
        let r2 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T01:00:00Z")
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
        compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T02:00:00Z")
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
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let r1 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T00:00:00Z")
            .await
            .expect("compile 1");
        assert_eq!(r1.degraded.len(), 1);
        assert!(
            compile_failures::get(&pool, "wikis/alice/cucina.md")
                .await
                .unwrap()
                .is_some()
        );

        let r2 = compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T01:00:00Z")
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
        };

        // Both Cronista attempts die on transport; the lista page needs no LLM.
        let cronista = ScriptedCronista::new(vec![Err("connection refused"), Err("timeout")]);
        let hub = FakeLlmBackend::new("fake", "# hub\n");
        let report =
            compile_dirty_pages(&pool, &tree, &plan, &cronista, &hub, "2026-07-02T00:00:00Z")
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
