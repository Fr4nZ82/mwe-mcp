// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smart-wiki views.
//!
//! Five routes layered on top of the generic wiki viewer:
//!
//! - `/dashboard/wiki/smart`             — the **Smart** tab of the
//!   unified Wikis page: lists every smart-family wiki (filters on the
//!   per-wiki `_meta.md` smart flag) with the last `wiki_admin_op_log`
//!   timestamp and the unread briefing count. The sibling **Standard**
//!   tab ([`super::wiki_view::list`], `/dashboard/wiki`) lists the
//!   narrative-compiler family; both share one "Wikis" nav entry,
//!   switched by [`super::wiki_view::wiki_family_tabs`].
//! - `/dashboard/wiki/:id/briefing`      — render `_briefing.md` as
//!   plain text + show the unread / archived item count from
//!   `wiki_briefing_items`.
//! - `/dashboard/wiki/:id/op-log`        — paged history table from
//!   `wiki_admin_op_log` for one wiki (200 newest); revertable rows
//!   carry a Revert button (admin-only) wired to
//!   [`mwe_core::wiki_admin::op_revert`].
//! - `POST /dashboard/wiki/:id/op-log/:op_id/revert` — strict-conflict
//!   revert. 404 / 400 / 409 are surfaced as
//!   flash banners on the op-log view; success redirects back with a
//!   success banner.
//! - `/dashboard/wiki/:id/sharing`       — show + edit the
//!   `_meta.md.shared_with` roster. Owner-only.
//!
//! All routes session-gated; sharing additionally checks the caller is the
//! wiki's owner (the derived scope principal is `user:<caller>`); the
//! revert POST is gated [`AdminUser`].

use std::path::Path;
use std::str::FromStr;

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::types::{Principal, WikiId};
use mwe_core::wiki::{META_FILENAME, WikiMeta, atomic_write, wiki_get_meta, wiki_read};
use mwe_core::wiki_admin::RevertError;
use serde::Deserialize;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};

pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/wiki/smart", get(list_smart_wikis))
        .route("/wiki/:id/briefing", get(view_briefing))
        .route("/wiki/:id/op-log", get(view_op_log))
        .route("/wiki/:id/op-log/:op_id/revert", post(submit_revert))
        .route("/wiki/:id/sharing", get(view_sharing).post(submit_sharing))
}

#[derive(Debug)]
struct SmartWikiRow {
    wiki_id: String,
    title: String,
    wiki_type: String,
    /// The wiki is a consumer agent's own operational memory: the `_meta.md`
    /// `is_agent` marker the sign-in flow stamps, **or** the `agent`
    /// `wiki_type` label that flow also writes.
    ///
    /// The union, unlike the signpost gate, which trusts the marker alone. The
    /// label is a free-form string the consumer passes to `wiki_admin_push`, so
    /// it can be claimed — but here the only thing at stake is a word next to
    /// your own wiki's type, while trusting the marker alone would leave every
    /// operational wiki forged before the marker existed unbadged until its
    /// next sign-in, which reads as the feature being broken.
    is_agent: bool,
    last_push: Option<String>,
    unread_briefing: i64,
}

async fn last_push_for(state: &DashboardState, wiki_id: &str) -> Result<Option<String>> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT ts FROM wiki_admin_op_log WHERE wiki_id = ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(wiki_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row)
}

async fn unread_briefing_for(state: &DashboardState, wiki_id: &str) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM wiki_briefing_items WHERE wiki_id = ? AND processed_at IS NULL",
    )
    .bind(wiki_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(count)
}

