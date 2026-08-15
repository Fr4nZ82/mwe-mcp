// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compilation **planner** — the topology stage of the narrative
//! compiler, ported from the old engine's "Forgia della Wiki" onto mwe-mcp.
//!
//! The planner turns the flat fact store ([`crate::fact_index`], fed by the
//! light dream) into a [`CompilationPlan`]: a hub→leaf page graph in which every
//! fact lives on **exactly one** page (the one-fact-one-page invariant), hubs
//! hold only narrative + links, and a persistent [`ConceptRegistry`] stops the
//! same concept page being re-invented run-to-run. The plan is the input the
//! Cronista compiles into prose; this module never writes prose itself.
//!
//! Five stages (run by [`build_wiki_plan`]):
//!
//! 1. **Fonditore** ([`build_foundation_pages`]) — deterministic, no LLM. From
//!    [`crate::enrollment`] users + groups: one `person` page per user, one
//!    `group_theme` hub per group, with `parent_hub` / `outgoing_links` wired
//!    from group membership. These map onto mwe-mcp's existing identity wikis.
//! 2. **Cartografo** ([`classify_facts`]) — strong-model LLM, batched. Assigns
//!    each fact to one page and proposes emergent concept pages (one-fact-one-
//!    page). Reuses existing pages (foundation + registry) rather than
//!    duplicating them. The engine hands it **structural signals only**
//!    ([`CartografoSignals`]: per-fact identity-page scope tags from
//!    enrollment, per-page fact mass) — the prompt carries the placement
//!    discipline (an identity index carries one subject; a grown page splits
//!    by content), never a hardcoded gate in Rust.
//! 3. **Conciliatore** ([`conciliate_new_pages`]) — strong-model LLM, one call.
//!    Folds semantically-duplicate proposed pages into existing ones (redirects).
//! 4. **Architetto** ([`build_compilation_plan`]) — deterministic. Materialises
//!    pages, applies assignments (+ redirects), computes parent→child, runs a
//!    **fixpoint** garbage-collection of empty concept pages, builds the
//!    bidirectional link graph, and orders hubs-before-leaves.
//! 5. **Incremental** ([`build_wiki_plan`]) — carries over prior assignments,
//!    classifies only NEW facts, skips entirely on 0-new-0-removed, and computes
//!    the dirty set via [`page_fingerprint`] so only changed pages recompile.
//!
//! ## mwe-mcp adaptations (vs the flat old engine)
//!
//! - Foundation pages are the typed identity wikis (`wiki-user` = person,
//!   `wiki-group` = group hub); a page's tree home is carried on
//!   [`PagePlan::wiki_id`] + [`PagePlan::page_path`]. Concept pages are `.md`
//!   pages **within** the relevant standard wiki. A page that grows is **split
//!   into more pages** (the Cartografo's split-by-mass lever); a **sub-wiki**
//!   emerges from a different signal — a *group* of existing pages that are one
//!   subject area, via the REM promote machinery — so a page never becomes a
//!   wiki and a wiki is never born holding one page. **Emergent-page
//!   creation leaves a receipt** in `structure_proposals`
//!   ([`crate::proposals::kind::PAGE_CREATE`], born-applied and revertable —
//!   see [`record_minted_pages`]). Until 2026-08-04 this paragraph asserted
//!   the same thing while no page-level kind existed at all: the only kinds
//!   were about wikis, and twelve container pages were minted over three weeks
//!   with nothing anywhere for the operator to read.
//! - Every [`FactForPage`] carries its **stable `fact_id`** so the Cronista can
//!   emit `{{… f=<id>}}` markers and recall/supersede survive a recompile (a
//!   defect the TS original had — it lost fact identity at render time).
//! - The plan + registry persist as JSON under `wikis/_plan/` via crash-safe
//!   `atomic_write`; they are a rebuildable cache (derivable from `fact_index` +
//!   enrollment), preserving the captures-journal invariant.
//! - Determinism: pages are keyed in a [`BTreeMap`] and every order-sensitive
//!   step sorts explicitly, so the plan + its fingerprints are reproducible and
//!   the dirty set does not churn spuriously.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::enrollment;
use crate::fact_index::{self, FactIndexError};
use crate::llm::{CompletionRequest, LlmBackend};
use crate::prompts::{self, PromptError};
use crate::types::{FactId, Principal};
use crate::wiki::{WikiError, WikiTree, atomic_write};

/// Bundled default for the Cartografo prompt (planner classification stage).
pub const BUNDLED_CARTOGRAFO_MD: &str = include_str!("../prompts/cartografo.md");
/// Bundled default for the Conciliatore prompt (planner dedup stage).
pub const BUNDLED_CONCILIATORE_MD: &str = include_str!("../prompts/conciliatore.md");

/// Facts per Cartografo LLM batch.
const CARTOGRAFO_BATCH: usize = 15;

/// How many pages the memory may hold before a placement stage stops being
/// shown **every** page of the forest and gets a per-wiki selection instead.
///
/// The page list is what makes a fact free to travel: a page nobody is shown
/// is a page nobody can choose. So the default is completeness, and the
/// ceiling exists only because the list cannot grow forever.
///
/// Below it the list is **identical for every batch of the run** — one wiki's
/// batches no longer see a different list from another's — so it rides the
/// prompt's cached prefix and completeness is also the cheap answer. Above it
/// a whole-forest list stops fitting a call at all, and the only remaining
/// shape is a ranked slice. Twin of
/// [`crate::compiler::CARD_INDEX_CACHE_CEILING_PAGES`], which answers the same
/// question for the writing stage.
const FOREST_PAGE_CEILING: usize = 400;

/// How many foreign concept pages a selection carries past the ceiling.
///
/// The batch's **own** wiki is never cut — that is where most of its facts
/// belong and where every page it coins is born. What is cut is the rest of
/// the forest, and it is cut by **nearness, never alphabetically**: where a
/// list is cut the order IS the selection (founder, 2026-08-09).
const FOREIGN_SELECTION_PAGES: usize = 40;

/// Errors raised by the planner.
#[derive(Debug, Error)]
pub enum PlannerError {
    /// Enrollment / DB access failed.
    #[error("planner db: {0}")]
    Db(#[from] sqlx::Error),
    /// `fact_index` access failed.
    #[error("planner fact_index: {0}")]
    FactIndex(#[from] FactIndexError),
    /// Filesystem (plan/registry persistence) failed.
    #[error("planner wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Low-level IO.
    #[error("planner io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialisation of the plan/registry failed.
    #[error("planner json: {0}")]
    Json(#[from] serde_json::Error),
    /// Loading a planner prompt failed (a broken operator override).
    #[error("planner prompt: {0}")]
    Prompt(#[from] PromptError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, PlannerError>;

/// The kinds of page the topology distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    /// A person's **identity card** ([`crate::wiki::PROFILE_FILENAME`]) — the
    /// `wiki-user` identity wiki's foundation page, holding the always-on
    /// identity core. Foundation — never garbage-collected.
    Person,
    /// A group's **identity card** ([`crate::wiki::PROFILE_FILENAME`]) — the
    /// `wiki-group` foundation page. Holds a group-owned fact when one has no
    /// better home; mostly it links its child leaves. Foundation — never
    /// garbage-collected.
    GroupTheme,
    /// A wiki's **buffer page** ([`crate::wiki::NOTES_FILENAME`]) — where a
    /// fact lands when nothing more specific fits, on every standard wiki.
    ///
    /// For a topic wiki (an emerged dossier, a hand-forged topic container) it
    /// is the *only* foundation page: a topic has no identity, so there is no
    /// card. Foundation — never garbage-collected, so the landing page
    /// survives its facts being drained onto real pages by REM's reorg sweep,
    /// which is what is supposed to happen to everything that lands here.
    ///
    /// Serialises as `wiki_buffer`; the `emerged_index` alias keeps a plan or
    /// registry written before 2026-08-03 loadable, when this node was the
    /// wiki's `index.md` and held facts there.
    #[serde(alias = "emerged_index")]
    WikiBuffer,
    /// **Legacy.** A page that groups other pages, with no facts of its own.
    ///
    /// Retired 2026-08-04 (founder: *«un contenitore è una wiki»*). Nothing
    /// mints one any more — the Cartografo's vocabulary no longer offers it
    /// (prompt v1.8) and the planner no longer promotes an emptied leaf into
    /// one. The variant stays so a plan or registry written before that date
    /// still loads, and an existing one is collected as soon as its children
    /// are re-homed ([`reparent_to_foundation`]).
    ///
    /// The reason it went is not tidiness: a page containing pages was a
    /// second, invisible mechanism for a job wikis already do, and its
    /// creation left no record anyone could read — twelve of them appeared
    /// over three weeks unnoticed. See
    /// [`crate::proposals::kind::PAGE_CREATE`] for the other half of that fix.
    ConceptHub,
    /// A thematic detail page. Holds facts; has a parent hub. Garbage-collected
    /// when it has no facts.
    ConceptLeaf,
}

impl PageType {
    /// Compilation-order rank: hubs (groups, concept hubs) before persons and
    /// buffers before concept leaves, so a hub is written after its
    /// children are placed.
    const fn order_rank(self) -> u8 {
        match self {
            Self::GroupTheme => 0,
            Self::ConceptHub => 1,
            Self::Person | Self::WikiBuffer => 2,
            Self::ConceptLeaf => 3,
        }
    }

    /// Foundation pages (the identity cards and the buffer) are a wiki's own
    /// pages rather than the topology's, and are never garbage-collected.
    pub(crate) const fn is_foundation(self) -> bool {
        matches!(self, Self::Person | Self::GroupTheme | Self::WikiBuffer)
    }

    /// True when this node is a wiki's **identity card** — the page that
    /// answers *who is this actor*, and the home of the always-on identity
    /// core an ingest `salience: "high"` reserves.
    pub(crate) const fn is_identity_card(self) -> bool {
        matches!(self, Self::Person | Self::GroupTheme)
    }

    /// The reserved file a foundation node of this type owns.
    ///
    /// **Never [`crate::wiki::INDEX_FILENAME`]**: a wiki's map is not a plan
    /// node at all, so nothing the compiler places can land there (founder's
    /// ruling, 2026-08-03 — the root serves REM and the ingest classifier as a
    /// map of where a fact belongs, and holds no facts of its own). With no
    /// plan owning it, the REM hub writer is free to author it.
    const fn foundation_page(self) -> Option<&'static str> {
        match self {
            Self::Person | Self::GroupTheme => Some(crate::wiki::PROFILE_FILENAME),
            Self::WikiBuffer => Some(crate::wiki::NOTES_FILENAME),
            Self::ConceptHub | Self::ConceptLeaf => None,
        }
    }
}

/// Plan key of a wiki's buffer node, derived from `slugify(wiki_id)`.
///
/// A wiki's card already owns `slugify(wiki_id)`, so the buffer needs a second
/// key — and it takes this one on *every* wiki, carded or not, because the
/// callers that map a page path back to a plan slug
/// ([`plan_slug_for_page`]) have only the wiki id and the page name to go on.
/// One rule, no lookup, no chance of the same file entering the plan under two
/// keys.
///
/// The `__` separator is **unreachable by [`slugify`]** — which collapses runs
/// of non-alphanumerics to a single `_` — so no classifier-proposed page name
/// can ever collide with it.
fn buffer_slug(wiki_slug: &str) -> String {
    format!("{wiki_slug}__notes")
}

/// Plan key of a wiki-relative page path — the single mapping every caller
/// that re-homes a fact in the persisted plan must agree on.
///
/// A wiki's **reserved** pages are foundation nodes keyed per wiki: the card
/// ([`crate::wiki::PROFILE_FILENAME`]) takes `slugify(wiki_id)` and the buffer
/// ([`crate::wiki::NOTES_FILENAME`]) takes [`buffer_slug`]. Everything else is
/// a concept page keyed by its own flattened stem.
///
/// [`crate::wiki::INDEX_FILENAME`] maps to the card slug for the benefit of
/// receipts written before 2026-08-03, when the root *was* the card; nothing
/// places a fact there any more.
#[must_use]
pub fn plan_slug_for_page(wiki_id: &str, page: &str) -> String {
    let stem = page.strip_suffix(".md").unwrap_or(page);
    let wiki_slug = slugify(wiki_id);
    match stem {
        "index" | "profile" => wiki_slug,
        "notes" => buffer_slug(&wiki_slug),
        other => slugify(other),
    }
}

/// A fact materialised onto a page (one-fact-one-page). Carries the stable
/// `fact_id` so the compiler can emit `{{… f=<id>}}` markers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactForPage {
    /// Stable `UUIDv7` (== the `capture_id` it was promoted from).
    pub fact_id: FactId,
    /// Verbatim claim text (no markers).
    pub text: String,
    /// Optional taxonomy hint.
    pub fact_type: Option<String>,
    /// The fact's **subject** — who or what it is *about* (not its author
    /// `sender`, not its audience `allow`).
    /// `global` / `user:<id>` / `group:<id>`.
    #[serde(alias = "owner")]
    pub subject: Principal,
    /// Extra read principals.
    #[serde(default)]
    pub allow: Vec<Principal>,
    /// Cross-user attribution (who said it); always set on write — equals
    /// `subject` for a self-authored fact. `None` only on legacy provenance.
    pub sender: Option<Principal>,
    /// The standard wiki the fact currently lives in (its `fact_index.wiki_id`).
    pub source_wiki_id: String,
    /// Validity window start (`fact_index.valid_from`, ISO-8601); `None` =
    /// open-start. A one-way projection from the DB so the Cronista can render a
    /// readable validity range in the prose — never re-parsed back.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Validity window end (`fact_index.valid_to`, ISO-8601); `None` = open
    /// ("true now"). See [`FactForPage::valid_from`].
    #[serde(default)]
    pub valid_to: Option<String>,
    /// Why the window closed (`fact_index.decay_reason`); `None` while the
    /// fact is alive. Projected so the Cronista can phrase the closure
    /// ("bought", "retracted", "superseded") and so a closure recompiles
    /// the page (the validity fields are part of [`page_fingerprint`]).
    #[serde(default)]
    pub decay_reason: Option<String>,
    /// The fact that replaced this one (`fact_index.successor_fact_id`),
    /// when the closure knew it. Projected so the compile feed can point
    /// the reader at the successor's home page ("no longer current — today
    /// see [[…]]") and so a stamped pointer recompiles the page (it is part
    /// of [`page_fingerprint`]). `None` = no recorded successor.
    #[serde(default)]
    pub successor_fact_id: Option<FactId>,
    /// The page the ingest classifier proposed this fact be placed on (a slug or
    /// `.md` path; `fact_index.target_page`). A *hint*: in the LIGHT cadence the
    /// planner settles the fact here without re-running the strong-model
    /// Cartografo; the REM Cartografo may re-home it. `None`
    /// = unproposed (older rows / the direct path) → orphan-fallback.
    #[serde(default)]
    pub target_page: Option<String>,
    /// Ingest-proposed writing style (closed palette `prosa` | `prosa-tecnica` |
    /// `lista`) seeding a freshly-placed page's testata
    /// (`fact_index.style`). `None` = unproposed.
    #[serde(default)]
    pub style: Option<String>,
    /// Ingest-proposed "cosa ci va dentro" one-liner seeding the page's
    /// testata description (`fact_index.page_description`). `None` = unproposed.
    #[serde(default)]
    pub page_description: Option<String>,
    /// Per-fact salience the producer deduced (`fact_index.salience`, closed
    /// palette `high` | `normal` | `low`). `high` = always-on material
    /// (identity, health/safety, hard standing constraints) whose home is the
    /// actor's identity card — see [`ingest_placement_blueprint`], which
    /// routes a `high` fact there by overriding its `target_page`. `None` =
    /// unspecified (older rows / a producer that did not classify it).
    #[serde(default)]
    pub salience: Option<String>,
    /// Project-wiki pages this fact's turn authored, as plain `[[wiki_id/page]]`
    /// wikilinks (`fact_index.authored_refs`). Projected so the Cronista can
    /// emit a **reference** to the project page instead of restating the body —
    /// the "link, don't duplicate" provenance tube. Empty
    /// for a pure-standard fact.
    #[serde(default)]
    pub authored_refs: Vec<String>,
}

impl FactForPage {
    /// The plan-side projection of a `fact_index` row — the single mapping
    /// `gather_standard_facts` and the plan-sync seam
    /// ([`rehome_facts_in_persisted_plan`]) share, so a re-homed fact
    /// fingerprints identically to a gathered one.
    #[must_use]
    pub fn from_row(row: &fact_index::FactIndexRow) -> Self {
        Self {
            fact_id: row.fact_id.clone(),
            text: row.text.clone(),
            fact_type: row.fact_type.clone(),
            subject: row.subject_id.clone(),
            allow: row.allow_ids.clone(),
            sender: row.sender_id.clone(),
            source_wiki_id: row.wiki_id.clone(),
            valid_from: row.valid_from.clone(),
            valid_to: row.valid_to.clone(),
            decay_reason: row.decay_reason.clone(),
            successor_fact_id: row.successor_fact_id.clone(),
            target_page: row.target_page.clone(),
            style: row.style.clone(),
            page_description: row.page_description.clone(),
            salience: row.salience.clone(),
            authored_refs: row.authored_refs.clone(),
        }
    }
}

/// One page's plan record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagePlan {
    /// Stable slug — the plan key.
    pub slug: String,
    /// Human title.
    pub title: String,
    /// One-line description (shown to OTHER pages as the starvation index).
    pub description: String,
    /// Ingest-proposed page writing style (closed palette `prosa` |
    /// `prosa-tecnica` | `lista`) seeding the page's testata. Carried from
    /// the ingest classifier through [`NewPage`]/[`ConceptRegistryEntry`]; inert
    /// until the compiler consumes it. `None` = the Cronista
    /// decides the style at compile time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Kind of page.
    pub page_type: PageType,
    /// Group scope prose (`group_theme` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_scope: Option<String>,
    /// Parent hub slug (`concept_leaf` / person).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_hub: Option<String>,
    /// Child leaf slugs (hub / group), recomputed every Architetto run.
    #[serde(default)]
    pub child_leaves: Vec<String>,
    /// The facts whose single home is this page.
    #[serde(default)]
    pub primary_facts: Vec<FactForPage>,
    /// Outgoing wikilink slugs.
    #[serde(default)]
    pub outgoing_links: Vec<String>,
    /// Incoming wikilink slugs.
    #[serde(default)]
    pub incoming_links: Vec<String>,
    /// The standard wiki this page lives in (its tree home).
    pub wiki_id: String,
    /// The `.md` path within `wiki_id`. A foundation node uses its type's
    /// reserved page ([`PageType::foundation_page`]); **never
    /// [`crate::wiki::INDEX_FILENAME`]**, which no plan node may claim.
    pub page_path: String,
}

/// The persisted plan artifact (`wikis/_plan/compilation-plan.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilationPlan {
    /// All pages, keyed by slug (sorted for determinism).
    pub pages: BTreeMap<String, PagePlan>,
    /// GC removals + dedup redirects, for audit.
    #[serde(default)]
    pub merged_pages: Vec<MergedPage>,
    /// Bidirectional adjacency (wikilinks).
    #[serde(default)]
    pub link_graph: BTreeMap<String, Vec<String>>,
    /// Slugs in compile order (hubs → persons → leaves).
    #[serde(default)]
    pub compilation_order: Vec<String>,
    /// ISO-8601 build time.
    pub generated_at: String,
    /// Active fact count at build time.
    pub fact_count: usize,
    /// Slugs whose fingerprint changed since the prior plan (the recompile set).
    #[serde(default)]
    pub dirty_pages: Vec<String>,
    /// Slugs an out-of-band structural change (a REM split, a page merge)
    /// marked for recompile regardless of fingerprint drift — the persisted
    /// half of the plan-sync seam ([`rehome_facts_in_persisted_plan`]). After
    /// a re-home the carried-over fingerprint *matches* (the persisted plan
    /// already reflects the move), so without this flag the touched pages
    /// would never re-render. The next [`build_wiki_plan`] unions these into
    /// `dirty_pages` and clears the field.
    #[serde(default)]
    pub force_dirty: Vec<String>,
    /// Fact ids the dream reviewer nominated for the refile sweep — the
    /// reviewer→refile bridge (`cross_subject_bloat` findings become
    /// mechanical candidates; the refile judge still decides). Parked by
    /// [`park_bridge_signals`] after the post-compile review, **carried**
    /// across plan rebuilds, drained by REM's refile sweep
    /// (`take_refile_candidates`) — one judge pass per nomination, parked
    /// again only if the next review still finds it.
    #[serde(default)]
    pub refile_candidates: Vec<String>,
    /// Page slugs whose CARRIED placements re-open at the next
    /// [`build_wiki_plan`]: their facts leave the carry-over and flow
    /// through the Cartografo again, placement re-judged with the mass +
    /// identity signals live — the healing half the carried-placement
    /// model lacks (old misplacements, split-by-mass on a grown page).
    /// Parked by [`park_bridge_signals`] (reviewer findings, repeated
    /// compile failures), consumed + cleared by the next plan build.
    #[serde(default)]
    pub reopen_pages: Vec<String>,
}

/// One folded-away page (GC or dedup redirect), recorded for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedPage {
    /// Slug that was removed/redirected.
    pub from: String,
    /// Where it folded into (or `—`).
    pub into: String,
    /// Why.
    pub reason: String,
}

/// The persistent concept-page registry (`wikis/_plan/concept-registry.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptRegistry {
    /// Schema version.
    pub version: u32,
    /// Concept pages minted in prior runs, keyed by slug.
    #[serde(default)]
    pub entries: BTreeMap<String, ConceptRegistryEntry>,
    /// ISO-8601.
    pub generated_at: String,
}

impl ConceptRegistry {
    /// An empty registry stamped now.
    fn empty(now: &str) -> Self {
        Self {
            version: REGISTRY_VERSION,
            entries: BTreeMap::new(),
            generated_at: now.to_owned(),
        }
    }
}

/// One persisted concept page (`concept_hub` / `concept_leaf` only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptRegistryEntry {
    /// Slug.
    pub slug: String,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Ingest-proposed writing style. See [`PagePlan::style`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Hub or leaf.
    pub page_type: PageType,
    /// Parent hub slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_hub: Option<String>,
    /// The wiki the page lives in.
    pub wiki_id: String,
    /// ISO-8601 creation time.
    pub created_at: String,
}

const REGISTRY_VERSION: u32 = 1;

/// One fact→page assignment from the Cartografo.
#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    /// The fact.
    pub fact_id: String,
    /// The page it belongs on (raw slug — slugified/validated downstream).
    pub page_slug: String,
}

/// A new concept page the Cartografo proposes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewPage {
    /// Proposed slug.
    pub slug: String,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Ingest-proposed writing style. See [`PagePlan::style`].
    /// `None` from the Cartografo (it does not propose a style); set by
    /// [`ingest_placement_blueprint`] from the fact's `fact_index.style`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// `concept_hub` | `concept_leaf`.
    pub page_type: PageType,
    /// Parent hub slug (must be an existing or same-batch hub).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_hub: Option<String>,
}

/// The Cartografo's merged output across batches.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Blueprint {
    /// Fact→page assignments (incl. carried-over ones in incremental mode).
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    /// Proposed new concept pages.
    #[serde(default)]
    pub new_pages: Vec<NewPage>,
}

/// The Conciliatore's verdict.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConciliatorResult {
    /// `proposed_slug` → `existing_slug` (merge target).
    #[serde(default)]
    pub redirects: BTreeMap<String, String>,
    /// Genuinely-new pages to materialise.
    #[serde(default)]
    pub accepted_new: Vec<NewPage>,
}

/// Canonical slugify used everywhere: lowercase, runs of non-`[a-z0-9]` → `_`,
/// trimmed of leading/trailing `_`.
#[must_use]
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_owned()
}

