// SPDX-License-Identifier: AGPL-3.0-or-later
//! Briefing-item synchronous Submit endpoint.
//!
//! Single POST route — `POST /dashboard/wiki/:id/briefing-items/:bi_id/process`.
//! For a **structured** non-smart wiki the handler calls the shared
//! [`mwe_core::rem::briefing_processor::process_briefing_item`] core
//! function (the same one REM's mark-passive path calls) and
//! redirects back to the wiki view so the operator immediately sees
//! the row drained from the inline comment list.
//!
//! ## Standard wikis are refused
//!
//! A comment on a **standard** page is applied by the REM dream as a fact op
//! (`correct` / `remove` / `add`), not mark-passive drained — so this endpoint
//! **refuses** a standard-wiki row with `400`. Draining it synchronously would
//! stamp `processed_at` and the dream would then never action-take it; and a
//! memory edit must never be a user-triggered token-burning click. The comment
//! stays parked until the next consolidation. See
//! [the compiler note](../../../../wiki/design-notes/narrative-compiler.md).
//!
//! ## Why a separate route file
//!
//! This endpoint is being shipped in parallel with the `wiki_admin_notify`
//! gate matrix on the tool-side and the form-to-chat bridge on the
//! Facts edit form. Each worktree keeps its surface
//! disjoint so the merges don't collide. The Submit endpoint touches
//! nothing in `wiki_view.rs` (the comment **read** view) — it lives
//! alongside it as a sibling router that the dashboard `build()`
//! mounts at the same level.
//!
//! ## Auth posture
//!
//! [`SessionUser`] gate only. The
//! Submit button "lives on the non-smart wiki view" and any
//! logged-in dashboard operator can legitimately drain a row from any
//! wiki they can see — there is no sender-specific authorship gate
//! (the comment author is recorded on the row via
//! `author_sender_id` regardless). The destination wiki view's own
//! read-access check (`enforce_read_access_or_not_found` in
//! `wiki_view.rs`) fires when the redirect lands the user on the
//! detail page; we don't duplicate the check on the way in, because a
//! user who cannot read the wiki cannot navigate to a page that has
//! a Submit button on it in the first place.
//!
//! ## Mark-passive policy
//!
//! See the docstring of
//! [`mwe_core::rem::briefing_processor`] for the policy rationale.
//! In short: the processor stamps `processed_at = NOW()` after a
//! pro-forma read of the cited context. No supersede / promote /
//! archive on the target fact in the MVP — that is a deferred
//! evolution tracked in the roadmap.

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

    // A comment on a NARRATIVE page is applied by the REM dream as a fact
    // op (correct / remove / add), not mark-passive drained. Refuse the
    // synchronous Submit for standard wikis — draining it here would stamp
    // `processed_at` and the dream would then never action-take it. A memory
    // edit must never be a user-triggered token-burning click; it waits for the
    // next consolidation. Structured non-smart wikis keep the Submit.
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
            "comments on standard wikis are applied automatically by the next memory \
             consolidation (REM); they cannot be submitted manually"
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

    Ok(Redirect::to(&format!("/dashboard/wiki/{}", wiki_id.as_str())).into_response())
}

/// Whether the wiki named `wiki_id` is **standard** — every wiki that is
/// not smart (read from the per-wiki `_meta.md` smart flag). Their
/// comments the REM dream applies as fact ops, so the synchronous
/// Submit is refused for them. A wiki that no longer resolves (deleted)
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
