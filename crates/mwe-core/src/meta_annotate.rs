// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compile-time navigation-annotation sync (wiki `_meta.md` + page testate).
//!
//! A deterministic, no-LLM compile sub-stage that derives navigation aids from
//! the fact index and writes them back into the wiki's card surfaces.
//! It is the producer side of the catalog / root-index data plane
//! ([`crate::wiki::CatalogEntry`], [`crate::wiki::render_root_index`]): the
//! catalog *carries* per-wiki `keywords`, and this stage *populates* them so the
//! catalog stops showing empty slots.
//!
//! It owns three navigation annotations:
//!
//! - **`_meta.keywords["topics"]`** — the sorted union of every active fact's
//!   `topics` in the wiki, comma-joined into a single scalar. Written by the
//!   deterministic [`sync_wiki_keywords`] pass that [`crate::dream::run_compile`]
//!   runs over the whole tree after the prose compile. This is exactly the shape
//!   the recall navigator's topic seeds match against
//!   ([`crate::recall_nav::gather_entry_points`] substring-scans the card
//!   keywords), so populating it gives the topic entry-point a populated source.
//! - **`_meta.extra["summary"]`** — the wiki's one-line **abstract**. Written by
//!   the compiler via [`sync_wiki_summary`] when it (re)compiles a wiki's
//!   `index.md` overview page, from the freshest one-liner available (the
//!   Cronista's `description` for a person wiki, the plan's description for a
//!   hub).
//! - **page testata `keywords["topics"]`** — the per-page card: the sorted union
//!   of the topics of the active facts *living on that page*
//!   (`fact_index.source_path`), written into each page's frontmatter by
//!   [`sync_page_keywords`]. The wiki-level union orients the hop *into* a wiki;
//!   the page-level entry orients the hop *inside* it, so the navigator decides
//!   from the card instead of reading whole pages.
//!
//! The `_meta` pair is what [`crate::wiki::flatten_keywords`] /
//! [`crate::wiki::CatalogEntry`] surface into the root index a recall navigator
//! reads to orient itself.
//!
//! The recall-navigation principle is to push intelligence to compile-time
//! (offline, strong model, not latency-critical) so the recall-time navigator
//! stays cheap. The remaining annotations (annotated `[[slug|hint]]` links, the
//! typed link graph) attach to the compiler's output later.
//! See the recall pipeline.
//!
//! Every write is **best-effort** (the caller logs failures, never fails the
//! dream — a missing annotation degrades recall, it does not corrupt the wiki)
//! and **idempotent** (a `_meta` already matching the computed value is left
//! untouched, so a steady-state compile rewrites nothing).
//!
//! The compile-time card those passes write is **owner-tier** (the
//! [`fact_at_default_visibility`] boundary) — the operator's Obsidian view. The
//! serve-time counterpart lives here too: [`build_reader_card`] recomputes the
//! card **per reader** from `fact_index` for the recall navigator, so a reader
//! never sees the topic of a fact they cannot read
//! ([card boundary](../../../docs/concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry)).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::fact_index::{self, FactFilters};
use crate::types::{Acl, Principal};
use crate::wiki::{self, WikiMeta, WikiTree};

/// Frontmatter `keywords` sub-key this stage owns.
const TOPICS_KEY: &str = "topics";
/// Separator used to join the topic set into the scalar keyword value.
const KEYWORD_JOIN: &str = ", ";
/// Frontmatter `extra` sub-key holding the wiki's one-line abstract — the same
/// key [`crate::wiki::meta_summary`] reads back into the catalog.
const SUMMARY_KEY: &str = "summary";

/// Sync every wiki's `_meta.keywords["topics"]` to the union of its facts'
/// topics, rewriting only the `_meta.md` files that actually change.
///
/// Walks the whole tree (the trees we target are small, and an unchanged wiki
/// costs one fact query and no write), so a wiki that gained topics through any
/// path — not just this compile's dirty pages — is backfilled. Returns the count
/// of `_meta.md` files rewritten; `0` means the annotations were already current.
///
/// # Errors
///
/// Surfaces tree-walk, fact-index query, and `_meta.md` read/parse/render/write
/// failures. The caller ([`crate::dream::run_compile`]) treats this as a
/// non-fatal pass and logs the error instead of propagating it.
pub async fn sync_wiki_keywords(pool: &SqlitePool, tree: &WikiTree) -> Result<usize> {
    let mut updated = 0usize;
    for d in tree.walk().context("walk wiki tree")? {
        let default = tree.resolve_scope_principal(&d.meta)?;
        let topics = collect_wiki_topics(pool, d.meta.wiki_id.as_str(), &default).await?;
        if sync_meta_topics(&d.abs_dir, &topics)? {
            updated += 1;
        }
    }
    Ok(updated)
}

/// The **ACL card boundary**: a fact contributes its topics to a card — the
/// wiki `_meta` keywords or a page's testata keywords — iff it sits at the
/// wiki's **default visibility**: `owner == global`, or `owner ==` the wiki's
/// resolved `acl_default` principal. An `allow=` list only *extends*
/// readability, so it never disqualifies a default-owned fact. Anything else
/// (a cross-user region, a group-owned region on a user wiki, …) is
/// special-cased content whose topic words must not surface on a card that is
/// readable at wiki level. See the boundary write-up in
/// [`identity-and-acl.md`](../../../docs/concepts/identity-and-acl.md).
fn fact_at_default_visibility(owner: &Principal, default: &Principal) -> bool {
    owner.is_global() || owner == default
}

/// Collect the sorted, de-duplicated union of every card-eligible active
/// fact's `topics` in a wiki (the [`fact_at_default_visibility`] boundary).
/// Blank topics are dropped; the [`BTreeSet`] gives a stable order
/// so the rendered keyword value is deterministic across runs.
async fn collect_wiki_topics(
    pool: &SqlitePool,
    wiki_id: &str,
    default: &Principal,
) -> Result<Vec<String>> {
    let rows = fact_index::find_by_filters(
        pool,
        &FactFilters {
            wiki_id: Some(wiki_id.to_owned()),
            ..FactFilters::default()
        },
    )
    .await
    .with_context(|| format!("query facts for wiki {wiki_id}"))?;
    let mut set: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        if !fact_at_default_visibility(&row.owner_id, default) {
            continue;
        }
        for topic in &row.topics {
            let trimmed = topic.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_owned());
            }
        }
    }
    Ok(set.into_iter().collect())
}

