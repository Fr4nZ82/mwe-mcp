// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recall-navigation phase 1 — deterministic entry-point gathering.
//!
//! Recall-as-navigation opens with a fan of **entry-points**: the wikis (and,
//! when a card pins one down, the pages) where a navigator should start
//! reading for the current turn. This module computes that fan
//! deterministically — no LLM call, no embedding — from three seed families.
//! (There were four: a **Principal** family seeded the identity wikis of the
//! people in the turn. It was deleted 2026-08-03 — a seed that can only name
//! a wiki is not a door, and who the turn is about now reaches the block by
//! being *served*.)
//!
//! - **Rag** — the flat-recall hits of the turn, mapped back to the
//!   `(wiki, page)` they live on. RAG opens the obvious doors; it is one of
//!   the seeds, not the engine.
//! - **Topic** — the classified topics of the turn, matched (case-insensitive
//!   substring) against the **cards**: the per-wiki `_meta.keywords` and,
//!   inside a matched wiki, the per-page testata keywords.
//! - **Situational** — free host-supplied strings (location, occasion),
//!   matched exactly like topics. Empty until a host sends them.
//!
//! Two invariants do the heavy lifting:
//!
//! - **Card matching only.** Topic and situational seeds match the compiled
//!   cards, never `fact_index.topics` directly — so the ACL card boundary
//!   (card metadata is built only from default-visibility facts, see
//!   [`crate::meta_annotate`]) also governs what can open a door here. A
//!   restricted fact's topic words cannot act as an entry-point.
//! - **Visibility is derived, never declared.** There is no wiki-level ACL
//!   gate. A wiki is reachable iff the reader can read ≥ 1 fact in it, and that
//!   signal already lives in the reader-relative card: a wiki whose card is
//!   empty seeds nothing for the card-driven and principal families, and the
//!   RAG family is already `can_read`-filtered upstream, so every seed is
//!   reader-visible by construction.
//!
//! Page-card descent happens only inside a wiki whose own card matched: the
//! wiki card's `topics` entry is the union of its pages' entries (both synced
//! by [`crate::meta_annotate`]), so a page can only match where its wiki
//! already does.
//!
//! Duplicates collapse on `(wiki, page)` keeping whichever copy would have
//! sorted first — one comparator ([`fan_order`]) settles the collision and
//! then sorts the survivors, so a door reached by two routes is ranked by its
//! **best** route. A principal seed that lands on a page some content family
//! also found therefore keeps the content ranking: the identity anchor is the
//! weakest claim on a door, never a demotion applied to one.
//!
//! The gatherer's fan feeds the **navigator funnel** ([`navigate`]): a
//! Rust-owned loop where the `navigator` LLM slot reads the root index, the
//! destination cards, and the prose collected so far, and decides which
//! pages to open next — semantics in the prompt, resources in the
//! [`NavigatorPolicy`] knobs. Every page it brings back is **projected
//! per-sender** ([`crate::render::render_for_sender`]) — the navigator never
//! sees a raw marker. Both call sites (the ingest recall-block tail and the
//! `wiki_navigate` tool) run gather → navigate; the funnel also journals its
//! own route ([`NavigationOutcome::trace`]) for the recall-trace surface
//! ([`crate::recall_trace`]). See
//! the recall pipeline.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::acl::FactAclMap;
use crate::enrollment;
use crate::fact_index;
use crate::llm::{CompletionRequest, LlmBackend, LlmError};
use crate::meta_annotate;
use crate::prompts;
use crate::recall::{MULTI_HOP_HARD_LIMIT, RecallHit, SenderContext, extract_wikilinks};
use crate::render::render_for_sender;
use crate::types::Principal;
use crate::wiki::{
    self, DiscoveredWiki, MarkdownDoc, WikiTree, render_root_index, wiki_catalog_list_for,
};

/// Weight of a topic seed that pinned down a **page** card.
pub const WEIGHT_TOPIC_PAGE: f32 = 0.8;
/// Weight of a situational seed that pinned down a **page** card.
pub const WEIGHT_SITUATIONAL_PAGE: f32 = 0.5;

/// Which seed family produced an [`EntryPoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrigin {
    /// A flat-recall hit of the turn, mapped back to its `(wiki, page)`.
    Rag,
    /// A classified topic matched a wiki / page card.
    Topic,
    /// A host-supplied situational string matched a wiki / page card.
    Situational,
}

impl EntryOrigin {
    /// Tiebreak **within one weight** — lower wins. A content hit beats a
    /// topic-card match beats a situational one.
    const fn rank(self) -> u8 {
        match self {
            Self::Rag => 0,
            Self::Topic => 1,
            Self::Situational => 2,
        }
    }
}

/// One place a navigator should start reading.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryPoint {
    /// Target wiki.
    pub wiki_id: String,
    /// The page to read, relative to the wiki directory.
    ///
    /// **Always a content page.** Recall does not "enter a wiki" — it opens
    /// the pages the turn's best facts live on, and finds itself in a wiki as
    /// a consequence (founder, 2026-08-03: *«il recall non entra in una wiki,
    /// il recall entra nelle pagine di contenuto relative ai fatti con score
    /// più alto»*). A seed that can only name a wiki is not a door.
    pub page: PathBuf,
    /// Seed family that produced this entry.
    pub origin: EntryOrigin,
    /// Relative priority within the fan, `0.0..=1.0`. Ordering material for
    /// the funnel's budget — not a probability.
    ///
    /// See [`fan_order`]: the funnel sorts on the family first, so this only
    /// ever breaks ties *within* one family.
    pub weight: f32,
}

/// Per-wiki precomputation shared by every seed family: the walk row and the
/// lowercased reader-relative wiki-card strings. A wiki with an empty `card`
/// has nothing this reader can see — that emptiness, not a wiki-level flag, is
/// the visibility signal.
struct WikiSeedInfo {
    wiki: DiscoveredWiki,
    card: Vec<String>,
}

/// Gather the entry-point fan for one turn.
///
/// Inputs come from work the ingest turn has already done: `sender` carries
/// the resolved group membership, `topics` comes from the classification,
/// `rag_hits` from the flat recall of the turn, `situation` from the host
/// (empty today). The call is deterministic and read-only — safe to run on
/// every turn, with no side effect on recall counters.
///
/// **There is no owner/subject channel any more.** It existed to seed a
/// principal's identity wiki, and a wiki is not a door: whose turn it is, and
/// whom it is about, reach the block by being *served* — deterministically,
/// as a card — not by the navigator being pointed at a person.
///
/// The result is deduplicated on `(wiki, page)` and sorted by weight
/// descending (ties: origin rank, then `wiki_id`, then page), so a funnel can
/// truncate to its budget by taking a prefix.
///
/// # Errors
///
/// Tree-walk / `_meta.md` parse failures surface; per-page card reads degrade
/// to "matches nothing" instead of erroring.
pub async fn gather_entry_points(
    pool: &SqlitePool,
    tree: &WikiTree,
    sender: &SenderContext,
    topics: &[String],
    rag_hits: &[RecallHit],
    situation: &[String],
) -> Result<Vec<EntryPoint>> {
    // Reader-relative card: the topic union the sender can actually read on
    // each wiki, recomputed from `fact_index` per turn so a seed never matches
    // a denied fact's theme (the owner-tier `.md` keywords would leak it).
    let reader_card =
        meta_annotate::build_reader_card(pool, tree, &sender.sender_id, &sender.sender_groups)
            .await
            .context("build reader card")?;
    let infos = build_seed_infos(tree, &reader_card)?;

    let mut candidates: Vec<EntryPoint> = Vec::new();

    // Card-driven seeds (topic + situational), reading the reader-relative
    // page topics straight from the prebuilt card — no per-page `.md` I/O.
    gather_card_seeds(
        &infos,
        &reader_card,
        topics,
        EntryOrigin::Topic,
        WEIGHT_TOPIC_PAGE,
        &mut candidates,
    );
    gather_card_seeds(
        &infos,
        &reader_card,
        situation,
        EntryOrigin::Situational,
        WEIGHT_SITUATIONAL_PAGE,
        &mut candidates,
    );

    // RAG seeds: content-driven. The hits are already `can_read`-filtered
    // upstream, so a hit is by definition readable — no further visibility
    // gate. What a hit cannot do any more is fall back to a wiki-level door:
    // there is none, so a hit that names no readable page seeds nothing (see
    // the per-hit filter below).
    for hit in rag_hits {
        let Some(info) = infos
            .iter()
            .find(|i| i.wiki.meta.wiki_id.as_str() == hit.wiki_id)
        else {
            continue;
        };
        // A door is a page to read. A `fresh` hit has no published page yet,
        // and a hit homed on the channel-only `rules.md` or on the wiki's map
        // names no readable page either — all three surface through the flat
        // slot and seed nothing here.
        if hit.fresh {
            continue;
        }
        let Some(page) = page_within(&info.wiki.rel_dir, &hit.source_path)
            .filter(|p| !is_rules_page_path(p) && !is_root_page_path(p))
        else {
            continue;
        };
        candidates.push(EntryPoint {
            wiki_id: hit.wiki_id.clone(),
            page,
            origin: EntryOrigin::Rag,
            weight: hit.score.clamp(0.0, 1.0),
        });
    }

    Ok(dedup_and_sort(candidates))
}

/// Build the per-wiki seed precomputation: walk the tree and attach each wiki's
/// reader-relative, lowercased card topics. Visibility is **derived** — a wiki
/// whose reader-relative card is empty holds nothing this reader can see.
///
/// Smart wikis are not funnel-navigable: free markdown pushed by the consumer,
/// with no synced testata cards, no `[[wikilink]]` graph, and wiki-level (not
/// per-fragment) ACL. They are surfaced via flat recall instead; the funnel
/// skips them as both seeds and destinations (mirrors the REM cross-wiki refile
/// sweep).
///
/// This is a **settled decision, not an omission**: teaching the funnel to
/// descend was weighed and withdrawn (roadmap 48g, 2026-07-27). A project
/// wiki's retrieval quality stays a property of its sections alone, so nobody
/// has to author link topology to be found; and a graph walk would spend one
/// model call per hop on the per-turn budget. If a hit ever needs its
/// surroundings, the cheap move is its neighbouring sections on the same page,
/// not a walk.
fn build_seed_infos(
    tree: &WikiTree,
    reader_card: &meta_annotate::ReaderCard,
) -> Result<Vec<WikiSeedInfo>> {
    Ok(tree
        .walk()
        .context("walk wiki tree")?
        .into_iter()
        .filter(|wiki| !wiki.meta.smart)
        .map(|wiki| {
            let card = reader_card
                .wiki_topics(wiki.meta.wiki_id.as_str())
                .iter()
                .map(|t| t.to_lowercase())
                .collect();
            WikiSeedInfo { wiki, card }
        })
        .collect())
}

/// Match `queries` against the wiki cards and — inside a matched wiki — the
/// page cards, pushing one seed per match. The cards are reader-relative
/// ([`build_reader_card`](crate::meta_annotate::build_reader_card)), so a wiki
/// the reader can read nothing in carries an empty card and matches no needle —
/// visibility falls out of the match itself, with no extra gate.
fn gather_card_seeds(
    infos: &[WikiSeedInfo],
    reader_card: &meta_annotate::ReaderCard,
    queries: &[String],
    origin: EntryOrigin,
    page_weight: f32,
    out: &mut Vec<EntryPoint>,
) {
    let needles: Vec<String> = queries
        .iter()
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty())
        .collect();
    if needles.is_empty() {
        return;
    }
    for info in infos {
        let wiki_id = info.wiki.meta.wiki_id.as_str();
        for needle in &needles {
            if !info.card.iter().any(|c| c.contains(needle)) {
                continue;
            }
            // The wiki-level match is not a door of its own — it is the gate
            // that lets the descent run. Descend into the wiki's
            // reader-visible page topics; a page whose
            // topics match the needle seeds at page granularity. The topics
            // are the reader-relative set (see `build_reader_card`), so no page
            // the reader cannot read into contributes a seed.
            let Some(pages) = reader_card.pages(wiki_id) else {
                continue;
            };
            for (source_path, topics) in pages {
                if !topics.iter().any(|t| t.to_lowercase().contains(needle)) {
                    continue;
                }
                let Some(rel_path) = page_within(&info.wiki.rel_dir, source_path) else {
                    continue;
                };
                if is_root_page_path(&rel_path) {
                    continue; // the wiki's map, not a page to read
                }
                out.push(EntryPoint {
                    wiki_id: wiki_id.to_owned(),
                    page: rel_path,
                    origin,
                    weight: page_weight,
                });
            }
        }
    }
}

/// True when a wiki-relative page path is the reserved `rules.md` policy
/// page ([`wiki::RULES_FILENAME`]) — channel-only, never navigable
/// (roadmap 41e; the `&str` twin is [`wiki::is_rules_page`]).
fn is_rules_page_path(page: &Path) -> bool {
    page.file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(wiki::RULES_FILENAME))
}

/// Map a `fact_index.source_path` (workdir-relative, POSIX separators) to the
/// page path relative to the wiki rooted at `rel_dir`. `None` when the path
/// does not sit under the wiki directory (a stale index row after a move) —
/// and `None` means **no candidate**, not "fall back to the wiki root": since
/// the map rule the root is not a landing at all.
fn page_within(rel_dir: &Path, source_path: &str) -> Option<PathBuf> {
    let prefix = format!("{}/", rel_dir.to_string_lossy().replace('\\', "/"));
    let rest = source_path.strip_prefix(&prefix)?;
    if rest.is_empty() {
        None
    } else {
        Some(PathBuf::from(rest))
    }
}