/// Canonicalize an LLM-proposed page path through [`slugify`]: trim,
/// strip a trailing `.md`, slugify each `/` segment, re-join, re-append
/// `.md`. Returns `None` when any segment slugifies to nothing.
///
/// This is the single chokepoint for every page name an LLM invents —
/// the ingest classifier's `target_page` and the REM auto-promote's
/// recommended target both pass through here, so the same concept can
/// never materialise twice under spelling variants (`lista-spesa` vs
/// `Lista spesa`).
#[must_use]
pub fn canonical_page_path(raw: &str) -> Option<String> {
    let stem = raw.trim();
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    let mut segments = Vec::new();
    for part in stem.split('/') {
        let slug = slugify(part);
        if slug.is_empty() {
            return None;
        }
        segments.push(slug);
    }
    Some(format!("{}.md", segments.join("/")))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

// ---------- Stadio 0 — Il Fonditore ----------

/// Build the foundation pages (deterministic, no LLM).
///
/// A `person` page per enrolled user and a `group_theme` hub per group, wired by
/// group membership. Returns the pages keyed by slug plus the per-group scope
/// strings.
///
/// # Errors
///
/// DB errors.
pub async fn build_foundation_pages(
    pool: &SqlitePool,
    tree: &WikiTree,
) -> Result<(BTreeMap<String, PagePlan>, BTreeMap<String, String>)> {
    let mut pages: BTreeMap<String, PagePlan> = BTreeMap::new();
    let mut group_scopes: BTreeMap<String, String> = BTreeMap::new();

    // GROUP PAGES first (persons may link to them).
    let groups = enrollment::list_groups(pool).await?;
    for g in &groups {
        let slug = slugify(&g.group_id);
        if slug.is_empty() {
            continue;
        }
        let scope = g.scope.clone().filter(|s| !s.trim().is_empty());
        if let Some(s) = &scope {
            group_scopes.insert(slug.clone(), s.clone());
        }
        pages.insert(
            slug.clone(),
            PagePlan {
                title: capitalize(&g.group_id),
                description: format!("Group {}", g.group_id),
                style: None,
                page_type: PageType::GroupTheme,
                owner_scope: scope,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: g.group_id.clone(),
                page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
                slug,
            },
        );
    }

    // PERSON PAGES. parent_hub = the user's first group (if it is a known group).
    let users = enrollment::list_users(pool).await?;
    for u in &users {
        let slug = slugify(&u.user_id);
        if slug.is_empty() {
            continue;
        }
        // Skip a person whose slug collides with a group (slug-collision guard).
        if pages
            .get(&slug)
            .is_some_and(|p| p.page_type == PageType::GroupTheme)
        {
            tracing::warn!(
                slug,
                "planner: person slug collides with a group page, skipping person"
            );
            continue;
        }
        let groups_of = enrollment::groups_with_scope_for(pool, &u.user_id).await?;
        let mut parent_hub = None;
        let mut outgoing_links: Vec<String> = Vec::new();
        for (gid, _) in &groups_of {
            let gslug = slugify(gid);
            if pages.contains_key(&gslug) {
                if parent_hub.is_none() {
                    parent_hub = Some(gslug.clone());
                }
                if !outgoing_links.contains(&gslug) {
                    outgoing_links.push(gslug);
                }
            }
        }
        pages.insert(
            slug.clone(),
            PagePlan {
                title: capitalize(&u.user_id),
                description: format!("Personal page of {}", capitalize(&u.user_id)),
                style: None,
                page_type: PageType::Person,
                owner_scope: None,
                parent_hub,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links,
                incoming_links: Vec::new(),
                wiki_id: u.user_id.clone(),
                page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
                slug,
            },
        );
    }

    let buffers = seed_wiki_buffers(tree, &mut pages)?;

    tracing::info!(
        groups = groups.len(),
        users = users.len(),
        buffers,
        pages = pages.len(),
        "planner: foundation pages built"
    );
    Ok((pages, group_scopes))
}

/// The Fonditore's third source — THE PER-WIKI BUFFER PAGE.
///
/// Every standard (non-smart) wiki gets a [`PageType::WikiBuffer`] foundation
/// node on its [`crate::wiki::NOTES_FILENAME`]: the landing page for a fact
/// with no better home. It is plan-owned — compiled from the DB like every
/// other page and never garbage-collected — so a wiki always has somewhere to
/// put a fact, and REM's reorg sweep always has somewhere to drain from.
///
/// The key is [`buffer_slug`] on **every** wiki, carded or not — see there for
/// why it cannot depend on whether the wiki has a card.
///
/// **No node points at [`crate::wiki::INDEX_FILENAME`]** — see
/// [`PageType::foundation_page`]. Returns how many nodes it seeded.
fn seed_wiki_buffers(tree: &WikiTree, pages: &mut BTreeMap<String, PagePlan>) -> Result<usize> {
    let mut seeded = 0usize;
    for d in tree.walk()? {
        if d.meta.smart {
            continue;
        }
        let wiki_slug = slugify(d.meta.wiki_id.as_str());
        if wiki_slug.is_empty() {
            continue;
        }
        let slug = buffer_slug(&wiki_slug);
        if pages.contains_key(&slug) {
            tracing::warn!(
                slug,
                wiki_id = d.meta.wiki_id.as_str(),
                "planner: wiki buffer slug collides with an enrollment foundation page, skipping"
            );
            continue;
        }
        // Hang the buffer under its own wiki's card when there is one, else
        // under the parent wiki's foundation node.
        let has_card = pages
            .get(&wiki_slug)
            .is_some_and(|p| p.page_type.is_identity_card());
        let parent_hub = if has_card {
            Some(wiki_slug.clone())
        } else {
            d.meta
                .parent_wiki_id
                .as_ref()
                .map(|p| slugify(p.as_str()))
                .filter(|p| pages.contains_key(p))
        };
        pages.insert(
            slug.clone(),
            PagePlan {
                title: d.meta.title.clone(),
                description: d.meta.scope.clone().unwrap_or_default(),
                style: None,
                page_type: PageType::WikiBuffer,
                owner_scope: None,
                parent_hub,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: d.meta.wiki_id.as_str().to_owned(),
                page_path: crate::wiki::NOTES_FILENAME.to_owned(),
                slug,
            },
        );
        seeded += 1;
    }
    Ok(seeded)
}

// ---------- Stadio 2 — L'Architetto ----------

/// Build the [`CompilationPlan`] (deterministic).
///
/// Materialises foundation + registry + accepted-new pages, applies assignments
/// (with redirects) under the one-fact-one-page rule, deterministically homes
/// orphan facts, computes the hub→leaf graph, **fixpoint** garbage-collects
/// empty concept pages, builds the symmetric link graph, and orders
/// hubs-before-leaves. Returns the plan and the updated registry.
#[must_use]
#[allow(clippy::too_many_lines)] // the Architetto reads top-to-bottom; splitting hides the flow
pub fn build_compilation_plan(
    facts: &[FactForPage],
    foundation: &BTreeMap<String, PagePlan>,
    blueprint: &Blueprint,
    conciliation: &ConciliatorResult,
    registry: &ConceptRegistry,
    now: &str,
) -> (CompilationPlan, ConceptRegistry) {
    // group scopes ride on each group_theme page's `owner_scope` (set by the
    // Fonditore and preserved when foundation pages are seeded below).
    let mut pages: BTreeMap<String, PagePlan> = BTreeMap::new();
    let mut merged: Vec<MergedPage> = Vec::new();
    let mut updated_registry = ConceptRegistry {
        version: REGISTRY_VERSION,
        entries: registry.entries.clone(),
        generated_at: now.to_owned(),
    };

    // Pre-pass (Option C — forest model, no root wiki): the source wiki each page
    // slug's facts live in. A concept page is homed where its facts are; the
    // retired `root` wiki is gone (see `resolve_page_wiki`).
    let fact_map: BTreeMap<&str, &FactForPage> =
        facts.iter().map(|f| (f.fact_id.as_str(), f)).collect();
    let mut slug_source_wiki: BTreeMap<String, String> = BTreeMap::new();
    for a in &blueprint.assignments {
        let Some(fact) = fact_map.get(a.fact_id.as_str()) else {
            continue;
        };
        let mut slug = slugify(&a.page_slug);
        if let Some(redir) = conciliation.redirects.get(&slug) {
            slug = slugify(redir);
        }
        slug_source_wiki
            .entry(slug)
            .or_insert_with(|| fact.source_wiki_id.clone());
    }
    // Migration: re-home (or drop) any carried-over registry entry still pinned to
    // the retired root wiki, so a pre-C plan does not resurrect it.
    updated_registry.entries.retain(|slug, e| {
        if e.wiki_id != crate::types::WikiId::ROOT {
            return true;
        }
        match resolve_page_wiki(
            slug,
            e.parent_hub.as_deref(),
            foundation,
            registry,
            &slug_source_wiki,
        ) {
            Some(w) => {
                e.wiki_id = w;
                true
            },
            None => false,
        }
    });
    // Staleness GC: an entry whose slug a foundation page owns can never
    // materialise again (step 2 below skips it every run) — it only lingers
    // as a stale redirect/reuse target the conciliator keeps seeing (the
    // enrolled `matteo` wiki shadowing an old `matteo` concept leaf). Drop
    // it: the foundation page wins.
    updated_registry.entries.retain(|slug, e| {
        if foundation.contains_key(slug) {
            tracing::info!(
                slug = %slug,
                wiki_id = %e.wiki_id,
                "planner: dropped registry entry shadowed by a foundation page"
            );
            false
        } else {
            true
        }
    });

    // 1. seed foundation (preserve nothing yet — foundation holds no DB facts).
    for (slug, p) in foundation {
        let mut np = p.clone();
        np.child_leaves.clear();
        np.primary_facts.clear();
        np.incoming_links.clear();
        pages.insert(slug.clone(), np);
    }

    // 2. materialise registry concept pages (foundation overrides registry).
    for (slug, e) in &updated_registry.entries {
        if pages.contains_key(slug) {
            continue;
        }
        pages.insert(slug.clone(), registry_to_page(e));
    }

    // 3. materialise accepted-new concept pages.
    for np in &conciliation.accepted_new {
        let slug = slugify(&np.slug);
        if slug.is_empty() || pages.contains_key(&slug) {
            continue;
        }
        // Option C: home the page in its facts' source wiki (else a factless
        // hub's parent wiki); skip a homeless page rather than minting a root.
        let Some(wiki_id) = resolve_page_wiki(
            &slug,
            np.parent_hub.as_deref(),
            foundation,
            registry,
            &slug_source_wiki,
        ) else {
            continue;
        };
        let entry = ConceptRegistryEntry {
            slug: slug.clone(),
            title: np.title.clone(),
            description: np.description.clone(),
            style: np.style.clone(),
            page_type: np.page_type,
            parent_hub: np.parent_hub.as_deref().map(slugify),
            wiki_id: wiki_id.clone(),
            created_at: now.to_owned(),
        };
        pages.insert(slug.clone(), new_page_to_plan(np, &slug, &wiki_id));
        updated_registry.entries.insert(slug, entry);
    }

    // 4. apply assignments (one-fact-one-page), with redirects. `fact_map` was
    // built once in the pre-pass above.
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    for a in &blueprint.assignments {
        let Some(fact) = fact_map.get(a.fact_id.as_str()) else {
            continue; // superseded/removed since classification — skip.
        };
        // Canonicalise an LLM-proposed slug, but never a key the plan
        // already holds: a foundation node's key may legitimately be one
        // `slugify` would rewrite (a buffer's `__` separator collapses to a
        // single `_`), and rewriting it mints a phantom leaf beside the real
        // page and splits the fact off from it.
        let mut slug = if pages.contains_key(&a.page_slug) {
            a.page_slug.clone()
        } else {
            slugify(&a.page_slug)
        };
        if let Some(redir) = conciliation.redirects.get(&slug) {
            slug = slugify(redir);
        }
        if !pages.contains_key(&slug) {
            // Never mint a reserved stem. The foundation nodes are keyed by
            // [`plan_slug_for_page`] — the card takes the wiki's own slug and
            // the buffer takes `<wiki>__notes` — so a bare `notes` or
            // `profile` misses the lookup above and mints a SECOND plan page
            // on the file the buffer or the card already owns. Drop the
            // assignment instead; the orphan pass below homes the fact on a
            // page that exists.
            if crate::wiki::is_reserved_page_stem(&slug) {
                tracing::warn!(
                    slug = %slug,
                    fact_id = %fact.fact_id,
                    "planner: assignment names a reserved page — dropped, the fact falls back"
                );
                continue;
            }
            // Fallback: mint a concept_leaf on the fly so the fact has a home.
            let wiki_id = fact.source_wiki_id.clone();
            let title = capitalize(&slug.replace('_', " "));
            pages.insert(
                slug.clone(),
                PagePlan {
                    title: title.clone(),
                    description: String::new(),
                    style: None,
                    page_type: PageType::ConceptLeaf,
                    owner_scope: None,
                    parent_hub: None,
                    child_leaves: Vec::new(),
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    incoming_links: Vec::new(),
                    wiki_id: wiki_id.clone(),
                    page_path: format!("{slug}.md"),
                    slug: slug.clone(),
                },
            );
            updated_registry
                .entries
                .entry(slug.clone())
                .or_insert_with(|| ConceptRegistryEntry {
                    slug: slug.clone(),
                    title,
                    description: String::new(),
                    style: None,
                    page_type: PageType::ConceptLeaf,
                    parent_hub: None,
                    wiki_id,
                    created_at: now.to_owned(),
                });
        }
        if let Some(page) = pages.get_mut(&slug) {
            page.primary_facts.push((*fact).clone());
            assigned.insert(fact.fact_id.as_str().to_owned());
        }
    }

    // 5. orphan fallback (deterministic): subject's person page, else the fact's
    // source wiki's foundation page, else skip (never an arbitrary page).
    for f in facts {
        if assigned.contains(f.fact_id.as_str()) {
            continue;
        }
        let target = orphan_target(f, &pages);
        if let Some(slug) = target
            && let Some(page) = pages.get_mut(&slug)
        {
            page.primary_facts.push(f.clone());
        } else {
            tracing::warn!(fact_id = %f.fact_id, "planner: orphan fact has no home page, dropped from plan");
        }
    }

    // 6. heal style-less registry entries from their facts' majority style.
    // The conciliation-time backfill (`backfill_accepted_new_style`) protects
    // only pages accepted THIS run; an entry already persisted with
    // `style: None` is skipped by step 3 forever, so without this repair a
    // record page (e.g. `lista`) stays demoted to full-prose compilation.
    // When such an entry's page carries a strict majority of non-empty
    // per-fact `fact_index.style` proposals agreeing on one style
    // (normalized to the compiler's closed palette), adopt it on both the
    // registry entry and this plan's page. Idempotent: once the entry has a
    // style, the heal never re-fires.
    for (slug, page) in &mut pages {
        if let Some(entry) = updated_registry.entries.get_mut(slug)
            && entry.style.is_none()
            && let Some(style) = majority_fact_style(&page.primary_facts)
        {
            tracing::info!(
                slug = %slug,
                style,
                "planner: healed style-less registry entry from its facts' majority style"
            );
            entry.style = Some(style.to_owned());
            page.style = Some(style.to_owned());
        }
    }

    // 6.bis heal dangling parents: a `parent_hub` naming no plan page would
    // silently skip step 7 (the parent lookup fails, the child joins no
    // `child_leaves`) and survive as a broken pointer on the compiled page —
    // the shape an absorbed/GC'd hub leaves behind. Re-point to the page's
    // own wiki foundation page when the plan has one, else clear. The
    // registry entry heals too, or the same pointer resurrects every build.
    // Prefer the wiki's card; a topic wiki has only its buffer. (`BTreeMap`
    // iterates by slug, and a carded wiki's buffer sorts after its card under
    // the `__notes` suffix, so the card is inserted first and kept.)
    let mut foundation_by_wiki: BTreeMap<String, String> = BTreeMap::new();
    for (slug, p) in &pages {
        if !p.page_type.is_foundation() {
            continue;
        }
        let entry = foundation_by_wiki.entry(p.wiki_id.clone());
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(slug.clone());
            },
            std::collections::btree_map::Entry::Occupied(mut o)
                if p.page_type.is_identity_card() =>
            {
                o.insert(slug.clone());
            },
            std::collections::btree_map::Entry::Occupied(_) => {},
        }
    }
    let known_slugs: BTreeSet<String> = pages.keys().cloned().collect();
    for (slug, page) in &mut pages {
        if let Some(h) = &page.parent_hub
            && !known_slugs.contains(h)
        {
            let heal = foundation_by_wiki.get(&page.wiki_id).cloned();
            tracing::info!(
                slug = %slug,
                dangling = %h,
                healed_to = heal.as_deref().unwrap_or("—"),
                "planner: healed dangling parent_hub"
            );
            page.parent_hub.clone_from(&heal);
            if let Some(e) = updated_registry.entries.get_mut(slug) {
                e.parent_hub = heal;
            }
        }
    }

    // 7. parent → child.
    let parents: Vec<(String, String)> = pages
        .iter()
        .filter_map(|(slug, p)| p.parent_hub.clone().map(|h| (slug.clone(), h)))
        .collect();
    for (slug, hub) in parents {
        if let Some(parent) = pages.get_mut(&hub)
            && !parent.child_leaves.contains(&slug)
        {
            parent.child_leaves.push(slug);
        }
    }
    for p in pages.values_mut() {
        p.child_leaves.sort();
    }

    // 8. FIXPOINT garbage collection of empty concept pages (the TS single-pass
    // bug fix): repeat until no removals — an empty hub whose only child is a
    // removed empty leaf must also go.
    loop {
        // **A page is never promoted into a container.** Until 2026-08-04 an
        // emptied leaf that other pages parented under was flipped to a
        // `ConceptHub` right here rather than removed — the planner's own
        // second route to minting one, beside the Cartografo's, and the
        // reason twelve of them exist. Founder's ruling: *«un contenitore è
        // una wiki»*. A grouping deep enough to need its own container is a
        // wiki, and wikis are raised by the promote machinery, which is
        // visible. So the emptied page stays a leaf and the sweep below
        // collects it like any other; `reparent_to_foundation` re-homes its
        // children first, which is what the flip used to be protecting
        // against.
        let to_remove: Vec<String> = pages
            .iter()
            .filter(|(_, p)| !p.page_type.is_foundation())
            .filter(|(_, p)| match p.page_type {
                PageType::ConceptLeaf => p.primary_facts.is_empty(),
                // Legacy only: no new one is minted (see above), and an
                // existing one goes as soon as its children are re-homed.
                PageType::ConceptHub => p.child_leaves.is_empty(),
                _ => false,
            })
            .map(|(slug, _)| slug.clone())
            .collect();
        // A page about to go must not take its children's parent with it.
        reparent_to_foundation(&mut pages, &to_remove);
        if to_remove.is_empty() {
            break;
        }
        for slug in to_remove {
            let reason = match pages.get(&slug).map(|p| p.page_type) {
                Some(PageType::ConceptLeaf) => "concept_leaf with 0 facts",
                _ => "concept_hub with 0 children",
            };
            let parent = pages.get(&slug).and_then(|p| p.parent_hub.clone());
            merged.push(MergedPage {
                from: slug.clone(),
                into: parent.clone().unwrap_or_else(|| "—".to_owned()),
                reason: reason.to_owned(),
            });
            if let Some(h) = &parent
                && let Some(parent_page) = pages.get_mut(h)
            {
                parent_page.child_leaves.retain(|c| c != &slug);
            }
            pages.remove(&slug);
            updated_registry.entries.remove(&slug);
        }
    }

    // 9. directed link graph (hub→child + foundation outgoing), then symmetric.
    let mut link_graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (slug, page) in &pages {
        let entry = link_graph.entry(slug.clone()).or_default();
        for child in &page.child_leaves {
            if pages.contains_key(child) && !entry.contains(child) {
                entry.push(child.clone());
            }
        }
        for link in &page.outgoing_links {
            if link != slug && pages.contains_key(link) && !entry.contains(link) {
                entry.push(link.clone());
            }
        }
    }
    // make symmetric.
    let edges: Vec<(String, String)> = link_graph
        .iter()
        .flat_map(|(s, ts)| ts.iter().map(move |t| (s.clone(), t.clone())))
        .collect();
    for (s, t) in edges {
        let back = link_graph.entry(t).or_default();
        if !back.contains(&s) {
            back.push(s);
        }
    }
    for links in link_graph.values_mut() {
        links.sort();
    }
    // sync onto pages.
    for (slug, page) in &mut pages {
        page.outgoing_links = link_graph.get(slug).cloned().unwrap_or_default();
        page.incoming_links = page.outgoing_links.clone(); // symmetric ⇒ equal
    }

    // 9.bis the map invariant, checked where the plan is sealed rather than
    // trusted at each of the places that mint a page. Nothing the compiler
    // places may land on a wiki's map: that page answers *where does a fact
    // belong*, it holds no facts, and the read path never opens it — so a
    // fact placed there is one no navigation route can reach and no sweep
    // will drain (founder's ruling, 2026-08-03). Loud rather than fatal: a
    // mis-keyed page is a bug to fix, not a reason to abandon the night's
    // compile.
    for (slug, p) in &pages {
        if p.page_path == crate::wiki::INDEX_FILENAME {
            tracing::error!(
                slug = %slug,
                wiki_id = %p.wiki_id,
                page_type = page_type_tag(p.page_type),
                "planner: a plan page points at the wiki's MAP — facts placed there are unreachable"
            );
        } else if let Some(expected) = p.page_type.foundation_page()
            && p.page_path != expected
        {
            tracing::error!(
                slug = %slug,
                wiki_id = %p.wiki_id,
                page_type = page_type_tag(p.page_type),
                page_path = %p.page_path,
                expected,
                "planner: foundation node is not on its reserved page"
            );
        }
    }

    // 10. compilation order: hubs → persons → leaves, then slug for stability.
    let mut order: Vec<String> = pages.keys().cloned().collect();
    order.sort_by(|a, b| {
        let ra = pages[a].page_type.order_rank();
        let rb = pages[b].page_type.order_rank();
        ra.cmp(&rb).then_with(|| a.cmp(b))
    });

    let fact_count = facts.len();
    let plan = CompilationPlan {
        pages,
        merged_pages: merged,
        link_graph,
        compilation_order: order.clone(),
        generated_at: now.to_owned(),
        fact_count,
        dirty_pages: order, // overwritten by the caller via compute_dirty_pages
        force_dirty: Vec::new(),
        refile_candidates: Vec::new(),
        reopen_pages: Vec::new(),
    };
    (plan, updated_registry)
}

fn registry_to_page(e: &ConceptRegistryEntry) -> PagePlan {
    // Concept pages (hub OR leaf) are `<slug>.md` pages WITHIN their wiki — a
    // wiki's reserved pages (its card, its buffer, its map) are foundation
    // nodes or nobody's, never concept pages, and `placement_slug` refuses
    // their names so one can never be minted here.
    let page_path = format!("{}.md", e.slug);
    PagePlan {
        slug: e.slug.clone(),
        title: e.title.clone(),
        description: e.description.clone(),
        style: e.style.clone(),
        page_type: e.page_type,
        owner_scope: None,
        parent_hub: e.parent_hub.clone(),
        child_leaves: Vec::new(),
        primary_facts: Vec::new(),
        outgoing_links: Vec::new(),
        incoming_links: Vec::new(),
        wiki_id: e.wiki_id.clone(),
        page_path,
    }
}

fn new_page_to_plan(np: &NewPage, slug: &str, wiki_id: &str) -> PagePlan {
    // Concept pages live at `<slug>.md` within their wiki (see registry_to_page).
    let page_path = format!("{slug}.md");
    PagePlan {
        slug: slug.to_owned(),
        title: np.title.clone(),
        description: np.description.clone(),
        style: np.style.clone(),
        page_type: np.page_type,
        owner_scope: None,
        parent_hub: np.parent_hub.as_deref().map(slugify),
        child_leaves: Vec::new(),
        primary_facts: Vec::new(),
        outgoing_links: Vec::new(),
        incoming_links: Vec::new(),
        wiki_id: wiki_id.to_owned(),
        page_path,
    }
}

/// The strict-majority writing style among a page's facts' non-empty
/// `fact_index.style` proposals, each normalized to the compiler's closed
/// palette ([`crate::compiler::normalize_style`]) before the vote. `None`
/// when no fact carries a style or no single style wins more than half of
/// the non-empty votes.
fn majority_fact_style(facts: &[FactForPage]) -> Option<&'static str> {
    let mut votes: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut total = 0_usize;
    for f in facts {
        let Some(s) = f.style.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        *votes
            .entry(crate::compiler::normalize_style(Some(s)))
            .or_default() += 1;
        total += 1;
    }
    votes
        .into_iter()
        .find(|&(_, n)| n * 2 > total)
        .map(|(style, _)| style)
}

/// Resolve the wiki a concept page lives in (Option C — forest model).
///
/// A page is homed in **its facts' source wiki** — the invariant that a fact's
/// region lives in the fact's own wiki, which also keeps `fact_index.wiki_id`
/// and the compiled `source_path` in the same wiki. A factless concept *hub*
/// falls back to its parent hub's wiki. Returns `None` when neither resolves —
/// a homeless, factless page the caller skips. Never resolves to the retired
/// `root` wiki (mwe-mcp's wiki tree is a forest of top-level wikis, with no
/// single materialised root).
fn resolve_page_wiki(
    slug: &str,
    parent_hub: Option<&str>,
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    slug_source_wiki: &BTreeMap<String, String>,
) -> Option<String> {
    let candidate = if let Some(w) = slug_source_wiki.get(slug) {
        Some(w.clone())
    } else {
        let hs = slugify(parent_hub?);
        foundation
            .get(&hs)
            .map(|p| p.wiki_id.clone())
            .or_else(|| registry.entries.get(&hs).map(|e| e.wiki_id.clone()))
    };
    candidate.filter(|w| w != crate::types::WikiId::ROOT)
}

/// Deterministic orphan home — the subject's wiki first, then the fact's source
/// wiki; `None` when neither has a foundation node.
///
/// **Which page of that wiki depends on the fact, not only on the wiki.** Two
/// different things arrive here and they do not belong together:
///
/// - a `salience: "high"` fact is always-on material the classifier
///   *reserved* — identity, health/safety, a hard standing constraint — and
///   its home is the wiki's **identity card**
///   ([`crate::wiki::PROFILE_FILENAME`]). This is the whole reason
///   [`ingest_placement_blueprint`] leaves it unassigned rather than honouring
///   its proposed page.
/// - anything else that reached the fallback simply has no page yet, and its
///   home is the wiki's **buffer** ([`crate::wiki::NOTES_FILENAME`]), from
///   which REM's reorg sweep lifts it onto a real page.
///
/// Sending both to one page is not a smaller version of this: it either buries
/// the card under unsorted facts or promotes every unplaced fact to identity.
/// A wiki with no card (a topic wiki) takes the buffer for both — a topic has
/// no identity to reserve.
fn orphan_target(f: &FactForPage, pages: &BTreeMap<String, PagePlan>) -> Option<String> {
    let identity = f.salience.as_deref() == Some("high");
    let subject_slug = match &f.subject {
        // The builtin global group has no subject page to home an orphan on.
        p if p.is_global() => String::new(),
        Principal::User(id) | Principal::Group(id) => slugify(id),
    };
    let src_slug = slugify(&f.source_wiki_id);
    for wiki_slug in [subject_slug, src_slug] {
        if wiki_slug.is_empty() {
            continue;
        }
        if let Some(slug) = foundation_slug_for(&wiki_slug, identity, pages) {
            return Some(slug);
        }
    }
    None
}

/// The foundation slug a fact should land on within the wiki keyed
/// `wiki_slug` — its card when `identity`, else its buffer. Falls back to
/// whichever of the two the wiki actually has (a topic wiki has no card).
fn foundation_slug_for(
    wiki_slug: &str,
    identity: bool,
    pages: &BTreeMap<String, PagePlan>,
) -> Option<String> {
    let card = pages
        .get(wiki_slug)
        .filter(|p| p.page_type.is_identity_card())
        .map(|_| wiki_slug.to_owned());
    let buf = buffer_slug(wiki_slug);
    let buffer = pages
        .get(&buf)
        .filter(|p| p.page_type == PageType::WikiBuffer)
        .map(|_| buf);
    if identity {
        card.or(buffer)
    } else {
        buffer.or(card)
    }
}

/// Re-home the children of every page about to be garbage-collected onto
/// their wiki's foundation page, so a removal never orphans a `parent_hub`.
///
/// Needed since the planner stopped promoting an emptied leaf into a
/// container (founder, 2026-08-04: *«un contenitore è una wiki»*). Before
/// that a leaf with children could not be collected — it became a
/// `ConceptHub` and lived on — so the question never arose. Now it is
/// collected like any other empty page, and its children have to land
/// somewhere first.
///
/// The wiki's own foundation is the only safe destination: it always exists,
/// it belongs to the same wiki (so nothing crosses an ACL boundary), and a
/// page parented there is exactly a page with no grouping above it. A child
/// whose wiki resolves to no foundation at all keeps its old parent — a
/// dangling `parent_hub` is inert, an invented one is not.
fn reparent_to_foundation(pages: &mut BTreeMap<String, PagePlan>, doomed: &[String]) {
    if doomed.is_empty() {
        return;
    }
    let doomed: BTreeSet<&str> = doomed.iter().map(String::as_str).collect();
    let snapshot = pages.clone();
    for page in pages.values_mut() {
        let Some(parent) = page.parent_hub.as_deref() else {
            continue;
        };
        if !doomed.contains(parent) {
            continue;
        }
        let wiki_slug = slugify(&page.wiki_id);
        let Some(foundation) = foundation_slug_for(&wiki_slug, false, &snapshot) else {
            continue;
        };
        tracing::info!(
            slug = %page.slug,
            from = %parent,
            to = %foundation,
            "planner: child re-homed onto its wiki's foundation — its parent page is going"
        );
        page.parent_hub = Some(foundation);
    }
    // Drop the stale child links from whatever still names them.
    for page in pages.values_mut() {
        page.child_leaves.retain(|c| !doomed.contains(c.as_str()));
    }
}

// ---------- fingerprint + dirty set ----------

/// Deterministic 64-bit FNV-1a over `bytes`.
///
/// Used to fold a fact's claim text into [`page_fingerprint`] so a content
/// correction is detected. Must stay stable across runs (the fingerprint is
/// persisted in the plan and compared next cycle), which rules out the
/// randomised `std` hasher.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The bytes of a fact that REACH THE RENDERED PAGE — the per-fact input
/// of [`page_fingerprint`] and the drift check.
///
/// Claim text plus the validity fields (`valid_from` / `valid_to` /
/// `decay_reason`) plus the successor pointer: the Cronista renders all of
/// them, so a change to any must recompile the page. Before the closure
/// verb existed the validity was immutable after promotion and `text`
/// alone sufficed; closing a fact now mutates `valid_to`/`decay_reason`
/// (and possibly `successor_fact_id`) in place, and a text-only key would
/// leave the prose announcing an open item forever.
fn fact_render_key(f: &FactForPage) -> String {
    // The ACL is part of the key because the Cronista's tagging now depends on
    // it: a restricted fact gets an `(audience: …)` hint and its prose is kept
    // inside its `<fN>` span (see `compiler::audience_hint`), so the compiled
    // page content is a function of subject/allow/sender. An ACL-only change must
    // therefore re-dirty the page so the next compile re-tags. `allow` is sorted
    // so a pure reordering is not a spurious change.
    let mut allow: Vec<String> = f.allow.iter().map(ToString::to_string).collect();
    allow.sort_unstable();
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        f.text,
        f.valid_from.as_deref().unwrap_or(""),
        f.valid_to.as_deref().unwrap_or(""),
        f.decay_reason.as_deref().unwrap_or(""),
        f.successor_fact_id.as_ref().map_or("", |s| s.as_str()),
        f.subject,
        allow.join(","),
        f.sender.as_ref().map_or(String::new(), ToString::to_string),
    )
}

/// Per-page fingerprint over content AND topology.
///
/// Captures fact ids + their render content ([`fact_render_key`]: claim
/// text + validity fields), plus links/parent/children, so a page goes
/// dirty when a fact is added/removed, when an existing fact's claim text
/// or validity changes, or when its link neighbourhood changes. Exactly:
/// `factId:hash,…|outgoing|parentHub|childLeaves`, each list sorted.
/// (Folding validity into the hash changed the fingerprint format — one
/// full recompile per wiki on the first plan build after the change.)
#[must_use]
pub fn page_fingerprint(p: &PagePlan) -> String {
    // Each fact contributes `id:<content-hash>` rather than the bare id, so an
    // in-place claim correction (same `fact_id`, new text — the shape a
    // dashboard comment produces), a validity closure (same id, new
    // `valid_to`/`decay_reason`), or an ACL change (same id, new
    // subject/allow/sender — which now steers the Cronista's tagging) flips the
    // fingerprint and marks the page dirty. A fact-id-only fingerprint would
    // miss them all: they keep the id.
    let mut facts: Vec<String> = p
        .primary_facts
        .iter()
        .map(|f| {
            format!(
                "{}:{:016x}",
                f.fact_id.as_str(),
                fnv1a64(fact_render_key(f).as_bytes())
            )
        })
        .collect();
    facts.sort_unstable();
    let mut out = p.outgoing_links.clone();
    out.sort();
    let mut children = p.child_leaves.clone();
    children.sort();
    format!(
        "{}|{}|{}|{}",
        facts.join(","),
        out.join(","),
        p.parent_hub.as_deref().unwrap_or(""),
        children.join(",")
    )
}

