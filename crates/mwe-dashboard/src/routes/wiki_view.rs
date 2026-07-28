// SPDX-License-Identifier: AGPL-3.0-or-later
//! Memory-wiki read-only viewer + textual page editor + per-page read
//! view with inline comments + comment write path.
//!
//! Routes:
//!
//! - GET `/dashboard/wiki`                     — the **Standard** tab of
//!   the unified Wikis page: lists every standard-family wiki under
//!   `<workdir>/wikis/` with the cached fact counter. Smart-family wikis
//!   live on the sibling **Smart** tab
//!   ([`super::smart_view::list_smart_wikis`], `/dashboard/wiki/smart`);
//!   the two share one "Wikis" nav entry, switched by the
//!   [`wiki_family_tabs`] bar.
//! - GET `/dashboard/wiki/:id`                 — render
//!   `<wiki_id>/index.md` along with the page list. The body goes
//!   through [`mwe_core::render::render_for_sender`] so the connected
//!   user sees the same declassified view a consumer agent would
//!   receive via `wiki_read` — invisible regions are replaced by
//!   `[redacted]`, the per-region count is surfaced as a badge.
//! - GET `/dashboard/wiki/:id/view/*path`      — read-only viewer for
//!   an arbitrary page. Renders the body through
//!   `render_for_sender`, then walks each line and interpolates any
//!   pending `wiki_briefing_items.target_cite` comment after the
//!   matching heading. Comments whose anchor is missing from the
//!   current body land in a footer `<section class="orphaned-comments">`
//!   so the operator does not lose the feedback when a heading is
//!   renamed. The spec ideal `/dashboard/wiki/:id/<path>` cannot
//!   coexist with the editor sibling `/wiki/:id/edit/*path` under
//!   axum 0.7's `matchit` router (overlapping captures panic at
//!   startup); we therefore use the `/view/` prefix and keep the
//!   `/cite/` resolver aligned with the new destination.
//!
//!   The optional query parameter `?mode=comment` enables
//!   "comment mode": each heading sprouts a `+ Comment` link that
//!   points at the GET form below, and a `Stop commenting` toggle
//!   sits at the top. The default view (no query param) is the clean
//!   read-only surface — no per-heading affordance — so a reader
//!   opting only to consume the page does not get a noisy UI.
//! - GET `/dashboard/wiki/:id/edit/*path`      — textual editor for
//!   the page at `path`. The raw free-text editor is a
//!   **discouraged escape hatch**, gated per roadmap 6j: **hard-
//!   forbidden on smart wikis** (the smart consumer is the sole
//!   writer — surfaced as a `404` so the editor is not even
//!   discoverable), **admin-only on standard wikis** (non-admins get
//!   a `403`). The owner check still applies on top
//!   ([`mwe_core::wiki_admin`]'s gate; non-owners get a `404`). There
//!   is no discoverable link to this route from the normal page view —
//!   the blessed write channels are the inline **comments**, the
//!   **operative chat**, and the **structured fact actions**
//!   (ACL / validity / supersede on `/dashboard/facts`).
//! - POST `/dashboard/wiki/:id/edit/*path`     — save the textual
//!   submission, under the same smart-forbidden / admin-only gate as
//!   the GET form. Funnels through
//!   [`mwe_core::wiki_admin::push`] with
//!   [`mwe_core::wiki_admin::ActorKind::Dashboard`] so the write
//!   lands in `wiki_admin_op_log` exactly like an MCP push — same
//!   audit machinery, same revert handler downstream.
//! - GET `/dashboard/wiki/:id/comment/*path?anchor=<slug>` — render
//!   the small "leave a comment" form for the heading addressed by
//!   `?anchor=`. Read-access required on the wiki —
//!   anyone who can read the page can leave a comment (owner +
//!   `shared_with` users / groups / global) so a team member with a
//!   shared smart-wiki can feed feedback to the smart consumer
//!   without owning the wiki.
//! - POST `/dashboard/wiki/:id/comment/*path?anchor=<slug>` — persist
//!   the comment as a row in `wiki_briefing_items` with
//!   `source_kind='dashboard_comment'`, `kind='external'`,
//!   `author_sender_id=<signed-in user>`, `target_cite='wiki://<id>/<path>#<anchor>'`.
//!   On success redirects 302 back to `/dashboard/wiki/:id/view/*path`
//!   (read mode, no `?mode=comment`) so the operator sees the
//!   comment freshly interpolated by the read view.
//!
//!   The write path is a direct `INSERT INTO wiki_briefing_items`
//!   rather than a layer on top of [`mwe_core::briefing::notify`]:
//!   `notify` enforces the smart-family gate (we want comments
//!   on every wiki the operator can read), does not
//!   expose the new `author_sender_id` column, does not accept the
//!   new `source_kind='dashboard_comment'` value (the enum widens
//!   only at the SQL level — see migration 0027 §4), and imposes a
//!   `50 notify/wiki/h` cap that is calibrated for REM/consumer
//!   traffic, not for a human reviewer. Bypassing keeps both surfaces
//!   coherent: `notify` stays the smart-wiki inbox channel; this
//!   handler stays the dashboard feedback channel. Either way the
//!   resulting row is identical at the DB level and surfaces in the
//!   read viewer the same way.
//!
//!   Range-selection comments (highlight a span of prose, anchor by
//!   byte range) are out of scope for MVP: the citation grammar
//!   (`wiki://<id>/<path>#<heading-slug>`) does not yet accept range
//!   fragments, so this milestone implements only "click on heading".
//!   Range support would extend the cite grammar + the read-view
//!   layout helper and is tracked in the deferred list.
//!
//! Writes to `_meta.md` are refused on this route with a clear
//! validation error: the metadata edit surface lives on
//! `/dashboard/wiki/:id/sharing` so the two flows do not
//! conflate. Future metadata fields will land their own dedicated
//! routes too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use mwe_core::briefing::{
    BODY_MAX_BYTES, HeadingAnchor, compose_cite, extract_anchors_from_markdown, parse_cite,
};
use mwe_core::dream_journal::DreamKind;
use mwe_core::enrollment;
use mwe_core::fact_index;
use mwe_core::page::DeletionMode;
use mwe_core::render;
use mwe_core::types::{Principal, WikiId};
use mwe_core::wiki::{META_FILENAME, wiki_get_meta, wiki_list_pages, wiki_read};
use mwe_core::wiki_admin::{
    ActorKind, AdminCaller, AdminError, PushMode, PushPage, PushRequest, resolve_read_access,
};
use mwe_core::wiki_delete;
use serde::Deserialize;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::md_render;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    // Route shape note: the original spec sketched the form
    // routes as `/wiki/:id/view/*path/comment`, but axum 0.7's
    // `matchit` cannot route a greedy capture (`*path`) followed by
    // a literal suffix — the suffix is consumed by the capture. We
    // use a sibling `comment/` prefix instead, which lives next to
    // `view/` and `edit/` without overlap. The handler still scopes
    // the comment to the page identified by `*path`, just from a
    // different URL stem.
    Router::new()
        .route("/wiki", get(list))
        .route("/wiki/:id", get(view))
        .route("/wiki/:id/export", get(export_archive))
        .route("/wiki/:id/delete", get(delete_confirm).post(delete_apply))
        .route("/wiki/:id/view/*path", get(view_page))
        .route("/wiki/:id/edit/*path", get(edit_form).post(submit_edit))
        .route(
            "/wiki/:id/describe/*path",
            get(describe_form).post(submit_describe),
        )
        .route(
            "/wiki/:id/comment/*path",
            get(comment_form).post(submit_comment),
        )
}

// Admin ACL-reveal — the dashboard-wide "show everything" switch and its
// single Settings toggle — lives in [`crate::reveal`]. The wiki read
// surfaces below consult [`crate::reveal::active`] (so a fragment the
// operator could not read via a consumer token renders highlighted via
// `render::render_admin_reveal` instead of `[redacted]`) and show
// [`crate::reveal::banner`] while it is on. Documented in
// the redaction-policy design note.

/// Admin-only download of the wiki subtree as a portable full-marker
/// archive (`mwe_core::export`): every DB-known region travels with its
/// own ACL inline, so the tar stands alone without `engine.db` next to
/// it. Served with `Content-Disposition: attachment` — the dashboard's
/// first download surface.
async fn export_archive(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let export = mwe_core::export::export_wiki_subtree(&state.pool, &memory.tree, &wiki_id)
        .await
        .map_err(|e| match e {
            mwe_core::export::ExportError::Wiki(we) => map_wiki_err(we),
            other => DashboardError::Internal(format!("export {id}: {other}")),
        })?;
    tracing::info!(
        actor = %user.sender_id,
        wiki = %id,
        wikis = export.report.wikis,
        pages = export.report.pages,
        regions_rewritten = export.report.regions_rewritten,
        regions_unindexed = export.report.regions_unindexed,
        media_bundled = export.report.media_bundled,
        media_missing = export.report.media_missing,
        "dashboard wiki export served"
    );
    let filename = format!("{}-export.tar", export.root_dir);
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-tar".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        export.tar_bytes,
    )
        .into_response())
}

/// Confirmation form body of `POST /dashboard/wiki/:id/delete`.
#[derive(Debug, Default, Deserialize)]
struct DeleteWikiForm {
    /// The operator must re-type the wiki id here — a strong confirmation
    /// proportional to how destructive a wiki delete is. A mismatch is
    /// refused server-side.
    #[serde(default)]
    confirm_id: String,
    /// How to dispose of the subtree's facts: `"move"` (the default — evacuate
    /// each foreign-authored fact to its sender's wiki, destroying nothing) or
    /// `"tombstone"` (tombstone every fact). Maps to [`DeletionMode`].
    #[serde(default)]
    disposition: String,
}

/// GET `/dashboard/wiki/:id/delete` — admin-only strong-confirmation page.
///
/// True when the wiki is a **living** principal's identity wiki (its
/// user/group is still enrolled). Drives the delete affordance: a living
/// identity wiki is removed via the user/group flow, an orphaned one is
/// deletable here.
async fn is_live_identity(
    state: &DashboardState,
    wiki_type: &str,
    wiki_id: &WikiId,
) -> Result<bool> {
    match wiki_delete::identity_principal(wiki_type, wiki_id) {
        Some(p) => enrollment::principal_exists(&state.pool, &p)
            .await
            .map_err(|e| DashboardError::Internal(format!("principal_exists: {e}"))),
        None => Ok(false),
    }
}

/// Shows the blast radius (sub-wiki count + active facts that will be
/// tombstoned) and a "type the id to confirm" form. A living principal's
/// identity wiki is refused here with a clear notice (no form) — it is
/// removed through the user / group deletion flow. An **orphaned** identity
/// wiki (its user/group already deleted) gets the form: no other flow can
/// remove it.
async fn delete_confirm(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let subtree = wiki_delete::collect_subtree(&memory.tree, &wiki_id).map_err(map_wiki_err)?;
    let root = subtree
        .iter()
        .find(|d| d.meta.wiki_id == wiki_id)
        .ok_or(DashboardError::NotFound)?;
    // Identity wikis are protected only while their principal is enrolled;
    // an orphan (user/group already deleted) has no other removal path.
    let live_identity = is_live_identity(&state, &root.meta.wiki_type, &wiki_id).await?;
    let orphan_identity = wiki_delete::is_identity_type(&root.meta.wiki_type) && !live_identity;
    let title = root.meta.title.clone();
    let wiki_type = root.meta.wiki_type.clone();
    let smart = root.meta.smart;
    let wikis_removed = subtree.len();

    // Sum active facts across the whole subtree so the operator sees the
    // full blast radius, not just the target wiki's own count.
    let mut facts = 0i64;
    for d in &subtree {
        facts += fact_index::count_active_in_wiki(&state.pool, d.meta.wiki_id.as_str())
            .await
            .map_err(|e| DashboardError::Internal(format!("count_active_in_wiki: {e}")))?;
    }

    let body = html! {
        h2 { "Delete wiki " code { (id) } }
        @if live_identity {
            p.flash.flash-error {
                "This is an identity wiki (" code { (wiki_type) } "). Identity wikis are removed "
                "through the user / group deletion flow, not deleted here."
            }
            p { a href="/dashboard/wiki" { "Back to the list" } }
        } @else {
            @if orphan_identity {
                p.muted {
                    strong { "Orphaned identity wiki" }
                    " — its user/group is no longer enrolled (the memory outlived "
                    "the identity), so this is the only place it can be deleted."
                }
            }
            p.muted {
                "Title: " (title) " · type: " code { (wiki_type) }
                @if smart { " · " span.badge { "smart" } }
            }
            p {
                "Deleting moves the whole directory subtree into "
                code { "<workdir>/trash/" } " — the files are never erased. What happens to the "
                strong { "facts" } " is a separate choice, and it is the one that matters: a "
                "tombstoned fact leaves recall, and putting the directory back does not bring it "
                "back."
            }
            ul {
                li { "Wikis removed (this one + sub-wikis): " strong { (wikis_removed) } }
                li { "Active facts on the subtree: " strong { (facts) } }
            }
            form action=(format!("/dashboard/wiki/{id}/delete")) method="post" {
                (disposition_fieldset())
                p {
                    label for="confirm-id" {
                        "Type the wiki id (" code { (id) } ") to confirm:"
                    }
                }
                input id="confirm-id" type="text" name="confirm_id"
                    autocomplete="off" placeholder=(id);
                p {
                    button type="submit" class="danger" { "Delete this wiki" }
                    " · "
                    a href="/dashboard/wiki" { "Cancel" }
                }
            }
        }
    };
    Ok(Html(layout::authenticated_reading_page(
        chrome,
        "Delete wiki",
        admin.session(),
        &body,
    )))
}