async fn list_smart_wikis(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: axum_extra::extract::cookie::CookieJar,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;

    let discovered = memory
        .tree
        .walk()
        .map_err(|e| DashboardError::Internal(format!("walk wikis: {e}")))?;

    // Smart family = the per-wiki `_meta.md` smart flag
    // (replaces the `wiki_types_registry` lookup).
    // The list is a read surface like any other: a smart wiki appears only to
    // someone who may read it (its owner, a member of the owning group, or a
    // `shared_with` entry). Until 2026-07-30 it listed every smart wiki to
    // every signed-in user — and the page behind it opened for them too.
    let reveal_all = crate::reveal::active(&state, &user, &jar);
    let mut rows = Vec::new();
    for d in discovered {
        if !d.meta.smart {
            continue;
        }
        if !reveal_all
            && !super::wiki_view::wiki_readable(&state, memory, &d.meta.wiki_id, &user.sender_id)
                .await?
        {
            continue;
        }
        let wiki_id = d.meta.wiki_id.as_str().to_owned();
        let last_push = last_push_for(&state, &wiki_id).await?;
        let unread = unread_briefing_for(&state, &wiki_id).await?;
        rows.push(SmartWikiRow {
            wiki_id,
            title: d.meta.title.clone(),
            wiki_type: d.meta.wiki_type.clone(),
            is_agent: d.meta.is_agent || d.meta.wiki_type == mwe_core::wiki::AGENT_WIKI_TYPE,
            last_push,
            unread_briefing: unread,
        });
    }
    rows.sort_by(|a, b| a.wiki_id.cmp(&b.wiki_id));

    let body = html! {
        (crate::routes::wiki_view::wiki_family_tabs(/* smart_active */ true))
        p.muted {
            "Read-only listing of every smart-family wiki the engine "
            "has indexed. Smart wikis are authoritatively managed by "
            "smart consumers via " code { "wiki_admin_*" }
            "; REM skips the legacy write-jobs on these and only runs the "
            "read-side sub-jobs (recall pre-indexing, Briefing dispatcher, "
            "Backlink reciprocity, lease expirer)."
        }

        @if rows.is_empty() {
            p.muted {
                "No smart wikis yet — a smart consumer will create the "
                "first one via " code { "wiki_admin_push mode=create" }
                ". See " a href="/dashboard/bridges" { "Bridges" }
                " to wire a consumer."
            }
        } @else {
            table {
                thead { tr {
                    th { "Wiki id" }
                    th { "Title" }
                    th { "Type" }
                    th { "Last push" }
                    th { "Unread briefing" }
                    th { "Actions" }
                } }
                tbody {
                    @for r in &rows {
                        tr {
                            td {
                                a href=(format!("/dashboard/wiki/{}", r.wiki_id)) {
                                    code { (r.wiki_id) }
                                }
                            }
                            td { (r.title) }
                            td.muted {
                                (r.wiki_type)
                                @if r.is_agent { " " span.badge { "agent" } }
                            }
                            td.muted { (r.last_push.clone().unwrap_or_else(|| "—".to_owned())) }
                            td {
                                @if r.unread_briefing > 0 {
                                    strong { (r.unread_briefing) }
                                } @else {
                                    span.muted { "0" }
                                }
                            }
                            td.muted {
                                a href=(format!("/dashboard/wiki/{}/briefing", r.wiki_id)) { "briefing" }
                                " · "
                                a href=(format!("/dashboard/wiki/{}/op-log", r.wiki_id)) { "op-log" }
                                " · "
                                a href=(format!("/dashboard/wiki/{}/sharing", r.wiki_id)) { "sharing" }
                            }
                        }
                    }
                }
            }
        }
    };

    Ok(Html(layout::authenticated_page(
        chrome, "Wikis", &user, &body,
    )))
}