/// Sync a wiki's `_meta.keywords["topics"]`, rewriting only on a change.
fn sync_meta_topics(abs_dir: &Path, topics: &[String]) -> Result<bool> {
    mutate_meta(abs_dir, |meta| {
        apply_topics_keyword(&mut meta.keywords, topics)
    })
}

/// Sync every page's testata `keywords["topics"]` to the union of the topics
/// of the active facts living on that page, rewriting only the pages that
/// actually change.
///
/// Facts are grouped by `fact_index.source_path` (the workdir-relative page
/// path every write-path primitive stamps), so a page whose facts were moved
/// away by a REM split sheds its stale entry on the next compile, and a page
/// that gained facts through any path is backfilled. Pages without a
/// frontmatter testata are left untouched — the card is compiler output, not
/// something this pass invents. Returns the count of pages rewritten.
///
/// # Errors
///
/// Surfaces tree-walk, fact-index query, and page read/parse/render/write
/// failures. The caller ([`crate::dream::run_compile`]) treats this as a
/// non-fatal pass and logs the error instead of propagating it.
pub async fn sync_page_keywords(pool: &SqlitePool, tree: &WikiTree) -> Result<usize> {
    let mut updated = 0usize;
    for d in tree.walk().context("walk wiki tree")? {
        let default = tree.resolve_scope_principal(&d.meta)?;
        let by_page = collect_page_topics(pool, d.meta.wiki_id.as_str(), &default).await?;
        let handle = tree.locate(&d.meta.wiki_id)?;
        for page in handle
            .list_pages()
            .with_context(|| format!("list pages of {}", d.meta.wiki_id))?
        {
            let key = wiki::workdir_relative_source_path(tree.workdir(), &page.abs_path);
            let topics = by_page.get(&key).map_or(&[][..], Vec::as_slice);
            if sync_page_topics(&page.abs_path, topics)? {
                updated += 1;
            }
        }
    }
    Ok(updated)
}

/// Collect the sorted, de-duplicated union of card-eligible active-fact topics
/// per page of one wiki (the [`fact_at_default_visibility`] boundary), keyed by
/// the workdir-relative `source_path`.
async fn collect_page_topics(
    pool: &SqlitePool,
    wiki_id: &str,
    default: &Principal,
) -> Result<BTreeMap<String, Vec<String>>> {
    let rows = fact_index::find_by_filters(
        pool,
        &FactFilters {
            wiki_id: Some(wiki_id.to_owned()),
            ..FactFilters::default()
        },
    )
    .await
    .with_context(|| format!("query facts for wiki {wiki_id}"))?;
    let mut by_page: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &rows {
        if !fact_at_default_visibility(&row.owner_id, default) {
            continue;
        }
        let set = by_page.entry(row.source_path.clone()).or_default();
        for topic in &row.topics {
            let trimmed = topic.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_owned());
            }
        }
    }
    Ok(by_page
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect())
}

// ---------------------------------------------------------------------
// Reader-relative card (serve-time, navigator-facing).
//
// The passes above WRITE the owner-tier card into the `.md` at compile
// time (the `fact_at_default_visibility` boundary). At serve time the
// recall navigator must NOT hand that owner-tier card to a reader who
// cannot read the underlying facts — a reader denied a page's body facts
// could otherwise infer their themes from the card's topics. So the
// navigator rebuilds the card PER READER from `fact_index`: topics are the
// union over the facts the reader `can_read`, and the wiki abstract is
// gated to readers at the wiki's default visibility.
// See [the ACL card boundary](../../../docs/concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry).
// ---------------------------------------------------------------------

/// Per-reader projection of every wiki's card, recomputed from `fact_index`.
///
/// Consumed by [`crate::recall_nav`] to keep the topic seeds, the candidate
/// cards, and the root-index reader-relative. Built once per navigator call
/// by [`build_reader_card`].
pub struct ReaderCard {
    /// `wiki_id` → the reader-visible topic union (original case, sorted).
    wiki_topics: BTreeMap<String, Vec<String>>,
    /// `wiki_id` → (`source_path` → the reader-visible page-topic union).
    page_topics: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// `wiki_id`s whose one-line abstract / page descriptions the reader may
    /// see (their read-set covers the wiki's default visibility).
    summary_wikis: BTreeSet<String>,
    /// `wiki_id`s in which the reader can read ≥ 1 fact — the derived-visibility
    /// signal (a wiki is reachable iff it holds a fact the reader `can_read`).
    /// Unlike [`Self::wiki_topics`], this counts a readable fact even when it
    /// carries no `topics`, so a fact-bearing-but-topic-less wiki still surfaces.
    readable_wikis: BTreeSet<String>,
}

impl ReaderCard {
    /// The reader-visible topic union for a wiki — empty when the reader can
    /// read no card-bearing fact there.
    #[must_use]
    pub fn wiki_topics(&self, wiki_id: &str) -> &[String] {
        self.wiki_topics.get(wiki_id).map_or(&[][..], Vec::as_slice)
    }

    /// The whole `wiki_id` → topics map (for the catalog / root-index render).
    #[must_use]
    pub const fn wiki_topics_map(&self) -> &BTreeMap<String, Vec<String>> {
        &self.wiki_topics
    }

    /// The reader-visible page-topic map for a wiki, keyed by workdir-relative
    /// `source_path` — `None` when the wiki has no reader-visible page facts.
    #[must_use]
    pub fn pages(&self, wiki_id: &str) -> Option<&BTreeMap<String, Vec<String>>> {
        self.page_topics.get(wiki_id)
    }

    /// Whether the reader may see this wiki's one-line abstract / page
    /// descriptions.
    #[must_use]
    pub fn summary_visible(&self, wiki_id: &str) -> bool {
        self.summary_wikis.contains(wiki_id)
    }

