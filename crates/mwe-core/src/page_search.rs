// SPDX-License-Identifier: AGPL-3.0-or-later
//! Finding the **pages a word appears on** — what the dashboard's Search box
//! asks the memory.
//!
//! It answers one question and only that one: *where, in everything I am
//! allowed to read, do these words appear?* The answer is a list of pages,
//! each with the sentence the words were found in and a link to the page.
//!
//! # Why it is words and not similarity
//!
//! The recall path ([`crate::recall`]) answers *what is this turn about*, with
//! an embedder and a ranking. A person typing in a search box is asking
//! something different and much narrower — they know the word, they want the
//! pages — and a similarity answer to that question is wrong in both
//! directions at once: it returns pages the word is not on, and it leaves out
//! the page the word is on because that page reads as being about something
//! else. So no embedder and no model runs here: the words are folded and
//! matched, and every hit carries every word.
//!
//! # The ACL is the store's job, the matching is Rust's
//!
//! Two families, two different questions, the same two the rest of the engine
//! asks (see [`crate::wiki_admin::wiki_readable_by`]):
//!
//! - a **standard** wiki hides per fragment, so the search matches the
//!   **facts** the searcher may read ([`fact_index::readable_fact_texts`],
//!   where the ACL is a SQL predicate) and a page is a hit when one of those
//!   facts carries the words. A fact the searcher may not read never leaves
//!   the database, so no page can be found through it;
//! - a **smart** wiki holds no facts at all — its consumer writes the pages
//!   whole — so the search matches the page text, and only in a wiki the
//!   searcher may read at all.
//!
//! The consequence on a standard wiki is worth stating, because it is a
//! limit and not an oversight: the words are matched against the facts, never
//! against the prose the page writer weaves around them, so a word that lives
//! only in that connecting prose does not bring its page back. Matching the
//! page text there would match the regions the reader is served `[redacted]`
//! — the search would answer out of what the page view refuses to show them.
//!
//! The match itself happens here rather than in SQL because the fold a person
//! expects from a search box is case **and** accents, and `LIKE` folds
//! neither the accents nor anything outside ASCII: a `LIKE '%citta%'` narrowing
//! would quietly drop the page that says *città*, which is the page they were
//! looking for. Reading the readable corpus and folding it is the same work
//! the recall scan already does on every conversational turn, and this one
//! runs when a person presses a button.

use std::collections::BTreeMap;
use std::path::Path;

use sqlx::SqlitePool;
use thiserror::Error;

use crate::fact_index;
use crate::types::WikiId;
use crate::wiki::WikiTree;