async fn view_briefing(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let _meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let body_md = match wiki_read(&memory.tree, &wiki_id, Path::new("_briefing.md")) {
        Ok(s) => Some(s),
        Err(mwe_core::wiki::WikiError::PageNotFound { .. }) => None,
        Err(e) => return Err(map_wiki_err(e)),
    };

    let unread_count = unread_briefing_for(&state, wiki_id.as_str()).await?;
    let total_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM wiki_briefing_items WHERE wiki_id = ?")
            .bind(wiki_id.as_str())
            .fetch_one(&state.pool)
            .await?;

    let body = html! {
        section.meta {
            dl {
                dt { "wiki_id" }       dd { code { (wiki_id.as_str()) } }
                dt { "Unread items" }  dd { (unread_count) }
                dt { "Total items" }   dd { (total_count) }
            }
        }

        h2 { "_briefing.md" }
        @if let Some(body_md) = &body_md {
            pre.wiki-page { (body_md) }
        } @else {
            p.muted {
                code { "_briefing.md" }
                " does not exist yet. It is created on demand by the first "
                code { "wiki_admin_notify" }
                " call against this wiki."
            }
        }

        p.muted {
            "This is the inbox channel — REM's Briefing dispatcher + "
            "Backlink reciprocity detector, openclaw forwards, shared-with "
            "team notifications all land here. The smart consumer reads "
            "it at session start and archives items into " code { "_briefing.archive.md" }
            " on action."
        }

        p {
            a href=(format!("/dashboard/wiki/{}", wiki_id.as_str())) { "← Back to wiki" }
            " · "
            a href="/dashboard/wiki/smart" { "Smart wikis" }
        }
    };

    let title = format!("Briefing — {}", wiki_id.as_str());
    Ok(Html(layout::authenticated_page(
        chrome, &title, &user, &body,
    )))
}

#[derive(Debug)]
struct OpLogEntry {
    op_id: i64,
    op_kind: String,
    op_mode: Option<String>,
    actor_kind: String,
    sender_id: String,
    consumer_id: Option<String>,
    pages_affected: i64,
    payload_hash: String,
    ts: String,
    has_pre_image: bool,
}

type OpLogRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
);

/// Reasons why a row's Revert button is rendered as a disabled tooltip
/// instead of a live form. Mirror of the negative branches of
/// [`mwe_core::wiki_admin::op_revert`] so the UI can pre-empt the user
/// before the POST fires.
fn revert_disabled_reason(entry: &OpLogEntry) -> Option<&'static str> {
    if entry.actor_kind == "system" {
        return Some(
            "Not revertable: this is a system compensation row (click the original target again to undo).",
        );
    }
    if !entry.op_kind.starts_with("push_") {
        return Some("Not revertable: non-write op (pull / notify).");
    }
    if !entry.has_pre_image {
        return Some(
            "Not revertable: no pre-image captured (legacy row or create — the wiki did not exist before).",
        );
    }
    None
}

/// Banner key passed back as `?flash=<key>` after a redirect from the
/// POST handler. Plain enum to keep the URL surface small + greppable
/// in server logs; the actual prose lives in `render_op_log` so it
/// stays in the same file as the table.
#[derive(Debug, Deserialize)]
struct OpLogFlashQuery {
    #[serde(default)]
    flash: Option<String>,
}

async fn view_op_log(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<OpLogFlashQuery>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let _meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let rows: Vec<OpLogRow> = sqlx::query_as(
        "SELECT op_id, op_kind, op_mode, actor_kind, sender_id, consumer_id,
                pages_affected, payload_hash, ts, pre_image_json
           FROM wiki_admin_op_log
          WHERE wiki_id = ?
          ORDER BY op_id DESC
          LIMIT 200",
    )
    .bind(wiki_id.as_str())
    .fetch_all(&state.pool)
    .await?;
    let entries: Vec<OpLogEntry> = rows
        .into_iter()
        .map(
            |(
                op_id,
                op_kind,
                op_mode,
                actor_kind,
                sender_id,
                consumer_id,
                pages_affected,
                payload_hash,
                ts,
                pre_image_json,
            )| {
                OpLogEntry {
                    op_id,
                    op_kind,
                    op_mode,
                    actor_kind,
                    sender_id,
                    consumer_id,
                    pages_affected,
                    payload_hash,
                    ts,
                    has_pre_image: pre_image_json.is_some(),
                }
            },
        )
        .collect();

    Ok(Html(render_op_log(
        chrome,
        &user,
        &wiki_id,
        &entries,
        q.flash.as_deref(),
    )))
}