    /// The set of `wiki_id`s whose abstract the reader may see (for the
    /// catalog / root-index render).
    #[must_use]
    pub const fn summary_wikis(&self) -> &BTreeSet<String> {
        &self.summary_wikis
    }

    /// Whether the reader can read ≥ 1 fact in this wiki — the derived-
    /// visibility predicate. True even when the readable facts carry no
    /// `topics` (so it is broader than a non-empty [`Self::wiki_topics`]).
    #[must_use]
    pub fn reader_can_read_in(&self, wiki_id: &str) -> bool {
        self.readable_wikis.contains(wiki_id)
    }

    /// The whole set of `wiki_id`s the reader can read ≥ 1 fact in (for the
    /// catalog's derived-visibility filter).
    #[must_use]
    pub const fn readable_wikis(&self) -> &BTreeSet<String> {
        &self.readable_wikis
    }
}

/// The **reader-relative** card boundary — the serve-time counterpart of
/// [`fact_at_default_visibility`]: a fact contributes its topics to the card
/// rendered FOR `reader` iff the reader can read it (the same
/// owner ∪ allow ∪ sender predicate the redaction path applies via
/// [`crate::acl::can_read`]).
fn fact_readable_by(
    row: &fact_index::CardAclRow,
    reader_id: &str,
    reader_groups: &[String],
) -> bool {
    let acl = Acl {
        owner: Some(row.owner_id.clone()),
        allow: row.allow_ids.clone(),
    };
    crate::acl::can_read(&acl, reader_id, reader_groups, row.sender_id.as_ref())
}

/// Build the [`ReaderCard`] for one reader: the navigator's serve-time,
/// per-reader view of every wiki's card.
///
/// Topics are recomputed from `fact_index` (the union over the reader's
/// readable facts, the [`fact_readable_by`] boundary), NOT read from the
/// owner-tier `.md` — so a reader never sees the topic of a fact they cannot
/// read, while every topic they CAN read still surfaces (filter, not drop, so
/// recall does not silently narrow). The abstract is gated separately: a
/// reader sees a wiki's `summary` / page `description` only when their read-set
/// covers the wiki's default visibility, i.e. they match the resolved
/// `acl_default` — cheap, no fact query.
///
/// # Errors
///
/// Surfaces the fact-index query and the tree-walk / `acl_default` resolution.
pub async fn build_reader_card(
    pool: &SqlitePool,
    tree: &WikiTree,
    reader_id: &str,
    reader_groups: &[String],
) -> Result<ReaderCard> {
    let rows = fact_index::active_card_acl_rows(pool)
        .await
        .context("query active card rows")?;

    let mut wiki_sets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut page_sets: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    let mut readable_wikis: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        if !fact_readable_by(row, reader_id, reader_groups) {
            continue;
        }
        // Derived-visibility signal: any readable fact makes its wiki reachable,
        // independent of whether the fact carries topics (the topic union below
        // is the narrower recall-matching card).
        readable_wikis.insert(row.wiki_id.clone());
        let topics: Vec<&str> = row
            .topics
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if topics.is_empty() {
            continue;
        }
        let wiki_set = wiki_sets.entry(row.wiki_id.clone()).or_default();
        for t in &topics {
            wiki_set.insert((*t).to_owned());
        }
        let page_set = page_sets
            .entry(row.wiki_id.clone())
            .or_default()
            .entry(row.source_path.clone())
            .or_default();
        for t in &topics {
            page_set.insert((*t).to_owned());
        }
    }

    let wiki_topics = wiki_sets
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect();
    let page_topics = page_sets
        .into_iter()
        .map(|(wiki, pages)| {
            let pages = pages
                .into_iter()
                .map(|(p, v)| (p, v.into_iter().collect()))
                .collect();
            (wiki, pages)
        })
        .collect();

    let mut summary_wikis = BTreeSet::new();
    for d in tree.walk().context("walk wiki tree")? {
        let default = tree.resolve_scope_principal(&d.meta)?;
        if crate::acl::principal_matches(&default, reader_id, reader_groups) {
            summary_wikis.insert(d.meta.wiki_id.as_str().to_owned());
        }
    }

    Ok(ReaderCard {
        wiki_topics,
        page_topics,
        summary_wikis,
        readable_wikis,
    })
}

/// Sync one page's testata `keywords["topics"]`, rewriting only on a change.
///
/// The page body is preserved verbatim; sibling frontmatter fields and sibling
/// `keywords` entries survive (the frontmatter mapping is re-serialized, so
/// purely cosmetic YAML quoting may normalize on the first rewrite). A page
/// with no frontmatter is skipped.
fn sync_page_topics(abs_path: &Path, topics: &[String]) -> Result<bool> {
    let raw = std::fs::read_to_string(abs_path)
        .with_context(|| format!("read {}", abs_path.display()))?;
    let Some(doc) = wiki::MarkdownDoc::parse(&raw) else {
        return Ok(false);
    };
    let mut fm: serde_yaml::Mapping = if doc.frontmatter.trim().is_empty() {
        serde_yaml::Mapping::new()
    } else {
        serde_yaml::from_str(&doc.frontmatter)
            .with_context(|| format!("parse frontmatter of {}", abs_path.display()))?
    };
    let kw_key = serde_yaml::Value::String("keywords".to_owned());
    let mut kw = match fm.get(&kw_key) {
        Some(serde_yaml::Value::Mapping(m)) => m.clone(),
        _ => serde_yaml::Mapping::new(),
    };
    if !apply_topics_keyword(&mut kw, topics) {
        return Ok(false);
    }
    if kw.is_empty() {
        fm.remove(&kw_key);
    } else {
        fm.insert(kw_key, serde_yaml::Value::Mapping(kw));
    }
    let yaml = if fm.is_empty() {
        String::new()
    } else {
        serde_yaml::to_string(&fm)
            .with_context(|| format!("render frontmatter of {}", abs_path.display()))?
    };
    let rendered = format!("---\n{yaml}---\n{}", doc.body);
    wiki::atomic_write(abs_path, rendered.as_bytes())
        .with_context(|| format!("write {}", abs_path.display()))?;
    Ok(true)
}

