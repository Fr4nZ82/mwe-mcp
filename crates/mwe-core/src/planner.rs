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
//!   pages **within** the relevant standard wiki (escalating to a sub-wiki only
//!   when they grow — via the existing promote machinery); emergent-page
//!   creation flows through `structure_proposals`, it is never a silent write.
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
    /// Canonical user leaf (a `wiki-user` identity wiki). Holds the user's
    /// identity/bio facts. Foundation — never garbage-collected.
    Person,
    /// Group hub (a `wiki-group` identity wiki). Holds NO own facts; links its
    /// child leaves. Foundation — never garbage-collected.
    GroupTheme,
    /// The root `index.md` of an emerged (or any standard non-identity)
    /// sub-wiki — a **topic container's** front page, carrying no identity
    /// semantics (a non-enrolled subject is a topic, not a user; maintainer
    /// 2026-07-05). Holds facts like a leaf while the topic is small and
    /// reads as the overview once children grow. Foundation — never
    /// garbage-collected, so the wiki root survives its facts moving to
    /// sub-pages.
    EmergedIndex,
    /// A thematic hub the Cartografo proposes to group ≥2 related leaves. Holds
    /// no facts. Garbage-collected when it has no children.
    ConceptHub,
    /// A thematic detail page. Holds facts; has a parent hub. Garbage-collected
    /// when it has no facts.
    ConceptLeaf,
}

impl PageType {
    /// Compilation-order rank: hubs (groups, concept hubs) before persons and
    /// emerged indexes before concept leaves, so a hub is written after its
    /// children are placed.
    const fn order_rank(self) -> u8 {
        match self {
            Self::GroupTheme => 0,
            Self::ConceptHub => 1,
            Self::Person | Self::EmergedIndex => 2,
            Self::ConceptLeaf => 3,
        }
    }

    /// Foundation pages (person, `group_theme`, emerged index) are wiki roots
    /// and are never garbage-collected.
    const fn is_foundation(self) -> bool {
        matches!(self, Self::Person | Self::GroupTheme | Self::EmergedIndex)
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
    /// Owning principal (`global` / `user:<id>` / `group:<id>`).
    pub owner: Principal,
    /// Extra read principals.
    #[serde(default)]
    pub allow: Vec<Principal>,
    /// Cross-user attribution (who said it); always set on write — equals
    /// `owner` for a self-authored fact. `None` only on legacy provenance.
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
    /// actor-wiki's `index.md` base context — see [`ingest_placement_blueprint`],
    /// which routes a `high` fact there by overriding its `target_page`. `None` =
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
            owner: row.owner_id.clone(),
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
    /// The `.md` path within `wiki_id` (foundation hubs use `index.md`).
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
                page_path: "index.md".to_owned(),
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
                page_path: "index.md".to_owned(),
                slug,
            },
        );
    }

    let emerged = seed_topic_wiki_indexes(tree, &mut pages)?;

    tracing::info!(
        groups = groups.len(),
        users = users.len(),
        emerged,
        pages = pages.len(),
        "planner: foundation pages built"
    );
    Ok((pages, group_scopes))
}

