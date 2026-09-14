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
//! **The redirect itself tells somebody where a page is**, which is why
//! this route answers nobody it has not recognised. The ids are
//! consecutive integers, so anyone can walk them; a `Location` carrying
//! a wiki and a page path is the name of that page handed over, and a
//! page's name is usually the news. Access control on the destination
//! arrives one hop too late — the header has already been read.
//!
//! So: no session, and the visitor goes to the sign-in page with the
//! CITE handle to come back to, never the destination. With a session,
//! the redirect happens only when that reader can read at least one
//! fact of the page it points at
//! ([`mwe_core::fact_index::readable_fact_on_page`]); otherwise the
//! answer is the one a handle that does not exist gets, so the two
//! cannot be told apart.
//!
//! **The short form forwards.** The session cookie is scoped to
//! `/dashboard`, so a browser sends it to the dashboard alias and to
//! nothing above it: the canonical `/cite/:bi_id` at the root could
//! never see a reader, and would bounce everybody to sign in. It
//! forwards to the alias instead — the same short URL to paste, one hop
//! more, and the checking happens where the cookie is.
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
//! 4. Check the reader against the page the cite names; a reader who
//!    can read nothing of it gets the same `404`.
//! 5. Compose the destination URL
//!    `/dashboard/wiki/<wiki_id>/view/<path>` (with `#<anchor>`
//!    appended when present) and return `302 Found`.
//!
//! The destination route `/dashboard/wiki/:id/view/*path` is the
//! inline-comment view. The `/view/` prefix keeps the greedy `*path`
//! capture from overlapping its `comment/` sibling, which axum 0.7's
//! `matchit` router panics on at startup; the resolver points at the
//! same prefix.
//!
//! ## Scope guard
//!
//! No write path, no inline-comment rendering, no comment popup — those
//! live elsewhere. This module is a single read-only handler.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::CookieJar;
use mwe_core::briefing::{parse_bi_id, parse_cite};
use mwe_core::{enrollment, fact_index};

use crate::auth::session_of;
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

/// The same short URL, mounted at the root, forwarding to the alias.
///
/// The session cookie is `Path=/dashboard`, so nothing above that prefix is
/// ever shown a reader. A resolver there could only bounce everybody to sign
/// in — including the person who is already signed in — so it sends the
/// browser one hop down to where the cookie is sent and the reader can be
/// recognised. Nothing but the handle the visitor already had travels in that
/// hop, and a handle that is not one is refused here rather than forwarded.
pub fn root_router() -> Router<DashboardState> {
    Router::new().route("/cite/:bi_id", get(forward))
}

/// GET `/cite/:bi_id` at the root — forward to the dashboard alias.
async fn forward(AxumPath(bi_id): AxumPath<String>) -> Result<Redirect> {
    // Rebuilt from the parsed number, never echoed: whatever shape the
    // visitor typed, what leaves here is the canonical one.
    let id = parse_bi_id(&bi_id).ok_or(DashboardError::NotFound)?;
    Ok(Redirect::to(&format!("/dashboard/cite/bi_{id}")))
}

/// GET `/dashboard/cite/:bi_id` — citation-handle resolver.
async fn resolve(
    State(state): State<DashboardState>,
    jar: CookieJar,
    AxumPath(bi_id): AxumPath<String>,
) -> Result<Response> {
    let id = parse_bi_id(&bi_id).ok_or(DashboardError::NotFound)?;

    // A stranger is sent to sign in, and what they are sent back to is this
    // handle — never the page it names. The check is the panel's own, run
    // here because this route sits outside the layer that normally runs it.
    let Some(user) = session_of(&state, &jar).await else {
        return Ok(Redirect::to(&format!(
            "/dashboard/login?next={}",
            crate::urlenc::query_value(&format!("/dashboard/cite/bi_{id}"))
        ))
        .into_response());
    };

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

    // The reading view. The `<path>` segment already contains the
    // forward slashes that map onto the URL hierarchy; the anchor (if
    // any) appears after `#` per the citation handle format.
    // The reader against the page the handle names. A reader who can read
    // nothing of it is told what somebody holding a handle to nothing is told,
    // so a walk through the ids learns only which ids exist — which the ids
    // being consecutive already says.
    let memory = state
        .memory
        .as_ref()
        .ok_or_else(|| DashboardError::Internal("no memory handles".to_owned()))?;
    let handle = memory
        .tree
        .locate(&parsed.wiki_id)
        .map_err(|_| DashboardError::NotFound)?;
    let source_path = handle.source_path(std::path::Path::new(&parsed.path));
    let groups = enrollment::groups_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?;
    if !fact_index::readable_fact_on_page(&state.pool, &source_path, &user.sender_id, &groups)
        .await
        .map_err(|e| DashboardError::Internal(format!("readable_fact_on_page: {e}")))?
    {
        return Err(DashboardError::NotFound);
    }

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

    Ok(Redirect::to(&location).into_response())
}

// Wire-shape tests for `parse_bi_id` live in
// `mwe_core::briefing::tests`, where the helper is defined so the MCP
// `wiki_admin_push.mark_processed` handler and the dashboard `/cite/`
// route share one definition.