/// A page's **card** as recall navigation reads it: the testata
/// `description` one-liner plus the `keywords` mapping flattened to the same
/// `key=value` search strings the catalog exposes for wiki cards
/// ([`crate::wiki::flatten_keywords_mapping`]), so card matching is uniform
/// across both levels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PageCard {
    /// The page's «what goes in here» one-liner, when the testata has one.
    pub description: Option<String>,
    /// Flattened `keywords` entries (`topics=food, wine`, …).
    pub keywords: Vec<String>,
}

/// Read a page's testata card — the read side of [`sync_page_keywords`].
/// The recall-navigation entry-point gatherer matches classified topics
/// against [`PageCard::keywords`]; the navigator funnel shows the whole card
/// when offering the page as a hop destination.
///
/// A page without frontmatter, with unparseable frontmatter, or without the
/// relevant fields yields an empty card — a card that cannot be read is a
/// card that matches nothing, never an error that kills recall.
///
/// # Errors
///
/// Only the page **read** itself surfaces (the caller decides whether a
/// missing file is tolerable); content-level defects degrade to empty.
pub(crate) fn read_page_card(abs_path: &Path) -> Result<PageCard> {
    let raw = std::fs::read_to_string(abs_path)
        .with_context(|| format!("read {}", abs_path.display()))?;
    let Some(doc) = wiki::MarkdownDoc::parse(&raw) else {
        return Ok(PageCard::default());
    };
    let Ok(fm) = serde_yaml::from_str::<serde_yaml::Mapping>(&doc.frontmatter) else {
        return Ok(PageCard::default());
    };
    let description = fm
        .get("description")
        .and_then(serde_yaml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let keywords = match fm.get("keywords") {
        Some(serde_yaml::Value::Mapping(kw)) => wiki::flatten_keywords_mapping(kw),
        _ => Vec::new(),
    };
    Ok(PageCard {
        description,
        keywords,
    })
}

/// Read a leaf page's testata `description` (its «what goes in here» one-liner).
///
/// `None` when the page has no frontmatter, no `description`, or an unparseable
/// testata. The read side of [`set_page_description`]; the dashboard's
/// page-description editor renders this into its form.
///
/// # Errors
///
/// Only the page **read** itself surfaces; content-level defects degrade to
/// `None` (a card that cannot be read matches nothing, never an error).
pub fn read_page_description(abs_path: &Path) -> Result<Option<String>> {
    Ok(read_page_card(abs_path)?.description)
}

/// Set / update / clear a leaf page's testata `description` («what goes in here»).
///
/// Preserves the page **body** and every sibling frontmatter field
/// (`keywords`, `style`, …). A blank `description` clears the key. A page with
/// no testata yet gains a minimal frontmatter so an operator can annotate a
/// hand-written page (a bare page + blank description stays a no-op). Rewrites
/// only on an actual change, and atomically.
///
/// Unlike the compiler-authored `keywords["topics"]` card
/// ([`sync_page_topics`]), the `description` is the one testata field meant to
/// be **hand-authored**: the REM / compiler passes preserve sibling fields, so
/// a dashboard edit survives later recompiles.
///
/// Returns whether a write happened.
///
/// # Errors
///
/// Surfaces page read / frontmatter parse / render / write failures.
pub fn set_page_description(abs_path: &Path, description: &str) -> Result<bool> {
    let raw = std::fs::read_to_string(abs_path)
        .with_context(|| format!("read {}", abs_path.display()))?;
    // Split testata from body. A page with no frontmatter fence keeps its
    // whole content as the body and gains a fresh (initially empty) testata.
    let (mut fm, body) = match wiki::MarkdownDoc::parse(&raw) {
        Some(doc) => {
            let fm: serde_yaml::Mapping = if doc.frontmatter.trim().is_empty() {
                serde_yaml::Mapping::new()
            } else {
                serde_yaml::from_str(&doc.frontmatter)
                    .with_context(|| format!("parse frontmatter of {}", abs_path.display()))?
            };
            (fm, doc.body)
        },
        None => (serde_yaml::Mapping::new(), raw),
    };
    let key = serde_yaml::Value::String("description".to_owned());
    let trimmed = description.trim();
    let changed = if trimmed.is_empty() {
        fm.remove(&key).is_some()
    } else {
        let new_val = serde_yaml::Value::String(trimmed.to_owned());
        if fm.get(&key) == Some(&new_val) {
            false
        } else {
            fm.insert(key, new_val);
            true
        }
    };
    if !changed {
        return Ok(false);
    }
    let yaml = if fm.is_empty() {
        String::new()
    } else {
        serde_yaml::to_string(&fm)
            .with_context(|| format!("render frontmatter of {}", abs_path.display()))?
    };
    let rendered = format!("---\n{yaml}---\n{body}");
    wiki::atomic_write(abs_path, rendered.as_bytes())
        .with_context(|| format!("write {}", abs_path.display()))?;
    Ok(true)
}

/// Persist a wiki's one-line abstract into its `_meta.md` (`extra["summary"]`),
/// rewriting only when it changed. A blank `summary` clears any stale entry.
///
/// Called by the narrative compiler when it (re)compiles a wiki's `index.md`
/// overview page — the source of the wiki's abstract — so the catalog / root
/// index can surface it. The prose body of `_meta.md` is preserved.
///
/// # Errors
///
/// Surfaces `_meta.md` read/parse/render/write failures. The caller treats this
/// as a best-effort, non-fatal step.
pub fn sync_wiki_summary(abs_dir: &Path, summary: &str) -> Result<bool> {
    mutate_meta(abs_dir, |meta| apply_summary(&mut meta.extra, summary))
}

/// Read a wiki's `_meta.md`, run `mutate` to (maybe) change it, and atomically
/// rewrite the file **only when `mutate` reports a change** — the prose body is
/// preserved verbatim. Returns whether a write happened.
///
/// `pub(crate)` so other `_meta.md` mutators in the crate (e.g.
/// [`sync_wiki_summary`] and the `topics` recall-navigation setter) reuse the
/// same read → mutate → write-if-changed core instead of re-implementing the
/// body-preserving, idempotent rewrite.
pub(crate) fn mutate_meta(
    abs_dir: &Path,
    mutate: impl FnOnce(&mut WikiMeta) -> bool,
) -> Result<bool> {
    let meta_path = abs_dir.join(wiki::META_FILENAME);
    let raw = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("read {}", meta_path.display()))?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw)
        .with_context(|| format!("parse {}", meta_path.display()))?;
    if !mutate(&mut meta) {
        return Ok(false);
    }
    let rendered = meta
        .render(&body)
        .with_context(|| format!("render {}", meta_path.display()))?;
    wiki::atomic_write(&meta_path, rendered.as_bytes())
        .with_context(|| format!("write {}", meta_path.display()))?;
    Ok(true)
}

