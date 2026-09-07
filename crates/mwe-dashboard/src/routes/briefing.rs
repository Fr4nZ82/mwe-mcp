// SPDX-License-Identifier: AGPL-3.0-or-later
//! Marking one pending comment read, now, without waiting for REM.
//!
//! Single POST route — `POST /dashboard/wiki/:id/briefing-items/:bi_id/process`.
//! On a **smart** wiki the handler calls the shared
//! [`mwe_core::rem::briefing_processor::process_briefing_item`] core
//! function (the same one REM's mark-passive path calls) and lands the
//! reader back on the page the comment names, where the block
//! they just cleared is gone from the inline list.
//!
//! The button that calls it is the "Mark as read" control on each
//! comment block of a smart wiki's page view
//! ([`crate::routes::wiki_view`]).
//!
//! ## Standard wikis are refused
//!
//! A comment on a **standard** page is applied by the nightly cycle as a
//! fact op (`correct` / `remove` / `add`), not mark-passive drained — so
//! this endpoint **refuses** a standard-wiki row with `400`. Draining it
//! synchronously would stamp `processed_at` and the cycle would then never
//! action-take it; and a memory edit must never be a user-triggered
//! token-burning click. The comment stays parked until the next cycle.
//!
//! ## Auth posture
//!
//! [`SessionUser`] gate only: any logged-in dashboard operator can drain
//! a row from a wiki they can see — there is no sender-specific
//! authorship gate (the comment author is recorded on the row via
//! `author_sender_id` regardless). The destination wiki view's own
//! read-access check (`enforce_read_access_or_not_found` in
//! `wiki_view.rs`) fires when the redirect lands the user on the detail
//! page; the check is not duplicated on the way in, because a user who
//! cannot read the wiki cannot see the row to address.
//!
//! ## Mark-passive policy
//!
//! See the docstring of
//! [`mwe_core::rem::briefing_processor`] for the policy rationale.
//! In short: the processor stamps `processed_at = NOW()` after a
//! pro-forma read of the cited context. No supersede / promote /
//! archive on the target fact: the processor records that the item was
//! read, and what to do about it is the operator's call.

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use mwe_core::briefing::parse_bi_id;
use mwe_core::rem::briefing_processor::{self, ProcessError};
use mwe_core::types::WikiId;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::state::DashboardState;

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new().route(
        "/wiki/:id/briefing-items/:bi_id/process",
        post(submit_process),
    )
}

/// `POST /dashboard/wiki/:id/briefing-items/:bi_id/process`.
///
/// Drains one briefing-item synchronously and redirects back to the
/// wiki view. Both path parameters are validated: the `id` must parse
/// as a [`WikiId`] and the `bi_id` must parse as the canonical
/// `bi_<N>` form (or a bare positive integer, same rule the cite
/// resolver uses).
///
/// The handler intentionally does **not** check that the row belongs
/// to the wiki named on the path — the core processor scopes its
/// work to the row's own `wiki_id` column. A path-vs-row mismatch
/// (somebody crafted the URL by hand) still redirects back to the
/// path the user is on; the row is processed against whichever wiki
/// actually owns it.
async fn submit_process(
    State(state): State<DashboardState>,
    _user: SessionUser,
    AxumPath((id, bi_id_raw)): AxumPath<(String, String)>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id)
        .map_err(|e| DashboardError::BadRequest(format!("invalid wiki id: {e}")))?;
    let bi_id = parse_bi_id(&bi_id_raw).ok_or_else(|| {
        DashboardError::BadRequest(format!(
            "invalid briefing-item id {bi_id_raw:?} (expected bi_<N> or <N>)"
        ))
    })?;

    // A comment on a standard page is applied by the nightly cycle as a
    // fact op (correct / remove / add), not mark-passive drained. Refuse the
    // button for standard wikis — draining it here would stamp
    // `processed_at` and the cycle would then never action-take it. A memory
    // edit must never be a user-triggered token-burning click; it waits for
    // the next cycle. A smart wiki keeps the button: its comments are the
    // consumer's inbox, and draining one is the whole act.
    let row_wiki: Option<String> =
        sqlx::query_scalar("SELECT wiki_id FROM wiki_briefing_items WHERE id = ?")
            .bind(bi_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| DashboardError::Internal(format!("briefing lookup: {e}")))?;
    let Some(row_wiki) = row_wiki else {
        return Err(DashboardError::NotFound);
    };
    if is_standard_wiki(&memory.tree, &row_wiki)? {
        return Err(DashboardError::BadRequest(
            "a comment on a standard wiki is applied by the nightly cycle, which \
             reads it and changes the facts; it cannot be submitted by hand"
                .to_owned(),
        ));
    }

    match briefing_processor::process_briefing_item(&state.pool, &memory.tree, bi_id).await {
        Ok(outcome) => {
            tracing::info!(
                wiki = %wiki_id.as_str(),
                bi_id,
                outcome = ?outcome,
                "dashboard briefing-item synchronous process"
            );
        },
        Err(ProcessError::NotFound(_)) => return Err(DashboardError::NotFound),
        Err(
            e @ (ProcessError::Db(_) | ProcessError::Wiki(_) | ProcessError::InvalidWikiId { .. }),
        ) => {
            return Err(DashboardError::Internal(format!("briefing_processor: {e}")));
        },
    }

    // Back to where the button was. The row's own `target_cite` names
    // that page, so the destination is derived server-side rather than
    // taken from the request: a caller-supplied return address on a POST
    // is an open redirect. A row with no usable cite (or one pointing at
    // another wiki) falls back to the wiki index.
    Ok(Redirect::to(&return_to(&state, bi_id, &wiki_id).await).into_response())
}

/// The page to land on after a comment is marked read: the page its
/// `target_cite` points at, else the wiki index.
async fn return_to(state: &DashboardState, bi_id: i64, wiki_id: &WikiId) -> String {
    let index = format!("/dashboard/wiki/{}", wiki_id.as_str());
    let cite: Option<Option<String>> =
        sqlx::query_scalar("SELECT target_cite FROM wiki_briefing_items WHERE id = ?")
            .bind(bi_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    let Some(Some(cite)) = cite else { return index };
    match mwe_core::briefing::parse_cite(&cite) {
        Ok(parsed) => format!(
            "/dashboard/wiki/{}/view/{}",
            parsed.wiki_id.as_str(),
            parsed.path
        ),
        Err(_) => index,
    }
}

/// Whether the wiki named `wiki_id` is **standard** — every wiki that is
/// not smart (read from the per-wiki `_meta.md` smart flag). Their
/// comments the nightly cycle applies as fact ops, so the synchronous
/// button is refused for them. A wiki that no longer resolves (deleted)
/// reads as not-standard — the row's own processor handles the
/// `WikiNotFound` case downstream.
fn is_standard_wiki(tree: &mwe_core::wiki::WikiTree, wiki_id: &str) -> Result<bool> {
    let smart = tree
        .walk()
        .map_err(|e| DashboardError::Internal(format!("tree walk: {e}")))?
        .into_iter()
        .find(|d| d.meta.wiki_id.as_str() == wiki_id)
        .map(|d| d.meta.smart);
    // Unknown / deleted wiki → not standard (fall through downstream).
    Ok(smart.is_some_and(|c| !c))
}

/// Same `require_memory` shape the sibling route modules use. Memory
/// handles are wired only when `mwe-mcp serve` starts with a workdir
/// (identity-console builds skip them).
fn require_memory(state: &DashboardState) -> Result<&crate::state::MemoryHandles> {
    state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })
}