fn render_op_log(
    chrome: layout::Chrome,
    user: &SessionUser,
    wiki_id: &WikiId,
    entries: &[OpLogEntry],
    flash_key: Option<&str>,
) -> String {
    let flash_banner: Option<Markup> = flash_key.and_then(decode_op_log_flash);

    let body = html! {
        @if let Some(banner) = &flash_banner {
            (banner)
        }
        p.muted {
            "Append-only audit log of every " code { "wiki_admin_*" }
            " mutation on this wiki. Showing the most recent 200 entries. "
            @if chrome.read_only {
                "This instance is read-only, so nothing here can be reverted."
            } @else {
                "Admin users can revert individual " code { "push_*" } " rows "
                "via the strict-conflict policy: a refusal banner "
                "fires when any later op touched the same page."
            }
        }
        @if entries.is_empty() {
            p.muted { "No admin operations recorded yet." }
        } @else {
            table {
                thead { tr {
                    th { "Op id" }
                    th { "Kind" }
                    th { "Mode" }
                    th { "Actor" }
                    th { "Sender" }
                    th { "Consumer" }
                    th { "Pages" }
                    th { "Payload hash" }
                    th { "Timestamp" }
                    th { "Actions" }
                } }
                tbody {
                    @for e in entries {
                        tr {
                            td.muted { (e.op_id) }
                            td { code { (e.op_kind) } }
                            td.muted { (e.op_mode.clone().unwrap_or_else(|| "—".to_owned())) }
                            td.muted { code { (e.actor_kind) } }
                            td { code { (e.sender_id) } }
                            td.muted { (e.consumer_id.clone().unwrap_or_else(|| "—".to_owned())) }
                            td.muted { (e.pages_affected) }
                            td.muted { code { (&e.payload_hash[..16.min(e.payload_hash.len())]) "…" } }
                            td.muted { (e.ts) }
                            td.muted {
                                (render_revert_cell(user, wiki_id, e, chrome.read_only))
                            }
                        }
                    }
                }
            }
        }
        p {
            a href=(format!("/dashboard/wiki/{}", wiki_id.as_str())) { "← Back to wiki" }
            " · "
            a href="/dashboard/wiki/smart" { "Smart wikis" }
        }
    };

    let title = format!("Op log — {}", wiki_id.as_str());
    layout::authenticated_page(chrome, &title, user, &body)
}

fn render_revert_cell(
    user: &SessionUser,
    wiki_id: &WikiId,
    entry: &OpLogEntry,
    frozen: bool,
) -> Markup {
    // Only admin users see any revert affordance at all — non-admins
    // get a muted dash so the column still aligns cleanly, and a frozen
    // deployment reverts nothing at all.
    if !user.is_admin || frozen {
        return html! { span.muted { "—" } };
    }
    if let Some(reason) = revert_disabled_reason(entry) {
        return html! {
            span.muted title=(reason) { "—" }
        };
    }
    let action = format!(
        "/dashboard/wiki/{}/op-log/{}/revert",
        wiki_id.as_str(),
        entry.op_id
    );
    let confirm = format!(
        "Revert op {} ({})? This will write a compensating row tagged \
         actor_kind=system. Refused with a banner if any later op touched \
         the same pages (strict conflict policy).",
        entry.op_id, entry.op_kind
    );
    components::destructive_form(&action, "Revert", &confirm)
}

/// Translate the opaque `flash` query key into a banner. Keeping the
/// keys narrow (rather than embedding the message in the URL) avoids
/// open-redirect / XSS surface — every legal flash key maps to a
/// hard-coded Markup block.
fn decode_op_log_flash(key: &str) -> Option<Markup> {
    let (kind, message): (&str, String) = match key {
        "revert_ok" => (
            "success",
            "Revert applied — compensating op written.".to_owned(),
        ),
        "revert_conflict" => (
            "error",
            "Revert refused: at least one later op touched the same page(s) (strict conflict policy). \
             Manual fall-back: revert the conflicting later op first, then retry; or read the row's \
             pre-image JSON from the API and re-paste via the editor."
                .to_owned(),
        ),
        "revert_not_revertable" => (
            "error",
            "Revert refused: this row carries no pre-image or is a non-write / system row. \
             Compensation rows are not revertable — click the original target instead."
                .to_owned(),
        ),
        "revert_not_found" => (
            "error",
            "Revert refused: no such op_id in the log for this wiki.".to_owned(),
        ),
        "revert_failed" => (
            "error",
            "Revert failed due to an internal error. Check the server logs.".to_owned(),
        ),
        _ => return None,
    };
    Some(components::flash(kind, &message))
}