/// The three fact dispositions of a wiki deletion, in the order that ranks
/// them by what they destroy: nothing, your own, everyone's.
///
/// Split out of [`delete_confirm`] because this markup — not the blast-radius
/// numbers above it — is what the operator actually has to read: the files
/// always survive in the trash, so the only irreversible choice on the page is
/// this one.
fn disposition_fieldset() -> Markup {
    html! {
        fieldset {
            legend { "What happens to the facts" }
            p {
                label {
                    input type="radio" name="disposition" value="dissolve" checked;
                    " " strong { "Dissolve" } " (recommended)"
                    " — the structure goes, the knowledge stays. Nothing is tombstoned: "
                    "every fact is freed and re-placed where it belongs across the rest "
                    "of the memory, by the same cartographer that files new facts."
                }
            }
            p {
                label {
                    input type="radio" name="disposition" value="authors";
                    " " strong { "Return to each author" }
                    " — each fact someone else contributed goes back to that person's own "
                    "wiki, intact; the ones "
                    em { "you" }
                    " contributed are tombstoned, and so are facts with no home to return to."
                }
            }
            p {
                label {
                    input type="radio" name="disposition" value="tombstone";
                    " " strong { "Tombstone all" }
                    " — tombstone every fact, including ones contributed by others. They "
                    "leave recall at once but survive as audit tombstones."
                }
            }
        }
    }
}

/// POST `/dashboard/wiki/:id/delete` — admin-only soft-delete apply.
///
/// Refuses unless `confirm_id` matches the path id exactly, then runs the
/// recoverable subtree delete ([`mwe_core::wiki_delete::delete_wiki_subtree`])
/// with the admin's chosen disposition — `move` (sender-keyed evacuation, the
/// default) or `tombstone` (tombstone every fact) — and moves the directory
/// into `<workdir>/trash/`. Redirects 303 back to the wiki list.
async fn delete_apply(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(id): AxumPath<String>,
    HtmlForm(form): HtmlForm<DeleteWikiForm>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    if form.confirm_id.trim() != id {
        return Err(DashboardError::Validation(
            "Confirmation failed: type the exact wiki id to delete it.".to_owned(),
        ));
    }
    let (mode, disposition) = match form.disposition.trim() {
        "tombstone" => (DeletionMode::TombstoneAll, "tombstone"),
        "authors" => (DeletionMode::SenderKeyed, "authors"),
        // Dissolve is the default: the only disposition that destroys no
        // one's contribution — the structure goes, every fact is kept and
        // re-placed.
        _ => (DeletionMode::Dissolve, "dissolve"),
    };
    let deleter = Principal::User(admin.sender_id().to_owned());
    let report =
        wiki_delete::delete_wiki_subtree(&state.pool, &memory.tree, &wiki_id, &deleter, mode)
            .await
            .map_err(map_wiki_delete_err)?;
    tracing::info!(
        actor = admin.sender_id(),
        wiki = %id,
        disposition,
        wikis_removed = report.wikis_removed,
        facts_tombstoned = report.facts_tombstoned,
        facts_evacuated = report.facts_evacuated,
        facts_unplaced = report.facts_unplaced,
        trash = %report.trash_dir.display(),
        "dashboard: wiki subtree soft-deleted to trash"
    );
    // A dissolve leaves its freed facts parked for re-placement: kick the full
    // reorg off now so the Cartografo re-decides where each one belongs while
    // the operator is still looking, instead of the memory sitting lopsided
    // until tonight. A busy gate is fine — the park is persisted on the plan
    // and the next build consumes it either way.
    if matches!(mode, DeletionMode::Dissolve) && report.facts_unplaced > 0 {
        let started = super::dream::spawn_dream(&state, DreamKind::Full);
        tracing::info!(
            wiki = %id,
            facts_unplaced = report.facts_unplaced,
            started,
            "dashboard: dissolve triggered a full reorg to re-place the freed facts"
        );
    }
    // The deleted subtree may have been a web-agent consumer's smart wiki:
    // drain the now-dangling consumer (and its OAuth rows) right away
    // instead of waiting for the next boot sweep. Best-effort.
    match mwe_core::housekeeping::run(&state.pool, &memory.tree).await {
        Ok(hk) if hk.is_noop() => {},
        Ok(hk) => tracing::info!(
            dangling_consumers_removed = hk.dangling_consumers_removed,
            stale_refresh_pruned = hk.stale_refresh_pruned,
            "dashboard: post-delete housekeeping swept"
        ),
        Err(error) => tracing::warn!(%error, "dashboard: post-delete housekeeping failed"),
    }
    Ok(Redirect::to("/dashboard/wiki").into_response())
}

/// Map a [`wiki_delete::WikiDeleteError`] onto a dashboard error.
fn map_wiki_delete_err(e: wiki_delete::WikiDeleteError) -> DashboardError {
    use wiki_delete::WikiDeleteError as E;
    match e {
        E::NotFound(_) => DashboardError::NotFound,
        E::Identity(_, _) => DashboardError::Validation(e.to_string()),
        E::Wiki(we) => map_wiki_err(we),
        E::FactIndex(_) | E::Refile(_) | E::Move { .. } | E::Enrollment(_) | E::Plan(_) => {
            DashboardError::Internal(e.to_string())
        },
    }
}

/// The two tabs of the unified Wikis page: **Standard** (the narrative
/// compiler's family — [`list`]) and **Smart** (the smart-consumer family
/// — [`super::smart_view::list_smart_wikis`]). Both live under the single
/// "Wikis" nav entry; this bar is the only switch between them, so each
/// wiki appears in exactly one tab (the lists filter on the per-wiki
/// `_meta.md` smart flag). Shared by both handlers so the two hrefs cannot
/// drift.
pub(super) fn wiki_family_tabs(smart_active: bool) -> Markup {
    components::tab_bar(&[
        ("Standard", "/dashboard/wiki", !smart_active),
        ("Smart", "/dashboard/wiki/smart", smart_active),
    ])
}

async fn list(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let discovered = memory
        .tree
        .walk()
        .map_err(|e| DashboardError::Internal(format!("walk wikis: {e}")))?;
    // Derived wiki visibility: filter the list to wikis the reader can read ≥1
    // fact in (admin reveal lists all), so the list never leaks the id/title of
    // a wiki whose facts are entirely outside the reader's ACL.
    let reveal = crate::reveal::active(&state, &user, &jar);
    let sender_groups = if reveal {
        Vec::new()
    } else {
        enrollment::groups_for(&state.pool, &user.sender_id)
            .await
            .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?
    };

    // Per-row tuple: (id, title, type, active fact count, live-identity). The
    // flag drives the delete affordance — a living principal's identity wiki
    // is removed through the user/group flow, not deleted here, while an
    // orphaned one (principal already deleted) is deletable. Smart wikis are
    // skipped: they live on the Smart tab (`smart_view::list_smart_wikis`),
    // which surfaces the columns that matter for them (last push, unread
    // briefing) — so each wiki shows up under exactly one tab.
    let mut rows: Vec<(String, String, String, i64, bool)> = Vec::with_capacity(discovered.len());
    for d in discovered {
        if d.meta.smart {
            continue;
        }
        if !reveal
            && !fact_index::wiki_visible_to(
                &state.pool,
                d.meta.wiki_id.as_str(),
                &user.sender_id,
                &sender_groups,
            )
            .await
            .map_err(|e| DashboardError::Internal(format!("wiki_visible_to: {e}")))?
        {
            continue;
        }
        let count = fact_index::count_active_in_wiki(&state.pool, d.meta.wiki_id.as_str())
            .await
            .map_err(|e| DashboardError::Internal(format!("count_active_in_wiki: {e}")))?;
        let live_identity = is_live_identity(&state, &d.meta.wiki_type, &d.meta.wiki_id).await?;
        rows.push((
            d.meta.wiki_id.as_str().to_owned(),
            d.meta.title.clone(),
            d.meta.wiki_type.clone(),
            count,
            live_identity,
        ));
    }
    rows.sort();

    let body = render_wikis_index(
        &user,
        &rows,
        reveal,
        crate::read_only::hides_writes(&state),
        &memory.tree.wikis_dir().display().to_string(),
    );

    Ok(Html(layout::authenticated_page(
        chrome, "Wikis", &user, &body,
    )))
}