/// Total order over the fan — **the same comparator settles a `(wiki, page)`
/// collision and sorts the survivors**, so a door two families found is
/// ranked by its *best* route rather than by whichever seed was emitted
/// first: weight descending, then family, then a deterministic tiebreak.
fn fan_order(a: &EntryPoint, b: &EntryPoint) -> std::cmp::Ordering {
    b.weight
        .total_cmp(&a.weight)
        .then_with(|| a.origin.rank().cmp(&b.origin.rank()))
        .then_with(|| a.wiki_id.cmp(&b.wiki_id))
        .then_with(|| a.page.cmp(&b.page))
}

/// Collapse duplicates on `(wiki, page)` keeping the copy that sorts first
/// under [`fan_order`], then sort the survivors with it.
fn dedup_and_sort(candidates: Vec<EntryPoint>) -> Vec<EntryPoint> {
    let mut best: BTreeMap<(String, PathBuf), EntryPoint> = BTreeMap::new();
    for ep in candidates {
        match best.entry((ep.wiki_id.clone(), ep.page.clone())) {
            Entry::Vacant(slot) => {
                slot.insert(ep);
            },
            Entry::Occupied(mut slot) => {
                if fan_order(&ep, slot.get()).is_lt() {
                    slot.insert(ep);
                }
            },
        }
    }
    let mut out: Vec<EntryPoint> = best.into_values().collect();
    out.sort_by(fan_order);
    out
}

// ---------- The navigator funnel ----------

/// Bundled system prompt for the `navigator` LLM slot
/// (`crates/mwe-core/prompts/navigator.md`); an operator override at
/// `<workdir>/prompts/navigator.md` wins.
pub const BUNDLED_NAVIGATOR_PROMPT_MD: &str = include_str!("../prompts/navigator.md");

/// Bundled prompt for the query-seed extractor (`wiki_navigate` fallback B);
/// an operator override at `<workdir>/prompts/query-seeds.md` wins.
pub const BUNDLED_QUERY_SEEDS_PROMPT_MD: &str = include_str!("../prompts/query-seeds.md");

/// True when a wiki-relative page path is the wiki's map
/// ([`wiki::INDEX_FILENAME`]).
///
/// Founder's ruling, 2026-08-03: *«la radice della wiki dovrebbe servire solo
/// al rem e all'ingest come mappa per dove mettere i fatti e non dovrebbe
/// neanche essere presa per nulla dal recall»*. Same treatment
/// [`wiki::RULES_FILENAME`] gets, for the same reason: a page whose job is
/// not to be read is filtered on the offer side and refused centrally in
/// [`open_target`], so no route can reach it.
fn is_root_page_path(page: &Path) -> bool {
    page.file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(wiki::INDEX_FILENAME))
}

/// Operator knobs for the navigator funnel — **resources only, never
/// semantics**.
///
/// Which links are worth following is the navigator LLM's call, guided by
/// the prompt. Pinned defaults (conservative) until the operator
/// recall-settings panel surfaces them.
#[derive(Debug, Clone)]
pub struct NavigatorPolicy {
    /// Depth dial: maximum navigator decisions (hops) per turn. Clamped to
    /// [`MULTI_HOP_HARD_LIMIT`].
    pub max_hops: usize,
    /// Maximum pages the navigator may open per hop.
    pub pages_per_hop: usize,
    /// Total character budget for the collected, sender-projected prose.
    pub char_budget: usize,
    /// Maximum candidates offered to the navigator per hop.
    pub max_candidates: usize,
    /// How many candidates a hop may be topped up to with **directory
    /// siblings**. **`0` — off** (founder, 2026-08-04: *«io credo sia meglio
    /// toglierle del tutto»*, on learning how they are picked).
    ///
    /// They were the funnel's structural breadth channel: on first entry into
    /// a wiki, every one of its pages was offered. The founder's question was
    /// *how* the survivors are chosen, and the answer retires the channel —
    /// [`wiki::list_wiki_pages`] sorts by path, so the ones that fit were the
    /// **alphabetically first**. That is not a choice, it is an accident of
    /// filenames, and each one it offers costs a summary-and-keywords line in
    /// every hop's prompt. Reachability now rests entirely on the four
    /// content-derived channels — a fact hit, a topic or situational match on
    /// the page's own card, or an authored `[[wikilink]]` — which is the same
    /// argument that makes the page cards load-bearing (card 62, lever 1).
    ///
    /// Kept as a knob rather than deleted with the code: it is one number to
    /// restore if the post-deploy dead-end rate says the corpus is not ready,
    /// and `examples/pool_shape.rs` sets it to [`usize::MAX`] to measure what
    /// the listing *would* have added, for the same reason it leaves
    /// `max_candidates` uncapped. Above `0` it is a floor, not a quota: see
    /// [`prune_pool`].
    pub sibling_floor: usize,
    /// `max_tokens` for each navigator completion (the decision JSON is
    /// small; this is a cost guard, not a quality knob).
    pub decision_max_tokens: u32,
}

impl Default for NavigatorPolicy {
    fn default() -> Self {
        Self {
            max_hops: 2,
            pages_per_hop: 3,
            char_budget: 8_000,
            max_candidates: 16,
            sibling_floor: 0,
            decision_max_tokens: 600,
        }
    }
}

/// One projected page the navigator brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigatedFragment {
    /// Wiki the page lives in.
    pub wiki_id: String,
    /// Page path relative to the wiki directory.
    pub page: PathBuf,
    /// Sender-projected prose ([`render_for_sender`] applied — never raw
    /// markers), truncated to the remaining character budget.
    pub text: String,
}

/// Outcome of [`navigate`]. Possibly partial: an LLM failure mid-funnel
/// degrades to "what was collected so far", never to a dead turn.
#[derive(Debug, Clone, Default)]
pub struct NavigationOutcome {
    /// Pages collected, in opening order.
    pub fragments: Vec<NavigatedFragment>,
    /// Navigator decisions actually spent (LLM calls).
    pub hops: usize,
    /// `true` when the character budget cut material short.
    pub truncated: bool,
    /// The funnel's own journal — one entry per decision, recording what was
    /// offered, what the navigator chose (with its one-line note) and what
    /// actually opened. Always populated (string clones, no extra I/O); the
    /// recall-trace surface persists it.
    pub trace: Vec<HopTrace>,
    /// Why the funnel ended.
    pub stop: NavStop,
}

/// Why a [`navigate`] run ended. `Default` is [`Self::EmptyFan`] — the only
/// way an outcome escapes without entering the hop loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavStop {
    /// The entry-point fan was empty — no completion was spent.
    #[default]
    EmptyFan,
    /// Every remaining candidate was already visited (or vanished).
    PoolExhausted,
    /// The character budget was spent.
    Budget,
    /// The navigator judged the collection sufficient.
    Done,
    /// LLM transport failure or an unparseable decision — partial recall.
    LlmDegraded,
    /// Every pick of the hop was vetted away — another hop would replay it.
    NothingOpened,
    /// The depth dial ran out.
    HopCap,
}

impl NavStop {
    /// Lowercase token (mirrors the serde encoding) for logs and payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyFan => "empty_fan",
            Self::PoolExhausted => "pool_exhausted",
            Self::Budget => "budget",
            Self::Done => "done",
            Self::LlmDegraded => "llm_degraded",
            Self::NothingOpened => "nothing_opened",
            Self::HopCap => "hop_cap",
        }
    }
}

/// Byte cap on the per-page excerpt kept in [`OpenedPage::excerpt`]. The full
/// prose already rides the injected block; the excerpt is what the trace
/// viewer streams onto the page card.
const TRACE_EXCERPT_CAP: usize = 700;

/// One funnel decision as journaled in [`NavigationOutcome::trace`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HopTrace {
    /// The candidate pool exactly as offered (post-prune, pre-decision).
    pub candidates: Vec<CandidateCard>,
    /// Every target the decision asked to open, in order, with whether the
    /// vetting let it through.
    pub requested: Vec<RequestedOpen>,
    /// The decision's `done` flag.
    pub done: bool,
    /// The navigator's own one-line rationale (the decision's `note`).
    pub note: Option<String>,
    /// Pages actually opened this hop, in opening order.
    pub opened: Vec<OpenedPage>,
}

/// One candidate line as offered to the navigator, card included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CandidateCard {
    /// Target wiki.
    pub wiki_id: String,
    /// `None` = the wiki's overview page.
    pub page: Option<String>,
    /// How it surfaced (`principal` | `rag` | `topic` | `situational` |
    /// `link` | `page`).
    pub origin: String,
    /// Reader-relative topic words of the card.
    pub keywords: Vec<String>,
    /// The card's one-line abstract, when the reader may see it.
    pub summary: Option<String>,
}

impl CandidateCard {
    fn from_candidate(c: &Candidate) -> Self {
        Self {
            wiki_id: c.wiki_id.clone(),
            page: Some(c.page.to_string_lossy().into_owned()),
            origin: c.origin.to_owned(),
            keywords: c.keywords.clone(),
            summary: c.summary.clone(),
        }
    }
}

/// One open-this pick of a decision, with the vetting outcome. `opened:
/// false` = discarded (hallucinated target, vanished wiki, already visited,
/// unreadable page, ACL map unloadable).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestedOpen {
    /// Target wiki the decision named.
    pub wiki_id: String,
    /// Target page (`None` = the wiki's overview page).
    pub page: Option<String>,
    /// Whether the vetting let the pick through and the page opened.
    pub opened: bool,
}

/// One page the funnel actually opened, as journaled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenedPage {
    /// Wiki the page lives in.
    pub wiki_id: String,
    /// Page path relative to the wiki directory.
    pub page: String,
    /// Length of the collected, sender-projected prose — same accounting
    /// unit as [`NavigatorPolicy::char_budget`].
    pub chars: usize,
    /// Leading slice of that prose (≤ [`TRACE_EXCERPT_CAP`] bytes, cut on a
    /// char boundary).
    pub excerpt: String,
    /// New candidates this page exposed (sibling pages + wikilink targets).
    pub discovered: usize,
}

/// What [`OpenedPage::page`] carries when the funnel **entered a wiki**
/// rather than reading a page: no prose, no budget, just its table of
/// contents. Not a path — no page by this name can exist
/// ([`wiki::is_safe_page_path`] rejects the parentheses).
const ENTERED_WIKI: &str = "(entered)";

/// Leading slice of `s`, at most `cap` bytes, cut on a char boundary.
fn excerpt_of(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_owned();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// One destination offered to the navigator, with its card.
struct Candidate {
    wiki_id: String,
    /// The page to read — always one. The funnel has no wiki-level door.
    page: PathBuf,
    /// Display label of how it surfaced (`principal`, `rag`, `topic`,
    /// `situational`, `link`, `page`).
    origin: &'static str,
    summary: Option<String>,
    keywords: Vec<String>,
}

/// The tier [`Candidate::prune_tier`] assigns a directory-listing sibling —
/// the demoted tail, and the tier [`prune_pool`] rations.
const SIBLING_TIER: u8 = 2;

impl Candidate {
    /// Ranking tier for [`prune_pool`] — lower sorts first. A wikilink rail
    /// beats the entry-point fan (`principal` | `rag` | `topic` |
    /// `situational`), which beats a directory-listing sibling. Anything
    /// not recognised above falls into the same tier as `page`, so a future
    /// origin added elsewhere without updating this match fails safe into
    /// the demoted tail rather than silently jumping the fan.
    fn prune_tier(&self) -> u8 {
        match self.origin {
            "link" => 0,
            "principal" | "rag" | "topic" | "situational" => 1,
            _ => SIBLING_TIER,
        }
    }
}

impl EntryOrigin {
    /// Lowercase label shown on a candidate line.
    const fn label(self) -> &'static str {
        match self {
            Self::Rag => "rag",
            Self::Topic => "topic",
            Self::Situational => "situational",
        }
    }
}

/// The navigator's decision JSON (one object per hop).
#[derive(Debug, Deserialize)]
struct NavDecision {
    #[serde(default)]
    open: Vec<NavOpen>,
    #[serde(default)]
    done: bool,
    /// The prompt's "one short line on why" — journaled in [`HopTrace`],
    /// never parsed for behaviour.
    #[serde(default)]
    note: Option<String>,
}

/// One open-this target inside [`NavDecision`].
#[derive(Debug, Deserialize)]
struct NavOpen {
    wiki_id: String,
    #[serde(default)]
    page: Option<String>,
}