/// Errors raised by the page search.
#[derive(Debug, Error)]
pub enum SearchError {
    /// The readable-fact query failed.
    #[error("page search facts: {0}")]
    Facts(#[from] fact_index::FactIndexError),

    /// Walking the wiki tree or reading a smart wiki's page failed.
    #[error("page search wiki: {0}")]
    Wiki(#[from] crate::wiki::WikiError),

    /// Resolving a smart wiki's read access failed.
    #[error("page search access: {0}")]
    Access(#[from] crate::wiki_admin::AdminError),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, SearchError>;

/// How many pages one search lists. A **page cap**, not a corpus bound: the
/// scan reads everything the searcher may read either way, and this only
/// decides how much of it is worth putting on one screen. A search that fills
/// it says so, so nobody reads a truncated list as the whole answer.
const MAX_HITS: usize = 200;

/// Characters of prose shown around a match. Long enough for the sentence the
/// words are in, short enough that a page is one line of the results list.
const EXCERPT_CHARS: usize = 200;

/// Who is searching, and how widely.
#[derive(Debug, Clone, Copy)]
pub struct Searcher<'a> {
    /// The person's own id.
    pub sender_id: &'a str,
    /// The groups they belong to ([`crate::enrollment::groups_for`]).
    pub groups: &'a [String],
    /// The dashboard's admin reveal is on ([`crate::acl`] bypassed), so the
    /// search matches every live fact and every wiki — the same widening the
    /// wiki page view applies when it renders every fragment instead of
    /// `[redacted]`. It is admin-only and server-checked at the surface.
    pub reveal: bool,
}

/// One page the searched words appear on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHit {
    /// The wiki the page belongs to.
    pub wiki_id: String,
    /// The wiki's title, for the heading the hits are grouped under.
    pub wiki_title: String,
    /// The page's path inside its wiki (`cucina.md`, `ricette/pasta.md`) —
    /// what the page view's URL carries after `/view/`.
    pub page: String,
    /// The prose the words were found in, cut to a readable window.
    pub excerpt: String,
}

/// What one search found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResult {
    /// The pages, wiki by wiki in id order, page by page in path order.
    pub hits: Vec<PageHit>,
    /// The cap on how many pages one search lists was reached, so there are
    /// pages carrying the words that are not in `hits`.
    pub truncated: bool,
}

/// The pages where every word of `query` appears, in what `searcher` may read.
///
/// A query with no words at all (blank, or punctuation only) finds nothing
/// rather than everything: it is the shape of a search box submitted empty,
/// and answering it with the entire memory would be a wrong answer that also
/// costs the whole corpus.
///
/// # Errors
///
/// The fact query, the wiki walk, a smart wiki's access resolution, or
/// reading one of its pages.
pub async fn search(
    pool: &SqlitePool,
    tree: &WikiTree,
    query: &str,
    searcher: Searcher<'_>,
) -> Result<SearchResult> {
    let words = folded_query(query);
    if words.is_empty() {
        return Ok(SearchResult::default());
    }

    // One walk, read twice: the standard half turns a fact's `source_path`
    // back into (wiki, page) with it, and the smart half enumerates from it.
    let wikis = tree.walk()?;
    let by_id: BTreeMap<&str, &crate::wiki::DiscoveredWiki> =
        wikis.iter().map(|w| (w.meta.wiki_id.as_str(), w)).collect();
    let mut hits: BTreeMap<(String, String), PageHit> = BTreeMap::new();

    // ---- the standard half: the facts this searcher may read ----
    let principals = crate::acl::reader_principals(searcher.sender_id, searcher.groups);
    let readable_by = if searcher.reveal {
        None
    } else {
        Some(principals.as_slice())
    };
    for fact in fact_index::readable_fact_texts(pool, readable_by).await? {
        let Some(hit) = matched(&fact.text, &words) else {
            continue;
        };
        // A wiki the walk does not know is a row left behind by a deleted
        // wiki: there is no page to link to, so there is no hit to show.
        let Some(wiki) = by_id.get(fact.wiki_id.as_str()) else {
            continue;
        };
        // A smart wiki holds no facts and is gated at wiki level, below,
        // rather than per fragment. A stray row carrying one's id would open
        // its pages through the per-fact gate instead — which is the shape
        // the wiki-level gate exists to refuse.
        if wiki.meta.smart {
            continue;
        }
        let prefix = format!("{}/", wiki.rel_dir_posix());
        let Some(page) = fact.source_path.strip_prefix(&prefix) else {
            continue;
        };
        hits.entry((fact.wiki_id.clone(), page.to_owned()))
            .or_insert_with(|| PageHit {
                wiki_id: fact.wiki_id.clone(),
                wiki_title: wiki.meta.title.clone(),
                page: page.to_owned(),
                excerpt: hit,
            });
    }

    // ---- the smart half: whole pages, in a wiki the searcher may read ----
    for wiki in wikis.iter().filter(|w| w.meta.smart) {
        if !searcher.reveal && !smart_readable(pool, tree, &wiki.meta.wiki_id, searcher).await? {
            continue;
        }
        for page in crate::wiki::list_wiki_pages(&wiki.abs_dir)? {
            let text = match std::fs::read_to_string(&page.abs_path) {
                Ok(text) => text,
                // A page enumerated and then unreadable (removed under us,
                // permissions) is one page missing from one search, never a
                // failed search.
                Err(e) => {
                    tracing::warn!(page = %page.abs_path.display(), error = %e,
                        "page search: page unread, skipped");
                    continue;
                },
            };
            let Some(excerpt) = matched(&text, &words) else {
                continue;
            };
            let rel = page.rel_path_posix();
            hits.entry((wiki.meta.wiki_id.as_str().to_owned(), rel.clone()))
                .or_insert_with(|| PageHit {
                    wiki_id: wiki.meta.wiki_id.as_str().to_owned(),
                    wiki_title: wiki.meta.title.clone(),
                    page: rel,
                    excerpt,
                });
        }
    }

    let truncated = hits.len() > MAX_HITS;
    Ok(SearchResult {
        hits: hits.into_values().take(MAX_HITS).collect(),
        truncated,
    })
}

/// May this searcher read this smart wiki at all — the wiki-level gate, asked
/// of the same function every other read path asks.
async fn smart_readable(
    pool: &SqlitePool,
    tree: &WikiTree,
    wiki_id: &WikiId,
    searcher: Searcher<'_>,
) -> Result<bool> {
    let handle = tree.locate(wiki_id)?;
    Ok(crate::wiki_admin::wiki_readable_by(
        pool,
        tree,
        &handle,
        searcher.sender_id,
        searcher.groups,
    )
    .await?)
}

/// The query as the words a match is tested against: folded, and each one
/// kept whole.
fn folded_query(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(crate::recall::fold_name)
        .filter(|w| !w.is_empty())
        .collect()
}

/// Does `text` carry **every** word, and if so, what does the reader see?
///
/// A query word matches a word of the text that **starts with it**, folded on
/// both sides. Prefix and not equality because a person types a search box
/// half a word at a time — *pediatr* must find *pediatra*; prefix and not
/// substring because the middle of an unrelated word is not where anybody
/// meant to look, and *casa* finding *cassata* would be noise the excerpt
/// then has to explain away.
///
/// The excerpt is built around the first word matched, so a long page opens
/// on the place the search was about rather than on its first line.
fn matched(text: &str, words: &[String]) -> Option<String> {
    let tokens = folded_tokens(text);

    let mut first: Option<usize> = None;
    for word in words {
        let at = tokens
            .iter()
            .find(|(token, _)| token.starts_with(word.as_str()))
            .map(|(_, at)| *at)?;
        first = Some(first.map_or(at, |f: usize| f.min(at)));
    }
    Some(excerpt(text, first.unwrap_or(0)))
}

/// Each word of `text`, folded, with the byte offset it starts at.
///
/// The same word rule [`folded_query`] applies to the query — a run of
/// alphanumerics — so the two sides are cut alike; the offset is what lets
/// the excerpt open on the match instead of on the first line.
fn folded_tokens(text: &str) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            out.push((crate::recall::fold_name(&text[s..i]), s));
        }
    }
    if let Some(s) = start {
        out.push((crate::recall::fold_name(&text[s..]), s));
    }
    out
}