/// The standard-wiki table. Split out of [`list`] so the handler stays
/// under the line cap: everything above it is the query, this is the page.
///
/// `rows` is `(id, title, type, active fact count, live-identity)`.
fn render_wikis_index(
    user: &SessionUser,
    rows: &[(String, String, String, i64, bool)],
    reveal: bool,
    frozen: bool,
    wikis_dir: &str,
) -> Markup {
    html! {
        (wiki_family_tabs(/* smart_active */ false))
        @if reveal { (crate::reveal::banner()) }
        p.muted {
            "Read-only view of every standard-family wiki the engine has indexed under "
            code { (wikis_dir) }
            ". Smart-consumer wikis live on the "
            a href="/dashboard/wiki/smart" { "Smart" } " tab."
        }

        @if rows.is_empty() {
            p.muted {
                "No standard wikis yet — capture a fact from any consumer that calls "
                code { "wiki_ingest_message" } " to create the first one."
            }
        } @else {
            table {
                thead { tr {
                    th { "Wiki id" }
                    th { "Title" }
                    th { "Type" }
                    th { "Active facts" }
                    @if user.is_admin && !frozen { th { "Actions" } }
                } }
                tbody {
                    @for (id, title, kind, count, live_identity) in rows {
                        tr {
                            td { a href=(format!("/dashboard/wiki/{id}")) { code { (id) } } }
                            td { (title) }
                            td.muted { (kind) }
                            td { (count) }
                            // Deleting a whole wiki subtree is the most
                            // destructive control in the panel; a frozen
                            // deployment drops the column entirely.
                            @if user.is_admin && !frozen {
                                td {
                                    @if *live_identity {
                                        span.muted
                                            title="Identity wiki — remove via the user / group flow" {
                                            "—"
                                        }
                                    } @else {
                                        a.link-button.danger
                                            href=(format!("/dashboard/wiki/{id}/delete")) {
                                            "delete"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn view(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let reveal = crate::reveal::active(&state, &user, &jar);
    // Derived wiki visibility (non-reveal): the wiki surfaces only to a reader
    // who can read ≥1 fact in it; otherwise 404 — never a wiki-level render of
    // its prose / page list / structure. An admin with reveal on sees all.
    if !reveal {
        let sender_groups = enrollment::groups_for(&state.pool, &user.sender_id)
            .await
            .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?;
        if !fact_index::wiki_visible_to(
            &state.pool,
            wiki_id.as_str(),
            &user.sender_id,
            &sender_groups,
        )
        .await
        .map_err(|e| DashboardError::Internal(format!("wiki_visible_to: {e}")))?
        {
            return Err(DashboardError::NotFound);
        }
    }
    let pages = wiki_list_pages(&memory.tree, &wiki_id).map_err(map_wiki_err)?;
    let fact_count = fact_index::count_active_in_wiki(&state.pool, wiki_id.as_str())
        .await
        .map_err(|e| DashboardError::Internal(format!("count_active_in_wiki: {e}")))?;
    let rendered_index_body = rendered_index_for(
        &state,
        &memory.tree,
        &meta,
        &wiki_id,
        &user.sender_id,
        reveal,
    )
    .await?;
    let link_index = wikilink_index(&memory.tree);

    // The owning principal is derived from topology (the root identity
    // wiki's type), not declared in `_meta.md`. Show it as the wiki's
    // owner; a malformed tree (a non-identity root) surfaces the error
    // text rather than failing the whole page render.
    let owner_label = memory
        .tree
        .resolve_scope_principal(&meta)
        .map_or_else(|e| format!("(unresolved: {e})"), |p| p.to_string());

    let body = html! {
        section.meta {
            dl {
                dt { "id" } dd { code { (wiki_id.as_str()) } }
                dt { "title" } dd { (meta.title) }
                dt { "type" } dd { (meta.wiki_type) }
                dt { "slug" } dd { code { (meta.slug.as_str()) } }
                dt { "facts" } dd { (fact_count) " active" }
                dt { "owner" } dd { (owner_label) }
            }
        }

        @if user.is_admin {
            @if reveal { (crate::reveal::banner()) }
            p.wiki-export {
                a href=(format!("/dashboard/wiki/{}/export", wiki_id.as_str())) {
                    "Export subtree (.tar)"
                }
                span.muted {
                    " — portable full-marker archive (this wiki + descendants); "
                    "every indexed region travels with its own ACL inline."
                }
            }
        }

        h2 { "index.md" }
        (render_index_preview(
            &wiki_id,
            rendered_index_body.as_ref(),
            &user.sender_id,
            reveal,
            &link_index,
        ))

        h2 { "Pages" }
        @if pages.is_empty() {
            p.muted { "No pages yet." }
        } @else {
            ul {
                @for p in &pages {
                    li {
                        // The file path itself becomes the View link —
                        // that's the page-reading + comment surface. No
                        // raw-edit sibling: manual free-text editing is
                        // forbidden (smart) / admin-only-discouraged
                        // (standard) per roadmap 6j.
                        a href=(format!(
                            "/dashboard/wiki/{}/view/{}",
                            wiki_id.as_str(),
                            p.rel_path.display(),
                        )) {
                            code { (p.rel_path.display().to_string()) }
                        }
                        " · " (p.size) " bytes"
                    }
                }
            }
        }
    };

    let title = format!("Wiki — {}", wiki_id.as_str());
    Ok(Html(layout::authenticated_reading_page(
        chrome, &title, &user, &body,
    )))
}

/// The `index.md` preview block of the wiki home: the redaction/reveal
/// badge, the rendered HTML (reveal mode keeps the highlight wrappers,
/// normal mode strips all raw HTML; wikilinks click through and
/// fact-backed regions carry their record anchor, same as the page
/// viewer), and the "Open index.md" link. `None` renders the "no index
/// yet" hint instead.
fn render_index_preview(
    wiki_id: &WikiId,
    rendered: Option<&render::SegmentedRenderOutput>,
    sender_id: &str,
    reveal: bool,
    link_index: &std::collections::BTreeMap<String, PathBuf>,
) -> Markup {
    let Some(rendered) = rendered else {
        return html! {
            p.muted {
                "This wiki has no "
                code { "index.md" }
                " — the REM Hub Writer regenerates one whenever the wiki has "
                "children + at least one active fact. Capture a fact to seed it."
            }
        };
    };
    let resolve_wikilink =
        |target: &str| resolve_wikilink_href(link_index, Some(wiki_id.as_str()), target);
    // The wiki home serves at `/dashboard/wiki/:id` — without the rewrite
    // the browser would resolve a relative `page.md` link against
    // `/dashboard/wiki/`, a dead URL.
    let resolve_md_link = |dest: &str| {
        resolve_relative_page_href(
            link_index.get(wiki_id.as_str())?,
            wiki_id.as_str(),
            "",
            dest,
        )
    };
    html! {
        @if reveal && rendered.blocks_revealed > 0 {
            p.muted {
                "Reveal mode for "
                code { (sender_id) } " — "
                (rendered.blocks_revealed)
                @if rendered.blocks_revealed == 1 {
                    " fragment normally hidden, shown highlighted."
                } @else {
                    " fragments normally hidden, shown highlighted."
                }
            }
        } @else if rendered.blocks_redacted > 0 {
            p.muted {
                "Showing the declassified view for "
                code { (sender_id) } " — "
                (rendered.blocks_redacted)
                @if rendered.blocks_redacted == 1 {
                    " region replaced by "
                } @else {
                    " regions replaced by "
                }
                code { "[redacted]" } "."
            }
        }
        // Rendered HTML preview — same `md_render` page context used by
        // the page viewer (wikilink click-through + fact-ref anchors), no
        // per-heading injection (this is the wiki home overview, not a
        // commentable surface).
        section.wiki-page-view.prose {
            (PreEscaped(md_render::render_page(
                &annotate_fact_refs(&rendered.segments),
                reveal,
                &md_render::PageRenderContext {
                    resolve_wikilink: &resolve_wikilink,
                    resolve_md_link: &resolve_md_link,
                    fact_refs: true,
                },
                |_| None,
            )))
        }
        p {
            // View link is the only affordance — it lands on the
            // read-only surface where the "Add comments" toggle lives. The
            // raw editor is forbidden (smart) / admin-only-discouraged
            // (standard) per roadmap 6j, so we do NOT surface a
            // discoverable edit link here.
            a href=(format!("/dashboard/wiki/{}/view/index.md", wiki_id.as_str())) {
                "Open index.md"
            }
        }
    }
}

// ---------- per-page read view with inline comments ----------

/// One pending briefing item attached to (or floating over) the current
/// page. Built from a `wiki_briefing_items` row + a successful
/// `parse_cite` of its `target_cite`.
#[derive(Debug, Clone)]
struct PageComment {
    /// `bi_<N>` opaque identifier so the operator can reference it
    /// when discussing the comment elsewhere.
    briefing_item_id: String,
    /// `author_sender_id` from the row, when present. `None` for
    /// REM-emitted items (no human author).
    author_sender_id: Option<String>,
    /// ISO-8601 timestamp of the row.
    ts: String,
    /// Raw comment body (no markdown re-rendering — we keep the text
    /// verbatim and rely on the surrounding `<pre>` for whitespace).
    body: String,
    /// Wire `source_kind` so we can attribute REM-emitted items
    /// without a human author.
    source_kind: String,
    /// Slug fragment of the heading the comment is anchored to, or
    /// `None` when only the page is referenced (no fragment in the
    /// `target_cite`). `None` lands in the orphaned footer per the
    /// read-view policy — the spec is silent on this, and rendering at
    /// the top would conflict with the operator's expectation that an
    /// inline block sits next to *a heading*.
    anchor: Option<String>,
}

/// Result of pairing the pending briefing items with the rendered
/// body. `inline_by_anchor` maps a heading slug present in the body
/// onto the comments to render right after that heading; `orphaned`
/// collects everything else (anchor missing from the body, anchor
/// absent in the cite, or path mismatch).
///
/// Earlier keying was per-line-number to match the line-by-line
/// `<pre>` walker. With the markdown preview rendered by
/// [`md_render`], the body is no longer walked line-by-line; the
/// renderer fires a per-heading callback with the slug, so keying by
/// slug is the natural fit. Same orphaned policy.
#[derive(Debug, Default)]
struct CommentLayout {
    inline_by_anchor: HashMap<String, Vec<PageComment>>,
    orphaned: Vec<PageComment>,
}

/// Query string of [`view_page`]. The single recognised key is
/// `mode=comment`, which flips the page into "comment mode" — each
/// heading sprouts a `+ Comment` link and a `Stop commenting` toggle
/// appears at the top. Anything else (missing, blank, unknown value)
/// renders the page in clean read mode.
#[derive(Debug, Default, Deserialize)]
pub struct ViewPageQuery {
    #[serde(default)]
    pub mode: Option<String>,
}

impl ViewPageQuery {
    fn is_comment_mode(&self) -> bool {
        self.mode.as_deref() == Some("comment")
    }
}

/// Render the read-only view for an arbitrary page. Comments anchored
/// to the page via `wiki_briefing_items.target_cite` are interpolated
/// right below the heading they point at; orphans land in the footer.
async fn view_page(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
    Query(q): Query<ViewPageQuery>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let rel = std::path::PathBuf::from(&page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }
    let raw_body = wiki_read(&memory.tree, &wiki_id, &rel).map_err(map_wiki_err)?;

    let reveal = crate::reveal::active(&state, &user, &jar);
    // Derived wiki visibility (non-reveal): a reader who can read ≥1 fact in
    // the wiki sees the page (fragments still redacted per-fragment below); one
    // who can read nothing gets 404. Admin reveal sees all.
    if !reveal {
        let sender_groups = enrollment::groups_for(&state.pool, &user.sender_id)
            .await
            .map_err(|e| DashboardError::Internal(format!("groups_for: {e}")))?;
        if !fact_index::wiki_visible_to(
            &state.pool,
            wiki_id.as_str(),
            &user.sender_id,
            &sender_groups,
        )
        .await
        .map_err(|e| DashboardError::Internal(format!("wiki_visible_to: {e}")))?
        {
            return Err(DashboardError::NotFound);
        }
    }
    // ACL-aware view: by default we declassify exactly the way the
    // consumer agents do via `wiki_read`, so the operator sees `[redacted]`
    // wherever they wouldn't have read access via a smart token. With the
    // admin reveal toggle on, every region is shown (highlighted).
    let rendered = project_page(
        &state,
        &memory.tree,
        &meta,
        &wiki_id,
        &rel,
        &raw_body,
        &user.sender_id,
        reveal,
    )
    .await?;
    // The plain joined text (comment-anchor extraction below) vs the
    // fact-ref-annotated markdown (the HTML render): the anchors never
    // see the synthetic markers.
    let rendered_text = rendered.text();
    let annotated_text = annotate_fact_refs(&rendered.segments);
    let link_index = wikilink_index(&memory.tree);

    // Pull the pending briefing items whose `target_cite` plausibly
    // points at this page. We use a `LIKE` with the wiki_id + path
    // prefix so a `wiki://<id>/<path>` (no anchor) and a
    // `wiki://<id>/<path>#<slug>` row both surface; the per-row
    // `parse_cite` below does the strict pairing.
    let cite_prefix = format!("wiki://{}/{}", wiki_id.as_str(), page_path);
    let cite_like = format!("{cite_prefix}%");
    let rows: Vec<BriefingCommentRow> = sqlx::query_as(
        "SELECT id, author_sender_id, source_kind, body, ts, target_cite
         FROM wiki_briefing_items
         WHERE wiki_id = ?
           AND target_cite IS NOT NULL
           AND target_cite LIKE ?
           AND processed_at IS NULL
         ORDER BY ts ASC, id ASC",
    )
    .bind(wiki_id.as_str())
    .bind(&cite_like)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| DashboardError::Internal(format!("load briefing comments: {e}")))?;

    let anchors = extract_anchors_from_markdown(&rendered_text);
    let layout = lay_out_comments(&rows, &page_path, &anchors);

    // Whether to offer the comment affordance at all — same gate the
    // comment endpoints enforce, so we never render a dead "+ Comment"
    // link. Reveal does not factor in (it is a read lens, not a write
    // grant; see `can_comment_on`).
    // A frozen deployment refuses both endpoints, so neither affordance
    // is offered — but the page says *that* rather than the ACL reason
    // (`render_view_page_body` branches on `frozen` first): telling a
    // visitor they lack write access, when in truth nobody has any,
    // would be a wrong explanation of a correct refusal.
    let frozen = crate::read_only::hides_writes(&state);
    let can_comment =
        !frozen && can_comment_on(&state.pool, memory, &wiki_id, &user.sender_id).await?;
    // Whether to offer the "✎ what goes here" affordance: standard wiki +
    // owner-or-admin. A non-owner reader never sees a link that would 404.
    let can_edit_meta = !frozen && may_edit_page_meta(&state.pool, memory, &wiki_id, &user).await?;

    // Relative markdown links resolve against this page's directory,
    // same as the browser would — but rewritten to the canonical view
    // route with the on-disk case spelling.
    let base_rel_dir = page_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let body = render_view_page_body(
        &wiki_id,
        &page_path,
        &annotated_text,
        rendered.blocks_redacted,
        rendered.blocks_revealed,
        &user.sender_id,
        &layout,
        PageViewFlags {
            comment_mode: q.is_comment_mode(),
            can_comment,
            can_edit_meta,
            reveal,
            frozen,
        },
        &|target| resolve_wikilink_href(&link_index, Some(wiki_id.as_str()), target),
        &|dest| {
            resolve_relative_page_href(
                link_index.get(wiki_id.as_str())?,
                wiki_id.as_str(),
                base_rel_dir,
                dest,
            )
        },
    );

    let title = format!("View — {}/{}", wiki_id.as_str(), page_path);
    Ok(Html(layout::authenticated_reading_page(
        chrome, &title, &user, &body,
    ))
    .into_response())
}

/// Raw row shape projected by [`view_page`]'s query against
/// `wiki_briefing_items`.
#[derive(sqlx::FromRow)]
struct BriefingCommentRow {
    id: i64,
    author_sender_id: Option<String>,
    source_kind: String,
    body: String,
    ts: String,
    target_cite: Option<String>,
}

/// Pair each row with the heading-line interpolation slot, or drop it
/// in the orphaned bucket. Pure function — no IO, no SQL — so it is
/// the natural place to pin the rendering policy with unit tests.
/// Load the page's authoritative fact-key → ACL map from the engine DB
/// (redaction-policy: DB first, inline marker attributes only as the
/// fallback for unindexed regions). A failed load is a hard error —
/// rendering the page on weaker gating would be a leak, not a
/// degradation.
///
/// Two variants, keyed on the render mode:
/// - **normal view** (`reveal = false`) → `page_acl_map_active`, the same
///   map the reader paths (`wiki_read`, recall-by-navigation) use: a
///   retired (superseded/tombstoned) region whose bytes still sit on disk
///   is absent from the map, falls through to the attribute-less bare
///   marker, and redacts **fail-closed** — the dashboard never renders a
///   retired region to its last-known audience.
/// - **admin reveal** (`reveal = true`) → the full `page_acl_map`, retired
///   rows included, so the operator supervision lens can still see retired
///   residue and — via its last-known ACL — highlight only what the viewer
///   could not have read when it was live.
async fn page_acl_map_for(
    state: &DashboardState,
    tree: &mwe_core::wiki::WikiTree,
    wiki_id: &WikiId,
    page: &Path,
    reveal: bool,
) -> Result<mwe_core::acl::FactAclMap> {
    let handle = tree
        .locate(wiki_id)
        .map_err(|e| DashboardError::Internal(format!("locate {wiki_id:?}: {e}")))?;
    let source_path = handle.rel_dir().join(page);
    let source_path = source_path.to_string_lossy();
    if reveal {
        fact_index::page_acl_map(&state.pool, &source_path)
            .await
            .map_err(|e| DashboardError::Internal(format!("page_acl_map: {e}")))
    } else {
        fact_index::page_acl_map_active(&state.pool, &source_path)
            .await
            .map_err(|e| DashboardError::Internal(format!("page_acl_map_active: {e}")))
    }
}

/// Read and declassify the wiki's `index.md` for the operator; `None`
/// when the wiki has no index page yet.
async fn rendered_index_for(
    state: &DashboardState,
    tree: &mwe_core::wiki::WikiTree,
    meta: &mwe_core::wiki::WikiMeta,
    wiki_id: &WikiId,
    sender_id: &str,
    reveal: bool,
) -> Result<Option<render::SegmentedRenderOutput>> {
    let index = Path::new("index.md");
    let raw = match wiki_read(tree, wiki_id, index) {
        Ok(s) => s,
        Err(mwe_core::wiki::WikiError::PageNotFound { .. }) => return Ok(None),
        Err(e) => return Err(map_wiki_err(e)),
    };
    let rendered = project_page(state, tree, meta, wiki_id, index, &raw, sender_id, reveal).await?;
    Ok(Some(rendered))
}

/// Declassify one page body for the signed-in operator — exactly the
/// way `wiki_read` does for a consumer: the wiki's `acl_default`, the
/// operator's group memberships, and the page's DB-authoritative
/// **active** ACL map (retired regions redact fail-closed), then
/// [`render::render_for_sender`]. Under the admin reveal the map switches
/// to the full variant and the render to [`render::render_admin_reveal`],
/// so retired residue stays visible to the supervision lens.
///
/// Like `wiki_read` (and the recall navigator), the **testata is stripped
/// before rendering**: the frontmatter card (`keywords`/`description`) is
/// owner-tier metadata, not prose, so leaving it in would leak the themes of a
/// wiki the operator is not the default reader of (the
/// [ACL card boundary](../../../../docs/concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry),
/// dashboard half). The structured fields the viewer needs
/// (`title`/`type`/`acl_default`) are shown separately from `meta`.
async fn project_page(
    state: &DashboardState,
    tree: &mwe_core::wiki::WikiTree,
    meta: &mwe_core::wiki::WikiMeta,
    wiki_id: &WikiId,
    page: &Path,
    raw: &str,
    sender_id: &str,
    reveal: bool,
) -> Result<render::SegmentedRenderOutput> {
    let effective_acl_default = tree
        .resolve_scope_principal(meta)
        .map_err(|e| DashboardError::Internal(format!("resolve_scope_principal: {e}")))?;
    let sender_groups = enrollment::groups_for(&state.pool, sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("enrollment::groups_for: {e}")))?;
    let db_acl = page_acl_map_for(state, tree, wiki_id, page, reveal).await?;
    // Drop the testata so neither render path carries the owner-tier card.
    let body =
        mwe_core::wiki::MarkdownDoc::parse(raw).map_or_else(|| raw.to_owned(), |doc| doc.body);
    // `reveal` is only ever true when the caller already confirmed the
    // admin role (see `reveal_active`); it picks the operator override
    // that shows every region. Non-reveal is the normal consumer-equivalent
    // declassified view. Both come back **segmented** (the dashboard-only
    // sibling of the plain render — same text when joined): each shown,
    // DB-known region carries its fact id so the HTML render can drop the
    // region → fact-record anchor after it.
    let rendered = if reveal {
        render::render_admin_reveal_segments(
            &body,
            &db_acl,
            &effective_acl_default,
            sender_id,
            &sender_groups,
        )
    } else {
        render::render_for_sender_segments(
            &body,
            &db_acl,
            &effective_acl_default,
            sender_id,
            &sender_groups,
        )
    };
    Ok(rendered)
}

/// Rebuild the markdown handed to [`md_render`] from the segmented
/// render: each fact-backed segment gets a `{{factref=<id>}}` marker at
/// its end — inserted **before the segment's trailing whitespace**, so
/// the superscript anchor hugs the region's last line instead of opening
/// a paragraph of its own. Fact-less segments (connective prose, embeds,
/// `[redacted]` placeholders) pass through verbatim, so a redacted
/// region never grows an anchor.
///
/// Two placement corners:
/// - a reveal-highlighted **block** segment ends with the
///   `ACL_REVEAL_BLOCK_CLOSE` wrapper on its own line — the marker goes
///   *inside* the block, before the wrapper (text glued after `</div>`
///   would merge into the HTML-block event and break the renderer's
///   trim-equality wrapper check);
/// - a region body ending on a code-fence close keeps its fence line
///   clean — the marker goes after the whitespace instead.
fn annotate_fact_refs(segments: &[render::RenderSegment]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for seg in segments {
        let Some(fid) = &seg.fact_id else {
            out.push_str(&seg.text);
            continue;
        };
        let trimmed = seg.text.trim_end();
        let insert_at = trimmed
            .strip_suffix(render::ACL_REVEAL_BLOCK_CLOSE)
            .map_or_else(
                || {
                    if trimmed.ends_with("```") {
                        seg.text.len()
                    } else {
                        trimmed.len()
                    }
                },
                |inner| inner.trim_end().len(),
            );
        out.push_str(&seg.text[..insert_at]);
        let _ = write!(out, "{{{{factref={fid}}}}}");
        out.push_str(&seg.text[insert_at..]);
    }
    out
}

/// The wiki-tree index backing the wikilink click-through resolver:
/// `wiki_id` → absolute wiki directory, one tree walk per request. Every
/// family is navigable (a smart wiki's `/dashboard/wiki/:id` view
/// resolves too); an unreadable tree degrades to an empty index — links
/// render literal, the page still serves.
pub fn wikilink_index(
    tree: &mwe_core::wiki::WikiTree,
) -> std::collections::BTreeMap<String, PathBuf> {
    tree.walk()
        .map(|wikis| {
            wikis
                .into_iter()
                .map(|d| (d.meta.wiki_id.as_str().to_owned(), d.abs_dir))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve one wikilink target against the tree, per the link grammar of
/// recall-pipeline.md:
/// `wiki_id` → that wiki's view route; `wiki_id/page-slug` → that page's
/// view route when the page file exists (the slug never carries `.md`;
/// nested slugs keep their `/`).
///
/// **Legacy fallback** (emit canonical, resolve legacy — the same stance
/// the marker grammar takes): a bare target that names no wiki is retried
/// as a page slug over the whole tree, in deterministic order — the wiki
/// whose prose is being rendered (`current_wiki`), its ancestors
/// nearest-first, its sub-wikis nearest-first, then the remaining wikis
/// in id order. The corpus predating the canonical grammar links pages by
/// bare name across wiki lines (`[[cucina]]` on a `famiglia` page names
/// an `morgana` page), and page prose is copied verbatim across compiles,
/// so those links never self-canonicalize. Precedence is deterministic: a
/// wiki id always wins over a same-named page, and a link resolves to the
/// same destination for every reader. Anything still unresolved — unknown
/// wiki, missing page, unsafe path, the mutant grammars — returns `None`,
/// and the caller leaves the literal text in the prose (never a broken
/// link).
pub fn resolve_wikilink_href(
    index: &std::collections::BTreeMap<String, PathBuf>,
    current_wiki: Option<&str>,
    target: &str,
) -> Option<String> {
    let target = target.trim();
    let (wiki_id, page) = match target.split_once('/') {
        Some((w, p)) => (w.trim(), Some(p.trim())),
        None => (target, None),
    };
    let Some(abs_dir) = index.get(wiki_id) else {
        if page.is_some() {
            return None;
        }
        let current_dir = index.get(current_wiki?)?;
        let mut ranked: Vec<(usize, usize, &String, &PathBuf)> = index
            .iter()
            .map(|(id, dir)| {
                let depth = dir.components().count();
                let (tier, key) = if dir == current_dir {
                    (0, 0)
                } else if current_dir.starts_with(dir) {
                    // Ancestor: nearest (deepest) first.
                    (1, usize::MAX - depth)
                } else if dir.starts_with(current_dir) {
                    // Descendant sub-wiki: nearest (shallowest) first.
                    (2, depth)
                } else {
                    (3, 0)
                };
                (tier, key, id, dir)
            })
            .collect();
        ranked.sort();
        return ranked
            .into_iter()
            .find_map(|(_, _, id, dir)| page_href(dir, id, target));
    };
    match page {
        None | Some("") => Some(format!("/dashboard/wiki/{}", encode_path_segments(wiki_id))),
        Some(slug) => page_href(abs_dir, wiki_id, slug),
    }
}

/// The view-route href for page `slug` of the wiki at `abs_dir` — `None`
/// unless the slug maps to a safe path whose file exists. Existence is
/// checked Obsidian-style (byte-exact first, else the unique
/// case-insensitive match, per
/// [`mwe_core::wiki::resolve_page_case_insensitive`]) and the href
/// carries the on-disk spelling.
fn page_href(abs_dir: &std::path::Path, wiki_id: &str, slug: &str) -> Option<String> {
    page_view_href(abs_dir, wiki_id, &PathBuf::from(format!("{slug}.md")))
}

/// Shared tail of [`page_href`] and [`resolve_relative_page_href`]: the
/// view-route href for the page at wiki-relative `rel` (`.md` included)
/// — `None` unless the path is safe and its file exists.
fn page_view_href(
    abs_dir: &std::path::Path,
    wiki_id: &str,
    rel: &std::path::Path,
) -> Option<String> {
    if !mwe_core::wiki::is_safe_page_path(rel) {
        return None;
    }
    let resolved = mwe_core::wiki::resolve_page_case_insensitive(abs_dir, rel)?;
    Some(format!(
        "/dashboard/wiki/{}/view/{}",
        encode_path_segments(wiki_id),
        encode_path_segments(&resolved.to_string_lossy().replace('\\', "/"))
    ))
}

/// Resolve one regular markdown link destination found in wiki prose to
/// the canonical `/view/` route. Only wiki-relative `.md` targets are
/// rewritten: the wiki home serves at `/dashboard/wiki/:id`, where the
/// browser would resolve `concepts/page.md` against `/dashboard/wiki/`
/// into a dead URL; rewriting on the `/view/*path` surface too keeps one
/// canonical href (on-disk case spelling) everywhere. `base_rel_dir` is
/// the rendering page's wiki-relative directory (`""` at the wiki root).
/// Everything else returns `None` and the destination stays exactly as
/// authored: absolute paths, scheme'd URLs (`https:`, `mailto:` …),
/// bare `#anchors`, query-carrying or non-`.md` targets, `..` escapes
/// above the wiki root (a cross-wiki hop is a wikilink's job), and
/// targets whose file does not exist — the same dead-rail posture as
/// [`resolve_wikilink_href`], never inventing a broken href.
pub fn resolve_relative_page_href(
    abs_dir: &std::path::Path,
    wiki_id: &str,
    base_rel_dir: &str,
    dest: &str,
) -> Option<String> {
    if dest.is_empty() || dest.starts_with('/') || dest.starts_with('#') {
        return None;
    }
    // A `:` before any `/` / `#` / `?` marks a scheme'd absolute URL.
    if dest
        .split(['/', '#', '?'])
        .next()
        .unwrap_or("")
        .contains(':')
    {
        return None;
    }
    let (path_part, fragment) = match dest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (dest, None),
    };
    if path_part.contains('?') || !path_part.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    // Normalize `.` / `..` against the page's directory.
    let mut segs: Vec<&str> = base_rel_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path_part.split('/') {
        match seg {
            "" => return None,
            "." => {},
            ".." => {
                segs.pop()?;
            },
            s => segs.push(s),
        }
    }
    let mut href = page_view_href(abs_dir, wiki_id, &PathBuf::from(segs.join("/")))?;
    if let Some(f) = fragment {
        href.push('#');
        href.push_str(f);
    }
    Some(href)
}

/// Percent-encode each `/`-separated segment of `path` (RFC 3986
/// unreserved set kept literal), preserving the separators — hrefs are
/// built from operator-authored ids/slugs, so this is defence in depth,
/// not a routine need.
fn encode_path_segments(path: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(char::from(b));
                },
                _ => {
                    let _ = write!(out, "%{b:02X}");
                },
            }
        }
    }
    out
}

fn lay_out_comments(
    rows: &[BriefingCommentRow],
    current_path: &str,
    anchors: &[HeadingAnchor],
) -> CommentLayout {
    let mut layout = CommentLayout::default();
    for row in rows {
        let comment = comment_from_row(row);
        let Some(cite) = row.target_cite.as_deref() else {
            // Should not happen — the SQL filters `target_cite IS NOT
            // NULL` — but the policy is the same as a parse failure.
            layout.orphaned.push(comment);
            continue;
        };
        // Corrupt cite (defence in depth — `notify_append` already
        // validates the value before it lands in the DB). Surface as
        // orphaned rather than swallowing.
        let Ok(parsed) = parse_cite(cite) else {
            layout.orphaned.push(comment);
            continue;
        };
        // The SQL `LIKE` is `wiki://<id>/<path>%`, which also matches
        // `wiki://<id>/<path>foo.md` when `path` is a prefix of
        // another. Re-check the parsed path here for a strict equality
        // before we promote a row from "candidate" to "comment for
        // this page". A path mismatch lands as orphaned too — the
        // operator will see *something* even when the LIKE catches a
        // false-positive sibling.
        if parsed.path != current_path {
            layout.orphaned.push(comment);
            continue;
        }
        let Some(anchor) = parsed.anchor.as_deref() else {
            // No anchor in the cite → orphaned by policy (the spec is
            // silent; we pick the safer bucket so an item without a
            // specific section does not collide with an unrelated
            // heading at the top of the file).
            layout.orphaned.push(PageComment {
                anchor: None,
                ..comment
            });
            continue;
        };
        if anchors.iter().any(|h| h.anchor == anchor) {
            layout
                .inline_by_anchor
                .entry(anchor.to_owned())
                .or_default()
                .push(PageComment {
                    anchor: Some(anchor.to_owned()),
                    ..comment
                });
        } else {
            // Anchor present in the cite but no heading in the body
            // produces that slug — the heading was renamed or the
            // section was removed. Surface as orphaned so the operator
            // still sees the feedback.
            layout.orphaned.push(PageComment {
                anchor: Some(anchor.to_owned()),
                ..comment
            });
        }
    }
    layout
}

fn comment_from_row(row: &BriefingCommentRow) -> PageComment {
    PageComment {
        briefing_item_id: format!("bi_{}", row.id),
        author_sender_id: row.author_sender_id.clone(),
        ts: row.ts.clone(),
        body: row.body.clone(),
        source_kind: row.source_kind.clone(),
        anchor: None,
    }
}

/// Build the maud body for the read-only viewer. Renders the page
/// body as **HTML preview** via [`md_render::render_with_heading_injections`]
/// — every heading gets `id="<slug>"` (matching the comment
/// grammar) and the renderer fires a per-heading callback that
/// interleaves the comment-mode CTA (when `comment_mode=true`) and
/// the inline comment blocks queued in `layout.inline_by_anchor`.
///
/// Earlier behaviour rendered the body line-by-line inside
/// `<pre class="wiki-page">` blocks so headings stayed as raw
/// markdown text (`## Boundary tokens`); the preview is the
/// successor surface where the operator reads + comments on a real
/// rendered page, and the textual editor moves to the bottom as the
/// "raw edit" escape hatch.
/// Display flags threaded into [`render_view_page_body`], grouped so the
/// signature stays out of bool-soup and call sites read by name instead
/// of a row of positional `true`/`false`.
// Four named display flags is exactly the readability win this grouping
// struct exists for; the lint's "fold into an enum" advice does not fit
// independent toggles.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
struct PageViewFlags {
    /// `?mode=comment` is on — interleave the per-heading "+ Comment" CTA.
    comment_mode: bool,
    /// The viewer may leave a comment (owner / shared / global read-access);
    /// gates the whole comment affordance vs the "can't comment" notice.
    can_comment: bool,
    /// The viewer may edit the page's testata `description` («what goes
    /// here») — owner of the (standard) wiki, or an admin. Gates the
    /// "✎ what goes here" affordance.
    can_edit_meta: bool,
    /// Admin reveal is on — render every fragment highlighted, not
    /// `[redacted]`, and show the reveal banner. (Implies admin: it can
    /// only be true for an admin, see [`crate::reveal::active`].)
    reveal: bool,
    /// The deployment is frozen ([`crate::read_only`]). Implies both
    /// `can_comment` and `can_edit_meta` are false, and changes the
    /// *reason* the page gives for it.
    frozen: bool,
}

#[allow(clippy::too_many_arguments)]
fn render_view_page_body(
    wiki_id: &WikiId,
    page_path: &str,
    annotated_text: &str,
    blocks_redacted: usize,
    blocks_revealed: usize,
    sender_id: &str,
    layout: &CommentLayout,
    flags: PageViewFlags,
    resolve_wikilink: &dyn Fn(&str) -> Option<String>,
    resolve_md_link: &dyn Fn(&str) -> Option<String>,
) -> Markup {
    let PageViewFlags {
        comment_mode,
        can_comment,
        can_edit_meta,
        reveal,
        frozen,
    } = flags;
    let view_url = format!("/dashboard/wiki/{}/view/{}", wiki_id.as_str(), page_path);
    let comment_mode_url = format!("{view_url}?mode=comment");

    // Render markdown → HTML once, consuming `inline_by_anchor` as
    // we go so each comment is emitted exactly once even if the
    // markdown has duplicate headings (would mean duplicate slugs;
    // first heading wins). Reveal mode keeps the highlight wrappers; the
    // default renderer strips all raw HTML. The page context linkifies
    // canonical `[[wikilinks]]` into in-dashboard navigation and turns
    // the `{{factref=…}}` markers (from the segmented render) into
    // per-region fact-record anchors.
    let mut inline_by_anchor = layout.inline_by_anchor.clone();
    let inject = |slug: &str| {
        let mut buf = String::new();
        if comment_mode && can_comment {
            buf.push_str(&render_add_comment_cta_html(wiki_id, page_path, slug));
        }
        if let Some(comments) = inline_by_anchor.remove(slug) {
            for c in &comments {
                buf.push_str(&render_comment_block(c, /* orphaned */ false).into_string());
            }
        }
        if buf.is_empty() { None } else { Some(buf) }
    };
    let ctx = md_render::PageRenderContext {
        resolve_wikilink,
        resolve_md_link,
        fact_refs: true,
    };
    let rendered_html = md_render::render_page(annotated_text, reveal, &ctx, inject);

    html! {
        section.meta {
            dl {
                dt { "wiki_id" } dd { code { (wiki_id.as_str()) } }
                dt { "page" }    dd { code { (page_path) } }
            }
        }

        // `reveal` already implies admin (see PageViewFlags::reveal).
        @if reveal { (crate::reveal::banner()) }

        @if reveal && blocks_revealed > 0 {
            p.muted {
                "Reveal mode for "
                code { (sender_id) } " — "
                (blocks_revealed)
                @if blocks_revealed == 1 {
                    " fragment normally hidden, shown highlighted."
                } @else {
                    " fragments normally hidden, shown highlighted."
                }
            }
        } @else if blocks_redacted > 0 {
            p.muted {
                "Showing the declassified view for "
                code { (sender_id) } " — "
                (blocks_redacted)
                @if blocks_redacted == 1 {
                    " region replaced by "
                } @else {
                    " regions replaced by "
                }
                code { "[redacted]" } "."
            }
        }

        @if can_comment {
            p.comment-mode-toggle {
                @if comment_mode {
                    span.muted { "Comment mode is ON — click " code { "+ Comment" } " next to a heading to leave feedback. " }
                    a href=(view_url) { "Stop commenting" }
                } @else {
                    a href=(comment_mode_url) { "Add comments" }
                    span.muted { " — opens a per-heading affordance to leave feedback for the smart consumer." }
                }
            }
        } @else if !frozen {
            p.comment-mode-toggle.muted {
                "You can't comment on this page — you don't have write access to it."
            }
        }

        section.wiki-page-view.prose {
            (PreEscaped(rendered_html))
        }

        @if !layout.orphaned.is_empty() {
            section.orphaned-comments {
                h3 { "Orphaned comments" }
                p.muted {
                    "These comments were anchored to a heading that no longer "
                    "exists in the current body, or were posted without a "
                    "section anchor. They are surfaced here so the feedback is "
                    "not lost when a heading is renamed."
                }
                @for c in &layout.orphaned {
                    (render_comment_block(c, /* orphaned */ true))
                }
            }
        }

        p {
            a href=(format!("/dashboard/wiki/{}", wiki_id.as_str())) { "← Back to wiki" }
        }

        @if can_edit_meta {
            (render_describe_affordance(wiki_id, page_path))
        }

        (blessed_channels_footer(&comment_mode_url, can_comment, frozen))
    }
}

/// The "blessed channels" footer: how to change the page without the
/// (removed) raw editor. The inline-comments link is offered only when
/// the viewer can actually comment, so it is never a dead link.
///
/// On a frozen deployment there are no channels at all — the footer would
/// otherwise be a list of three links to things that refuse — so it says
/// that instead.
fn blessed_channels_footer(comment_mode_url: &str, can_comment: bool, frozen: bool) -> Markup {
    if frozen {
        return html! {
            p class="blessed-channels-footer muted" {
                "This instance is read-only, so this page cannot be changed from here "
                "by anybody — not through comments, not through the chat, not through "
                "the fact actions."
            }
        };
    }
    html! {
        // Manual free-text editing from the dashboard is forbidden
        // (smart) / admin-only-discouraged (standard) per roadmap 6j —
        // there is NO discoverable "open the raw editor" link here. The
        // blessed write channels are the inline comments above, the
        // operative chat, and the structured ACL / validity / supersede
        // fact actions.
        p class="blessed-channels-footer muted" {
            @if can_comment {
                "To change this page, use the blessed channels: leave inline "
                a href=(comment_mode_url) { "comments" }
                ", talk to the "
                a href="/dashboard/chat" { "operative chat" }
                ", or use the structured "
                a href="/dashboard/facts" { "fact actions" }
                " (ACL / validity / supersede)."
            } @else {
                "To change this page, use the blessed channels: talk to the "
                a href="/dashboard/chat" { "operative chat" }
                ", or use the structured "
                a href="/dashboard/facts" { "fact actions" }
                " (ACL / validity / supersede)."
            }
            " Manual free-text rewriting of wiki pages from the dashboard is no longer offered."
        }
    }
}

/// Build the "+ Comment on #slug" CTA as a raw HTML string,
/// for embedding into the rendered preview via
/// [`md_render::render_with_heading_injections`]. Kept here next to
/// the maud `render_comment_block` so the two stay visually
/// consistent.
fn render_add_comment_cta_html(wiki_id: &WikiId, page_path: &str, anchor: &str) -> String {
    html! {
        p.add-comment-cta {
            a href=(format!(
                "/dashboard/wiki/{}/comment/{}?anchor={}",
                wiki_id.as_str(),
                page_path,
                anchor,
            )) {
                "+ Comment on " code { "#" (anchor) }
            }
        }
    }
    .into_string()
}

fn render_comment_block(c: &PageComment, orphaned: bool) -> Markup {
    let attribution = match (c.author_sender_id.as_deref(), c.source_kind.as_str()) {
        (Some(author), _) => format!("Comment by @{author}"),
        (None, "rem") => "From REM".to_owned(),
        (None, _) => "Comment".to_owned(),
    };
    html! {
        aside.comment-block.(if orphaned { "orphaned" } else { "inline" }) {
            header.comment-meta {
                span.comment-attribution { (attribution) }
                ", "
                time { (c.ts) }
                " · "
                code.comment-id { (c.briefing_item_id) }
                @if let Some(anchor) = c.anchor.as_deref()
                    && orphaned
                {
                    " · "
                    span.muted {
                        "anchor "
                        code { "#" (anchor) }
                        " not found in body"
                    }
                }
            }
            div.comment-body {
                pre { (c.body) }
            }
        }
    }
}

// ---------- inline-comment write path ----------

/// Query string of the comment form GET + POST handlers. `anchor`
/// addresses the heading the comment is anchored to. Validated as a
/// non-empty slug shape `[a-z0-9-]+` (no leading / trailing dash) so
/// it round-trips with [`mwe_core::briefing::slug_from_heading`] and
/// the existing read-view renderer can pair it back to the heading.
#[derive(Debug, Deserialize)]
pub struct CommentAnchor {
    pub anchor: String,
}

/// Form body of [`submit_comment`]. The single field is the free-form
/// markdown body of the comment, trimmed + length-checked server-side.
#[derive(Debug, Deserialize)]
pub struct CommentSubmission {
    /// Free-form comment body. Trimmed and length-checked by the
    /// handler before it touches the DB.
    pub body: String,
}

/// Hard ceiling on the comment body length. Matches
/// [`mwe_core::briefing::BODY_MAX_BYTES`] so a comment that survives
/// the dashboard form also survives a hypothetical future MCP path
/// that funnels through the same column. The textarea exposes the
/// same limit via `maxlength` so the browser short-circuits before
/// posting overflowing bodies.
const COMMENT_BODY_MAX_BYTES: usize = BODY_MAX_BYTES;

/// `topic` column is constrained to 200 bytes on disk. We derive a
/// truncated `topic` from the body so the dashboard `/briefing`
/// surface can list comments in a tabular shape without
/// fetching the full body. 80 chars is the soft target; we cut on a
/// word boundary when possible to keep the topic line readable.
const COMMENT_TOPIC_TARGET_BYTES: usize = 80;

async fn comment_form(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
    Query(q): Query<CommentAnchor>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let _meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let rel = std::path::PathBuf::from(&page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }
    let anchor = validate_anchor_shape(&q.anchor)?;

    // Read-access check on the wiki — anyone who can read the page
    // can leave a comment. The owner clearly passes; `shared_with`
    // members (user, group, global) also pass per the sharing
    // model — feedback from a teammate is the whole point of the
    // smart-wiki sharing surface.
    enforce_read_access_or_not_found(&state.pool, memory, &wiki_id, &user).await?;

    // Surface the heading text so the operator knows what they are
    // commenting on. The lookup is best-effort — a missing heading
    // does not block the write (the comment will land in the
    // orphaned bucket in the read view), but we surface a warning
    // so the operator can fix the cite proactively.
    let heading_label =
        resolve_heading_label(&memory.tree, &wiki_id, &rel, &anchor).map_err(map_wiki_err)?;

    Ok(Html(render_comment_form(
        chrome,
        &user,
        &id,
        &page_path,
        &anchor,
        heading_label.as_deref(),
        /* body */ "",
        /* error */ None,
    ))
    .into_response())
}

async fn submit_comment(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
    Query(q): Query<CommentAnchor>,
    HtmlForm(form): HtmlForm<CommentSubmission>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let _meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let rel = std::path::PathBuf::from(&page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }
    let anchor = validate_anchor_shape(&q.anchor)?;

    enforce_read_access_or_not_found(&state.pool, memory, &wiki_id, &user).await?;

    let body = form.body.trim();
    if body.is_empty() {
        return Err(DashboardError::Validation(
            "Comment body must not be empty.".to_owned(),
        ));
    }
    if body.len() > COMMENT_BODY_MAX_BYTES {
        return Err(DashboardError::Validation(format!(
            "Comment body must not exceed {COMMENT_BODY_MAX_BYTES} bytes (got {}).",
            body.len()
        )));
    }

    let target_cite = compose_cite(&wiki_id, &page_path, Some(&anchor)).map_err(|e| {
        // `compose_cite` re-validates the anchor shape we already
        // checked above — a failure here would indicate a path
        // segment containing `#` or an absurdly long compose. Map to
        // BadRequest so the user gets a precise reason instead of a
        // generic 500.
        DashboardError::BadRequest(format!("compose_cite failed: {e}"))
    })?;
    let topic = derive_topic(body);
    let ts = chrono::Utc::now().to_rfc3339();

    // Direct INSERT rather than a `briefing::notify` layer — see the
    // module docstring for the rationale. The shape of the row
    // matches what the read path expects (filter
    // `processed_at IS NULL`, `target_cite LIKE 'wiki://<id>/<path>%'`,
    // `author_sender_id` surfaces in the attribution line) plus what
    // `mark_processed` will flip when the smart consumer
    // addresses the feedback in its next `wiki_admin_push`.
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_briefing_items
            (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, \
             author_sender_id, processed_at)
         VALUES (?, 'dashboard_comment', ?, ?, ?, 'external', ?, ?, ?, NULL)
         RETURNING id",
    )
    .bind(wiki_id.as_str())
    // `source_ref` is a stable string identifying the channel + the
    // author. Mirrors the dashboard's existing convention for the
    // notify path (`source_ref = "user:<id>"`) but tagged with the
    // dashboard channel so a downstream diagnostic can tell the two
    // apart at a glance without joining against `source_kind`.
    .bind(format!("dashboard:{}", user.sender_id))
    .bind(&topic)
    .bind(body)
    .bind(&ts)
    .bind(&target_cite)
    .bind(&user.sender_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| DashboardError::Internal(format!("insert dashboard_comment briefing row: {e}")))?;

    tracing::info!(
        actor = %user.sender_id,
        wiki = %id,
        page = %page_path,
        anchor = %anchor,
        briefing_item_id = format!("bi_{}", row.0),
        "dashboard inline comment persisted"
    );

    // Redirect back to the read view (no `?mode=comment`) so the
    // operator immediately sees their comment freshly interpolated
    // below the matching heading.
    Ok(Redirect::to(&format!(
        "/dashboard/wiki/{}/view/{}",
        wiki_id.as_str(),
        page_path
    ))
    .into_response())
}

/// Validate the anchor query parameter against the canonical slug
/// charset (`[a-z0-9-]+`, no leading / trailing dash). Same rules
/// applied by [`mwe_core::briefing::slug_from_heading`] so a slug
/// derived from a heading round-trips cleanly through the URL.
fn validate_anchor_shape(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DashboardError::Validation(
            "Comment anchor must not be empty.".to_owned(),
        ));
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') {
        return Err(DashboardError::Validation(format!(
            "Comment anchor {trimmed:?} cannot start or end with `-`."
        )));
    }
    for c in trimmed.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !ok {
            return Err(DashboardError::Validation(format!(
                "Comment anchor {trimmed:?} contains invalid character {c:?} \
                 (allowed: [a-z0-9-])."
            )));
        }
    }
    Ok(trimmed.to_owned())
}

/// Resolve the heading text whose slug matches `anchor`, when the
/// page exists. Returns `Ok(None)` when the page is readable but the
/// anchor does not match any current heading — the comment write is
/// still allowed (the user might be commenting on a heading about to
/// be renamed; the read view will surface the row in the orphaned
/// bucket). Page-not-found bubbles up as a wiki error.
fn resolve_heading_label(
    tree: &mwe_core::wiki::WikiTree,
    wiki_id: &WikiId,
    rel: &Path,
    anchor: &str,
) -> std::result::Result<Option<String>, mwe_core::wiki::WikiError> {
    let body = wiki_read(tree, wiki_id, rel)?;
    Ok(extract_anchors_from_markdown(&body)
        .into_iter()
        .find(|h| h.anchor == anchor)
        .map(|h| h.heading_text))
}

/// Derive a short topic line from the body. Cuts on a word boundary
/// when one sits below the byte target so the topic never breaks a
/// word mid-character; falls back to a hard byte cut on a char
/// boundary when no whitespace appears in the prefix. Single line —
/// the topic column is rendered in tabular dashboards and
/// must not span multiple rows.
fn derive_topic(body: &str) -> String {
    let single_line = body.lines().next().unwrap_or(body).trim().to_owned();
    if single_line.len() <= COMMENT_TOPIC_TARGET_BYTES {
        return single_line;
    }
    // Find the last whitespace at or before the soft target so the
    // truncation lands on a word boundary. `char_indices` lets us
    // honour the UTF-8 alignment of the cut point.
    let mut last_boundary: Option<usize> = None;
    for (idx, ch) in single_line.char_indices() {
        if idx > COMMENT_TOPIC_TARGET_BYTES {
            break;
        }
        if ch.is_whitespace() {
            last_boundary = Some(idx);
        }
    }
    let cut = last_boundary.unwrap_or_else(|| {
        // No whitespace: fall back to the nearest char boundary at or
        // below the soft target so we never panic on a non-ASCII body.
        single_line
            .char_indices()
            .take_while(|(i, _)| *i <= COMMENT_TOPIC_TARGET_BYTES)
            .last()
            .map_or(0, |(i, ch)| i + ch.len_utf8())
    });
    let mut truncated = single_line[..cut].trim_end().to_owned();
    truncated.push('…');
    truncated
}

fn render_comment_form(
    chrome: layout::Chrome,
    user: &SessionUser,
    wiki_id: &str,
    page_path: &str,
    anchor: &str,
    heading_label: Option<&str>,
    body: &str,
    error: Option<&str>,
) -> String {
    let title = format!("Comment — {wiki_id}/{page_path}#{anchor}");
    let view_url = format!("/dashboard/wiki/{wiki_id}/view/{page_path}");
    let form_action = format!("/dashboard/wiki/{wiki_id}/comment/{page_path}?anchor={anchor}");
    let html_body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        section.meta {
            dl {
                dt { "wiki_id" } dd { code { (wiki_id) } }
                dt { "page" }    dd { code { (page_path) } }
                dt { "anchor" }  dd { code { "#" (anchor) } }
                @if let Some(label) = heading_label {
                    dt { "heading" } dd { (label) }
                } @else {
                    dt { "heading" }
                    dd.muted {
                        "(no current heading produces this slug — the comment "
                        "will land in the orphaned bucket in the view until "
                        "the heading is restored or renamed)"
                    }
                }
            }
        }

        p.muted {
            "The comment is persisted as a "
            code { "wiki_briefing_items" }
            " row with "
            code { "source_kind=dashboard_comment" }
            ", "
            code { "kind=external" }
            ", "
            code { "author_sender_id=" (user.sender_id) }
            ". The smart consumer recipient sees it at the next "
            code { "smart_bootstrap" }
            " and can mark it processed via "
            code { "wiki_admin_push.mark_processed" } "."
        }

        form action=(form_action) method="post" {
            p {
                label for="body" { "Comment body" }
                textarea
                    id="body"
                    name="body"
                    rows="8"
                    cols="80"
                    required
                    maxlength=(COMMENT_BODY_MAX_BYTES)
                    placeholder="Leave feedback for the smart consumer — what should change, why, where the supporting evidence lives." {
                    (body)
                }
            }
            (components::submit("Save"))
            " "
            a href=(view_url.clone()) { "Cancel" }
        }

        p {
            a href=(view_url) { "← Back to page" }
        }
    };
    layout::authenticated_reading_page(chrome, &title, user, &html_body)
}

/// Whether `sender_id` may leave a dashboard comment on `wiki_id`:
/// anyone who can read the page (owner + `shared_with` matches — direct
/// user, group via enrollment, or global). The bool sibling of
/// [`enforce_read_access_or_not_found`], used by [`view_page`] to decide
/// whether to render the comment affordance at all (vs a "you can't
/// comment" notice) — so the UI never shows a link the endpoint would
/// then 403. Admin reveal deliberately does **not** unlock commenting:
/// reveal is a read lens, while a comment is a write REM later turns into
/// fact ops on the owner's memory (carrying no commenter provenance), so
/// it stays scoped to the page's read-set.
async fn can_comment_on(
    pool: &sqlx::SqlitePool,
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    sender_id: &str,
) -> Result<bool> {
    let Ok(handle) = memory.tree.locate(wiki_id) else {
        return Ok(false);
    };
    let outcome = resolve_read_access(pool, &memory.tree, &handle, sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("resolve_read_access: {e}")))?;
    // Anyone who can read may comment — owner, owning-group member, shared
    // user/group, or global. A non-member of a group-owned wiki is Denied.
    Ok(outcome.is_granted())
}

/// Read-access check for the comment write path. Anyone who can read
/// the wiki can comment on it — owner + `shared_with` matches (direct
/// user, group via enrollment, or global). Non-matching callers get
/// a 403, mirroring the `wiki_admin_notify` semantics for cross-user
/// briefing items.
async fn enforce_read_access_or_not_found(
    pool: &sqlx::SqlitePool,
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    user: &SessionUser,
) -> Result<()> {
    let Ok(handle) = memory.tree.locate(wiki_id) else {
        return Err(DashboardError::NotFound);
    };
    let outcome = resolve_read_access(pool, &memory.tree, &handle, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("resolve_read_access: {e}")))?;
    if outcome.is_granted() {
        Ok(())
    } else {
        // `resolve_read_access` has no admin bypass, so this is a content ACL
        // denial, not an admin gate: `NoAccess` keeps the copy honest ("you
        // don't have access" rather than "admin rights required").
        Err(DashboardError::NoAccess)
    }
}

