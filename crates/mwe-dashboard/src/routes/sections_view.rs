// SPDX-License-Identifier: AGPL-3.0-or-later
//! The smart-family half of the memory browser: a read-only listing of
//! the **section index** (`wiki_sections`).
//!
//! The Facts page browses `fact_index` — the governed claims of standard
//! wikis. A smart wiki has none of those: its pages are chunked into
//! heading-delimited sections and indexed for search
//! ([`mwe_core::sections`]). Same split the wiki explorer already makes
//! between its Standard and Smart tabs, applied to what the engine
//! indexed rather than to the wikis themselves — so each row shows up
//! under exactly one tab.
//!
//! Read-only on purpose. A section has no lifecycle to act on: it is
//! re-derived from its page on every reindex, so there is nothing to
//! supersede, forget, re-own or date. The operator's lever is the page —
//! every row deep-links to it in the wiki viewer, and editing the page
//! (or having the smart consumer push it) is what changes the index.
//!
//! Access is resolved **per wiki**, from the `smart_wikis` registry:
//! owner plus the `_meta.shared_with` roster, the same set
//! [`mwe_core::recall::search_sections`] uses. The admin reveal lens
//! lists every wiki's sections, matching the Facts page.

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use mwe_core::acl::can_read;
use mwe_core::enrollment;
use mwe_core::sections::{self, BrowseFilter, SectionSummary};
use mwe_core::types::Acl;
use serde::Deserialize;

use crate::auth::session::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Hard cap on rows pulled for one listing. Mirrors the Facts browser's
/// scan cap: past it the total is a lower bound, flagged in the pager.
const MAX_SCAN_ROWS: usize = 2_000;

/// Rows per page, and the allowed sizes the form offers.
const DEFAULT_PAGE_SIZE: usize = 50;
const PAGE_SIZES: [usize; 4] = [25, 50, 100, 200];

/// Characters of section text shown in the table before truncation.
const PREVIEW_CHARS: usize = 160;

pub fn router() -> Router<DashboardState> {
    Router::new().route("/facts/sections", get(index))
}

#[derive(Debug, Default, Deserialize)]
pub struct SectionsFilters {
    #[serde(default)]
    wiki_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    page: Option<usize>,
    #[serde(default)]
    page_size: Option<usize>,
}