/// Run the navigator funnel over an entry-point fan.
///
/// The loop is deterministic Rust — hop count, per-hop page cap, character
/// budget, candidate vetting, ACL projection — while each hop's *choice* (which
/// candidates to open, whether to stop) is one completion on the `navigator`
/// LLM slot. Candidates grow as pages are opened: the sibling pages of every
/// wiki entered (their testata cards) and the wikis reachable via `[[wikilinks]]`
/// from the collected prose (their `_meta` cards, `Visible`-only) are offered on
/// the next hop.
///
/// `already_served` names pages whose prose the **caller has already put in
/// front of the consumer by another route**, as `(wiki_id, page)`. They enter
/// the funnel as if it had opened them: never offered as a candidate, never
/// opened, never charged to the budget. Re-reading them would spend the
/// turn's scarcest resource — a page open — on text that is already there.
///
/// The ingest recall block passes the sender's identity card, which
/// `WHO IS SPEAKING` serves deterministically every turn (roadmap 69a), so
/// `index.md` is not a navigation destination for its own owner at all
/// (69b; founder, 2026-08-03: *«non ci frega dell'indice se col rag arriviamo
/// già sulle pagine giuste»* — the recalled facts land on the right pages
/// directly, so the hub's routing is not needed to get there). `wiki_navigate`
/// passes nothing: it builds no block, so it has delivered nothing.
///
/// Degradation contract: an LLM transport failure or an unparseable decision
/// stops the funnel and returns what was collected so far (recall degrades,
/// the turn survives). An empty fan returns an empty outcome without spending
/// a single completion.
///
/// # Errors
///
/// Tree-walk / `_meta.md` parse / prompt-load failures surface (deployment
/// problems, not turn-level noise). LLM-level failures do **not** error — see
/// the degradation contract above.
#[allow(clippy::too_many_lines)] // the funnel loop reads top-to-bottom, splitting hides the flow
pub async fn navigate(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    sender: &SenderContext,
    turn_text: &str,
    entry_points: &[EntryPoint],
    policy: &NavigatorPolicy,
    already_served: &[(String, PathBuf)],
) -> Result<NavigationOutcome> {
    let mut outcome = NavigationOutcome::default();
    if entry_points.is_empty() {
        return Ok(outcome);
    }

    // Reader-relative card for the prompt-facing surfaces (root index +
    // candidate cards): topics the sender can read, abstract gated to the
    // wiki's default visibility — never the owner-tier `.md`.
    let reader_card =
        meta_annotate::build_reader_card(pool, tree, &sender.sender_id, &sender.sender_groups)
            .await
            .context("build reader card")?;

    let wikis = tree.walk().context("walk wiki tree")?;
    // Smart wikis are excluded from the navigable graph (see
    // `gather_entry_points`): no cards / wikilinks / per-fragment ACL to hop
    // through. They never appear as a candidate, sibling, or link target.
    let by_id: BTreeMap<&str, &DiscoveredWiki> = wikis
        .iter()
        .filter(|d| !d.meta.smart)
        .map(|d| (d.meta.wiki_id.as_str(), d))
        .collect();
    let root_index = render_root_index(&wiki_catalog_list_for(
        tree,
        reader_card.readable_wikis(),
        reader_card.wiki_topics_map(),
        reader_card.summary_wikis(),
    )?);
    let system = prompts::render(
        "navigator",
        tree.workdir(),
        BUNDLED_NAVIGATOR_PROMPT_MD,
        &[("page_budget", policy.pages_per_hop.to_string().as_str())],
    )
    .context("load navigator prompt")?;

    let max_hops = policy.max_hops.min(MULTI_HOP_HARD_LIMIT);
    let mut candidates = initial_pool(entry_points, &by_id, &reader_card);
    let mut state = FunnelState {
        // Pages the caller already delivered start out **visited**: that one
        // set is what `prune_pool` filters the offer by and what `open_target`
        // refuses on, so a single line makes the guarantee hold on every route
        // into the funnel — the fan, a directory sibling, a `[[wikilink]]` —
        // instead of three filters that have to agree.
        visited: already_served.iter().cloned().collect(),
        entered: BTreeSet::new(),
        acl_defaults: BTreeMap::new(),
        remaining: policy.char_budget,
        sibling_floor: policy.sibling_floor,
    };

    // Overwritten by every earlier exit; reaching the loop's natural end
    // means the depth dial ran out.
    outcome.stop = NavStop::HopCap;
    for _ in 0..max_hops {
        prune_pool(
            &mut candidates,
            &state.visited,
            policy.max_candidates,
            policy.sibling_floor,
        );
        if candidates.is_empty() || state.remaining == 0 {
            outcome.stop = if state.remaining == 0 {
                NavStop::Budget
            } else {
                NavStop::PoolExhausted
            };
            break;
        }

        let user = build_user_prompt(
            turn_text,
            sender,
            &root_index,
            &candidates,
            &outcome.fragments,
            outcome.hops,
            max_hops,
            state.remaining,
        );
        let mut hop = HopTrace {
            candidates: candidates
                .iter()
                .map(CandidateCard::from_candidate)
                .collect(),
            ..HopTrace::default()
        };
        outcome.hops += 1;
        let Some(decision) = request_decision(llm, &system, user, policy).await else {
            outcome.trace.push(hop);
            outcome.stop = NavStop::LlmDegraded;
            break; // degraded turn — keep what was collected
        };
        hop.done = decision.done;
        hop.note = decision.note.clone();
        if decision.done && decision.open.is_empty() {
            outcome.trace.push(hop);
            outcome.stop = NavStop::Done;
            break;
        }

        let mut opened_this_hop = 0usize;
        let mut discoveries: Vec<Candidate> = Vec::new();
        for target in decision.open.iter().take(policy.pages_per_hop) {
            let fragments_before = outcome.fragments.len();
            if let Some(mut found) = open_target(
                pool,
                tree,
                sender,
                &by_id,
                &candidates,
                target,
                &mut state,
                &mut outcome,
                &reader_card,
            )
            .await?
            {
                // A page open pushes exactly one fragment; entering a wiki
                // pushes none and yields its table of contents instead —
                // journal whichever happened, so the operator record never
                // attributes an enter to the previous page.
                if outcome.fragments.len() > fragments_before {
                    let frag = &outcome.fragments[fragments_before];
                    hop.opened.push(OpenedPage {
                        wiki_id: frag.wiki_id.clone(),
                        page: frag.page.to_string_lossy().into_owned(),
                        chars: frag.text.len(),
                        excerpt: excerpt_of(&frag.text, TRACE_EXCERPT_CAP),
                        discovered: found.len(),
                    });
                } else {
                    hop.opened.push(OpenedPage {
                        wiki_id: target.wiki_id.clone(),
                        page: ENTERED_WIKI.to_owned(),
                        chars: 0,
                        excerpt: String::new(),
                        discovered: found.len(),
                    });
                }
                hop.requested.push(RequestedOpen {
                    wiki_id: target.wiki_id.clone(),
                    page: target.page.clone(),
                    opened: true,
                });
                discoveries.append(&mut found);
                opened_this_hop += 1;
            } else {
                hop.requested.push(RequestedOpen {
                    wiki_id: target.wiki_id.clone(),
                    page: target.page.clone(),
                    opened: false,
                });
            }
            if state.remaining == 0 {
                break;
            }
        }
        outcome.trace.push(hop);
        if opened_this_hop == 0 {
            // Every choice was vetted away (or re-opened) — another hop
            // would replay the same decision.
            outcome.stop = NavStop::NothingOpened;
            break;
        }
        // Fresh context first: what the navigator just entered outranks the
        // unopened tail of the original fan.
        discoveries.append(&mut candidates);
        candidates = discoveries;
    }

    tracing::info!(
        sender_id = sender.sender_id,
        fragments = outcome.fragments.len(),
        hops = outcome.hops,
        truncated = outcome.truncated,
        stop = outcome.stop.as_str(),
        "recall_nav: navigation done"
    );
    Ok(outcome)
}

/// JSON shape returned by the query-seed extractor.
#[derive(Debug, Default, Deserialize)]
struct QuerySeedsJson {
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
}

/// Extract topic + owner seeds from a free-text query via the `navigator` slot.
///
/// The `wiki_navigate` fallback **B** (roadmap 24b: the caller's explicit
/// `topics`/`owners` first (C), then this, then principal+RAG only (A)). Ingest
/// gets these seeds from its classifier; a standalone search has no classifier
/// in the loop, so this is a small dedicated extraction (not the heavy ingest
/// classifier).
///
/// Best-effort by contract: any LLM or parse failure returns `(empty, empty)`
/// and the caller degrades to A. Extracted entity names are resolved against
/// enrollment (user id / alias → `user:`, group id → `group:`) **and are kept
/// as topics either way** — resolving a name says who the turn is about, it
/// does not make the word less useful for matching the cards of the pages that
/// mention them.
pub async fn extract_query_seeds(
    pool: &SqlitePool,
    workdir: &Path,
    llm: &dyn LlmBackend,
    query: &str,
) -> (Vec<String>, Vec<Principal>) {
    let system = match prompts::render("query-seeds", workdir, BUNDLED_QUERY_SEEDS_PROMPT_MD, &[]) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "recall_nav: query-seeds prompt load failed, no extracted seeds");
            return (Vec::new(), Vec::new());
        },
    };
    let request = CompletionRequest::new(query.to_owned())
        .with_system(system)
        .with_temperature(0.1)
        .with_max_tokens(300);
    let resp = match llm.complete(request).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(error = %err, "recall_nav: query-seeds LLM failed, no extracted seeds");
            return (Vec::new(), Vec::new());
        },
    };
    let Some(parsed) = parse_query_seeds(&resp.text) else {
        tracing::warn!(
            preview = %resp.text.chars().take(120).collect::<String>(),
            "recall_nav: unparseable query-seeds, no extracted seeds"
        );
        return (Vec::new(), Vec::new());
    };

    let mut topics = parsed.topics;
    let mut owners: Vec<Principal> = Vec::new();
    if !parsed.entities.is_empty() {
        let users = match enrollment::list_users(pool).await {
            Ok(users) => users,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "recall_nav: enrollment user list failed, extracted entities fold into topics"
                );
                Vec::new()
            },
        };
        let groups = match enrollment::list_groups(pool).await {
            Ok(groups) => groups,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "recall_nav: enrollment group list failed, extracted entities fold into topics"
                );
                Vec::new()
            },
        };
        fold_entities(parsed.entities, &users, &groups, &mut topics, &mut owners);
    }
    (topics, owners)
}

/// Route each extracted entity name into the two seed channels.
///
/// An entity is recorded on `owners` when the roster resolves it, and is kept
/// as a `topics` needle **either way**.
///
/// That "either way" is the fix, and the shape it replaced is worth naming:
/// it was an `if let … else`, so a resolved name went ONLY to `owners`. Since
/// 69b `owners` seeds no door — a principal names a wiki, and recall opens
/// content pages — so a name the roster *recognised* went to a dead channel
/// while an unrecognised one went to the live card matcher. A query about an
/// enrolled person was served strictly worse than one about a stranger.
/// Resolving a name says who the turn is about; it does not make the word less
/// useful for finding the pages that mention them.
fn fold_entities(
    entities: Vec<String>,
    users: &[enrollment::EnrolledUserLite],
    groups: &[enrollment::EnrolledGroupLite],
    topics: &mut Vec<String>,
    owners: &mut Vec<Principal>,
) {
    for entity in entities {
        if let Some(p) = resolve_entity(&entity, users, groups)
            && !owners.contains(&p)
        {
            owners.push(p);
        }
        if !topics.iter().any(|t| t.eq_ignore_ascii_case(&entity)) {
            topics.push(entity);
        }
    }
}

/// Resolve one entity name (case-insensitive) to a principal via enrollment:
/// a user id or one of its aliases → `Principal::User`, a group id →
/// `Principal::Group`. `None` when nothing matches.
fn resolve_entity(
    name: &str,
    users: &[enrollment::EnrolledUserLite],
    groups: &[enrollment::EnrolledGroupLite],
) -> Option<Principal> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    if let Some(u) = users.iter().find(|u| {
        u.user_id.eq_ignore_ascii_case(n) || u.aliases.iter().any(|a| a.eq_ignore_ascii_case(n))
    }) {
        return Some(Principal::User(u.user_id.clone()));
    }
    groups
        .iter()
        .find(|g| g.group_id.eq_ignore_ascii_case(n))
        .map(|g| Principal::Group(g.group_id.clone()))
}

/// Tolerant parse of the extractor's JSON (strips a leading/trailing code
/// fence by slicing the first `{` to the last `}`).
fn parse_query_seeds(raw: &str) -> Option<QuerySeedsJson> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&raw[start..=end]).ok()
}

/// Mutable funnel bookkeeping threaded through the hops: pages already
/// opened (resolved paths), wikis whose siblings were already offered,
/// the per-wiki resolved `acl_default` cache, and the character budget
/// still spendable.
struct FunnelState {
    visited: BTreeSet<(String, PathBuf)>,
    entered: BTreeSet<String>,
    acl_defaults: BTreeMap<String, Principal>,
    remaining: usize,
    /// Mirror of [`NavigatorPolicy::sibling_floor`]. `0` (the default) means
    /// the directory listing is not produced at all — not produced and then
    /// discarded: building it walks the wiki and reads every page's testata,
    /// which is real work for candidates that would be dropped.
    sibling_floor: usize,
}