/// The Fonditore's third source — EMERGED / TOPIC-WIKI INDEX PAGES.
///
/// Every standard (non-smart) wiki that is not an identity wiki — the
/// emerged sub-wikis `file_to_subwiki` mints, and any hand-forged topic
/// wiki — gets its `index.md` as an [`PageType::EmergedIndex`] foundation
/// node, so the wiki root is plan-owned: compiled from the DB like every
/// other page, never garbage-collected, and the stable landing slug for
/// its facts. A topic container carries NO identity semantics (its subject
/// is a topic, not a user). The slug is `slugify(wiki_id)` — when a
/// registry concept page holds the same slug (the pre-4j shape: the
/// emerged content living on a sibling leaf named after the old plan
/// slug), the foundation node takes it over and the carried facts
/// re-attach to the index (the registry staleness GC drops the shadowed
/// entry). Returns how many nodes it seeded.
fn seed_topic_wiki_indexes(
    tree: &WikiTree,
    pages: &mut BTreeMap<String, PagePlan>,
) -> Result<usize> {
    let mut emerged = 0usize;
    for d in tree.walk()? {
        if d.meta.smart
            || d.meta.wiki_type == crate::wiki::IDENTITY_WIKI_TYPE
            || d.meta.wiki_type == crate::wiki::GROUP_IDENTITY_WIKI_TYPE
        {
            continue;
        }
        let slug = slugify(d.meta.wiki_id.as_str());
        if slug.is_empty() {
            continue;
        }
        if pages.contains_key(&slug) {
            tracing::warn!(
                slug,
                wiki_id = d.meta.wiki_id.as_str(),
                "planner: topic-wiki index slug collides with an enrollment foundation page, skipping"
            );
            continue;
        }
        let parent_hub = d
            .meta
            .parent_wiki_id
            .as_ref()
            .map(|p| slugify(p.as_str()))
            .filter(|p| pages.contains_key(p));
        pages.insert(
            slug.clone(),
            PagePlan {
                title: d.meta.title.clone(),
                description: d.meta.scope.clone().unwrap_or_default(),
                style: None,
                page_type: PageType::EmergedIndex,
                owner_scope: None,
                parent_hub,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: d.meta.wiki_id.as_str().to_owned(),
                page_path: "index.md".to_owned(),
                slug,
            },
        );
        emerged += 1;
    }
    Ok(emerged)
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
        let mut slug = slugify(&a.page_slug);
        if let Some(redir) = conciliation.redirects.get(&slug) {
            slug = slugify(redir);
        }
        if !pages.contains_key(&slug) {
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

    // 5. orphan fallback (deterministic): owner's person page, else the fact's
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
    let foundation_by_wiki: BTreeMap<String, String> = pages
        .iter()
        .filter(|(_, p)| p.page_type.is_foundation() && p.page_path == "index.md")
        .map(|(slug, p)| (p.wiki_id.clone(), slug.clone()))
        .collect();
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
        // Normalisation before the sweep: an emptied leaf that other pages
        // parent under functions as a hub — flip it instead of removing it
        // (removal would orphan every child's `parent_hub`: the dangling-
        // pointer factory step 6.bis exists to clean up after). The
        // semantic work — where its facts went — already happened upstream
        // (refile / placement re-open); this is topology bookkeeping only.
        for (slug, page) in &mut pages {
            if page.page_type == PageType::ConceptLeaf
                && page.primary_facts.is_empty()
                && !page.child_leaves.is_empty()
            {
                tracing::info!(
                    slug = %slug,
                    children = page.child_leaves.len(),
                    "planner: empty leaf with children normalised to concept_hub"
                );
                page.page_type = PageType::ConceptHub;
                if let Some(e) = updated_registry.entries.get_mut(slug) {
                    e.page_type = PageType::ConceptHub;
                }
            }
        }
        let to_remove: Vec<String> = pages
            .iter()
            .filter(|(_, p)| !p.page_type.is_foundation())
            .filter(|(_, p)| match p.page_type {
                PageType::ConceptLeaf => p.primary_facts.is_empty(),
                PageType::ConceptHub => p.child_leaves.is_empty(),
                _ => false,
            })
            .map(|(slug, _)| slug.clone())
            .collect();
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
    // Concept pages (hub OR leaf) are `<slug>.md` pages WITHIN their wiki — the
    // wiki's own `index.md` is the foundation (person/group_theme) hub, never a
    // concept page.
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

/// Deterministic orphan home: the owner's person/group page if it exists, else
/// the fact's source wiki's foundation page if present, else `None`.
fn orphan_target(f: &FactForPage, pages: &BTreeMap<String, PagePlan>) -> Option<String> {
    let owner_slug = match &f.owner {
        // The builtin global group has no owner page to home an orphan on.
        p if p.is_global() => String::new(),
        Principal::User(id) | Principal::Group(id) => slugify(id),
    };
    if !owner_slug.is_empty() && pages.contains_key(&owner_slug) {
        return Some(owner_slug);
    }
    let src = slugify(&f.source_wiki_id);
    if pages.contains_key(&src) {
        return Some(src);
    }
    None
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
    // page content is a function of owner/allow/sender. An ACL-only change must
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
        f.owner,
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
    // owner/allow/sender — which now steers the Cronista's tagging) flips the
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
    /// concept-leaf shape). The emergence seed sets `index.md`.
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
    /// the `fact_refile` cross-wiki plan re-home. The plan key (slug) is
    /// derived from the page the same way [`crate::promote`] flattens a
    /// page path to a plan slug (`index.md` → `slugify(wiki_id)`, else
    /// `slugify(<stem>)`), with `page_path` pinned to the given page and
    /// `wiki_id` to the destination wiki — so a fact lands on the right
    /// page of the right wiki when it crosses the boundary.
    #[must_use]
    pub fn page_in_wiki(page: &str, wiki_id: &str) -> Self {
        let stem = page.strip_suffix(".md").unwrap_or(page);
        let slug = if stem == "index" {
            slugify(wiki_id)
        } else {
            slugify(stem)
        };
        Self {
            title: capitalize(&slug.replace('_', " ")),
            slug,
            description: String::new(),
            style: None,
            wiki_id: wiki_id.to_owned(),
            page_path: Some(page.to_owned()),
        }
    }

    /// The seed for an **emerged sub-wiki's `index.md`** — the
    /// `file_to_subwiki` plan re-home: slug = the plan key of a wiki's
    /// index page (`slugify(wiki_id)`), path pinned to `index.md`.
    ///
    /// The page enters the plan as a fact-bearing `ConceptLeaf` whose
    /// path happens to be the wiki's index — enough to make the
    /// emergence plan-aware (no zombie re-render of the old page, the
    /// emerged index recompiles into real prose). A dedicated
    /// foundation type for emerged wikis is the tracked follow-up.
    #[must_use]
    pub fn wiki_index(wiki_id: &str, title: &str, description: &str) -> Self {
        Self {
            slug: slugify(wiki_id),
            title: title.to_owned(),
            description: description.to_owned(),
            style: None,
            wiki_id: wiki_id.to_owned(),
            page_path: Some("index.md".to_owned()),
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
    let mut rehomed = 0usize;
    for (row, seed) in moves {
        let dest = slugify(&seed.slug);
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
            page.primary_facts.push(FactForPage::from_row(row));
            touched.insert(dest);
            rehomed += 1;
        }
    }
    for husk in remove_pages {
        let husk = slugify(husk);
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
/// - **identity-page scope** (per fact, via its owner) — which `person`
///   pages the fact's *subject* covers, so the model can keep a foreign
///   subject off a user's identity index (an identity index carries one
///   subject; the relation surfaces through the page-user's own facts plus
///   a `[[wikilink]]`). Computed from enrollment by [`subject_scopes_for`].
/// - **page mass** (per plan page) — how many facts currently live on each
///   page, so the model can split a grown page by content before it exceeds
///   what renders reliably as one page. The numbers are the signal; where
///   the content splits is the model's judgment.
#[derive(Debug, Default)]
pub struct CartografoSignals {
    /// Owner principal (wire form, e.g. `user:bruno` / `group:famiglia` /
    /// `global`) → the rendered identity-page scope tag: a comma-joined list
    /// of `person`-page slugs, `any` (the builtin global group — world
    /// context is never a foreign subject), or `none` (a group with no
    /// enrolled members). See [`subject_scopes_for`].
    pub subject_scopes: BTreeMap<String, String>,
    /// Plan slug → number of facts currently homed on that page (the
    /// carried-over count entering this build). [`classify_facts`] adds its
    /// own in-run assignments on top so later batches see the pile grow.
    pub page_mass: BTreeMap<String, usize>,
}

impl CartografoSignals {
    /// The identity-page scope tag for one owner, from the precomputed map.
    ///
    /// Falls back to what is derivable without enrollment (a bag built
    /// empty): a user covers their own page, the global group covers `any`,
    /// a group with unknown membership covers `none`.
    #[must_use]
    fn identity_scope_tag(&self, owner: &Principal) -> String {
        if let Some(tag) = self.subject_scopes.get(&owner.to_string()) {
            return tag.clone();
        }
        match owner {
            p if p.is_global() => "any".to_owned(),
            Principal::User(id) => slugify(id),
            Principal::Group(_) => "none".to_owned(),
        }
    }
}

/// Compute the per-owner identity-page scope tags for `facts` from the
/// enrollment tables — the mechanical half of the identity-page discipline.
///
/// A fact is *foreign* to an identity index when its `owner` is a
/// **different user**, or a **group the page's user is not a member of** (a
/// group the user belongs to is their own shared context, never foreign).
/// Rendered per distinct owner as the pages the subject covers:
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
        let key = f.owner.to_string();
        if scopes.contains_key(&key) {
            continue;
        }
        let tag = match &f.owner {
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

/// Assign each fact to one page and propose emergent concept pages.
///
/// LLM, batched, one-fact-one-page. Resilient: a batch whose LLM call or JSON
/// parse fails is **skipped softly** (its facts fall to the Architetto's
/// deterministic owner-page fallback) rather than aborting the cycle.
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
    for batch in facts.chunks(CARTOGRAFO_BATCH) {
        let foundation_desc = describe_foundation(foundation, &running_mass);
        let concept_desc = describe_concepts(registry, &merged.new_pages, &running_mass);
        let facts_desc = describe_facts(batch, signals);
        let system = prompts::render(
            "cartografo",
            workdir,
            BUNDLED_CARTOGRAFO_MD,
            &[
                ("foundation_pages", foundation_desc.as_str()),
                ("concept_pages", concept_desc.as_str()),
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
            if !slug.is_empty() && known.insert(slug.clone()) {
                merged.new_pages.push(NewPage { slug, ..np });
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
    /// LIGHT cadence: settle each new fact onto the page the ingest classifier
    /// already proposed (`fact_index.target_page`), deterministically and with
    /// NO LLM call — the strong-model Cartografo is REM-only. A fact with
    /// no concrete proposed page (`index.md` / empty / `None`) orphan-falls-back.
    Ingest,
    /// FULL / REM cadence: classify new facts with the strong-model Cartografo.
    Cartografo(&'a dyn LlmBackend),
    /// No placement intelligence: every new fact orphan-falls-back to its
    /// owner / source-wiki foundation page — the historical `cartografo = None`
    /// degradation, kept for a Full pass on a deployment with no strong slot.
    OrphanFallback,
}

/// Flatten an ingest `target_page` hint (a slug or `.md` path) to a concept-leaf
/// slug, or `None` when it names no concrete page.
///
/// The ingest classifier "catalogues and lays down, it does NOT do emergence"
/// → folders / nesting are REM's job, so a path like `recipes/dinner.md`
/// flattens to a single leaf `recipes_dinner` (slugify maps the separator to
/// `_`). `index.md` / empty resolve to `None`: the fact belongs on its wiki's
/// foundation page, which the deterministic orphan-fallback already homes — never
/// a concept page named "index". The reserved channel pages resolve to `None`
/// too — `rules.md` ([`crate::wiki::RULES_FILENAME`]) and `projects.md`
/// ([`crate::wiki::PROJECTS_FILENAME`]): neither is a fact-bearing concept
/// page, and both are written by a deterministic channel, so a fact
/// mis-targeted there orphan-falls-back instead of landing among the policy
/// or the signposts. The `.md` suffix is stripped first so slugify does not
/// fold it into a trailing `_md`.
fn placement_slug(target_page: &str) -> Option<String> {
    let stripped = target_page.strip_suffix(".md").unwrap_or(target_page);
    let slug = slugify(stripped);
    if slug.is_empty() || slug == "index" || slug == "rules" || slug == "projects" {
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
        // constraints) whose home is the actor-wiki's `index.md` base context,
        // *overriding* any concrete ingest `target_page`. We achieve that by
        // leaving it UNASSIGNED here: the deterministic orphan-fallback in
        // `build_compilation_plan` then homes it on the owner's foundation page,
        // whose `page_path` is `index.md`. No new branch, no LLM — the same path a
        // fact with no proposed page already takes ("una pipeline sola").
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
        NewFactPlacement::OrphanFallback => Ok(Blueprint::default()),
    }
}

// ---------- Stadio 1.5 — Il Conciliatore (LLM) ----------

/// Fold semantically-duplicate proposed pages into existing ones.
///
/// LLM, one call. Infallible: on any failure it falls back to accepting every
/// proposed page with no merges (conservative — never loses a page).
pub async fn conciliate_new_pages(
    llm: &dyn LlmBackend,
    new_pages: &[NewPage],
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
    workdir: &Path,
) -> ConciliatorResult {
    if new_pages.is_empty() {
        return ConciliatorResult::default();
    }
    let accept_all = || ConciliatorResult {
        redirects: BTreeMap::new(),
        accepted_new: new_pages.to_vec(),
    };
    let existing = describe_existing(foundation, registry);
    let proposed = describe_new_pages(new_pages);
    let system = match prompts::render(
        "conciliatore",
        workdir,
        BUNDLED_CONCILIATORE_MD,
        &[
            ("existing_pages", existing.as_str()),
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
/// the ingest classifier's `target_page` hint, no LLM; FULL = the strong-model
/// Cartografo; or deterministic orphan-fallback). `conciliatore` is the
/// strong-model backend for the dedup stage; `None` accepts every proposed page
/// as-is. Carried-over assignments of already-known facts are preserved either
/// way — only NEW facts flow through `placement`.
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
    // saves. Only a build that actually runs the Cartografo may consume
    // the park: an Ingest (light) or OrphanFallback (degraded-full) build
    // would re-settle the re-opened facts on stale ingest `target_page`
    // hints / the owner's foundation page — burning the nomination and
    // silently reversing considered moves (observed live 2026-07-04: a
    // light build undid the refile judge's cross-wiki move within three
    // hours). Non-Cartografo builds carry the park forward untouched.
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
    if matches!(placement, NewFactPlacement::Cartografo(_)) {
        signals.subject_scopes = subject_scopes_for(pool, &facts).await?;
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

    let conciliation = if blueprint.new_pages.is_empty() {
        ConciliatorResult::default()
    } else if let Some(llm) = conciliatore {
        conciliate_new_pages(
            llm,
            &blueprint.new_pages,
            &foundation,
            &registry,
            tree.workdir(),
        )
        .await
    } else {
        ConciliatorResult {
            redirects: BTreeMap::new(),
            accepted_new: blueprint.new_pages.clone(),
        }
    };
    if !conciliation.redirects.is_empty() {
        for a in &mut blueprint.assignments {
            let s = slugify(&a.page_slug);
            if let Some(r) = conciliation.redirects.get(&s) {
                r.clone_into(&mut a.page_slug);
            }
        }
    }

    let (mut plan, updated_registry) = build_compilation_plan(
        &facts,
        &foundation,
        &blueprint,
        &conciliation,
        &registry,
        now,
    );
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
            // look "new", orphan-fall-back onto `index.md`, and their channel
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
        PageType::EmergedIndex => "emerged_index",
        PageType::ConceptHub => "concept_hub",
        PageType::ConceptLeaf => "concept_leaf",
    }
}

fn describe_foundation(
    foundation: &BTreeMap<String, PagePlan>,
    mass: &BTreeMap<String, usize>,
) -> String {
    if foundation.is_empty() {
        return "(none)".to_owned();
    }
    foundation
        .values()
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
            PageType::EmergedIndex => format!(
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
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_concepts(
    registry: &ConceptRegistry,
    this_run: &[NewPage],
    mass: &BTreeMap<String, usize>,
) -> String {
    // The container signal, sibling of the fact-mass one: how many pages
    // parent under each slug. A page with children functions as a hub — the
    // prompt tells the model not to pile facts onto it; the number only
    // makes the shape visible.
    let mut children: BTreeMap<&str, usize> = BTreeMap::new();
    for e in registry.entries.values() {
        if let Some(h) = &e.parent_hub {
            *children.entry(h.as_str()).or_default() += 1;
        }
    }
    for np in this_run {
        if let Some(h) = &np.parent_hub {
            *children.entry(h.as_str()).or_default() += 1;
        }
    }
    let children_of = |slug: &str| -> String {
        children
            .get(slug)
            .map(|n| format!(" | children: {n}"))
            .unwrap_or_default()
    };
    let mut lines: Vec<String> = registry
        .entries
        .values()
        .map(|e| {
            format!(
                "- [{}] {} — {} | {} | facts: {}{}",
                page_type_tag(e.page_type),
                e.slug,
                e.title,
                e.description,
                mass_of(mass, &e.slug),
                children_of(&e.slug),
            )
        })
        .collect();
    for np in this_run {
        lines.push(format!(
            "- [{}] {} — {} | {} | facts: {}{} (proposed this run)",
            page_type_tag(np.page_type),
            np.slug,
            np.title,
            np.description,
            mass_of(mass, &np.slug),
            children_of(&np.slug),
        ));
    }
    if lines.is_empty() {
        "(none yet)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn describe_facts(batch: &[FactForPage], signals: &CartografoSignals) -> String {
    batch
        .iter()
        .map(|f| {
            format!(
                "[id:{}] \"{}\" type={} owner={} identity_pages={}",
                f.fact_id,
                f.text.replace('\n', " "),
                f.fact_type.as_deref().unwrap_or("other"),
                f.owner,
                signals.identity_scope_tag(&f.owner),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_existing(
    foundation: &BTreeMap<String, PagePlan>,
    registry: &ConceptRegistry,
) -> String {
    let mut lines: Vec<String> = foundation
        .values()
        .map(|p| {
            format!(
                "- [{}] {} — {} | {}",
                page_type_tag(p.page_type),
                p.slug,
                p.title,
                p.description
            )
        })
        .collect();
    for e in registry.entries.values() {
        lines.push(format!(
            "- [{}] {} — {} | {}",
            page_type_tag(e.page_type),
            e.slug,
            e.title,
            e.description
        ));
    }
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

    fn fact(id_seed: u8, text: &str, owner: &str, src: &str) -> FactForPage {
        // Deterministic UUIDv7-shaped ids for tests.
        let id = format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_seed:02x}");
        FactForPage {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(&id).unwrap(),
            text: text.to_owned(),
            fact_type: Some("bio".to_owned()),
            owner: owner.parse::<Principal>().unwrap(),
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
                owner_id: "user:alice".parse::<Principal>().unwrap(),
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
        // Both facts land on alice (1 assigned, 2 orphan→owner page).
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
            PageType::EmergedIndex,
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

        // First build (no LLM → foundation + deterministic owner-page fallback).
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
        assert_eq!(alice.primary_facts.len(), 1, "fact homed on owner page");
        assert_eq!(alice.primary_facts[0].fact_id, fid);
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
        assert_eq!(plan2.pages["alice"].primary_facts.len(), 1);
        drop(dir);
    }

    /// A behaviour-rule fact lives on the reserved policy page `rules.md`
    /// (written by the rules pipeline's direct path, not the planner). The
    /// compiler must leave it there: gathering it would orphan-fall-back it
    /// onto `index.md`, changing its `source_path` so `recall_behaviour_rules`
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
            owner_id: "user:alice".parse::<Principal>().unwrap(),
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
        assert_eq!(plan.pages["alice"].primary_facts.len(), 1);

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
            edited.pages["alice"].primary_facts.is_empty(),
            "fact detached from the old page"
        );
        assert_eq!(
            edited.force_dirty,
            vec!["alice".to_owned(), "karate".to_owned()],
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
            vec!["alice".to_owned(), "karate".to_owned()],
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
            plan3.pages["alice"]
                .primary_facts
                .iter()
                .any(|f| f.fact_id == fid),
            "the re-opened page's fact re-entered the pool and re-placed (fallback → foundation)"
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
        // an `index.md` fact orphan-falls-back to its owner's foundation page.
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
        // The `index.md` fact orphan-fell-back onto alice's foundation page.
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_type, PageType::Person);
        assert_eq!(alice.primary_facts.len(), 1);
        assert_eq!(alice.primary_facts[0].fact_id, home);

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
                owner_id: "user:alice".parse::<Principal>().unwrap(),
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
            vec!["alice".to_owned()],
            "corrected claim → ONLY its page dirty (contained, no whole-wiki rescan)"
        );
        assert_eq!(
            plan3.pages["alice"].primary_facts[0].text,
            "Alice was born in 1986"
        );
        drop(dir);
    }

    /// The identity-page scope tags — the mechanical half of the 32a
    /// identity-page discipline. A user owner covers exactly their own
    /// person page; a group owner expands through enrollment to its
    /// members' pages (a group the page's user belongs to is their own
    /// shared context — the tag CONTAINS their page, so the fact is not
    /// foreign there); a group the user is NOT in yields a tag WITHOUT
    /// their page (foreign); the builtin global group is `any` (world
    /// context, never a foreign subject); a memberless group is `none`.
    #[tokio::test]
    async fn subject_scopes_expand_owners_through_enrollment() {
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
        // The user owner falls back to its own page even without a map entry.
        assert!(out.contains("owner=user:bruno identity_pages=bruno"));
        assert!(out.contains("owner=group:famiglia identity_pages=bruno,franz"));
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

        let f = describe_foundation(&foundation, &mass);
        assert!(f.contains("- [person] alice — Alice (parent_hub: —) | facts: 7"));
        let c = describe_concepts(&registry, &[], &mass);
        assert!(
            c.contains("dossier — Dossier | d | facts: 51 | children: 1"),
            "a parented-under page carries the children signal"
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
        let facts = vec![fact(
            1,
            "Bruno's therapy schedule",
            "user:bruno",
            "famiglia",
        )];
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

    /// The conciliatore output schema omits `NewPage::style`, so a parsed
    /// `accepted_new` item comes back with `style: None`. The backfill must
    /// re-attach the ingest-proposed style from the original proposal, or a
    /// `lista` page would be silently demoted to full-prose compilation.
    #[tokio::test]
    async fn conciliation_preserves_ingest_proposed_style() {
        use crate::llm::FakeLlmBackend;
        let dir = tempfile::tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        let foundation = BTreeMap::new();
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
        let result =
            conciliate_new_pages(&llm, &new_pages, &foundation, &registry, tree.workdir()).await;
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
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("CONTAINER PAGES"),
            "container-page rule present"
        );
        assert!(
            BUNDLED_CARTOGRAFO_MD.contains("children: N"),
            "prompt explains the children tag"
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

    #[test]
    fn empty_leaf_with_children_normalises_to_hub() {
        // A leaf whose facts drained but that other pages parent under is
        // flipped to concept_hub instead of GC-removed (removal would orphan
        // every child's parent_hub).
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
        assert_eq!(
            plan.pages["cucina"].page_type,
            PageType::ConceptHub,
            "the emptied container flips to hub instead of being GC'd"
        );
        assert_eq!(
            reg.entries["cucina"].page_type,
            PageType::ConceptHub,
            "the registry entry flips with it"
        );
        assert!(
            plan.pages["cucina"]
                .child_leaves
                .contains(&"cucina_tecniche".to_owned()),
            "the child stays parented under the flipped hub"
        );
    }

    #[tokio::test]
    async fn standard_topic_wikis_get_an_emerged_index_foundation_node() {
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

        let node = &foundation["famiglia_bruno_battaglia"];
        assert_eq!(node.page_type, PageType::EmergedIndex);
        assert_eq!(node.page_path, "index.md");
        assert_eq!(node.wiki_id, "famiglia-bruno-battaglia");
        assert_eq!(
            node.parent_hub.as_deref(),
            Some("famiglia"),
            "the topic wiki hangs under its parent wiki's hub"
        );
        assert_eq!(node.description, "Tutto su Bruno Battaglia");
        assert!(
            !foundation.contains_key("famiglia_notes_smart"),
            "smart wikis stay out of the compiler's perimeter"
        );
        assert!(
            !foundation.contains_key("alice"),
            "identity wikis are enrollment's, never the topic pass's"
        );
        drop(dir);
    }

    #[test]
    fn emerged_index_absorbs_the_legacy_leaf_slug_and_survives_gc() {
        // The 4j absorption (maintainer option A): the foundation node takes
        // the slug the legacy content leaf held, the carried facts re-attach
        // to the wiki's `index.md`, the shadowed registry entry drops — and
        // an emptied emerged index is never GC'd (it is a foundation page).
        let mut emerged = person("famiglia_bruno_battaglia");
        emerged.page_type = PageType::EmergedIndex;
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
        assert_eq!(page.page_type, PageType::EmergedIndex);
        assert_eq!(
            page.page_path, "index.md",
            "the slug now renders the wiki's index, not the legacy sibling file"
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
            "an emptied emerged index is never garbage-collected"
        );
        drop(plan_empty);
    }
}
