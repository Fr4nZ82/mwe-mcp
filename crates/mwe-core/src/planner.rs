// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compilation **planner** — the topology stage of the narrative
//! compiler, ported from the old engine's "Forgia della Wiki" onto mwe-mcp.
//!
//! The planner turns the flat fact store ([`crate::fact_index`], fed by the
//! light dream) into a [`CompilationPlan`]: a page graph in which every
//! fact lives on **exactly one** page (the one-fact-one-page invariant), and
//! a persistent [`ConceptRegistry`] stops the same concept page being
//! re-invented run-to-run. The plan is the input the
//! Cronista compiles into prose; this module never writes prose itself.
//!
//! Five stages (run by [`build_wiki_plan`]):
//!
//! 1. **Fonditore** ([`build_foundation_pages`]) — deterministic, no LLM. From
//!    [`crate::enrollment`] users + groups: an identity card per user and a
//!    buffer per wiki. A group wiki gets no card — what it is lives in
//!    its `_meta.md` (founder, 2026-08-19).
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
//!    pages, applies assignments (+ redirects), homes the facts nobody placed,
//!    collects the concept pages left with no facts, builds the symmetric link
//!    graph, and orders the wiki's own pages first.
//! 5. **Incremental** ([`build_wiki_plan`]) — carries over prior assignments,
//!    classifies only NEW facts, skips entirely on 0-new-0-removed, and computes
//!    the dirty set via [`page_fingerprint`] so only changed pages recompile.
//!
//! ## mwe-mcp adaptations (vs the flat old engine)
//!
//! - Foundation pages belong to the typed identity wikis (`wiki-user`,
//!   `wiki-group`); a page's tree home is carried on
//!   [`PagePlan::wiki_id`] + [`PagePlan::page_path`]. Concept pages are `.md`
//!   pages **within** the relevant standard wiki. A page that grows is **split
//!   into more pages** (the Cartografo's split-by-mass lever); a **sub-wiki**
//!   emerges from a different signal — a *group* of existing pages that are one
//!   subject area, via the REM promote machinery — so a page never becomes a
//!   wiki and a wiki is never born holding one page. **Emergent-page
//!   creation leaves a receipt** in `structure_proposals`
//!   ([`crate::proposals::kind::PAGE_CREATE`], born-applied —
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
/// batches all see the same list — so it rides the prompt's cached prefix and
/// completeness is also the cheap answer. Above it a whole-forest list stops
/// fitting a call at all, and what a batch is shown becomes a selection
/// ([`crate::candidates`]). Twin of
/// [`crate::compiler::CARD_INDEX_CACHE_CEILING_PAGES`], which answers the same
/// question for the writing stage.
const FOREST_PAGE_CEILING: usize = 400;

/// How many foreign concept pages a selection carries past the ceiling.
///
/// The batch's **own** wiki is never cut — that is where most of its facts
/// belong and where every page it coins is born. What is cut is the rest of
/// the forest, and [`crate::candidates`] decides what survives the cut: never
/// alphabetically, and never by nearness alone, because a page that resembles
/// nothing this wiki holds is exactly the one a search would never find.
const FOREIGN_SELECTION_PAGES: usize = crate::candidates::SELECTION_PAGES;

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

/// Plan key of a wiki-relative page path — the single mapping every caller
/// that re-homes a fact in the persisted plan must agree on.
///
/// A wiki's **identity card** ([`crate::wiki::PROFILE_FILENAME`]) is a
/// foundation node keyed per wiki: it takes `slugify(wiki_id)`. Everything
/// else is a concept page keyed by its own flattened stem.
#[must_use]
pub fn plan_slug_for_page(wiki_id: &str, page: &str) -> String {
    let stem = page.strip_suffix(".md").unwrap_or(page);
    // Both spellings, and that is deliberate: the `@` marker landed on
    // 2026-08-18, and a receipt or a persisted plan written before it still
    // names the bare form. `slugify` would eat the marker anyway — it keeps
    // only letters and digits — so an unmatched `@profile` would silently
    // become the concept slug `profile` instead of the card's key.
    match stem {
        "@profile" | "profile" => slugify(wiki_id),
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
    /// = unproposed (older rows / the direct path) → identity fallback.
    #[serde(default)]
    pub target_page: Option<String>,
    /// Ingest-proposed writing style (closed palette `prosa` | `prosa-tecnica` |
    /// `lista`) seeding a freshly-placed page's testata
    /// (`fact_index.style`). `None` = unproposed.
    #[serde(default, deserialize_with = "de_style_lenient")]
    pub style: Option<crate::wiki::PageStyle>,
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
            style: row.style,
            salience: row.salience.clone(),
            authored_refs: row.authored_refs.clone(),
        }
    }
}

/// Read a persisted `style` without letting an old value break the whole
/// artefact.
///
/// The plan and the registry are ours, so they hold one of the three — but a
/// file written before 2026-08-19, when the column was free text, may hold
/// anything the classifier said. Refusing to parse would throw away a whole
/// plan over one page's label, so an unrecognised value reads as *no style*
/// and the majority heal supplies one at the next build.
fn de_style_lenient<'de, D>(d: D) -> std::result::Result<Option<crate::wiki::PageStyle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = serde::Deserialize::deserialize(d)?;
    Ok(crate::wiki::PageStyle::parse_lenient(raw.as_deref()))
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
    #[serde(default, deserialize_with = "de_style_lenient")]
    pub style: Option<crate::wiki::PageStyle>,
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
    /// reserved page (`@profile.md` / `appunti.md`) — and it is that name, not
    /// a separate field, that says which kind of page this is.
    pub page_path: String,
}

impl PagePlan {
    /// The wiki's **identity card** — who this person or group is. The one
    /// page the wiki owns rather than grows, and the one page the empty-page
    /// GC never takes: a person exists before anybody says anything about
    /// them.
    #[must_use]
    pub fn is_identity_card(&self) -> bool {
        self.page_path == crate::wiki::PROFILE_FILENAME
    }

    /// What this page is, in the words the placement prompt uses.
    ///
    /// A **rendering**, not a stored classification: derived from the page's
    /// own name every time it is shown. The model needs words to talk about
    /// pages; the engine does not need to keep a copy of them.
    #[must_use]
    pub fn prompt_kind(&self) -> &'static str {
        if self.is_identity_card() {
            "person"
        } else {
            "concept_leaf"
        }
    }

    /// Compilation order: a wiki's own page (its card) first,
    /// the pages the topology grows after them.
    fn order_rank(&self) -> u8 {
        u8::from(!self.is_identity_card())
    }
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
    /// Slugs in compile order (a wiki's own pages first).
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

/// One persisted concept page — a page the Cartografo coined, never a
/// wiki's own card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptRegistryEntry {
    /// Slug.
    pub slug: String,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Ingest-proposed writing style. See [`PagePlan::style`].
    #[serde(default, deserialize_with = "de_style_lenient")]
    pub style: Option<crate::wiki::PageStyle>,
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

