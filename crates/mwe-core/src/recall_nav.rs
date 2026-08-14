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
//! # 🚨 THE READ SIDE HAS NO CONCEPT OF A WIKI
//!
//! **Whoever reads the memory does not know wikis exist.** No catalogue is
//! rendered, none is needed, and nothing here picks or enters a container: the
//! reader arrives on the pages its own facts landed on and travels by the
//! wikilinks written on them. A wiki is the WRITE side's instrument. On this
//! side `wiki_id` is an **address** — the first half of `wiki_id/page.md`, as
//! a folder is the first half of a file path — never a place. What is still
//! derived per wiki is **visibility** (access control: a reader who can read
//! no fact in a wiki sees nothing from it) and the **smart-wiki skip** (a
//! storage kind, not a container). Founder's ruling, shipped 2026-08-03; a
//! comment anywhere that says the reader chooses or enters a wiki is stale.
//!
//! The gatherer's fan feeds the **navigator funnel** ([`navigate`]): a
//! Rust-owned loop where the `navigator` LLM slot reads the
//! destination cards and the prose collected so far, and decides which
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
use crate::page_card;
use crate::prompts;
use crate::recall::{MULTI_HOP_HARD_LIMIT, RecallHit, SenderContext, extract_wikilinks};
use crate::render::render_for_sender;
use crate::types::Principal;
use crate::wiki::{self, DiscoveredWiki, MarkdownDoc, WikiTree};

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

/// One walk row, kept so a page's `source_path` can be resolved back to its
/// wiki-relative path. It carries no card of its own: matching is per page
/// ([`gather_card_seeds`]), and the reader is never shown the container.
struct WikiSeedInfo {
    wiki: DiscoveredWiki,
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
    let infos = build_seed_infos(tree)?;

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
fn build_seed_infos(tree: &WikiTree) -> Result<Vec<WikiSeedInfo>> {
    Ok(tree
        .walk()
        .context("walk wiki tree")?
        .into_iter()
        .filter(|wiki| !wiki.meta.smart)
        .map(|wiki| WikiSeedInfo { wiki })
        .collect())
}

/// Match `queries` against the **page** cards, pushing one seed per match.
///
/// The cards are reader-relative
/// ([`build_reader_card`](crate::meta_annotate::build_reader_card)), so a page
/// the reader can read nothing on carries an empty card and matches no needle —
/// visibility falls out of the match itself, with no extra gate.
///
/// **There is no wiki-level step.** Until 2026-08-04 this matched the wiki's
/// topic union first and descended into its pages only on a hit. That gate
/// could never actually hide a page — the wiki union is built by unioning the
/// same per-fact topics, so any page match implies its wiki matches — but it
/// shaped the code as though the reader navigated containers, and the reader
/// does not know containers exist (founder's ruling; see [`navigate`]). Now it
/// walks pages.
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

/// True when a page may never be a navigation destination: the wiki's map
/// ([`is_root_page_path`]) or its channel-only policy page
/// ([`is_rules_page_path`]).
///
/// The one place outside the funnel that needs the same judgement is REM's
/// rail detector, which must not nominate a link to a page nobody can open.
#[must_use]
pub fn is_reserved_page_path(page: &Path) -> bool {
    is_root_page_path(page) || is_rules_page_path(page)
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
    /// Failsafe cap on the `[[wikilinks]]` harvested from **one** served
    /// identity card (see `navigate`'s `served_cards`).
    ///
    /// A failsafe, not a policy: the real bound is the tier — a `card` rail
    /// sorts below the whole fan, so it can only consume slack the content
    /// doors left. This exists because the number of rails on a card is not
    /// bounded by anything the funnel controls: it is whatever the compiler's
    /// link graph wired, and a person's card can reach every group they
    /// belong to. On the live corpus a person's card carries 3–6; a turn
    /// naming two other people serves three cards. `8` per card leaves that
    /// untouched and stops one over-wired card from being the whole tail.
    pub max_card_rails: usize,
    /// `max_tokens` for each navigator completion (the decision JSON is
    /// small; this is a cost guard, not a quality knob).
    pub decision_max_tokens: u32,
}

/// What the caller has **already put in front of the consumer** by another
/// route, and which the funnel must therefore not spend a page open on.
///
/// The two fields name the *same* pages by two different keys, on purpose:
/// one closes the page against re-reading, the other keeps what is written on
/// it usable. Splitting them into positional arguments made that read like a
/// coincidence.
#[derive(Debug, Default, Clone, Copy)]
pub struct Served<'a> {
    /// `(wiki_id, page)` — entered as already visited: never offered as a
    /// candidate, never opened, never charged to the budget.
    pub pages: &'a [(String, PathBuf)],
    /// `(wiki_id, projected markdown)` of the identity cards among them — the
    /// one thing rescued from a served page, its `[[wikilinks]]`. A served
    /// page never reaches [`open_target`], which is the only place a rail is
    /// harvested, so without this its links reach the funnel by no route.
    pub cards: &'a [(String, String)],
}