/// Vet one navigator pick against the candidate pool and — when it holds —
/// open the page, project it for the sender, charge the budget, and push the
/// fragment. Returns `Some(discoveries)` (the new candidates the opened page
/// exposes) when a page was actually opened, `None` when the pick was
/// discarded (hallucinated target, vanished wiki, already visited,
/// unreadable page, ACL map unloadable).
///
/// # Errors
///
/// Only `acl_default` resolution surfaces — a broken `_meta` chain is a
/// deployment problem, not turn-level noise.
#[allow(clippy::too_many_arguments, reason = "one funnel-step's full context")]
async fn open_target(
    pool: &SqlitePool,
    tree: &WikiTree,
    sender: &SenderContext,
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    candidates: &[Candidate],
    target: &NavOpen,
    state: &mut FunnelState,
    outcome: &mut NavigationOutcome,
    reader_card: &meta_annotate::ReaderCard,
) -> Result<Option<Vec<Candidate>>> {
    // Anti-hallucination vetting: the target must match an offered candidate
    // verbatim — the navigator picks doors, it does not mint them.
    // A request that names no page names a wiki, and a wiki is not a door
    // (founder, 2026-08-03) — it matches no candidate and is discarded here
    // with everything else the navigator may have invented.
    let target_page = target.page.as_ref().map(PathBuf::from);
    let Some(cand) = candidates
        .iter()
        .find(|c| c.wiki_id == target.wiki_id && Some(&c.page) == target_page.as_ref())
    else {
        tracing::debug!(
            wiki_id = %target.wiki_id,
            page = ?target.page,
            "recall_nav: navigator chose a non-candidate, discarded"
        );
        return Ok(None);
    };
    let Some(d) = by_id.get(cand.wiki_id.as_str()) else {
        return Ok(None);
    };
    let page = cand.page.clone();
    // The wiki root is never opened, whatever door the funnel found — the
    // offer-side filters keep it out of the pool, this is the central gate.
    if is_root_page_path(&page) {
        tracing::debug!(
            wiki_id = %cand.wiki_id,
            "recall_nav: the wiki root is a map for REM and ingest, not a recall page — discarded"
        );
        return Ok(None);
    }
    // The reserved `rules.md` policy page is not navigable (roadmap 41e):
    // standing directives reach the consumer through the dedicated `rules`
    // field only, and the page's seeded boilerplate is noise as recalled
    // prose. Central fail-safe — the offer-side filters keep the fan clean,
    // this gate guarantees the invariant whatever door the funnel found.
    if is_rules_page_path(&page) {
        tracing::debug!(
            wiki_id = %cand.wiki_id,
            page = %page.display(),
            "recall_nav: rules page is channel-only, not navigable — discarded"
        );
        return Ok(None);
    }
    if !state.visited.insert((cand.wiki_id.clone(), page.clone())) {
        return Ok(None);
    }
    let default = match state.acl_defaults.entry(cand.wiki_id.clone()) {
        Entry::Occupied(e) => e.get().clone(),
        Entry::Vacant(slot) => slot
            .insert(
                tree.resolve_scope_principal(&d.meta)
                    .with_context(|| format!("resolve scope principal of {}", cand.wiki_id))?,
            )
            .clone(),
    };
    // Authoritative per-fact ACL for the page, keyed by fact id
    // (redaction-policy: DB first, inline attributes as fallback). A
    // page whose map cannot load is skipped, not rendered on weaker
    // gating — same soft-fail class as an unreadable file. `_active`:
    // regions whose fact was superseded/deleted but whose bytes still sit
    // on the page must NOT be surfaced by navigation — they drop from the
    // map and redact fail-closed.
    let source_path = wiki::workdir_relative_source_path(tree.workdir(), &d.abs_dir.join(&page));
    let db_acl = match fact_index::page_acl_map_active(pool, &source_path).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                wiki_id = d.meta.wiki_id.as_str(),
                page = %page.display(),
                error = %err,
                "recall_nav: page ACL map unloadable, page skipped"
            );
            return Ok(None);
        },
    };
    let Some(projected) = open_projected(d, &page, &db_acl, &default, sender) else {
        return Ok(None);
    };
    let (text, cut) = take_budget(projected, state.remaining);
    state.remaining -= text.len();
    outcome.truncated |= cut;
    let mut discoveries = Vec::new();
    // First entry into a wiki used to dump its whole directory here. It is now
    // off by default (`sibling_floor = 0`) — see [`prune_pool`] and
    // [`sibling_page_candidates`].
    if state.sibling_floor > 0 && state.entered.insert(cand.wiki_id.clone()) {
        discoveries.extend(sibling_page_candidates(d, &state.visited, reader_card));
    }
    discoveries.extend(linked_wiki_candidates(&text, d, by_id, reader_card));
    outcome.fragments.push(NavigatedFragment {
        wiki_id: cand.wiki_id.clone(),
        page,
        text,
    });
    Ok(Some(discoveries))
}

/// Turn the entry-point fan into the hop-0 candidate pool, each entry
/// enriched with its wiki card. A fan entry whose wiki vanished from the
/// tree (raced rename) is silently dropped.
fn initial_pool(
    entry_points: &[EntryPoint],
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    reader_card: &meta_annotate::ReaderCard,
) -> Vec<Candidate> {
    entry_points
        .iter()
        .filter_map(|ep| {
            by_id.get(ep.wiki_id.as_str()).map(|d| {
                let (summary, keywords) = reader_wiki_card(d, reader_card);
                Candidate {
                    wiki_id: ep.wiki_id.clone(),
                    page: ep.page.clone(),
                    origin: ep.origin.label(),
                    summary,
                    keywords,
                }
            })
        })
        .collect()
}

/// The reader-relative wiki-level card shown for a candidate: the abstract
/// gated to readers at the wiki's default visibility, and the reader-visible
/// topic union — never the owner-tier `.md` card a denied reader must not see.
fn reader_wiki_card(
    d: &DiscoveredWiki,
    reader_card: &meta_annotate::ReaderCard,
) -> (Option<String>, Vec<String>) {
    let wiki_id = d.meta.wiki_id.as_str();
    let summary = reader_card
        .summary_visible(wiki_id)
        .then(|| wiki_summary(d))
        .flatten();
    (summary, reader_card.wiki_topics(wiki_id).to_vec())
}

/// One navigator completion + parse, with a single retry on the failures
/// that are worth retrying. `None` on a hard failure or an unparseable
/// decision — the funnel's degradation contract (log, keep the partial
/// recall, never kill the turn).
///
/// The retry exists because the degradation is invisible where it lands:
/// a caller gets an answer built from a partial walk and cannot tell it
/// apart from a complete one. In the 2026-07-29 corpus rebuild 2 of 276
/// calls came back with no `text` block at all — the model spent its
/// budget on a thinking block — and each silently cost that turn its
/// navigation. One more attempt is cheap next to an answer that is
/// quietly worse.
///
/// Only [`LlmError::Protocol`], [`LlmError::Transport`] and
/// [`LlmError::Backend`] are retried. An `Invalid` (a 400: bad params,
/// unknown model, prompt too long) reproduces exactly on a second
/// identical request, and `Auth` / `RateLimit` want the operator or a
/// back-off window rather than an immediate retry.
async fn request_decision(
    llm: &dyn LlmBackend,
    system: &str,
    user: String,
    policy: &NavigatorPolicy,
) -> Option<NavDecision> {
    let build = || {
        CompletionRequest::new(user.clone())
            .with_system(system.to_owned())
            .with_temperature(0.1)
            .with_max_tokens(policy.decision_max_tokens)
    };
    let resp = match llm.complete(build()).await {
        Ok(resp) => resp,
        Err(err) if navigator_retriable(&err) => {
            tracing::warn!(error = %err, "recall_nav: navigator LLM failed, retrying once");
            match llm.complete(build()).await {
                Ok(resp) => resp,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "recall_nav: navigator LLM failed after retry, partial recall"
                    );
                    return None;
                },
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "recall_nav: navigator LLM failed, partial recall");
            return None;
        },
    };
    let decision = parse_decision(&resp.text);
    if decision.is_none() {
        tracing::warn!(
            preview = %resp.text.chars().take(120).collect::<String>(),
            "recall_nav: unparseable navigator decision, partial recall"
        );
    }
    decision
}

/// Whether a navigator failure is worth one more identical attempt.
/// See [`request_decision`] for why the other variants are not.
const fn navigator_retriable(err: &LlmError) -> bool {
    matches!(
        err,
        LlmError::Protocol(_) | LlmError::Transport(_) | LlmError::Backend(_)
    )
}

/// The wiki's one-line abstract from `_meta.extra["summary"]` (the same key
/// the catalog surfaces).
fn wiki_summary(d: &DiscoveredWiki) -> Option<String> {
    wiki::meta_summary(&d.meta)
}

/// Drop visited / duplicate candidates, stably rank the survivors by tier,
/// then cap the pool for the next prompt.
///
/// The pool mixes two producers of very different value. `linked_wiki_candidates`
/// offers wikilink destinations found in the prose just read — an authored
/// assertion that two pages belong together, the design's only expansion
/// mechanism. `sibling_page_candidates` offers **every page of a wiki's
/// directory**, filesystem order, the moment the funnel first enters it —
/// a crutch for an unevenly linked corpus, not a rail. Truncating this pool
/// positionally lets whichever producer happened to run last, or a big
/// alphabetically-sorted directory, crowd out the other: measured on the
/// live corpus, one turn offered all 16 candidates from a single wiki's
/// listing while another entered wiki's 20 pages were never offered, and
/// `famiglia` (22 pages, cap 16) always lost the same three content pages.
///
/// So before truncating, [`Candidate::prune_tier`] partitions the pool
/// stably into: wikilink rails first; the entry-point fan (`principal` |
/// `rag` | `topic` | `situational`) next, **in the order the gatherer
/// already weighed them** — untouched here, because from hop 1 on these are
/// seeds the navigator was already offered and did not choose, whereas a
/// freshly discovered link is a rail straight out of the page it just read;
/// siblings last, a demoted tail kept only because a page nobody links
/// would otherwise be reachable solely by direct RAG seeding — it must
/// never displace a rail or a seed. At hop 0 there are no links yet, so
/// this ordering only bites from hop 1 on.
///
/// **The ranking runs before the dedup, and that order is the point.** One
/// page routinely reaches the pool by more than one route at once — a page
/// linked from the prose just read is, whenever it lives in the wiki the
/// funnel just entered, *also* one of that directory's siblings — and the two
/// copies are the same destination at two very different tiers. Deduplicating
/// first keeps whichever copy the producers happened to emit first, which is
/// the sibling ([`open_target`] lists the directory before it reads the
/// links), so the surviving copy carries the demoted tier and is cut with the
/// filesystem tail. Ranking first makes the survivor the *best* route instead
/// of the earliest one — the same rule [`dedup_and_sort`] already applies to
/// the fan, where the heaviest seed wins a collision.
///
/// Measured on the live corpus over 141 real turns (card 66): the cap bites on
/// **49 % of hops**, and ranking first moves **1 628 of 10 955 offered
/// candidates (14.9 %) to a better tier** — 1 442 of them from sibling to rail.
/// It is what lost the traced turn its answer page: `famiglia/concerti.md`,
/// linked from the page just read, was offered at #18 of a 54-candidate pool as
/// a filesystem sibling and cut; it now leads at #12 and survives.
///
/// **`sibling_floor` — the directory listing is a last resort, not an offer**
/// (founder, 2026-08-04: *«le pagine vicine entrano solo se non c'è altro,
/// come ultima risorsa… non è importante vedere le pagine vicine quanto
/// seguire i links»*). Tiering alone did not deliver that: a sibling sorted
/// last still fills every slot the rails and the fan leave free, and the
/// directory listing is by far the funnel's largest producer — entering
/// `carol` alone contributes 47 candidates, and **98.2 % of everything the
/// cap cuts is siblings**. So they are now rationed rather than merely
/// demoted: they enter only to bring the offer up to `sibling_floor`
/// candidates, and not at all above it. The floor is [`NavigatorPolicy`]'s
/// `pages_per_hop` — below the number of pages the navigator may open, the
/// choice is not a choice — which also keeps the dead-end continuation alive:
/// a page with no rails in a wiki with no other route still has somewhere to
/// go. This is card 66's 66b, answered by ruling rather than by measurement.
fn prune_pool(
    pool: &mut Vec<Candidate>,
    visited: &BTreeSet<(String, PathBuf)>,
    cap: usize,
    sibling_floor: usize,
) {
    // Stable, so within a tier the producers' order still stands.
    pool.sort_by_key(Candidate::prune_tier);
    let mut seen: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    pool.retain(|c| {
        let key = (c.wiki_id.clone(), c.page.clone());
        !visited.contains(&key) && seen.insert(key)
    });
    // Siblings are the last resort, not a routine offer. `prune_tier` sorts
    // them last, which kept them out of the top of the offer but still let
    // them fill every leftover slot: a hop with 5 rails and a cap of 16 still
    // showed the navigator 11 directory entries. They now enter only to bring
    // the offer up to `sibling_floor`.
    let rails_and_fan = pool
        .iter()
        .filter(|c| c.prune_tier() < SIBLING_TIER)
        .count();
    let sibling_room = sibling_floor.saturating_sub(rails_and_fan);
    let mut siblings_kept = 0usize;
    pool.retain(|c| {
        if c.prune_tier() < SIBLING_TIER {
            return true;
        }
        siblings_kept += 1;
        siblings_kept <= sibling_room
    });
    pool.truncate(cap);
}

/// Read + project one page for the sender. `None` (logged) when the page
/// cannot be read — a vetted candidate whose file vanished mid-turn is a
/// race, not a failure worth killing recall for.
fn open_projected(
    d: &DiscoveredWiki,
    page: &Path,
    db_acl: &FactAclMap,
    acl_default: &Principal,
    sender: &SenderContext,
) -> Option<String> {
    let raw = match std::fs::read_to_string(d.abs_dir.join(page)) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::debug!(
                wiki_id = d.meta.wiki_id.as_str(),
                page = %page.display(),
                error = %err,
                "recall_nav: page unreadable, skipped"
            );
            return None;
        },
    };
    // The testata is card metadata, not prose — drop it when present.
    let body = MarkdownDoc::parse(&raw).map_or_else(|| raw.clone(), |doc| doc.body);
    Some(
        render_for_sender(
            &body,
            db_acl,
            acl_default,
            &sender.sender_id,
            &sender.sender_groups,
        )
        .text,
    )
}

/// Truncate `text` to `budget` characters (on a char boundary). Returns the
/// kept text and whether a cut happened.
fn take_budget(text: String, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text, false);
    }
    let cut = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= budget)
        .last()
        .unwrap_or(0);
    (text[..cut].to_owned(), true)
}