impl SectionsFilters {
    /// Blank form fields arrive as `Some("")`; treat them as absent so an
    /// empty box does not become a `LIKE '%%'` no-op predicate.
    fn cleaned(value: Option<&String>) -> Option<String> {
        value
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    fn to_core_filter(&self, limit: usize) -> BrowseFilter {
        BrowseFilter {
            wiki_id: Self::cleaned(self.wiki_id.as_ref()),
            path_contains: Self::cleaned(self.path.as_ref()),
            text_contains: Self::cleaned(self.text.as_ref()),
            limit,
        }
    }

    /// Re-emit the active filters as a query string, so the pager and the
    /// page-size links preserve them.
    fn query_string(&self, page: usize, page_size: usize) -> String {
        let mut parts = vec![format!("page={page}"), format!("page_size={page_size}")];
        for (key, value) in [
            ("wiki_id", Self::cleaned(self.wiki_id.as_ref())),
            ("path", Self::cleaned(self.path.as_ref())),
            ("text", Self::cleaned(self.text.as_ref())),
        ] {
            if let Some(v) = value {
                parts.push(format!("{key}={}", super::facts::url_encode(&v)));
            }
        }
        parts.join("&")
    }
}

fn normalise_pagination(filters: &SectionsFilters) -> (usize, usize) {
    let page = filters.page.unwrap_or(1).max(1);
    let page_size = filters
        .page_size
        .filter(|s| PAGE_SIZES.contains(s))
        .unwrap_or(DEFAULT_PAGE_SIZE);
    (page, page_size)
}

/// The Facts / Sections tab bar — the memory-browser counterpart of the
/// wiki explorer's Standard / Smart split.
pub(super) fn corpus_tabs(sections_active: bool) -> Markup {
    components::tab_bar(&[
        ("Facts", "/dashboard/facts", !sections_active),
        ("Sections", "/dashboard/facts/sections", sections_active),
    ])
}

async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Query(filters): Query<SectionsFilters>,
) -> Result<Html<String>> {
    let (page, page_size) = normalise_pagination(&filters);
    let reveal = crate::reveal::active(&user, &jar);

    // Which smart wikis may this viewer read? One decision per wiki, from
    // the registry — the wiki-level ACL a smart wiki actually has. Under
    // the reveal lens an admin sees every wiki's sections.
    let registry = sections::list_smart_wikis(&state.pool)
        .await
        .map_err(|e| DashboardError::Internal(format!("list_smart_wikis: {e}")))?;
    let sender_groups = if reveal {
        Vec::new()
    } else {
        enrollment::groups_for(&state.pool, &user.sender_id)
            .await
            .map_err(|e| DashboardError::Internal(format!("enrollment::groups_for: {e}")))?
    };
    let readable: Vec<String> = registry
        .into_iter()
        .filter(|w| {
            reveal
                || can_read(
                    &Acl {
                        owner: Some(w.owner_id.clone()),
                        allow: w.shared_with.clone(),
                    },
                    &user.sender_id,
                    &sender_groups,
                    None,
                )
        })
        .map(|w| w.wiki_id)
        .collect();

    let rows = sections::browse(
        &state.pool,
        &readable,
        &filters.to_core_filter(MAX_SCAN_ROWS),
    )
    .await
    .map_err(|e| DashboardError::Internal(format!("sections::browse: {e}")))?;

    let total_is_estimate = rows.len() >= MAX_SCAN_ROWS;
    let total = rows.len();
    let total_pages = total.div_ceil(page_size).max(1);
    let page = page.min(total_pages);
    let start = (page - 1).saturating_mul(page_size).min(total);
    let end = (start + page_size).min(total);
    let page_rows = &rows[start..end];

    tracing::debug!(
        sender_id = %user.sender_id,
        readable_wikis = readable.len(),
        page,
        page_size,
        total,
        total_is_estimate,
        "dashboard: /facts/sections loaded"
    );

    Ok(Html(render_index(
        &user,
        &filters,
        page,
        page_size,
        page_rows,
        total,
        total_pages,
        total_is_estimate,
        reveal,
        readable.is_empty(),
    )))
}

#[allow(
    clippy::too_many_arguments,
    reason = "a render function threading the page state it displays; grouping it into a struct would only move the argument list"
)]
fn render_index(
    user: &SessionUser,
    filters: &SectionsFilters,
    page: usize,
    page_size: usize,
    page_rows: &[SectionSummary],
    total: usize,
    total_pages: usize,
    total_is_estimate: bool,
    reveal: bool,
    no_readable_wikis: bool,
) -> String {
    let body = html! {
        (corpus_tabs(/* sections_active */ true))

        @if reveal {
            (crate::reveal::banner())
        }

        p.muted {
            "What the engine indexed from your "
            a href="/dashboard/wiki/smart" { "smart wikis" }
            " — each page is cut into sections at its headings, and each "
            "section is what a search can match. These are not facts: they "
            "carry no owner, no validity and no history, because they are "
            "re-derived from the page every time it changes."
        }
        p.muted {
            "Read-only. To change what is indexed, edit the page — the row "
            "links to it."
        }

        (filter_form(filters, page_size))

        @if no_readable_wikis {
            p.muted { "No smart wiki is readable from this account." }
        } @else if page_rows.is_empty() {
            p.muted { "No sections to show." }
        } @else {
            table.facts-table.compact {
                thead { tr {
                    th { "wiki" }
                    th { "page" }
                    th { "#" }
                    th { "heading" }
                    th { "text" }
                    th { "recalls 30d" }
                    th { "last recall" }
                    th { "indexed" }
                } }
                tbody {
                    @for row in page_rows {
                        tr {
                            td { code { (row.wiki_id) } }
                            td {
                                @match page_href(&row.wiki_id, &row.source_path) {
                                    Some(href) => {
                                        a href=(href) title=(row.source_path) {
                                            (page_label(&row.source_path))
                                        }
                                    },
                                    None => { (page_label(&row.source_path)) }
                                }
                            }
                            td { (row.section_ord) }
                            td {
                                @match row.heading_path.as_deref() {
                                    Some(h) if !h.is_empty() => { (components::truncate_chars(h, 48)) },
                                    _ => { span.muted { "—" } }
                                }
                            }
                            td { (components::truncate_chars(&row.text, PREVIEW_CHARS)) }
                            td { (row.recall_count_30d) }
                            td {
                                @match row.last_recall_at.as_deref() {
                                    Some(t) if !t.is_empty() => { (fmt_ts(t)) },
                                    _ => { span.muted { "—" } }
                                }
                            }
                            td { (fmt_ts(&row.updated_at)) }
                        }
                    }
                }
            }
        }

        (pagination_links(filters, page, page_size, total, total_pages, total_is_estimate))
    };
    layout::authenticated_page("Sections", user, &body)
}

