// SPDX-License-Identifier: AGPL-3.0-or-later
//! `/cite/<bi_id>` — citation-handle resolver.
//!
//! Public redirect route that translates a citation ID of
//! `wiki_briefing_items` into a deep-link to the wiki page + anchor it
//! points at. The smart consumer renders short clickable URLs of the
//! form `/cite/bi_42` in its chat output; the user clicks and lands on
//! the destination page at the right heading.
//!
//! ## Auth posture
//!
//! **The resolver itself is anonymous** — no session cookie required,
//! no `SessionUser` / `AdminUser` extractor on the handler. The route
//! performs *only* the translation (`bi_id` → wiki page URL). Access
//! control fires on the destination `/dashboard/wiki/<wiki_id>/<path>`
//! page, which already runs through the session middleware. This keeps
//! the URL short, copy-pasteable, and embeddable in agent responses
//! even when the recipient is not logged in (they will be redirected
//! to `/dashboard/login` on the destination if needed).
//!
//! ## Algorithm
//!
//! 1. Accept `bi_<N>` *or* bare `N` on the path param; reject anything
//!    else as `404`. The `bi_<N>` shape is the canonical user-facing
//!    form (see [`mwe_core::briefing::BriefingItem::briefing_item_id`])
//!    so smart consumers can paste the same string the API returned.
//! 2. `SELECT target_cite, wiki_id FROM wiki_briefing_items WHERE id = ?`.
//!    `Row not found` → `404`. `target_cite IS NULL` → `404` (briefing
//!    item exists but does not point at a specific anchor).
//! 3. `parse_cite(target_cite)` via the shared utility. On
//!    parse error → `404` (corrupt cite — shouldn't happen because
//!    `notify_append` validates on the way in, but the resolver is
//!    defensive).
//! 4. Compose the destination URL
//!    `/dashboard/wiki/<wiki_id>/view/<path>` (with `#<anchor>`
//!    appended when present) and return `302 Found`.
//!
//! The destination route `/dashboard/wiki/:id/view/*path` is the
//! inline-comment view. axum 0.7's `matchit` router cannot host the
//! bare `/wiki/:id/<path>` capture alongside the existing
//! `/wiki/:id/edit/*path` editor route (overlapping captures panic at
//! startup), so the destination uses the `/view/` prefix. The
//! resolver redirects to this prefix to stay consistent. The textual
//! editor `/dashboard/wiki/:id/edit/<path>` is *not* the chosen target
//! because the spec pins the reading view.
//!
//! ## Scope guard
//!
//! No write path, no inline-comment rendering, no comment popup — those
//! live elsewhere. This module is a single read-only handler.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::Redirect;
use axum::routing::get;
use mwe_core::briefing::{parse_bi_id, parse_cite};

use crate::error::{DashboardError, Result};
use crate::state::DashboardState;

/// Sub-router exposing the resolver under whichever mount point the
/// caller picks. The dashboard router (`/dashboard/*`) merges this for
/// the discoverable alias `/dashboard/cite/:bi_id`; `mwe-mcp-server`
/// nests the same router at the top level so the canonical short form
/// `/cite/:bi_id` works as well.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/cite/:bi_id", get(resolve))
}

/// GET `/cite/:bi_id` — citation-handle resolver.
async fn resolve(
    State(state): State<DashboardState>,
    AxumPath(bi_id): AxumPath<String>,
) -> Result<Redirect> {
    let id = parse_bi_id(&bi_id).ok_or(DashboardError::NotFound)?;

    let row: Option<(Option<String>, String)> =
        sqlx::query_as("SELECT target_cite, wiki_id FROM wiki_briefing_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;

    let (target_cite, _wiki_id) = row.ok_or(DashboardError::NotFound)?;
    let cite = target_cite.ok_or(DashboardError::NotFound)?;

    // `parse_cite` already validates the whole handle (scheme prefix,
    // wiki_id charset, path non-empty, anchor charset). On the rare
    // corrupt-row case we treat it as a 404 too — the resolver hides
    // the malformed value rather than exposing the parse error.
    let parsed = parse_cite(&cite).map_err(|_| DashboardError::NotFound)?;

    // Spec destination — the read-only viewer. The `/view/` prefix
    // disambiguates from the editor
    // sibling `/wiki/:id/edit/*path` under axum 0.7 (overlapping
    // captures panic at startup). The `<path>` segment already
    // contains the forward slashes that map onto the URL hierarchy;
    // the anchor (if any) appears after `#` per the citation handle
    // format.
    let location = if let Some(anchor) = parsed.anchor.as_deref() {
        format!(
            "/dashboard/wiki/{}/view/{}#{}",
            parsed.wiki_id.as_str(),
            parsed.path,
            anchor,
        )
    } else {
        format!(
            "/dashboard/wiki/{}/view/{}",
            parsed.wiki_id.as_str(),
            parsed.path,
        )
    };

    Ok(Redirect::to(&location))
}

// Wire-shape tests for `parse_bi_id` live in
// `mwe_core::briefing::tests`, where the helper is defined so the MCP
// `wiki_admin_push.mark_processed` handler and the dashboard `/cite/`
// route share one definition.