/// Sibling pages of a freshly entered wiki, offered with their
/// **reader-relative** testata cards: every page is still offered (navigation
/// breadth is structural, not a leak), but its keywords are the reader-visible
/// page topics and its description is shown only at the wiki's default
/// visibility — never the owner-tier testata.
fn sibling_page_candidates(
    d: &DiscoveredWiki,
    visited: &BTreeSet<(String, PathBuf)>,
    reader_card: &meta_annotate::ReaderCard,
) -> Vec<Candidate> {
    let Ok(pages) = wiki::list_wiki_pages(&d.abs_dir) else {
        return Vec::new();
    };
    let wiki_id = d.meta.wiki_id.as_str();
    let show_description = reader_card.summary_visible(wiki_id);
    // Reader-visible page topics keyed by the page path relative to the wiki.
    let topics_by_page: BTreeMap<PathBuf, &Vec<String>> = reader_card
        .pages(wiki_id)
        .map(|pages| {
            pages
                .iter()
                .filter_map(|(source_path, topics)| {
                    Some((page_within(&d.rel_dir, source_path)?, topics))
                })
                .collect()
        })
        .unwrap_or_default();
    pages
        .into_iter()
        // Neither the reserved policy page nor the wiki root is a navigation
        // door (see `open_target`'s fail-safe gates).
        .filter(|p| !is_rules_page_path(&p.rel_path) && !is_root_page_path(&p.rel_path))
        .filter(|p| !visited.contains(&(wiki_id.to_owned(), p.rel_path.clone())))
        .map(|p| {
            // Read the owner-tier testata only for the (gated) description; the
            // keywords come from the reader-relative topic set instead.
            let summary = show_description
                .then(|| {
                    meta_annotate::read_page_card(&p.abs_path)
                        .unwrap_or_default()
                        .description
                })
                .flatten();
            let keywords = topics_by_page
                .get(&p.rel_path)
                .map_or_else(Vec::new, |t| (*t).clone());
            Candidate {
                wiki_id: wiki_id.to_owned(),
                page: p.rel_path,
                origin: "page",
                summary,
                keywords,
            }
        })
        .collect()
}

/// The page a bare `[[wiki_id]]` rail resolves to: the wiki's **foundation
/// page**, never its map.
///
/// Two reserved names can hold one, and which of them a wiki has is a
/// property of the wiki, not of the link — a person or a group has a card
/// ([`wiki::PROFILE_FILENAME`]), a theme wiki has only its buffer
/// ([`wiki::NOTES_FILENAME`]). Decided on disk rather than from the
/// compilation plan because the funnel has no plan: the file is the fact.
/// `None` when the wiki has neither, which is a wiki with nothing authored
/// yet — the rail is then dropped.
fn foundation_slug(d: &DiscoveredWiki) -> Option<String> {
    for name in [wiki::PROFILE_FILENAME, wiki::NOTES_FILENAME] {
        if d.abs_dir.join(name).is_file() {
            return Some(name.trim_end_matches(".md").to_owned());
        }
    }
    None
}

/// Destinations reachable via `[[wikilinks]]` from freshly collected prose,
/// following the link grammar
/// (recall-pipeline.md §Link grammar).
/// A `[[wiki_id/page-slug]]` page hop offers that **page** directly (its
/// testata card, reader-relative), so the navigator opens it in one hop. A
/// bare `[[wiki_id]]` offers that wiki's **foundation page** — never its map,
/// which is written for filing and refused by every route of the read path —
/// and nothing at all when the wiki has neither reserved page (see
/// [`foundation_slug`]). A `|display` alias never reaches this point
/// ([`extract_wikilinks`] strips it).
///
/// **Legacy fallback** (emit canonical, resolve legacy — the marker
/// grammar's stance): a bare target that names no wiki is retried as a
/// page slug of `origin`, the wiki the prose came from. The pre-canonical
/// corpus links same-wiki pages that way (`[[cucina]]` on an `morgana`
/// page), and page prose is copied verbatim across compiles, so those
/// rails never self-canonicalize. A wiki id always wins over a same-named
/// page.
///
/// Visibility is **derived**: a linked destination is offered only when the
/// reader can read ≥ 1 fact in its wiki
/// ([`reader_can_read_in`](meta_annotate::ReaderCard::reader_can_read_in),
/// topic-less facts included). A page hop whose target file does not exist
/// (a dead rail) is silently dropped.
fn linked_wiki_candidates(
    text: &str,
    origin: &DiscoveredWiki,
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    reader_card: &meta_annotate::ReaderCard,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for link in extract_wikilinks(text) {
        let Some(d) = by_id.get(link.wiki_id.as_str()) else {
            // Legacy bare-slug fallback: `[[slug]]` naming no wiki → the
            // same-named page, resolved over the whole tree in the
            // deterministic order of [`resolve_bare_slug_wiki`] (the
            // legacy corpus links pages by bare name across wiki lines).
            // The resolved destination is then reader-gated like any
            // other candidate.
            if link.page.is_none() {
                let rel = PathBuf::from(format!("{}.md", link.wiki_id));
                if wiki::is_safe_page_path(&rel)
                    && !is_rules_page_path(&rel)
                    && !is_root_page_path(&rel)
                    && let Some(target) = resolve_bare_slug_wiki(origin, by_id, &rel)
                    && reader_card.reader_can_read_in(target.meta.wiki_id.as_str())
                {
                    let (summary, keywords) = reader_page_card(target, &rel, reader_card);
                    out.push(Candidate {
                        wiki_id: target.meta.wiki_id.as_str().to_owned(),
                        page: rel,
                        origin: "link",
                        summary,
                        keywords,
                    });
                }
            }
            continue;
        };
        if !reader_card.reader_can_read_in(link.wiki_id.as_str()) {
            continue;
        }
        // `[[wiki]]` names a wiki, and recall opens pages, not wikis. It used
        // to be dropped outright, because the wiki's address resolves to its
        // `index.md` and that is the map — REM's artefact, refused by
        // `open_target`. But dropping it deletes the commonest rail the corpus
        // holds: `[[franz]]`, `[[carol]]`, `[[bob]]` are 20 % of every
        // link written on a content page, and after 63 §8 split the root they
        // all pointed at a page nobody may read. What the prose means by
        // `[[franz]]` is *the person*, and since that split the person is
        // `profile.md`, so the bare form resolves to the wiki's **foundation
        // page** instead — the card if it has one, else the buffer. The map
        // keeps only outgoing links (founder, 2026-08-04), and no rail is lost
        // to a rename we performed ourselves.
        let page_slug = link.page.clone().or_else(|| foundation_slug(d));
        if let Some(slug) = page_slug {
            let rel = PathBuf::from(format!("{slug}.md"));
            // Vet the page half: safe path + the file actually exists
            // (a mutant / stale link is a dead rail, not a candidate) +
            // never the channel-only rules page. Existence is checked
            // Obsidian-style — byte-exact first, else the unique
            // case-insensitive match — so a link whose case drifted
            // from the filename resolves the same way it does on the
            // consumer's local mirror instead of dying silently.
            if !wiki::is_safe_page_path(&rel) {
                continue;
            }
            let Some(resolved) = wiki::resolve_page_case_insensitive(&d.abs_dir, &rel) else {
                continue;
            };
            if is_rules_page_path(&resolved) {
                continue;
            }
            // A rail onto the wiki's map (`[[wiki/index]]`) names the
            // wiki, not a page to read — not a door either.
            if is_root_page_path(&resolved) {
                continue;
            }
            let (summary, keywords) = reader_page_card(d, &resolved, reader_card);
            out.push(Candidate {
                wiki_id: link.wiki_id,
                page: resolved,
                origin: "link",
                summary,
                keywords,
            });
        }
    }
    out
}

/// The wiki whose root carries the page `rel`, picked in the deterministic
/// resolution order of a legacy bare slug relative to `origin`: origin
/// itself, its ancestors nearest-first, its sub-wikis nearest-first, then
/// the remaining wikis in id order. The pick is reader-independent — a
/// link resolves to the same destination for every reader (the caller
/// applies the reader gate to the resolved destination, the same posture
/// as the dashboard resolver).
fn resolve_bare_slug_wiki<'a>(
    origin: &DiscoveredWiki,
    by_id: &BTreeMap<&str, &'a DiscoveredWiki>,
    rel: &Path,
) -> Option<&'a DiscoveredWiki> {
    let mut ranked: Vec<(usize, usize, &str, &'a DiscoveredWiki)> = by_id
        .values()
        .map(|d| {
            let depth = d.abs_dir.components().count();
            let (tier, key) = if d.abs_dir == origin.abs_dir {
                (0, 0)
            } else if origin.abs_dir.starts_with(&d.abs_dir) {
                // Ancestor: nearest (deepest) first.
                (1, usize::MAX - depth)
            } else if d.abs_dir.starts_with(&origin.abs_dir) {
                // Descendant sub-wiki: nearest (shallowest) first.
                (2, depth)
            } else {
                (3, 0)
            };
            (tier, key, d.meta.wiki_id.as_str(), *d)
        })
        .collect();
    ranked.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    ranked
        .into_iter()
        .map(|(_, _, _, d)| d)
        .find(|d| d.abs_dir.join(rel).is_file())
}

/// The reader-relative testata card for one page — the single-page
/// counterpart of [`sibling_page_candidates`]' card logic: keywords are the
/// reader-visible page topics, the description is read from the owner-tier
/// testata but shown only at the wiki's default visibility.
fn reader_page_card(
    d: &DiscoveredWiki,
    rel: &Path,
    reader_card: &meta_annotate::ReaderCard,
) -> (Option<String>, Vec<String>) {
    let wiki_id = d.meta.wiki_id.as_str();
    let summary = reader_card
        .summary_visible(wiki_id)
        .then(|| {
            meta_annotate::read_page_card(&d.abs_dir.join(rel))
                .unwrap_or_default()
                .description
        })
        .flatten();
    let keywords = reader_card
        .pages(wiki_id)
        .and_then(|pages| {
            pages
                .iter()
                .find(|(source_path, _)| {
                    page_within(&d.rel_dir, source_path).is_some_and(|p| p == rel)
                })
                .map(|(_, topics)| topics.clone())
        })
        .unwrap_or_default();
    (summary, keywords)
}

/// Assemble the per-hop user prompt: the turn, the budget line, the root
/// index, the collected prose, and the numbered candidate list.
#[allow(clippy::too_many_arguments, reason = "one-shot prompt assembly")]
fn build_user_prompt(
    turn_text: &str,
    sender: &SenderContext,
    root_index: &str,
    pool: &[Candidate],
    fragments: &[NavigatedFragment],
    hops_spent: usize,
    max_hops: usize,
    chars_remaining: usize,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "TURN (from sender `{}`):", sender.sender_id);
    out.push_str(turn_text.trim());
    let _ = writeln!(
        out,
        "\n\nBUDGET: hop {} of {max_hops}; ~{chars_remaining} characters of prose still collectable.",
        hops_spent + 1
    );
    out.push_str("\nROOT INDEX:\n");
    out.push_str(if root_index.trim().is_empty() {
        "(empty)"
    } else {
        root_index
    });
    out.push_str("\n\nCOLLECTED:\n");
    if fragments.is_empty() {
        out.push_str("(none yet)\n");
    } else {
        for f in fragments {
            let _ = writeln!(out, "=== {} / {} ===", f.wiki_id, f.page.display());
            out.push_str(&f.text);
            out.push('\n');
        }
    }
    out.push_str("\nCANDIDATES:\n");
    for (i, c) in pool.iter().enumerate() {
        let _ = write!(
            out,
            "{}. wiki_id={} page={} | origin={}",
            i + 1,
            c.wiki_id,
            c.page.display(),
            c.origin
        );
        if let Some(s) = &c.summary {
            let _ = write!(out, " | summary: {s}");
        }
        if !c.keywords.is_empty() {
            let _ = write!(out, " | keywords: {}", c.keywords.join("; "));
        }
        out.push('\n');
    }
    out.push_str("\nReply with the JSON object only.\n");
    out
}