impl Default for NavigatorPolicy {
    fn default() -> Self {
        Self {
            max_hops: 2,
            pages_per_hop: 3,
            char_budget: 8_000,
            max_candidates: 16,
            max_card_rails: 8,
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
    /// `true` when the budget cut THIS page short.
    ///
    /// Per fragment, not per run, because that is the granularity the reader
    /// needs: the outcome-wide [`NavigationOutcome::truncated`] says a page
    /// somewhere was cut, and the consumer is handed the pages one after
    /// another with no way to tell which. A list page arriving cut at an
    /// arbitrary character reads as a complete list — the failure the
    /// founder's 2026-08-09 rule names for the list inventory, here on the
    /// page itself.
    pub truncated: bool,
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
    /// New candidates this page exposed (its `[[wikilink]]` targets).
    pub discovered: usize,
}

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
    /// Display label of how it surfaced (`rag`, `topic`, `situational`,
    /// `link`, `card`) — the tiers [`Candidate::prune_tier`] ranks by.
    origin: &'static str,
    /// The page's one-line card, filled by [`fill_summaries`] **after**
    /// [`prune_pool`] — never by the gatherers. It is the only part of a
    /// candidate that costs a page read, and the fan that produces
    /// candidates is unbounded (every reader-visible page whose card topics
    /// match a classified topic seeds one) while the pool is cut to
    /// `max_candidates`. Reading it eagerly meant paying a read + a YAML
    /// parse for every page the cut was about to discard, and that count
    /// grows with the memory.
    summary: Option<String>,
    /// Whether [`fill_summaries`] has already looked. Distinguishes "not
    /// read yet" from "read, and this page has no card" — without it a
    /// card-less page is re-read on every hop it survives.
    summary_read: bool,
    keywords: Vec<String>,
}

/// The demoted tail: the tier [`Candidate::prune_tier`] assigns anything it
/// does not recognise.
///
/// **Nothing produces it today** — every origin the funnel emits is matched
/// above — and that is the design, not an oversight. It is the fail-safe for
/// an origin added elsewhere without updating the match: such a candidate
/// lands behind every content-derived door instead of silently jumping the
/// fan. It stops being empty the moment someone adds a channel and forgets
/// this file, which is exactly when it is worth having.
const UNKNOWN_TIER: u8 = 3;

impl Candidate {
    /// Ranking tier for [`prune_pool`] — lower sorts first. A wikilink rail
    /// off collected prose beats the entry-point fan (`rag` | `topic` |
    /// `situational`), which beats a rail off a **served** card. Anything not
    /// recognised falls to [`UNKNOWN_TIER`], so an origin added elsewhere
    /// without updating this match fails safe into the demoted tail rather
    /// than silently jumping the fan.
    ///
    /// Three tiers, and they are the three ways a page can be reached at all:
    /// a fact hit put its page in the fan, somebody wrote a `[[wikilink]]` to
    /// it, or its own card matched. Nothing offers a page for merely sitting
    /// in the same folder as one that was opened.
    ///
    /// **Why `card` sits below the fan.** The two link tiers differ in what
    /// they are evidence *of*. A `link` rail was written on a page the
    /// navigator chose to open for this turn, so it carries the turn's own
    /// judgement twice over. A `card` rail was written on the identity card,
    /// which arrives unconditionally on every turn — it is evidence about the
    /// *person*, never about the question. Ranked with the rails it would put
    /// the sender's whole neighbourhood ahead of the pages the question
    /// actually found, which is the shape of the regression
    /// [69b](../../planning/69_identity-seed-family.md) removed (identity
    /// pages took 79 % of first opens). Below the fan it can only ever
    /// consume slack the content doors left.
    fn prune_tier(&self) -> u8 {
        match self.origin {
            "link" => 0,
            "rag" | "topic" | "situational" => 1,
            "card" => 2,
            _ => UNKNOWN_TIER,
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
/// LLM slot. Candidates grow as pages are opened: the pages reachable via
/// `[[wikilinks]]` from the collected prose are offered on the next hop,
/// carrying their own testata cards. That is the only way the pool grows —
/// a page is never offered for merely sitting in the same folder as one that
/// was opened, so an authored link is the load-bearing structure here.
///
/// [`Served::pages`] names pages whose prose the **caller has already put in
/// front of the consumer by another route**, as `(wiki_id, page)`. They enter
/// the funnel as if it had opened them: never offered as a candidate, never
/// opened, never charged to the budget. Re-reading them would spend the
/// turn's scarcest resource — a page open — on text that is already there.
///
/// [`Served::cards`] names the same pages a second time, as
/// `(wiki_id, projected card markdown)`, and is the **only** thing rescued
/// from them: a served page never passes through [`open_target`], which is
/// the sole place `[[wikilinks]]` are harvested, so the card's own rails
/// reached the funnel through no route at all. They are offered as `card`
/// candidates — the destination's page card attached like any other rail,
/// ranked below the whole fan (see [`Candidate::prune_tier`]), capped at
/// [`NavigatorPolicy::max_card_rails`] per card. The text must be the
/// **projected** card, so a link inside a region this reader may not see is
/// already gone before it can become a door.
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
    served: Served<'_>,
) -> Result<NavigationOutcome> {
    let mut outcome = NavigationOutcome::default();
    if entry_points.is_empty() {
        return Ok(outcome);
    }

    // Reader-relative card for the prompt-facing surface (the candidate page
    // cards): topics the sender can read, descriptions gated to the wiki's
    // default visibility — never the owner-tier `.md`. Wiki-keyed because that
    // is how the ACL is derived, not because anything renders a wiki.
    let reader_card =
        meta_annotate::build_reader_card(pool, tree, &sender.sender_id, &sender.sender_groups)
            .await
            .context("build reader card")?;

    let wikis = tree.walk().context("walk wiki tree")?;
    // Smart wikis are excluded from the navigable graph (see
    // `gather_entry_points`): no cards / wikilinks / per-fragment ACL to hop
    // through. They never appear as a candidate or a link target.
    let by_id: BTreeMap<&str, &DiscoveredWiki> = wikis
        .iter()
        .filter(|d| !d.meta.smart)
        .map(|d| (d.meta.wiki_id.as_str(), d))
        .collect();
    // No wiki catalog is built. Until 2026-08-04 every hop carried a ROOT
    // INDEX — one line per visible wiki with its `_meta` abstract and its
    // whole topic union, ~13.5k characters on the live corpus, ~27k a turn
    // across two hops. Founder's ruling: *«chi legge non ha bisogno di sapere
    // quali sono le wiki, arriva direttamente sui fatti, e da lì legge tramite
    // i link le pagine collegate»*. The structure is the WRITE side's
    // instrument — it tells the filer where a fact goes and which pages to
    // link — and the read side is meant to arrive on a page and follow rails.
    // A catalog of containers answers a question the reader never asks, and
    // the day before this it had to be labelled "orientation, not doors"
    // because the navigator kept trying to open its entries.
    let system = prompts::render(
        "navigator",
        tree.workdir(),
        BUNDLED_NAVIGATOR_PROMPT_MD,
        &[("page_budget", policy.pages_per_hop.to_string().as_str())],
    )
    .context("load navigator prompt")?;

    let max_hops = policy.max_hops.min(MULTI_HOP_HARD_LIMIT);
    let mut candidates = initial_pool(entry_points, &by_id, &reader_card);
    candidates.extend(card_rail_candidates(
        served.cards,
        &by_id,
        &reader_card,
        policy.max_card_rails,
    ));
    let mut state = FunnelState {
        // Pages the caller already delivered start out **visited**: that one
        // set is what `prune_pool` filters the offer by and what `open_target`
        // refuses on, so a single line makes the guarantee hold on every route
        // into the funnel — the fan or a `[[wikilink]]` — instead of two
        // filters that have to agree.
        visited: served.pages.iter().cloned().collect(),
        acl_defaults: BTreeMap::new(),
        remaining: policy.char_budget,
    };

    // Overwritten by every earlier exit; reaching the loop's natural end
    // means the depth dial ran out.
    outcome.stop = NavStop::HopCap;
    for _ in 0..max_hops {
        prune_pool(&mut candidates, &state.visited, policy.max_candidates);
        // Only now, and only for the survivors: the card is the one field
        // that costs a page read, and the fan above is unbounded.
        fill_summaries(pool, tree, &mut candidates, &by_id, &reader_card).await;
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
                // `open_target` reaches this arm only after pushing exactly
                // one fragment — every other path returns `None` — so the
                // page just read is the one at `fragments_before`, and the
                // operator record can never attribute it to the previous
                // page.
                let frag = &outcome.fragments[fragments_before];
                hop.opened.push(OpenedPage {
                    wiki_id: frag.wiki_id.clone(),
                    page: frag.page.to_string_lossy().into_owned(),
                    chars: frag.text.len(),
                    excerpt: excerpt_of(&frag.text, TRACE_EXCERPT_CAP),
                    discovered: found.len(),
                });
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
/// opened (resolved paths), the per-wiki resolved `acl_default` cache, and
/// the character budget still spendable.
struct FunnelState {
    visited: BTreeSet<(String, PathBuf)>,
    acl_defaults: BTreeMap<String, Principal>,
    remaining: usize,
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
    discoveries.extend(linked_wiki_candidates(&text, d, by_id, reader_card));
    outcome.fragments.push(NavigatedFragment {
        wiki_id: cand.wiki_id.clone(),
        page,
        text,
        truncated: cut,
    });
    Ok(Some(discoveries))
}

/// Turn the entry-point fan into the hop-0 candidate pool, each entry
/// enriched with its wiki card. A fan entry whose wiki vanished from the
/// tree (raced rename) is silently dropped.
/// The `[[wikilinks]]` written on the identity cards the caller already
/// served, as candidates.
///
/// A served card is inserted into [`FunnelState::visited`] so the funnel
/// never re-reads prose the block already holds — but `visited` also means
/// the page never reaches [`open_target`], the one place a rail is harvested.
/// The links were therefore dropped by omission, not by decision: the
/// [`Served::pages`] contract says "never offered, never opened, never
/// charged" and says nothing about what is written on the page.
///
/// This matters more since 69c gave the card a ~1800-character ceiling —
/// the ceiling's whole point is that what does not fit moves onto **linked**
/// pages, so a card with no live rails is a card that has hidden its own
/// detail. The founder's example is the test: a card reading *«celiaca»*
/// links to the food page, and that link should be a door.
fn card_rail_candidates(
    served_cards: &[(String, String)],
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    reader_card: &meta_annotate::ReaderCard,
    max_per_card: usize,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for (wiki_id, text) in served_cards {
        let Some(origin) = by_id.get(wiki_id.as_str()) else {
            continue;
        };
        let mut rails = linked_wiki_candidates(text, origin, by_id, reader_card);
        rails.truncate(max_per_card);
        for r in &mut rails {
            r.origin = "card";
        }
        out.extend(rails);
    }
    out
}

fn initial_pool(
    entry_points: &[EntryPoint],
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    reader_card: &meta_annotate::ReaderCard,
) -> Vec<Candidate> {
    entry_points
        .iter()
        .filter_map(|ep| {
            by_id.get(ep.wiki_id.as_str()).map(|d| {
                // The **destination page's** card, never its wiki's. Every
                // seed has named a page since 63 §8, and the wiki-level card
                // is what the fan used when a seed could still name a wiki
                // alone: it describes the subject, not the page, so N hits in
                // one wiki reached the navigator as N candidates carrying the
                // *same* sentence and the *same* keyword union, separable
                // only by their file name. The card is the sole input to
                // every choice the funnel makes, so that was the fan handing
                // it a constant.
                Candidate {
                    wiki_id: ep.wiki_id.clone(),
                    page: ep.page.clone(),
                    origin: ep.origin.label(),
                    summary: None,
                    summary_read: false,
                    keywords: reader_page_keywords(d, &ep.page, reader_card),
                }
            })
        })
        .collect()
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

/// Drop visited / duplicate candidates, stably rank the survivors by tier,
/// then cap the pool for the next prompt.
///
/// A page reaches the pool by exactly three routes, and they are not of equal
/// value. A **wikilink rail** is a destination found in the prose the
/// navigator has just read — an authored assertion that two pages belong
/// together, and the design's only expansion mechanism. The **entry-point
/// fan** (`rag` | `topic` | `situational`) are the doors the turn's own
/// content found. A **card rail** is a link written on a served identity
/// card, which arrives on every turn whatever was asked.
///
/// Truncating positionally would let whichever producer happened to run last
/// crowd out the others, so [`Candidate::prune_tier`] partitions the pool
/// stably first: rails off collected prose, then the fan **in the order the
/// gatherer already weighed it** — untouched here, because from hop 1 on
/// these are seeds the navigator was already offered and did not choose,
/// whereas a freshly discovered link is a rail straight out of the page it
/// just read — then card rails. At hop 0 there are no links yet, so the
/// ordering only bites from hop 1 on.
///
/// **The ranking runs before the dedup, and that order is the point.** One
/// page routinely reaches the pool by more than one route at once, and the
/// copies carry different tiers. Deduplicating first keeps whichever copy the
/// producers happened to emit earliest; ranking first makes the survivor the
/// **best** route instead of the earliest one — the same rule
/// [`dedup_and_sort`] already applies to the fan, where the heaviest seed wins
/// a collision.
///
/// **There is no directory listing**: nothing offers a page for merely sitting
/// in the same folder as one that was opened. Reachability rests entirely on
/// the content-derived channels — a fact hit, a topic or situational match on
/// the page's own card, and an authored `[[wikilink]]` — which is what makes
/// the page cards and the authored link graph load-bearing rather than
/// decorative.
fn prune_pool(pool: &mut Vec<Candidate>, visited: &BTreeSet<(String, PathBuf)>, cap: usize) {
    // Stable, so within a tier the producers' order still stands.
    pool.sort_by_key(Candidate::prune_tier);
    let mut seen: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    pool.retain(|c| {
        let key = (c.wiki_id.clone(), c.page.clone());
        !visited.contains(&key) && seen.insert(key)
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
                    let keywords = reader_page_keywords(target, &rel, reader_card);
                    out.push(Candidate {
                        wiki_id: target.meta.wiki_id.as_str().to_owned(),
                        page: rel,
                        origin: "link",
                        summary: None,
                        summary_read: false,
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
            let keywords = reader_page_keywords(d, &resolved, reader_card);
            out.push(Candidate {
                wiki_id: link.wiki_id,
                page: resolved,
                origin: "link",
                summary: None,
                summary_read: false,
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

/// The reader-visible page topics of one page — the single-page counterpart
/// of the entry fan's card logic.
///
/// Free: they come from the prebuilt [`meta_annotate::ReaderCard`], which is
/// derived from `fact_index` once per turn, so every gatherer may attach them
/// eagerly. The page's *description* is the half that costs a read, and it is
/// filled later by [`fill_summaries`].
fn reader_page_keywords(
    d: &DiscoveredWiki,
    rel: &Path,
    reader_card: &meta_annotate::ReaderCard,
) -> Vec<String> {
    let wiki_id = d.meta.wiki_id.as_str();
    reader_card
        .pages(wiki_id)
        .and_then(|pages| {
            pages
                .iter()
                .find(|(source_path, _)| {
                    page_within(&d.rel_dir, source_path).is_some_and(|p| p == rel)
                })
                .map(|(_, topics)| topics.clone())
        })
        .unwrap_or_default()
}

/// Read the one-line card of every candidate that survived the cut, once.
///
/// Called right after [`prune_pool`], and that order is the whole point: the
/// gatherers produce as many candidates as the memory has matching pages,
/// the pool keeps `max_candidates`, and the description is the only field
/// that costs a filesystem read and a YAML parse. Filling it at gather time
/// paid that price for every discarded page — a bill that grows with the
/// memory while the number of pages actually shown to the model does not.
///
/// `page_card` is consulted first and its stamp checked against the file
/// ([`page_card::PageCardRow::matches_file`]), so the usual case costs a
/// `stat`. Anything else — no row, a stale stamp, a DB that will not answer —
/// falls back to opening the page, which is never wrong; the table is a
/// cache and an empty one degrades to exactly the previous behaviour.
///
/// The description is read from the **owner-tier** testata but shown only
/// where the reader is inside the wiki's default visibility
/// (`summary_visible`) — the same gate as before, moved, not relaxed. A page
/// whose card cannot be read (vanished, unparseable) is marked read with no
/// summary: a missing card is a candidate with no abstract, never an error.
async fn fill_summaries(
    db: &SqlitePool,
    tree: &WikiTree,
    pool: &mut [Candidate],
    by_id: &BTreeMap<&str, &DiscoveredWiki>,
    reader_card: &meta_annotate::ReaderCard,
) {
    for c in pool.iter_mut().filter(|c| !c.summary_read) {
        c.summary_read = true;
        let Some(d) = by_id.get(c.wiki_id.as_str()) else {
            continue;
        };
        if !reader_card.summary_visible(c.wiki_id.as_str()) {
            continue;
        }
        let abs = d.abs_dir.join(&c.page);
        let source_path = wiki::workdir_relative_source_path(tree.workdir(), &abs);
        if let Ok(Some(row)) = page_card::get(db, &source_path).await
            && row.matches_file(&abs)
        {
            c.summary = row.description;
            continue;
        }
        c.summary = meta_annotate::read_page_card(&abs)
            .unwrap_or_default()
            .description;
    }
}

/// Assemble the per-hop user prompt: the turn, the budget line, the root
/// index, the collected prose, and the numbered candidate list.
#[allow(clippy::too_many_arguments, reason = "one-shot prompt assembly")]
fn build_user_prompt(
    turn_text: &str,
    sender: &SenderContext,
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
    out.push_str("\nCOLLECTED:\n");
    if fragments.is_empty() {
        out.push_str("(none yet)\n");
    } else {
        for f in fragments {
            let _ = writeln!(out, "=== {}/{} ===", f.wiki_id, f.page.display());
            out.push_str(&f.text);
            out.push('\n');
        }
    }
    out.push_str("\nCANDIDATES:\n");
    for (i, c) in pool.iter().enumerate() {
        // The address is one string, not a wiki and a page. The reader is
        // never told what a wiki is (founder, 2026-08-04: *«chi legge non ha
        // bisogno di sapere quali sono le wiki»*) — `wiki_id/page` is how a
        // page is spelled, the same way a file name carries its directory.
        let _ = write!(
            out,
            "{}. {}/{} | origin={}",
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
            summary_read: false,
            keywords: Vec::new(),
        }
    }

    #[test]
    fn one_page_reached_by_two_routes_keeps_the_better_route_not_the_first() {
        let visited = BTreeSet::new();
        // One destination reached by two surviving routes: a served identity
        // card's rail (lowest tier of the three) and an authored `[[wikilink]]`
        // on the page just read. `open_target` fills `discoveries` in its own
        // order, so the weaker copy can arrive first — deduplicating before
        // ranking would keep it and file the rail in the demoted tail.
        let mut pool = vec![
            cand("alice", "concerti.md", "card"),
            cand("alice", "concerti.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 16);
        assert_eq!(pool.len(), 1, "the two copies are one destination");
        assert_eq!(
            pool[0].origin, "link",
            "the surviving copy must carry the best route to the page, not the earliest one"
        );
    }

    #[test]
    fn prune_pool_keeps_the_highest_tier_entries_not_the_alphabetically_first_ones() {
        let visited = BTreeSet::new();
        // Insertion order is deliberately the reverse of tier order: the
        // first four entries are card rails (the weakest claim on a door) and
        // the last two are the strongest. A positional `truncate` would keep
        // exactly the wrong two; tier order must win instead.
        let mut pool = vec![
            cand("a-card", "index_stand_in.md", "card"),
            cand("b-card", "index_stand_in.md", "card"),
            cand("c-card", "index_stand_in.md", "card"),
            cand("d-card", "index_stand_in.md", "card"),
            cand("e-rag", "index_stand_in.md", "rag"),
            cand("f-link", "index_stand_in.md", "link"),
        ];
        prune_pool(&mut pool, &visited, 2);
        assert_eq!(
            pool.iter().map(|c| c.wiki_id.as_str()).collect::<Vec<_>>(),
            vec!["f-link", "e-rag"],
            "the cap must keep the rail and the fan seed, not the first-inserted card rails"
        );
    }

    #[test]
    fn prune_pool_preserves_the_gatherers_relative_order_inside_the_fan_tier() {
        let visited = BTreeSet::new();
        // principal / rag / topic / situational, in this order, is the
        // gatherer's own weight order (`dedup_and_sort`) — pruning must
        // carry it through unchanged, never re-sort or re-weight it.
        let mut pool = vec![
            cand("r", "index_stand_in.md", "rag"),
            cand("t", "index_stand_in.md", "topic"),
            cand("s", "index_stand_in.md", "situational"),
        ];
        prune_pool(&mut pool, &visited, 16);
        assert_eq!(
            pool.iter().map(|c| c.origin).collect::<Vec<_>>(),
            vec!["rag", "topic", "situational"],
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

    /// Build the two arguments every pool-shaping helper needs, off a real tree.
    async fn pool_inputs(
        tree: &WikiTree,
        sender: &str,
        readable_in: &[(&str, &str)],
    ) -> (Vec<DiscoveredWiki>, meta_annotate::ReaderCard, SqlitePool) {
        let pool = make_pool().await;
        // A destination is only offered when the reader can read something in
        // its wiki, so a link test needs at least one globally-readable fact
        // per wiki it expects to reach.
        for (n, (wiki, page)) in readable_in.iter().enumerate() {
            seed_fact(
                &pool,
                &fid(u8::try_from(n).expect("small fixture") + 1),
                wiki,
                &format!("wikis/{wiki}/{page}"),
                Principal::global(),
                &[],
            )
            .await;
        }
        let reader = meta_annotate::build_reader_card(&pool, tree, sender, &[])
            .await
            .expect("reader card");
        (tree.walk().expect("walk"), reader, pool)
    }

    fn by_id_of(wikis: &[DiscoveredWiki]) -> BTreeMap<&str, &DiscoveredWiki> {
        wikis
            .iter()
            .filter(|d| !d.meta.smart)
            .map(|d| (d.meta.wiki_id.as_str(), d))
            .collect()
    }

    /// The fan describes each door by the **page** it names, not by the wiki
    /// it sits in. Before 2026-08-04 `initial_pool` reached for the wiki-level
    /// card, so two hits in one wiki arrived as two candidates carrying one
    /// sentence — and the card is the only thing the navigator gets to judge
    /// on. Inverting this test is what shows the defect: point both candidates
    /// at the same wiki summary and the two lines become indistinguishable.
    #[tokio::test]
    async fn two_doors_in_one_wiki_carry_their_own_cards_not_their_wikis() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(
            &tree,
            "alice",
            "cucina.md",
            "---\ntitle: \"Cucina\"\ndescription: \"gluten-free recipes and what to cook\"\n---\n\nprose\n",
        );
        write_page(
            &tree,
            "alice",
            "auto.md",
            "---\ntitle: \"Auto\"\ndescription: \"servicing and insurance for the car\"\n---\n\nprose\n",
        );
        let (wikis, reader, db) = pool_inputs(
            &tree,
            "alice",
            &[("alice", "cucina.md"), ("alice", "auto.md")],
        )
        .await;
        let mut pool = initial_pool(
            &[
                entry("alice", "cucina.md", EntryOrigin::Rag, 0.9),
                entry("alice", "auto.md", EntryOrigin::Rag, 0.8),
            ],
            &by_id_of(&wikis),
            &reader,
        );
        // The card is attached after the cut, so a pool straight from the fan
        // carries none yet — the funnel does the same, one step later.
        fill_summaries(&db, &tree, &mut pool, &by_id_of(&wikis), &reader).await;
        assert_eq!(pool.len(), 2);
        assert_eq!(
            pool[0].summary.as_deref(),
            Some("gluten-free recipes and what to cook"),
            "a door must be described by its own page card"
        );
        assert_eq!(
            pool[1].summary.as_deref(),
            Some("servicing and insurance for the car")
        );
        assert_ne!(
            pool[0].summary, pool[1].summary,
            "two pages of one wiki must not reach the navigator as the same card"
        );
    }

    /// A gatherer must leave the card unread until the pool has been cut.
    ///
    /// The fan is unbounded — every reader-visible page whose topics match a
    /// classified topic seeds a candidate — while the pool keeps
    /// `max_candidates`. A description fetched at gather time is therefore a
    /// page read and a YAML parse spent on a candidate that is about to be
    /// discarded, and the bill grows with the memory while the number of
    /// pages the model is shown does not.
    #[tokio::test]
    async fn the_fan_reads_no_page_card_until_the_pool_has_been_cut() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        for (page, desc) in [
            ("cucina.md", "gluten-free recipes and what to cook"),
            ("auto.md", "servicing and insurance for the car"),
        ] {
            write_page(
                &tree,
                "alice",
                page,
                &format!("---\ntitle: \"t\"\ndescription: \"{desc}\"\n---\n\nprose\n"),
            );
        }
        let (wikis, reader, db) = pool_inputs(
            &tree,
            "alice",
            &[("alice", "cucina.md"), ("alice", "auto.md")],
        )
        .await;
        let mut pool = initial_pool(
            &[
                entry("alice", "cucina.md", EntryOrigin::Rag, 0.9),
                entry("alice", "auto.md", EntryOrigin::Rag, 0.8),
            ],
            &by_id_of(&wikis),
            &reader,
        );
        assert!(
            pool.iter().all(|c| !c.summary_read && c.summary.is_none()),
            "the fan produced candidates without opening a single page"
        );
        prune_pool(&mut pool, &BTreeSet::new(), 1);
        fill_summaries(&db, &tree, &mut pool, &by_id_of(&wikis), &reader).await;
        assert_eq!(pool.len(), 1, "the cut kept one candidate");
        assert!(
            pool[0].summary.is_some(),
            "the survivor is the one page whose card was read"
        );
    }

    /// The stored card is used when its stamp still vouches for the file, and
    /// ignored the moment it does not.
    ///
    /// Both halves are the contract. Without the first the table is dead
    /// weight; without the second it is a way to show a reader a sentence the
    /// page stopped saying — which on this path is the *only* thing that
    /// decides whether the page gets opened at all. The row here carries a
    /// description the file does not, so there is no way to pass by accident.
    #[tokio::test]
    async fn a_stored_card_is_used_only_while_its_stamp_still_matches_the_file() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(
            &tree,
            "alice",
            "cucina.md",
            "---\ntitle: \"Cucina\"\ndescription: \"from the file\"\n---\n\nprose\n",
        );
        let (wikis, reader, db) = pool_inputs(&tree, "alice", &[("alice", "cucina.md")]).await;
        let abs = tree.workdir().join("wikis/alice/cucina.md");
        let (mtime, size) = page_card::file_stamp(&abs).expect("stamp");
        page_card::upsert(
            &db,
            &page_card::NewPageCard {
                source_path: "wikis/alice/cucina.md".to_owned(),
                wiki_id: "alice".to_owned(),
                description: Some("from the table".to_owned()),
                keywords: Vec::new(),
                style: None,
                file_mtime_ms: Some(mtime),
                file_size: Some(size),
            },
        )
        .await
        .expect("upsert");

        let mut pool = initial_pool(
            &[entry("alice", "cucina.md", EntryOrigin::Rag, 0.9)],
            &by_id_of(&wikis),
            &reader,
        );
        fill_summaries(&db, &tree, &mut pool, &by_id_of(&wikis), &reader).await;
        assert_eq!(
            pool[0].summary.as_deref(),
            Some("from the table"),
            "a vouched row spares the page read"
        );

        // Rewrite the page: the stamp no longer matches, so the stale row must
        // lose to what the page actually says now.
        write_page(
            &tree,
            "alice",
            "cucina.md",
            "---\ntitle: \"Cucina\"\ndescription: \"the page says something else now\"\n---\n\nprose\n",
        );
        let mut pool = initial_pool(
            &[entry("alice", "cucina.md", EntryOrigin::Rag, 0.9)],
            &by_id_of(&wikis),
            &reader,
        );
        fill_summaries(&db, &tree, &mut pool, &by_id_of(&wikis), &reader).await;
        assert_eq!(
            pool[0].summary.as_deref(),
            Some("the page says something else now"),
            "a stale row is never shown — the file is the truth"
        );
    }

    /// The identity card is served, never opened — so its `[[wikilinks]]` are
    /// the one part of the corpus the funnel could reach by no route at all
    /// (`visited` skips `open_target`, the only place rails are harvested).
    #[tokio::test]
    async fn a_served_card_contributes_its_rails_ranked_below_the_fan() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(
            &tree,
            "alice",
            "alimentazione.md",
            "---\ntitle: \"Alimentazione\"\ndescription: \"what she can and cannot eat\"\n---\n\nprose\n",
        );
        let (wikis, reader, db) =
            pool_inputs(&tree, "alice", &[("alice", "alimentazione.md")]).await;
        let mut rails = card_rail_candidates(
            &[(
                "alice".to_owned(),
                "She is coeliac — the detail lives on [[alice/alimentazione]].".to_owned(),
            )],
            &by_id_of(&wikis),
            &reader,
            8,
        );
        fill_summaries(&db, &tree, &mut rails, &by_id_of(&wikis), &reader).await;
        assert_eq!(rails.len(), 1, "the card's one rail must become a door");
        assert_eq!(rails[0].page, PathBuf::from("alimentazione.md"));
        assert_eq!(rails[0].origin, "card");
        assert_eq!(
            rails[0].summary.as_deref(),
            Some("what she can and cannot eat"),
            "a card rail carries the DESTINATION's card, like every other rail"
        );
        // The tier is the whole safety argument: a card arrives on every turn,
        // so its links say nothing about this question and must never outrank
        // the doors the question itself found.
        assert!(
            rails[0].prune_tier()
                > Candidate {
                    wiki_id: "alice".to_owned(),
                    page: PathBuf::from("cucina.md"),
                    origin: "rag",
                    summary: None,
                    summary_read: false,
                    keywords: Vec::new(),
                }
                .prune_tier(),
            "a card rail must sort below the entry fan"
        );
    }

    /// A card with more rails than the failsafe allows contributes only the cap.
    #[tokio::test]
    async fn one_over_wired_card_cannot_become_the_whole_tail() {
        use std::fmt::Write as _;

        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        let mut card = String::from("Links:");
        for n in 0..6 {
            write_page(
                &tree,
                "alice",
                &format!("p{n}.md"),
                &format!("---\ntitle: \"P{n}\"\ndescription: \"page {n}\"\n---\n\nprose\n"),
            );
            let _ = write!(card, " [[alice/p{n}]]");
        }
        let (wikis, reader, _db) = pool_inputs(&tree, "alice", &[("alice", "p0.md")]).await;
        let rails =
            card_rail_candidates(&[("alice".to_owned(), card)], &by_id_of(&wikis), &reader, 2);
        assert_eq!(rails.len(), 2, "the per-card failsafe must bind");
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
        // One hop is the whole walk here, and that is the shipped behaviour:
        // an opened page with no `[[wikilinks]]` exposes nothing, there is no
        // directory listing to stand in for one, and the wiki root is the map
        // rather than a page. What this test is about is the projection of the
        // page that WAS opened.
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"notes.md"}],"done":false,"note":"go"}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("mallory", &[]),
            "what do we know?",
            &[entry("alice", "notes.md", EntryOrigin::Topic, 0.8)],
            &NavigatorPolicy::default(),
            Served::default(),
        )
        .await
        .unwrap();

        assert_eq!(out.hops, 1);
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

        // The funnel journal mirrors the run: hop 1 offered the fan card and
        // opened the pick (note captured), and the walk then stopped because
        // the page it read exposed no further door — which is the ordinary
        // shape of a walk now that nothing offers a page nobody linked.
        assert_eq!(out.stop, NavStop::PoolExhausted);
        assert_eq!(out.trace.len(), 1);
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
            Served::default(),
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
            Served::default(),
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
    /// content, arrived at by reading a page rather than by choosing a wiki.
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
    /// navigator request for it, and a `[[wikilink]]` naming it.
    #[tokio::test]
    async fn navigate_never_opens_a_page_the_caller_already_served() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(&tree, "alice", "index.md", "# Alice\n\nHer whole card.\n");
        write_page(&tree, "alice", "notes.md", "Ordinary prose.\n");
        // Hop 1 asks for the wiki root — which resolves to `index.md` — and
        // for a real page beside it. Hop 2 asks for `index.md` by name, i.e.
        // the navigator naming the map verbatim: `open_target`'s gate is the
        // central fail-safe and this is the case that exercises it.
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
            Served {
                pages: &[("alice".to_owned(), PathBuf::from("index.md"))],
                cards: &[],
            },
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
            "nor is it ever offered — not from the fan, not from a rail"
        );
        assert!(
            !out.fragments.iter().any(|f| f.text.contains("whole card")),
            "and none of its prose reaches the caller a second time"
        );
    }

    /// Roadmap 41e — the reserved `rules.md` policy page is channel-only:
    /// no route offers it as a door, and even a navigator that asks for it
    /// verbatim is discarded by the `open_target` fail-safe.
    #[tokio::test]
    async fn navigate_never_offers_nor_opens_the_rules_page() {
        let (_dir, tree) = open_tree();
        forge_user(&tree, "alice");
        write_page(&tree, "alice", "rules.md", "# Rules\n\nStanding policy.\n");
        write_page(&tree, "alice", "notes.md", "Ordinary prose.\n");
        write_page(&tree, "alice", "rails.md", "Entry prose.\n");
        // The navigator asks for the rules page **verbatim**, which is the
        // case that matters: `open_target`'s gate is the central fail-safe and
        // refuses it whatever door the funnel found. The offer side is checked
        // below on the same walk — with the directory listing retired, nothing
        // can put a reserved page in front of the navigator by accident, so
        // the remaining risk is exactly a navigator that names one itself.
        let llm = ScriptedLlm::new(&[
            r#"{"open":[{"wiki_id":"alice","page":"rules.md"},{"wiki_id":"alice","page":"rails.md"}],"done":false}"#,
        ]);

        let out = navigate(
            &make_pool().await,
            &tree,
            &llm,
            &sender("alice", &[]),
            "what do we know?",
            &[entry("alice", "rails.md", EntryOrigin::Rag, 0.9)],
            &NavigatorPolicy::default(),
            Served::default(),
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
        // The control: the ordinary page named beside it in the same decision
        // IS opened, so the assertions above are about the reserved page and
        // not about a walk that did nothing.
        assert!(
            out.fragments
                .iter()
                .any(|f| f.page == Path::new("rails.md")),
            "the ordinary page named in the same decision must still be opened"
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
            Served::default(),
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
            Served::default(),
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
            Served::default(),
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