// ---------- textual page editor ----------

/// Form submission for the textual editor.
#[derive(Debug, Deserialize)]
pub struct PageEditSubmission {
    /// Full body the operator typed in the textarea. Written verbatim.
    pub body: String,
}

async fn edit_form(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;

    // The raw free-text editor is the discouraged escape hatch: hard-
    // forbidden on smart wikis (the smart consumer is the sole writer),
    // admin-only on standard wikis. Both gates run before the owner check
    // so a non-admin / smart attempt never even reveals the page body.
    enforce_raw_editor_allowed(memory, &wiki_id, &user)?;

    // Owner check via the same path the writes use — non-owners get a
    // generic 404 (no information leak about which wikis exist).
    enforce_owner_or_not_found(&state.pool, memory, &wiki_id, &user).await?;

    // Refuse the metadata file up-front: the editor surface for
    // `_meta.md` lives on `/dashboard/wiki/:id/sharing`, this route
    // would conflate sharing edits with content edits.
    if is_meta_filename(&page_path) {
        return Err(meta_route_violation());
    }

    let rel = PathBuf::from(&page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }

    let current_body = match wiki_read(&memory.tree, &wiki_id, &rel) {
        Ok(s) => s,
        // Editing a path that doesn't yet exist creates it on save —
        // the textarea opens blank. Mirrors the "new page" affordance
        // a textual wiki editor is expected to provide.
        Err(mwe_core::wiki::WikiError::PageNotFound { .. }) => String::new(),
        Err(e) => return Err(map_wiki_err(e)),
    };

    Ok(Html(render_edit_form(
        chrome,
        &user,
        &id,
        &page_path,
        &current_body,
        None,
    ))
    .into_response())
}