/// Extract the first balanced JSON object from `raw` and parse it — tolerant
/// of fences / prose around the object, same discipline as the ingest plan
/// parser.
fn parse_decision(raw: &str) -> Option<NavDecision> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str::<NavDecision>(&raw[start..=i]).ok();
                }
            },
            _ => {},
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    use crate::types::{FactId, WikiId};
    use crate::wiki::{IdentityKind, create_identity_wiki};

    const UUID_1: &str = "018f1234-5678-7abc-9def-0123456789ab";

    #[test]
    fn parse_query_seeds_tolerates_a_code_fence() {
        let raw = "```json\n{\"topics\": [\"birthday\"], \"entities\": [\"Morgana\"]}\n```";
        let parsed = parse_query_seeds(raw).expect("parsed");
        assert_eq!(parsed.topics, vec!["birthday".to_owned()]);
        assert_eq!(parsed.entities, vec!["Morgana".to_owned()]);
        // Garbage in → None (the caller degrades to A).
        assert!(parse_query_seeds("not json at all").is_none());
    }

    #[test]
    fn a_resolved_entity_stays_a_topic_needle_instead_of_only_a_dead_owner() {
        // `owners` seeds no door since 69b. Routing a RESOLVED name there and
        // ONLY an unresolved one to `topics` served a query about an enrolled
        // person strictly worse than one about a stranger.
        let users = vec![enrollment::EnrolledUserLite {
            user_id: "morgana".to_owned(),
            aliases: vec!["Xheni".to_owned()],
            is_agent: false,
        }];
        let groups: Vec<enrollment::EnrolledGroupLite> = Vec::new();
        let mut topics = vec!["concerti".to_owned()];
        let mut owners: Vec<Principal> = Vec::new();

        fold_entities(
            vec!["Xheni".to_owned(), "Gandalf".to_owned()],
            &users,
            &groups,
            &mut topics,
            &mut owners,
        );

        assert!(
            topics.iter().any(|t| t == "Xheni"),
            "a resolved alias must still reach the card matcher: {topics:?}"
        );
        assert!(
            topics.iter().any(|t| t == "Gandalf"),
            "an unresolved name keeps reaching it: {topics:?}"
        );
        assert_eq!(
            owners,
            vec![Principal::User("morgana".to_owned())],
            "and the resolution is still recorded, once"
        );
        assert_eq!(topics[0], "concerti", "classifier topics keep their order");
    }

    #[test]
    fn fold_entities_does_not_duplicate_a_name_the_classifier_already_gave() {
        let users: Vec<enrollment::EnrolledUserLite> = Vec::new();
        let groups: Vec<enrollment::EnrolledGroupLite> = Vec::new();
        let mut topics = vec!["gandalf".to_owned()];
        let mut owners: Vec<Principal> = Vec::new();
        fold_entities(
            vec!["Gandalf".to_owned()],
            &users,
            &groups,
            &mut topics,
            &mut owners,
        );
        assert_eq!(topics, vec!["gandalf".to_owned()], "case-insensitive dedup");
    }

    #[test]
    fn resolve_entity_matches_user_alias_and_group_else_none() {
        let users = vec![enrollment::EnrolledUserLite {
            user_id: "morgana".to_owned(),
            aliases: vec!["Xheni".to_owned()],
            is_agent: false,
        }];
        let groups = vec![enrollment::EnrolledGroupLite {
            group_id: "famiglia".to_owned(),
            scope: None,
        }];
        // Canonical id (case-insensitive).
        assert_eq!(
            resolve_entity("Morgana", &users, &groups),
            Some(Principal::User("morgana".to_owned()))
        );
        // Alias.
        assert_eq!(
            resolve_entity("xheni", &users, &groups),
            Some(Principal::User("morgana".to_owned()))
        );
        // Group id.
        assert_eq!(
            resolve_entity("famiglia", &users, &groups),
            Some(Principal::Group("famiglia".to_owned()))
        );
        // Unknown name → None (the caller folds it into topics).
        assert_eq!(resolve_entity("Nobody", &users, &groups), None);
    }

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    fn open_tree() -> (tempfile::TempDir, WikiTree) {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        (dir, tree)
    }

    fn forge_user(tree: &WikiTree, id: &str) {
        let wid = WikiId::parse(id).unwrap();
        create_identity_wiki(tree, &wid, id, IdentityKind::User).unwrap();
    }

    fn forge_group(tree: &WikiTree, id: &str) {
        let wid = WikiId::parse(id).unwrap();
        create_identity_wiki(tree, &wid, id, IdentityKind::Group).unwrap();
    }

    fn write_page(tree: &WikiTree, wiki: &str, page: &str, contents: &str) {
        let wid = WikiId::parse(wiki).unwrap();
        let handle = tree.locate(&wid).unwrap();
        handle.write_page(Path::new(page), contents).unwrap();
    }

    fn rag_hit(wiki: &str, source_path: &str, score: f32, fresh: bool) -> RecallHit {
        RecallHit {
            fact_id: FactId::parse(UUID_1).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: source_path.to_owned(),
            region_start: None,
            region_end: None,
            text: "claim".to_owned(),
            owner_id: Principal::global(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            created_at: "2026-06-10T00:00:00Z".to_owned(),
            valid_from: None,
            valid_to: None,
            score,
            fresh,
        }
    }

    fn sender(id: &str, groups: &[&str]) -> SenderContext {
        SenderContext {
            sender_id: id.to_owned(),
            sender_groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        }
    }

    /// A distinct, parse-valid `UUIDv7` per `n` for fact-row seeding.
    fn fid(n: u8) -> String {
        format!("018f1234-5678-7abc-9def-0123456789{n:02x}")
    }

    /// Insert one active fact carrying `topics` with the given ACL `owner`, so
    /// the reader-relative card recomputes from `fact_index` (the navigator no
    /// longer reads topics from the `.md` testata).
    async fn seed_fact(
        pool: &SqlitePool,
        id: &str,
        wiki: &str,
        source_path: &str,
        owner: Principal,
        topics: &[&str],
    ) {
        fact_index::insert(
            pool,
            &fact_index::NewFact {
                fact_id: FactId::parse(id).unwrap(),
                wiki_id: wiki.to_owned(),
                source_path: source_path.to_owned(),
                region_start: None,
                region_end: None,
                text: "body".to_owned(),
                embedding: vec![0.0, 0.0, 0.0, 0.0],
                owner_id: owner,
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: topics.iter().map(|t| (*t).to_owned()).collect(),
                valid_from: None,
                valid_to: None,
                salience: None,
                target_page: None,
                style: None,
                page_description: None,
                source_ref: None,
                authored_refs: Vec::new(),
            },
        )
        .await
        .unwrap();
    }

    fn find<'a>(fan: &'a [EntryPoint], wiki: &str, page: &str) -> Option<&'a EntryPoint> {
        fan.iter()
            .find(|e| e.wiki_id == wiki && e.page == Path::new(page))
    }

    /// Founder, 2026-08-03: *«il recall non entra in una wiki, il recall entra
    /// nelle pagine di contenuto relative ai fatti con score più alto»*. So a
    /// door is a **page**, and nothing that can only name a wiki produces one:
    /// not the sender, not a group they belong to, not the wiki's own map.
    #[tokio::test]
    async fn a_wiki_is_never_a_door_only_its_content_pages_are() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        forge_group(&tree, "famiglia");
        let pool = make_pool().await;
        // One readable fact per wiki, homed on the wiki root — the shape the
        // old principal family turned into a door. None of them may seed one.
        for (n, w) in [(1u8, "alice"), (2, "bob"), (3, "famiglia")] {
            seed_fact(
                &pool,
                &fid(n),
                w,
                &format!("wikis/{w}/index.md"),
                Principal::global(),
                &[],
            )
            .await;
        }

        let fan = gather_entry_points(&pool, &tree, &sender("alice", &["famiglia"]), &[], &[], &[])
            .await
            .unwrap();

        assert!(
            fan.is_empty(),
            "no wiki-level door survives — the sender's own, their group's, nor a third party's: {fan:?}"
        );
    }

    #[tokio::test]
    async fn topic_seeds_reach_page_cards_and_never_the_wiki_map() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "bob");
        let pool = make_pool().await;
        // The reader-relative card derives from `fact_index`, not the `.md`:
        // public facts on bob's wiki so the searcher (alice) can read their
        // topics. boats.md carries "Sailing"; food.md does not. The wiki-level
        // card is the union, so "sailing" matches the wiki and lets the
        // descent run — but only boats.md becomes a door.
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/boats.md",
            Principal::global(),
            &["Sailing", "regatta"],
        )
        .await;
        seed_fact(
            &pool,
            &fid(2),
            "bob",
            "wikis/bob/food.md",
            Principal::global(),
            &["pasta"],
        )
        .await;
        // The same topic on the wiki's map: it feeds the card, never a door.
        seed_fact(
            &pool,
            &fid(3),
            "bob",
            "wikis/bob/index.md",
            Principal::global(),
            &["Sailing"],
        )
        .await;

        // Case-insensitive: classified topic "SAILING" vs card "Sailing".
        let fan = gather_entry_points(
            &pool,
            &tree,
            &sender("alice", &[]),
            &["SAILING".to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();

        let page_seed = find(&fan, "bob", "boats.md").expect("page-level topic seed");
        assert_eq!(page_seed.origin, EntryOrigin::Topic);
        assert!((page_seed.weight - WEIGHT_TOPIC_PAGE).abs() < f32::EPSILON);
        assert!(
            find(&fan, "bob", "food.md").is_none(),
            "non-matching page card must not seed"
        );
        assert!(
            find(&fan, "bob", "index.md").is_none(),
            "the wiki's map is not a door however well its card matches"
        );
        assert_eq!(fan.len(), 1);
    }

    #[tokio::test]
    async fn topic_seeds_are_reader_relative_a_private_topic_never_seeds_a_denied_reader() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "bob");
        let pool = make_pool().await;
        // A PRIVATE fact on bob's wiki: only bob can read "celiachia" (owner
        // bob, no allow). The owner-tier `.md` card would carry the topic to
        // anyone; the reader-relative card must not.
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/salute.md",
            Principal::User("bob".to_owned()),
            &["celiachia"],
        )
        .await;

        // bob (the owner) is topic-seeded down to the page.
        let bob_fan = gather_entry_points(
            &pool,
            &tree,
            &sender("bob", &[]),
            &["celiachia".to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();
        let page_seed = find(&bob_fan, "bob", "salute.md").expect("owner page topic seed");
        assert_eq!(page_seed.origin, EntryOrigin::Topic);

        // alice (denied) gets NO seed from the private topic — the leak closed.
        let alice_fan = gather_entry_points(
            &pool,
            &tree,
            &sender("alice", &[]),
            &["celiachia".to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();
        assert!(
            alice_fan.iter().all(|e| e.wiki_id != "bob"),
            "a denied reader must not be seeded by a private fact's topic"
        );
    }

    #[tokio::test]
    async fn blank_queries_and_unmatched_topics_seed_nothing() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "bob");
        let pool = make_pool().await;
        // A real, readable topic to NOT match — so the test exercises the
        // blank/no-match filter, not merely an empty card.
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/boats.md",
            Principal::global(),
            &["sailing"],
        )
        .await;

        let fan = gather_entry_points(
            &pool,
            &tree,
            &sender("", &[]),
            &[String::new(), "   ".to_owned(), "quantum".to_owned()],
            &[],
            &[],
        )
        .await
        .unwrap();
        assert!(fan.is_empty());
    }

    #[tokio::test]
    async fn situational_strings_match_like_topics_with_their_own_weight() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "bob");
        let pool = make_pool().await;
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/boats.md",
            Principal::global(),
            &["sailing"],
        )
        .await;

        let fan = gather_entry_points(
            &pool,
            &tree,
            &sender("alice", &[]),
            &[],
            &[],
            &["sailing".to_owned()],
        )
        .await
        .unwrap();

        let seed = find(&fan, "bob", "boats.md").expect("situational seed");
        assert_eq!(seed.origin, EntryOrigin::Situational);
        assert!((seed.weight - WEIGHT_SITUATIONAL_PAGE).abs() < f32::EPSILON);
    }

    /// A RAG hit is a door only when it names a page the funnel may read. The
    /// three that do not — an un-promoted `fresh` capture with no published
    /// page, a hit on the channel-only `rules.md` (roadmap 41e), and a hit on
    /// the wiki's map — surface through the flat slot and seed nothing.
    #[tokio::test]
    async fn rag_seeds_map_to_their_page_and_the_unreadable_ones_seed_nothing() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(&tree, "alice", "recipes.md", "# Recipes\n");
        write_page(&tree, "alice", "rules.md", "# Rules\n");
        let pool = make_pool().await;

        let fan = gather_entry_points(
            &pool,
            &tree,
            &sender("alice", &[]),
            &[],
            &[
                rag_hit("alice", "wikis/alice/recipes.md", 0.42, false),
                rag_hit("alice", "wikis/alice/_captures.md", 0.9, true), // fresh
                rag_hit("alice", "wikis/alice/rules.md", 0.9, false),    // channel-only
                rag_hit("alice", "wikis/alice/index.md", 0.9, false),    // the map
                rag_hit("nowhere", "wikis/nowhere/x.md", 0.9, false),    // unknown wiki
            ],
            &[],
        )
        .await
        .unwrap();

        let page_seed = find(&fan, "alice", "recipes.md").expect("rag page seed");
        assert_eq!(page_seed.origin, EntryOrigin::Rag);
        assert!((page_seed.weight - 0.42).abs() < f32::EPSILON);
        assert_eq!(fan.len(), 1, "only the readable page is a door: {fan:?}");
    }

    #[tokio::test]
    async fn dedup_keeps_the_best_route_to_a_door_and_sorts_by_weight() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        let pool = make_pool().await;
        // alice's "food" fact on a real page → a topic seed at 0.8.
        seed_fact(
            &pool,
            &fid(1),
            "alice",
            "wikis/alice/menu.md",
            Principal::User("alice".to_owned()),
            &["food"],
        )
        .await;

        let fan = gather_entry_points(
            &pool,
            &tree,
            &sender("alice", &[]),
            &["food".to_owned()],
            &[
                rag_hit("alice", "wikis/alice/menu.md", 0.9, false), // same door, heavier
                rag_hit("bob", "wikis/bob/notes.md", 0.3, false),
                rag_hit("bob", "wikis/bob/notes.md", 0.7, false), // duplicate, heavier
            ],
            &[],
        )
        .await
        .unwrap();

        // alice's menu page is reached twice — as a topic card match (0.8) and
        // as a rag hit (0.9). The better route survives.
        let alice_page = find(&fan, "alice", "menu.md").unwrap();
        assert_eq!(alice_page.origin, EntryOrigin::Rag);
        assert!((alice_page.weight - 0.9).abs() < f32::EPSILON);
        // bob's page: the heavier rag duplicate survived.
        let bob_page = find(&fan, "bob", "notes.md").unwrap();
        assert!((bob_page.weight - 0.7).abs() < f32::EPSILON);
        for pair in fan.windows(2) {
            assert!(pair[0].weight >= pair[1].weight);
        }
    }

    #[test]
    fn page_within_strips_the_wiki_prefix_and_rejects_foreign_paths() {
        // Nested wiki: rel_dir carries the parent chain.
        assert_eq!(
            page_within(Path::new("wikis/alice/acme"), "wikis/alice/acme/notes.md"),
            Some(PathBuf::from("notes.md"))
        );
        assert_eq!(
            page_within(Path::new("wikis/alice"), "wikis/alice/sub/page.md"),
            Some(PathBuf::from("sub/page.md"))
        );
        // A path outside the wiki (stale row) falls back to the root.
        assert_eq!(
            page_within(Path::new("wikis/alice"), "wikis/bob/x.md"),
            None
        );
        assert_eq!(page_within(Path::new("wikis/alice"), "wikis/alice/"), None);
    }

    // ---------- prune_pool ----------

    /// Minimal `Candidate` fixture for `prune_pool` tests: only `wiki_id`,
    /// `page` and `origin` are load-bearing for pruning, so the card fields
    /// are left empty.
    fn cand(wiki: &str, page: &str, origin: &'static str) -> Candidate {
        Candidate {
            wiki_id: wiki.to_owned(),
            page: PathBuf::from(page),
            origin,
            summary: None,
            keywords: Vec::new(),
        }
    }

    #[test]
    fn prune_pool_ranks_a_link_above_a_sibling_page_of_the_same_wiki() {
        let visited = BTreeSet::new();
        // Built in the order the real producers emit them: `open_target`
        // extends `discoveries` with siblings first, links second — the
        // exact positional bias that used to let the directory dump bury
        // the rail.
        let mut pool = vec![cand("alice", "b.md", "page"), cand("alice", "a.md", "link")];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["link", "page"],
            "a wikilink rail must be offered ahead of a directory sibling of the same wiki"
        );
    }

    #[test]
    fn one_page_reached_by_two_routes_keeps_the_better_route_not_the_first() {
        let visited = BTreeSet::new();
        // The commonest collision in the live corpus: the funnel enters
        // `alice`, lists its directory, then reads a page whose prose links
        // `alice/concerti`. The SAME destination is now in the pool twice —
        // sibling first (that is the order `open_target` fills `discoveries`
        // in), rail second. Deduplicating before ranking kept the sibling and
        // filed an authored rail in the demoted tail.
        let mut pool = vec![
            cand("alice", "concerti.md", "page"),
            cand("alice", "concerti.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(pool.len(), 1, "the two copies are one destination");
        assert_eq!(
            pool[0].origin, "link",
            "the surviving copy must carry the best route to the page, not the earliest one"
        );
    }

    #[test]
    fn a_linked_page_that_is_also_a_sibling_survives_a_cap_full_of_siblings() {
        let visited = BTreeSet::new();
        // The consequence of the rule above, at the size the live corpus
        // actually offers: a directory listing far larger than the cap, with
        // the one page the prose links to sitting late in it. Filed as a
        // sibling it is cut with the tail; filed as the rail it is, it leads.
        let mut pool: Vec<Candidate> = (0..30)
            .map(|i| Candidate {
                wiki_id: "alice".to_owned(),
                page: PathBuf::from(format!("p{i:02}.md")),
                origin: "page",
                summary: None,
                keywords: Vec::new(),
            })
            .collect();
        pool.push(cand("alice", "p29.md", "link"));
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            (pool[0].origin, Some(pool[0].page.as_path())),
            ("link", Some(Path::new("p29.md"))),
            "the linked page must lead the pool even when its directory buries it"
        );
    }

    #[test]
    fn a_fan_seed_that_is_also_a_sibling_is_not_demoted_to_the_tail() {
        let visited = BTreeSet::new();
        // Same rule, the other collision: the gatherer already weighed this
        // page into the fan, and entering its wiki later re-offers it as one
        // of the directory's siblings.
        let mut pool = vec![
            cand("alice", "hit.md", "page"),
            cand("alice", "hit.md", "rag"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["rag"],
            "a page the fan already weighed must not lose that provenance to a directory listing"
        );
    }

    #[test]
    fn prune_pool_never_lets_a_sibling_page_displace_a_rag_seed() {
        let visited = BTreeSet::new();
        let mut pool = vec![
            cand("alice", "sib.md", "page"),
            cand("bob", "index_stand_in.md", "rag"),
        ];
        // Cap forces a choice between the two.
        prune_pool(&mut pool, &visited, 1, 3);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["rag"],
            "a RAG-seeded page must survive the cap over a sibling, whatever their pool position"
        );
    }

    #[test]
    fn prune_pool_keeps_the_highest_tier_entries_not_the_alphabetically_first_ones() {
        let visited = BTreeSet::new();
        // `wiki_id` is alphabetical in insertion order — mirroring
        // `wiki::list_wiki_pages`'s sort — but the alphabetically-first
        // entries are the lowest tier (`page`) and the alphabetically-last
        // are the highest (`rag`, `link`). A positional `truncate` would
        // keep exactly the wrong four; tier order must win instead.
        let mut pool = vec![
            cand("a-sib", "index.md", "page"),
            cand("b-sib", "index.md", "page"),
            cand("c-sib", "index.md", "page"),
            cand("d-sib", "index.md", "page"),
            cand("e-rag", "index_stand_in.md", "rag"),
            cand("f-link", "index_stand_in.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 2, 3);
        assert_eq!(
            pool.iter().map(|c| c.wiki_id.as_str()).collect::<Vec<_>>(),
            vec!["f-link", "e-rag"],
            "the cap must keep the rail and the fan seed, not the alphabetically-first siblings"
        );
    }

    #[test]
    fn prune_pool_still_offers_a_sibling_only_page_when_the_cap_allows() {
        let visited = BTreeSet::new();
        let mut pool = vec![
            cand("alice", "index_stand_in.md", "rag"),
            // Linked from nowhere yet — reachable only as a directory sibling.
            cand("alice", "orphan.md", "page"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert!(
            pool.iter()
                .any(|c| c.wiki_id == "alice"
                    && Some(c.page.as_path()) == Some(Path::new("orphan.md"))),
            "a page reachable only as a sibling must still surface while the offer is under \
             the floor — rationed, not removed"
        );
    }

    #[test]
    fn prune_pool_admits_no_sibling_at_all_once_the_rails_reach_the_floor() {
        let visited = BTreeSet::new();
        // The shape the founder ruled on (2026-08-04): the funnel has real
        // rails to follow, and the wiki it just entered also offers its whole
        // directory. Tiering alone left the directory filling every slot below
        // the rails; the floor now keeps it out entirely.
        let mut pool = vec![
            cand("alice", "sib1.md", "page"),
            cand("alice", "sib2.md", "page"),
            cand("alice", "sib3.md", "page"),
            cand("alice", "rail1.md", "link"),
            cand("alice", "rail2.md", "link"),
            cand("alice", "rail3.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["link", "link", "link"],
            "with the floor already met by rails, the directory listing must not be offered \
             at all — the cap has room, and that is no longer the test"
        );
    }

    #[test]
    fn prune_pool_tops_the_offer_up_with_siblings_when_the_rails_fall_short() {
        let visited = BTreeSet::new();
        // The other half of the same rule: one rail is not a choice, so the
        // directory tops the offer up to the floor — and stops there. This is
        // the dead-end continuation (card 66's 66b) the ruling keeps alive.
        let mut pool = vec![
            cand("alice", "sib1.md", "page"),
            cand("alice", "sib2.md", "page"),
            cand("alice", "sib3.md", "page"),
            cand("alice", "rail1.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            pool.len(),
            3,
            "one rail plus two siblings reaches the floor of 3, and the third sibling stays out"
        );
        assert_eq!(pool[0].origin, "link", "the rail still leads");
    }

    #[test]
    fn prune_pool_preserves_the_gatherers_relative_order_inside_the_fan_tier() {
        let visited = BTreeSet::new();
        // principal / rag / topic / situational, in this order, is the
        // gatherer's own weight order (`dedup_and_sort`) — pruning must
        // carry it through unchanged, never re-sort or re-weight it.
        let mut pool = vec![
            cand("p", "index_stand_in.md", "principal"),
            cand("r", "index_stand_in.md", "rag"),
            cand("t", "index_stand_in.md", "topic"),
            cand("s", "index_stand_in.md", "situational"),
        ];
        prune_pool(&mut pool, &visited, 16, 3);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["principal", "rag", "topic", "situational"],
            "prune_pool must not disturb the entry-point fan's relative order"
        );
    }

    // ---------- the navigator funnel ----------

    use std::collections::VecDeque;

    use crate::llm::{CompletionResponse, CompletionUsage, FinishReason};

    /// Scripted backend: returns the queued responses in order, then
    /// "done" forever — so a test scripts each hop's decision.
    struct ScriptedLlm(std::sync::Mutex<VecDeque<String>>);

    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self(std::sync::Mutex::new(
                responses.iter().map(|s| (*s).to_owned()).collect(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for ScriptedLlm {
        fn model_id(&self) -> &'static str {
            "scripted"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::llm::Result<CompletionResponse> {
            let text = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"open":[],"done":true}"#.to_owned());
            Ok(CompletionResponse {
                text,
                finish_reason: FinishReason::EndOfTurn,
                usage: CompletionUsage::default(),
            })
        }
    }

    fn entry(wiki: &str, page: &str, origin: EntryOrigin, weight: f32) -> EntryPoint {
        EntryPoint {
            wiki_id: wiki.to_owned(),
            page: PathBuf::from(page),
            origin,
            weight,
        }
    }

    #[tokio::test]
    async fn navigate_opens_vetted_pages_and_projects_acl() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(
            &tree,
            "alice",
            "notes.md",
            &format!(
                "---\ntitle: \"Notes\"\n---\n\nShared prose.\n\n\
                 {{{{owner=user:alice f={UUID_1}}}}}secret{{{{/}}}}\n"
            ),
        );
        // A second page, so the pool is not empty at hop 2 and the scripted
        // "done" decision is reached: the wiki root no longer stands in as a
        // sibling, being the map rather than a page.
        write_page(&tree, "alice", "more.md", "Filler.\n");
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"notes.md"}],"done":false,"note":"go"}"#,
            r#"{"open":[],"done":true}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("mallory", &[]),
            "what do we know?",
            &[entry("alice", "notes.md", EntryOrigin::Topic, 0.8)],
            // These filters guard the directory listing, which is OFF by
            // default since 2026-08-04 — turn it on so they are still tested.
            &NavigatorPolicy {
                sibling_floor: 16,
                ..NavigatorPolicy::default()
            },
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 2);
        assert!(!out.truncated);
        assert_eq!(out.fragments.len(), 1);
        let f = &out.fragments[0];
        assert_eq!(f.wiki_id, "alice");
        assert_eq!(f.page, PathBuf::from("notes.md"));
        assert!(f.text.contains("Shared prose."));
        assert!(
            !f.text.contains("secret") && f.text.contains("[redacted]"),
            "alice's region must be projected away for mallory: {}",
            f.text
        );
        assert!(!f.text.contains("title:"), "testata must be dropped");

        // The funnel journal mirrors the run: hop 1 offered the fan card,
        // opened the pick (note captured), hop 2 was the done decision.
        assert_eq!(out.stop, NavStop::Done);
        assert_eq!(out.trace.len(), 2);
        let hop = &out.trace[0];
        assert_eq!(hop.note.as_deref(), Some("go"));
        assert_eq!(hop.candidates.len(), 1);
        assert_eq!(hop.candidates[0].wiki_id, "alice");
        assert_eq!(hop.requested.len(), 1);
        assert!(hop.requested[0].opened);
        assert_eq!(hop.opened.len(), 1);
        assert_eq!(hop.opened[0].chars, f.text.len());
        assert!(hop.opened[0].excerpt.contains("Shared prose."));
        assert!(
            hop.opened[0].excerpt.contains("[redacted]")
                && !hop.opened[0].excerpt.contains("secret"),
            "the journaled excerpt is the projected prose, never the raw region"
        );
        assert!(out.trace[1].done && out.trace[1].requested.is_empty());
    }

    #[tokio::test]
    async fn navigate_gates_regions_by_db_acl_over_inline_attributes() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        // The marker claims global (stale inline copy) — the DB row is
        // the authority and says owner=user:alice.
        write_page(
            &tree,
            "alice",
            "notes.md",
            &format!("Shared prose.\n\n{{{{owner=global f={UUID_1}}}}}secret{{{{/}}}}\n"),
        );
        let pool = make_pool().await;
        fact_index::insert(
            &pool,
            &fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: FactId::parse(UUID_1).unwrap(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/notes.md".to_owned(),
                region_start: None,
                region_end: None,
                text: "secret".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                owner_id: Principal::User("alice".into()),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                salience: None,
                target_page: None,
                style: None,
                page_description: None,
                source_ref: None,
            },
        )
        .await
        .expect("insert fact row");
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"notes.md"}],"done":false}"#,
            r#"{"open":[],"done":true}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("mallory", &[]),
            "what do we know?",
            &[entry("alice", "notes.md", EntryOrigin::Topic, 0.8)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.fragments.len(), 1);
        let f = &out.fragments[0];
        assert!(
            !f.text.contains("secret") && f.text.contains("[redacted]"),
            "the DB owner must out-gate the inline owner=global: {}",
            f.text
        );
    }

    #[tokio::test]
    async fn navigate_discards_hallucinated_targets_and_stops() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"ghost"},{"wiki_id":"alice","page":"nope.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();

        // Both picks were vetted away (unknown wiki / non-candidate page)
        // → zero pages opened → the funnel stops instead of replaying.
        assert_eq!(out.hops, 1);
        assert!(out.fragments.is_empty());
        // The journal shows both discards, so the trace viewer can replay
        // the vetting.
        assert_eq!(out.stop, NavStop::NothingOpened);
        assert_eq!(out.trace.len(), 1);
        assert_eq!(out.trace[0].requested.len(), 2);
        assert!(out.trace[0].requested.iter().all(|r| !r.opened));
        assert!(out.trace[0].opened.is_empty());
    }

    /// Founder, 2026-08-03 — *«il recall non entra in una wiki»*. A bare
    /// `[[bob]]` rail names a wiki, so it is **not** a door: what it points at
    /// is that wiki's map, which belongs to REM and the ingest classifier. A
    /// page hop (`[[bob/hobbies]]`) still is one — that is the rail that names
    /// content, and the sibling listing is the only other way in, arrived at by
    /// reading a page rather than by choosing a wiki.
    #[tokio::test]
    async fn a_bare_wiki_rail_resolves_to_the_foundation_page_never_the_map() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        write_page(
            &tree,
            "alice",
            "rails.md",
            "# Rails\n\nSee [[bob]] and [[bob/hobbies]].\n",
        );
        write_page(&tree, "bob", "hobbies.md", "# Hobbies\n\nBob sails.\n");
        // What `[[bob]]` means is *bob*, and since 63 §8 bob is his card.
        write_page(
            &tree,
            "bob",
            wiki::PROFILE_FILENAME,
            "# Bob\n\nBob is 40.\n",
        );
        let pool = make_pool().await;
        // Derived visibility: a rail is followed only if alice can read ≥ 1
        // fact in bob's wiki. Seed a public (global-owned) fact there.
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/hobbies.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            // Both are asked for; only the page hop is an offered candidate.
            r#"{"open":[{"wiki_id":"bob"},{"wiki_id":"bob","page":"hobbies.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(), // max_hops = 2
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.fragments.len(), 2);
        assert_eq!(out.fragments[1].wiki_id, "bob");
        assert_eq!(out.fragments[1].page, PathBuf::from("hobbies.md"));
        assert!(out.fragments[1].text.contains("Bob sails."));
        // The bare rail now IS a door — onto bob's card. What it must never
        // reach is the map, whatever route asked for it.
        let offered: Vec<&str> = out.trace[1]
            .candidates
            .iter()
            .filter(|c| c.wiki_id == "bob")
            .filter_map(|c| c.page.as_deref())
            .collect();
        assert!(
            offered.contains(&wiki::PROFILE_FILENAME),
            "`[[bob]]` must be offered as bob's foundation page: {offered:?}"
        );
        assert!(
            !offered.contains(&wiki::INDEX_FILENAME),
            "no route may offer a wiki's map: {offered:?}"
        );
        // The navigator asked for the wiki with no page at all — that shape
        // is still not a target, it is what the rail resolved *away from*.
        let asked = &out.trace[1].requested;
        assert!(
            asked.iter().any(|r| r.page.is_none() && !r.opened),
            "a page-less request must be discarded, not opened: {asked:?}"
        );
    }

    #[tokio::test]
    async fn a_bare_wiki_rail_stays_dead_when_the_wiki_has_no_foundation_page() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        // Bob has content but nothing authored as his foundation: no card,
        // no buffer. There is nothing for `[[bob]]` to mean, so the rail is
        // dropped rather than falling back onto the map.
        write_page(&tree, "alice", "rails.md", "# Rails\n\nSee [[bob]].\n");
        write_page(&tree, "bob", "hobbies.md", "# Hobbies\n\nBob sails.\n");
        let pool = make_pool().await;
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/hobbies.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            r#"{"open":[],"done":true}"#,
        ]);
        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();
        let bob_offers: Vec<Option<&str>> = out
            .trace
            .iter()
            .flat_map(|hop| hop.candidates.iter())
            .filter(|c| c.wiki_id == "bob")
            .map(|c| c.page.as_deref())
            .collect();
        assert!(
            bob_offers.is_empty(),
            "a foundation-less wiki must yield no door at all, least of all its map: \
             {bob_offers:?}"
        );
    }

    #[tokio::test]
    async fn navigate_follows_legacy_bare_slug_links_as_same_wiki_pages() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        // Pre-canonical corpus grammar: a bare `[[notes]]` naming no wiki
        // must resolve as the same-wiki page `notes.md` (emit canonical,
        // resolve legacy). `[[ghost]]` matches nothing and stays a dead rail.
        write_page(
            &tree,
            "alice",
            "rails.md",
            "# Rails\n\nSee [[notes]] and [[ghost]].\n",
        );
        write_page(&tree, "alice", "notes.md", "# Notes\n\nAlice paints.\n");
        let pool = make_pool().await;
        // The fallback keeps the derived-visibility gate on the resolved
        // wiki: alice must read ≥ 1 fact there for the page to be offered.
        seed_fact(
            &pool,
            &fid(1),
            "alice",
            "wikis/alice/notes.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            r#"{"open":[{"wiki_id":"alice","page":"notes.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(), // max_hops = 2
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 2);
        assert_eq!(out.fragments.len(), 2);
        assert_eq!(
            (
                out.fragments[1].wiki_id.as_str(),
                out.fragments[1].page.display().to_string().as_str(),
            ),
            ("alice", "notes.md"),
            "the bare [[notes]] rail must open the same-wiki page"
        );
        assert!(out.fragments[1].text.contains("Alice paints."));
    }

    #[tokio::test]
    async fn navigate_follows_legacy_bare_slug_links_across_wiki_lines() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        // The legacy corpus links pages by bare name across wikis:
        // `[[hobbies]]` on an alice page names bob's page — no `hobbies.md`
        // in alice, so the deterministic order reaches bob's.
        write_page(&tree, "alice", "rails.md", "# Rails\n\nSee [[hobbies]].\n");
        write_page(&tree, "bob", "hobbies.md", "# Hobbies\n\nBob sails.\n");
        let pool = make_pool().await;
        // Reader gate on the resolved destination: alice must read ≥ 1
        // fact in bob's wiki for the page to be offered.
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/hobbies.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            r#"{"open":[{"wiki_id":"bob","page":"hobbies.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(), // max_hops = 2
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 2);
        assert_eq!(out.fragments.len(), 2);
        assert_eq!(
            (
                out.fragments[1].wiki_id.as_str(),
                out.fragments[1].page.display().to_string().as_str(),
            ),
            ("bob", "hobbies.md"),
            "the bare [[hobbies]] rail must resolve across wiki lines"
        );
        assert!(out.fragments[1].text.contains("Bob sails."));
    }

    #[tokio::test]
    async fn navigate_follows_page_hop_wikilinks_directly_and_strips_aliases() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        // The page hop carries a `|display` alias — resolution must strip it.
        // `[[bob/missing]]` is a dead rail (no such file) and must never
        // become a candidate.
        write_page(
            &tree,
            "alice",
            "rails.md",
            "# Rails\n\nDetail at [[bob/hobbies|Bob's hobbies]] and [[bob/missing]].\n",
        );
        write_page(&tree, "bob", "hobbies.md", "# Hobbies\n\nBob sails.\n");
        let pool = make_pool().await;
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/hobbies.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            // The linked PAGE itself must be an offered candidate — one hop,
            // no descent through bob's wiki.
            r#"{"open":[{"wiki_id":"bob","page":"hobbies.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(), // max_hops = 2
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 2);
        assert_eq!(out.fragments.len(), 2);
        assert_eq!(
            (
                out.fragments[1].wiki_id.as_str(),
                out.fragments[1].page.as_path()
            ),
            ("bob", Path::new("hobbies.md")),
            "a [[wiki/page|alias]] hop must offer the page itself as a candidate"
        );
        assert!(out.fragments[1].text.contains("Bob sails."));
    }

    #[tokio::test]
    async fn navigate_never_offers_a_dead_page_hop() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        forge_user(&tree, "bob");
        // Only a dead rail in the prose: the page does not exist.
        write_page(
            &tree,
            "alice",
            "rails.md",
            "# Rails\n\nSee [[bob/missing]].\n",
        );
        write_page(&tree, "bob", "overview.md", "# Bob\n\nBob overview.\n");
        let pool = make_pool().await;
        seed_fact(
            &pool,
            &fid(1),
            "bob",
            "wikis/bob/overview.md",
            Principal::global(),
            &[],
        )
        .await;
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            // The navigator tries the dead page anyway — it must have been
            // vetted away (never offered), so nothing opens and the funnel stops.
            r#"{"open":[{"wiki_id":"bob","page":"missing.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &pool,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.fragments.len(), 1, "only alice's page was readable");
        assert_eq!(out.fragments[0].wiki_id, "alice");
    }

    #[tokio::test]
    async fn navigate_respects_the_char_budget() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(
            &tree,
            "alice",
            "notes.md",
            "---\ntitle: \"Notes\"\n---\n\nA very long body that does not fit the budget at all.\n",
        );
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"notes.md"}],"done":false}"#,
            r#"{"open":[{"wiki_id":"alice","page":"rules.md"}],"done":false}"#,
        ]);
        let policy = NavigatorPolicy {
            char_budget: 10,
            ..NavigatorPolicy::default()
        };

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "notes.md", EntryOrigin::Rag, 0.9)],
            &policy,
            &[],
        )
        .await
        .unwrap();

        assert!(out.truncated);
        assert_eq!(out.fragments.len(), 1);
        assert!(out.fragments[0].text.len() <= 10);
        assert_eq!(out.hops, 1, "an exhausted budget must not buy another hop");
        assert_eq!(out.stop, NavStop::Budget);
    }

    /// Roadmap 69b — a page the caller has **already delivered** is not a
    /// navigation destination by any route. The ingest turn passes the
    /// sender's identity page, which `WHO IS SPEAKING` serves in full every
    /// turn (69a): re-reading it would spend a page open and a slice of the
    /// character budget on prose already in the block.
    ///
    /// Founder, 2026-08-03: *«non ci frega dell'indice se col rag arriviamo
    /// già sulle pagine giuste»*. The three routes are covered here — the
    /// entry fan (the seed is offered but the pool drops it), a verbatim
    /// navigator request for it, and the directory listing of the wiki once
    /// the funnel is inside.
    #[tokio::test]
    async fn navigate_never_opens_a_page_the_caller_already_served() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(&tree, "alice", "index.md", "# Alice\n\nHer whole card.\n");
        write_page(&tree, "alice", "notes.md", "Ordinary prose.\n");
        // Hop 1 asks for the wiki root — which resolves to `index.md` — and
        // for a real page beside it. Hop 2 asks for `index.md` by name, the
        // shape a sibling listing would offer.
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice"},{"wiki_id":"alice","page":"notes.md"}],"done":false}"#,
            r#"{"open":[{"wiki_id":"alice","page":"index.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "what do we know?",
            &[
                entry("alice", "rails.md", EntryOrigin::Rag, 0.9),
                entry("alice", "notes.md", EntryOrigin::Rag, 0.5),
            ],
            &NavigatorPolicy::default(),
            &[("alice".to_owned(), PathBuf::from("index.md"))],
        )
        .await
        .unwrap();

        assert!(
            !out.fragments
                .iter()
                .any(|f| f.page == Path::new("index.md")),
            "the served page must never be opened: {:?}",
            out.fragments
        );
        assert!(
            out.fragments
                .iter()
                .any(|f| f.page == Path::new("notes.md")),
            "and refusing it must not cost the walk its other choice"
        );
        assert!(
            out.trace.iter().all(|hop| hop
                .candidates
                .iter()
                .all(|c| c.page.as_deref() != Some("index.md"))),
            "nor is it ever offered — not from the fan, not from the directory listing"
        );
        assert!(
            !out.fragments.iter().any(|f| f.text.contains("whole card")),
            "and none of its prose reaches the caller a second time"
        );
    }

    /// Roadmap 41e — the reserved `rules.md` policy page is channel-only:
    /// the sibling fan never offers it as a door, and even a navigator that
    /// asks for it verbatim is discarded by the `open_target` fail-safe.
    #[tokio::test]
    async fn navigate_never_offers_nor_opens_the_rules_page() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(&tree, "alice", "rules.md", "# Rules\n\nStanding policy.\n");
        write_page(&tree, "alice", "notes.md", "Ordinary prose.\n");
        write_page(&tree, "alice", "rails.md", "Entry prose.\n");
        // Hop 1 opens a page (which reveals the wiki's sibling listing); hop 2
        // asks for the rules page verbatim — a non-candidate by construction,
        // and gated even if it were offered.
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
            r#"{"open":[{"wiki_id":"alice","page":"rules.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "what do we know?",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            // These filters guard the directory listing, which is OFF by
            // default since 2026-08-04 — turn it on so they are still tested.
            &NavigatorPolicy {
                sibling_floor: 16,
                ..NavigatorPolicy::default()
            },
            &[],
        )
        .await
        .unwrap();

        assert!(
            out.fragments
                .iter()
                .all(|f| f.page != Path::new("rules.md")),
            "the rules page must never be opened"
        );
        assert!(
            out.trace
                .iter()
                .flat_map(|h| h.candidates.iter())
                .all(|c| c.page.as_deref() != Some("rules.md")),
            "the rules page must never be offered as a candidate door"
        );
        // The ordinary sibling page IS offered once the wiki is entered.
        assert!(
            out.trace
                .iter()
                .flat_map(|h| h.candidates.iter())
                .any(|c| c.page.as_deref() == Some("notes.md")),
            "content siblings keep being offered"
        );
    }

    #[tokio::test]
    async fn navigate_soft_fails_on_unparseable_decision() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        let llm = ScriptedLlm::new(&["I would suggest opening the alice wiki first."]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 1);
        assert!(out.fragments.is_empty(), "degrade, don't error");
        // The spent decision is journaled (candidates offered, no picks) so
        // the degraded hop stays visible in the trace.
        assert_eq!(out.stop, NavStop::LlmDegraded);
        assert_eq!(out.trace.len(), 1);
        assert!(!out.trace[0].candidates.is_empty());
        assert!(out.trace[0].requested.is_empty());
    }

    #[tokio::test]
    async fn navigate_stops_on_done_and_skips_llm_on_empty_fan() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");

        // done=true on the first hop → nothing collected, one hop spent.
        let llm = ScriptedLlm::new(&[r#"{"open":[],"done":true,"note":"enough"}"#]);
        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(out.hops, 1);
        assert!(out.fragments.is_empty());
        assert_eq!(out.stop, NavStop::Done);
        assert_eq!(out.trace.len(), 1);
        assert_eq!(out.trace[0].note.as_deref(), Some("enough"));
        assert!(out.trace[0].done);

        // Empty fan → no completion at all.
        let llm = ScriptedLlm::new(&[]);
        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "turn",
            &[],
            &NavigatorPolicy::default(),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(out.hops, 0);
        assert!(out.fragments.is_empty());
        assert_eq!(out.stop, NavStop::EmptyFan);
        assert!(out.trace.is_empty());
    }

    #[test]
    fn parse_decision_tolerates_fences_and_prose() {
        let d = parse_decision("```json\n{\"open\":[{\"wiki_id\":\"a\"}],\"done\":false}\n```")
            .expect("parsed");
        assert_eq!(d.open.len(), 1);
        assert_eq!(d.open[0].wiki_id, "a");
        assert!(!d.done);
        assert!(parse_decision("no json here").is_none());
    }
}