/// Deep-link a section's page into the wiki viewer.
///
/// `source_path` is workdir-relative (`wikis/<…>/page.md`) while the
/// viewer route wants the path **within** the wiki, so the wiki's own
/// directory prefix is stripped. Returns `None` when the path does not
/// sit under the expected prefix — the label then renders unlinked rather
/// than pointing somewhere wrong.
fn page_href(wiki_id: &str, source_path: &str) -> Option<String> {
    let rest = source_path.strip_prefix("wikis/")?;
    // The wiki directory is everything up to the last `/`; the viewer
    // takes the remainder. Sub-wikis nest, so this cannot assume depth 1.
    let (_dir, file) = rest.rsplit_once('/')?;
    Some(format!(
        "/dashboard/wiki/{}/view/{}",
        super::facts::url_encode(wiki_id),
        file
    ))
}

/// The page cell's text: the path without its `wikis/` prefix, which is
/// noise repeated on every row.
fn page_label(source_path: &str) -> String {
    source_path
        .strip_prefix("wikis/")
        .unwrap_or(source_path)
        .to_owned()
}

fn filter_form(filters: &SectionsFilters, page_size: usize) -> Markup {
    html! {
        form.facts-filter method="get" action="/dashboard/facts/sections" {
            div.field-grid {
                p {
                    label for="filter-wiki-id" { "wiki_id" }
                    input id="filter-wiki-id" type="text" name="wiki_id"
                        value=(filters.wiki_id.as_deref().unwrap_or(""))
                        placeholder="e.g. alice-lnprint";
                }
                p {
                    label for="filter-path" { "page contains" }
                    input id="filter-path" type="text" name="path"
                        value=(filters.path.as_deref().unwrap_or(""))
                        placeholder="e.g. auth";
                }
                p {
                    label for="filter-text" { "text contains" }
                    input id="filter-text" type="text" name="text"
                        value=(filters.text.as_deref().unwrap_or(""))
                        placeholder="e.g. recovery codes";
                }
                p {
                    label for="filter-page-size" { "rows" }
                    select id="filter-page-size" name="page_size" {
                        @for size in PAGE_SIZES {
                            option value=(size) selected[size == page_size] { (size) }
                        }
                    }
                }
            }
            p {
                button type="submit" { "Filter" }
                " "
                a href="/dashboard/facts/sections" { "Reset" }
            }
        }
    }
}