/// Set / update / clear the `topics` entry of a `keywords` mapping.
///
/// `topics` is joined **verbatim** in the order given (the caller sorts); an
/// empty slice clears any stale entry. Returns `true` iff the mapping changed,
/// so a caller can skip a no-op write.
fn apply_topics_keyword(keywords: &mut serde_yaml::Mapping, topics: &[String]) -> bool {
    if topics.is_empty() {
        return apply_keyword(keywords, TOPICS_KEY, None);
    }
    apply_keyword(keywords, TOPICS_KEY, Some(&topics.join(KEYWORD_JOIN)))
}

/// Set / update / clear one scalar `keywords` entry, preserving siblings.
///
/// `value = Some(v)` inserts or updates `key → v`; `value = None` clears it.
/// Returns `true` iff the mapping actually changed, so a caller can skip a
/// no-op write. The single mutation point for `keywords` entries so every
/// owner (topics, lifecycle params, …) shares one idempotent, sibling-safe
/// implementation.
fn apply_keyword(keywords: &mut serde_yaml::Mapping, key: &str, value: Option<&str>) -> bool {
    let key = serde_yaml::Value::String(key.to_owned());
    match value {
        None => keywords.remove(&key).is_some(),
        Some(v) => {
            let new_val = serde_yaml::Value::String(v.to_owned());
            if keywords.get(&key) == Some(&new_val) {
                return false;
            }
            keywords.insert(key, new_val);
            true
        },
    }
}