async fn submit_edit(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
    HtmlForm(form): HtmlForm<PageEditSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;

    enforce_raw_editor_allowed(memory, &wiki_id, &user)?;
    enforce_owner_or_not_found(&state.pool, memory, &wiki_id, &user).await?;
    if is_meta_filename(&page_path) {
        return Err(meta_route_violation());
    }

    let rel = PathBuf::from(&page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }

    let caller = AdminCaller {
        sender_id: user.sender_id.clone(),
        // The dashboard operator is not behind an MCP device — every
        // dashboard write is consumer-agnostic, so `consumer_id` is
        // intentionally None. The cross-user attribution channel is
        // `sender_id`.
        consumer_id: None,
        // Irrelevant on the `Dashboard` path (the gate is bypassed
        // by `ActorKind::Dashboard`), but we surface the human's
        // class for completeness.
        consumer_class: mwe_core::jwt::ConsumerClass::Standard,
    };

    let req = PushRequest {
        mode: PushMode::Upsert,
        wiki_id: Some(wiki_id.clone()),
        parent_wiki_id: None,
        slug: None,
        title: None,
        wiki_type: None,
        smart: false,
        project_id: None,
        pages: vec![PushPage {
            path: page_path.clone(),
            content: form.body.clone(),
        }],
        deletes: Vec::new(),
        // Dashboard editor never opportunistically marks briefing
        // items processed — the comment-recepiment loop is the
        // smart-consumer's responsibility.
        mark_processed: Vec::new(),
        expected_op_log_head: None,
    };

    match mwe_core::wiki_admin::push(
        &state.pool,
        &memory.tree,
        &caller,
        ActorKind::Dashboard,
        req,
    )
    .await
    {
        Ok(resp) => {
            tracing::info!(
                actor = %user.sender_id,
                wiki = %id,
                page = %page_path,
                op_log_id = resp.op_log_id,
                "dashboard editor saved page"
            );
            Ok(Redirect::to(&format!(
                "/dashboard/wiki/{}/edit/{}",
                wiki_id.as_str(),
                page_path
            ))
            .into_response())
        },
        Err(err) => {
            let msg = render_admin_error(&err);
            Ok(Html(render_edit_form(
                chrome,
                &user,
                &id,
                &page_path,
                &form.body,
                Some(&msg),
            ))
            .into_response())
        },
    }
}