fn pagination_links(
    filters: &SectionsFilters,
    page: usize,
    page_size: usize,
    total: usize,
    total_pages: usize,
    total_is_estimate: bool,
) -> Markup {
    let prev_href = (page > 1).then(|| format!("?{}", filters.query_string(page - 1, page_size)));
    let next_href =
        (page < total_pages).then(|| format!("?{}", filters.query_string(page + 1, page_size)));
    html! {
        p.muted {
            @match prev_href {
                Some(href) => { a href=(href) { "← prev" } },
                None => { span.muted { "← prev" } }
            }
            " · page " (page) " of " (total_pages) " · "
            (total) @if total_is_estimate { "+" } " sections · "
            @match next_href {
                Some(href) => { a href=(href) { "next →" } },
                None => { span.muted { "next →" } }
            }
        }
        @if total_is_estimate {
            p.muted {
                "The scan is capped at " (MAX_SCAN_ROWS) " rows, so the total is a "
                "lower bound — narrow the filters to see the rest."
            }
        }
    }
}

/// Trim an ISO-8601 stamp to `YYYY-MM-DD HH:MM:SS` for the table.
fn fmt_ts(raw: &str) -> String {
    let spaced = raw.replacen('T', " ", 1);
    match spaced.get(..19) {
        Some(s) if s.as_bytes().get(10) == Some(&b' ') => s.to_owned(),
        _ => spaced
            .trim_end_matches('Z')
            .trim_end_matches("+00:00")
            .trim_end()
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_href_strips_the_wikis_prefix_and_the_wiki_directory() {
        assert_eq!(
            page_href("alice-lnprint", "wikis/alice/lnprint/04-identity.md").as_deref(),
            Some("/dashboard/wiki/alice-lnprint/view/04-identity.md")
        );
    }

    #[test]
    fn page_href_declines_a_path_outside_the_wikis_tree() {
        assert!(page_href("alice", "somewhere/else.md").is_none());
        assert!(page_href("alice", "wikis/no-directory.md").is_none());
    }

    #[test]
    fn page_label_drops_the_repeated_prefix() {
        assert_eq!(page_label("wikis/alice/proj/x.md"), "alice/proj/x.md");
        assert_eq!(page_label("odd/path.md"), "odd/path.md");
    }

    #[test]
    fn blank_filter_fields_are_treated_as_absent() {
        let filters = SectionsFilters {
            wiki_id: Some("  ".to_owned()),
            path: Some(String::new()),
            text: Some("  auth ".to_owned()),
            ..SectionsFilters::default()
        };
        let core = filters.to_core_filter(10);
        assert!(core.wiki_id.is_none());
        assert!(core.path_contains.is_none());
        assert_eq!(core.text_contains.as_deref(), Some("auth"));
        assert_eq!(core.limit, 10);
    }

    #[test]
    fn query_string_preserves_filters_and_escapes_them() {
        let filters = SectionsFilters {
            wiki_id: Some("alice-lnprint".to_owned()),
            text: Some("recovery codes".to_owned()),
            ..SectionsFilters::default()
        };
        let qs = filters.query_string(3, 50);
        assert!(qs.contains("page=3"));
        assert!(qs.contains("page_size=50"));
        assert!(qs.contains("wiki_id=alice-lnprint"));
        assert!(qs.contains("text=recovery%20codes"));
        assert!(!qs.contains("path="), "absent filters are not emitted");
    }

    #[test]
    fn page_size_falls_back_when_the_query_asks_for_an_unlisted_size() {
        let odd = SectionsFilters {
            page_size: Some(7),
            ..SectionsFilters::default()
        };
        assert_eq!(normalise_pagination(&odd).1, DEFAULT_PAGE_SIZE);
        let ok = SectionsFilters {
            page_size: Some(100),
            ..SectionsFilters::default()
        };
        assert_eq!(normalise_pagination(&ok).1, 100);
        // Page 0 (hand-typed) clamps into range.
        let zero = SectionsFilters {
            page: Some(0),
            ..SectionsFilters::default()
        };
        assert_eq!(normalise_pagination(&zero).0, 1);
    }
}