/// The recompile set: pages new, fingerprint-changed, or type-changed in
/// `next`, plus pages removed since `prev`.
///
/// Removed pages ride along so the compiler can delete their `.md`. The
/// type check rides beside the fingerprint (not inside it — that would
/// flip every stored fingerprint at once and recompile the world): a leaf
/// normalised to hub renders through a different writer, so it must go
/// dirty even when facts/links/children are unchanged.
#[must_use]
pub fn compute_dirty_pages(prev: &CompilationPlan, next: &CompilationPlan) -> Vec<String> {
    let prev_fp: BTreeMap<&String, (String, PageType)> = prev
        .pages
        .iter()
        .map(|(s, p)| (s, (page_fingerprint(p), p.page_type)))
        .collect();
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    for (slug, p) in &next.pages {
        let fp = page_fingerprint(p);
        match prev_fp.get(slug) {
            Some((prev, ptype)) if *prev == fp && *ptype == p.page_type => {},
            _ => {
                dirty.insert(slug.clone());
            },
        }
    }
    for slug in prev.pages.keys() {
        if !next.pages.contains_key(slug) {
            dirty.insert(slug.clone());
        }
    }
    dirty.into_iter().collect()
}

// ---------- persistence (wikis/_plan/*.json) ----------

fn plan_dir(tree: &WikiTree) -> std::path::PathBuf {
    tree.workdir().join(crate::wiki::WIKIS_DIR).join("_plan")
}

/// Load the previous plan, or `None` when absent/invalid.
///
/// # Errors
///
/// IO errors other than not-found.
pub fn load_previous_plan(tree: &WikiTree) -> Result<Option<CompilationPlan>> {
    let path = plan_dir(tree).join("compilation-plan.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<CompilationPlan>(&s) {
            Ok(p) if !p.pages.is_empty() || !p.compilation_order.is_empty() => Ok(Some(p)),
            _ => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Load the concept registry, or an empty one when absent/invalid.
///
/// # Errors
///
/// IO errors other than not-found.
pub fn load_concept_registry(tree: &WikiTree, now: &str) -> Result<ConceptRegistry> {
    let path = plan_dir(tree).join("concept-registry.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(serde_json::from_str::<ConceptRegistry>(&s)
            .unwrap_or_else(|_| ConceptRegistry::empty(now))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConceptRegistry::empty(now)),
        Err(e) => Err(e.into()),
    }
}

/// Persist the plan (crash-safe atomic write).
///
/// # Errors
///
/// IO / JSON errors.
pub fn save_plan(tree: &WikiTree, plan: &CompilationPlan) -> Result<()> {
    let path = plan_dir(tree).join("compilation-plan.json");
    let bytes = serde_json::to_vec_pretty(plan)?;
    atomic_write(&path, &bytes)?;
    Ok(())
}

/// Identity seed for a re-home destination page the plan may not know yet.
///
/// Used by [`rehome_facts_in_persisted_plan`] for a REM split target or a
/// merge survivor. The page is always the single-segment concept-leaf form
/// (`page_path = <slug>.md`) — the only shape the concept registry can
/// re-materialise.
#[derive(Debug, Clone)]
pub struct RehomePageSeed {
    /// Destination page slug (slugified before use).
    pub slug: String,
    /// Human title.
    pub title: String,
    /// One-line description (empty lets the Cronista author one at compile).
    pub description: String,
    /// Optional testata style.
    pub style: Option<String>,
    /// The standard wiki the page lives in.
    pub wiki_id: String,
    /// Wiki-relative page path override; `None` = `<slug>.md` (the
    /// concept-leaf shape). A reserved page (card / buffer) sets it
    /// explicitly.
    pub page_path: Option<String>,
}

impl RehomePageSeed {
    /// A concept-leaf seed derived from a slug alone (title from the slug,
    /// empty description, no style) — the REM split's shape.
    #[must_use]
    pub fn concept(slug: &str, wiki_id: &str) -> Self {
        let slug = slugify(slug);
        Self {
            title: capitalize(&slug.replace('_', " ")),
            slug,
            description: String::new(),
            style: None,
            wiki_id: wiki_id.to_owned(),
            page_path: None,
        }
    }

    /// A seed for an **arbitrary wiki-relative page in a named wiki** —
    /// the `fact_refile` cross-wiki plan re-home. `page_path` is pinned to
    /// the given page and `wiki_id` to the destination wiki, so a fact lands
    /// on the right page of the right wiki when it crosses the boundary.
    ///
    /// The plan key (slug) is derived from the page the same way
    /// [`crate::promote`] flattens a page path — `slugify(<stem>)` — **except
    /// for a wiki's reserved pages, which are foundation nodes keyed per
    /// wiki**: `profile.md` is the card (`slugify(wiki_id)`) and `notes.md`
    /// the buffer ([`buffer_slug`], or the plain wiki slug on a topic wiki
    /// that has no card). Deriving `notes` from the stem instead would give
    /// every wiki's buffer the same forest-wide plan key, which is exactly
    /// the collision the cross-wiki lander exists to avoid.
    #[must_use]
    pub fn page_in_wiki(page: &str, wiki_id: &str) -> Self {
        let slug = plan_slug_for_page(wiki_id, page);
        Self {
            title: capitalize(&slug.replace('_', " ")),
            slug,
            description: String::new(),
            style: None,
            wiki_id: wiki_id.to_owned(),
            page_path: Some(page.to_owned()),
        }
    }
}

/// Re-home facts in the **persisted** plan + registry after an act-first
/// structural move (a REM split, a page merge) — the plan-sync seam.
///
/// The act-first move machinery rewrites disk bytes and `fact_index` rows,
/// but the planner's carry-over reads the persisted plan: without this seam
/// it re-assigns every moved fact to its old slug, and the next recompile of
/// the old page pulls the fact back — silently undoing the move and leaving
/// zombie markers on the target page. The seam:
///
/// - detaches each moved fact from whatever page holds it and appends it to
///   its destination page, seeding the page (and a registry entry) when the
///   plan does not know it yet;
/// - drops `remove_pages` husks from the plan + registry (audited in
///   `merged_pages`, scrubbed from order / links / children);
/// - marks every touched page [`CompilationPlan::force_dirty`], because after
///   the edit the carried-over fingerprint *matches* the next build — without
///   the flag the destination would never be woven by the Cronista.
///
/// No persisted plan yet ⇒ no-op (the first build derives placement fresh).
/// Returns how many facts were re-homed.
///
/// # Errors
///
/// Plan / registry IO.
#[allow(
    clippy::too_many_lines,
    reason = "one linear pass of plan surgery — detach, seed or relocate, attach, drop husks, park dirty — whose correctness is the order itself; splitting it hides that"
)]
pub fn rehome_facts_in_persisted_plan(
    tree: &WikiTree,
    moves: &[(&fact_index::FactIndexRow, &RehomePageSeed)],
    remove_pages: &[String],
    now: &str,
) -> Result<usize> {
    let Some(mut plan) = load_previous_plan(tree)? else {
        return Ok(0);
    };
    let mut registry = load_concept_registry(tree, now)?;
    let mut touched: BTreeSet<String> = BTreeSet::new();
    // Destinations this call actually landed facts on — the husk loop
    // below must not delete one of them.
    let mut seeded: BTreeSet<String> = BTreeSet::new();
    let mut rehomed = 0usize;
    for (row, seed) in moves {
        // As in `build_compilation_plan` step 4: canonicalise a proposed slug,
        // but leave a key the plan already holds alone — a buffer's `__`
        // separator does not survive `slugify`.
        let dest = if plan.pages.contains_key(&seed.slug) {
            seed.slug.clone()
        } else {
            slugify(&seed.slug)
        };
        if dest.is_empty() {
            continue;
        }
        // Detach from whatever page currently holds the fact.
        for (slug, page) in &mut plan.pages {
            let before = page.primary_facts.len();
            page.primary_facts.retain(|f| f.fact_id != row.fact_id);
            if page.primary_facts.len() != before {
                touched.insert(slug.clone());
            }
        }
        // Seed the destination page + registry entry when absent.
        if !plan.pages.contains_key(&dest) {
            plan.pages.insert(
                dest.clone(),
                PagePlan {
                    slug: dest.clone(),
                    title: seed.title.clone(),
                    description: seed.description.clone(),
                    style: seed.style.clone(),
                    page_type: PageType::ConceptLeaf,
                    owner_scope: None,
                    parent_hub: None,
                    child_leaves: Vec::new(),
                    primary_facts: Vec::new(),
                    outgoing_links: Vec::new(),
                    incoming_links: Vec::new(),
                    wiki_id: seed.wiki_id.clone(),
                    page_path: seed
                        .page_path
                        .clone()
                        .unwrap_or_else(|| format!("{dest}.md")),
                },
            );
            if !plan.compilation_order.iter().any(|s| s == &dest) {
                plan.compilation_order.push(dest.clone());
            }
            registry
                .entries
                .entry(dest.clone())
                .or_insert_with(|| ConceptRegistryEntry {
                    slug: dest.clone(),
                    title: seed.title.clone(),
                    description: seed.description.clone(),
                    style: seed.style.clone(),
                    page_type: PageType::ConceptLeaf,
                    parent_hub: None,
                    wiki_id: seed.wiki_id.clone(),
                    created_at: now.to_owned(),
                });
        }
        if let Some(page) = plan.pages.get_mut(&dest) {
            // A node the plan already holds may be the **same page moving
            // house**: a plan slug is the page's stem, so a page carried
            // into another wiki under its own name keeps its key and only
            // its address changes. Follow the seed when it names one — the
            // explicit `page_path` is what distinguishes "this exact file"
            // from a concept seed that merely proposes a slug. Without this
            // the node kept pointing at the wiki the page just left, and
            // the compiler would write it back there.
            if seed.page_path.is_some() {
                page.wiki_id.clone_from(&seed.wiki_id);
                if let Some(path) = &seed.page_path {
                    page.page_path.clone_from(path);
                }
            }
            page.primary_facts.push(FactForPage::from_row(row));
            touched.insert(dest.clone());
            seeded.insert(dest);
            rehomed += 1;
        }
    }
    for husk in remove_pages {
        let husk = slugify(husk);
        // A husk that is also a destination of this same call is not a husk:
        // the page did not empty out, it moved. Removing it here would
        // delete the node the loop above just filled and leave those facts
        // in no page at all — and since a plan with no pages reads back as
        // *no plan*, that is not a cosmetic loss.
        if seeded.contains(&husk) {
            continue;
        }
        if plan.pages.remove(&husk).is_some() {
            plan.merged_pages.push(MergedPage {
                from: husk.clone(),
                into: "—".to_owned(),
                reason: "act-first re-home removed the page".to_owned(),
            });
            plan.compilation_order.retain(|s| s != &husk);
            plan.dirty_pages.retain(|s| s != &husk);
            plan.link_graph.remove(&husk);
            for links in plan.link_graph.values_mut() {
                links.retain(|s| s != &husk);
            }
            for page in plan.pages.values_mut() {
                page.child_leaves.retain(|s| s != &husk);
                page.outgoing_links.retain(|s| s != &husk);
                page.incoming_links.retain(|s| s != &husk);
            }
        }
        registry.entries.remove(&husk);
        touched.remove(&husk);
    }
    for slug in touched {
        if !plan.force_dirty.contains(&slug) {
            plan.force_dirty.push(slug);
        }
    }
    now.clone_into(&mut plan.generated_at);
    save_plan(tree, &plan)?;
    save_concept_registry(tree, &registry)?;
    Ok(rehomed)
}

/// Park `slugs` on the **persisted** plan's [`CompilationPlan::force_dirty`]
/// so the next [`build_wiki_plan`] recompiles them regardless of fingerprint
/// drift.
///
/// The compiler calls this for every page whose compile **failed or
/// degraded**, so a failed page is retried for a proper rewrite next cycle
/// instead of settling as cleanly compiled (without the flag, an idle night's
/// early-skip would clear the dirty set and freeze the page until its facts
/// change). Idempotent: a slug already parked is not duplicated. No persisted
/// plan ⇒ no-op (the first build derives the dirty set fresh anyway).
///
/// Returns how many slugs were newly parked.
///
/// # Errors
///
/// Plan IO.
pub fn park_force_dirty_in_persisted_plan(tree: &WikiTree, slugs: &[String]) -> Result<usize> {
    if slugs.is_empty() {
        return Ok(0);
    }
    let Some(mut plan) = load_previous_plan(tree)? else {
        return Ok(0);
    };
    let mut added = 0usize;
    for slug in slugs {
        if !plan.force_dirty.contains(slug) {
            plan.force_dirty.push(slug.clone());
            added += 1;
        }
    }
    if added > 0 {
        save_plan(tree, &plan)?;
    }
    Ok(added)
}

/// Every persisted-plan slug whose page lives in `wiki_id`.
///
/// The lookup a **dissolve** needs ([`crate::wiki_delete`]): the slugs it
/// hands to [`park_bridge_signals`] as `reopen_pages`, so the facts those
/// pages carried leave the next build's carry-over and flow through the
/// Cartografo again instead of inheriting a placement whose page no longer
/// exists. A missing plan yields nothing — there is no carry-over to
/// re-open, and the first build classifies those facts as new anyway.
///
/// # Errors
///
/// Plan IO.
pub fn plan_slugs_of_wiki(tree: &WikiTree, wiki_id: &str) -> Result<Vec<String>> {
    let Some(plan) = load_previous_plan(tree)? else {
        return Ok(Vec::new());
    };
    Ok(plan
        .pages
        .values()
        .filter(|p| p.wiki_id == wiki_id)
        .map(|p| p.slug.clone())
        .collect())
}

/// Park the dream reviewer's bridge signals on the persisted plan.
///
/// `refile_candidates` are fact ids the next refile sweep judges;
/// `reopen_pages` are slugs whose carried placements the next plan
/// build re-judges. Deduped against what is already parked; a missing
/// plan is a no-op — there is nothing to bridge into.
///
/// Returns how many entries were newly parked (both fields).
///
/// # Errors
///
/// Plan IO.
pub fn park_bridge_signals(
    tree: &WikiTree,
    refile_candidates: &[String],
    reopen_pages: &[String],
) -> Result<usize> {
    if refile_candidates.is_empty() && reopen_pages.is_empty() {
        return Ok(0);
    }
    let Some(mut plan) = load_previous_plan(tree)? else {
        return Ok(0);
    };
    let mut added = 0usize;
    for fid in refile_candidates {
        if !plan.refile_candidates.contains(fid) {
            plan.refile_candidates.push(fid.clone());
            added += 1;
        }
    }
    for slug in reopen_pages {
        if !plan.reopen_pages.contains(slug) {
            plan.reopen_pages.push(slug.clone());
            added += 1;
        }
    }
    if added > 0 {
        save_plan(tree, &plan)?;
    }
    Ok(added)
}

/// Drain the parked refile candidates — the consume half of the
/// reviewer→refile bridge.
///
/// Returns them and clears the field, so each nomination gets exactly
/// one judge pass (parked again only if the next review still finds
/// it). A missing plan yields nothing.
///
/// # Errors
///
/// Plan IO.
pub fn take_refile_candidates(tree: &WikiTree) -> Result<Vec<String>> {
    let Some(mut plan) = load_previous_plan(tree)? else {
        return Ok(Vec::new());
    };
    if plan.refile_candidates.is_empty() {
        return Ok(Vec::new());
    }
    let taken = std::mem::take(&mut plan.refile_candidates);
    save_plan(tree, &plan)?;
    Ok(taken)
}

/// Persist the concept registry (crash-safe atomic write).
///
/// # Errors
///
/// IO / JSON errors.
pub fn save_concept_registry(tree: &WikiTree, registry: &ConceptRegistry) -> Result<()> {
    let path = plan_dir(tree).join("concept-registry.json");
    let bytes = serde_json::to_vec_pretty(registry)?;
    atomic_write(&path, &bytes)?;
    Ok(())
}

/// Whether any fact carried over from `prev` now has different claim text.
///
/// An in-place correction (same `fact_id`, new text — the dashboard-comment
/// shape) is neither a new nor a removed fact, so the `build_wiki_plan` early
/// skip would otherwise short-circuit before the correction reached the prose.
fn any_content_drift(facts: &[FactForPage], prev: &CompilationPlan) -> bool {
    let prev_key: BTreeMap<&str, String> = prev
        .pages
        .values()
        .flat_map(|p| p.primary_facts.iter())
        .map(|f| (f.fact_id.as_str(), fact_render_key(f)))
        .collect();
    facts.iter().any(|f| {
        prev_key
            .get(f.fact_id.as_str())
            .is_some_and(|k| *k != fact_render_key(f))
    })
}

/// Fact ids already homed by the previous plan → their page slug.
#[must_use]
pub fn extract_assigned_fact_ids(plan: &CompilationPlan) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (slug, page) in &plan.pages {
        for f in &page.primary_facts {
            map.insert(f.fact_id.as_str().to_owned(), slug.clone());
        }
    }
    map
}

// ---------- Stadio 1 — Il Cartografo (LLM) ----------

/// Structural signals the engine hands the Cartografo alongside the facts.
///
/// **Information the model weighs, never a gate the code enforces** (the
/// no-hardcoded-gates pillar: the prompt carries the placement discipline,
/// Rust only computes what the model cannot see on its own). Two signals
/// ride the prompt context:
///
/// - **identity-page scope** (per fact, via its subject) — which `person`
///   pages the fact's *subject* covers, so the model can keep a foreign
///   subject off a user's identity index (an identity index carries one
///   subject; the relation surfaces through the page-user's own facts plus
///   a `[[wikilink]]`). Computed from enrollment by [`subject_scopes_for`].
/// - **page mass** (per plan page) — how many facts currently live on each
///   page, so the model can split a grown page by content before it exceeds
///   what renders reliably as one page. The numbers are the signal; where
///   the content splits is the model's judgment.
#[derive(Debug, Default, Clone)]
pub struct CartografoSignals {
    /// What the placement stages are shown of the **rest of the forest** —
    /// every page of it, or a per-wiki slice once the memory outgrows
    /// [`FOREST_PAGE_CEILING`]. Built by [`foreign_page_offers`].
    pub foreign_pages: ForeignPages,
    /// Subject principal (wire form, e.g. `user:bruno` / `group:famiglia` /
    /// `global`) → the rendered identity-page scope tag: a comma-joined list
    /// of `person`-page slugs, `any` (the builtin global group — world
    /// context is never a foreign subject), or `none` (a group with no
    /// enrolled members). See [`subject_scopes_for`].
    pub subject_scopes: BTreeMap<String, String>,
    /// Plan slug → number of facts currently homed on that page (the
    /// carried-over count entering this build). [`classify_facts`] adds its
    /// own in-run assignments on top so later batches see the pile grow.
    pub page_mass: BTreeMap<String, usize>,
    /// Source wiki id → that wiki's rendered `LANGUAGE` directive. The
    /// Cartografo and the Conciliatore coin page titles and descriptions a
    /// person reads, so each batch is cut to one wiki and carries that
    /// wiki's language. Built by [`wiki_locales_for`].
    pub wiki_locales: BTreeMap<String, String>,
}

impl CartografoSignals {
    /// The identity-page scope tag for one subject, from the precomputed map.
    ///
    /// Falls back to what is derivable without enrollment (a bag built
    /// empty): a user covers their own page, the global group covers `any`,
    /// a group with unknown membership covers `none`.
    /// The `LANGUAGE` directive for one source wiki. An unknown wiki
    /// falls back to English, the same fallback every memory-writing slot
    /// uses — see [`crate::locale::render_memory_language_directive`].
    #[must_use]
    fn language_for(&self, wiki_id: &str) -> String {
        self.wiki_locales
            .get(wiki_id)
            .cloned()
            .unwrap_or_else(|| crate::locale::render_memory_language_directive(None))
    }

    #[must_use]
    fn identity_scope_tag(&self, subject: &Principal) -> String {
        if let Some(tag) = self.subject_scopes.get(&subject.to_string()) {
            return tag.clone();
        }
        match subject {
            p if p.is_global() => "any".to_owned(),
            Principal::User(id) => slugify(id),
            Principal::Group(_) => "none".to_owned(),
        }
    }
}

/// What a placement stage is shown of the wikis its batch does not come from.
///
/// **A fact is free to live in any wiki** (founder, 2026-08-10): where a fact
/// is filed changes nothing about who may read it — read permission is judged
/// per fact on `subject ∪ allow ∪ sender`, never on the container — so the only
/// question is whether the prose it lands in hangs together. That makes the
/// page list the whole mechanism: a page the model is not shown is a page a
/// fact can never reach, and until 2026-08-14 the list was its own wiki's
/// pages plus the bare *names* of everyone else's, so a fact could never be
/// re-homed once it landed.
#[derive(Debug, Default, Clone)]
pub enum ForeignPages {
    /// Every page of the forest, described. The default, and the right answer
    /// while the memory fits one list.
    #[default]
    Whole,
    /// Past [`FOREST_PAGE_CEILING`]: wiki id → the foreign concept-page slugs
    /// that wiki's batches are offered, **nearest first**.
    ///
    /// A wiki absent from the map, or present with an empty list, is offered
    /// no foreign concept page at all — its own pages and the forest's
    /// identity cards remain, and the names of the rest still ride the
    /// collision list. Smaller, never wrong.
    Selected(BTreeMap<String, Vec<String>>),
}

impl ForeignPages {
    /// Whether a foreign page is offered to `wiki`'s batches as a destination.
    fn offers(&self, wiki: &str, slug: &str) -> bool {
        match self {
            Self::Whole => true,
            Self::Selected(by_wiki) => by_wiki
                .get(wiki)
                .is_some_and(|picked| picked.iter().any(|s| s == slug)),
        }
    }

    /// The rank of a foreign page for `wiki` — its position in the selection,
    /// so the rendering can keep *nearest first* instead of re-sorting by
    /// slug, which would hand the model an alphabetical list again.
    fn rank(&self, wiki: &str, slug: &str) -> usize {
        match self {
            Self::Whole => 0,
            Self::Selected(by_wiki) => by_wiki
                .get(wiki)
                .and_then(|picked| picked.iter().position(|s| s == slug))
                .unwrap_or(usize::MAX),
        }
    }
}

/// Decide what each wiki's batches are shown of the rest of the forest.
///
/// Whole below [`FOREST_PAGE_CEILING`]; above it, one ranked slice per wiki
/// that has facts in this run.
///
/// **Ranked by nearness between page cards**, which is the only signal
/// available here: the planner has no embedder and deliberately does not grow
/// one (same rule as the compiler). Cards are embedded by the reindex
/// pipeline, so a page whose card never embedded does not rank — it makes the
/// offer smaller, never wrong. A foreign page scores its **best** similarity
/// against any of the asking wiki's own cards rather than against their
/// average: a user's wiki spans several unrelated subjects, and a centroid
/// over them is a point about none of them.
pub async fn foreign_page_offers(
    pool: &SqlitePool,
    tree: &WikiTree,
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    wikis: &BTreeSet<String>,
) -> ForeignPages {
    if foundation.len() + registry.entries.len() <= FOREST_PAGE_CEILING {
        return ForeignPages::Whole;
    }
    // slug → (wiki, card vector), for the concept pages only: identity cards
    // are offered whole at any size (the product limits cap them) and a
    // foreign buffer is never a destination.
    let mut vectors: BTreeMap<String, (String, Vec<f32>)> = BTreeMap::new();
    for e in registry.entries.values() {
        let Some(path) = registry_source_path(tree, e) else {
            continue;
        };
        if let Ok(Some(row)) = crate::page_card::get(pool, &path).await
            && let Some(v) = row.embedding
        {
            vectors.insert(e.slug.clone(), (e.wiki_id.clone(), v));
        }
    }
    let mut by_wiki: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for wiki in wikis {
        let mine: Vec<&Vec<f32>> = vectors
            .values()
            .filter(|(w, _)| w == wiki)
            .map(|(_, v)| v)
            .collect();
        let mut scored: Vec<(f32, &str)> = vectors
            .iter()
            .filter(|(_, (w, _))| w != wiki)
            .filter_map(|(slug, (_, v))| {
                let best = mine
                    .iter()
                    .map(|m| crate::recall::cosine_similarity(m, v))
                    .fold(f32::NEG_INFINITY, f32::max);
                best.is_finite().then_some((best, slug.as_str()))
            })
            .collect();
        // Descending by nearness; the registry's own order breaks ties, so the
        // result is deterministic for a given corpus.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        by_wiki.insert(
            wiki.clone(),
            scored
                .into_iter()
                .take(FOREIGN_SELECTION_PAGES)
                .map(|(_, slug)| slug.to_owned())
                .collect(),
        );
    }
    tracing::info!(
        pages = foundation.len() + registry.entries.len(),
        ceiling = FOREST_PAGE_CEILING,
        embedded = vectors.len(),
        wikis = by_wiki.len(),
        "planner: forest page list over its ceiling — offering the nearest foreign pages per wiki"
    );
    ForeignPages::Selected(by_wiki)
}

/// The `page_card` key of a registry page — its workdir-relative source path.
fn registry_source_path(tree: &WikiTree, e: &ConceptRegistryEntry) -> Option<String> {
    let handle = tree
        .locate(&crate::types::WikiId::parse(&e.wiki_id).ok()?)
        .ok()?;
    Some(crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &handle.abs_dir().join(format!("{}.md", e.slug)),
    ))
}

/// Compute the per-subject identity-page scope tags for `facts` from the
/// enrollment tables — the mechanical half of the identity-page discipline.
///
/// A fact is *foreign* to an identity index when its `subject` is a
/// **different user**, or a **group the page's user is not a member of** (a
/// group the user belongs to is their own shared context, never foreign).
/// Rendered per distinct subject as the pages the subject covers:
///
/// - `user:<id>` → that user's `person`-page slug;
/// - `group:<g>` → the member users' `person`-page slugs
///   ([`enrollment::members_for`]), `none` when the group has no enrolled
///   members (foreign on every identity index);
/// - the builtin global group → `any` (universal membership — world context
///   is never another subject).
///
/// # Errors
///
/// DB errors from the enrollment lookups.
pub async fn subject_scopes_for(
    pool: &SqlitePool,
    facts: &[FactForPage],
) -> Result<BTreeMap<String, String>> {
    let mut scopes: BTreeMap<String, String> = BTreeMap::new();
    for f in facts {
        let key = f.subject.to_string();
        if scopes.contains_key(&key) {
            continue;
        }
        let tag = match &f.subject {
            p if p.is_global() => "any".to_owned(),
            Principal::User(id) => slugify(id),
            Principal::Group(g) => {
                let members = enrollment::members_for(pool, g).await?;
                let slugs: Vec<String> = members
                    .iter()
                    .map(|m| slugify(m))
                    .filter(|s| !s.is_empty())
                    .collect();
                if slugs.is_empty() {
                    "none".to_owned()
                } else {
                    slugs.join(",")
                }
            },
        };
        scopes.insert(key, tag);
    }
    Ok(scopes)
}

/// Hold a proposed page to the two rules the prompt states and nothing
/// enforced — **a container is a wiki** (founder, 2026-08-04).
///
/// 1. **Every proposal is a `concept_leaf`.** `PageType` still deserialises
///    `concept_hub` so a plan written before that date loads, so a model that
///    emits one was accepted and a retired page type came back into a fresh
///    corpus.
/// 2. **`parent_hub` is a foundation page of this wiki**, or nothing. A page
///    parented under another *page* is exactly the container the ruling
///    abolished, and it used to arrive by two routes at once: nobody checked,
///    and the prompt's own container rule then told the model to treat the
///    malformed page as a legitimate hub and keep filling it. Dropping the
///    bad parent is the right correction rather than dropping the page — the
///    facts still need a home, and `resolve_page_wiki` then homes it where
///    its facts are instead of following an invented parent into a foreign
///    wiki.
///
/// **The `this wiki` half of rule 2 survived the un-fencing of 2026-08-14,
/// with a different reason.** It is not a fence around where a fact may
/// live: a fact may be assigned to any page in the forest. It is what
/// *proposing* a page means. A new page is born where its facts are
/// ([`resolve_page_wiki`] reads `slug_source_wiki` first), and this batch's
/// facts are this wiki's — so a page it coins lands in this wiki, and a hub
/// in another one would be a parent the page does not live under. That also
/// keeps `{locale}` honest: the batch coins titles for its own wiki only.
/// [`vet_accepted`] asks only that the hub exist, because by the time the
/// Conciliatore runs the page may have been merged into one that is already
/// homed elsewhere.
///
/// Corrected, not refused: a rejected proposal costs the batch a page its
/// facts were meant to have, and the model has no second chance to fix it.
fn vet_proposal(mut np: NewPage, foundation: &BTreeMap<String, PagePlan>, wiki: &str) -> NewPage {
    if np.page_type != PageType::ConceptLeaf {
        tracing::warn!(
            slug = %np.slug,
            proposed = page_type_tag(np.page_type),
            "cartografo: proposed a page type it may not create — filed as a concept_leaf"
        );
        np.page_type = PageType::ConceptLeaf;
    }
    if let Some(hub) = &np.parent_hub {
        let hub = slugify(hub);
        let is_local_foundation = foundation
            .get(&hub)
            .is_some_and(|p| p.wiki_id == wiki && p.page_type.is_foundation());
        if is_local_foundation {
            np.parent_hub = Some(hub);
        } else {
            tracing::warn!(
                slug = %np.slug,
                parent_hub = %hub,
                wiki,
                "cartografo: parent_hub is not a foundation page of this wiki — dropped"
            );
            np.parent_hub = None;
        }
    }
    np
}