/// Convert a `wiki_admin::push` failure into a user-facing flash. The
/// dashboard editor never proxies the raw error to keep the surface
/// stable: a `WikiOwnedByOtherUser` here would indicate a state
/// mismatch since `enforce_owner_or_not_found` already ran.
fn render_admin_error(err: &AdminError) -> String {
    match err {
        AdminError::InvalidInput(msg) => format!("Save refused: {msg}"),
        AdminError::WikiLockedByLease {
            held_by_consumer_id,
            held_by_sender_id,
            expires_at,
            ..
        } => format!(
            "This wiki is locked by a cooperative lease (sender_id={held_by_sender_id}, \
             consumer_id={held_by_consumer_id:?}, expires_at={expires_at}). Try again after \
             the lease expires."
        ),
        other => format!("Save failed: {other}"),
    }
}

fn render_edit_form(
    chrome: layout::Chrome,
    user: &SessionUser,
    wiki_id: &str,
    page_path: &str,
    body: &str,
    error: Option<&str>,
) -> String {
    let title = format!("Edit — {wiki_id}/{page_path}");
    let html_body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        section.meta {
            dl {
                dt { "wiki_id" } dd { code { (wiki_id) } }
                dt { "page" }    dd { code { (page_path) } }
            }
        }

        (components::flash(
            "warning",
            "Raw free-text editing is a discouraged admin-only escape hatch. \
             Prefer the blessed channels: inline comments, the operative chat, \
             and the structured fact actions (ACL / validity / supersede). \
             Smart-wiki pages cannot be edited here at all.",
        ))

        p.muted {
            "The save goes through "
            code { "mwe_core::wiki_admin::push" }
            " with "
            code { "actor_kind = Dashboard" }
            ", so it lands in "
            code { "wiki_admin_op_log" }
            " alongside smart-consumer pushes and can be reverted from the op-log view."
        }

        form action=(format!("/dashboard/wiki/{wiki_id}/edit/{page_path}")) method="post" {
            p {
                label for="body" { "Page body" }
                textarea id="body" name="body" rows="24" cols="80" {
                    (body)
                }
            }
            (components::submit("Save"))
        }

        p {
            a href=(format!("/dashboard/wiki/{wiki_id}")) { "← Back to wiki" }
        }
    };
    layout::authenticated_reading_page(chrome, &title, user, &html_body)
}