async fn submit_revert(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath((id, op_id)): AxumPath<(String, i64)>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;

    // Confirm the wiki exists — guards against a typo'd id leaking
    // information through the revert handler's `op_log_id` lookup.
    let _meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;

    let sender = admin.sender_id().to_owned();
    let result = mwe_core::wiki_admin::op_revert(&state.pool, &memory.tree, op_id, &sender).await;
    let target = format!("/dashboard/wiki/{}/op-log", wiki_id.as_str());
    let flash = match result {
        Ok(outcome) => {
            tracing::info!(
                actor = %sender,
                wiki = %wiki_id.as_str(),
                target_op_id = op_id,
                compensating_op_id = outcome.compensating_op_id,
                restored_pages = ?outcome.restored_pages,
                "dashboard revert succeeded"
            );
            "revert_ok"
        },
        Err(RevertError::NotFound(_)) => "revert_not_found",
        Err(RevertError::TargetChanged {
            ref conflicting_ops,
            ref conflicting_pages,
        }) => {
            tracing::info!(
                actor = %sender,
                wiki = %wiki_id.as_str(),
                target_op_id = op_id,
                conflicting_ops = ?conflicting_ops,
                conflicting_pages = ?conflicting_pages,
                "dashboard revert refused: target_changed (strict conflict policy)"
            );
            "revert_conflict"
        },
        Err(
            RevertError::NoPreImage { .. }
            | RevertError::NotRevertable { .. }
            | RevertError::MalformedPreImage { .. },
        ) => "revert_not_revertable",
        Err(other) => {
            tracing::error!(
                actor = %sender,
                wiki = %wiki_id.as_str(),
                target_op_id = op_id,
                error = %other,
                "dashboard revert failed unexpectedly"
            );
            "revert_failed"
        },
    };
    Ok(Redirect::to(&format!("{target}?flash={flash}")).into_response())
}

#[derive(Debug)]
struct SharingState {
    wiki_id: String,
    title: String,
    owner_user: String,
    shared_with: Vec<Principal>,
    /// Carried through so a roster edit can refresh the whole registry row
    /// without re-reading `_meta.md`.
    project_id: Option<String>,
    wiki_type: String,
    slug: String,
    /// Carried for the same reason as `project_id`: a roster edit rewrites
    /// the whole registry row, and dropping this would blank the project's
    /// door sign until the next reindex sweep noticed.
    description: Option<String>,
}