/// Hold a page the Conciliatore **accepted** to the rules [`vet_proposal`]
/// holds a proposal to.
///
/// That stage runs one earlier, on the Cartografo's raw output; what comes
/// back from this one is materialised into the plan *and persisted into the
/// concept registry*, and it arrives as free-form JSON — the model is asked to
/// re-emit `slug` / `page_type` / `parent_hub` while it decides merges, so
/// every field can come back changed. Three checks, in the order the damage
/// would land:
///
/// 1. **A reserved page name is refused outright** (`index`, `rules`,
///    `projects`, `profile`, `notes`). A concept page keyed by one of those
///    stems compiles to the same file as the wiki's own card or buffer — two
///    plan pages, one path. The Cartografo path drops such a proposal too; the
///    facts meant for it fall through to the orphan pass, which has a real
///    page for them.
/// 2. **Every accepted page is a `concept_leaf`** — the only kind anything may
///    create since *a container is a wiki* (founder, 2026-08-04).
/// 3. **`parent_hub` names a real foundation page**, or nothing. Deliberately
///    weaker than [`vet_proposal`]'s: that one also demands the hub belong to
///    *this* wiki, and a cross-wiki placement is the engine's prerogative
///    (founder, 2026-08-10 — see card 72f, where that fence is the work item).
///    What has to hold here is that the hub exists at all; `resolve_page_wiki`
///    then homes the page under it.
fn vet_accepted(mut np: NewPage, foundation: &BTreeMap<String, PagePlan>) -> Option<NewPage> {
    let slug = slugify(&np.slug);
    if crate::wiki::is_reserved_page_stem(&slug) {
        tracing::warn!(
            slug = %slug,
            "conciliatore: accepted a reserved page name — page dropped"
        );
        return None;
    }
    if np.page_type != PageType::ConceptLeaf {
        tracing::warn!(
            slug = %slug,
            accepted = page_type_tag(np.page_type),
            "conciliatore: accepted a page type nothing may create — filed as a concept_leaf"
        );
        np.page_type = PageType::ConceptLeaf;
    }
    if let Some(hub) = &np.parent_hub {
        let hub = slugify(hub);
        if foundation
            .get(&hub)
            .is_some_and(|p| p.page_type.is_foundation())
        {
            np.parent_hub = Some(hub);
        } else {
            tracing::warn!(
                slug = %slug,
                parent_hub = %hub,
                "conciliatore: parent_hub is not a foundation page — dropped"
            );
            np.parent_hub = None;
        }
    }
    np.slug = slug;
    Some(np)
}

/// Hold the Conciliatore's redirects to what a merge can actually be.
///
/// A redirect says *«this proposed page is really that existing one»*, and the
/// plan builder obeys it twice — assignments are rewritten to the target
/// before the build, and step 4 rewrites the slug again. Nothing checked the
/// target until 2026-08-10.
///
/// - **The target must exist**, as a registry page or as one accepted this
///   run. When it does not, step 4's fallback mints a blank page under the
///   name and *«merge into X»* becomes *«create an empty X»* — carrying no
///   `style`, so a record page redirected onto an invented name is then
///   compiled as prose. The prompt's contract calls this stage conservative,
///   *never loses a page*; an invented destination loses the page it named.
/// - **The target is never a foundation page.** A card carries a subject's
///   identity and a buffer is where a fact waits for a home; neither is a
///   topic something can be merged *into*. [`describe_existing`] no longer
///   offers them and this refuses one named anyway — the two halves of the
///   same rule, because the prompt's own bias is *«when in doubt, prefer the
///   redirect»*.
/// - **Nothing redirects onto itself**, which would only cost a lookup.
///
/// Refusing a redirect is the conservative outcome: the proposed page stays
/// its own page, and the next cycle can still merge it correctly.
fn vet_redirects(
    proposed: BTreeMap<String, String>,
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    accepted_new: &[NewPage],
) -> BTreeMap<String, String> {
    let accepted: BTreeSet<String> = accepted_new.iter().map(|np| slugify(&np.slug)).collect();
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
    for (from, to) in proposed {
        let from_slug = slugify(&from);
        let target = slugify(&to);
        if target.is_empty() || target == from_slug {
            tracing::warn!(
                from = %from, to = %to,
                "conciliatore: redirect onto itself or onto nothing — dropped"
            );
            continue;
        }
        if foundation.contains_key(&target) {
            tracing::warn!(
                from = %from, to = %target,
                "conciliatore: redirect targets a foundation page — dropped, the page stays its own"
            );
            continue;
        }
        if !registry.entries.contains_key(&target) && !accepted.contains(&target) {
            tracing::warn!(
                from = %from, to = %target,
                "conciliatore: redirect targets a page that does not exist — dropped, the page stays its own"
            );
            continue;
        }
        kept.insert(from_slug, target);
    }
    kept
}

/// Cut `facts` into the Cartografo's units of work: **grouped by source
/// wiki first**, chunked to [`CARTOGRAFO_BATCH`] second.
///
/// The order matters and is the point. This stage coins page titles and
/// descriptions a person reads, so a batch has to belong to one wiki for
/// "the language of this page" to exist at all — chunking first and
/// grouping after would leave a 15-fact chunk straddling two wikis with
/// two languages and one directive.
///
/// The wiki of the batch is **not** the wiki of the pages it may choose
/// from — a fact is free to live in any wiki (founder, 2026-08-10), and
/// [`describe_foundation`] / [`describe_concepts`] offer the forest. What
/// one wiki per batch still buys, and the only thing it buys, is the
/// language: the pages a batch **coins** are homed by
/// [`resolve_page_wiki`] in its own facts' wiki, so `{locale}` is the
/// language of every title and description this call writes. A page it
/// merely **chooses** was titled by whoever coined it, in that wiki's own
/// language, and this call does not rewrite it.
///
/// Deterministic: `BTreeMap` orders the wikis, and the caller's
/// `fact_id` sort survives inside each one.
fn cartografo_batches(facts: &[FactForPage]) -> BTreeMap<String, Vec<FactForPage>> {
    let mut by_wiki: BTreeMap<String, Vec<FactForPage>> = BTreeMap::new();
    for f in facts {
        by_wiki
            .entry(f.source_wiki_id.clone())
            .or_default()
            .push(f.clone());
    }
    by_wiki
}

/// Resolve the `LANGUAGE` directive of every distinct source wiki in
/// `facts` — the language half of [`CartografoSignals`].
///
/// One lookup per distinct wiki, not per fact: the scope-chain walk that
/// turns a wiki into a principal is not free, and a build routinely
/// carries hundreds of facts across a handful of wikis.
pub async fn wiki_locales_for(
    pool: &SqlitePool,
    tree: &WikiTree,
    facts: &[FactForPage],
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for f in facts {
        if out.contains_key(&f.source_wiki_id) {
            continue;
        }
        let Ok(id) = crate::types::WikiId::parse(&f.source_wiki_id) else {
            continue;
        };
        let directive = crate::locale::memory_directive_for_wiki(pool, tree, &id).await;
        out.insert(f.source_wiki_id.clone(), directive);
    }
    out
}

/// Assign each fact to one page and propose emergent concept pages.
///
/// LLM, batched, one-fact-one-page. Resilient: a batch whose LLM call or JSON
/// parse fails is **skipped softly** (its facts fall to the Architetto's
/// deterministic subject-page fallback) rather than aborting the cycle.
///
/// `signals` is the structural context ([`CartografoSignals`]): each fact
/// line carries its identity-page scope tag, each page line its fact mass —
/// the running mass includes this run's own assignments, so a later batch
/// sees the pages earlier batches filled.
///
/// # Errors
///
/// Only a broken prompt (operator override) surfaces; LLM/parse failures are
/// soft per-batch.
pub async fn classify_facts(
    llm: &dyn LlmBackend,
    facts: &[FactForPage],
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    workdir: &Path,
    signals: &CartografoSignals,
) -> Result<Blueprint> {
    let mut merged = Blueprint::default();
    let mut known: BTreeSet<String> = foundation
        .keys()
        .cloned()
        .chain(registry.entries.keys().cloned())
        .collect();
    // Running per-page mass: the carried-over counts plus what THIS run has
    // already assigned, so batch k sees the pile batches 1..k-1 built up.
    let mut running_mass = signals.page_mass.clone();
    // Materialised into an owned Vec rather than iterated lazily: the loop
    // below awaits, and a borrowed chunk iterator alive across that await
    // defeats the compiler's `Send` inference for every caller of this
    // future (the dashboard's dream routes stop compiling).
    let batches: Vec<(String, Vec<FactForPage>)> = cartografo_batches(facts)
        .into_iter()
        .flat_map(|(wiki, rows)| {
            rows.chunks(CARTOGRAFO_BATCH)
                .map(|c| (wiki.clone(), c.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect();
    // Which wiki proposed each page of this run — a proposal is homed in its
    // facts' wiki, so an earlier batch's page belongs to that batch's wiki. It
    // is offered to every batch all the same: a page about to exist is a page
    // to reuse rather than duplicate, and reusing one across wikis is a
    // legitimate placement.
    let mut proposal_wikis: BTreeMap<String, String> = BTreeMap::new();
    for (wiki, batch) in &batches {
        let wiki = wiki.as_str();
        let batch = batch.as_slice();
        let proposed_so_far: Vec<(&NewPage, &str)> = merged
            .new_pages
            .iter()
            .filter_map(|np| proposal_wikis.get(&np.slug).map(|w| (np, w.as_str())))
            .collect();
        let foundation_desc = describe_foundation(foundation, wiki, &running_mass);
        let concept_desc = describe_concepts(
            registry,
            wiki,
            &proposed_so_far,
            &running_mass,
            &signals.foreign_pages,
        );
        let taken_desc = describe_taken_slugs(foundation, registry, wiki, &signals.foreign_pages);
        let facts_desc = describe_facts(batch, signals);
        let language_directive = signals.language_for(wiki);
        let system = prompts::render(
            "cartografo",
            workdir,
            BUNDLED_CARTOGRAFO_MD,
            &[
                ("locale", language_directive.as_str()),
                ("foundation_pages", foundation_desc.as_str()),
                ("concept_pages", concept_desc.as_str()),
                ("taken_slugs", taken_desc.as_str()),
                ("facts", facts_desc.as_str()),
            ],
        )?;
        let resp = match llm
            .complete(
                CompletionRequest::new("Assign the facts and return the JSON object.")
                    .with_system(system)
                    .with_temperature(0.2)
                    // Content-scaled reply: the assignment JSON grows with
                    // the batch; a clipped plan is silent corruption. Any
                    // ceiling hit warns centrally in the llm layer.
                    .with_max_tokens(8_000),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "cartografo: LLM failed for a batch, facts will orphan-fallback");
                continue;
            },
        };
        let Some(bp) = parse_json::<Blueprint>(&resp.text) else {
            tracing::warn!("cartografo: unparseable batch output, facts will orphan-fallback");
            continue;
        };
        for a in &bp.assignments {
            *running_mass.entry(slugify(&a.page_slug)).or_insert(0) += 1;
        }
        merged.assignments.extend(bp.assignments);
        for np in bp.new_pages {
            let slug = slugify(&np.slug);
            // A coined name is never one of the reserved pages. `placement_slug`
            // has always refused them for a page the *user* named; a
            // `concept_leaf` the Cartografo invents took `slugify` alone, so a
            // page called `projects` or `rules` would have been materialised
            // straight over that wiki's channel page — and
            // `compiler::sweep_orphan_page_files` exempts `index.md` and
            // `rules.md` but not `projects.md`, so the signposts were the ones
            // with no floor under them.
            if crate::wiki::is_reserved_page_stem(&slug) {
                tracing::warn!(
                    slug = %slug,
                    wiki,
                    "cartografo: proposed a reserved page name — proposal dropped"
                );
                continue;
            }
            if !slug.is_empty() && known.insert(slug.clone()) {
                proposal_wikis.insert(slug.clone(), wiki.to_owned());
                merged
                    .new_pages
                    .push(vet_proposal(NewPage { slug, ..np }, foundation, wiki));
            }
        }
    }
    tracing::info!(
        assignments = merged.assignments.len(),
        new_pages = merged.new_pages.len(),
        "cartografo: classification done"
    );
    Ok(merged)
}

// ---------- Stadio 1 (light cadence) — ingest placement (no LLM) ----------

/// How NEW facts get placed onto pages in [`build_wiki_plan`], per cadence.
///
/// Already-known facts keep their carried-over
/// assignments regardless; only the facts the planner has not seen before flow
/// through this choice.
pub enum NewFactPlacement<'a> {
    /// Settle each new fact onto the page the ingest classifier already
    /// proposed (`fact_index.target_page`), deterministically and with NO LLM
    /// call. A fact with no concrete proposed page (a reserved name / empty /
    /// `None`) orphan-falls-back.
    ///
    /// Since the classifier stopped proposing a page for prose, this places
    /// only what the USER named — a `lista`, or a container they asked for by
    /// name — so on its own it leaves every prose fact on the wiki's buffer.
    /// Kept as the degraded light path for a deployment with no ingest slot
    /// wired, and as the first half of [`Self::NamedThenCartografo`].
    Ingest,
    /// FULL / REM cadence: classify new facts with the strong-model Cartografo.
    Cartografo(&'a dyn LlmBackend),
    /// LIGHT cadence: honour every page the user named, then hand ONLY the
    /// remainder to the cheap-tier Cartografo.
    ///
    /// The two halves are not interchangeable and the order is the design.
    /// A page the user named is not a model's to choose: a `lista` is a **set**
    /// (half a shopping list is a wrong answer, not a partial one) and a
    /// container someone asked for by name was already written there, live, in
    /// front of them. Handing those to a model that is shown neither the style
    /// nor the proposed page ([`describe_facts`]) is how an item leaves the
    /// list it was added to. So they are settled first, deterministically, and
    /// never reach the batch.
    ///
    /// What DOES reach it is everything the classifier left unplaced — which,
    /// since prose stopped carrying a page name, is the material the buffer was
    /// filling up with. That is the whole point of running the Cartografo
    /// hourly: the write side gets its structure within the hour instead of
    /// overnight.
    ///
    /// A `salience: "high"` fact is in neither half — it is reserved for the
    /// subject's identity card and gets there by orphan-fallback, exactly as
    /// under [`Self::Ingest`].
    NamedThenCartografo(&'a dyn LlmBackend),
    /// No placement intelligence: every new fact orphan-falls-back to its
    /// subject / source-wiki foundation page — the historical `cartografo = None`
    /// degradation, kept for a Full pass on a deployment with no strong slot.
    OrphanFallback,
}

impl NewFactPlacement<'_> {
    /// Does this placement actually call the Cartografo?
    ///
    /// Gates the context the model needs and nothing else pays for — the
    /// enrollment-derived identity scopes and the per-wiki language directive.
    /// Deliberately NOT the gate for consuming the re-open park: see
    /// [`build_wiki_plan`], where that is the strong pass's alone.
    #[must_use]
    pub const fn runs_cartografo(&self) -> bool {
        matches!(self, Self::Cartografo(_) | Self::NamedThenCartografo(_))
    }

    /// Stable name of the placement, for the compile log and for the test that
    /// pins which one each cadence actually ships with.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Cartografo(_) => "cartografo",
            Self::NamedThenCartografo(_) => "named-then-cartografo",
            Self::OrphanFallback => "orphan-fallback",
        }
    }
}

/// Flatten an ingest `target_page` hint (a slug or `.md` path) to a concept-leaf
/// slug, or `None` when it names no concrete page.
///
/// The ingest classifier "catalogues and lays down, it does NOT do emergence"
/// → folders / nesting are REM's job, so a path like `recipes/dinner.md`
/// flattens to a single leaf `recipes_dinner` (slugify maps the separator to
/// `_`). Empty resolves to `None`, and so does **every reserved page name**,
/// because none of them is a concept page a classifier may mint:
///
/// - `index.md` — the wiki's map, which holds no facts at all;
/// - `profile.md` and `notes.md` — the wiki's card and buffer, which are
///   per-wiki **foundation nodes**; minting a concept page here would put the
///   same file in the plan under a second, forest-wide key;
/// - `rules.md` ([`crate::wiki::RULES_FILENAME`]) and `projects.md`
///   ([`crate::wiki::PROJECTS_FILENAME`]) — written by a deterministic
///   channel, so a fact mis-targeted there must not land among the policy or
///   the signposts.
///
/// In every case the fact falls through to [`orphan_target`], which homes it
/// on its wiki's card or buffer according to what the fact is. The `.md`
/// suffix is stripped first so slugify does not fold it into a trailing
/// `_md`.
fn placement_slug(target_page: &str) -> Option<String> {
    let stripped = target_page.strip_suffix(".md").unwrap_or(target_page);
    let slug = slugify(stripped);
    if slug.is_empty() || crate::wiki::is_reserved_page_stem(&slug) {
        None
    } else {
        Some(slug)
    }
}

/// Deterministic, no-LLM Cartografo substitute for the LIGHT cadence: place each
/// fact on the page its ingest classifier proposed (`FactForPage::target_page`).
///
/// Mirrors [`classify_facts`]'s output shape so it drops into the same
/// [`build_compilation_plan`] pipeline unchanged: one [`Assignment`] per
/// placeable fact, plus one [`NewPage`] (a `concept_leaf`) per distinct target
/// slug, seeded with the fact's ingest-proposed `style` + `page_description` so
/// the freshly-minted page carries a testata (the on-the-fly mint in
/// `build_compilation_plan` would give only `description = ""`). A slug already
/// in the registry/foundation is skipped downstream, so this is idempotent across
/// runs. Facts with no concrete target are left unassigned for the
/// orphan-fallback. Pure — testable without a DB or an LLM.
fn ingest_placement_blueprint(facts: &[FactForPage]) -> Blueprint {
    let mut assignments = Vec::new();
    // BTreeMap → dedup distinct target slugs deterministically; the first fact's
    // style/description seeds the page, a later same-page fact only adds an
    // assignment.
    let mut new_pages: BTreeMap<String, NewPage> = BTreeMap::new();
    for f in facts {
        // The routing IS the reservation. A `high`-salience
        // fact is always-on material (identity, health/safety, hard standing
        // constraints) whose home is the actor's identity CARD,
        // *overriding* any concrete ingest `target_page`. We achieve that by
        // leaving it UNASSIGNED here: the deterministic orphan-fallback in
        // `build_compilation_plan` then homes it on the subject's card node
        // (`profile.md`) — see `orphan_target`, which reads the same salience
        // to tell a reserved fact from one that merely has no page yet. No new
        // branch, no LLM — the same path a fact with no proposed page already
        // takes ("una pipeline sola").
        if f.salience.as_deref() == Some("high") {
            continue;
        }
        let Some(slug) = f.target_page.as_deref().and_then(placement_slug) else {
            continue;
        };
        assignments.push(Assignment {
            fact_id: f.fact_id.as_str().to_owned(),
            page_slug: slug.clone(),
        });
        new_pages.entry(slug.clone()).or_insert_with(|| NewPage {
            title: capitalize(&slug.replace('_', " ")),
            slug,
            description: f.page_description.clone().unwrap_or_default(),
            style: f.style.clone(),
            page_type: PageType::ConceptLeaf,
            parent_hub: None,
        });
    }
    Blueprint {
        assignments,
        new_pages: new_pages.into_values().collect(),
    }
}

/// Place a batch of NEW facts per the cadence's [`NewFactPlacement`] — the single
/// site the two [`build_wiki_plan`] classify branches (fresh plan / incremental)
/// share, so the cadence policy lives in exactly one `match`. `signals` feeds
/// only the Cartografo branch (the deterministic paths take no structural
/// context).
async fn place_new_facts(
    placement: &NewFactPlacement<'_>,
    facts: &[FactForPage],
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    workdir: &Path,
    signals: &CartografoSignals,
) -> Result<Blueprint> {
    match placement {
        NewFactPlacement::Cartografo(llm) => {
            classify_facts(*llm, facts, foundation, registry, workdir, signals).await
        },
        NewFactPlacement::Ingest => Ok(ingest_placement_blueprint(facts)),
        NewFactPlacement::NamedThenCartografo(llm) => {
            let named = ingest_placement_blueprint(facts);
            let settled: BTreeSet<&str> = named
                .assignments
                .iter()
                .map(|a| a.fact_id.as_str())
                .collect();
            // The remainder is what the classifier left unplaced. A
            // `high`-salience fact is excluded with the same `continue` that
            // keeps it out of `named`: its home is the subject's card, reserved
            // by the routing rather than chosen, so putting it in front of the
            // Cartografo would offer a decision that is already made.
            let remainder: Vec<FactForPage> = facts
                .iter()
                .filter(|f| {
                    f.salience.as_deref() != Some("high") && !settled.contains(f.fact_id.as_str())
                })
                .cloned()
                .collect();
            if remainder.is_empty() {
                return Ok(named);
            }
            // The mass the deterministic half just added is part of what the
            // Cartografo has to judge against — a page that took four list
            // items this hour is not the empty page the carried-over count
            // says it is.
            let mut signals = signals.clone();
            for a in &named.assignments {
                *signals.page_mass.entry(a.page_slug.clone()).or_default() += 1;
            }
            let classified =
                classify_facts(*llm, &remainder, foundation, registry, workdir, &signals).await?;
            Ok(merge_blueprints(named, classified))
        },
        NewFactPlacement::OrphanFallback => Ok(Blueprint::default()),
    }
}

/// Fold the Cartografo's blueprint onto the deterministic one.
///
/// The two halves place disjoint fact sets, so assignments simply concatenate.
/// Pages can collide — both halves may propose the same slug — and the
/// deterministic one wins: it carries the style and description the user's own
/// turn supplied, which is better testata than anything the model coins for a
/// page it is meeting for the first time.
fn merge_blueprints(mut named: Blueprint, classified: Blueprint) -> Blueprint {
    let claimed: BTreeSet<String> = named.new_pages.iter().map(|p| p.slug.clone()).collect();
    named.assignments.extend(classified.assignments);
    named.new_pages.extend(
        classified
            .new_pages
            .into_iter()
            .filter(|p| !claimed.contains(&p.slug)),
    );
    named
}

// ---------- Stadio 1.5 — Il Conciliatore (LLM) ----------

/// Fold semantically-duplicate proposed pages into existing ones.
///
/// LLM, **one call per prospective wiki**: the stage passes titles and
/// descriptions through (and picks which survives a merge), so its batch
/// has to belong to one wiki for a language directive to mean anything.
/// `page_wikis` maps a proposed slug to the wiki its facts live in —
/// `slug_source_wiki`'s rule, applied one stage earlier; a proposal no
/// fact claims is homeless and rides the last group, where the plan
/// builder will decide its home or drop it.
///
/// `existing` is the forest's **concept** pages, that wiki's first
/// ([`describe_existing`]) — foundation pages are never merge targets, and
/// the wiki decides the order and the cut, not what is on the list: a
/// duplicate does not stop being one by sitting in another wiki. The homeless
/// bucket takes the forest as it comes.
///
/// Infallible: on any failure the affected group falls back to accepting
/// every proposed page with no merges (conservative — never loses a page).
/// What comes back is **not** trusted: [`build_wiki_plan`] runs
/// [`vet_accepted`] and [`vet_redirects`] over it before either half reaches
/// the plan or the registry.
pub async fn conciliate_new_pages(
    llm: &dyn LlmBackend,
    new_pages: &[NewPage],
    registry: &ConceptRegistry,
    workdir: &Path,
    page_wikis: &BTreeMap<String, String>,
    signals: &CartografoSignals,
) -> ConciliatorResult {
    if new_pages.is_empty() {
        return ConciliatorResult::default();
    }
    let by_wiki = conciliatore_groups(new_pages, page_wikis);
    let mut merged = ConciliatorResult::default();
    for (wiki, group) in by_wiki {
        // Rendered per group, not once outside the loop: the group's wiki
        // decides which pages lead the list and, past the ceiling, which of
        // the others are on it at all. The homeless bucket (`""`) has no wiki
        // to order by.
        let existing = describe_existing(
            registry,
            (!wiki.is_empty()).then_some(wiki.as_str()),
            &signals.foreign_pages,
        );
        let result = conciliate_one_wiki(
            llm,
            &group,
            &existing,
            workdir,
            &signals.language_for(&wiki),
        )
        .await;
        merged.redirects.extend(result.redirects);
        merged.accepted_new.extend(result.accepted_new);
    }
    merged
}

/// Group proposed pages by the wiki their facts live in.
///
/// A proposal no assignment claims has no prospective wiki; it lands in
/// the `""` bucket, gets the English fallback, and is left for the plan
/// builder to home or drop as before.
///
/// Deterministic: `BTreeMap` orders the wikis, proposal order survives
/// inside each.
fn conciliatore_groups(
    new_pages: &[NewPage],
    page_wikis: &BTreeMap<String, String>,
) -> BTreeMap<String, Vec<NewPage>> {
    let mut by_wiki: BTreeMap<String, Vec<NewPage>> = BTreeMap::new();
    for np in new_pages {
        let wiki = page_wikis
            .get(&slugify(&np.slug))
            .cloned()
            .unwrap_or_default();
        by_wiki.entry(wiki).or_default().push(np.clone());
    }
    by_wiki
}

/// One Conciliatore call over the proposals of a single wiki.
async fn conciliate_one_wiki(
    llm: &dyn LlmBackend,
    new_pages: &[NewPage],
    existing: &str,
    workdir: &Path,
    language_directive: &str,
) -> ConciliatorResult {
    let accept_all = || ConciliatorResult {
        redirects: BTreeMap::new(),
        accepted_new: new_pages.to_vec(),
    };
    let proposed = describe_new_pages(new_pages);
    let system = match prompts::render(
        "conciliatore",
        workdir,
        BUNDLED_CONCILIATORE_MD,
        &[
            ("locale", language_directive),
            ("existing_pages", existing),
            ("new_pages", proposed.as_str()),
        ],
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "conciliatore: prompt render failed, accepting all proposed pages");
            return accept_all();
        },
    };
    match llm
        .complete(
            CompletionRequest::new("Return the redirects/accepted_new JSON object.")
                .with_system(system)
                .with_temperature(0.1)
                // Content-scaled reply (one entry per proposed page).
                .with_max_tokens(4_000),
        )
        .await
    {
        Ok(r) => {
            let mut result = parse_json::<ConciliatorResult>(&r.text).unwrap_or_else(|| {
                tracing::warn!("conciliatore: unparseable output, accepting all proposed pages");
                accept_all()
            });
            backfill_accepted_new_style(&mut result.accepted_new, new_pages);
            result
        },
        Err(e) => {
            tracing::warn!(error = %e, "conciliatore: LLM failed, accepting all proposed pages");
            accept_all()
        },
    }
}

/// Re-attach each accepted page's writing `style` from the original proposal.
///
/// The conciliatore output schema carries only slug / title / description /
/// `page_type` / `parent_hub` — not `style` — so a parsed `accepted_new` item
/// comes back with `style: None`. Left as-is, an ingest-proposed `lista` page
/// would lose its style through conciliation and be demoted to full-prose
/// compilation. We restore it from the original `new_pages` by slug (matching
/// the canonical `slugify` form so a re-slugged proposal still lands), never
/// trusting the LLM to transcribe it. `style` is the only `NewPage` field the
/// schema drops; the rest (title / description / `page_type` / `parent_hub`)
/// the model is asked to preserve verbatim.
fn backfill_accepted_new_style(accepted: &mut [NewPage], original: &[NewPage]) {
    use std::collections::btree_map::Entry;
    // First proposal wins on a slugified-key collision — the same collapse
    // direction page materialisation uses for duplicate slugs.
    let mut styles: BTreeMap<String, Option<String>> = BTreeMap::new();
    for p in original {
        match styles.entry(slugify(&p.slug)) {
            Entry::Vacant(e) => {
                e.insert(p.style.clone());
            },
            Entry::Occupied(e) => {
                tracing::debug!(
                    slug = %e.key(),
                    "conciliatore: duplicate proposed slug in style backfill — first proposal wins"
                );
            },
        }
    }
    for np in accepted.iter_mut() {
        if np.style.is_none()
            && let Some(style) = styles.get(&slugify(&np.slug))
        {
            np.style.clone_from(style);
        }
    }
}