/// Returns `Ok(())` if the caller owns the wiki, `Err(NotFound)`
/// otherwise. The shared write path of `wiki_admin::push` enforces
/// the same gate at write time; this pre-check is for the GET form
/// page so we don't render an editor for a wiki the operator can't
/// save into.
async fn enforce_owner_or_not_found(
    pool: &sqlx::SqlitePool,
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    user: &SessionUser,
) -> Result<()> {
    let Ok(handle) = memory.tree.locate(wiki_id) else {
        return Err(DashboardError::NotFound);
    };
    let outcome = resolve_read_access(pool, &memory.tree, &handle, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("resolve_read_access: {e}")))?;
    // Owner-equivalent (the user owner, or a member of the owning group) may
    // edit; sharing grants reads/notify but NOT writes — non-equivalents 404.
    if !outcome.is_owner_equivalent() {
        return Err(DashboardError::NotFound);
    }
    Ok(())
}

// ---------- page «what goes here» (testata description) editor ----------
//
// A leaf page's testata `description` is the one frontmatter field meant to
// be hand-authored (the rest of the testata is compiler output that REM
// regenerates): it labels the page «what goes here», guides the planner's
// fact placement, and titles the page in recall navigation. Editing it is
// LIGHTER than the raw editor (admin AND owner): it is a placement hint, not
// content, so the gate is standard-wiki + (owner OR admin) — an operator can
// curate any user's cards, an owner annotates their own.

/// The "✎ Edit «what goes here»" affordance shown under a page view to an
/// owner / admin (gated by `PageViewFlags::can_edit_meta`). Links to the
/// page-description editor.
fn render_describe_affordance(wiki_id: &WikiId, page_path: &str) -> Markup {
    html! {
        p.muted.page-meta-actions {
            a href=(format!(
                "/dashboard/wiki/{}/describe/{}",
                wiki_id.as_str(),
                page_path
            )) { "✎ Edit «what goes here»" }
            " — the page's one-line purpose (guides fact placement + recall navigation)."
        }
    }
}

/// Cap on the page testata `description` — a one-liner card, not prose. A
/// generous tweet-length bound that refuses an accidental whole-page paste.
const MAX_PAGE_DESCRIPTION_LEN: usize = 280;

/// Body of the describe POST.
#[derive(Debug, Deserialize)]
pub struct DescribeSubmission {
    #[serde(default)]
    pub description: String,
}

/// Whether `user` may edit a standard page's testata `description`: standard
/// wiki only (smart pages are consumer-authored), and the caller must own the
/// wiki OR be an admin. Drives both the gate and the view-page affordance, so
/// the link never shows where the POST would 404.
async fn may_edit_page_meta(
    pool: &sqlx::SqlitePool,
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    user: &SessionUser,
) -> Result<bool> {
    let Ok(handle) = memory.tree.locate(wiki_id) else {
        return Ok(false);
    };
    if handle.meta().smart {
        return Ok(false);
    }
    if user.is_admin {
        return Ok(true);
    }
    let outcome = resolve_read_access(pool, &memory.tree, &handle, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("resolve_read_access: {e}")))?;
    Ok(outcome.is_owner_equivalent())
}

/// Resolve a page's absolute path under its wiki, refusing `_meta.md`, unsafe
/// (traversal) paths, and pages that do not exist. Shared by the describe
/// GET / POST so they agree on what is editable.
fn resolve_existing_page(
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    page_path: &str,
) -> Result<std::path::PathBuf> {
    if is_meta_filename(page_path) {
        return Err(meta_route_violation());
    }
    let rel = PathBuf::from(page_path);
    if !mwe_core::wiki::is_safe_page_path(&rel) {
        return Err(DashboardError::BadRequest(format!(
            "unsafe page path: {page_path}"
        )));
    }
    let handle = memory.tree.locate(wiki_id).map_err(map_wiki_err)?;
    let abs = handle.abs_dir().join(&rel);
    if !abs.is_file() {
        return Err(DashboardError::NotFound);
    }
    Ok(abs)
}

async fn describe_form(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    if !may_edit_page_meta(&state.pool, memory, &wiki_id, &user).await? {
        return Err(DashboardError::NotFound);
    }
    let abs = resolve_existing_page(memory, &wiki_id, &page_path)?;
    let current = mwe_core::meta_annotate::read_page_description(&abs)
        .map_err(|e| DashboardError::Internal(format!("read page description: {e}")))?
        .unwrap_or_default();
    Ok(Html(render_describe_form(
        chrome, &user, &id, &page_path, &current, None,
    ))
    .into_response())
}

async fn submit_describe(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath((id, page_path)): AxumPath<(String, String)>,
    HtmlForm(form): HtmlForm<DescribeSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    if !may_edit_page_meta(&state.pool, memory, &wiki_id, &user).await? {
        return Err(DashboardError::NotFound);
    }
    let abs = resolve_existing_page(memory, &wiki_id, &page_path)?;

    if form.description.chars().count() > MAX_PAGE_DESCRIPTION_LEN {
        let msg = format!(
            "Too long — keep «what goes here» under {MAX_PAGE_DESCRIPTION_LEN} characters."
        );
        return Ok(Html(render_describe_form(
            chrome,
            &user,
            &id,
            &page_path,
            &form.description,
            Some(&msg),
        ))
        .into_response());
    }

    mwe_core::meta_annotate::set_page_description(&abs, &form.description)
        .map_err(|e| DashboardError::Internal(format!("set page description: {e}")))?;
    tracing::info!(
        actor = %user.sender_id,
        wiki = %id,
        page = %page_path,
        "dashboard set page testata description"
    );
    Ok(Redirect::to(&format!(
        "/dashboard/wiki/{}/view/{}",
        wiki_id.as_str(),
        page_path
    ))
    .into_response())
}

fn render_describe_form(
    chrome: layout::Chrome,
    user: &SessionUser,
    id: &str,
    page_path: &str,
    current: &str,
    error: Option<&str>,
) -> String {
    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        section.meta {
            dl {
                dt { "wiki" } dd { code { (id) } }
                dt { "page" } dd { code { (page_path) } }
            }
        }
        p.muted {
            "The «what goes here» one-liner is the page's "
            strong { "testata description" }
            " — a short note on what this page is for. It guides where new "
            "facts get placed (the planner shows it to other pages) and labels "
            "the page in recall navigation. It is "
            strong { "preserved across recompiles" }
            " — REM never overwrites it. Leave blank to clear."
        }
        form action=(format!("/dashboard/wiki/{id}/describe/{page_path}")) method="post" {
            p {
                label for="description" { "What goes here" }
                textarea id="description" name="description" rows="3" cols="60"
                    placeholder="e.g. Alice's health: doctors, meds, appointments" {
                    (current)
                }
            }
            (components::submit("Save"))
        }
        p {
            a href=(format!("/dashboard/wiki/{id}/view/{page_path}")) { "← Back to page" }
        }
    };
    let title = format!("Describe — {page_path}");
    layout::authenticated_reading_page(chrome, &title, user, &body)
}