/// Set / update / clear the `summary` entry of an `extra` mapping. The value is
/// trimmed; an empty result clears the key. Returns `true` iff it changed.
fn apply_summary(extra: &mut serde_yaml::Mapping, summary: &str) -> bool {
    let key = serde_yaml::Value::String(SUMMARY_KEY.to_owned());
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return extra.remove(&key).is_some();
    }
    let new_val = serde_yaml::Value::String(trimmed.to_owned());
    if extra.get(&key) == Some(&new_val) {
        return false;
    }
    extra.insert(key, new_val);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    use crate::types::{FactId, WikiId};
    use crate::wiki::IdentityKind;

    const UUID_1: &str = "018f1234-5678-7abc-9def-0123456789ab";
    const UUID_2: &str = "018f1234-5678-7abc-9def-0123456789ac";
    const UUID_3: &str = "018f1234-5678-7abc-9def-0123456789ad";

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

    fn forge(tree: &WikiTree, id: &str) {
        let wid = WikiId::parse(id).unwrap();
        wiki::create_identity_wiki(tree, &wid, id, IdentityKind::User).unwrap();
    }

    async fn insert_fact(pool: &SqlitePool, uuid: &str, wiki: &str, topics: &[&str]) {
        insert_fact_at(pool, uuid, wiki, "index.md", topics).await;
    }

    async fn insert_fact_at(
        pool: &SqlitePool,
        uuid: &str,
        wiki: &str,
        page: &str,
        topics: &[&str],
    ) {
        insert_fact_owned(pool, uuid, wiki, page, &format!("user:{wiki}"), topics).await;
    }

    async fn insert_fact_owned(
        pool: &SqlitePool,
        uuid: &str,
        wiki: &str,
        page: &str,
        owner: &str,
        topics: &[&str],
    ) {
        let fact = fact_index::NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(uuid).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: format!("wikis/{wiki}/{page}"),
            region_start: Some(0),
            region_end: Some(16),
            text: "claim".to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            owner_id: owner.parse().unwrap(),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: None,
            topics: topics.iter().map(|t| (*t).to_owned()).collect(),
            valid_from: None,
            valid_to: None,
            // Inert: re-derived/non-ingest fact — no classifier placement
            // proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        fact_index::insert(pool, &fact).await.expect("insert fact");
    }

    /// Insert a fact with an explicit `owner` + `allow` list, for the
    /// reader-relative card boundary tests.
    async fn insert_fact_acl(
        pool: &SqlitePool,
        uuid: &str,
        wiki: &str,
        page: &str,
        owner: &str,
        allow: &[&str],
        topics: &[&str],
    ) {
        let fact = fact_index::NewFact {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(uuid).unwrap(),
            wiki_id: wiki.to_owned(),
            source_path: format!("wikis/{wiki}/{page}"),
            region_start: Some(0),
            region_end: Some(16),
            text: "claim".to_owned(),
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            owner_id: owner.parse().unwrap(),
            allow_ids: allow.iter().map(|a| a.parse().unwrap()).collect(),
            sender_id: None,
            fact_type: None,
            topics: topics.iter().map(|t| (*t).to_owned()).collect(),
            valid_from: None,
            valid_to: None,
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        };
        fact_index::insert(pool, &fact).await.expect("insert fact");
    }

    fn read_topics(tree: &WikiTree, wiki: &str) -> Option<String> {
        let raw =
            std::fs::read_to_string(tree.wikis_dir().join(wiki).join(wiki::META_FILENAME)).unwrap();
        let (meta, _body) = WikiMeta::parse(Path::new("x"), &raw).unwrap();
        meta.keywords
            .get(serde_yaml::Value::String(TOPICS_KEY.to_owned()))
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned)
    }

    fn read_summary_at(abs_dir: &Path) -> Option<String> {
        let raw = std::fs::read_to_string(abs_dir.join(wiki::META_FILENAME)).unwrap();
        let (meta, _body) = WikiMeta::parse(Path::new("x"), &raw).unwrap();
        meta.extra
            .get(serde_yaml::Value::String(SUMMARY_KEY.to_owned()))
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned)
    }

    // ---------- set_page_description / read_page_description ----------

    #[test]
    fn set_page_description_sets_clears_and_preserves_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("p.md");
        std::fs::write(
            &page,
            "---\nstyle: prosa\nkeywords:\n  topics: food, wine\n---\n# Recipes\n\nPasta.\n",
        )
        .unwrap();

        // Set: the description lands; siblings (style, keywords) and body survive.
        assert!(set_page_description(&page, "  Cooking notes  ").unwrap());
        let raw = std::fs::read_to_string(&page).unwrap();
        assert!(raw.contains("description: Cooking notes"), "{raw}");
        assert!(raw.contains("style: prosa"), "style sibling lost: {raw}");
        assert!(raw.contains("food, wine"), "keywords sibling lost: {raw}");
        assert!(raw.contains("Pasta."), "body lost: {raw}");
        assert_eq!(
            read_page_description(&page).unwrap().as_deref(),
            Some("Cooking notes"),
            "read side must round-trip (trimmed)"
        );

        // Idempotent: the same trimmed value reports no change.
        assert!(!set_page_description(&page, "Cooking notes").unwrap());

        // Clear: a blank value removes the key; siblings stay.
        assert!(set_page_description(&page, "   ").unwrap());
        let raw = std::fs::read_to_string(&page).unwrap();
        assert!(
            !raw.contains("description:"),
            "description not cleared: {raw}"
        );
        assert!(raw.contains("style: prosa"), "sibling lost on clear: {raw}");
        assert_eq!(read_page_description(&page).unwrap(), None);
    }

    #[test]
    fn set_page_description_creates_frontmatter_on_a_bare_page_but_blank_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();

        // A page with no testata gains a minimal frontmatter; body preserved.
        let page = dir.path().join("bare.md");
        std::fs::write(&page, "# Just prose\n\nNo testata here.\n").unwrap();
        assert!(set_page_description(&page, "What goes here").unwrap());
        let raw = std::fs::read_to_string(&page).unwrap();
        assert!(raw.starts_with("---\n"), "frontmatter not prepended: {raw}");
        assert!(raw.contains("description: What goes here"), "{raw}");
        assert!(raw.contains("No testata here."), "body lost: {raw}");

        // A bare page + blank description stays untouched (no empty fence).
        let page2 = dir.path().join("bare2.md");
        std::fs::write(&page2, "# Plain\n").unwrap();
        assert!(!set_page_description(&page2, "  ").unwrap());
        assert_eq!(std::fs::read_to_string(&page2).unwrap(), "# Plain\n");
    }

    // ---------- apply_topics_keyword (pure) ----------

    #[test]
    fn apply_topics_keyword_inserts_updates_and_clears() {
        let mut kw = serde_yaml::Mapping::new();

        // Insert.
        assert!(apply_topics_keyword(
            &mut kw,
            &["food".to_owned(), "wine".to_owned()]
        ));
        assert_eq!(
            kw.get(serde_yaml::Value::String("topics".to_owned()))
                .and_then(serde_yaml::Value::as_str),
            Some("food, wine")
        );

        // Identical → no change.
        assert!(!apply_topics_keyword(
            &mut kw,
            &["food".to_owned(), "wine".to_owned()]
        ));

        // Different → change.
        assert!(apply_topics_keyword(&mut kw, &["beer".to_owned()]));
        assert_eq!(
            kw.get(serde_yaml::Value::String("topics".to_owned()))
                .and_then(serde_yaml::Value::as_str),
            Some("beer")
        );

        // Empty → clears the key and reports the change.
        assert!(apply_topics_keyword(&mut kw, &[]));
        assert!(!kw.contains_key(serde_yaml::Value::String("topics".to_owned())));
        // Clearing an absent key is a no-op.
        assert!(!apply_topics_keyword(&mut kw, &[]));
    }

    #[test]
    fn apply_topics_keyword_preserves_sibling_keywords() {
        let mut kw = serde_yaml::Mapping::new();
        kw.insert(
            serde_yaml::Value::String("city".to_owned()),
            serde_yaml::Value::String("rivendell".to_owned()),
        );
        assert!(apply_topics_keyword(&mut kw, &["lore".to_owned()]));
        assert_eq!(
            kw.get(serde_yaml::Value::String("city".to_owned()))
                .and_then(serde_yaml::Value::as_str),
            Some("rivendell"),
            "an unrelated keyword must survive the topics write"
        );
    }

    // ---------- apply_summary (pure) ----------

    #[test]
    fn apply_summary_inserts_updates_clears_and_trims() {
        let mut extra = serde_yaml::Mapping::new();
        let get = |e: &serde_yaml::Mapping| {
            e.get(serde_yaml::Value::String("summary".to_owned()))
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_owned)
        };

        // Insert, trimming surrounding whitespace.
        assert!(apply_summary(&mut extra, "  Alice, a Milan engineer  "));
        assert_eq!(get(&extra).as_deref(), Some("Alice, a Milan engineer"));
        // Identical (after trim) → no change.
        assert!(!apply_summary(&mut extra, "Alice, a Milan engineer"));
        // Different → change.
        assert!(apply_summary(&mut extra, "Alice, now in Rome"));
        assert_eq!(get(&extra).as_deref(), Some("Alice, now in Rome"));
        // Blank → clears; clearing an absent key is a no-op.
        assert!(apply_summary(&mut extra, "   "));
        assert!(!extra.contains_key(serde_yaml::Value::String("summary".to_owned())));
        assert!(!apply_summary(&mut extra, ""));
    }

    // ---------- sync_wiki_summary (filesystem) ----------

    #[test]
    fn sync_wiki_summary_writes_extra_preserves_body_and_is_idempotent() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let abs = tree.wikis_dir().join("alice");

        // Give _meta.md a prose body + an existing topics keyword to prove both
        // survive an extra["summary"] write.
        assert!(sync_meta_topics(&abs, &["lore".to_owned()]).unwrap());
        let raw = std::fs::read_to_string(abs.join(wiki::META_FILENAME)).unwrap();
        let (meta, _body) = WikiMeta::parse(Path::new("x"), &raw).unwrap();
        std::fs::write(
            abs.join(wiki::META_FILENAME),
            meta.render("# Alice\n\nNotes.\n").unwrap(),
        )
        .unwrap();

        assert!(sync_wiki_summary(&abs, "Alice, a Milan engineer").unwrap());
        assert_eq!(
            read_summary_at(&abs).as_deref(),
            Some("Alice, a Milan engineer")
        );
        assert_eq!(
            read_topics(&tree, "alice").as_deref(),
            Some("lore"),
            "topics survive"
        );
        let after = std::fs::read_to_string(abs.join(wiki::META_FILENAME)).unwrap();
        assert!(after.contains("Notes."), "body must survive");

        // Idempotent.
        assert!(!sync_wiki_summary(&abs, "Alice, a Milan engineer").unwrap());
        // Blank clears.
        assert!(sync_wiki_summary(&abs, "").unwrap());
        assert_eq!(read_summary_at(&abs), None);
    }

    // ---------- sync_meta_topics (filesystem) ----------

    #[test]
    fn sync_meta_topics_preserves_body_and_is_idempotent() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let abs = tree.wikis_dir().join("alice");

        // Give _meta.md a prose body so we can prove it survives the rewrite.
        let raw = std::fs::read_to_string(abs.join(wiki::META_FILENAME)).unwrap();
        let (meta, _body) = WikiMeta::parse(Path::new("x"), &raw).unwrap();
        std::fs::write(
            abs.join(wiki::META_FILENAME),
            meta.render("# Alice\n\nHand-written notes.\n").unwrap(),
        )
        .unwrap();

        // Caller passes a sorted slice; sync joins verbatim.
        let topics = ["a".to_owned(), "b".to_owned()];
        assert!(sync_meta_topics(&abs, &topics).unwrap());

        let after = std::fs::read_to_string(abs.join(wiki::META_FILENAME)).unwrap();
        assert!(after.contains("Hand-written notes."), "body must survive");
        let (m2, _) = WikiMeta::parse(Path::new("x"), &after).unwrap();
        assert_eq!(
            m2.keywords
                .get(serde_yaml::Value::String("topics".to_owned()))
                .and_then(serde_yaml::Value::as_str),
            Some("a, b")
        );

        // Second identical sync writes nothing.
        assert!(!sync_meta_topics(&abs, &topics).unwrap());
    }

    // ---------- sync_wiki_keywords (db + fs end to end) ----------

    #[tokio::test]
    async fn sync_wiki_keywords_writes_per_wiki_sorted_union_and_is_idempotent() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        forge(&tree, "bob");
        let pool = make_pool().await;

        // alice: overlapping topics across two facts → sorted, de-duplicated.
        insert_fact(&pool, UUID_1, "alice", &["food", "italian"]).await;
        insert_fact(&pool, UUID_2, "alice", &["italian", "wine"]).await;
        // bob: a disjoint topic — must not leak into alice.
        insert_fact(&pool, UUID_3, "bob", &["sailing"]).await;

        let updated = sync_wiki_keywords(&pool, &tree).await.unwrap();
        assert_eq!(updated, 2, "both wikis gained a topics keyword");

        assert_eq!(
            read_topics(&tree, "alice").as_deref(),
            Some("food, italian, wine")
        );
        assert_eq!(read_topics(&tree, "bob").as_deref(), Some("sailing"));

        // Idempotent: nothing changed → no rewrite.
        assert_eq!(sync_wiki_keywords(&pool, &tree).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sync_wiki_keywords_skips_wiki_without_topics() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let pool = make_pool().await;
        // A fact with no topics must not produce a (stale, empty) keyword.
        insert_fact(&pool, UUID_1, "alice", &[]).await;

        assert_eq!(sync_wiki_keywords(&pool, &tree).await.unwrap(), 0);
        assert_eq!(read_topics(&tree, "alice"), None);
    }

    // ---------- sync_page_keywords (db + fs end to end) ----------

    fn write_page(tree: &WikiTree, wiki: &str, page: &str, contents: &str) {
        let wid = WikiId::parse(wiki).unwrap();
        let handle = tree.locate(&wid).unwrap();
        handle.write_page(Path::new(page), contents).unwrap();
    }

    fn read_page_topics(tree: &WikiTree, wiki: &str, page: &str) -> Option<String> {
        let raw = std::fs::read_to_string(tree.wikis_dir().join(wiki).join(page)).unwrap();
        let doc = wiki::MarkdownDoc::parse(&raw)?;
        let fm: serde_yaml::Mapping = serde_yaml::from_str(&doc.frontmatter).ok()?;
        let kw = fm.get(serde_yaml::Value::String("keywords".to_owned()))?;
        kw.as_mapping()?
            .get(serde_yaml::Value::String(TOPICS_KEY.to_owned()))
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn sync_page_keywords_writes_per_page_union_and_is_idempotent() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let pool = make_pool().await;
        write_page(
            &tree,
            "alice",
            "recipes.md",
            "---\ntitle: \"Recipes\"\nstyle: prosa\ndescription: \"Cooking notes\"\n---\n\nPasta.\n",
        );
        write_page(
            &tree,
            "alice",
            "travel.md",
            "---\ntitle: \"Travel\"\nstyle: prosa\n---\n\nRome.\n",
        );

        // Facts on two different pages of the same wiki: each page gets its
        // own union, not the wiki-wide one. (The bootstrap `index.md` has no
        // testata yet — the compiler writes it on first compile — so the pass
        // leaves it alone; that path is covered by the skip test below.)
        insert_fact_at(&pool, UUID_1, "alice", "travel.md", &["places"]).await;
        insert_fact_at(&pool, UUID_2, "alice", "recipes.md", &["food", "italian"]).await;
        insert_fact_at(&pool, UUID_3, "alice", "recipes.md", &["italian", "wine"]).await;

        assert_eq!(sync_page_keywords(&pool, &tree).await.unwrap(), 2);
        assert_eq!(
            read_page_topics(&tree, "alice", "recipes.md").as_deref(),
            Some("food, italian, wine")
        );
        assert_eq!(
            read_page_topics(&tree, "alice", "travel.md").as_deref(),
            Some("places")
        );

        // Sibling frontmatter fields and the body survive the rewrite.
        let raw = std::fs::read_to_string(tree.wikis_dir().join("alice/recipes.md")).unwrap();
        assert!(raw.contains("Cooking notes"), "description must survive");
        assert!(raw.contains("style: prosa"), "style must survive");
        assert!(raw.contains("Pasta."), "body must survive");

        // Idempotent: nothing changed → no rewrite.
        assert_eq!(sync_page_keywords(&pool, &tree).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sync_page_keywords_clears_stale_entry_when_facts_moved_away() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let pool = make_pool().await;
        // A page whose testata carries topics no fact backs any more (e.g.
        // after a REM split relocated its facts) sheds the stale entry; the
        // operator-authored sibling keyword survives.
        write_page(
            &tree,
            "alice",
            "old.md",
            "---\ntitle: \"Old\"\nkeywords:\n  topics: stale, gone\n  city: rivendell\n---\n\nBody.\n",
        );

        assert_eq!(sync_page_keywords(&pool, &tree).await.unwrap(), 1);
        assert_eq!(read_page_topics(&tree, "alice", "old.md"), None);
        let raw = std::fs::read_to_string(tree.wikis_dir().join("alice/old.md")).unwrap();
        assert!(
            raw.contains("city: rivendell"),
            "operator keyword must survive the topics clear"
        );
        assert!(raw.contains("Body."), "body must survive");
    }

    // ---------- the ACL card boundary ----------

    #[test]
    fn fact_at_default_visibility_rules() {
        let alice: Principal = "user:alice".parse().unwrap();
        let bob: Principal = "user:bob".parse().unwrap();
        let family: Principal = "group:family".parse().unwrap();

        // Default-owned and global contribute; anything else does not.
        assert!(fact_at_default_visibility(&alice, &alice));
        assert!(fact_at_default_visibility(&Principal::global(), &alice));
        assert!(!fact_at_default_visibility(&bob, &alice));
        assert!(!fact_at_default_visibility(&family, &alice));
        // Group wiki: the group's own facts contribute, a member's private
        // region does not.
        assert!(fact_at_default_visibility(&family, &family));
        assert!(!fact_at_default_visibility(&alice, &family));
    }

    #[tokio::test]
    async fn card_keywords_exclude_off_default_owners_on_both_levels() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let pool = make_pool().await;
        write_page(
            &tree,
            "alice",
            "notes.md",
            "---\ntitle: \"Notes\"\nstyle: prosa\n---\n\nBody.\n",
        );

        // alice's wiki (acl_default user:alice): her own fact and a global
        // fact contribute; bob's cross-user region must not leak its topic
        // words onto either card.
        insert_fact_owned(&pool, UUID_1, "alice", "notes.md", "user:alice", &["food"]).await;
        insert_fact_owned(&pool, UUID_2, "alice", "notes.md", "global", &["public"]).await;
        insert_fact_owned(&pool, UUID_3, "alice", "notes.md", "user:bob", &["secret"]).await;

        sync_wiki_keywords(&pool, &tree).await.unwrap();
        sync_page_keywords(&pool, &tree).await.unwrap();

        assert_eq!(read_topics(&tree, "alice").as_deref(), Some("food, public"));
        assert_eq!(
            read_page_topics(&tree, "alice", "notes.md").as_deref(),
            Some("food, public")
        );
    }

    #[tokio::test]
    async fn build_reader_card_is_reader_relative() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        forge(&tree, "bob");
        let pool = make_pool().await;
        // On alice's wiki (acl_default = user:alice): a private fact only she
        // can read, a fact shared with bob, and a public fact.
        insert_fact_acl(
            &pool,
            UUID_1,
            "alice",
            "index.md",
            "user:alice",
            &[],
            &["celiachia"],
        )
        .await;
        insert_fact_acl(
            &pool,
            UUID_2,
            "alice",
            "index.md",
            "user:alice",
            &["user:bob"],
            &["viaggio"],
        )
        .await;
        insert_fact_acl(
            &pool,
            UUID_3,
            "alice",
            "index.md",
            "global",
            &[],
            &["ricette"],
        )
        .await;

        let alice_card = build_reader_card(&pool, &tree, "alice", &[]).await.unwrap();
        let bob_card = build_reader_card(&pool, &tree, "bob", &[]).await.unwrap();
        let carol_card = build_reader_card(&pool, &tree, "carol", &[]).await.unwrap();

        let topics = |c: &ReaderCard, w: &str| -> Vec<String> { c.wiki_topics(w).to_vec() };
        // Owner sees every topic of her own wiki.
        assert_eq!(
            topics(&alice_card, "alice"),
            ["celiachia", "ricette", "viaggio"]
        );
        // The denied reader (bob) never sees the private "celiachia" — the leak
        // 25a closes — but does see the shared + public topics.
        assert_eq!(topics(&bob_card, "alice"), ["ricette", "viaggio"]);
        // An outsider sees only the public topic.
        assert_eq!(topics(&carol_card, "alice"), ["ricette"]);
        // Page-level projection mirrors the wiki-level filtering.
        assert_eq!(
            bob_card.pages("alice").unwrap()["wikis/alice/index.md"],
            ["ricette", "viaggio"]
        );

        // The abstract is gated to the wiki's default visibility: only alice
        // (the acl_default principal) may see her wiki's summary.
        assert!(alice_card.summary_visible("alice"));
        assert!(!bob_card.summary_visible("alice"));
        assert!(!carol_card.summary_visible("alice"));
    }

    #[tokio::test]
    async fn sync_page_keywords_skips_page_without_frontmatter() {
        let (_dir, tree) = open_tree();
        forge(&tree, "alice");
        let pool = make_pool().await;
        write_page(&tree, "alice", "raw.md", "No testata here.\n");
        insert_fact_at(&pool, UUID_1, "alice", "raw.md", &["food"]).await;

        // The card is compiler output — this pass never invents a testata.
        assert_eq!(sync_page_keywords(&pool, &tree).await.unwrap(), 0);
        let raw = std::fs::read_to_string(tree.wikis_dir().join("alice/raw.md")).unwrap();
        assert_eq!(raw, "No testata here.\n");
    }
}