// ---------- orchestrator ----------

/// Build (or incrementally update) the compilation plan and persist it.
///
/// Operates over the standard wikis. Carries over prior assignments,
/// classifies only NEW facts, skips entirely on 0-new-0-removed, and computes
/// the dirty set.
///
/// `placement` chooses how NEW facts are placed ([`NewFactPlacement`]: LIGHT =
/// the page the user named, then the cheap-tier Cartografo for the rest; FULL =
/// the strong-model Cartografo; or deterministic orphan-fallback).
/// `conciliatore` is the strong-model backend for the dedup stage; `None`
/// accepts every proposed page as-is. Carried-over assignments of already-known
/// facts are preserved either way — only NEW facts flow through `placement`.
///
/// # Errors
///
/// DB / filesystem / prompt-load failures. LLM/parse failures are soft.
#[allow(clippy::too_many_lines)] // the orchestrator reads top-to-bottom; splitting hides the flow
pub async fn build_wiki_plan(
    pool: &SqlitePool,
    tree: &WikiTree,
    placement: NewFactPlacement<'_>,
    conciliatore: Option<&dyn LlmBackend>,
    now: &str,
) -> Result<CompilationPlan> {
    let facts = gather_standard_facts(pool, tree).await?;
    let (foundation, _scopes) = build_foundation_pages(pool, tree).await?;
    let registry = load_concept_registry(tree, now)?;
    let prev = load_previous_plan(tree)?;
    let current_ids: BTreeSet<String> = facts
        .iter()
        .map(|f| f.fact_id.as_str().to_owned())
        .collect();
    // Placement re-opening (the carried-placement healing bridge): the
    // parked pages' facts leave the carry-over below and flow through the
    // Cartografo again — consumed here, cleared on the plan this build
    // saves.
    //
    // **The park belongs to the STRONG pass, and to it alone.** It is filled
    // by the reviewer and the compile-failure ledger with pages whose CARRIED
    // placements deserve a second judgement — a considered re-home, not a
    // first guess. Consuming it clears it, so whichever build consumes it is
    // the one that answers the nomination. A cheap hourly build answering it
    // is how a cross-wiki move made overnight gets silently reversed before
    // morning (observed live 2026-07-04: a light build undid the refile
    // judge's move within three hours).
    //
    // So this is deliberately NOT `placement.runs_cartografo()`. The light
    // cadence now runs a Cartografo too ([`NewFactPlacement::NamedThenCartografo`]),
    // on the cheap tier, and it must still carry the park forward untouched:
    // its job is placing facts that have never had a page, never re-judging
    // one the strong model already chose. Every other placement carries it
    // forward for the older reason — it has no judgement to bring at all.
    // Only slugs the previous plan actually knows count.
    let reopen_consumable = matches!(placement, NewFactPlacement::Cartografo(_));
    let reopen: BTreeSet<String> = if reopen_consumable {
        prev.as_ref()
            .map(|p| {
                p.reopen_pages
                    .iter()
                    .filter(|s| p.pages.contains_key(s.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    } else {
        BTreeSet::new()
    };

    // Structural signals for the Cartografo (information, never a gate):
    // per-page fact mass from the carried-over placements (only facts that
    // still exist count; a re-opened page starts at zero and its pile
    // regrows through the in-run mass as batches re-assign), and — only
    // when the Cartografo actually runs — the enrollment-derived
    // identity-page scopes.
    let mut signals = CartografoSignals::default();
    if let Some(prev) = &prev {
        for (slug, page) in &prev.pages {
            if reopen.contains(slug) {
                continue;
            }
            let n = page
                .primary_facts
                .iter()
                .filter(|f| current_ids.contains(f.fact_id.as_str()))
                .count();
            if n > 0 {
                signals.page_mass.insert(slug.clone(), n);
            }
        }
    }
    if placement.runs_cartografo() {
        signals.subject_scopes = subject_scopes_for(pool, &facts).await?;
        // Same shape, same moment: the page names both LLM stages coin are
        // read by a person, so each batch carries its wiki's language.
        signals.wiki_locales = wiki_locales_for(pool, tree, &facts).await;
        // And what each wiki's batches are shown of the rest of the forest —
        // everything while it fits one list, the nearest pages once it does
        // not. Computed once per run: the list is per wiki, not per batch.
        let wikis: BTreeSet<String> = facts.iter().map(|f| f.source_wiki_id.clone()).collect();
        signals.foreign_pages =
            foreign_page_offers(pool, tree, &foundation, &registry, &wikis).await;
    }

    let mut blueprint = Blueprint::default();
    if let Some(prev) = &prev {
        let mut prev_assigned = extract_assigned_fact_ids(prev);
        // Re-opened pages: their facts fall out of the carried set and
        // re-enter the to-place pool below.
        if !reopen.is_empty() {
            prev_assigned.retain(|_, slug| !reopen.contains(slug));
        }
        let new_facts: Vec<FactForPage> = facts
            .iter()
            .filter(|f| !prev_assigned.contains_key(f.fact_id.as_str()))
            .cloned()
            .collect();
        let removed = prev_assigned
            .keys()
            .filter(|id| !current_ids.contains(id.as_str()))
            .count();
        // A dashboard comment corrects a claim in place (same `fact_id`,
        // new text), and a validity closure stamps `valid_to`/`decay_reason`
        // in place. Neither is "new" nor "removed", so the skip must also
        // notice a carried-over fact whose render content drifted — otherwise
        // the correction/closure never reaches the prose.
        let content_changed = any_content_drift(&facts, prev);
        if new_facts.is_empty() && removed == 0 && !content_changed {
            // Reuse the prior plan wholesale — but still honor (and clear) any
            // force-dirty pages an out-of-band re-home parked on it: those
            // pages' fingerprints match by construction, so this is their only
            // route to a recompile.
            let mut reused = prev.clone();
            now.clone_into(&mut reused.generated_at);
            reused.dirty_pages = prev
                .force_dirty
                .iter()
                .filter(|s| prev.pages.contains_key(s.as_str()))
                .cloned()
                .collect();
            reused.force_dirty = Vec::new();
            // A parked re-open with no surviving facts re-places nothing:
            // a Cartografo build consumes it here too, or it would sit
            // parked forever. A non-Cartografo build keeps the park (the
            // `prev.clone()` above already carries it).
            if reopen_consumable {
                reused.reopen_pages = Vec::new();
            }
            if reused.dirty_pages.is_empty() {
                tracing::info!("planner: SKIPPED (0 new, 0 removed, 0 content-changed)");
            } else {
                tracing::info!(
                    forced = reused.dirty_pages.len(),
                    "planner: plan reused, compiling force-dirty pages only"
                );
            }
            // The card heal belongs on THIS path too, and mostly on this one:
            // a page's written card changes when the page is compiled, which
            // is precisely a build with no new facts. Reached only by the
            // reuse branch it would almost never run at all.
            let mut reused_registry = registry.clone();
            if heal_page_cards(tree, &mut reused, &mut reused_registry) > 0 {
                save_concept_registry(tree, &reused_registry)?;
            }
            save_plan(tree, &reused)?;
            return Ok(reused);
        }
        // Carry over prior assignments for facts that still exist — except
        // the re-opened pages': those flow through `place_new_facts` above
        // as part of `new_facts`, so the Cartografo re-judges them.
        for (slug, page) in &prev.pages {
            if reopen.contains(slug) {
                continue;
            }
            for f in &page.primary_facts {
                if current_ids.contains(f.fact_id.as_str()) {
                    blueprint.assignments.push(Assignment {
                        fact_id: f.fact_id.as_str().to_owned(),
                        page_slug: slug.clone(),
                    });
                }
            }
        }
        if !new_facts.is_empty() {
            let bp = place_new_facts(
                &placement,
                &new_facts,
                &foundation,
                &registry,
                tree.workdir(),
                &signals,
            )
            .await?;
            blueprint.assignments.extend(bp.assignments);
            blueprint.new_pages.extend(bp.new_pages);
        }
    } else {
        blueprint = place_new_facts(
            &placement,
            &facts,
            &foundation,
            &registry,
            tree.workdir(),
            &signals,
        )
        .await?;
    }

    let mut conciliation = if blueprint.new_pages.is_empty() {
        ConciliatorResult::default()
    } else if let Some(llm) = conciliatore {
        // Which wiki each proposal is destined for, by the same rule the
        // plan builder applies a few lines down (`slug_source_wiki`): the
        // source wiki of the first fact assigned to it.
        let mut page_wikis: BTreeMap<String, String> = BTreeMap::new();
        let fact_wikis: BTreeMap<&str, &str> = facts
            .iter()
            .map(|f| (f.fact_id.as_str(), f.source_wiki_id.as_str()))
            .collect();
        for a in &blueprint.assignments {
            let Some(wiki) = fact_wikis.get(a.fact_id.as_str()) else {
                continue;
            };
            page_wikis
                .entry(slugify(&a.page_slug))
                .or_insert_with(|| (*wiki).to_owned());
        }
        conciliate_new_pages(
            llm,
            &blueprint.new_pages,
            &registry,
            tree.workdir(),
            &page_wikis,
            &signals,
        )
        .await
    } else {
        ConciliatorResult {
            redirects: BTreeMap::new(),
            accepted_new: blueprint.new_pages.clone(),
        }
    };
    // Vet the Conciliatore's output before either half of it reaches the plan:
    // its accepted pages are materialised AND persisted into the registry, and
    // its redirects rewrite assignments here and again in the plan builder. Both
    // ran unchecked until 2026-08-10 — `vet_proposal` sits one stage earlier, on
    // the Cartografo's raw output only.
    let accepted = std::mem::take(&mut conciliation.accepted_new);
    conciliation.accepted_new = accepted
        .into_iter()
        .filter_map(|np| vet_accepted(np, &foundation))
        .collect();
    let proposed = std::mem::take(&mut conciliation.redirects);
    conciliation.redirects =
        vet_redirects(proposed, &foundation, &registry, &conciliation.accepted_new);

    if !conciliation.redirects.is_empty() {
        for a in &mut blueprint.assignments {
            let s = slugify(&a.page_slug);
            if let Some(r) = conciliation.redirects.get(&s) {
                r.clone_into(&mut a.page_slug);
            }
        }
    }

    let (mut plan, mut updated_registry) = build_compilation_plan(
        &facts,
        &foundation,
        &blueprint,
        &conciliation,
        &registry,
        now,
    );
    // A page's card is whatever the writer wrote on it, not the guess the
    // classifier made before it existed — see `heal_page_cards`. Runs before
    // the receipts so a page minted THIS build still records the frame it was
    // invented with, which is the thing the operator needs to see.
    heal_page_cards(tree, &mut plan, &mut updated_registry);
    // The refile-candidate park survives plan rebuilds (drained only by
    // the refile sweep); the re-open park survives every build except the
    // Cartografo one that consumes it.
    if let Some(prev) = &prev {
        plan.refile_candidates.clone_from(&prev.refile_candidates);
        if !reopen_consumable {
            plan.reopen_pages = prev
                .reopen_pages
                .iter()
                .filter(|s| plan.pages.contains_key(s.as_str()))
                .cloned()
                .collect();
        }
    }
    if !reopen.is_empty() {
        tracing::info!(
            reopened = reopen.len(),
            "planner: parked pages re-opened — their placements re-judged"
        );
    }
    // Every page the machine invented this cycle gets a receipt the operator
    // can read. Not a gate: the nightly pass cannot stop and wait for an
    // answer, so the record is **born-applied** and revertable — the same
    // act-first-with-a-receipt rung the refile sweep uses.
    //
    // This closes a promise the module doc had been making since the cutover
    // while the only proposal kinds that existed were about *wikis*: twelve
    // container pages were minted over three weeks with nothing anywhere for
    // the founder to read (2026-08-04).
    if let Some(prev) = &prev {
        record_minted_pages(pool, prev, &plan, now).await;
    }
    plan.dirty_pages = match &prev {
        Some(prev) => {
            // Union in any force-dirty pages an out-of-band re-home parked on
            // the prior plan (their carried-over fingerprints match, so the
            // compute alone would skip them), then clear the flag.
            let mut dirty = compute_dirty_pages(prev, &plan);
            for s in &prev.force_dirty {
                if plan.pages.contains_key(s) && !dirty.contains(s) {
                    dirty.push(s.clone());
                }
            }
            dirty.sort();
            dirty
        },
        None => plan.compilation_order.clone(),
    };
    save_plan(tree, &plan)?;
    save_concept_registry(tree, &updated_registry)?;
    tracing::info!(
        pages = plan.pages.len(),
        dirty = plan.dirty_pages.len(),
        facts = plan.fact_count,
        "planner: plan built"
    );
    Ok(plan)
}

/// Adopt each page's **written** card into the plan and the registry.
///
/// [`PagePlan::description`] is seeded once — from the ingest classifier's
/// `page_description` proposal — and the registry then persists that first
/// guess **forever**: nothing ever read back what the page turned out to say.
/// Meanwhile the writer puts its own card in the page's testata on every
/// compile, so the two diverge, and the stale one is the copy the models are
/// shown: the Cronista's page index carries every page's description
/// (`compiler::page_index_block`), its own line included, and the Hub Writer's
/// `{snippet}` carries its children's.
///
/// **That is how an invented frame becomes permanent.** Production,
/// 2026-07-24 (card 57): a turn complaining that an assistant had signed the
/// sender up for a fair minted a page described as *«Progetti e attività
/// relativi a …»* — a whole area of work nobody had described — and the
/// description stayed in the plan, was fed back to the compiler, and grew into
/// a paragraph about managing external collaborations. The *fact* was roughly
/// faithful; the **frame** was invented, and nothing could correct it, because
/// the guess outlived every page that was written from it.
///
/// Reading the page back closes the loop: the card the writer produced from
/// the page's actual facts replaces the guess, so a bad first frame is
/// **self-correcting** instead of permanent.
///
/// Best-effort and idempotent — an unreadable or testata-less page is left
/// alone. It never marks a page dirty by itself: [`page_fingerprint`] does not
/// carry the description, so the correction rides the next compile that
/// happens for its own reasons.
fn heal_page_cards(
    tree: &WikiTree,
    plan: &mut CompilationPlan,
    registry: &mut ConceptRegistry,
) -> usize {
    let mut healed = 0usize;
    for (slug, page) in &mut plan.pages {
        let Ok(wid) = crate::types::WikiId::parse(&page.wiki_id) else {
            continue;
        };
        let Ok(handle) = tree.locate(&wid) else {
            continue;
        };
        let Ok(contents) = handle.read_page(std::path::Path::new(&page.page_path)) else {
            continue;
        };
        let Some(written) = testata_description(&contents) else {
            continue;
        };
        if written == page.description {
            continue;
        }
        page.description.clone_from(&written);
        if let Some(entry) = registry.entries.get_mut(slug) {
            entry.description.clone_from(&written);
        }
        healed += 1;
    }
    if healed > 0 {
        tracing::info!(
            healed,
            "planner: page cards adopted from what the writer actually wrote"
        );
    }
    healed
}

/// The `description:` line of a page's testata — its **card**.
///
/// Scoped to the leading `---` fence on purpose: a `description:` line in the
/// body is prose somebody wrote, not the page's card.
fn testata_description(page: &str) -> Option<String> {
    let mut lines = page.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let t = line.trim();
        if t == "---" {
            return None;
        }
        if let Some(rest) = t.strip_prefix("description:") {
            let v = rest.trim().trim_matches(['"', '\'']).trim();
            return (!v.is_empty()).then(|| v.to_owned());
        }
    }
    None
}

/// Emit one born-applied `page_create` receipt per page this build invented.
///
/// "Invented" is `next.pages ∖ prev.pages`, the same set
/// [`compute_dirty_pages`] already walks — a page nobody asked for by name,
/// minted because the Cartografo judged some facts fitted no existing page.
/// Foundation pages are excluded: a person's card and a wiki's buffer appear
/// because a *user* or a *wiki* was created, which has its own visible route.
///
/// Best-effort by contract. A failure here must never fail the nightly plan:
/// a missing receipt is a gap in the record, a failed plan is a night of no
/// memory maintenance at all. Logged at `warn!` and swallowed.
async fn record_minted_pages(
    pool: &SqlitePool,
    prev: &CompilationPlan,
    next: &CompilationPlan,
    now: &str,
) {
    for (slug, page) in &next.pages {
        if prev.pages.contains_key(slug) || page.page_type.is_foundation() {
            continue;
        }
        let context = serde_json::json!({
            "slug": slug,
            "wiki_id": page.wiki_id,
            "page_path": page.page_path,
            "title": page.title,
            "description": page.description,
            "page_type": format!("{:?}", page.page_type),
            "parent_hub": page.parent_hub,
            "fact_count": page.primary_facts.len(),
            "minted_at": now,
        });
        let params = crate::proposals::EmitParams::new(
            crate::proposals::kind::PAGE_CREATE,
            context.clone(),
            serde_json::json!([]),
        );
        if let Err(e) =
            crate::proposals::emit_applied_proposal(pool, params, context, Some("planner")).await
        {
            tracing::warn!(
                slug = %slug,
                error = %e,
                "planner: could not record a minted page — the page exists, the receipt does not"
            );
        }
    }
}

/// Gather every active fact in the standard wikis as [`FactForPage`]s, in a
/// deterministic (fact-id) order.
async fn gather_standard_facts(pool: &SqlitePool, tree: &WikiTree) -> Result<Vec<FactForPage>> {
    let mut out = Vec::new();
    for d in tree.walk()? {
        // "standard" = "not smart": smart wikis are
        // smart-consumer-owned and out of the compiler's perimeter.
        if d.meta.smart {
            continue;
        }
        for row in fact_index::find_active_in_wiki(pool, d.meta.wiki_id.as_str()).await? {
            // The reserved channel pages (`rules.md`, `projects.md`) are their
            // own pipelines' perimeter, not the compiler's: their facts are
            // written directly and read back keyed on that path. The compiler
            // must NOT gather them — absent from the persisted plan they would
            // look "new", orphan-fall-back onto the wiki's buffer, and their channel
            // (which filters on the page) would stop seeing them.
            // (engine_rule governance is raw `rules.md` prose, not a
            // `fact_index` row, so only behaviour-rule rows are spared here.)
            if crate::wiki::is_channel_page(&row.source_path) {
                continue;
            }
            out.push(FactForPage::from_row(&row));
        }
    }
    out.sort_by(|a, b| a.fact_id.as_str().cmp(b.fact_id.as_str()));
    Ok(out)
}

// ---------- prompt context + JSON helpers ----------

/// Fact mass of one page as the Cartografo sees it (0 when unknown).
fn mass_of(mass: &BTreeMap<String, usize>, slug: &str) -> usize {
    mass.get(slug).copied().unwrap_or(0)
}

/// The canonical wire token of a [`PageType`] — the exact `snake_case` tag
/// the enum's serde produces and the LLM stages must emit back.
///
/// Every surface that shows a page's type — the prompt-context renderers
/// here and the compiler's page frontmatter — routes through this one
/// mapping (`person` / `group_theme` / `concept_hub` / `concept_leaf`),
/// never the Rust `Debug` (`ConceptHub`): a model that mirrors the
/// `PascalCase` it was shown produces a `page_type` serde cannot parse,
/// collapsing the whole batch to the accept-all fallback. Kept in lockstep
/// with the enum's serde `rename_all` by the
/// `page_type_tag_is_the_serde_wire_form` test.
#[must_use]
pub const fn page_type_tag(pt: PageType) -> &'static str {
    match pt {
        PageType::Person => "person",
        PageType::GroupTheme => "group_theme",
        PageType::WikiBuffer => "wiki_buffer",
        PageType::ConceptHub => "concept_hub",
        PageType::ConceptLeaf => "concept_leaf",
    }
}

/// The foundation pages the batch may place onto: **this wiki's, then every
/// other wiki's identity card**.
///
/// The scoping this replaced was justified as a correctness property — a
/// fact's wiki is decided at capture, so the structure that should receive it
/// is the one it is already in. Under the ruling of 2026-08-10 that is only
/// half true: capture decides where a fact *starts*, and a fact is free to
/// live in any wiki. The half that stayed true is that the identity-page
/// discipline governs which card may hold it, and that discipline is
/// enforced per fact by the `identity_pages=` tag, not by the page list.
///
/// Fencing the list did not uphold the discipline; it made the discipline's
/// own instruction unfollowable. *«Home it on the subject's own pages
/// instead»* is what the prompt tells the model to do with a fact about
/// somebody else — and the subject's card lives in the subject's wiki, which
/// was the one place the list could not name. A group-owned fact about Bruno,
/// captured in the family wiki, could be kept off the family card and still
/// not be put on Bruno's.
///
/// **Foreign buffers are not offered.** A buffer is where a fact of *that*
/// wiki waits for a home; parking a fact in another wiki's inbox is not a
/// placement, and the local buffer is already the fallback for a fact with no
/// page. Cards are bounded by the product limits (24 users, 8 groups), so this
/// list does not grow with the memory and is never cut.
fn describe_foundation(
    foundation: &BTreeMap<String, PagePlan>,
    wiki: &str,
    mass: &BTreeMap<String, usize>,
) -> String {
    let mut lines: Vec<String> = foundation
        .values()
        .filter(|p| p.wiki_id == wiki)
        .map(|p| match p.page_type {
            PageType::Person => format!(
                "- [{}] {} — {} (parent_hub: {}) | facts: {}",
                page_type_tag(p.page_type),
                p.slug,
                p.title,
                p.parent_hub.as_deref().unwrap_or("—"),
                mass_of(mass, &p.slug),
            ),
            PageType::GroupTheme => format!(
                "- [{}] {} — {} | scope: {} | facts: {}",
                page_type_tag(p.page_type),
                p.slug,
                p.title,
                p.owner_scope.as_deref().unwrap_or("—"),
                mass_of(mass, &p.slug),
            ),
            PageType::WikiBuffer => format!(
                "- [{}] {} — {} (parent_hub: {}) | {} | facts: {}",
                page_type_tag(p.page_type),
                p.slug,
                p.title,
                p.parent_hub.as_deref().unwrap_or("—"),
                if p.description.is_empty() {
                    "—"
                } else {
                    p.description.as_str()
                },
                mass_of(mass, &p.slug),
            ),
            _ => format!("- [{}] {}", page_type_tag(p.page_type), p.slug),
        })
        .collect();
    // The other wikis' identity cards, each named with the wiki it belongs to
    // so the model is choosing a page in a place, not a bare slug.
    lines.extend(
        foundation
            .values()
            .filter(|p| p.wiki_id != wiki && p.page_type.is_identity_card())
            .map(|p| {
                format!(
                    "- [{}] {} — {} | wiki: {} | facts: {}",
                    page_type_tag(p.page_type),
                    p.slug,
                    p.title,
                    p.wiki_id,
                    mass_of(mass, &p.slug),
                )
            }),
    );
    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        lines.join("\n")
    }
}

/// The concept pages the batch may place onto: **this wiki's in full, then the
/// rest of the forest's** — the registry's, plus what earlier batches of this
/// run proposed.
///
/// Local first and never cut: that is where most of a batch's facts belong,
/// where the dedup question actually bites (*«do not create a page equivalent
/// to one that exists»*), and where every page this batch coins is born. The
/// foreign half is what [`ForeignPages`] decides — all of it while the forest
/// fits one list, the nearest [`FOREIGN_SELECTION_PAGES`] once it does not.
///
/// Each foreign line carries `wiki: <id>`, because choosing a page is choosing
/// a place, and a slug alone does not say which.
fn describe_concepts(
    registry: &ConceptRegistry,
    wiki: &str,
    this_run: &[(&NewPage, &str)],
    mass: &BTreeMap<String, usize>,
    foreign: &ForeignPages,
) -> String {
    let line = |page_type, slug: &str, title: &str, description: &str, home: Option<&str>| {
        // The `wiki:` field appears only on a page of another wiki: on the
        // batch's own pages it would be the same id on every line.
        let home = home.map_or_else(String::new, |h| format!("wiki: {h} | "));
        format!(
            "- [{}] {slug} — {title} | {home}{description} | facts: {}",
            page_type_tag(page_type),
            mass_of(mass, slug),
        )
    };
    let mut lines: Vec<String> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id == wiki)
        .map(|e| line(e.page_type, &e.slug, &e.title, &e.description, None))
        .collect();
    // Nearest first, and no re-sort by slug afterwards: where a list is cut
    // the order IS the selection, and rendering it alphabetically would hand
    // back the ordering the selection exists to replace.
    let mut foreign_entries: Vec<&ConceptRegistryEntry> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id != wiki && foreign.offers(wiki, &e.slug))
        .collect();
    foreign_entries.sort_by_key(|e| foreign.rank(wiki, &e.slug));
    lines.extend(foreign_entries.into_iter().map(|e| {
        line(
            e.page_type,
            &e.slug,
            &e.title,
            &e.description,
            Some(&e.wiki_id),
        )
    }));
    for (np, home) in this_run {
        lines.push(format!(
            "{} (proposed this run)",
            line(
                np.page_type,
                &np.slug,
                &np.title,
                &np.description,
                (*home != wiki).then_some(*home),
            )
        ));
    }
    if lines.is_empty() {
        "(none yet)".to_owned()
    } else {
        lines.join("\n")
    }
}

/// The page names taken by pages this batch was **not shown**, as bare slugs.
///
/// The list had two jobs and the ruling of 2026-08-10 left it one. It is no
/// longer *«the other wikis' pages, which you may neither read nor file
/// into»* — those are offered above now, described, and choosing one is
/// legitimate. What survives is the collision half, and it survives intact: a
/// plan is keyed by slug across the whole forest
/// ([`CompilationPlan::pages`]), so a name is unique memory-wide, and a batch
/// that **coins** a name another wiki already owns would have
/// [`build_compilation_plan`] file its facts onto that wiki's page — a
/// destination chosen by nobody. Choosing a page on purpose is a judgement;
/// colliding with its name is an accident.
///
/// So the list holds exactly what the described lists leave out: the foreign
/// **buffers** (never a destination), and, past the ceiling, the foreign
/// concept pages the selection did not carry.
///
/// Bare names, not full lines: it answers "is this name free?" and nothing
/// else, so a slug costs a few tokens where a described page line costs ten
/// times that.
///
/// **Never truncated**, and that is why it carries no ordering rule: a
/// collision guard missing an entry reports "free" for a taken name, which is
/// worse than no guard at all.
fn describe_taken_slugs(
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    wiki: &str,
    foreign: &ForeignPages,
) -> String {
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    for p in foundation.values() {
        if p.wiki_id != wiki && !p.page_type.is_identity_card() {
            taken.insert(p.slug.as_str());
        }
    }
    for e in registry.entries.values() {
        if e.wiki_id != wiki && !foreign.offers(wiki, &e.slug) {
            taken.insert(e.slug.as_str());
        }
    }
    // This run's proposals are all shown, wherever they were proposed — a page
    // about to exist is a page that can be reused rather than duplicated — so
    // none of them belongs here.
    if taken.is_empty() {
        "(none)".to_owned()
    } else {
        taken.into_iter().collect::<Vec<_>>().join(", ")
    }
}

