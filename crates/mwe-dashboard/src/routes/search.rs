// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Search** — the pages a word appears on.
//!
//! One route, `GET /dashboard/search?q=…`, open to every signed-in person:
//! it takes the words from the box in the top bar and answers with the pages
//! that carry them, grouped by wiki, each with the line the words are in and
//! a link to the page.
//!
//! # What it shows is what the reader may read
//!
//! The whole question is answered by [`mwe_core::page_search`], which asks the
//! two ACL questions the rest of the engine asks — the facts a person may read
//! in a standard wiki, the wiki-level roster of a smart one. This surface adds
//! nothing to that and takes nothing away: it hands over the searcher, their
//! groups, and whether the admin reveal is on, and renders what comes back.
//! Reveal widens it exactly as it widens the wiki page view, which shows every
//! fragment instead of `[redacted]`.
//!
//! # Why the box is in the top bar
//!
//! *"Where does this word appear"* is a question asked from wherever the
//! reader happens to be — halfway down a page, in the facts table, on their
//! home. A box that lived on one page would make them navigate to it first,
//! which is the step the box exists to remove. It sits inside the nav, so on a
//! narrow screen it collapses behind the same hamburger as the links.

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use mwe_core::enrollment;
use mwe_core::page_search::{self, PageHit, SearchResult, Searcher};
use serde::Deserialize;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::layout;

/// Where the top-bar box submits, and where a `?q=` link lands.
pub const PATH: &str = "/dashboard/search";

/// Routes for the Search surface. Merged into the authenticated tree and not
/// under `/admin/`: everybody searches their own memory.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/search", get(index))
}

/// Query string of [`index`]. `q` is the words to look for; absent or blank
/// is the box as it opens, not a search that found nothing.
#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
}

/// `GET /dashboard/search` — the box, and the pages the words are on.
async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Query(query): Query<SearchQuery>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let asked = query.q.unwrap_or_default();
    let reveal = crate::reveal::active(&state, &user, &jar);

    let body = if asked.trim().is_empty() {
        render_body(&asked, None, reveal)
    } else {
        let memory = state.memory.as_ref().ok_or_else(|| {
            DashboardError::Internal(
                "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
            )
        })?;
        // Reveal reads past the ACL, so the groups it would be narrowed by
        // are not looked up at all — the same shape the wiki listing uses.
        let groups = if reveal {
            Vec::new()
        } else {
            enrollment::groups_for(&state.pool, &user.sender_id)
                .await
                .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?
        };
        let found = page_search::search(
            &state.pool,
            &memory.tree,
            &asked,
            Searcher {
                sender_id: &user.sender_id,
                groups: &groups,
                reveal,
            },
        )
        .await
        .map_err(|e| DashboardError::Internal(format!("page_search: {e}")))?;
        render_body(&asked, Some(&found), reveal)
    };

    Ok(Html(layout::authenticated_page(
        chrome, "Search", &user, &body,
    )))
}

/// The search box, rendered on the page itself so somebody who arrived by
/// address has one, and so the words they asked for are still in front of
/// them while they read the answer.
fn page_box(asked: &str) -> Markup {
    html! {
        form.page-search method="get" action=(PATH) role="search" {
            label for="page-search-q" { "Words to look for" }
            input id="page-search-q" type="search" name="q" value=(asked)
                placeholder="e.g. pediatra" autofocus;
            " "
            button type="submit" { "Search" }
        }
    }
}

/// The whole page under the heading: the box, then what the words found.
fn render_body(asked: &str, found: Option<&SearchResult>, reveal: bool) -> Markup {
    html! {
        @if reveal { (crate::reveal::banner()) }
        p class="text-text-dim max-w-prose" {
            "Every page you can read that carries the words, with the line "
            "they appear in. A word finds the words that begin with it, so "
            code { "pediatr" } " finds " code { "pediatra" }
            "; accents and capitals make no difference. Give two words and a "
            "page comes back only if it carries both."
        }
        (page_box(asked))
        @match found {
            None => {
                p class="mt-6" {
                    "Type a word above and press Search."
                }
            },
            Some(found) if found.hits.is_empty() => {
                p class="mt-6" {
                    "Nothing you can read carries " strong { (asked) } ". "
                    "Try one word instead of several, or a shorter piece of it "
                    "— and remember that a page you have never been shown is "
                    "not searched."
                }
            },
            Some(found) => (results(found)),
        }
    }
}

/// The hits, under one heading per wiki.
fn results(found: &SearchResult) -> Markup {
    let pages = found.hits.len();
    html! {
        p class="mt-6" {
            @if pages == 1 {
                "One page."
            } @else {
                (pages) " pages."
            }
            @if found.truncated {
                " That is as many as one search lists — narrow it with another word."
            }
        }
        @for (wiki_id, wiki_title, hits) in by_wiki(&found.hits) {
            section class="mt-6" {
                h2 class="text-lg mb-2" {
                    a href=(format!("/dashboard/wiki/{wiki_id}")) { (wiki_title) }
                    " "
                    span class="text-text-dim text-xs" { (wiki_id) }
                }
                ul.search-hits {
                    @for hit in hits {
                        li {
                            @if page_search::is_linkable(&hit.page) {
                                a href=(page_search::page_view_path(&hit.wiki_id, &hit.page)) {
                                    (hit.page)
                                }
                            } @else {
                                span { (hit.page) }
                            }
                            div class="text-text-dim" { (hit.excerpt) }
                        }
                    }
                }
            }
        }
    }
}

/// Group the hits by the wiki they belong to, keeping the order they arrive
/// in — the engine already sorted them by wiki and then by page.
fn by_wiki(hits: &[PageHit]) -> Vec<(&str, &str, Vec<&PageHit>)> {
    let mut out: Vec<(&str, &str, Vec<&PageHit>)> = Vec::new();
    for hit in hits {
        match out.last_mut() {
            Some((wiki_id, _, group)) if *wiki_id == hit.wiki_id => group.push(hit),
            _ => out.push((&hit.wiki_id, &hit.wiki_title, vec![hit])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(wiki_id: &str, page: &str) -> PageHit {
        PageHit {
            wiki_id: wiki_id.to_owned(),
            wiki_title: wiki_id.to_uppercase(),
            page: page.to_owned(),
            excerpt: "…".to_owned(),
        }
    }

    #[test]
    fn hits_group_under_one_heading_per_wiki() {
        let hits = vec![
            hit("alice", "a.md"),
            hit("alice", "b.md"),
            hit("bob", "c.md"),
        ];
        let grouped = by_wiki(&hits);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "alice");
        assert_eq!(grouped[0].2.len(), 2);
        assert_eq!(grouped[1].0, "bob");
        assert_eq!(grouped[1].2.len(), 1);
    }
}