/// A readable window of `text` around the byte offset `at`, whitespace
/// collapsed onto one line and an ellipsis wherever the window cuts.
fn excerpt(text: &str, at: usize) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let hit = chars
        .iter()
        .position(|(i, _)| *i >= at)
        .unwrap_or_else(|| chars.len().saturating_sub(1));
    // A third of the window before the match, the rest after: enough of the
    // run-up to read the sentence, and the room where the answer usually is
    // after it.
    let before = EXCERPT_CHARS / 3;
    let start = hit.saturating_sub(before);
    let end = (start + EXCERPT_CHARS).min(chars.len());
    let slice: String = chars[start..end].iter().map(|(_, c)| *c).collect();
    let mut out = String::with_capacity(slice.len() + 2);
    if start > 0 {
        out.push('…');
    }
    let mut space = false;
    for c in slice.trim().chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Turn a hit into the address of the page view that shows it.
///
/// One function so the search results and anything else that links a hit
/// cannot disagree about the shape of that URL.
#[must_use]
pub fn page_view_path(wiki_id: &str, page: &str) -> String {
    format!("/dashboard/wiki/{wiki_id}/view/{page}")
}

/// Is `page` a path this search may hand to the page view?
///
/// The page came from the wiki tree or from a `source_path` the engine wrote,
/// so it is well-formed by construction — but it becomes a URL, and a link
/// built from a path that walked out of its wiki would be a link into
/// somebody else's.
#[must_use]
pub fn is_linkable(page: &str) -> bool {
    crate::wiki::is_safe_page_path(Path::new(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_folded_for_case_and_accents() {
        assert_eq!(folded_query("Città  Vecchia!"), vec!["citta", "vecchia"]);
        assert_eq!(folded_query("   "), Vec::<String>::new());
        assert_eq!(folded_query("!?-"), Vec::<String>::new());
    }

    #[test]
    fn every_word_must_appear_and_a_prefix_is_a_word() {
        let text = "Il pediatra di Alice è il dottor Bianchi.";
        assert!(matched(text, &folded_query("pediatra")).is_some());
        // The half-typed word finds the whole one …
        assert!(matched(text, &folded_query("pediatr")).is_some());
        // … but the middle of a word is not a match.
        assert!(matched(text, &folded_query("iatra")).is_none());
        // Both words, or nothing.
        assert!(matched(text, &folded_query("pediatra bianchi")).is_some());
        assert!(matched(text, &folded_query("pediatra rossi")).is_none());
    }

    #[test]
    fn the_accent_is_folded_on_both_sides() {
        assert!(matched("Vivono in città da marzo.", &folded_query("citta")).is_some());
        assert!(matched("Vivono in citta da marzo.", &folded_query("città")).is_some());
    }

    #[test]
    fn the_excerpt_opens_on_the_match_and_is_one_line() {
        let text = format!(
            "{}\n\nla parola cercata sta qui.\n{}",
            "x ".repeat(200),
            "y ".repeat(200)
        );
        let excerpt = matched(&text, &folded_query("cercata")).expect("matches");
        assert!(excerpt.contains("parola cercata"), "{excerpt}");
        assert!(!excerpt.contains('\n'), "{excerpt}");
        assert!(
            excerpt.starts_with('…') && excerpt.ends_with('…'),
            "{excerpt}"
        );
        assert!(excerpt.chars().count() <= EXCERPT_CHARS + 2, "{excerpt}");
    }

    #[test]
    fn a_short_text_is_shown_whole() {
        let excerpt = matched(
            "Il pediatra è il dottor Bianchi.",
            &folded_query("pediatra"),
        )
        .expect("matches");
        assert_eq!(excerpt, "Il pediatra è il dottor Bianchi.");
    }

    #[test]
    fn a_page_becomes_the_address_of_its_view() {
        assert_eq!(
            page_view_path("alice", "ricette/pasta.md"),
            "/dashboard/wiki/alice/view/ricette/pasta.md"
        );
        assert!(is_linkable("ricette/pasta.md"));
        assert!(!is_linkable("../altrove.md"));
    }
}