/// Canonicalize an LLM-proposed page name through [`slugify`]: trim, strip a
/// trailing `.md`, slugify, re-append `.md`. Returns `None` when nothing is
/// left to slugify.
///
/// This is the single chokepoint for every page name an LLM invents — the
/// ingest classifier's `target_page` and the REM auto-promote's recommended
/// target both pass through here, so the same concept can never materialise
/// twice under spelling variants (`lista-spesa` vs `Lista spesa`).
///
/// **A coined name is ONE segment.** A `/` in it is flattened, not honoured
/// (founder, 2026-08-19: *«un utente non può poter creare cartelle, non ne
/// vedo il motivo»*). Honouring it would make the folder: the write creates
/// missing parents, so `spesa/detersivi.md` would raise a container nobody
/// asked for as a side effect of a page name — and *«un contenitore è una
/// wiki»*. Where a page belongs is the WIKI's job: *«se un utente dice creami
/// una lista della spesa, il classificatore la metterà sul gruppo famiglia
/// perché c'è scritto nello scope del gruppo; se chiede una lista dei libri
/// che ha letto, verrà creata nella sua wiki personale»*.
///
/// Folders inside a wiki are still read (an import may have them, and a smart
/// wiki's tree is its consumer's business) — [`crate::wiki::is_safe_page_path`]
/// is unchanged. They are just never *coined* here.
#[must_use]
pub fn canonical_page_path(raw: &str) -> Option<String> {
    let stem = raw.trim();
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    // A traversal is refused, not flattened. `spesa/detersivi` is a two-word
    // name written with a slash and becomes `spesa_detersivi`; `../escape` is
    // a malformed proposal, and slugifying it into the plausible page `escape`
    // would invent a page out of an attack.
    if stem
        .split(['/', '\\'])
        .any(|seg| matches!(seg.trim(), "." | ".."))
    {
        return None;
    }
    let slug = slugify(stem);
    if slug.is_empty() {
        return None;
    }
    Some(format!("{slug}.md"))
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
/// An identity card per enrolled user. A group wiki gets no card (founder,
/// 2026-08-19): what the group is lives in its `_meta.md`. Returns the pages
/// keyed by slug.
///
/// # Errors
///
/// DB errors.
pub async fn build_foundation_pages(pool: &SqlitePool) -> Result<BTreeMap<String, PagePlan>> {
    let mut pages: BTreeMap<String, PagePlan> = BTreeMap::new();

    // **A group wiki has no card, and no foundation page at all** (founder,
    // 2026-08-19). What the group needs is already in its `_meta.md`: its
    // title, its `scope` prose (inherited from the group's enrollment scope,
    // and read by the ingest classifier as a placement signal) and its
    // one-line abstract. Recall serves a PERSON's card and never a group's,
    // so a page holding the same thing would be written by a model on every
    // compile and read by nobody. The groups are listed here for one reason:
    // their slugs are taken forest-wide, so a person's card must not claim
    // one.
    let groups = enrollment::list_groups(pool).await?;
    let group_slugs: BTreeSet<String> = groups
        .iter()
        .map(|g| slugify(&g.group_id))
        .filter(|s| !s.is_empty())
        .collect();

    // PERSON PAGES.
    let users = enrollment::list_users(pool).await?;
    for u in &users {
        let slug = slugify(&u.user_id);
        if slug.is_empty() {
            continue;
        }
        // Skip a person whose slug collides with a group's: the slug is taken
        // forest-wide, whichever of the two holds it.
        if group_slugs.contains(&slug) {
            tracing::warn!(
                slug,
                "planner: person slug collides with a group, skipping person"
            );
            continue;
        }
        // A person's card links to no group. What ties a person to a group is
        // the ACL on a fact, never a link on a page.
        pages.insert(
            slug.clone(),
            PagePlan {
                title: capitalize(&u.user_id),
                description: format!("Personal page of {}", capitalize(&u.user_id)),
                style: None,
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: u.user_id.clone(),
                page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
                slug,
            },
        );
    }

    tracing::info!(
        groups = groups.len(),
        users = users.len(),
        pages = pages.len(),
        "planner: foundation pages built"
    );
    Ok(pages)
}

// ---------- Stadio 2 — L'Architetto ----------

/// Build the [`CompilationPlan`] (deterministic).
///
/// Materialises foundation + registry + accepted-new pages, applies assignments
/// (with redirects) under the one-fact-one-page rule, deterministically homes
/// orphan facts, collects the concept pages left with no facts, builds the
/// symmetric link graph, and puts a wiki's own pages first. Returns the plan
/// and the updated registry.
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
        match resolve_page_wiki(slug, &slug_source_wiki) {
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
        // Option C: home the page in its facts' source wiki (else its
        // parent's wiki); skip a homeless page rather than minting a root.
        let Some(wiki_id) = resolve_page_wiki(&slug, &slug_source_wiki) else {
            continue;
        };
        let entry = ConceptRegistryEntry {
            slug: slug.clone(),
            title: np.title.clone(),
            description: np.description.clone(),
            style: crate::wiki::PageStyle::parse_lenient(np.style.as_deref()),
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
            // Never mint a reserved stem. A foundation node is keyed by
            // [`plan_slug_for_page`], which gives the identity card the
            // wiki's own slug — so a bare `profile` misses the lookup above
            // and would mint a SECOND plan page on the file the card already
            // owns. Drop the assignment instead; the orphan pass below homes
            // the fact on a page that exists.
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
                    wiki_id,
                    created_at: now.to_owned(),
                });
        }
        if let Some(page) = pages.get_mut(&slug) {
            page.primary_facts.push((*fact).clone());
            assigned.insert(fact.fact_id.as_str().to_owned());
        }
    }

    // 5. The identity fallback (deterministic), and it is the only one left.
    //
    // A `salience: "high"` fact is always-on material the classifier
    // *reserved* — identity, health/safety, a hard standing constraint — so it
    // has a home whatever anybody decided: the subject's identity card.
    //
    // **Everything else that reaches here is simply not placed**, and that is
    // a state, not a problem (founder, 2026-08-22): the claim waits in the
    // buffer, where the next pass sees it again — and the last pass of the
    // night has to give it a page. A claim left here is NOT dropped:
    // `dream_light::materialise` promotes only what the plan placed, so an
    // unplaced claim keeps its buffer row.
    for f in facts {
        if assigned.contains(f.fact_id.as_str()) {
            continue;
        }
        if f.salience.as_deref() != Some("high") {
            continue;
        }
        if let Some(slug) = identity_card_target(f, &pages)
            && let Some(page) = pages.get_mut(&slug)
        {
            page.primary_facts.push(f.clone());
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
                style = style.as_str(),
                "planner: healed style-less registry entry from its facts' majority style"
            );
            entry.style = Some(style);
            page.style = Some(style);
        }
    }

    // 8. Collect every concept page left with no facts.
    //
    // One pass is the whole job: a page's fact set is fixed by the assignment
    // steps above, so removing one page cannot empty another. **An emptied
    // page is removed, never turned into something else** — a container is a
    // wiki (founder, 2026-08-04: *«un contenitore è una wiki»*), and wikis are
    // raised by the promote machinery, where a person can see it happen.
    let to_remove: Vec<String> = pages
        .iter()
        .filter(|(_, p)| !p.is_identity_card())
        // An ordinary page with no facts left is nothing; the card is
        // exempted by the filter above, because a wiki keeps its card even
        // on the day it holds nothing.
        .filter(|(_, p)| p.primary_facts.is_empty())
        .map(|(slug, _)| slug.clone())
        .collect();
    for slug in to_remove {
        merged.push(MergedPage {
            from: slug.clone(),
            into: "—".to_owned(),
            reason: "page with 0 facts".to_owned(),
        });
        pages.remove(&slug);
        updated_registry.entries.remove(&slug);
    }

    // 9. directed link graph, then symmetric.
    //
    // One source: [`PagePlan::outgoing_links`], which is meant to carry the
    // `[[wikilinks]]` the page's own text names. A page is reached because one
    // of its facts ranked, its description matched, or another page's prose
    // links it — there is no fourth way in, and nothing points down at it from
    // above.
    //
    // ⚠️ **Nothing fills that field today**, so this graph is empty on every
    // build, and everything downstream of it — the Cronista's recommended
    // rails, `compiler::missing_rails`, the `outgoing` half of
    // [`page_fingerprint`], the reviewer's asymmetric-link check — is running
    // on an empty set rather than on no links.
    let mut link_graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (slug, page) in &pages {
        let entry = link_graph.entry(slug.clone()).or_default();
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

    // 10. compilation order: a wiki's own pages first, then slug for stability.
    let mut order: Vec<String> = pages.keys().cloned().collect();
    order.sort_by(|a, b| {
        let ra = pages[a].order_rank();
        let rb = pages[b].order_rank();
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
    // A concept page is a `<slug>.md` page WITHIN its wiki — a wiki's
    // reserved pages are foundation nodes or nobody's, never concept pages,
    // and `placement_slug` refuses their names so one can never be minted
    // here.
    let page_path = format!("{}.md", e.slug);
    PagePlan {
        slug: e.slug.clone(),
        title: e.title.clone(),
        description: e.description.clone(),
        style: e.style,
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
        style: crate::wiki::PageStyle::parse_lenient(np.style.as_deref()),
        primary_facts: Vec::new(),
        outgoing_links: Vec::new(),
        incoming_links: Vec::new(),
        wiki_id: wiki_id.to_owned(),
        page_path,
    }
}

/// The strict-majority writing style among a page's facts' non-empty
/// `fact_index.style` proposals, each normalized to the compiler's closed
/// palette by type ([`crate::wiki::PageStyle`]). `None`
/// when no fact carries a style or no single style wins more than half of
/// the non-empty votes.
fn majority_fact_style(facts: &[FactForPage]) -> Option<crate::wiki::PageStyle> {
    let mut votes: BTreeMap<&'static str, (crate::wiki::PageStyle, usize)> = BTreeMap::new();
    let mut total = 0_usize;
    for f in facts {
        let Some(style) = f.style else { continue };
        votes.entry(style.as_str()).or_insert((style, 0)).1 += 1;
        total += 1;
    }
    votes
        .into_values()
        .find(|&(_, n)| n * 2 > total)
        .map(|(style, _)| style)
}

/// Resolve the wiki a concept page lives in (Option C — forest model).
///
/// A page is homed in **its facts' source wiki** — the invariant that a fact's
/// region lives in the fact's own wiki, which also keeps `fact_index.wiki_id`
/// and the compiled `source_path` in the same wiki. Returns `None` for a page
/// with no facts to home it: a factless page has nothing to be about, and the
/// caller skips it — there is nothing else to ask, because a page hangs under
/// nothing. Never resolves to a `root` wiki: the tree is a forest of
/// top-level wikis with no single materialised root.
fn resolve_page_wiki(slug: &str, slug_source_wiki: &BTreeMap<String, String>) -> Option<String> {
    slug_source_wiki
        .get(slug)
        .cloned()
        .filter(|w| w != crate::types::WikiId::ROOT)
}

/// The identity card a `salience: "high"` fact belongs on — the subject's
/// wiki first, then the fact's source wiki.
///
/// `None` when neither has a card, which is the ordinary case for a topic
/// wiki and for the builtin global group: a topic has no identity to reserve,
/// so the fact stays unplaced and keeps waiting like any other.
///
/// This is the last deterministic placement left. Everything that is not
/// always-on material has no fallback at all since 2026-08-22 — there is no
/// page that means "unsorted", so an unplaced claim simply waits.
fn identity_card_target(f: &FactForPage, pages: &BTreeMap<String, PagePlan>) -> Option<String> {
    let subject_slug = match &f.subject {
        // The builtin global group has no subject page of its own.
        p if p.is_global() => String::new(),
        Principal::User(id) | Principal::Group(id) => slugify(id),
    };
    let src_slug = slugify(&f.source_wiki_id);
    for wiki_slug in [subject_slug, src_slug] {
        if wiki_slug.is_empty() {
            continue;
        }
        if let Some(slug) = identity_card_slug_for(&wiki_slug, pages) {
            return Some(slug);
        }
    }
    None
}

/// The identity card's slug within the wiki keyed `wiki_slug`, or `None` when
/// that wiki has no card.
///
/// **No fallback, deliberately.** A topic wiki has no identity to reserve, so
/// a `high` fact whose wikis have no card stays in the buffer like any other
/// unplaced claim. Anywhere else would say "unsorted", and "always-on
/// material with nowhere to be always-on" is a different thing.
fn identity_card_slug_for(wiki_slug: &str, pages: &BTreeMap<String, PagePlan>) -> Option<String> {
    pages
        .get(wiki_slug)
        .filter(|p| p.is_identity_card())
        .map(|_| wiki_slug.to_owned())
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
    // subject/allow/sender — which steers the Cronista's tagging) flips the
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
    format!("{}|{}", facts.join(","), out.join(","))
}

/// The recompile set: pages new, fingerprint-changed, or type-changed in
/// `next`, plus pages removed since `prev`.
///
/// Removed pages ride along so the compiler can delete their `.md`. The
/// page-path check rides beside the fingerprint (not inside it — that
/// would flip every stored fingerprint at once and recompile the world):
/// a page that moved renders elsewhere, so it must go dirty even when
/// facts/links/children are unchanged.
#[must_use]
pub fn compute_dirty_pages(prev: &CompilationPlan, next: &CompilationPlan) -> Vec<String> {
    let prev_fp: BTreeMap<&String, (String, String)> = prev
        .pages
        .iter()
        .map(|(s, p)| (s, (page_fingerprint(p), p.page_path.clone())))
        .collect();
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    for (slug, p) in &next.pages {
        let fp = page_fingerprint(p);
        match prev_fp.get(slug) {
            Some((prev, path)) if *prev == fp && *path == p.page_path => {},
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
    pub style: Option<crate::wiki::PageStyle>,
    /// The standard wiki the page lives in.
    pub wiki_id: String,
    /// Wiki-relative page path override; `None` = `<slug>.md` (the
    /// concept-leaf shape). A reserved page sets it explicitly.
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
    /// for a wiki's identity card, which is a foundation node keyed per
    /// wiki**: `@profile.md` takes `slugify(wiki_id)`, so every wiki's card
    /// gets its own key instead of them all colliding on `profile`.
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
                    style: seed.style,
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
                    style: seed.style,
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
    /// Which pass is asking, and therefore what it is allowed to leave
    /// undone. Rendered into the prompt's `{cadence}` slot — the one place
    /// where the passes that share this prompt part company.
    pub cadence: CartografoCadence,
}

/// Which of the three passes is calling the Cartografo.
///
/// One prompt, three jobs, and they differ on exactly two questions: how many
/// facts must group before a page may be born, and whether "I could not place
/// this one" is an acceptable answer. Rendering the difference instead of
/// writing it into the body is what keeps one pass from reading another's
/// rule.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CartografoCadence {
    /// The hourly pass, on the cheap tier. Groups or waits; a page needs
    /// [`PAGE_BIRTH_FLOOR`] facts to be born.
    #[default]
    Hourly,
    /// The nightly pass, on the strong tier, reading a whole wiki at once. No
    /// floor — its judgement is the point — and it may still leave a claim
    /// waiting, because [`Self::Closing`] runs after it.
    Nightly,
    /// The last pass of the night, over what every other pass declined.
    ///
    /// **Nothing runs after it**, so "leave it waiting" stops being an answer:
    /// a claim it does not place waits another whole day. It is the only pass
    /// that may open a page for a single fact, and it is expected to.
    Closing,
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
    /// Past [`FOREST_PAGE_CEILING`]: wiki id → the foreign concept pages that
    /// wiki's batches are offered, each carrying the source that chose it.
    ///
    /// A wiki absent from the map, or present with an empty list, is offered
    /// no foreign concept page at all — its own pages and the forest's
    /// identity cards remain, and the names of the rest still ride the
    /// collision list. Smaller, never wrong.
    Selected(BTreeMap<String, Vec<crate::candidates::Candidate>>),
}

impl ForeignPages {
    /// Whether a foreign page is offered to `wiki`'s batches as a destination.
    fn offers(&self, wiki: &str, slug: &str) -> bool {
        match self {
            Self::Whole => true,
            Self::Selected(_) => self.picked(wiki, slug).is_some(),
        }
    }

    /// The rank of a foreign page for `wiki` — its position in the selection,
    /// so the rendering keeps the order the selection chose instead of
    /// re-sorting by slug, which would hand the model an alphabetical list
    /// again.
    fn rank(&self, wiki: &str, slug: &str) -> usize {
        match self {
            Self::Whole => 0,
            Self::Selected(by_wiki) => by_wiki
                .get(wiki)
                .and_then(|picked| picked.iter().position(|c| c.key == slug))
                .unwrap_or(usize::MAX),
        }
    }

    /// Why `slug` is offered to `wiki` — the tag its line carries, so the
    /// model can tell an offer made *because* the page does not resemble it
    /// from one made because it does.
    fn source(&self, wiki: &str, slug: &str) -> Option<crate::candidates::CandidateSource> {
        self.picked(wiki, slug).map(|c| c.source)
    }

    fn picked(&self, wiki: &str, slug: &str) -> Option<&crate::candidates::Candidate> {
        match self {
            Self::Whole => None,
            Self::Selected(by_wiki) => by_wiki.get(wiki)?.iter().find(|c| c.key == slug),
        }
    }
}

/// Decide what each wiki's batches are shown of the rest of the forest.
///
/// Whole below [`FOREST_PAGE_CEILING`]; above it, one selection per wiki that
/// has facts in this run, composed by [`crate::candidates`].
///
/// The asking side is **the wiki's own pages, all of them** — its concept
/// pages and its identity card — so what the selection knows about the asker
/// is everything that wiki holds. What comes back is only ever a foreign
/// concept page: a wiki's own pages are never cut, and every identity card of
/// the forest is offered whole at any size (the product limits cap them), so
/// both are excluded here rather than competing for the seats.
///
/// Card vectors are embedded by the reindex pipeline — the planner has no
/// embedder and deliberately does not grow one (same rule as the compiler) —
/// so a page whose card never embedded cannot be ranked by nearness. It can
/// still arrive on the two sources that read no vector at all.
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
    // Every page of the forest, so the asking side sees all of its own; the
    // exclusions below decide what may come back.
    let mut by_source_path: BTreeMap<String, String> = BTreeMap::new();
    let mut wiki_of: BTreeMap<String, String> = BTreeMap::new();
    for e in registry.entries.values() {
        if let Some(path) = registry_source_path(tree, e) {
            by_source_path.insert(path, e.slug.clone());
            wiki_of.insert(e.slug.clone(), e.wiki_id.clone());
        }
    }
    let foundation_slugs: BTreeSet<String> = foundation.keys().cloned().collect();
    for (slug, p) in foundation {
        if let Some(path) = plan_page_source_path(tree, p) {
            by_source_path.insert(path, slug.clone());
            wiki_of.insert(slug.clone(), p.wiki_id.clone());
        }
    }

    let mut candidates = crate::candidates::CandidatePool::load(pool, &by_source_path).await;
    let mut embedded = 0usize;
    for (path, slug) in &by_source_path {
        if let Ok(Some(row)) = crate::page_card::get(pool, path).await
            && let Some(v) = row.embedding
        {
            candidates.set_embedding(slug, v);
            embedded += 1;
        }
    }

    let mut by_wiki: BTreeMap<String, Vec<crate::candidates::Candidate>> = BTreeMap::new();
    for wiki in wikis {
        let mine: Vec<&str> = wiki_of
            .iter()
            .filter(|(_, w)| *w == wiki)
            .map(|(slug, _)| slug.as_str())
            .collect();
        let ask = candidates.ask_for(mine.iter().copied());
        // Out of the running: this wiki's own pages (never cut) and every
        // identity card (offered whole at any size).
        let exclude: BTreeSet<String> = mine
            .iter()
            .map(|s| (*s).to_owned())
            .chain(foundation_slugs.iter().cloned())
            .collect();
        by_wiki.insert(
            wiki.clone(),
            candidates.pick(&ask, &exclude, FOREIGN_SELECTION_PAGES, &[]),
        );
    }
    tracing::info!(
        pages = foundation.len() + registry.entries.len(),
        ceiling = FOREST_PAGE_CEILING,
        embedded,
        wikis = by_wiki.len(),
        "planner: forest page list over its ceiling — composing foreign candidates per wiki"
    );
    ForeignPages::Selected(by_wiki)
}

/// The `page_card` key of a planned page — its workdir-relative source path.
fn plan_page_source_path(tree: &WikiTree, p: &PagePlan) -> Option<String> {
    let handle = tree
        .locate(&crate::types::WikiId::parse(&p.wiki_id).ok()?)
        .ok()?;
    Some(crate::wiki::workdir_relative_source_path(
        tree.workdir(),
        &handle.abs_dir().join(&p.page_path),
    ))
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

/// Hold a page the Conciliatore **accepted** to the one rule left.
///
/// What comes back from this stage is materialised into the plan *and
/// persisted into the concept registry*, and it arrives as free-form JSON —
/// the model re-emits `slug` while it decides merges, so it can come back
/// changed.
///
/// **A reserved page name is refused outright** (`rules`, `projects`,
/// `profile`). A page keyed by one of those stems compiles to the same file
/// as the wiki's card or one of its channels — two plan pages, one path. The
/// facts meant for it fall through to the orphan pass.
fn vet_accepted(mut np: NewPage, _foundation: &BTreeMap<String, PagePlan>) -> Option<NewPage> {
    let slug = slugify(&np.slug);
    if crate::wiki::is_reserved_page_stem(&slug) {
        tracing::warn!(
            slug = %slug,
            "conciliatore: accepted a reserved page name — page dropped"
        );
        return None;
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
///   identity, which is not a topic something can be merged *into*.
///   [`describe_existing`] does not offer one and this refuses one named
///   anyway — the two halves of the same rule, because the prompt's own bias
///   is *«when in doubt, prefer the redirect»*.
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
        let cadence_directive = cadence_directive(signals.cadence);
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
                ("cadence", cadence_directive.as_str()),
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
                tracing::warn!(error = %e, "cartografo: LLM failed for a batch, facts will identity fallback");
                continue;
            },
        };
        let Some(bp) = parse_json::<Blueprint>(&resp.text) else {
            tracing::warn!("cartografo: unparseable batch output, facts will identity fallback");
            continue;
        };
        for a in &bp.assignments {
            *running_mass.entry(slugify(&a.page_slug)).or_insert(0) += 1;
        }
        merged.assignments.extend(bp.assignments);
        for np in bp.new_pages {
            let slug = slugify(&np.slug);
            // A coined name is never one of the reserved pages, on either
            // side of the fence: `placement_slug` refuses them for a page the
            // *user* named, and this refuses them for a `concept_leaf` the
            // Cartografo invents. Without it a page called `projects` or
            // `rules` materialises straight over that wiki's channel page —
            // two plan pages, one file — and the channel's own reader, which
            // keys on the path, quietly stops finding what it is for.
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
    /// Settle each new fact onto the page the ingest classifier already
    /// proposed (`fact_index.target_page`), deterministically and with NO LLM
    /// call. A fact with no concrete proposed page (a reserved name / empty /
    /// `None`) falls back to the identity card.
    ///
    /// Since the classifier stopped proposing a page for prose, this places
    /// only what the USER named — a `lista`, or a container they asked for by
    /// name — so on its own it leaves every prose fact waiting in the buffer.
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
    /// subject's identity card and gets there by identity fallback, exactly as
    /// under [`Self::Ingest`].
    NamedThenCartografo(&'a dyn LlmBackend),
    /// The LAST pass of the night: the strong Cartografo over what every
    /// earlier pass left waiting, with the whole forest in view and no next
    /// pass to defer to.
    ///
    /// Same model and same shape as [`Self::Cartografo`]; what differs is the
    /// question. The nightly pass asks *where does this belong*, and declining
    /// is a fine answer because this one comes after it. This one asks *what
    /// page does this need*, and declining costs the claim another day.
    ClosingCartografo(&'a dyn LlmBackend),
    /// No placement intelligence: every new fact falls back to the identity card to its
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
        matches!(
            self,
            Self::Cartografo(_) | Self::NamedThenCartografo(_) | Self::ClosingCartografo(_)
        )
    }

    /// Which pass this placement is, for the prompt's `{cadence}` slot.
    #[must_use]
    pub const fn cadence(&self) -> CartografoCadence {
        match self {
            Self::NamedThenCartografo(_) => CartografoCadence::Hourly,
            Self::ClosingCartografo(_) => CartografoCadence::Closing,
            Self::Ingest | Self::Cartografo(_) | Self::OrphanFallback => CartografoCadence::Nightly,
        }
    }

    /// Stable name of the placement, for the compile log and for the test that
    /// pins which one each cadence actually ships with.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Cartografo(_) => "cartografo",
            Self::NamedThenCartografo(_) => "named-then-cartografo",
            Self::ClosingCartografo(_) => "closing-cartografo",
            Self::OrphanFallback => "identity fallback",
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
/// - `@profile.md` — the wiki's card, a per-wiki **foundation node**; minting
///   a concept page here would put the same file in the plan under a second,
///   forest-wide key;
/// - `@rules.md` ([`crate::wiki::RULES_FILENAME`]) and `@projects.md`
///   ([`crate::wiki::PROJECTS_FILENAME`]) — written by a deterministic
///   channel, so a fact mis-targeted there must not land among the policy or
///   the signposts.
///
/// In every case the fact falls through to [`identity_card_target`], which
/// homes it on its wiki's card when the fact is card material and leaves it
/// unplaced otherwise. The `.md` suffix is stripped first so slugify does not
/// fold it into a trailing `_md`.
fn placement_slug(target_page: &str) -> Option<String> {
    let stripped = target_page.strip_suffix(".md").unwrap_or(target_page);
    // The reserved check runs on the RAW stem, before slugify: slugify keeps
    // only letters and digits, so it eats the `@` marker and `@profile` would
    // arrive here as the innocent-looking `profile`. Checking first is what
    // makes the marker mean anything on this path.
    if crate::wiki::is_reserved_page_stem(&stripped.to_ascii_lowercase()) {
        return None;
    }
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
/// identity fallback. Pure — testable without a DB or an LLM.
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
        // leaving it UNASSIGNED here: the deterministic identity fallback in
        // `build_compilation_plan` then homes it on the subject's card node
        // (`@profile.md`) — see `identity_card_target`, which reads the same salience
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
            // No card here: what belongs on a page is the PAGE's, and the
            // turn that created this page wrote it on the testata
            // (`capture::seed_page_card`). `heal_page_cards` adopts it from
            // the file into this plan and the registry.
            description: String::new(),
            style: f.style.map(|s| s.as_str().to_owned()),
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
        NewFactPlacement::Cartografo(llm) | NewFactPlacement::ClosingCartografo(llm) => {
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
            // The floor applies to what the MODEL invented, never to what the
            // user's own turn named (the `named` half): a list or a container
            // asked for by name is one fact's page by right.
            Ok(merge_blueprints(named, hold_to_birth_floor(classified)))
        },
        NewFactPlacement::OrphanFallback => Ok(Blueprint::default()),
    }
}

/// The `{cadence}` block of the Cartografo prompt.
///
/// Three passes share one prompt file, and this is where they part: the cheap
/// hourly tier groups or waits, the nightly strong tier judges, and the
/// closing pass finishes the queue. Rendered rather than written into the body
/// so no pass reads another's rule.
fn cadence_directive(cadence: CartografoCadence) -> String {
    match cadence {
        CartografoCadence::Hourly => format!(
            "PARK RATHER THAN GUESS — you are the hourly pass, and you are cheap on purpose.\n\
             - Assign a fact to an existing page only when that page is a STRONG match. \
             \"Related\" is not a match: a fact filed on a page it only brushes against is \
             harder to find than one nobody filed, because the page's card stops describing \
             what is on it.\n\
             - **LEAVING A FACT UNPLACED IS AN ANSWER.** When no page is a strong match, OMIT \
             the fact from `assignments`. It is not lost and it lands nowhere: it keeps \
             waiting where it is, and the next pass — or tonight's, which reads a whole wiki \
             at once with a stronger model — sees it again. There is no page meaning \
             \"unsorted\", so never reach for one.\n\
             - You may propose a new page ONLY when you are grouping at least {PAGE_BIRTH_FLOOR} facts on one \
             theme. Below that, omit them: a page born from one or two facts takes its card from \
             them, and that card is the only thing a reader is shown before deciding whether to \
             open the page. Let the pile grow — you will see it again — or leave it to the \
             nightly pass, which reads the whole wiki at once.",
        ),
        CartografoCadence::Nightly => "YOUR JUDGEMENT DECIDES — you are the nightly pass and \
             you are shown the whole wiki. Propose the pages the material actually needs; \
             there is no floor on how many facts a page must group.\n\
             - **LEAVING A FACT UNPLACED IS AN ANSWER**, still: omit a fact no page here \
             suits, and it keeps waiting. A closing pass runs after you, over everything \
             every pass declined, and it is the one that has to finish the queue."
            .to_owned(),
        CartografoCadence::Closing => "YOU ARE THE LAST PASS — every fact in front of you was \
             shown to an earlier pass, which declined it, and NOTHING RUNS AFTER YOU. A fact \
             you leave unassigned waits another whole day.\n\
             - **Every fact must come out with a page.** Omitting one is not an answer here; \
             it is the one outcome this pass exists to prevent.\n\
             - **A page for a single fact is allowed, and you are the only pass that may open \
             one.** If a fact belongs with nothing that exists, that is not a reason to leave \
             it waiting — it is the reason to give it a page of its own. Write its card for \
             the fact itself: what a reader looking for THIS would search for. A thin page is \
             a state, not a mistake; it grows, or a later night folds it into a better home.\n\
             - Prefer an existing page when one genuinely fits — a strong match is still \
             better than a new page. `far` candidates are in front of you precisely because \
             they resemble nothing here: read them before inventing.\n\
             - The one thing you may not do is force a fact onto a page it does not belong \
             on. If no page fits, open one."
            .to_owned(),
    }
}

/// How many facts on one theme must have piled up before a page is born
/// (founder, 2026-08-18).
///
/// The cheap tier must not mint a page out of a fact it is merely unsure
/// about: when nothing on disk is a strong match the claim waits, and a page
/// is born only once at least five claims on one theme have piled up.
///
/// A page born from one fact takes its card from that fact, and the card is
/// the only thing a reader is shown before deciding whether to open the page —
/// so a page opened on a guess is a page nobody can find on purpose. The floor
/// is a **cheap-tier** rule and nothing more: the nightly pass reads a whole
/// wiki at once and keeps its judgement, and the closing pass
/// ([`CartografoCadence::Closing`]) opens a page for a single fact on purpose,
/// because by then the alternative is not "wait an hour" but "wait a day".
///
/// One level up sits its sibling, `RemPolicy::auto_promote_group_min_pages`
/// (default 9): how many pages must group before a **wiki** is born. Same
/// shape, different level — facts make a page, pages make a wiki.
pub const PAGE_BIRTH_FLOOR: usize = 5;

/// Hold a cheap-tier proposal to [`PAGE_BIRTH_FLOOR`]: a page grouping fewer
/// facts than that is **not** born, and the facts meant for it fall through to
/// the orphan pass, which leaves them in the buffer, where they wait for the
/// theme to grow (the queue re-offers them at every light build) or for the
/// night to read them — first the nightly pass, then the closing one, which
/// has to give each of them a page whatever the pile looks like.
///
/// Only proposals are held: an assignment onto a page that already exists is
/// the model recognising a home, not inventing one, and no floor applies.
fn hold_to_birth_floor(mut bp: Blueprint) -> Blueprint {
    if bp.new_pages.is_empty() {
        return bp;
    }
    let mut mass: BTreeMap<&str, usize> = BTreeMap::new();
    for a in &bp.assignments {
        *mass.entry(a.page_slug.as_str()).or_default() += 1;
    }
    let refused: BTreeSet<String> = bp
        .new_pages
        .iter()
        .filter(|np| mass.get(np.slug.as_str()).copied().unwrap_or(0) < PAGE_BIRTH_FLOOR)
        .map(|np| np.slug.clone())
        .collect();
    if refused.is_empty() {
        return bp;
    }
    for slug in &refused {
        tracing::info!(
            slug = %slug,
            floor = PAGE_BIRTH_FLOOR,
            grouped = mass.get(slug.as_str()).copied().unwrap_or(0),
            "cartografo: proposal under the birth floor — its facts park instead"
        );
    }
    bp.new_pages.retain(|np| !refused.contains(&np.slug));
    bp.assignments.retain(|a| !refused.contains(&a.page_slug));
    bp
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
/// The conciliatore output schema carries only slug / title / description —
/// not `style` — so a parsed `accepted_new` item comes back with
/// `style: None`. Left as-is, an ingest-proposed `lista` page
/// would lose its style through conciliation and be demoted to full-prose
/// compilation. We restore it from the original `new_pages` by slug (matching
/// the canonical `slugify` form so a re-slugged proposal still lands), never
/// trusting the LLM to transcribe it. `style` is the only `NewPage` field the
/// schema drops; the rest the model is asked to preserve verbatim.
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
/// the strong-model Cartografo; or deterministic identity fallback).
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
    waiting: &[FactForPage],
) -> Result<CompilationPlan> {
    // Two sources, one placement. `gather_standard_facts` is the memory as it
    // stands — facts already on a page. `waiting` is the queue: claims screened
    // out of `capture_buffer` that are not `fact_index` rows yet, and become
    // ones only once this plan says which page each goes on
    // (`dream_light::materialise`). Judging both together is the point: a claim
    // is placed against the memory as it is at that moment, neighbours
    // included.
    let mut facts = gather_standard_facts(pool, tree).await?;
    facts.extend(waiting.iter().cloned());
    facts.sort_by(|a, b| a.fact_id.as_str().cmp(b.fact_id.as_str()));
    let foundation = build_foundation_pages(pool).await?;
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
    let mut signals = CartografoSignals {
        // Which pass is asking. It decides the whole of what the prompt's
        // `{cadence}` slot says — the floor, and whether declining a fact is
        // an answer.
        cadence: placement.cadence(),
        ..CartografoSignals::default()
    };
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
    // ran unchecked until 2026-08-10.
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
    // answer, so the record is **born-applied** — the same
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
/// (`compiler::page_index_block`), its own line included.
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
/// A person's card is excluded: it appears because a *user* was enrolled,
/// which has its own visible route.
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
        if prev.pages.contains_key(slug) || page.is_identity_card() {
            continue;
        }
        let context = serde_json::json!({
            "slug": slug,
            "wiki_id": page.wiki_id,
            "page_path": page.page_path,
            "title": page.title,
            "description": page.description,
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
            // The reserved channel pages (`@rules.md`, `@projects.md`) are their
            // own pipelines' perimeter, not the compiler's: their facts are
            // written directly and read back keyed on that path. The compiler
            // must NOT gather them — absent from the persisted plan they would
            // look "new", fall back to the identity card, and their channel
            // (which filters on the page) would stop seeing them.
            // (engine_rule governance is raw `@rules.md` prose, not a
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
/// Cards are bounded by the product limits (24 users, 8 groups), so this
/// list does not grow with the memory and is never cut.
fn describe_foundation(
    foundation: &BTreeMap<String, PagePlan>,
    wiki: &str,
    mass: &BTreeMap<String, usize>,
) -> String {
    let mut lines: Vec<String> = foundation
        .values()
        .filter(|p| p.wiki_id == wiki)
        .map(|p| {
            format!(
                "- [{}] {} — {} | facts: {}",
                p.prompt_kind(),
                p.slug,
                p.title,
                mass_of(mass, &p.slug),
            )
        })
        .collect();
    // The other wikis' identity cards, each named with the wiki it belongs to
    // so the model is choosing a page in a place, not a bare slug.
    lines.extend(
        foundation
            .values()
            .filter(|p| p.wiki_id != wiki && p.is_identity_card())
            .map(|p| {
                format!(
                    "- [{}] {} — {} | wiki: {} | facts: {}",
                    p.prompt_kind(),
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
/// a place, and a slug alone does not say which — and, once the forest is past
/// its ceiling, `via: <source>`, because a page offered *because it resembles
/// nothing here* is a different offer from one made because it resembles
/// everything, and only the model can act on the difference.
fn describe_concepts(
    registry: &ConceptRegistry,
    wiki: &str,
    this_run: &[(&NewPage, &str)],
    mass: &BTreeMap<String, usize>,
    foreign: &ForeignPages,
) -> String {
    // Every registry entry is an ordinary page — a wiki's own pages are
    // foundation nodes and never live here — so the kind is a constant.
    let line = |slug: &str, title: &str, description: &str, home: Option<&str>| {
        // The `wiki:` field appears only on a page of another wiki: on the
        // batch's own pages it would be the same id on every line. Same for
        // `via:`, which only a selected page has.
        let via = foreign
            .source(wiki, slug)
            .map_or_else(String::new, |s| format!("via: {} | ", s.tag()));
        let home = home.map_or_else(String::new, |h| format!("wiki: {h} | "));
        format!(
            "- [concept_leaf] {slug} — {title} | {home}{via}{description} | facts: {}",
            mass_of(mass, slug),
        )
    };
    let mut lines: Vec<String> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id == wiki)
        .map(|e| line(&e.slug, &e.title, &e.description, None))
        .collect();
    // In the order the selection picked, and no re-sort by slug afterwards:
    // where a list is cut the order IS the selection, and rendering it
    // alphabetically would hand back the ordering the selection exists to
    // replace.
    let mut foreign_entries: Vec<&ConceptRegistryEntry> = registry
        .entries
        .values()
        .filter(|e| e.wiki_id != wiki && foreign.offers(wiki, &e.slug))
        .collect();
    foreign_entries.sort_by_key(|e| foreign.rank(wiki, &e.slug));
    lines.extend(
        foreign_entries
            .into_iter()
            .map(|e| line(&e.slug, &e.title, &e.description, Some(&e.wiki_id))),
    );
    for (np, home) in this_run {
        lines.push(format!(
            "{} (proposed this run)",
            line(
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
        if p.wiki_id != wiki && !p.is_identity_card() {
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
/// **A wiki's card is not offered, because a merge cannot land on one.** It
/// carries a subject's identity, which is not a topic a page can become part
/// of, and the prompt's standing bias is *«when in doubt, prefer the
/// redirect»*. [`vet_redirects`] refuses one named anyway.
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
            "- [concept_leaf] {} — {} | wiki: {} | {}",
            e.slug, e.title, e.wiki_id, e.description
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
    // In the order the selection picked, never re-sorted by slug: where a
    // list is cut, the order IS the selection.
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
                "- [concept_leaf] {} — {} | {}",
                np.slug, np.title, np.description
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
        named.style = Some(crate::wiki::PageStyle::Lista);

        let mut unplaced = fact(
            2,
            "Bob ha cominciato nuoto il martedi",
            "user:franz",
            "franz",
        );
        unplaced.target_page = None;

        let mut reserved = fact(3, "Franz e celiaco", "user:franz", "franz");
        reserved.target_page = None;
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
        named.style = Some(crate::wiki::PageStyle::Lista);
        let mut unplaced = fact(2, "nuoto il martedi", "user:franz", "franz");
        unplaced.target_page = None;

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
            primary_facts: Vec::new(),
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
            wiki_id: slug.to_owned(),
            page_path: crate::wiki::PROFILE_FILENAME.to_owned(),
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
    /// proposal (`target_page` / `style`) on `fact_index`. Returns the fact id.
    /// Plant a fact for alice. `salience: "high"` is what makes it placeable
    /// with no model in the loop: since 2026-08-22 the identity card is the
    /// only deterministic home, and a normal-salience fact nobody places
    /// simply waits.
    async fn plant_alice_fact(
        pool: &SqlitePool,
        id_tail: &str,
        text: &str,
        target_page: Option<&str>,
        style: Option<&str>,
    ) -> FactId {
        plant_alice_fact_with_salience(pool, id_tail, text, target_page, style, None).await
    }

    async fn plant_alice_fact_with_salience(
        pool: &SqlitePool,
        id_tail: &str,
        text: &str,
        target_page: Option<&str>,
        style: Option<&str>,
        salience: Option<&str>,
    ) -> FactId {
        let fid = FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id_tail}")).unwrap();
        fact_index::insert(
            pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
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
                style: crate::wiki::PageStyle::parse_lenient(style),
                salience: salience.map(str::to_owned),
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
        // **A coined name is one segment**, flattened (founder, 2026-08-19:
        // *«un utente non può poter creare cartelle»*). Honoured per segment
        // it would make the folder — the write creates missing parents — and
        // where a page belongs is the wiki's call, not a prefix's.
        assert_eq!(
            canonical_page_path("Diario/Episodi Marzo"),
            Some("diario_episodi_marzo.md".to_owned())
        );
        // A traversal is still refused rather than flattened: `escape` is a
        // plausible page name, and inventing one out of an attack is worse
        // than dropping the name.
        assert_eq!(canonical_page_path("nested/../escape"), None);
        // Traversal / noise segments are refused, not repaired.
        assert_eq!(canonical_page_path("../escape"), None);
        assert_eq!(canonical_page_path("---"), None);
        assert_eq!(canonical_page_path("   "), None);
    }

    #[test]
    fn placement_slug_flattens_paths_and_refuses_reserved_names() {
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
        // Empty → None, so the fact reaches the subject's foundation page
        // via identity fallback.
        assert_eq!(placement_slug(""), None);
        assert_eq!(placement_slug("  "), None);
        // `@rules.md` → None: the reserved user-policy page is never a
        // fact-bearing concept page; a mis-targeted fact falls back to the identity card.
        assert_eq!(placement_slug("@rules.md"), None);
        assert_eq!(placement_slug("rules"), None);
    }

    /// **A page is not born for one fact.** The cheap hourly pass may propose a
    /// page only when it is grouping at least [`PAGE_BIRTH_FLOOR`] facts on one
    /// theme; below that the proposal is dropped and its facts fall through to
    /// the orphan pass, which leaves them in the buffer. They come back to
    /// the same pass next hour, so the pile can still reach the floor
    /// (founder, 2026-08-18).
    #[test]
    fn a_page_is_not_born_under_the_birth_floor() {
        let np = |slug: &str| NewPage {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: "cosa ci va".to_owned(),
            style: None,
        };
        let assign = |id: u8, slug: &str| Assignment {
            fact_id: format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{id:02}"),
            page_slug: slug.to_owned(),
        };
        let bp = Blueprint {
            new_pages: vec![np("giardino"), np("nuoto")],
            assignments: vec![
                // `giardino` groups five → born.
                assign(1, "giardino"),
                assign(2, "giardino"),
                assign(3, "giardino"),
                assign(4, "giardino"),
                assign(5, "giardino"),
                // `nuoto` groups two → parked instead.
                assign(6, "nuoto"),
                assign(7, "nuoto"),
                // An EXISTING page takes one fact and no floor applies: the
                // model recognised a home, it did not invent one.
                assign(8, "preferenze"),
            ],
        };

        let held = hold_to_birth_floor(bp);
        assert_eq!(
            held.new_pages
                .iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["giardino"],
            "only the page that grouped enough is born"
        );
        let slugs: Vec<&str> = held
            .assignments
            .iter()
            .map(|a| a.page_slug.as_str())
            .collect();
        assert_eq!(slugs.iter().filter(|s| **s == "giardino").count(), 5);
        assert_eq!(
            slugs.iter().filter(|s| **s == "nuoto").count(),
            0,
            "the refused page's facts are unassigned, so the orphan pass leaves them in the buffer"
        );
        assert_eq!(
            slugs.iter().filter(|s| **s == "preferenze").count(),
            1,
            "an assignment onto an existing page is untouched"
        );
    }

    /// One prompt, three passes, and the two questions they part on: how many
    /// facts a page needs to be born, and whether declining is an answer.
    ///
    /// The last one is the whole of this slot's job. The hourly and nightly
    /// passes may leave a claim waiting because something runs after them; the
    /// closing pass has nothing after it, so for it the same sentence would
    /// cost the claim another day.
    #[test]
    fn each_pass_is_told_what_it_may_leave_undone() {
        let hourly = cadence_directive(CartografoCadence::Hourly);
        assert!(hourly.contains("at least 5"), "{hourly}");
        assert!(
            hourly.contains("LEAVING A FACT UNPLACED IS AN ANSWER"),
            "{hourly}"
        );

        let nightly = cadence_directive(CartografoCadence::Nightly);
        assert!(nightly.contains("no floor"), "{nightly}");
        assert!(!nightly.contains("at least 5"), "{nightly}");
        assert!(
            nightly.contains("LEAVING A FACT UNPLACED IS AN ANSWER"),
            "a closing pass runs after it, so it may still decline: {nightly}"
        );

        let closing = cadence_directive(CartografoCadence::Closing);
        assert!(
            !closing.contains("LEAVING A FACT UNPLACED IS AN ANSWER"),
            "nothing runs after the closing pass: {closing}"
        );
        assert!(
            closing.contains("Every fact must come out with a page"),
            "{closing}"
        );
        assert!(
            closing.contains("single fact is allowed"),
            "it is the only pass that may open a page for one fact: {closing}"
        );
    }

    /// Which pass each placement is. The closing pass shares the nightly
    /// model and the nightly shape; what it does not share is permission to
    /// leave the queue as it found it.
    #[test]
    fn a_placement_knows_which_pass_it_is() {
        use crate::llm::FakeLlmBackend;
        let llm = FakeLlmBackend::new("fake", "{}");
        assert_eq!(
            NewFactPlacement::NamedThenCartografo(&llm).cadence(),
            CartografoCadence::Hourly
        );
        assert_eq!(
            NewFactPlacement::Cartografo(&llm).cadence(),
            CartografoCadence::Nightly
        );
        assert_eq!(
            NewFactPlacement::ClosingCartografo(&llm).cadence(),
            CartografoCadence::Closing
        );
        assert!(NewFactPlacement::ClosingCartografo(&llm).runs_cartografo());
    }

    #[test]
    fn ingest_placement_blueprint_assigns_to_target_and_leaves_the_card_to_the_page() {
        // Two facts → the same `spesa` page (dedup to ONE NewPage, the first
        // fact's style wins), one fact → a reserved name (no assignment and no
        // page, left for identity fallback), one fact → no proposal at all.
        let mut latte = fact(1, "latte", "user:alice", "alice");
        latte.target_page = Some("spesa.md".to_owned());
        latte.style = Some(crate::wiki::PageStyle::Lista);
        let mut pane = fact(2, "pane", "user:alice", "alice");
        pane.target_page = Some("spesa.md".to_owned());
        pane.style = Some(crate::wiki::PageStyle::Prosa); // ignored — first fact wins.
        let mut bio = fact(3, "Alice lives in Lisbon", "user:alice", "alice");
        bio.target_page = Some("@rules.md".to_owned()); // → orphan, not a page.
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
        assert_eq!(np.style.as_deref(), Some("lista"));
        assert_eq!(
            np.description, "",
            "the card is the PAGE's: the turn that created the page wrote it              on the testata, and `heal_page_cards` adopts it from there"
        );
    }

    #[test]
    fn ingest_placement_blueprint_routes_high_salience_off_its_target_page() {
        // A `high`-salience fact's home is the subject's identity card —
        // the routing IS the reservation. Even with a
        // concrete ingest `target_page`, it must be left UNASSIGNED here (the
        // override) so the identity fallback homes it on the foundation page. A
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
    fn high_salience_fact_homes_on_the_card_via_orphan_fallback() {
        // End-to-end through the deterministic plan: a `high` fact with a concrete
        // target_page lands on the actor's card (`@profile.md`), and NO
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

        // The high fact falls back to the identity card onto alice's card.
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_path, crate::wiki::PROFILE_FILENAME);
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
        // Only the ASSIGNED fact lands. The other reached the fallback and is
        // normal-salience, so it is placed nowhere and keeps waiting.
        let alice = &plan.pages["alice"];
        assert_eq!(alice.primary_facts.len(), 1);
        // `fact_count` is what the plan was HANDED, not what it placed — both
        // facts are still the corpus's, one of them just has no page yet.
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
                    wiki_id: "alice".to_owned(),
                    created_at: "t".to_owned(),
                },
            );
        }
        let mut latte = fact(1, "latte", "user:alice", "alice");
        latte.style = Some(crate::wiki::PageStyle::Lista);
        let mut pane = fact(2, "pane", "user:alice", "alice");
        pane.style = crate::wiki::PageStyle::parse(" Lista "); // read loosely, same value
        let mut nutella = fact(3, "nutella", "user:alice", "alice");
        nutella.style = Some(crate::wiki::PageStyle::Prosa); // outvoted 2:1
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
            reg.entries["spesa"]
                .style
                .map(crate::wiki::PageStyle::as_str),
            Some("lista"),
            "majority fact style adopted into the registry entry"
        );
        assert_eq!(
            plan.pages["spesa"]
                .style
                .map(crate::wiki::PageStyle::as_str),
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
    fn architetto_gc_removes_every_empty_concept_page() {
        let foundation = BTreeMap::new();
        // Two registry pages, neither of which any fact was assigned to.
        let mut registry = ConceptRegistry::empty("t");
        registry.entries.insert(
            "cucina".to_owned(),
            ConceptRegistryEntry {
                slug: "cucina".to_owned(),
                title: "Cucina".to_owned(),
                description: "d".to_owned(),
                style: None,
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
        // Both go, and the registry loses them too — a page nobody filed a
        // fact on is not a page the next cycle should reuse.
        assert!(!plan.pages.contains_key("leaf"));
        assert!(!plan.pages.contains_key("cucina"));
        assert!(!updated.entries.contains_key("cucina"));
        assert!(!updated.entries.contains_key("leaf"));
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
              \"new_pages\":[{\"slug\":\"salute_alice\",\"title\":\"Salute\",\"description\":\"d\"}]}",
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
    async fn the_plan_adopts_the_card_the_page_itself_carries() {
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The shape of the confirmed case: the turn named the fair and the
        // page was minted for it, with no card of its own yet.
        let fid = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d99").unwrap();
        fact_index::insert(
            &pool,
            &NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
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
                style: Some(crate::wiki::PageStyle::Prosa),
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
            &[],
        )
        .await
        .expect("plan");
        assert_eq!(
            plan.pages["fiera"].description, "",
            "a page nobody has described yet has no card — the card is the \
             PAGE's, and this page has not been written"
        );

        // The page is written, and the writer's card says what the facts
        // actually support.
        std::fs::write(
            wikis.join("alice/fiera.md"),
            "---\ntitle: \"Fiera\"\ncreated: 2026-08-05\nupdated: 2026-08-05\nstyle: prosa\ndescription: \"the east fair, and what Alice has said about it\"\n---\n\nqualcosa.\n",
        )
        .unwrap();

        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-08-05T01:00:00Z",
            &[],
        )
        .await
        .expect("plan 2");
        assert_eq!(
            plan2.pages["fiera"].description, "the east fair, and what Alice has said about it",
            "the plan adopts what the page itself says"
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
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

        // First build (no LLM → foundation + deterministic subject-page fallback).
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T00:00:00Z",
            &[],
        )
        .await
        .expect("plan");
        assert!(plan.pages.contains_key("alice"), "alice person page exists");
        let alice = &plan.pages["alice"];
        assert_eq!(alice.page_path, "@profile.md", "the identity card");
        assert!(
            alice.primary_facts.is_empty(),
            "a normal-salience orphan belongs on the buffer, not the identity card"
        );
        assert!(
            plan.pages.values().all(|p| p.primary_facts.is_empty()),
            "a normal-salience fact nobody placed is on NO page: it keeps its \
             buffer row and the next pass sees it again"
        );
        let _ = fid;
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
            &[],
        )
        .await
        .expect("plan2");
        assert!(
            plan2.dirty_pages.is_empty(),
            "unchanged corpus → 0 dirty pages"
        );
        assert!(
            plan2.pages.values().all(|p| p.primary_facts.is_empty()),
            "and it is still on no page — waiting is a stable state"
        );
        drop(dir);
    }

    /// A behaviour-rule fact lives on the reserved policy page `@rules.md`
    /// (written by the rules pipeline's direct path, not the planner). The
    /// compiler must leave it there: gathering it would put it through the
    /// placement pass, changing its `source_path` so `recall_behaviour_rules`
    /// (which filters on `@rules.md`) stops seeing it. Regression for the
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
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
            salience: Some("high".to_owned()),
            source_ref: None,
        };
        // A normal content fact (must be homed) ...
        let content = mk("01", "wikis/alice/appunti_vari.md", "preference");
        let content_id = content.fact_id.clone();
        fact_index::insert(&pool, &content).await.unwrap();
        // ... and a behaviour-rule fact on the reserved `@rules.md` (must be spared).
        let rule = mk("02", "wikis/alice/@rules.md", "rule");
        let rule_id = rule.fact_id.clone();
        fact_index::insert(&pool, &rule).await.unwrap();

        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-30T00:00:00Z",
            &[],
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
    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end seam: plant, plan, move, re-plan — splitting it \
                  would hide the order the test is about"
    )]
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact_with_salience(
            &pool,
            "b1",
            "Matteo does karate on Mondays",
            None,
            None,
            Some("high"),
        )
        .await;

        // First build: the fact orphan-homes on alice's foundation page.
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-06-11T00:00:00Z",
            &[],
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
            &[],
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
            &[],
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact_with_salience(
            &pool,
            "b1",
            "Matteo does karate on Mondays",
            None,
            None,
            Some("high"),
        )
        .await;
        build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-07-02T00:00:00Z",
            &[],
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
            &[],
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
            &[],
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
            "the re-opened page's fact re-entered the pool and re-placed (fallback → the identity card)"
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let fid = plant_alice_fact(&pool, "b3", "potatura", Some("giardinaggio"), None).await;
        build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-08-05T00:00:00Z",
            &[],
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The ingest classifier placed the fact on `spesa`.
        let fid = plant_alice_fact(&pool, "b2", "latte", Some("spesa"), Some("lista")).await;
        let plan = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-06-11T00:00:00Z",
            &[],
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
        // a fact naming a reserved page falls back to the identity card to its subject's wiki.
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let wikis = dir.path().join("wikis");
        std::fs::create_dir_all(wikis.join("alice")).unwrap();
        std::fs::write(
            wikis.join("alice/_meta.md"),
            "---\nwiki_id: alice\nwiki_type: wiki-user\nslug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n",
        )
        .unwrap();
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
        let tree = WikiTree::open(dir.path()).expect("tree");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('alice','[]',0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let spesa = plant_alice_fact(&pool, "a1", "latte", Some("spesa.md"), Some("lista")).await;
        let home = plant_alice_fact(
            &pool,
            "a2",
            "Alice lives in Lisbon",
            Some("@rules.md"),
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
            &[],
        )
        .await
        .expect("plan");

        // The `spesa.md` fact made a `spesa` concept_leaf, NOT homed on alice.
        let spesa_page = plan.pages.get("spesa").expect("spesa page minted");
        assert_eq!(spesa_page.wiki_id, "alice"); // homed in the fact's wiki.
        assert_eq!(spesa_page.style, Some(crate::wiki::PageStyle::Lista));
        assert_eq!(
            spesa_page.description, "",
            "the card belongs to the page, written on its testata by the turn \
             that created it — not carried here by a fact"
        );
        assert_eq!(spesa_page.primary_facts.len(), 1);
        assert_eq!(spesa_page.primary_facts[0].fact_id, spesa);
        // The reserved-name fact stayed in the capture buffer, on no page at
        // all: the card is for the identity core a `high` salience reserves,
        // and this fact is `normal`.
        let alice = &plan.pages["alice"];
        assert!(alice.primary_facts.is_empty());
        assert!(
            plan.pages
                .values()
                .all(|p| p.primary_facts.iter().all(|f| f.fact_id != home)),
            "a normal-salience fact naming a reserved page is placed nowhere \
             and waits in the buffer"
        );

        // Incremental idempotency: re-running with no change → 0 dirty pages.
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-05-31T01:00:00Z",
            &[],
        )
        .await
        .expect("plan2");
        assert!(plan2.dirty_pages.is_empty(), "unchanged → 0 dirty");
        assert_eq!(plan2.pages["spesa"].primary_facts.len(), 1);

        // A NEW fact on the SAME ingest page accretes onto it — no duplicate page.
        plant_alice_fact(&pool, "a3", "pane", Some("spesa.md"), Some("lista")).await;
        let plan3 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::Ingest,
            None,
            "2026-05-31T02:00:00Z",
            &[],
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
        std::fs::write(wikis.join("alice/cucina.md"), "# cucina\n").unwrap();
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
                source_path: "wikis/alice/appunti_vari.md".to_owned(),
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
                salience: Some("high".to_owned()),
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
            &[],
        )
        .await
        .expect("plan");
        let plan2 = build_wiki_plan(
            &pool,
            &tree,
            NewFactPlacement::OrphanFallback,
            None,
            "2026-05-31T01:00:00Z",
            &[],
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
            &[],
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

    /// A page proposed by an EARLIER batch of another wiki is a destination,
    /// not a forbidden name.
    ///
    /// It is homed in that batch's wiki, and this batch may still assign to
    /// it: a page about to exist is a page to reuse rather than duplicate, and
    /// reusing one across wikis is a legitimate placement. What the collision
    /// list keeps is what nothing showed — here, the other wiki's card.
    #[test]
    fn a_proposal_from_another_wikis_batch_is_offered_not_fenced_off() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let registry = ConceptRegistry::empty("t");
        let orto = NewPage {
            slug: "orto".to_owned(),
            title: "Orto".to_owned(),
            description: "the vegetable patch".to_owned(),
            style: None,
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
            "(none)",
            "alice's card is offered by name, so its slug is not also fenced off"
        );
    }

    /// A forest over [`FOREST_PAGE_CEILING`], laid out so the two pages that
    /// only their own source can reach sit at the far end of the ranking.
    ///
    /// `bob` owns the first three pages; every other page is `alice`'s, and
    /// their cards fan away from `bob`'s axis one step at a time. Returns the
    /// slugs of the far page that shares a principal with `bob` and the far
    /// page that shares a turn with him.
    async fn over_ceiling_corpus(
        dir: &std::path::Path,
        pool: &SqlitePool,
        registry: &mut ConceptRegistry,
    ) -> (String, String) {
        let wikis = dir.join("wikis");
        for w in ["alice", "bob"] {
            std::fs::create_dir_all(wikis.join(w)).unwrap();
            std::fs::write(
                wikis.join(w).join("_meta.md"),
                format!(
                    "---\nwiki_id: {w}\nwiki_type: wiki-user\nslug: {w}\ntitle: {w}\nacl_default: 'user:{w}'\n---\n"
                ),
            )
            .unwrap();
        }
        let card = async |slug: &str, wiki: &str, v: Vec<f32>| {
            let source_path = format!("wikis/{wiki}/{slug}.md");
            crate::page_card::upsert(
                pool,
                &crate::page_card::NewPageCard {
                    source_path: source_path.clone(),
                    wiki_id: wiki.to_owned(),
                    description: Some(format!("{slug} desc")),
                    keywords: Vec::new(),
                    style: None,
                    file_mtime_ms: None,
                    file_size: None,
                },
            )
            .await
            .expect("card");
            crate::page_card::set_embedding(pool, &source_path, &v)
                .await
                .expect("vector");
        };
        for i in 0..=FOREST_PAGE_CEILING {
            let wiki = if i < 3 { "bob" } else { "alice" };
            let slug = format!("p{i:04}");
            registry
                .entries
                .insert(slug.clone(), concept_entry(&slug, wiki));
            #[expect(clippy::cast_precision_loss, reason = "bounded loop counter")]
            let t = i as f32 / (FOREST_PAGE_CEILING + 1) as f32;
            card(&slug, wiki, vec![1.0 - t, t]).await;
        }

        let plant = async |tail: u8, path: &str, subject: &str| -> FactId {
            let fid =
                FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{tail:02x}")).unwrap();
            fact_index::insert(
                pool,
                &crate::fact_index::NewFact {
                    authored_refs: Vec::new(),
                    fact_id: fid.clone(),
                    wiki_id: "alice".to_owned(),
                    source_path: path.to_owned(),
                    region_start: None,
                    region_end: None,
                    text: "x".to_owned(),
                    embedding: vec![0.1, 0.2],
                    subject_id: subject.parse::<Principal>().unwrap(),
                    allow_ids: Vec::new(),
                    sender_id: None,
                    fact_type: None,
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
            .expect("fact");
            fid
        };
        let far_person = format!("p{FOREST_PAGE_CEILING:04}");
        let far_turn = format!("p{:04}", FOREST_PAGE_CEILING - 1);
        // `carol` is on one of bob's pages and on the far page, and nowhere
        // else — so she discriminates instead of being everybody's principal.
        plant(0x01, "wikis/bob/p0000.md", "user:carol").await;
        plant(0x02, &format!("wikis/alice/{far_person}.md"), "user:carol").await;
        // And one turn produced a fact on one of bob's pages and one on the
        // other far page.
        let a = plant(0x03, "wikis/bob/p0001.md", "user:zoe").await;
        let b = plant(0x04, &format!("wikis/alice/{far_turn}.md"), "user:yan").await;
        for id in [&a, &b] {
            sqlx::query(
                "INSERT INTO capture_buffer
                   (capture_id, body, subject_id, status, captured_at, origin_message_hash)
                 VALUES (?, 'x', 'user:bob', 'promoted', 't', 'turn-1')",
            )
            .bind(id.as_str())
            .execute(pool)
            .await
            .expect("buffer row");
        }
        (far_person, far_turn)
    }

    /// Past the ceiling the foreign offer is composed, not ranked — and every
    /// source that has something to give is in it.
    ///
    /// The two pages this asserts on resemble `bob` in **nothing**: a
    /// nearest-N cut would put them at the bottom of four hundred and they
    /// would never be offered, so a fact of `bob`'s that belongs on either
    /// could never find its way there. That is the whole change.
    #[tokio::test]
    async fn past_the_ceiling_the_foreign_offer_comes_from_all_four_sources() {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        let mut registry = ConceptRegistry::empty("t");
        let (far_person, far_turn) = over_ceiling_corpus(dir.path(), &pool, &mut registry).await;
        let tree = WikiTree::open(dir.path()).expect("tree");

        let foundation: BTreeMap<String, PagePlan> =
            std::iter::once(("bob".to_owned(), person("bob"))).collect();
        let asking: BTreeSet<String> = std::iter::once("bob".to_owned()).collect();
        let offers = foreign_page_offers(&pool, &tree, &foundation, &registry, &asking).await;
        let ForeignPages::Selected(by_wiki) = &offers else {
            panic!("over the ceiling the offer must be a selection");
        };
        let picked = by_wiki.get("bob").expect("bob asked");

        let by_source: BTreeMap<&str, Vec<&str>> =
            picked.iter().fold(BTreeMap::new(), |mut acc, c| {
                acc.entry(c.source.tag()).or_default().push(&c.key);
                acc
            });
        assert!(
            by_source
                .get("same-people")
                .is_some_and(|v| v.contains(&far_person.as_str())),
            "the page about the same person is offered although nothing about it is near: {by_source:?}"
        );
        assert!(
            by_source
                .get("same-turn")
                .is_some_and(|v| v.contains(&far_turn.as_str())),
            "and so is the page from the same conversation: {by_source:?}"
        );
        assert!(by_source.contains_key("near"), "{by_source:?}");
        assert!(by_source.contains_key("far"), "{by_source:?}");
        assert!(
            picked
                .iter()
                .all(|c| !["p0000", "p0001", "p0002"].contains(&c.key.as_str())),
            "bob's own pages are never cut, so they never take a foreign seat: {picked:?}"
        );
        assert!(
            picked.iter().all(|c| c.key != "bob"),
            "an identity card is offered whole and never competes here: {picked:?}"
        );
        assert_eq!(
            picked.len(),
            FOREIGN_SELECTION_PAGES,
            "the budget is filled"
        );

        // Same corpus, same list: a selection a second run disagrees with
        // would recompile every page it moved.
        let again = foreign_page_offers(&pool, &tree, &foundation, &registry, &asking).await;
        let ForeignPages::Selected(again) = &again else {
            panic!("still a selection");
        };
        assert_eq!(again.get("bob"), Some(picked));
        drop(dir);
    }

    /// Past the ceiling the forest is cut, and the two halves stay exhaustive:
    /// what the selection carries is described in the order it was picked and
    /// says why, what it drops falls back onto the collision list rather than
    /// vanishing.
    ///
    /// A page nobody shows and nobody names is a name a later batch can coin,
    /// which is the accident `{taken_slugs}` exists to prevent — so the cut
    /// may make the offer smaller, never the guard.
    #[test]
    fn past_the_ceiling_the_offer_is_cut_and_the_rest_stays_a_taken_name() {
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
                .insert(slug.to_owned(), concept_entry(slug, wiki));
        }
        // The selection picked two of alice's three, in this order, and each
        // for its own reason.
        let foreign = ForeignPages::Selected(BTreeMap::from([(
            "bob".to_owned(),
            vec![
                crate::candidates::Candidate {
                    key: "karate".to_owned(),
                    source: crate::candidates::CandidateSource::Near,
                },
                crate::candidates::Candidate {
                    key: "orto".to_owned(),
                    source: crate::candidates::CandidateSource::Far,
                },
            ],
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
        assert!(
            shown.contains("orto — Orto | wiki: alice | via: far |"),
            "a foreign line says which source offered it: {shown}"
        );
        assert!(
            !shown.contains("cucina_bob — Cucina_bob | via:"),
            "the batch's own pages are not a selection, so they carry no source: {shown}"
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
        }];
        // The LLM echoes the page back exactly as the schema asks — no `style`.
        let llm = FakeLlmBackend::new(
            "fake",
            "{\"redirects\":{},\"accepted_new\":[{\"slug\":\"spesa\",\"title\":\"Spesa\",\
             \"description\":\"La lista della spesa\",\
}]}",
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
            "no children signal: nothing lists pages"
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
    fn a_page_that_moved_file_is_dirty_on_its_own() {
        // The fingerprint covers a page's facts and its links, and neither
        // moves when the page does — so the page-path check beside it is the
        // only thing that can catch a move.
        let old_leaf = person("alice");
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
        // A page that MOVED FILE is dirty even with identical content: the
        // fingerprint is the same, the address is not — and what a page IS is
        // its file name, so a move is also a change of kind.
        let mut moved = prev.clone();
        moved.pages.get_mut("alice").unwrap().page_path = "altrove.md".to_owned();
        let dirty = compute_dirty_pages(&prev, &moved);
        assert_eq!(dirty, vec!["alice".to_owned()], "a move alone → dirty");
    }

    fn concept_entry(slug: &str, wiki_id: &str) -> ConceptRegistryEntry {
        ConceptRegistryEntry {
            slug: slug.to_owned(),
            title: capitalize(slug),
            description: "d".to_owned(),
            style: None,
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
        registry
            .entries
            .insert("matteo".to_owned(), concept_entry("matteo", "famiglia"));
        let (plan, reg) = build_compilation_plan(
            &[],
            &foundation,
            &Blueprint::default(),
            &ConciliatorResult::default(),
            &registry,
            "t2",
        );
        assert!(
            plan.pages["matteo"].is_identity_card(),
            "the slug belongs to the foundation page, and a page IS its file name"
        );
        assert!(
            !reg.entries.contains_key("matteo"),
            "the shadowed entry is GC'd — the foundation page wins"
        );
    }

    fn proposal(slug: &str) -> NewPage {
        NewPage {
            slug: slug.to_owned(),
            title: capitalize(slug),
            description: "d".to_owned(),
            style: None,
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
            .insert("spesa".to_owned(), concept_entry("spesa", "alice"));
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
            &[proposal("cinema")],
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
    fn a_redirect_onto_a_card_is_refused_and_never_offered() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        registry
            .entries
            .insert("cucina".to_owned(), concept_entry("cucina", "alice"));

        let kept = vet_redirects(
            BTreeMap::from([
                ("ricette".to_owned(), "alice".to_owned()),
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
            !offered.contains("[person]"),
            "a card is never offered as a merge target: {offered}"
        );
    }

    /// What the Conciliatore hands back is vetted like what the Cartografo
    /// proposes — it re-enters the plan AND the registry.
    #[test]
    fn an_accepted_page_is_vetted_like_a_proposed_one() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));

        assert!(
            vet_accepted(proposal("rules"), &foundation).is_none(),
            "a reserved page name is refused outright"
        );
        assert!(
            vet_accepted(proposal("Profile"), &foundation).is_none(),
            "the check is on the canonical slug, not the raw string"
        );

        assert!(
            vet_accepted(proposal("karate"), &foundation).is_some(),
            "an ordinary slug is kept"
        );
    }

    /// An assignment naming a reserved page never mints a second plan page on
    /// the file the engine already owns.
    ///
    /// The card is keyed by [`plan_slug_for_page`] under the wiki's own slug,
    /// so a bare `profile` misses the lookup and reached the fallback mint,
    /// which would have produced a second page writing `@profile.md` in the
    /// same wiki.
    #[test]
    fn an_assignment_naming_a_reserved_page_mints_nothing() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let facts = vec![fact(1, "alice runs on tuesdays", "user:alice", "alice")];
        let blueprint = Blueprint {
            assignments: vec![Assignment {
                fact_id: facts[0].fact_id.as_str().to_owned(),
                page_slug: "profile".to_owned(),
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
            !plan.pages.contains_key("profile"),
            "no page is minted under a reserved stem: {:?}",
            plan.pages.keys().collect::<Vec<_>>()
        );
        assert!(
            !reg.entries.contains_key("profile"),
            "and none is persisted"
        );
        assert!(
            plan.pages.values().all(|p| p.primary_facts.is_empty()),
            "and the fact is placed nowhere — it waits rather than landing on \
             a page nobody chose"
        );
    }
    /// An emptied page is removed, never promoted into a container.
    ///
    /// Founder's ruling 2026-08-04: a container is a wiki, and wikis are
    /// raised by the visible promote machinery — so a page that runs out of
    /// facts has nothing left to be and goes.
    #[test]
    fn an_emptied_page_is_removed() {
        let mut foundation = BTreeMap::new();
        foundation.insert("alice".to_owned(), person("alice"));
        let mut registry = ConceptRegistry::empty("t");
        registry
            .entries
            .insert("cucina".to_owned(), concept_entry("cucina", "alice"));
        registry.entries.insert(
            "cucina_tecniche".to_owned(),
            concept_entry("cucina_tecniche", "alice"),
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
        assert!(
            plan.pages
                .values()
                .all(|p| !p.outgoing_links.contains(&"cucina".to_owned())),
            "and no surviving page still links to it"
        );
    }
}