/// Gate the raw free-text page editor (roadmap 6j: manual wiki editing
/// from the dashboard is forbidden / discouraged; the blessed channels
/// are inline comments, the operative chat, and the structured fact
/// actions).
///
/// - **Smart wikis: hard-forbidden.** The smart consumer is the sole
///   writer — a raw page rewrite would collide with its pushes. Returns a
///   `404 Not Found` so the route is indistinguishable from a missing
///   page (no information leak that the smart wiki even exists / has a raw
///   editor), with a message pointing at the consumer.
/// - **Standard wikis: admin-only.** Demoted to a prominently-discouraged
///   escape hatch — a non-admin gets `403 Forbidden`. The owner check
///   still runs after this (an admin editing a wiki they do not own is a
///   separate concern handled by `enforce_owner_or_not_found`).
///
/// A wiki that cannot be located is a `404` (mirrors the owner gate).
fn enforce_raw_editor_allowed(
    memory: &crate::state::MemoryHandles,
    wiki_id: &WikiId,
    user: &SessionUser,
) -> Result<()> {
    let Ok(handle) = memory.tree.locate(wiki_id) else {
        return Err(DashboardError::NotFound);
    };
    if handle.meta().smart {
        // Hard-forbidden — surface as NotFound so the smart raw editor is
        // not even discoverable. The message points at the blessed
        // channel (the smart consumer / comments).
        return Err(DashboardError::NotFound);
    }
    if !user.is_admin {
        return Err(DashboardError::Forbidden);
    }
    Ok(())
}

fn is_meta_filename(rel: &str) -> bool {
    // Normalise separators so `_meta.md` is refused on both Unix and
    // Windows-style submissions.
    let normalised = rel.replace('\\', "/");
    normalised == META_FILENAME || normalised.ends_with(&format!("/{META_FILENAME}"))
}

fn meta_route_violation() -> DashboardError {
    DashboardError::Validation(format!(
        "{META_FILENAME} is not editable from this route. \
         Use /dashboard/wiki/:id/sharing for shared_with edits."
    ))
}

fn map_wiki_err(e: mwe_core::wiki::WikiError) -> DashboardError {
    match e {
        mwe_core::wiki::WikiError::WikiNotFound { .. }
        | mwe_core::wiki::WikiError::PageNotFound { .. } => DashboardError::NotFound,
        other => DashboardError::Internal(other.to_string()),
    }
}

fn require_memory(state: &DashboardState) -> Result<&crate::state::MemoryHandles> {
    state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_meta_filename_matches_exact_and_nested() {
        assert!(is_meta_filename("_meta.md"));
        assert!(is_meta_filename("alice/_meta.md"));
        assert!(is_meta_filename("alice\\_meta.md"));
        assert!(!is_meta_filename("notes/_meta_backup.md"));
        assert!(!is_meta_filename("_meta.md.bak"));
        assert!(!is_meta_filename("index.md"));
    }

    // ---------- annotate_fact_refs (region → fact click-through) ----------

    fn seg(text: &str, fact_id: Option<&str>) -> render::RenderSegment {
        render::RenderSegment {
            text: text.to_owned(),
            fact_id: fact_id.map(|f| mwe_core::types::FactId::parse(f).unwrap()),
        }
    }

    const FID: &str = "018f1234-5678-7abc-9def-0123456789ab";

    #[test]
    fn annotate_fact_refs_marks_fact_segments_before_trailing_whitespace() {
        let segments = vec![
            seg("Alice pesa ", None),
            seg("72 kg", Some(FID)),
            seg(" al 10 maggio.", None),
        ];
        assert_eq!(
            annotate_fact_refs(&segments),
            format!("Alice pesa 72 kg{{{{factref={FID}}}}} al 10 maggio.")
        );
        // A block region ending in newlines keeps the marker on its last
        // prose line, not in a paragraph of its own.
        let segments = vec![seg("Body line.\n\n", Some(FID)), seg("Next para.", None)];
        assert_eq!(
            annotate_fact_refs(&segments),
            format!("Body line.{{{{factref={FID}}}}}\n\nNext para.")
        );
    }

    #[test]
    fn annotate_fact_refs_leaves_factless_segments_verbatim() {
        // A redacted region is fact-less filler — no marker may appear.
        let segments = vec![seg("Alice pesa [redacted] al 10 maggio.", None)];
        assert_eq!(
            annotate_fact_refs(&segments),
            "Alice pesa [redacted] al 10 maggio."
        );
    }

    #[test]
    fn annotate_fact_refs_keeps_a_fence_close_line_clean() {
        let segments = vec![seg("```\ncode\n```\n", Some(FID))];
        assert_eq!(
            annotate_fact_refs(&segments),
            format!("```\ncode\n```\n{{{{factref={FID}}}}}")
        );
    }

    #[test]
    fn annotate_fact_refs_places_the_marker_inside_a_reveal_block() {
        // The reveal-highlighted block segment shape produced by
        // `render_admin_reveal_segments`: the marker must land inside the
        // wrapper (after the body prose), never glued to `</div>` where
        // it would corrupt the HTML-block event.
        let text = format!(
            "\n\n{}\n\nBob ha un segreto.\n\n{}\n\n",
            mwe_core::render::ACL_REVEAL_BLOCK_OPEN,
            mwe_core::render::ACL_REVEAL_BLOCK_CLOSE
        );
        let segments = vec![seg(&text, Some(FID))];
        assert_eq!(
            annotate_fact_refs(&segments),
            format!(
                "\n\n{}\n\nBob ha un segreto.{{{{factref={FID}}}}}\n\n{}\n\n",
                mwe_core::render::ACL_REVEAL_BLOCK_OPEN,
                mwe_core::render::ACL_REVEAL_BLOCK_CLOSE
            )
        );
    }

    // ---------- wikilink href resolution ----------

    fn link_index_fixture() -> (
        tempfile::TempDir,
        std::collections::BTreeMap<String, PathBuf>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let alice = dir.path().join("alice");
        std::fs::create_dir_all(alice.join("modules")).unwrap();
        std::fs::write(alice.join("notes.md"), "x").unwrap();
        std::fs::write(alice.join("modules/auth.md"), "x").unwrap();
        let famiglia = dir.path().join("famiglia");
        let sub = famiglia.join("bruno-battaglia");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(famiglia.join("dossier.md"), "x").unwrap();
        std::fs::write(famiglia.join("notes.md"), "x").unwrap();
        std::fs::write(sub.join("referto.md"), "x").unwrap();
        let mut index = std::collections::BTreeMap::new();
        index.insert("alice".to_owned(), alice);
        index.insert("famiglia".to_owned(), famiglia);
        index.insert("famiglia-bruno-battaglia".to_owned(), sub);
        (dir, index)
    }

    #[test]
    fn resolve_wikilink_href_resolves_both_canonical_forms() {
        let (_dir, index) = link_index_fixture();
        assert_eq!(
            resolve_wikilink_href(&index, None, "alice").as_deref(),
            Some("/dashboard/wiki/alice")
        );
        assert_eq!(
            resolve_wikilink_href(&index, None, "alice/notes").as_deref(),
            Some("/dashboard/wiki/alice/view/notes.md")
        );
        // Nested page slug keeps its separators.
        assert_eq!(
            resolve_wikilink_href(&index, None, "alice/modules/auth").as_deref(),
            Some("/dashboard/wiki/alice/view/modules/auth.md")
        );
        // Flat sub-wiki id — never a directory path.
        assert_eq!(
            resolve_wikilink_href(&index, None, "famiglia-bruno-battaglia/referto").as_deref(),
            Some("/dashboard/wiki/famiglia-bruno-battaglia/view/referto.md")
        );
    }

    #[test]
    fn resolve_wikilink_href_rejects_dead_and_unsafe_targets() {
        let (_dir, index) = link_index_fixture();
        // Unknown wiki (the underscored mutant class included).
        assert_eq!(resolve_wikilink_href(&index, None, "ghost"), None);
        assert_eq!(
            resolve_wikilink_href(&index, None, "famiglia_bruno_battaglia/referto"),
            None
        );
        // Known wiki, missing page.
        assert_eq!(resolve_wikilink_href(&index, None, "alice/missing"), None);
        // Traversal never reaches the filesystem.
        assert_eq!(resolve_wikilink_href(&index, None, "alice/../secret"), None);
        assert_eq!(resolve_wikilink_href(&index, None, ""), None);
    }

    #[test]
    fn resolve_wikilink_href_legacy_bare_slug_resolves_in_deterministic_tree_order() {
        let (_dir, index) = link_index_fixture();
        // Current wiki first: `notes.md` exists in both `alice` and
        // `famiglia`, and `[[notes]]` on an alice page stays home.
        assert_eq!(
            resolve_wikilink_href(&index, Some("alice"), "notes").as_deref(),
            Some("/dashboard/wiki/alice/view/notes.md")
        );
        // Ancestor next: `[[dossier]]` on a page of the emerged sub-wiki
        // reaches the parent's page (the prod dossier-stub shape).
        assert_eq!(
            resolve_wikilink_href(&index, Some("famiglia-bruno-battaglia"), "dossier").as_deref(),
            Some("/dashboard/wiki/famiglia/view/dossier.md")
        );
        // Unrelated wikis last: the corpus cross-links by bare name
        // (`[[referto]]` on an alice page names the sub-wiki's page).
        assert_eq!(
            resolve_wikilink_href(&index, Some("alice"), "referto").as_deref(),
            Some("/dashboard/wiki/famiglia-bruno-battaglia/view/referto.md")
        );
        // A wiki id always wins over a same-named page: `alice/alice.md`
        // exists, but the bare target is the wiki.
        std::fs::write(index["alice"].join("alice.md"), "x").unwrap();
        assert_eq!(
            resolve_wikilink_href(&index, Some("alice"), "alice").as_deref(),
            Some("/dashboard/wiki/alice")
        );
        // Still dead: no such page anywhere, no current wiki, slashed
        // targets never fall back, traversal stays rejected.
        assert_eq!(resolve_wikilink_href(&index, Some("alice"), "ghost"), None);
        assert_eq!(resolve_wikilink_href(&index, None, "notes"), None);
        assert_eq!(
            resolve_wikilink_href(&index, Some("alice"), "ghost/notes"),
            None
        );
        assert_eq!(
            resolve_wikilink_href(&index, Some("alice"), "../secret"),
            None
        );
    }

    // ---------- relative markdown-link href resolution ----------

    #[test]
    fn resolve_relative_page_href_rewrites_wiki_relative_md_targets() {
        let (_dir, index) = link_index_fixture();
        let alice = &index["alice"];
        // From the wiki root (the home page's base).
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "", "notes.md").as_deref(),
            Some("/dashboard/wiki/alice/view/notes.md")
        );
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "", "modules/auth.md").as_deref(),
            Some("/dashboard/wiki/alice/view/modules/auth.md")
        );
        // From a nested page's directory: sibling and `..` hops.
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "modules", "auth.md").as_deref(),
            Some("/dashboard/wiki/alice/view/modules/auth.md")
        );
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "modules", "../notes.md").as_deref(),
            Some("/dashboard/wiki/alice/view/notes.md")
        );
        // `./` and case variants normalize to the on-disk spelling.
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "", "./Modules/Auth.MD").as_deref(),
            Some("/dashboard/wiki/alice/view/modules/auth.md")
        );
        // A fragment rides along on the rewritten href.
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "", "notes.md#history").as_deref(),
            Some("/dashboard/wiki/alice/view/notes.md#history")
        );
    }

    #[test]
    fn resolve_relative_page_href_leaves_everything_else_as_authored() {
        let (_dir, index) = link_index_fixture();
        let alice = &index["alice"];
        for dest in [
            "",
            "#top",                     // bare in-page anchor
            "/dashboard/wiki/alice",    // site-absolute
            "https://example.com/x.md", // scheme'd URL
            "mailto:bob@example.com",   // scheme'd, no path
            "notes.md?raw=1",           // query-carrying
            "notes.txt",                // non-.md target
            "missing.md",               // known-shape, dead file
            "../secret.md",             // escapes the wiki root
            "modules//auth.md",         // empty segment
        ] {
            assert_eq!(
                resolve_relative_page_href(alice, "alice", "", dest),
                None,
                "{dest} must stay as authored"
            );
        }
        // `..` above the root from a nested base is still an escape.
        assert_eq!(
            resolve_relative_page_href(alice, "alice", "modules", "../../secret.md"),
            None
        );
    }

    #[test]
    fn encode_path_segments_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(encode_path_segments("alice"), "alice");
        assert_eq!(encode_path_segments("modules/auth.md"), "modules/auth.md");
        assert_eq!(encode_path_segments("a b/c\"d"), "a%20b/c%22d");
    }
}