fn load_sharing(state: &DashboardState, user: &SessionUser, id: &str) -> Result<SharingState> {
    let memory = require_memory(state)?;
    let wiki_id = WikiId::parse(id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let meta = wiki_get_meta(&memory.tree, &wiki_id).map_err(map_wiki_err)?;
    // Sharing is the WIKI-LEVEL ACL surface, and that axis only exists for
    // smart wikis. A standard wiki governs reads per-fragment (each fact's own
    // owner/allow in `fact_index`); a wiki-level reproject (`submit_sharing` →
    // `reproject_wiki_acl`) would flatten that granularity. Refuse for standard
    // wikis with a 404 — not even discoverable — the inverse of the raw
    // editor's `if meta.smart { NotFound }` guard. Gates both GET and POST,
    // since both flows resolve through here.
    if !meta.smart {
        return Err(DashboardError::NotFound);
    }
    // The owner is derived from topology (the root identity wiki's type).
    // Sharing is owner-only and the UI is keyed on a single user, so a
    // group-owned wiki is refused here.
    let owner_user = match memory
        .tree
        .resolve_scope_principal(&meta)
        .map_err(map_wiki_err)?
    {
        Principal::User(u) => u,
        Principal::Group(g) => {
            return Err(DashboardError::BadRequest(format!(
                "Wiki owner derives to group:{g} — sharing UI is owner-only (user:<id>)."
            )));
        },
    };
    if owner_user != user.sender_id && !user.is_admin {
        return Err(DashboardError::NotFound);
    }
    let shared_with = meta.shared_with.clone();
    let project_id = meta
        .extra
        .get(serde_yaml::Value::from("project_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let description = meta.door_description();
    Ok(SharingState {
        wiki_id: wiki_id.as_str().to_owned(),
        title: meta.title,
        owner_user,
        shared_with,
        description,
        project_id,
        wiki_type: meta.wiki_type,
        slug: meta.slug.as_str().to_owned(),
    })
}

async fn view_sharing(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let sharing = load_sharing(&state, &user, &id)?;
    Ok(Html(render_sharing(chrome, &user, &sharing, None)))
}

#[derive(Debug, Deserialize)]
pub struct SharingSubmission {
    #[serde(default)]
    pub shared_with_raw: String,
}

async fn submit_sharing(
    State(state): State<DashboardState>,
    user: SessionUser,
    AxumPath(id): AxumPath<String>,
    HtmlForm(submission): HtmlForm<SharingSubmission>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let sharing = load_sharing(&state, &user, &id)?;

    let mut new_roster = Vec::new();
    for line in submission.shared_with_raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match Principal::from_str(line) {
            Ok(p) => new_roster.push(p),
            Err(e) => {
                let msg = format!("Cannot parse {line:?} as a principal: {e}");
                let mut sticky = sharing;
                sticky.shared_with =
                    parse_lines(&submission.shared_with_raw).unwrap_or(sticky.shared_with);
                return Ok(Html(render_sharing(chrome, &user, &sticky, Some(&msg))).into_response());
            },
        }
    }

    let memory = require_memory(&state)?;
    let wiki_id = WikiId::parse(&sharing.wiki_id)
        .map_err(|e| DashboardError::Internal(format!("re-parse wiki_id: {e}")))?;
    let roster_len = new_roster.len();
    let owner = update_shared_with(&memory.tree, &wiki_id, new_roster.clone())?;
    // Refresh the wiki's registry row SYNCHRONOUSLY — a revoke must close
    // the recall read-window before this request returns; the periodic
    // safety-net sweep (~5 min) is too slow for an access revocation.
    //
    // This is now a **single-row** write. Read access belongs to the wiki,
    // not to its sections, so there is nothing to re-stamp per section —
    // where a roster change used to rewrite one row per indexed section
    // (over a thousand on a large project wiki), it now touches one.
    mwe_core::sections::upsert_smart_wiki(
        &state.pool,
        &mwe_core::sections::SmartWikiRow {
            wiki_id: wiki_id.as_str().to_owned(),
            slug: sharing.slug.clone(),
            owner_id: owner,
            shared_with: new_roster,
            project_id: sharing.project_id.clone(),
            wiki_type: sharing.wiki_type.clone(),
            description: sharing.description.clone(),
        },
    )
    .await
    .map_err(|e| DashboardError::Internal(format!("refresh smart wiki registry: {e}")))?;

    tracing::info!(
        actor = %user.sender_id,
        wiki = %sharing.wiki_id,
        roster_len,
        "dashboard updated shared_with + refreshed the smart wiki registry row"
    );

    Ok(Redirect::to(&format!("/dashboard/wiki/{}/sharing", sharing.wiki_id)).into_response())
}

fn parse_lines(raw: &str) -> Option<Vec<Principal>> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(Principal::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()
}

/// Round-trip `_meta.md` to replace the `shared_with` roster.
///
/// `WikiMeta::parse` → mutate field → `WikiMeta::render` → `atomic_write`.
/// `WikiMeta::render` preserves the canonical key order, so the on-disk
/// file remains diffable after the round-trip.
/// Round-trip `_meta.md` to replace the `shared_with` roster, returning
/// the wiki's resolved owner principal (the section indexer stamps it onto
/// every row) so the caller can re-project the new ACL onto the wiki's
/// content-indexed fact rows.
fn update_shared_with(
    tree: &mwe_core::wiki::WikiTree,
    wiki_id: &WikiId,
    new_roster: Vec<Principal>,
) -> Result<Principal> {
    let handle = tree.locate(wiki_id).map_err(map_wiki_err)?;
    let meta_path = handle.abs_dir().join(META_FILENAME);
    let raw = std::fs::read_to_string(&meta_path)
        .map_err(|e| DashboardError::Internal(format!("read {}: {e}", meta_path.display())))?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw).map_err(map_wiki_err)?;
    meta.shared_with = new_roster;
    let new_content = meta
        .render(&body)
        .map_err(|e| DashboardError::Internal(format!("re-serialize _meta.md: {e}")))?;
    atomic_write(&meta_path, new_content.as_bytes()).map_err(map_wiki_err)?;
    tree.resolve_scope_principal(&meta).map_err(map_wiki_err)
}

fn render_sharing(
    chrome: layout::Chrome,
    user: &SessionUser,
    sharing: &SharingState,
    error: Option<&str>,
) -> String {
    let roster_text: String = sharing
        .shared_with
        .iter()
        .map(Principal::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        section.meta {
            dl {
                dt { "wiki_id" } dd { code { (sharing.wiki_id) } }
                dt { "title" }   dd { (sharing.title) }
                dt { "owner" }   dd { code { "user:" (sharing.owner_user) } }
            }
        }

        p.muted {
            "The " code { "shared_with" }
            " roster extends read + " code { "wiki_admin_notify" }
            " access to non-owners. " strong { "Writes stay owner-only" }
            " — " code { "wiki_admin_push" } " / " code { "wiki_admin_pull" }
            " from a shared-with caller still raises "
            code { "403 wiki_owned_by_other_user" } " (owner-write invariant preserved). "
            "One principal per line: " code { "user:<id>" } ", "
            code { "group:<id>" } ", or " code { "global" } "."
        }

        @if chrome.read_only {
            (crate::read_only::notice())
            pre { (roster_text) }
        } @else {
            form action=(format!("/dashboard/wiki/{}/sharing", sharing.wiki_id)) method="post" {
                p {
                    label for="shared_with_raw" { "Roster" }
                    textarea id="shared_with_raw" name="shared_with_raw" rows="8" cols="60"
                        placeholder="user:bob\ngroup:lnprint-devs\nglobal" {
                        (roster_text)
                    }
                }
                (components::submit("Save"))
            }
        }

        p {
            a href=(format!("/dashboard/wiki/{}", sharing.wiki_id)) { "← Back to wiki" }
            " · "
            a href="/dashboard/wiki/smart" { "Smart wikis" }
        }
    };

    let title = format!("Sharing — {}", sharing.wiki_id);
    layout::authenticated_page(chrome, &title, user, &body)
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
    use super::parse_lines;
    use mwe_core::types::Principal;

    #[test]
    fn parse_lines_round_trips_a_mixed_roster() {
        let raw = "user:bob\ngroup:lnprint-devs\nglobal";
        let parsed = parse_lines(raw).expect("ok");
        assert_eq!(
            parsed,
            vec![
                Principal::User("bob".into()),
                Principal::Group("lnprint-devs".into()),
                Principal::global(),
            ]
        );
    }

    #[test]
    fn parse_lines_skips_blank_and_whitespace_lines() {
        let raw = "user:alice\n\n   \nglobal\n";
        let parsed = parse_lines(raw).expect("ok");
        assert_eq!(
            parsed,
            vec![Principal::User("alice".into()), Principal::global()]
        );
    }

    #[test]
    fn parse_lines_returns_none_on_invalid_principal() {
        let raw = "user:bob\nnot-a-principal\nglobal";
        assert!(parse_lines(raw).is_none());
    }
}