fn describe_facts(batch: &[FactForPage], signals: &CartografoSignals) -> String {
    batch
        .iter()
        .map(|f| {
            format!(
                "[id:{}] \"{}\" type={} subject={} identity_pages={}",
                f.fact_id,
                f.text.replace('\n', " "),
                f.fact_type.as_deref().unwrap_or("other"),
                f.subject,
                signals.identity_scope_tag(&f.subject),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The pages a proposal may be folded into: `wiki`'s **concept** pages, or —
/// for the homeless bucket (`None`, a proposal no assignment claims) — the
/// forest's.
///
/// **Foundation pages are not offered, because a merge cannot land on one.**
/// A card carries a subject's identity and a buffer is where a fact waits for
/// a home; neither is a topic a page can become part of. Listing them was
/// worse than idle: they were rendered *first*, the buffer node carries the
/// wiki's own title and scope as its description (see [`seed_wiki_buffers`]),
/// so `notes.md` read to the model like the wiki's canonical topic page — and
/// the prompt's standing bias is *«when in doubt, prefer the redirect»*.
/// [`vet_redirects`] refuses one named anyway.
///
/// **The forest's concept pages, this wiki's first.** The scoping this
/// replaced was justified as *«a redirect is a merge, so folding a proposal
/// into another wiki's page would move this wiki's facts there»* — true, and
/// not a reason: moving them there is legitimate (founder, 2026-08-10). The
/// stage's job is to stop two pages saying the same thing, and a duplicate
/// does not become a different page by sitting in another wiki. Un-scoping it
/// is also what keeps this stage consistent with the one before it: the
/// Cartografo may now assign across the forest, so a proposal it made can be
/// the duplicate of a page anywhere in it.
///
/// `wiki` therefore decides **order and cut**, not membership: the asking
/// wiki's pages first and never cut, then whatever [`ForeignPages`] offers —
/// all of it below the ceiling, the nearest slice above it. The homeless
/// bucket (`None`, a proposal no assignment claims) has no wiki to order by
/// and takes the forest as it comes.
fn describe_existing(
    registry: &ConceptRegistry,
    wiki: Option<&str>,
    foreign: &ForeignPages,
) -> String {
    let render = |e: &ConceptRegistryEntry| {
        format!(
            "- [{}] {} — {} | wiki: {} | {}",
            page_type_tag(e.page_type),
            e.slug,
            e.title,
            e.wiki_id,
            e.description
        )
    };
    let Some(wiki) = wiki else {
        let lines: Vec<String> = registry.entries.values().map(render).collect();
        return if lines.is_empty() {
            "(none)".to_owned()
        } else {
            lines.join("\n")
        };
    };
    let mut lines: Vec<String> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id == wiki)
        .map(render)
        .collect();
    let mut foreign_entries: Vec<&ConceptRegistryEntry> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id != wiki && foreign.offers(wiki, &e.slug))
        .collect();
    // Nearest first where the list is cut — never re-sorted by slug.
    foreign_entries.sort_by_key(|e| foreign.rank(wiki, &e.slug));
    lines.extend(foreign_entries.into_iter().map(render));
    if lines.is_empty() {
        "(none)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn describe_new_pages(new_pages: &[NewPage]) -> String {
    new_pages
        .iter()
        .map(|np| {
            format!(
                "- [{}] {} — {} | {} (parent_hub: {})",
                page_type_tag(np.page_type),
                np.slug,
                np.title,
                np.description,
                np.parent_hub.as_deref().unwrap_or("—")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a JSON value from an LLM response, tolerating a markdown code fence and
/// surrounding prose (takes the first `{` .. last `}`).
fn parse_json<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<T>(&raw[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `compilation-plan.json` written before the rename must still parse.
    ///
    /// `FactForPage.subject` carries no `#[serde(default)]`, so without the
    /// alias the whole plan file fails to deserialize — and `load_previous_plan`
    /// maps any parse error to `Ok(None)`, i.e. "there was never a plan". The
    /// engine then resets its fingerprints and recompiles every page through the
    /// Cronista: one metered LLM call per page, with no error and nothing to
    /// attribute the cost to. The alias is the only thing between an upgrade and
    /// that bill, and a typo in it would be invisible.
    #[test]
    fn a_plan_file_written_before_the_rename_still_parses() {
        let legacy = serde_json::json!({
            "fact_id": crate::types::SAMPLE_UUID_V7,
            "text": "Alice prefers coffee black",
            "fact_type": "preference",
            "owner": "user:alice",
            "sender": "user:alice",
            "source_wiki_id": "alice",
        });
        let f: FactForPage = serde_json::from_value(legacy).expect("the pre-rename key must parse");
        assert_eq!(f.subject, Principal::User("alice".into()));

        // Control: an unrecognised key really would leave the field unset, so
        // the assertion above cannot pass for some other reason.
        let bogus = serde_json::json!({
            "fact_id": crate::types::SAMPLE_UUID_V7,
            "text": "x",
            "fact_type": null,
            "proprietor": "user:alice",
            "sender": null,
            "source_wiki_id": "alice",
        });
        assert!(serde_json::from_value::<FactForPage>(bogus).is_err());
    }

    fn fact(id_seed: u8, text: &str, subject: &str, src: &str) -> FactForPage {
        // Deterministic UUIDv7-shaped ids for tests.
        let id = format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_seed:02x}");
        FactForPage {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&id).unwrap(),
            text: text.to_owned(),
            fact_type: Some("bio".to_owned()),
            subject: subject.parse::<Principal>().unwrap(),
            allow: Vec::new(),
            sender: None,
            source_wiki_id: src.to_owned(),
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

    /// The light cadence's two halves, and the order that makes it safe.
    ///
    /// A page the USER named is settled deterministically and **never reaches
    /// the model**: a `lista` is a set, and a shopping item re-homed by a
    /// classifier an hour after it was added leaves the list it was added to.
    /// Everything the classifier left unplaced — which since v2.59 is every
    /// prose fact — is exactly what the Cartografo is there for.
    #[tokio::test]
    async fn light_placement_settles_the_named_page_and_shows_the_model_only_the_rest() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let llm = FakeLlmBackend::new("flash", "{\"assignments\":[],\"new_pages\":[]}");

        let mut named = fact(1, "detersivo per i piatti", "group:famiglia", "famiglia");
        named.target_page = Some("spesa.md".to_owned());
        named.style = Some("lista".to_owned());

        let mut unplaced = fact(
            2,
            "Bob ha cominciato nuoto il martedi",
            "user:franz",
            "franz",
        );
        unplaced.target_page = Some(crate::wiki::NOTES_FILENAME.to_owned());

        let mut reserved = fact(3, "Franz e celiaco", "user:franz", "franz");
        reserved.target_page = Some(crate::wiki::NOTES_FILENAME.to_owned());
        reserved.salience = Some("high".to_owned());

        let bp = place_new_facts(
            &NewFactPlacement::NamedThenCartografo(&llm),
            &[named.clone(), unplaced.clone(), reserved.clone()],
            &BTreeMap::new(),
            &ConceptRegistry::empty("2026-08-09T00:00:00Z"),
            dir.path(),
            &CartografoSignals::default(),
        )
        .await
        .expect("placement");

        let seen = format!(
            "{}{}",
            llm.last_system_prompt().unwrap_or_default(),
            llm.last_prompt().unwrap_or_default()
        );
        assert!(
            !seen.contains("detersivo"),
            "the list item the user named a page for must never be offered to the model"
        );
        assert!(
            seen.contains("nuoto"),
            "the unplaced prose fact is precisely what the light Cartografo is for"
        );
        assert!(
            !seen.contains("celiaco"),
            "a high-salience fact is reserved for the subject's card by the routing, \
             so it is not a decision to offer"
        );
        assert_eq!(
            bp.assignments
                .iter()
                .map(|a| a.page_slug.as_str())
                .collect::<Vec<_>>(),
            vec!["spesa"],
            "only the deterministic half assigned anything — the fake returned no verdict"
        );
    }

    /// With no cheap tier to run it on, the light pass keeps the deterministic
    /// half and places nothing else — the pre-2026-08-09 behaviour, which is
    /// the right degradation and not a silent one (`placement.label()` is on
    /// the compile log).
    #[tokio::test]
    async fn light_placement_without_a_model_still_honours_the_named_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut named = fact(1, "detersivo", "group:famiglia", "famiglia");
        named.target_page = Some("spesa.md".to_owned());
        named.style = Some("lista".to_owned());
        let mut unplaced = fact(2, "nuoto il martedi", "user:franz", "franz");
        unplaced.target_page = Some(crate::wiki::NOTES_FILENAME.to_owned());

        let bp = place_new_facts(
            &NewFactPlacement::Ingest,
            &[named, unplaced],
            &BTreeMap::new(),
            &ConceptRegistry::empty("2026-08-09T00:00:00Z"),
            dir.path(),
            &CartografoSignals::default(),
        )
        .await
        .expect("placement");
        assert_eq!(bp.assignments.len(), 1);
        assert_eq!(bp.assignments[0].page_slug, "spesa");
    }

    fn person(slug: &str) -> PagePlan {
        PagePlan {
            slug: slug.to_owned(),
            title: capitalize(slug),
            description: format!("Personal page of {slug}"),
            style: None,
            page_type: PageType::Person,
            owner_scope: None,
            parent_hub: None,
            child_leaves: Vec::new(),
            primary_facts: Vec::new(),
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
            wiki_id: slug.to_owned(),
            page_path: "index.md".to_owned(),
        }
    }

    /// A page the machine invents leaves a receipt; a foundation page does not.
    ///
    /// Twelve container pages were minted over three weeks with nothing
    /// anywhere for the operator to read, while the module doc asserted the
    /// opposite. The negative half matters as much: a person's card appears
    /// because a *user* was enrolled, which is already visible, so receipting
    /// it would bury the ones that are actually the machine's own idea.
    #[tokio::test]
    async fn a_page_the_machine_invents_leaves_a_receipt_and_a_foundation_page_does_not() {
        let (_workdir, pool) = crate::test_db::TestWorkdir::with_db().await;
        let mut prev_pages = BTreeMap::new();
        prev_pages.insert("alice".to_owned(), person("alice"));
        let prev = CompilationPlan {
            pages: prev_pages,
            merged_pages: vec![],
            link_graph: BTreeMap::new(),
            compilation_order: vec![],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: vec![],
            force_dirty: vec![],
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };

        let mut next = prev.clone();
        next.pages.insert("bob".to_owned(), person("bob"));
        let mut leaf = person("ricette");
        leaf.page_type = PageType::ConceptLeaf;
        leaf.page_path = "ricette.md".to_owned();
        leaf.wiki_id = "alice".to_owned();
        next.pages.insert("ricette".to_owned(), leaf);

        record_minted_pages(&pool, &prev, &next, "2026-08-04T00:00:00Z").await;

        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT kind, status, context FROM structure_proposals ORDER BY proposal_id",
        )
        .fetch_all(&pool)
        .await
        .expect("rows");
        assert_eq!(rows.len(), 1, "one receipt, for the invented page only");
        assert_eq!(rows[0].0, crate::proposals::kind::PAGE_CREATE);
        assert_eq!(rows[0].1, "applied", "born-applied, never a gate");
        assert!(
            rows[0].2.contains("ricette"),
            "the receipt must name the page: {}",
            rows[0].2
        );
        assert!(
            !rows[0].2.contains("bob"),
            "a person's card is not the machine's idea: {}",
            rows[0].2
        );
    }

    /// Insert a promoted fact in alice's wiki carrying an ingest placement
    /// proposal (`target_page` / `style` / `page_description`) on `fact_index`.
    /// Returns the fact id.
    async fn plant_alice_fact(
        pool: &SqlitePool,
        id_tail: &str,
        text: &str,
        target_page: Option<&str>,
        style: Option<&str>,
        desc: Option<&str>,
    ) -> FactId {
        let fid = FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_tail}")).unwrap();
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
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: target_page.map(str::to_owned),
                style: style.map(str::to_owned),
                page_description: desc.map(str::to_owned),
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
        fid
    }

    #[test]
    fn slugify_is_canonical() {
        assert_eq!(slugify("Salute & Benessere"), "salute_benessere");
        assert_eq!(slugify("  Frodo  "), "frodo");
        assert_eq!(slugify("a--b__c"), "a_b_c");
    }

    #[test]
    fn canonical_page_path_is_one_spelling_per_concept() {
        // The two spellings that fragmented the famiglia wiki must fold
        // onto the same canonical name.
        assert_eq!(
            canonical_page_path("orari-matteo"),
            Some("orari_matteo.md".to_owned())
        );
        assert_eq!(
            canonical_page_path("Orari Matteo.md"),
            Some("orari_matteo.md".to_owned())
        );
        // Nested paths canonicalise per segment.
        assert_eq!(
            canonical_page_path("Diario/Episodi Marzo"),
            Some("diario/episodi_marzo.md".to_owned())
        );
        // Traversal / noise segments are refused, not repaired.
        assert_eq!(canonical_page_path("../escape"), None);
        assert_eq!(canonical_page_path("---"), None);
        assert_eq!(canonical_page_path("   "), None);
    }

    #[test]
    fn placement_slug_flattens_paths_and_sends_index_to_orphan() {
        // A concrete page → its flattened leaf slug (the `.md` is stripped BEFORE
        // slugify, so it never folds into a trailing `_md`).
        assert_eq!(
            placement_slug("preferenze.md"),
            Some("preferenze".to_owned())
        );
        assert_eq!(placement_slug("spesa"), Some("spesa".to_owned()));
        assert_eq!(
            placement_slug("Spesa Famiglia"),
            Some("spesa_famiglia".to_owned())
        );
        // A path flattens to ONE leaf — the classifier does not do folders/nesting (REM).
        assert_eq!(
            placement_slug("recipes/dinner.md"),
            Some("recipes_dinner".to_owned())
        );
        // `index.md` / empty → None: the foundation page, via orphan-fallback —
        // never a concept page named "index".
        assert_eq!(placement_slug("index.md"), None);
        assert_eq!(placement_slug("index"), None);
        assert_eq!(placement_slug(""), None);
        assert_eq!(placement_slug("  "), None);
        // `rules.md` → None: the reserved user-policy page is never a
        // fact-bearing concept page; a mis-targeted fact orphan-falls-back.
        assert_eq!(placement_slug("rules.md"), None);
        assert_eq!(placement_slug("rules"), None);
    }

    #[test]
    fn ingest_placement_blueprint_assigns_to_target_dedups_and_skips_index() {
        // Two facts → the same `spesa` page (dedup to ONE NewPage, first
        // style/description wins), one fact → `index.md` (no assignment/page,
        // left for orphan-fallback), one fact → no proposal at all (skipped).
        let mut latte = fact(1, "latte", "user:alice", "alice");
        latte.target_page = Some("spesa.md".to_owned());
        latte.style = Some("lista".to_owned());
        latte.page_description = Some("cosa comprare".to_owned());
        let mut pane = fact(2, "pane", "user:alice", "alice");
        pane.target_page = Some("spesa.md".to_owned());
        pane.style = Some("prosa".to_owned()); // ignored — first fact wins.
        pane.page_description = Some("altra desc".to_owned());
        let mut bio = fact(3, "Alice lives in Lisbon", "user:alice", "alice");
        bio.target_page = Some("index.md".to_owned()); // → orphan, not a page.
        let unproposed = fact(4, "chit chat", "user:alice", "alice"); // target None.

        let bp = ingest_placement_blueprint(&[latte.clone(), pane.clone(), bio, unproposed]);

        // Only the two `spesa` facts are assigned; index/unproposed are not.
        assert_eq!(bp.assignments.len(), 2);
        assert!(bp.assignments.iter().all(|a| a.page_slug == "spesa"));
        let assigned: BTreeSet<&str> = bp.assignments.iter().map(|a| a.fact_id.as_str()).collect();
        assert!(assigned.contains(latte.fact_id.as_str()));
        assert!(assigned.contains(pane.fact_id.as_str()));

        // Exactly one deduped NewPage, seeded from the FIRST fact's testata.
        assert_eq!(bp.new_pages.len(), 1);
        let np = &bp.new_pages[0];
        assert_eq!(np.slug, "spesa");
        assert_eq!(np.title, "Spesa");
        assert_eq!(np.page_type, PageType::ConceptLeaf);
        assert_eq!(np.parent_hub, None);
        assert_eq!(np.style.as_deref(), Some("lista"));
        assert_eq!(np.description, "cosa comprare");
    }

    #[test]
    fn ingest_placement_blueprint_routes_high_salience_off_its_target_page() {
        // A `high`-salience fact's home is the actor-wiki
        // `index.md` base context — the routing IS the reservation. Even with a
        // concrete ingest `target_page`, it must be left UNASSIGNED here (the
        // override) so the orphan-fallback homes it on the foundation page. A
        // `normal` fact with the same target_page is assigned as usual.
        let mut allergy = fact(1, "deathly peanut allergy", "user:alice", "alice");
        allergy.target_page = Some("preferenze.md".to_owned()); // concrete page…
        allergy.salience = Some("high".to_owned()); // …but high → overridden.
        let mut hobby = fact(2, "likes hiking", "user:alice", "alice");
        hobby.target_page = Some("preferenze.md".to_owned());
        hobby.salience = Some("normal".to_owned());
        let mut unspecified = fact(3, "drinks coffee", "user:alice", "alice");
        unspecified.target_page = Some("preferenze.md".to_owned());
        // salience left None → treated as normal → assigned.

        let bp = ingest_placement_blueprint(&[allergy.clone(), hobby.clone(), unspecified.clone()]);

        // The high fact is NOT assigned; the normal + unspecified ones are.
        let assigned: BTreeSet<&str> = bp.assignments.iter().map(|a| a.fact_id.as_str()).collect();
        assert!(!assigned.contains(allergy.fact_id.as_str()));
        assert!(assigned.contains(hobby.fact_id.as_str()));
        assert!(assigned.contains(unspecified.fact_id.as_str()));
        assert_eq!(bp.assignments.len(), 2);
        // The page is minted only by the non-high facts.
        assert_eq!(bp.new_pages.len(), 1);
        assert_eq!(bp.new_pages[0].slug, "preferenze");
    }

    #[test]
    fn high_salience_fact_homes_on_actor_index_via_orphan_fallback() {
        // End-to-end through the deterministic plan: a `high` fact with a concrete
        // target_page lands on the actor's foundation page (`index.md`), and NO
        // concept page named after its overridden target_page is created.
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut allergy = fact(1, "deathly peanut allergy", "user:alice", "alice");
        allergy.target_page = Some("salute.md".to_owned());
        allergy.salience = Some("high".to_owned());
        let facts = vec![allergy.clone()];

        let blueprint = ingest_placement_blueprint(&facts);
        let (plan, _reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &ConceptRegistry::empty("t"),
            "2026-06-08T00:00:00Z",
        );

        // The high fact orphan-falls-back onto alice's foundation page (index.md).
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_path, "index.md");
        assert_eq!(alice.primary_facts.len(), 1);
        assert_eq!(alice.primary_facts[0].fact_id, allergy.fact_id);
        // The overridden target_page never became a page.
        assert!(!plan.pages.contains_key("salute"));
    }

    #[test]
    fn architetto_homes_assigned_and_orphan_facts() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let facts = vec![
            fact(1, "Alice loves pasta", "user:alice", "alice"),
            fact(2, "Alice runs daily", "user:alice", "alice"),
        ];
        // Cartografo assigned fact 1 to alice; fact 2 left orphan.
        let blueprint = Blueprint {
            assignments: vec![Assignment {
                fact_id: facts[0].fact_id.as_str().to_owned(),
                page_slug: "alice".to_owned(),
            }],
            new_pages: Vec::new(),
        };
        let (plan, _reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &ConceptRegistry::empty("t"),
            "2026-05-31T00:00:00Z",
        );
        // Both facts land on alice (1 assigned, 2 orphan→subject page).
        let alice = &plan.pages["alice"];
        assert_eq!(alice.primary_facts.len(), 2);
        assert_eq!(plan.fact_count, 2);
    }

    #[test]
    fn architetto_heals_style_less_registry_entry_from_fact_majority() {
        // A registry entry persisted with `style: None` whose facts' non-empty
        // style proposals strictly agree is healed: entry AND plan page adopt
        // the majority style. A page with no styled facts stays None.
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        for (slug, title) in [("spesa", "Spesa"), ("hobby", "Hobby")] {
            registry.entries.insert(
                slug.to_owned(),
                ConceptRegistryEntry {
                    slug: slug.to_owned(),
                    title: title.to_owned(),
                    description: "d".to_owned(),
                    style: None, // persisted style-less
                    page_type: PageType::ConceptLeaf,
                    parent_hub: None,
                    wiki_id: "alice".to_owned(),
                    created_at: "t".to_owned(),
                },
            );
        }
        let mut latte = fact(1, "latte", "user:alice", "alice");
        latte.style = Some("lista".to_owned());
        let mut pane = fact(2, "pane", "user:alice", "alice");
        pane.style = Some(" Lista ".to_owned()); // normalized before the vote
        let mut nutella = fact(3, "nutella", "user:alice", "alice");
        nutella.style = Some("prosa".to_owned()); // outvoted 2:1
        let hiking = fact(4, "likes hiking", "user:alice", "alice"); // style None
        let facts = vec![latte, pane, nutella, hiking];
        let blueprint = Blueprint {
            assignments: facts
                .iter()
                .map(|f| Assignment {
                    fact_id: f.fact_id.as_str().to_owned(),
                    page_slug: if f.text == "likes hiking" {
                        "hobby".to_owned()
                    } else {
                        "spesa".to_owned()
                    },
                })
                .collect(),
            new_pages: Vec::new(),
        };
        let (plan, reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &registry,
            "2026-07-04T00:00:00Z",
        );
        assert_eq!(
            reg.entries["spesa"].style.as_deref(),
            Some("lista"),
            "majority fact style adopted into the registry entry"
        );
        assert_eq!(
            plan.pages["spesa"].style.as_deref(),
            Some("lista"),
            "this plan's page adopts the healed style too"
        );
        assert_eq!(
            reg.entries["hobby"].style, None,
            "a page with no styled facts stays None"
        );
        assert_eq!(plan.pages["hobby"].style, None);
    }

    #[test]
    fn page_type_tag_is_the_serde_wire_form() {
        // The const mapping is the one canonical PageType→wire-token table
        // (prompt renderers + the compiler's frontmatter); this lock keeps it
        // in lockstep with the enum's serde `rename_all`.
        for pt in [
            PageType::Person,
            PageType::GroupTheme,
            PageType::WikiBuffer,
            PageType::ConceptHub,
            PageType::ConceptLeaf,
        ] {
            let wire = serde_json::to_value(pt).expect("serialize");
            assert_eq!(
                wire.as_str(),
                Some(page_type_tag(pt)),
                "tag must equal serde's wire form"
            );
            let back: PageType = serde_json::from_value(wire).expect("parse back");
            assert_eq!(back, pt, "tag round-trips through serde");
        }
    }

    #[test]
    fn resolve_page_wiki_uses_facts_source_else_parent_never_root() {
        // Option C (forest model): a concept page lives in its facts' source
        // wiki; a factless hub falls back to its parent's wiki; the retired
        // `root` wiki never surfaces; a homeless factless page resolves to None.
        let foundation = BTreeMap::new();
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "famiglia".to_owned(),
            ConceptRegistryEntry {
                slug: "famiglia".to_owned(),
                title: "Famiglia".to_owned(),
                description: "d".to_owned(),
                style: None,
                page_type: PageType::GroupTheme,
                parent_hub: None,
                wiki_id: "famiglia".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        registry.entries.insert(
            "rootish".to_owned(),
            ConceptRegistryEntry {
                slug: "rootish".to_owned(),
                title: "R".to_owned(),
                description: "d".to_owned(),
                style: None,
                page_type: PageType::ConceptHub,
                parent_hub: None,
                wiki_id: "root".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        let mut src = BTreeMap::new();
        src.insert("siti_personali".to_owned(), "frodo".to_owned());

        // facts win: a global fact's page → its source wiki, no parent needed.
        assert_eq!(
            resolve_page_wiki("siti_personali", None, &foundation, &registry, &src),
            Some("frodo".to_owned())
        );
        // facts win even over a parent in a different wiki (kills the divergence).
        assert_eq!(
            resolve_page_wiki(
                "siti_personali",
                Some("famiglia"),
                &foundation,
                &registry,
                &src
            ),
            Some("frodo".to_owned())
        );
        // factless hub → parent's wiki.
        assert_eq!(
            resolve_page_wiki("eventi", Some("famiglia"), &foundation, &registry, &src),
            Some("famiglia".to_owned())
        );
        // a parent that itself resolves to the retired root → filtered to None.
        assert_eq!(
            resolve_page_wiki(
                "x",
                Some("rootish"),
                &foundation,
                &registry,
                &BTreeMap::new()
            ),
            None
        );
        // homeless: no facts, no resolvable parent → None (skipped, never root).
        assert_eq!(
            resolve_page_wiki("orfana", None, &foundation, &registry, &BTreeMap::new()),
            None
        );
    }

    #[test]
    fn architetto_fixpoint_gc_removes_empty_concept_chain() {
        let foundation = BTreeMap::new();
        // A registry with an empty hub whose only child is an empty leaf.
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "hub".to_owned(),
            ConceptRegistryEntry {
                slug: "hub".to_owned(),
                title: "Hub".to_owned(),
                description: "d".to_owned(),
                style: None,
                page_type: PageType::ConceptHub,
                parent_hub: None,
                wiki_id: "alice".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        registry.entries.insert(
            "leaf".to_owned(),
            ConceptRegistryEntry {
                slug: "leaf".to_owned(),
                title: "Leaf".to_owned(),
                description: "d".to_owned(),
                style: None,
                page_type: PageType::ConceptLeaf,
                parent_hub: Some("hub".to_owned()),
                wiki_id: "alice".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        let (plan, updated) = build_compilation_plan(
            &[],
            &foundation,
            &Blueprint::default(),
            &ConciliatorResult::default(),
            &registry,
            "t",
        );
        // Empty leaf removed → hub now childless → also removed (fixpoint).
        assert!(!plan.pages.contains_key("leaf"));
        assert!(!plan.pages.contains_key("hub"));
        assert!(!updated.entries.contains_key("hub"));
        assert_eq!(plan.merged_pages.len(), 2);
    }

    #[test]
    fn fingerprint_changes_on_topology_not_just_facts() {
        let mut p = person("alice");
        let fp1 = page_fingerprint(&p);
        p.outgoing_links.push("bob".to_owned());
        let fp2 = page_fingerprint(&p);
        assert_ne!(fp1, fp2, "a link change must change the fingerprint");
    }

    #[test]
    fn fingerprint_changes_when_a_facts_validity_closes() {
        // A validity closure mutates valid_to/decay_reason in place (same
        // fact_id, same text). The fingerprint must notice, or the closed
        // window would never reach the rendered prose.
        let mut p = person("alice");
        p.primary_facts
            .push(fact(1, "wants to watch Jumanji", "user:alice", "alice"));
        let open = page_fingerprint(&p);
        p.primary_facts[0].valid_to = Some("2026-06-11T20:00:00Z".to_owned());
        let closed = page_fingerprint(&p);
        assert_ne!(open, closed, "closing valid_to must dirty the page");
        p.primary_facts[0].decay_reason = Some("completed".to_owned());
        let reasoned = page_fingerprint(&p);
        assert_ne!(closed, reasoned, "stamping the reason must dirty the page");
    }

    #[tokio::test]
    async fn cartografo_parses_blueprint_and_dedups_new_pages() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let facts = vec![fact(1, "Alice's medical routine", "user:alice", "alice")];
        let foundation = BTreeMap::new();
        let registry = ConceptRegistry::empty("t");
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"assignments\":[{\"fact_id\":\"0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d01\",\"page_slug\":\"salute_alice\"}],\
              \"new_pages\":[{\"slug\":\"salute_alice\",\"title\":\"Salute\",\"description\":\"d\",\"page_type\":\"concept_leaf\",\"parent_hub\":\"alice\"}]}",
        );
        let bp = classify_facts(
            &llm,
            &facts,
            &foundation,
            &registry,
            tree.workdir(),
            &CartografoSignals::default(),
        )
        .await
        .expect("classify");
        assert_eq!(bp.assignments.len(), 1);
        assert_eq!(bp.assignments[0].page_slug, "salute_alice");
        assert_eq!(bp.new_pages.len(), 1);
        assert_eq!(bp.new_pages[0].page_type, PageType::ConceptLeaf);
        drop(dir);
    }

    /// An invented frame must not outlive the page written from it (card 57).
    ///
    /// The classifier proposes a page and describes it; that description used
    /// to be frozen in the registry forever, fed back to the writer on every
    /// compile, and never checked against what the page turned out to say.
    /// Now the plan adopts the written card — and does so without marking the
    /// page dirty, since the description is not in the fingerprint.
    #[tokio::test]
    async fn the_plan_adopts_the_written_card_over_the_classifiers_guess() {
        use crate::fact_index::NewFact;
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The shape of the confirmed case: the turn named the fair, the
        // classifier minted a page and called it an area of work.
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99").unwrap();
        fact_index::insert(
            &pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice did not ask to be signed up for the east fair".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: Some("episode".to_owned()),
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                target_page: Some("fiera.md".to_owned()),
                style: Some("prosa".to_owned()),
                page_description: Some("Projects and activities relating to the fair".to_owned()),
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-08-05T00:00:00Z",
        )
        .await
        .expect("plan");
        assert_eq!(
            plan.pages["fiera"].description, "Projects and activities relating to the fair",
            "the classifier's guess seeds the fresh page"
        );

        // The page is written, and the writer's card says what the facts
        // actually support.
        std::fs::write(
            wikis.join("alice/fiera.md"),
            "---\ntitle: \"Fiera\"\ncreated: 2026-08-05\nupdated: 2026-08-05\npage_type: concept_leaf\nstyle: prosa\ndescription: \"the east fair, and what Alice has said about it\"\n---\n\nqualcosa.\n",
        )
        .unwrap();

        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-08-05T01:00:00Z",
        )
        .await
        .expect("plan 2");
        assert_eq!(
            plan2.pages["fiera"].description, "the east fair, and what Alice has said about it",
            "the written card replaces the guess"
        );
        assert!(
            !plan2.dirty_pages.contains(&"fiera".to_owned()),
            "adopting a card is not a reason to recompile: {:?}",
            plan2.dirty_pages
        );
        // Persisted, so the correction survives the next build rather than
        // being re-derived from the frozen guess.
        let reg = load_concept_registry(&tree, "2026-08-05T01:00:00Z").unwrap();
        assert_eq!(
            reg.entries["fiera"].description, "the east fair, and what Alice has said about it",
            "the registry learned it too"
        );
        drop(dir);
    }

    /// The card is read from the testata only — a `description:` line in the
    /// body is prose somebody wrote, not the page's card.
    #[test]
    fn testata_description_reads_the_fence_and_not_the_body() {
        assert_eq!(
            testata_description("---\ntitle: \"X\"\ndescription: \"the card\"\n---\n\nbody\n"),
            Some("the card".to_owned())
        );
        assert_eq!(
            testata_description("---\ntitle: \"X\"\n---\n\ndescription: not the card\n"),
            None,
            "past the closing fence is body prose"
        );
        assert_eq!(testata_description("no frontmatter at all\n"), None);
        assert_eq!(
            testata_description("---\ntitle: \"X\"\ndescription: \"\"\n---\n"),
            None,
            "an empty card is not a card"
        );
    }

    #[tokio::test]
    async fn build_wiki_plan_homes_facts_and_is_incrementally_idempotent() {
        use crate::fact_index::NewFact;
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        // A standard wiki-user "alice".
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(wikis.join("alice/index.md"), "# alice\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        // Enroll alice (direct insert is fine for a test).
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A promoted fact in alice's wiki.
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d77").unwrap();
        fact_index::insert(
            &pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
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
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        // First build (no LLM → foundation + deterministic subject-page fallback).
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("plan");
        assert!(plan.pages.contains_key("alice"), "alice person page exists");
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_type, PageType::Person);
        assert_eq!(alice.page_path, "profile.md", "the card, not the map");
        assert!(
            alice.primary_facts.is_empty(),
            "a normal-salience orphan belongs on the buffer, not the identity card"
        );
        let buffer = &plan.pages["alice__notes"];
        assert_eq!(buffer.page_type, PageType::WikiBuffer);
        assert_eq!(buffer.page_path, "notes.md");
        assert_eq!(buffer.primary_facts.len(), 1, "fact homed on the buffer");
        assert_eq!(buffer.primary_facts[0].fact_id, fid);
        assert_eq!(
            plan.dirty_pages.len(),
            plan.pages.len(),
            "first run: all dirty"
        );
        // Persisted.
        assert!(load_previous_plan(&tree).unwrap().is_some());

        // Second build, nothing changed → SKIP (0 new, 0 removed): no dirty pages.
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T01:00:00Z",
        )
        .await
        .expect("plan2");
        assert!(
            plan2.dirty_pages.is_empty(),
            "unchanged corpus → 0 dirty pages"
        );
        assert_eq!(plan2.pages["alice__notes"].primary_facts.len(), 1);
        drop(dir);
    }

    /// A behaviour-rule fact lives on the reserved policy page `rules.md`
    /// (written by the rules pipeline's direct path, not the planner). The
    /// compiler must leave it there: gathering it would orphan-fall-back it
    /// onto the wiki's buffer, changing its `source_path` so `recall_behaviour_rules`
    /// (which filters on `rules.md`) stops seeing it. Regression for the
    /// durability bug found 2026-06-30.
    #[tokio::test]
    async fn build_wiki_plan_never_gathers_a_rules_md_fact() {
        use crate::fact_index::NewFact;
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mk = |id_tail: &str, source_path: &str, fact_type: &str| NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_tail}"))
                .unwrap(),
            wiki_id: "alice".to_owned(),
            source_path: source_path.to_owned(),
            region_start: None,
            region_end: None,
            text: "x".to_owned(),
            embedding: vec![0.1, 0.2],
            subject_id: "user:alice".parse::<Principal>().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some(fact_type.to_owned()),
            topics: Vec::new(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        // A normal content fact (must be homed) ...
        let content = mk("01", "wikis/alice/_captures.md", "preference");
        let content_id = content.fact_id.clone();
        fact_index::insert(&pool, &content).await.unwrap();
        // ... and a behaviour-rule fact on the reserved `rules.md` (must be spared).
        let rule = mk("02", "wikis/alice/rules.md", "rule");
        let rule_id = rule.fact_id.clone();
        fact_index::insert(&pool, &rule).await.unwrap();

        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-30T00:00:00Z",
        )
        .await
        .expect("plan");

        let placed: Vec<FactId> = plan
            .pages
            .values()
            .flat_map(|p| p.primary_facts.iter().map(|f| f.fact_id.clone()))
            .collect();
        assert!(placed.contains(&content_id), "content fact homed on a page");
        assert!(
            !placed.contains(&rule_id),
            "the rules.md behaviour-rule fact must never be gathered into the plan"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn rehome_survives_the_carry_over_and_forces_the_touched_pages_dirty() {
        // The plan-sync seam: an act-first move (REM split / page merge)
        // re-homes its facts in the persisted plan, so the next build's
        // carry-over keeps them on the destination instead of pulling them
        // back (silently undoing the move), and the touched pages recompile
        // exactly once via force_dirty even though their carried-over
        // fingerprints match.
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact(
            &pool,
            "b1",
            "Matteo does karate on Mondays",
            None,
            None,
            None,
        )
        .await;

        // First build: the fact orphan-homes on alice's foundation page.
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("plan");
        assert_eq!(plan.pages["alice__notes"].primary_facts.len(), 1);

        // An act-first move re-homes the fact onto a new `karate` page.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        let seed = RehomePageSeed::concept("karate", "alice");
        let n =
            rehome_facts_in_persisted_plan(&tree, &[(&row, &seed)], &[], "2026-06-11T01:00:00Z")
                .expect("rehome");
        assert_eq!(n, 1);

        let edited = load_previous_plan(&tree).unwrap().unwrap();
        assert!(
            edited.pages["karate"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "fact re-homed onto the destination page"
        );
        assert!(
            edited.pages["alice__notes"].primary_facts.is_empty(),
            "fact detached from the old page"
        );
        assert_eq!(
            edited.force_dirty,
            vec!["alice__notes".to_owned(), "karate".to_owned()],
            "both touched pages parked for recompile"
        );
        assert!(
            load_concept_registry(&tree, "t")
                .unwrap()
                .entries
                .contains_key("karate"),
            "destination registered so later builds re-materialise it"
        );

        // Next build (nothing else changed): the carry-over keeps the fact on
        // the destination — it does NOT fight the move — and the skip path
        // honors force_dirty as the dirty set, then clears it.
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-11T02:00:00Z",
        )
        .await
        .expect("plan2");
        assert!(
            plan2.pages["karate"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "carry-over preserved the re-home"
        );
        assert_eq!(
            plan2.dirty_pages,
            vec!["alice__notes".to_owned(), "karate".to_owned()],
            "force-dirty pages become the recompile set"
        );
        assert!(plan2.force_dirty.is_empty(), "flag cleared after honoring");

        // A third build is back to a clean skip.
        let plan3 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-11T03:00:00Z",
        )
        .await
        .expect("plan3");
        assert!(plan3.dirty_pages.is_empty(), "steady state again");
        drop(dir);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn reopened_pages_re_enter_the_to_place_pool_and_parks_drain() {
        // The bridge's plan half: a parked re-open is consumed ONLY by a
        // build that runs the Cartografo — a non-Cartografo build (light
        // Ingest, degraded-full OrphanFallback) carries it untouched, so
        // the nomination is never burned on a build that would re-settle
        // the facts on stale hints (the live 2026-07-04 reversal). The
        // parked refile candidates SURVIVE every rebuild until the refile
        // sweep drains them.
        use crate::llm::FakeLlmBackend;
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact(
            &pool,
            "b1",
            "Matteo does karate on Mondays",
            None,
            None,
            None,
        )
        .await;
        build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-07-02T00:00:00Z",
        )
        .await
        .expect("plan");
        // Move the fact onto its own page, then park that page for re-open
        // plus one refile candidate.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        let seed = RehomePageSeed::concept("karate", "alice");
        rehome_facts_in_persisted_plan(&tree, &[(&row, &seed)], &[], "2026-07-02T01:00:00Z")
            .expect("rehome");
        let parked = park_bridge_signals(&tree, &[fid.as_str().to_owned()], &["karate".to_owned()])
            .expect("park");
        assert_eq!(parked, 2);
        // Idempotent: parking the same signals again adds nothing.
        assert_eq!(
            park_bridge_signals(&tree, &[fid.as_str().to_owned()], &["karate".to_owned()])
                .expect("re-park"),
            0
        );

        // A non-Cartografo build cannot re-judge: it carries the re-open
        // park and leaves the placement alone.
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-07-02T02:00:00Z",
        )
        .await
        .expect("plan2");
        assert_eq!(
            plan2.reopen_pages,
            vec!["karate".to_owned()],
            "a non-Cartografo build carries the re-open park untouched"
        );
        assert!(
            plan2.pages["karate"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "the placement is NOT re-settled by a build that cannot re-judge"
        );

        // The Cartografo build consumes the park: the page's facts re-enter
        // the to-place pool (the fake assigns nothing, so the fact falls to
        // the Architetto's deterministic fallback → the foundation page).
        let llm = FakeLlmBackend::new("fake", "{\"assignments\":[],\"new_pages\":[]}");
        let plan3 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Cartografo(&llm),
            None,
            "2026-07-02T03:00:00Z",
        )
        .await
        .expect("plan3");
        assert!(
            plan3.reopen_pages.is_empty(),
            "the re-open park is consumed by the Cartografo build"
        );
        assert_eq!(
            plan3.refile_candidates,
            vec![fid.as_str().to_owned()],
            "the refile park is carried, not consumed"
        );
        assert!(
            plan3.pages["alice__notes"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "the re-opened page's fact re-entered the pool and re-placed (fallback → buffer)"
        );

        let taken = take_refile_candidates(&tree).expect("take");
        assert_eq!(taken, vec![fid.as_str().to_owned()]);
        assert!(
            load_previous_plan(&tree)
                .unwrap()
                .unwrap()
                .refile_candidates
                .is_empty(),
            "the drain clears the park"
        );
        drop(dir);
    }

    /// The emergence shape: a page crosses into another wiki **under its
    /// own name**, so its plan slug — the page stem — does not change and
    /// the caller's husk *is* the destination. The node must be relocated,
    /// not deleted: deleting it would strand the facts in no page at all,
    /// and on a one-page corpus the emptied plan reads back as *no plan*.
    #[tokio::test]
    async fn rehome_relocates_a_page_that_moved_wiki_under_its_own_name() {
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact(&pool, "b3", "potatura", Some("giardinaggio"), None, None).await;
        build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-08-05T00:00:00Z",
        )
        .await
        .expect("plan");

        // `giardinaggio.md` emerges as its own wiki, carried over under the
        // same file name — husk and destination are the same plan slug.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        let seed = RehomePageSeed::page_in_wiki("giardinaggio.md", "alice-giardinaggio");
        assert_eq!(seed.slug, "giardinaggio", "the slug is the page stem");
        rehome_facts_in_persisted_plan(
            &tree,
            &[(&row, &seed)],
            &["giardinaggio".to_owned()],
            "2026-08-05T01:00:00Z",
        )
        .expect("rehome");

        let edited = load_previous_plan(&tree)
            .expect("load")
            .expect("the plan survives");
        let moved = edited
            .pages
            .get("giardinaggio")
            .expect("the node was relocated, not deleted");
        assert_eq!(moved.wiki_id, "alice-giardinaggio", "it followed the seed");
        assert_eq!(moved.page_path, "giardinaggio.md");
        assert!(
            moved.primary_facts.iter().any(|f| f.fact_id == fid),
            "the fact rode along"
        );
        assert!(
            !edited.merged_pages.iter().any(|m| m.from == "giardinaggio"),
            "a page that moved was not merged away"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn rehome_removes_husk_pages_from_plan_and_registry() {
        // The page-merge shape: every fact of a page moves to a survivor and
        // the husk is dropped from the plan + registry (audited), so no later
        // build re-materialises it.
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The ingest classifier placed the fact on `spesa`.
        let fid = plant_alice_fact(&pool, "b2", "latte", Some("spesa"), Some("lista"), None).await;
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-06-11T00:00:00Z",
        )
        .await
        .expect("plan");
        assert!(plan.pages.contains_key("spesa"));

        // Merge: the fact moves to `dispensa`, `spesa` becomes the husk.
        let row = fact_index::find_by_id(&pool, &fid).await.unwrap().unwrap();
        let seed = RehomePageSeed::concept("dispensa", "alice");
        rehome_facts_in_persisted_plan(
            &tree,
            &[(&row, &seed)],
            &["spesa".to_owned()],
            "2026-06-11T01:00:00Z",
        )
        .expect("rehome");

        let edited = load_previous_plan(&tree).unwrap().unwrap();
        assert!(!edited.pages.contains_key("spesa"), "husk dropped");
        assert!(
            edited.pages["dispensa"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "fact lives on the survivor"
        );
        assert!(
            edited.merged_pages.iter().any(|m| m.from == "spesa"),
            "husk removal audited"
        );
        assert_eq!(
            edited.force_dirty,
            vec!["dispensa".to_owned()],
            "only the survivor recompiles; the husk is gone"
        );
        assert!(
            !load_concept_registry(&tree, "t")
                .unwrap()
                .entries
                .contains_key("spesa"),
            "husk dropped from the registry too"
        );
        drop(dir);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // end-to-end fixture: setup → plant → 3 builds, reads top-to-bottom
    async fn ingest_placement_settles_facts_on_their_page_without_llm() {
        // In the LIGHT cadence the planner places NEW facts on the
        // page the ingest classifier proposed — with NO LLM. A fact with a
        // concrete `target_page` lands on a concept_leaf (carrying its testata);
        // an `index.md` fact orphan-falls-back to its subject's foundation page.
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let spesa = plant_alice_fact(
            &pool,
            "a1",
            "latte",
            Some("spesa.md"),
            Some("lista"),
            Some("cosa comprare"),
        )
        .await;
        let home = plant_alice_fact(
            &pool,
            "a2",
            "Alice lives in Lisbon",
            Some("index.md"),
            None,
            None,
        )
        .await;

        // LIGHT cadence placement: no LLM passed at all.
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("plan");

        // The `spesa.md` fact made a `spesa` concept_leaf, NOT homed on alice.
        let spesa_page = plan.pages.get("spesa").expect("spesa page minted");
        assert_eq!(spesa_page.page_type, PageType::ConceptLeaf);
        assert_eq!(spesa_page.wiki_id, "alice"); // homed in the fact's wiki.
        assert_eq!(spesa_page.style.as_deref(), Some("lista"));
        assert_eq!(spesa_page.description, "cosa comprare");
        assert_eq!(spesa_page.primary_facts.len(), 1);
        assert_eq!(spesa_page.primary_facts[0].fact_id, spesa);
        // The reserved-name fact orphan-fell-back onto alice's BUFFER — the
        // card is for the identity core a `high` salience reserves, and this
        // fact is `normal`.
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_type, PageType::Person);
        assert!(alice.primary_facts.is_empty());
        let buffer = &plan.pages["alice__notes"];
        assert_eq!(buffer.page_type, PageType::WikiBuffer);
        assert_eq!(buffer.primary_facts.len(), 1);
        assert_eq!(buffer.primary_facts[0].fact_id, home);

        // Incremental idempotency: re-running with no change → 0 dirty pages.
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-05-31T01:00:00Z",
        )
        .await
        .expect("plan2");
        assert!(plan2.dirty_pages.is_empty(), "unchanged → 0 dirty");
        assert_eq!(plan2.pages["spesa"].primary_facts.len(), 1);

        // A NEW fact on the SAME ingest page accretes onto it — no duplicate page.
        plant_alice_fact(
            &pool,
            "a3",
            "pane",
            Some("spesa.md"),
            Some("lista"),
            Some("cosa comprare"),
        )
        .await;
        let plan3 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("plan3");
        assert_eq!(
            plan3.pages["spesa"].primary_facts.len(),
            2,
            "both facts on one spesa page"
        );
        assert_eq!(
            plan3.pages.values().filter(|p| p.slug == "spesa").count(),
            1,
            "exactly one spesa page (idempotent across runs)"
        );
        assert_eq!(
            plan3.dirty_pages,
            vec!["spesa".to_owned()],
            "only spesa recompiled"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn content_correction_marks_only_that_page_dirty() {
        // A dashboard comment corrects a claim in place — same
        // `fact_id`, new text. The planner must notice (the fingerprint folds
        // content) and recompile ONLY that page, never the rest of the wiki.
        use crate::fact_index::NewFact;
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
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d88").unwrap();
        fact_index::insert(
            &pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/_captures.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "Alice was born in 1985".to_owned(),
                embedding: vec![0.1, 0.2],
                subject_id: "user:alice".parse::<Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
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

        build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T00:00:00Z",
        )
        .await
        .expect("plan");
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T01:00:00Z",
        )
        .await
        .expect("plan2");
        assert!(plan2.dirty_pages.is_empty(), "unchanged → 0 dirty");

        // Correct the claim in place (the shape `apply_comments` produces).
        sqlx::query("UPDATE fact_index SET text = ? WHERE fact_id = ?")
            .bind("Alice was born in 1986")
            .bind(fid.as_str())
            .execute(&pool)
            .await
            .unwrap();

        let plan3 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T02:00:00Z",
        )
        .await
        .expect("plan3");
        assert_eq!(
            plan3.dirty_pages,
            vec!["alice__notes".to_owned()],
            "corrected claim → ONLY its page dirty (contained, no whole-wiki rescan)"
        );
        assert_eq!(
            plan3.pages["alice__notes"].primary_facts[0].text,
            "Alice was born in 1986"
        );
        drop(dir);
    }

    /// The identity-page scope tags — the mechanical half of the 32a
    /// identity-page discipline. A user subject covers exactly their own
    /// person page; a group subject expands through enrollment to its
    /// members' pages (a group the page's user belongs to is their own
    /// shared context — the tag CONTAINS their page, so the fact is not
    /// foreign there); a group the user is NOT in yields a tag WITHOUT
    /// their page (foreign); the builtin global group is `any` (world
    /// context, never a foreign subject); a memberless group is `none`.
    #[tokio::test]
    async fn subject_scopes_expand_subjects_through_enrollment() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("famiglia")
            .bind("[\"franz\",\"bruno\"]")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("condominio")
            .bind("[\"bruno\"]")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("vuoto")
            .bind("[]")
            .execute(&pool)
            .await
            .unwrap();

        let facts = vec![
            fact(1, "Bruno's therapy", "user:bruno", "famiglia"),
            fact(2, "family shopping", "group:famiglia", "famiglia"),
            fact(3, "stairwell repaint", "group:condominio", "franz"),
            fact(4, "water boils at 100C", "global", "franz"),
            fact(5, "orphaned", "group:vuoto", "franz"),
        ];
        let scopes = subject_scopes_for(&pool, &facts).await.expect("scopes");

        // Foreign user: bruno's fact covers ONLY bruno's page — franz's
        // identity index is outside the tag.
        assert_eq!(scopes["user:bruno"], "bruno");
        // Own group: franz IS a member, so his page is in the tag (his own
        // shared context, never foreign to him).
        assert_eq!(scopes["group:famiglia"], "bruno,franz");
        // Foreign group: franz is NOT a member — his page is absent.
        assert_eq!(scopes["group:condominio"], "bruno");
        // Global: never a foreign subject anywhere.
        assert_eq!(scopes["global"], "any");
        // A group with no enrolled members covers no identity page.
        assert_eq!(scopes["group:vuoto"], "none");
        drop(dir);
    }

    #[test]
    fn describe_facts_carries_the_identity_scope_tag() {
        let mut signals = CartografoSignals::default();
        signals
            .subject_scopes
            .insert("group:famiglia".to_owned(), "bruno,franz".to_owned());
        let facts = vec![
            fact(1, "Bruno's therapy", "user:bruno", "famiglia"),
            fact(2, "family shopping", "group:famiglia", "famiglia"),
        ];
        let out = describe_facts(&facts, &signals);
        // The user subject falls back to its own page even without a map entry.
        assert!(out.contains("subject=user:bruno identity_pages=bruno"));
        assert!(out.contains("subject=group:famiglia identity_pages=bruno,franz"));
    }

    #[test]
    fn page_descriptions_carry_fact_mass() {
        // The split-by-mass signal: every page line shows how many facts
        // currently live on it — a number the model weighs, never a gate.
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "dossier".to_owned(),
            ConceptRegistryEntry {
                slug: "dossier".to_owned(),
                title: "Dossier".to_owned(),
                description: "d".to_owned(),
                style: None,
                page_type: PageType::ConceptLeaf,
                parent_hub: None,
                wiki_id: "alice".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        registry.entries.insert(
            "dossier_terapie".to_owned(),
            ConceptRegistryEntry {
                slug: "dossier_terapie".to_owned(),
                title: "Terapie".to_owned(),
                description: "t".to_owned(),
                style: None,
                page_type: PageType::ConceptLeaf,
                parent_hub: Some("dossier".to_owned()),
                wiki_id: "alice".to_owned(),
                created_at: "t".to_owned(),
            },
        );
        let mut mass = BTreeMap::new();
        mass.insert("alice".to_owned(), 7usize);
        mass.insert("dossier".to_owned(), 51usize);

        let f = describe_foundation(&foundation, "alice", &mass);
        assert!(f.contains("- [person] alice — Alice (parent_hub: —) | facts: 7"));
        let c = describe_concepts(&registry, "alice", &[], &mass, &ForeignPages::Whole);
        assert!(
            c.contains("dossier — Dossier | d | facts: 51"),
            "a page line carries its fact mass"
        );
        assert!(
            c.contains("dossier_terapie — Terapie | t | facts: 0\n")
                || c.ends_with("dossier_terapie — Terapie | t | facts: 0"),
            "a childless page carries no children suffix"
        );
    }

    /// The assembled Cartografo prompt input carries both structural
    /// signals: the per-page fact-mass counts and the per-fact identity
    /// scope tags — and the running mass folds in this run's own
    /// assignments so a later batch sees the pile grow.
    #[tokio::test]
    async fn cartografo_prompt_input_carries_mass_and_identity_signals() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let mut foundation = BTreeMap::new();
        foundation.insert("franz".to_owned(), person("franz"));
        let registry = ConceptRegistry::empty("t");
        let mut signals = CartografoSignals::default();
        signals.page_mass.insert("franz".to_owned(), 12);
        signals
            .subject_scopes
            .insert("user:bruno".to_owned(), "bruno".to_owned());
        // In `franz`'s wiki: the pages a batch is shown are its own wiki's,
        // so the page under test has to live where the fact does.
        let facts = vec![fact(1, "Bruno's therapy schedule", "user:bruno", "franz")];
        let llm = FakeLlmBackend::new("fake", "{\"assignments\":[],\"new_pages\":[]}");
        classify_facts(
            &llm,
            &facts,
            &foundation,
            &registry,
            tree.workdir(),
            &signals,
        )
        .await
        .expect("classify");
        let system = llm.last_system_prompt().expect("system prompt captured");
        assert!(
            system.contains("- [person] franz — Franz (parent_hub: —) | facts: 12"),
            "page mass visible to the model"
        );
        assert!(
            system.contains("identity_pages=bruno"),
            "identity scope tag visible to the model"
        );
        drop(dir);
    }

    /// A batch is shown the **whole forest** as destinations — its own wiki's
    /// pages and everyone else's — and the collision list keeps only the names
    /// nothing offered.
    ///
    /// The fence this replaced let a fact reach only pages of the wiki it was
    /// already in, so a fact could never be re-homed: *a fact is free to live
    /// in any wiki, and the engine moving one there is its judgment, not
    /// damage* (founder, 2026-08-10). Read permission is judged per fact, so a
    /// move changes nothing about who may see it.
    ///
    /// The bare-name half is permanent, with its remaining job: a plan is
    /// keyed by slug across the whole memory, so a model that *coins* a name
    /// another wiki owns files these facts onto that page by accident.
    /// Choosing a page is a judgement; colliding with its name is not. What
    /// stays on that list is what the described lists leave out — here, the
    /// other wiki's buffer.
    #[tokio::test]
    async fn a_batch_is_offered_the_whole_forest_and_only_unshown_names_are_taken() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        foundation.insert("bob".to_owned(), person("bob"));
        let mut alice_buffer = person("alice");
        alice_buffer.slug = "alice__notes".to_owned();
        alice_buffer.page_type = PageType::WikiBuffer;
        alice_buffer.page_path = crate::wiki::NOTES_FILENAME.to_owned();
        foundation.insert("alice__notes".to_owned(), alice_buffer);
        let mut registry = ConceptRegistry::empty("t");
        for (slug, wiki) in [("cucina_alice", "alice"), ("cucina_bob", "bob")] {
            registry.entries.insert(
                slug.to_owned(),
                ConceptRegistryEntry {
                    slug: slug.to_owned(),
                    title: "Cucina".to_owned(),
                    description: "what gets cooked".to_owned(),
                    style: None,
                    page_type: PageType::ConceptLeaf,
                    parent_hub: None,
                    wiki_id: wiki.to_owned(),
                    created_at: "t".to_owned(),
                },
            );
        }
        let facts = vec![fact(1, "bob roasted a chicken", "user:bob", "bob")];
        let llm = FakeLlmBackend::new("fake", "{\"assignments\":[],\"new_pages\":[]}");
        classify_facts(
            &llm,
            &facts,
            &foundation,
            &registry,
            tree.workdir(),
            &CartografoSignals::default(),
        )
        .await
        .expect("classify");
        let system = llm.last_system_prompt().expect("system prompt captured");
        assert!(
            system.contains("- [person] bob"),
            "this wiki's own card is offered"
        );
        assert!(
            system.contains("- [person] alice — Alice | wiki: alice"),
            "another wiki's identity card IS a destination, named with its wiki: {system}"
        );
        assert!(
            system.contains("cucina_bob — Cucina"),
            "this wiki's concept page is offered, described"
        );
        assert!(
            system.contains("cucina_alice — Cucina | wiki: alice"),
            "and so is another wiki's, with the place it lives in: {system}"
        );
        // What is left of the collision list: the names of pages nothing
        // offered. Here that is the other wiki's buffer — a fact parked in
        // somebody else's inbox is not a placement.
        assert!(
            system.contains("NAMES ALREADY TAKEN"),
            "the collision guard is in the prompt"
        );
        let taken_line = system
            .lines()
            .zip(system.lines().skip(1))
            .find(|(l, _)| l.starts_with("NAMES ALREADY TAKEN by pages NOT listed above"))
            .map(|(_, next)| next.to_owned())
            .expect("the taken list follows its heading");
        assert_eq!(
            taken_line, "alice__notes",
            "only the unshown names are taken — the offered pages left the list"
        );
        drop(dir);
    }

    /// A page proposed by an EARLIER batch of another wiki is a destination,
    /// not a forbidden name.
    ///
    /// It is homed in that batch's wiki, and this batch may still assign to
    /// it: a page about to exist is a page to reuse rather than duplicate, and
    /// reusing one across wikis is a legitimate placement. What the collision
    /// list keeps is what nothing showed — here, the other wiki's buffer.
    #[test]
    fn a_proposal_from_another_wikis_batch_is_offered_not_fenced_off() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut buffer = person("alice");
        buffer.slug = "alice__notes".to_owned();
        buffer.page_type = PageType::WikiBuffer;
        foundation.insert("alice__notes".to_owned(), buffer);
        let registry = ConceptRegistry::empty("t");
        let orto = NewPage {
            slug: "orto".to_owned(),
            title: "Orto".to_owned(),
            description: "the vegetable patch".to_owned(),
            style: None,
            page_type: PageType::ConceptLeaf,
            parent_hub: None,
        };
        let shown = describe_concepts(
            &registry,
            "bob",
            &[(&orto, "alice")],
            &BTreeMap::new(),
            &ForeignPages::Whole,
        );
        assert!(
            shown.contains("orto — Orto | wiki: alice") && shown.contains("(proposed this run)"),
            "alice's fresh proposal is offered to bob's batch: {shown}"
        );
        assert_eq!(
            describe_taken_slugs(&foundation, &registry, "bob", &ForeignPages::Whole),
            "alice__notes",
            "the card is offered and the buffer is not, so only the buffer's name is taken"
        );
    }

    /// Past the ceiling the forest is cut, and the two halves stay exhaustive:
    /// what the selection carries is described **nearest first**, what it
    /// drops falls back onto the collision list rather than vanishing.
    ///
    /// A page nobody shows and nobody names is a name a later batch can coin,
    /// which is the accident `{taken_slugs}` exists to prevent — so the cut
    /// may make the offer smaller, never the guard.
    #[test]
    fn past_the_ceiling_the_offer_is_cut_by_nearness_and_the_rest_stays_a_taken_name() {
        let foundation = BTreeMap::new();
        let mut registry = ConceptRegistry::empty("t");
        for (slug, wiki) in [
            ("orto", "alice"),
            ("karate", "alice"),
            ("motori", "alice"),
            ("cucina_bob", "bob"),
        ] {
            registry
                .entries
                .insert(slug.to_owned(), concept_entry(slug, None, wiki));
        }
        // The selection picked two of alice's three, in this order.
        let foreign = ForeignPages::Selected(BTreeMap::from([(
            "bob".to_owned(),
            vec!["karate".to_owned(), "orto".to_owned()],
        )]));

        let shown = describe_concepts(&registry, "bob", &[], &BTreeMap::new(), &foreign);
        let offered: Vec<&str> = shown
            .lines()
            .filter_map(|l| l.split(" — ").next())
            .filter_map(|l| l.strip_prefix("- [concept_leaf] "))
            .collect();
        assert_eq!(
            offered,
            vec!["cucina_bob", "karate", "orto"],
            "own wiki first, then the selection in the order it was picked — not by slug: {shown}"
        );
        assert_eq!(
            describe_taken_slugs(&foundation, &registry, "bob", &foreign),
            "motori",
            "the page the cut dropped is still a name that cannot be coined"
        );
    }

    /// The Cartografo groups by wiki FIRST and chunks second, so a batch
    /// never straddles two wikis. The other order — chunk the globally
    /// sorted list, then hope — is what shipped before and is what makes
    /// a single language directive a lie for part of the batch. Sized so
    /// the two orders cannot agree: `alice` alone overflows one chunk
    /// while `bob` does not, so grouping-first yields three batches
    /// (2 + 1) and chunking-first would yield two mixed ones.
    #[test]
    fn cartografo_batches_never_mix_two_wikis() {
        let mut facts = Vec::new();
        for i in 0..u8::try_from(CARTOGRAFO_BATCH + 1).unwrap() {
            facts.push(fact(i, "alice fact", "user:alice", "alice"));
        }
        for i in 0..3u8 {
            facts.push(fact(100 + i, "bob fact", "user:bob", "bob"));
        }
        let grouped = cartografo_batches(&facts);
        assert_eq!(
            grouped.keys().collect::<Vec<_>>(),
            vec!["alice", "bob"],
            "wikis are grouped, in a deterministic order"
        );
        let batches: Vec<(&str, &[FactForPage])> = grouped
            .iter()
            .flat_map(|(w, rows)| rows.chunks(CARTOGRAFO_BATCH).map(move |c| (w.as_str(), c)))
            .collect();
        assert_eq!(
            batches.len(),
            3,
            "alice overflows into two batches, bob takes one"
        );
        for (wiki, batch) in &batches {
            assert!(
                batch.iter().all(|f| f.source_wiki_id == *wiki),
                "a batch labelled `{wiki}` carried a fact from another wiki"
            );
        }
        assert_eq!(
            batches.iter().map(|(_, b)| b.len()).sum::<usize>(),
            facts.len(),
            "regrouping must not drop or duplicate a fact"
        );
    }

    /// Every batch gets its own wiki's directive, and a wiki nobody
    /// declared a language for gets English — not the mirror clause the
    /// conversational slots fall back to.
    #[test]
    fn cartografo_signals_hand_each_wiki_its_own_language() {
        let mut signals = CartografoSignals::default();
        signals.wiki_locales.insert(
            "alice".to_owned(),
            crate::locale::render_memory_language_directive(Some("it-IT")),
        );
        assert!(signals.language_for("alice").contains("Respond in Italian"));
        let unknown = signals.language_for("bob");
        assert!(unknown.contains("Respond in English"));
        assert!(
            !unknown.contains("Mirror the language"),
            "a compiled page has no user message to mirror: {unknown}"
        );
    }

    /// The Conciliatore groups proposals by their prospective wiki, and
    /// a proposal no assignment claims falls into its own bucket rather
    /// than being silently attached to somebody's wiki.
    #[test]
    fn conciliatore_groups_proposals_by_prospective_wiki() {
        let np = |slug: &str| NewPage {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: String::new(),
            style: None,
            page_type: PageType::ConceptLeaf,
            parent_hub: None,
        };
        let mut page_wikis = BTreeMap::new();
        page_wikis.insert("salute".to_owned(), "alice".to_owned());
        page_wikis.insert("viaggi".to_owned(), "bob".to_owned());
        let groups = conciliatore_groups(&[np("salute"), np("viaggi"), np("orfana")], &page_wikis);
        assert_eq!(groups.len(), 3, "two wikis plus the homeless bucket");
        assert_eq!(groups["alice"].len(), 1);
        assert_eq!(groups["bob"].len(), 1);
        assert_eq!(
            groups[""][0].slug, "orfana",
            "an unclaimed proposal keeps its own bucket"
        );
    }

    /// The conciliatore output schema omits `NewPage::style`, so a parsed
    /// `accepted_new` item comes back with `style: None`. The backfill must
    /// re-attach the ingest-proposed style from the original proposal, or a
    /// `lista` page would be silently demoted to full-prose compilation.
    #[tokio::test]
    async fn conciliation_preserves_ingest_proposed_style() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let registry = ConceptRegistry::empty("t");
        let new_pages = vec![NewPage {
            slug: "spesa".to_owned(),
            title: "Spesa".to_owned(),
            description: "La lista della spesa".to_owned(),
            style: Some("lista".to_owned()),
            page_type: PageType::ConceptLeaf,
            parent_hub: Some("famiglia".to_owned()),
        }];
        // The LLM echoes the page back exactly as the schema asks — no `style`.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"redirects\":{},\"accepted_new\":[{\"slug\":\"spesa\",\"title\":\"Spesa\",\
             \"description\":\"La lista della spesa\",\"page_type\":\"concept_leaf\",\
             \"parent_hub\":\"famiglia\"}]}",
        );
        let result = conciliate_new_pages(
            &llm,
            &new_pages,
            &registry,
            tree.workdir(),
            &BTreeMap::new(),
            &CartografoSignals::default(),
        )
        .await;
        assert_eq!(
            result.accepted_new.len(),
            1,
            "the page survives as accepted"
        );
        assert_eq!(
            result.accepted_new[0].style.as_deref(),
            Some("lista"),
            "ingest-proposed style must survive conciliation"
        );
        drop(dir);
    }

    #[test]
    fn bundled_cartografo_prompt_carries_the_identity_and_mass_levers() {
        // The 32a identity-page discipline and the 32e split-by-mass lever
        // live in the prompt (the code supplies only the signals).
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("IDENTITY-PAGE DISCIPLINE"),
            "identity-page discipline section present"
        );
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("identity_pages="),
            "prompt explains the identity_pages tag"
        );
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("PAGE MASS"),
            "split-by-mass section present"
        );
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("normal maintenance, not an error"),
            "splitting framed as routine maintenance"
        );
        // The negative half, and it is the point: a container is a WIKI
        // (founder, 2026-08-04). The prompt kept a CONTAINER PAGES section for
        // six days after nothing could mint one — describing, in its own
        // words, "a page whose facts are being re-homed so it can settle into
        // its real hub role", the mechanism that ruling deleted. Asserting its
        // absence is what stops it coming back the next time someone reads the
        // old wording and takes it for the design.
        assert!(
            !BUNDLED_CARTOGRAFO_MD.contains("CONTAINER PAGES"),
            "the container-page rule is gone: a container is a wiki"
        );
        assert!(
            !BUNDLED_CARTOGRAFO_MD.contains("children:"),
            "no children signal: it could only ever count a parent_hub the rules forbid"
        );
    }

    /// A proposal is held to the two rules the prompt states and nothing used
    /// to enforce: it is always a leaf, and its parent is a foundation page of
    /// its own wiki or nothing at all.
    ///
    /// Both malformed shapes are the abolished container arriving by the back
    /// door — one as a page type that may no longer be minted, the other as a
    /// page parented under a page.
    #[test]
    fn a_proposal_may_not_smuggle_a_container_back_in() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        foundation.insert("bob".to_owned(), person("bob"));
        let proposal = |pt, hub: Option<&str>| NewPage {
            slug: "cucina".to_owned(),
            title: "Cucina".to_owned(),
            description: "d".to_owned(),
            style: None,
            page_type: pt,
            parent_hub: hub.map(str::to_owned),
        };

        let hub = vet_proposal(
            proposal(PageType::ConceptHub, Some("alice")),
            &foundation,
            "alice",
        );
        assert_eq!(
            hub.page_type,
            PageType::ConceptLeaf,
            "a retired page type is filed as a leaf, never accepted"
        );

        // Parented under another PAGE — the container itself.
        let under_page = vet_proposal(
            proposal(PageType::ConceptLeaf, Some("dossier")),
            &foundation,
            "alice",
        );
        assert_eq!(
            under_page.parent_hub, None,
            "a parent that is not a foundation page is dropped, and the page kept"
        );

        // Parented under ANOTHER WIKI's card: dropped too, or `resolve_page_wiki`
        // would follow the invented parent and home the page in Bob's wiki.
        let foreign = vet_proposal(
            proposal(PageType::ConceptLeaf, Some("bob")),
            &foundation,
            "alice",
        );
        assert_eq!(
            foreign.parent_hub, None,
            "a foreign foundation page is not a parent either"
        );

        let good = vet_proposal(
            proposal(PageType::ConceptLeaf, Some("alice")),
            &foundation,
            "alice",
        );
        assert_eq!(
            good.parent_hub.as_deref(),
            Some("alice"),
            "the legitimate parent survives"
        );
    }

    #[test]
    fn dirty_set_is_changed_plus_new_plus_removed() {
        let mut prev_pages = BTreeMap::new();
        prev_pages.insert("alice".to_owned(), person("alice"));
        prev_pages.insert("gone".to_owned(), person("gone"));
        let prev = CompilationPlan {
            pages: prev_pages,
            merged_pages: vec![],
            link_graph: BTreeMap::new(),
            compilation_order: vec![],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: vec![],
            force_dirty: vec![],
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        let mut next_pages = BTreeMap::new();
        let mut alice = person("alice");
        alice
            .primary_facts
            .push(fact(9, "new", "user:alice", "alice")); // changed
        next_pages.insert("alice".to_owned(), alice);
        next_pages.insert("bob".to_owned(), person("bob")); // new
        let next = CompilationPlan {
            pages: next_pages,
            ..prev.clone()
        };
        let dirty: BTreeSet<String> = compute_dirty_pages(&prev, &next).into_iter().collect();
        assert!(dirty.contains("alice"), "changed");
        assert!(dirty.contains("bob"), "new");
        assert!(dirty.contains("gone"), "removed");
    }

    #[test]
    fn page_type_change_alone_dirties_the_page() {
        // The leaf→hub normalisation changes nothing the fingerprint covers
        // (facts empty, links/children/parent equal) — the type check beside
        // it must still mark the page for recompile (hub renders through a
        // different writer).
        let mut old_leaf = person("alice");
        old_leaf.page_type = PageType::ConceptLeaf;
        old_leaf.child_leaves = vec!["child".to_owned()];
        let mut prev_pages = BTreeMap::new();
        prev_pages.insert("alice".to_owned(), old_leaf);
        let prev = CompilationPlan {
            pages: prev_pages,
            merged_pages: vec![],
            link_graph: BTreeMap::new(),
            compilation_order: vec![],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: vec![],
            force_dirty: vec![],
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        let mut flipped = prev.clone();
        flipped.pages.get_mut("alice").unwrap().page_type = PageType::ConceptHub;
        let dirty = compute_dirty_pages(&prev, &flipped);
        assert_eq!(dirty, vec!["alice".to_owned()], "type flip alone → dirty");
    }

    fn concept_entry(slug: &str, parent_hub: Option<&str>, wiki_id: &str) -> ConceptRegistryEntry {
        ConceptRegistryEntry {
            slug: slug.to_owned(),
            title: capitalize(slug),
            description: "d".to_owned(),
            style: None,
            page_type: PageType::ConceptLeaf,
            parent_hub: parent_hub.map(str::to_owned),
            wiki_id: wiki_id.to_owned(),
            created_at: "t".to_owned(),
        }
    }

    #[test]
    fn registry_entry_shadowed_by_a_foundation_slug_is_dropped() {
        // The enrolled `matteo` wiki's foundation page owns the slug; the old
        // concept-leaf entry can never materialise again (step 2 skips it) —
        // the staleness GC drops it so the conciliator stops seeing it.
        let mut foundation = BTreeMap::new();
        foundation.insert("matteo".to_owned(), person("matteo"));
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "matteo".to_owned(),
            concept_entry("matteo", Some("famiglia"), "famiglia"),
        );
        let (plan, reg) = build_compilation_plan(
            &[],
            &foundation,
            &Blueprint::default(),
            &ConciliatorResult::default(),
            &registry,
            "t2",
        );
        assert!(
            !reg.entries.contains_key("matteo"),
            "the shadowed entry is GC'd — the foundation page wins"
        );
        assert_eq!(plan.pages["matteo"].page_type, PageType::Person);
    }

    fn proposal(slug: &str, parent_hub: Option<&str>) -> NewPage {
        NewPage {
            slug: slug.to_owned(),
            title: capitalize(slug),
            description: "d".to_owned(),
            style: None,
            page_type: PageType::ConceptLeaf,
            parent_hub: parent_hub.map(str::to_owned),
        }
    }

    /// A merge whose destination does not exist is refused, not invented.
    ///
    /// The plan builder's fallback mints a blank page under any unknown slug,
    /// so an unchecked redirect turned *«merge into X»* into *«create an empty
    /// X»* — and the minted page carries no `style`, which is how a `lista`
    /// redirected onto an invented name ended up compiled as prose. The
    /// prompt's contract for this stage is *never loses a page*.
    #[test]
    fn a_redirect_to_a_destination_that_does_not_exist_is_refused() {
        let mut registry = ConceptRegistry::empty("t");
        registry
            .entries
            .insert("spesa".to_owned(), concept_entry("spesa", None, "alice"));
        let proposed = BTreeMap::from([
            ("lista_spesa".to_owned(), "spesa".to_owned()),
            ("film_visti".to_owned(), "cinema".to_owned()),
        ]);
        let kept = vet_redirects(proposed, &BTreeMap::new(), &registry, &[]);
        assert_eq!(
            kept.get("lista_spesa").map(String::as_str),
            Some("spesa"),
            "a redirect onto a page that exists survives"
        );
        assert!(
            !kept.contains_key("film_visti"),
            "an invented destination is dropped — the page stays its own"
        );
        // A page accepted this same run is a legitimate destination too.
        let kept = vet_redirects(
            BTreeMap::from([("film_visti".to_owned(), "cinema".to_owned())]),
            &BTreeMap::new(),
            &registry,
            &[proposal("cinema", None)],
        );
        assert_eq!(kept.get("film_visti").map(String::as_str), Some("cinema"));
    }

    /// Neither half of a wiki's foundation is a merge target — and the model
    /// is not offered them in the first place.
    ///
    /// A card carries a subject's identity, a buffer is where a fact waits for
    /// a home; a topic page cannot become part of either. They were rendered
    /// FIRST in the merge-target list, the buffer wearing the wiki's own title
    /// and scope as its description, under a prompt whose standing bias is
    /// *«when in doubt, prefer the redirect»*.
    #[test]
    fn a_redirect_onto_a_card_or_a_buffer_is_refused_and_never_offered() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut buffer = person("alice");
        buffer.slug = "alice__notes".to_owned();
        buffer.page_type = PageType::WikiBuffer;
        buffer.page_path = crate::wiki::NOTES_FILENAME.to_owned();
        foundation.insert("alice__notes".to_owned(), buffer);
        let mut registry = ConceptRegistry::empty("t");
        registry
            .entries
            .insert("cucina".to_owned(), concept_entry("cucina", None, "alice"));

        let kept = vet_redirects(
            BTreeMap::from([
                ("ricette".to_owned(), "alice".to_owned()),
                ("appunti".to_owned(), "alice__notes".to_owned()),
                ("piatti".to_owned(), "cucina".to_owned()),
            ]),
            &foundation,
            &registry,
            &[],
        );
        assert_eq!(
            kept.len(),
            1,
            "only the concept-page merge survives: {kept:?}"
        );
        assert_eq!(kept.get("piatti").map(String::as_str), Some("cucina"));

        let offered = describe_existing(&registry, Some("alice"), &ForeignPages::Whole);
        assert!(offered.contains("cucina"), "concept pages are offered");
        assert!(
            !offered.contains("[person]") && !offered.contains("[wiki_buffer]"),
            "no foundation page is offered as a merge target: {offered}"
        );
    }

    /// What the Conciliatore hands back is vetted like what the Cartografo
    /// proposes — it re-enters the plan AND the registry.
    #[test]
    fn an_accepted_page_is_vetted_like_a_proposed_one() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));

        assert!(
            vet_accepted(proposal("notes", None), &foundation).is_none(),
            "a reserved page name is refused outright"
        );
        assert!(
            vet_accepted(proposal("Profile", None), &foundation).is_none(),
            "the check is on the canonical slug, not the raw string"
        );

        let mut hub_page = proposal("karate", Some("nowhere"));
        hub_page.page_type = PageType::GroupTheme;
        let vetted = vet_accepted(hub_page, &foundation).expect("kept");
        assert_eq!(
            vetted.page_type,
            PageType::ConceptLeaf,
            "a container is a wiki — nothing may accept another page type"
        );
        assert_eq!(
            vetted.parent_hub, None,
            "a parent_hub naming no foundation page is dropped"
        );
        assert_eq!(
            vet_accepted(proposal("karate", Some("alice")), &foundation)
                .expect("kept")
                .parent_hub
                .as_deref(),
            Some("alice"),
            "a real foundation page survives as the hub"
        );
    }

    /// An assignment naming a reserved page never mints a second plan page on
    /// the file the wiki's own buffer or card already owns.
    ///
    /// The foundation nodes are keyed by [`plan_slug_for_page`] — the card
    /// takes the wiki's slug, the buffer takes `<wiki>__notes` — so a bare
    /// `notes` misses the lookup and reached the fallback mint, which would
    /// have produced a second page writing `notes.md` in the same wiki.
    #[test]
    fn an_assignment_naming_a_reserved_page_mints_nothing() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let facts = vec![fact(1, "alice runs on tuesdays", "user:alice", "alice")];
        let blueprint = Blueprint {
            assignments: vec![Assignment {
                fact_id: facts[0].fact_id.as_str().to_owned(),
                page_slug: "notes".to_owned(),
            }],
            new_pages: Vec::new(),
        };
        let (plan, reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &ConceptRegistry::empty("t"),
            "t2",
        );
        assert!(
            !plan.pages.contains_key("notes"),
            "no page is minted under a reserved stem: {:?}",
            plan.pages.keys().collect::<Vec<_>>()
        );
        assert!(!reg.entries.contains_key("notes"), "and none is persisted");
        assert_eq!(
            plan.pages["alice"].primary_facts.len(),
            1,
            "the fact falls back to a page that exists"
        );
    }

    #[test]
    fn dangling_parent_hub_heals_to_the_wiki_foundation_or_clears() {
        // `karate` (wiki alice) points at an absorbed hub → re-pointed to
        // alice's foundation page (and the registry entry heals with it);
        // `stray` lives in a wiki with no foundation page → cleared.
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "karate".to_owned(),
            concept_entry("karate", Some("gone_hub"), "alice"),
        );
        registry.entries.insert(
            "stray".to_owned(),
            concept_entry("stray", Some("gone_hub"), "ghost"),
        );
        let facts = vec![
            fact(1, "kick practice", "user:alice", "alice"),
            fact(2, "stray note", "user:alice", "ghost"),
        ];
        let blueprint = Blueprint {
            assignments: vec![
                Assignment {
                    fact_id: facts[0].fact_id.as_str().to_owned(),
                    page_slug: "karate".to_owned(),
                },
                Assignment {
                    fact_id: facts[1].fact_id.as_str().to_owned(),
                    page_slug: "stray".to_owned(),
                },
            ],
            new_pages: Vec::new(),
        };
        let (plan, reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &registry,
            "t2",
        );
        assert_eq!(
            plan.pages["karate"].parent_hub.as_deref(),
            Some("alice"),
            "dangling parent re-pointed to the wiki's foundation page"
        );
        assert!(
            plan.pages["alice"]
                .child_leaves
                .contains(&"karate".to_owned()),
            "the healed parent gains the child in step 7"
        );
        assert_eq!(
            reg.entries["karate"].parent_hub.as_deref(),
            Some("alice"),
            "the registry entry heals too — no resurrection next build"
        );
        assert_eq!(
            plan.pages["stray"].parent_hub, None,
            "no foundation page in that wiki → cleared"
        );
    }

    /// An emptied page is removed, and its children are re-homed onto their
    /// wiki's foundation — never promoted into a container.
    ///
    /// This test used to assert the opposite (*"the emptied container flips to
    /// hub instead of being GC'd"*), which was the planner's own second route
    /// to minting a `ConceptHub` and half the reason twelve of them existed.
    /// Founder's ruling 2026-08-04: a container is a wiki, and wikis are
    /// raised by the visible promote machinery. The re-homing is what the flip
    /// was really protecting against — an orphaned `parent_hub`.
    #[test]
    fn an_emptied_page_is_removed_and_its_children_rise_to_the_foundation() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        registry
            .entries
            .insert("cucina".to_owned(), concept_entry("cucina", None, "alice"));
        registry.entries.insert(
            "cucina_tecniche".to_owned(),
            concept_entry("cucina_tecniche", Some("cucina"), "alice"),
        );
        let facts = vec![fact(1, "impasto lievitato", "user:alice", "alice")];
        let blueprint = Blueprint {
            assignments: vec![Assignment {
                fact_id: facts[0].fact_id.as_str().to_owned(),
                page_slug: "cucina_tecniche".to_owned(),
            }],
            new_pages: Vec::new(),
        };
        let (plan, reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &registry,
            "t2",
        );
        assert!(
            !plan.pages.contains_key("cucina"),
            "an emptied page is collected, never promoted into a container"
        );
        assert!(
            !reg.entries.contains_key("cucina"),
            "and it leaves the registry, so the next build does not resurrect it"
        );
        assert_eq!(
            plan.pages["cucina_tecniche"].parent_hub.as_deref(),
            Some("alice"),
            "the child rises to its wiki's foundation instead of dangling"
        );
        assert!(
            plan.pages
                .values()
                .all(|p| !p.child_leaves.contains(&"cucina".to_owned())),
            "nothing still names the removed page as a child"
        );
    }

    #[tokio::test]
    async fn every_standard_wiki_gets_a_buffer_foundation_node() {
        // The Fonditore's third source: every standard non-identity wiki's
        // `index.md` becomes an EmergedIndex foundation node (plan-owned,
        // never GC'd); smart wikis and identity/group wikis never qualify.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("famiglia/bruno-battaglia")).unwrap();
        std::fs::create_dir_all(wikis.join("famiglia/notes-smart")).unwrap();
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("famiglia/_meta.md"),
            "---\nwiki_id: famiglia\nwiki_type: wiki-group\nslug: famiglia\ntitle: Famiglia\nacl_default: 'group:famiglia'\n---\n",
        )
        .unwrap();
        std::fs::write(
            wikis.join("famiglia/bruno-battaglia/_meta.md"),
            "---\nwiki_id: famiglia-bruno-battaglia\nwiki_type: wiki-tech\nparent_wiki_id: famiglia\nslug: bruno-battaglia\ntitle: Bruno Battaglia\nscope: 'Tutto su Bruno Battaglia'\nacl_default: 'user:franz'\n---\n",
        )
        .unwrap();
        std::fs::write(
            wikis.join("famiglia/notes-smart/_meta.md"),
            "---\nwiki_id: famiglia-notes-smart\nwiki_type: wiki-tech\nparent_wiki_id: famiglia\nslug: notes-smart\ntitle: Notes\nsmart: true\nacl_default: 'user:franz'\n---\n",
        )
        .unwrap();
        // An identity wiki with NO enrollment row: covered by neither pass.
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_groups (group_id, members, scope) \
             VALUES ('famiglia', '[]', 'La famiglia')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (foundation, _scopes) = build_foundation_pages(&pool, &tree)
            .await
            .expect("fonditore");

        let node = &foundation["famiglia_bruno_battaglia__notes"];
        assert_eq!(node.page_type, PageType::WikiBuffer);
        assert_eq!(node.page_path, "notes.md", "the buffer, never the map");
        assert_eq!(node.wiki_id, "famiglia-bruno-battaglia");
        assert_eq!(
            node.parent_hub.as_deref(),
            Some("famiglia"),
            "a topic wiki's buffer hangs under its parent wiki's hub"
        );
        assert_eq!(node.description, "Tutto su Bruno Battaglia");
        // The group's own card, and its buffer hanging under it.
        assert_eq!(foundation["famiglia"].page_path, "profile.md");
        assert_eq!(foundation["famiglia"].page_type, PageType::GroupTheme);
        assert_eq!(
            foundation["famiglia__notes"].parent_hub.as_deref(),
            Some("famiglia"),
            "a carded wiki's buffer hangs under its own card"
        );
        assert!(
            !foundation.contains_key("famiglia_notes_smart__notes"),
            "smart wikis stay out of the compiler's perimeter"
        );
        assert!(
            !foundation.contains_key("alice"),
            "an identity wiki with no enrollment row gets no card"
        );
        assert!(
            foundation.contains_key("alice__notes"),
            "…but it still gets a buffer: every standard wiki needs somewhere \
             to put a fact"
        );
        // The invariant the whole change exists for.
        assert!(
            foundation.values().all(|p| p.page_path != "index.md"),
            "no foundation node may claim the wiki's map"
        );
        drop(dir);
    }

    #[test]
    fn a_buffer_absorbs_the_legacy_leaf_slug_and_survives_gc() {
        // The 4j absorption (maintainer option A): the foundation node takes
        // the slug the legacy content leaf held, the carried facts re-attach
        // to the buffer, the shadowed registry entry drops — and an emptied
        // buffer is never GC'd (it is a foundation page).
        let mut emerged = person("famiglia_bruno_battaglia");
        emerged.page_type = PageType::WikiBuffer;
        emerged.page_path = "notes.md".to_owned();
        emerged.wiki_id = "famiglia-bruno-battaglia".to_owned();
        let mut foundation = BTreeMap::new();
        foundation.insert("famiglia_bruno_battaglia".to_owned(), emerged);
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "famiglia_bruno_battaglia".to_owned(),
            concept_entry("famiglia_bruno_battaglia", None, "famiglia-bruno-battaglia"),
        );
        let facts = vec![fact(
            1,
            "Bruno è nato nel 1950",
            "user:franz",
            "famiglia-bruno-battaglia",
        )];
        let blueprint = Blueprint {
            assignments: vec![Assignment {
                fact_id: facts[0].fact_id.as_str().to_owned(),
                page_slug: "famiglia_bruno_battaglia".to_owned(),
            }],
            new_pages: Vec::new(),
        };
        let (plan, reg) = build_compilation_plan(
            &facts,
            &foundation,
            &blueprint,
            &ConciliatorResult::default(),
            &registry,
            "t2",
        );
        let page = &plan.pages["famiglia_bruno_battaglia"];
        assert_eq!(page.page_type, PageType::WikiBuffer);
        assert_eq!(
            page.page_path, "notes.md",
            "the slug now renders the wiki's buffer, not the legacy sibling file"
        );
        assert_eq!(page.primary_facts.len(), 1, "the carried fact re-attached");
        assert!(
            !reg.entries.contains_key("famiglia_bruno_battaglia"),
            "the shadowed legacy entry is GC'd"
        );

        // Same node with NO facts: a foundation page survives the fixpoint GC.
        let (plan_empty, _) = build_compilation_plan(
            &[],
            &foundation,
            &Blueprint::default(),
            &ConciliatorResult::default(),
            &ConceptRegistry::empty("t"),
            "t3",
        );
        assert!(
            plan_empty.pages.contains_key("famiglia_bruno_battaglia"),
            "an emptied buffer is never garbage-collected"
        );
        drop(plan_empty);
    }
}
