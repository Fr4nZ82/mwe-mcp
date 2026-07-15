// SPDX-License-Identifier: AGPL-3.0-or-later
//! Memory-fact browser + structured fact actions + form-to-chat bridge.
//!
//! Routes:
//!
//! - `GET /dashboard/facts` — paginated, filterable list of every fact
//!   the connected user can read (ACL projected via
//!   [`mwe_core::recall::SenderContext`]). The page renders the active
//!   filter form, a result table, and `prev / next` links that preserve
//!   the active filters in the query string. Per-row link "edit"
//!   deep-links to the edit form below.
//! - `GET /dashboard/facts/:fact_id/edit` — edit form for a single fact,
//!   pre-populated from the current `fact_index` row. It carries two
//!   **structured** sub-forms (the per-fragment **ACL** — owner-or-admin —
//!   and **validity** — owner-or-admin — surfaces, engine-direct,
//!   standard-wikis only, born-applied + revertible) plus the **body /
//!   topics / `fact_type`**
//!   supersede, which still rides the **form-to-chat bridge** (the chat
//!   agentic loop takes the supersede through its cascade-aware machinery
//!   under a HARD RULE explicit confirmation).
//! - `POST /dashboard/facts/:fact_id/acl` — structured ACL change. Owner
//!   -or-admin gated, refused on smart wikis (those carry wiki-level ACL,
//!   not per-fragment — see
//!   smart-wikis). Calls
//!   [`mwe_core::operator_edits::acl_change_operator`], posts the
//!   `structure_applied` notice, and 303-redirects to the born-applied
//!   receipt's open-in-chat page so the operator lands on the revertible
//!   receipt.
//! - `POST /dashboard/facts/:fact_id/validity` — structured validity edit
//!   (`valid_from` / `valid_to`). **Owner-or-admin** gated (validity is the
//!   subject's *update* of a fact about themselves — the write-authority
//!   model, [identity and ACL](../../../../docs/concepts/identity-and-acl.md)),
//!   the same owner axis as the ACL action; same standard-wiki gate + paper
//!   trail otherwise, via
//!   [`mwe_core::operator_edits::validity_edit_operator`].
//! - `POST /dashboard/facts/:fact_id/edit/submit` — the form-to-chat
//!   bridge for the **body / topics / `fact_type`** supersede only: runs
//!   a deterministic mapper that turns the delta vs. the original row into
//!   a single textual instruction, runs the agentic loop with that
//!   instruction as the user turn, and primes `window.__mweChatPrimer`
//!   with the resulting trace so a redirect to `/dashboard/chat` lands the
//!   user inside the conversation that already shows the proposed change
//!   waiting for explicit confirmation.
//!
//! ACL + validity left the chat bridge because no chat tool applied them
//! deterministically; they are now structured engine-direct actions whose
//! revert is FREE — the born-applied receipts are `wiki_promote` variants
//! the existing `POST /dashboard/proposals/:id/revert` route already
//! undoes.

use axum::Form;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use mwe_core::acl::can_read;
use mwe_core::capture_buffer::BufferedCapture;
use mwe_core::enrollment;
use mwe_core::events::{self, EventKind};
use mwe_core::fact_index::{self, FactFilters, FactIndexRow, FactSort, FactSortKey};
use mwe_core::operator_edits::{self, OperatorEditError};
use mwe_core::promote::DirectApplied;
use mwe_core::proposals;
use mwe_core::recall::{self, SenderContext};
use mwe_core::types::{Acl, CatalogId, FactId, Principal, WikiId};
use serde::Deserialize;

use crate::auth::SessionUser;
use crate::error::{DashboardError, Result};
use crate::md_render;
use crate::routes::{chat, wiki_view};
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Default rows per page when the query string does not pin `page_size`.
const DEFAULT_PAGE_SIZE: usize = 50;
/// Hard upper bound on `page_size` — protects the in-process pagination
/// path (the handler loads `page * page_size` rows from `wiki_facts_for`
/// then slices, so an attacker-controlled enormous `page_size` would
/// otherwise dominate latency).
const MAX_PAGE_SIZE: usize = 100;
/// Upper bound on the rows scanned per facts-page render. The handler loads the
/// full matching window (ACL-projected in-process), counts the visible rows to
/// derive the real page count, then slices the requested page — that count is
/// what powers "page N of M" and the correct prev/next enablement. This caps
/// the scan so a pathologically large workdir cannot dominate latency; a
/// workdir that exceeds it renders a lower-bound "M+" estimate (see
/// `total_is_estimate` in [`index`]).
const MAX_SCAN_ROWS: usize = 5_000;
/// Body-truncation cap used in the table cell — keeps the row scannable
/// without scrollbars.
const BODY_PREVIEW_CHARS: usize = 120;

/// Mount under the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/facts", get(index))
        .route("/facts/:fact_id/edit", get(edit_form))
        .route("/facts/:fact_id/edit/submit", post(edit_submit))
        .route("/facts/:fact_id/acl", post(acl_submit))
        .route("/facts/:fact_id/validity", post(validity_submit))
        .route("/facts/:fact_id/delete", post(delete_fact))
}

/// Query-string shape for `GET /dashboard/facts`.
///
/// Every field is optional — the empty query string lists every visible
/// fact in `created_at` descending order, capped to the first page. The
/// dashboard preserves the same shape on the pagination links so the
/// filter set survives navigation.
#[derive(Debug, Default, Deserialize)]
pub struct FactsFilters {
    /// Scope to a single `wiki_id`. Empty string treated as unset.
    #[serde(default)]
    pub wiki_id: Option<String>,
    /// Scope to a single `fact_type` tag. Empty string treated as unset.
    #[serde(default)]
    pub fact_type: Option<String>,
    /// ANY-match against a single topic. Multi-topic filtering can ship
    /// later by switching the form input to a comma-separated list and
    /// splitting here — the underlying `FactFilters::topics_any` already
    /// supports it.
    #[serde(default)]
    pub topic: Option<String>,
    /// Inclusive lower bound on `created_at` (ISO 8601). Empty string
    /// treated as unset.
    #[serde(default)]
    pub created_after: Option<String>,
    /// Exclusive upper bound on `created_at` (ISO 8601). Empty string
    /// treated as unset.
    #[serde(default)]
    pub created_before: Option<String>,
    /// 1-based page index. Values below 1 normalise to 1.
    #[serde(default)]
    pub page: Option<usize>,
    /// Rows per page. Defaults to [`DEFAULT_PAGE_SIZE`], capped at
    /// [`MAX_PAGE_SIZE`].
    #[serde(default)]
    pub page_size: Option<usize>,
    /// Sort column token (`created_at`, `recall_count_30d`, …). Parsed via
    /// [`FactSortKey::from_token`]; unknown / empty falls back to the default
    /// `created_at DESC`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Sort direction: `asc` or `desc`. Only meaningful when `sort` is set;
    /// anything other than `asc` reads as descending.
    #[serde(default)]
    pub dir: Option<String>,
    /// `"1"` when the "include inactive" toggle is checked — surfaces
    /// superseded / deleted rows too. Absent (unchecked) keeps the listing
    /// active-only.
    #[serde(default)]
    pub include_inactive: Option<String>,
}

impl FactsFilters {
    /// Coalesce the form-friendly shape into a [`FactFilters`] the
    /// memory engine accepts.
    ///
    /// Empty strings count as "filter not set" — `<input type="text">`
    /// always submits the field, so the query string ends up with
    /// `wiki_id=` etc. on every navigation; treating those as `None`
    /// keeps the SQL clean.
    fn to_core_filters(&self, limit: usize) -> FactFilters {
        FactFilters {
            wiki_id: non_empty(self.wiki_id.as_deref()),
            owner_id: None,
            sender_id: None,
            fact_type: non_empty(self.fact_type.as_deref()),
            created_after: non_empty(self.created_after.as_deref()),
            created_before: non_empty(self.created_before.as_deref()),
            topics_any: non_empty(self.topic.as_deref())
                .map(|t| vec![t])
                .unwrap_or_default(),
            valid_at: None,
            limit,
            sort: self.sort_directive(),
            include_inactive: self.include_inactive(),
        }
    }

    /// The active sort directive, or `None` (engine default `created_at DESC`)
    /// when no recognised column is pinned.
    fn sort_directive(&self) -> Option<FactSort> {
        let key = FactSortKey::from_token(non_empty(self.sort.as_deref())?.as_str())?;
        // Default to descending; only an explicit `asc` flips it.
        let desc = self.dir.as_deref() != Some("asc");
        Some(FactSort { key, desc })
    }

    /// Whether the "include inactive" toggle is on.
    fn include_inactive(&self) -> bool {
        non_empty(self.include_inactive.as_deref()).is_some()
    }

    /// Re-serialise the active filter set as a URL query string with the
    /// given `page` substituted in. Empty fields are skipped so the
    /// emitted URL stays compact.
    fn to_query_string(&self, page: usize, page_size: usize) -> String {
        self.to_query_string_with_sort(page, page_size, self.sort.as_deref(), self.dir.as_deref())
    }

    /// As [`Self::to_query_string`] but with the sort column / direction
    /// overridden — the column-header sort links call this to flip the order
    /// while preserving every other active filter and resetting to page 1.
    fn to_query_string_with_sort(
        &self,
        page: usize,
        page_size: usize,
        sort: Option<&str>,
        dir: Option<&str>,
    ) -> String {
        let include_inactive = if self.include_inactive() {
            Some("1")
        } else {
            None
        };
        let pairs: [(&str, Option<&str>); 8] = [
            ("wiki_id", self.wiki_id.as_deref()),
            ("fact_type", self.fact_type.as_deref()),
            ("topic", self.topic.as_deref()),
            ("created_after", self.created_after.as_deref()),
            ("created_before", self.created_before.as_deref()),
            ("sort", sort),
            ("dir", dir),
            ("include_inactive", include_inactive),
        ];
        let mut parts: Vec<String> = pairs
            .into_iter()
            .filter_map(|(key, value)| {
                let trimmed = value?.trim();
                (!trimmed.is_empty()).then(|| format!("{key}={}", url_encode(trimmed)))
            })
            .collect();
        parts.push(format!("page={page}"));
        parts.push(format!("page_size={page_size}"));
        parts.join("&")
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Resolve the effective (`page`, `page_size`) from the request,
/// applying the documented defaults + cap.
fn normalise_pagination(filters: &FactsFilters) -> (usize, usize) {
    let page = filters.page.filter(|&p| p >= 1).unwrap_or(1);
    let page_size = filters
        .page_size
        .filter(|&s| s >= 1)
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .min(MAX_PAGE_SIZE);
    (page, page_size)
}

/// One displayable row of the facts browser. Unifies a durable
/// [`FactIndexRow`] and an un-promoted [`BufferedCapture`] into a single shape
/// so the table renders both. Fields are pre-stringified for the view; raw
/// timestamps are formatted at render time by [`fmt_ts`]. Carries every column
/// the operator asked for — kept local to the dashboard so the slim
/// [`mwe_core::recall::RecallHit`] recall projection stays unbloated.
struct FactRow {
    fact_id: String,
    wiki_id: String,
    fact_type: Option<String>,
    owner_id: String,
    sender_id: Option<String>,
    allow_ids: Vec<String>,
    topics: Vec<String>,
    salience: Option<String>,
    style: Option<String>,
    page_description: Option<String>,
    body: String,
    valid_from: Option<String>,
    valid_to: Option<String>,
    decay_reason: Option<String>,
    created_at: String,
    updated_at: Option<String>,
    last_recall_at: Option<String>,
    recall_count_30d: Option<i64>,
    source_ref: Option<String>,
    authored_refs: Vec<String>,
    superseded_at: Option<String>,
    superseded_by: Option<String>,
    successor_fact_id: Option<String>,
    deleted_at: Option<String>,
    deleted_reason: Option<String>,
    /// Un-promoted buffer capture (vs a durable `fact_index` fact).
    fresh: bool,
    /// Active = neither superseded nor deleted (always true for a fresh row).
    active: bool,
}

impl FactRow {
    fn from_fact(r: FactIndexRow) -> Self {
        let active = r.superseded_at.is_none() && r.deleted_at.is_none();
        Self {
            fact_id: r.fact_id.as_str().to_owned(),
            wiki_id: r.wiki_id,
            fact_type: r.fact_type,
            owner_id: r.owner_id.to_string(),
            sender_id: r.sender_id.map(|p| p.to_string()),
            allow_ids: r.allow_ids.iter().map(ToString::to_string).collect(),
            topics: r.topics,
            salience: r.salience,
            style: r.style,
            page_description: r.page_description,
            body: r.text,
            valid_from: r.valid_from,
            valid_to: r.valid_to,
            decay_reason: r.decay_reason,
            created_at: r.created_at,
            updated_at: Some(r.updated_at),
            last_recall_at: r.last_recall_at,
            recall_count_30d: Some(r.recall_count_30d),
            source_ref: r.source_ref,
            authored_refs: r.authored_refs,
            superseded_at: r.superseded_at,
            superseded_by: r.superseded_by.map(|f| f.as_str().to_owned()),
            successor_fact_id: r.successor_fact_id.map(|f| f.as_str().to_owned()),
            deleted_at: r.deleted_at,
            deleted_reason: r.deleted_reason,
            fresh: false,
            active,
        }
    }

    fn from_capture(c: BufferedCapture) -> Self {
        Self {
            fact_id: c.capture_id.as_str().to_owned(),
            wiki_id: c.wiki_id.as_str().to_owned(),
            fact_type: c.fact_type,
            owner_id: c.owner.to_string(),
            sender_id: c.sender.map(|p| p.to_string()),
            allow_ids: c.allow.iter().map(ToString::to_string).collect(),
            topics: c.topics,
            salience: c.salience,
            style: c.style,
            page_description: c.page_description,
            body: c.body,
            valid_from: c.valid_from,
            valid_to: c.valid_to,
            decay_reason: c.decay_reason,
            created_at: c.captured_at,
            // A buffered capture has no post-promotion lifecycle yet.
            updated_at: None,
            last_recall_at: None,
            recall_count_30d: None,
            source_ref: c.source_ref,
            authored_refs: c.authored_refs,
            superseded_at: None,
            superseded_by: None,
            successor_fact_id: None,
            deleted_at: None,
            deleted_reason: None,
            fresh: true,
            active: true,
        }
    }
}

/// GET `/dashboard/facts`.
///
/// Renders the filter form + a paginated table of every fact the
/// connected user can read.
async fn index(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    Query(filters): Query<FactsFilters>,
) -> Result<Html<String>> {
    let (page, page_size) = normalise_pagination(&filters);

    // Load the FULL visible window (ACL-projected in-process), count it to
    // derive the real page count, then slice the requested page. Loading
    // everything — rather than `page * page_size` — is what lets the pager show
    // "page N of M" and enable/disable prev/next against a true total instead of
    // a heuristic. The `to_core_filters` cap (`MAX_SCAN_ROWS`) keeps that scan
    // bounded; when it is hit the total is a lower bound (`total_is_estimate`).
    // Pushing offset+limit into SQL is a later optimisation — at the workdir
    // sizes we target this is cheap and keeps the ACL projection in one place.
    let core_filters = filters.to_core_filters(MAX_SCAN_ROWS);
    let sender_groups = enrollment::groups_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("enrollment::groups_for: {e}")))?;
    let sender_ctx = SenderContext {
        sender_id: user.sender_id.clone(),
        sender_groups,
    };
    // Admin reveal lens: when on, both fetches skip the per-row ACL gate so
    // the table lists every user's facts and the owner-or-admin actions
    // (ACL / validity / delete) become reachable on them. Gated on
    // `is_admin` inside `reveal::active`, so a non-admin can never trip it.
    let reveal = crate::reveal::active(&user, &jar);

    // The facts table reads `fact_index` (promoted facts). A freshly-ingested
    // claim sits un-promoted in `capture_buffer` until the light dream
    // consolidates it — invisible here, yet already recalled by the agent via
    // the "fresh" slot. Surface those captures at the top, badged, so the
    // operator view does not silently lag the agent's knowledge. Both calls
    // pull the FULL row (every column), ACL-filtered, honouring the same
    // filters incl. `sort` / `include_inactive`.
    let fresh = recall::wiki_buffered_full_for(&state.pool, &core_filters, &sender_ctx, reveal)
        .await
        .map_err(|e| DashboardError::Internal(format!("wiki_buffered_full_for: {e}")))?;
    let promoted = recall::wiki_facts_full_for(&state.pool, &core_filters, &sender_ctx, reveal)
        .await
        .map_err(|e| DashboardError::Internal(format!("wiki_facts_full_for: {e}")))?;

    // When the promoted scan comes back full, more rows may exist beyond the
    // cap — the total (hence the last page) is then a lower bound.
    let total_is_estimate = promoted.len() >= MAX_SCAN_ROWS;

    // Fresh captures lead; promoted facts follow. Both are already ACL-filtered
    // and honour the active filters incl. `sort` / `include_inactive`; the fresh
    // prefix is small and capped, so it rides at the head of the first page.
    let mut rows: Vec<FactRow> = fresh.into_iter().map(FactRow::from_capture).collect();
    rows.extend(promoted.into_iter().map(FactRow::from_fact));

    // Real page count from the visible total (at least one page, even when the
    // set is empty). Clamp the requested page into range so a stale or
    // hand-typed `page=` lands on the last page rather than an empty slice.
    let total = rows.len();
    let total_pages = total.div_ceil(page_size).max(1);
    let page = page.min(total_pages);
    let start = (page - 1).saturating_mul(page_size).min(total);
    let end = (start + page_size).min(total);
    let page_rows = &rows[start..end];

    tracing::debug!(
        sender_id = %user.sender_id,
        page,
        page_size,
        total,
        total_pages,
        total_is_estimate,
        "dashboard: /facts loaded ACL-filtered window"
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
    )))
}

/// GET `/dashboard/facts/:fact_id/edit`.
///
/// Pre-populates the deterministic edit form from the current
/// `fact_index` row, gated by the same ACL projection
/// [`recall::wiki_facts_for`] applies — a sender that cannot read the
/// row gets `404 Not Found` rather than a form they could not submit
/// (the chat would refuse the supersede anyway because the recall step
/// would not find the fact).
///
/// The form action targets the submit handler below; on submit the
/// bridge composes the textual instruction, runs the agentic loop, and
/// primes the chat panel — see [`edit_submit`] for the mapper logic
/// and the redirect contract.
async fn edit_form(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(fact_id_raw): AxumPath<String>,
) -> Result<Html<String>> {
    let fact_id =
        FactId::parse(&fact_id_raw).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_visible_fact(&state, &user, &fact_id, reveal).await?;
    // Both structured forms gate on the **owner** axis (the write-authority
    // model — docs/concepts/identity-and-acl.md): ACL (visibility) is the
    // subject's privacy call, and
    // validity (an *update* of the fact, not a destruction) is likewise the
    // subject's act. Only `delete` keys on `sender` / a vote.
    let can_acl = owner_or_admin(&user, &row);
    let can_validity = owner_or_admin(&user, &row);
    let is_smart = wiki_is_smart(&state, &row.wiki_id);
    tracing::info!(
        sender_id = %user.sender_id,
        fact_id = %fact_id,
        "dashboard: /facts/.../edit form rendered"
    );
    Ok(Html(render_edit_form(
        &state,
        &user,
        &fact_id,
        &row,
        can_acl,
        can_validity,
        is_smart,
        None,
    )))
}

/// The owner-or-admin predicate — the gate for the subject's acts on a fact
/// about themselves: **`acl_change`** (visibility) and **`validity_edit`** (an
/// *update* of the fact, not a destruction). Both are the owner's call (the
/// write-authority model —
/// [identity and ACL](../../../../docs/concepts/identity-and-acl.md)).
/// User-owner only (a group-owned fact's
/// member updates it via ingest / admin), matching `acl_submit`.
fn owner_or_admin(user: &SessionUser, row: &FactIndexRow) -> bool {
    row.owner_id == Principal::User(user.sender_id.clone()) || user.is_admin
}

/// The sender-or-admin predicate — the **`delete`** (author-direct) gate
/// (the write-authority model —
/// [identity and ACL](../../../../docs/concepts/identity-and-acl.md)):
/// only the fact's `sender` (its author) **destroys** their
/// own contribution directly; an admin may delete any fact. A non-sender owner's
/// path is a request → vote, opened from the dashboard. *Updates* (edit /
/// validity) are the owner's, not the sender's — see [`owner_or_admin`].
/// Delegates to [`mwe_core::acl::can_delete`] so the policy stays in one place.
fn sender_or_admin(user: &SessionUser, row: &FactIndexRow) -> bool {
    mwe_core::acl::can_delete(row.sender_id.as_ref(), &user.sender_id, user.is_admin)
}

/// Best-effort: is the fact's wiki smart? Used only to decide whether the
/// form renders the structured ACL / validity sub-forms (the authoritative
/// refusal lives in [`enforce_standard_wiki`] on the POST path). A wiki
/// that cannot be located — or memory handles not wired — reads as
/// non-smart so the form still offers the actions; the POST then surfaces
/// the precise error.
fn wiki_is_smart(state: &DashboardState, wiki_id: &str) -> bool {
    let Some(memory) = state.memory.as_ref() else {
        return false;
    };
    let Ok(parsed) = WikiId::parse(wiki_id) else {
        return false;
    };
    memory.tree.locate(&parsed).is_ok_and(|h| h.meta().smart)
}

/// Form payload accepted by `POST /dashboard/facts/:fact_id/edit/submit`.
///
/// The supersede surfaces that still ride the form-to-chat bridge:
/// `topics`, `fact_type`, and `body`. ACL (owner + allow) and validity
/// moved to the structured engine-direct actions ([`acl_submit`] /
/// [`validity_submit`]), so they are NOT here. Every field is trimmed
/// before the mapper compares it to the original row, so whitespace-only
/// edits do not trip a "changed" branch.
#[derive(Debug, Default, Deserialize)]
pub struct EditFactForm {
    /// Comma-separated topic list. Empty means "do not change"; an
    /// explicit non-empty value replaces the current topic list.
    #[serde(default)]
    pub topics: String,
    /// Optional `fact_type` taxonomy hint (`bio`, `preference`, …) or
    /// the literal `clear` to remove the hint. Empty means "do not
    /// change".
    #[serde(default)]
    pub fact_type: String,
    /// Replacement body text. Empty means "do not change". The body is
    /// fenced inside the composed message so the chat's prompt parser
    /// sees it as a single block, not as new instructions to interpret.
    #[serde(default)]
    pub body: String,
}

/// POST `/dashboard/facts/:fact_id/edit/submit`.
///
/// Composes the textual instruction from the form delta (see
/// [`compose_edit_message`]), runs it through the dashboard's agentic
/// loop (same chokepoint as a chat panel submit — `hub_writer` slot +
/// whitelisted `_internal.*` tool registry + HARD RULE explicit
/// confirmation), packages the resulting turn into a primer for the
/// chat panel, and redirects the browser to `/dashboard/chat` with the
/// primer payload embedded inline. The chat panel's `chat.js` reads
/// `window.__mweChatPrimer` on hydrate, splices the turn into the
/// scrollback, and persists it to `localStorage` — so the user lands
/// on the chat page already seeing both the composed instruction and
/// the model's first response asking for explicit confirmation.
///
/// When the form delta is empty (no field actually changed), the
/// handler refuses with a `422 Unprocessable Entity` carrying an
/// inline flash — submitting an unchanged form is a UX accident, not a
/// design path, and the chat loop would just produce a confused turn.
async fn edit_submit(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(fact_id_raw): AxumPath<String>,
    Form(form): Form<EditFactForm>,
) -> Result<Html<String>> {
    let fact_id =
        FactId::parse(&fact_id_raw).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_visible_fact(&state, &user, &fact_id, reveal).await?;
    let delta = form_to_delta(&form);
    let Some(message) = compose_edit_message(&fact_id, &row, &delta) else {
        // Nothing changed — re-render the form with an inline flash so
        // the user understands why the submit didn't go anywhere.
        let can_acl = owner_or_admin(&user, &row);
        let can_validity = owner_or_admin(&user, &row);
        let is_smart = wiki_is_smart(&state, &row.wiki_id);
        return Ok(Html(render_edit_form(
            &state,
            &user,
            &fact_id,
            &row,
            can_acl,
            can_validity,
            is_smart,
            Some("No change detected — edit at least one field before submitting."),
        )));
    };

    tracing::info!(
        sender_id = %user.sender_id,
        fact_id = %fact_id,
        "dashboard: /facts/.../edit/submit composing form-to-chat primer"
    );
    // Form-to-chat primer: a single composed instruction, no replay window.
    let turn = chat::agentic_submission(&state, &user, &message, &[], reveal).await?;
    let payload = serde_json::json!({
        "user_text": turn.user_text,
        "trace": turn.trace,
        "final_message": turn.final_message,
        "final_message_html": turn.final_message_html,
        "iterations": turn.iterations,
        "budget_exhausted": turn.budget_exhausted,
        "ts": chrono::Utc::now().timestamp_millis(),
    });
    let payload_js = serde_json::to_string(&payload).unwrap_or_else(|_| "null".into());
    let body = html! {
        h2 { "Fact edit " code { (fact_id.as_str()) } " opened in chat" }
        p {
            "The composed message and the agentic chat's reply are in the panel "
            "on the right. Confirm explicitly to apply the edit, or go back to "
            a href="/dashboard/facts" { "Facts" }
            " to discard it."
        }
        script {
            (PreEscaped(format!(
                "window.__mweChatPrimer = {payload_js};"
            )))
        }
    };
    Ok(Html(layout::authenticated_page(
        "Fact edit in chat",
        &user,
        &body,
    )))
}

// ---------- structured engine-direct fact actions (ACL + validity) ----------

/// Form body of `POST /dashboard/facts/:fact_id/acl`.
#[derive(Debug, Default, Deserialize)]
pub struct AclActionForm {
    /// New owner principal as a Display string (`global` / `user:…` /
    /// `group:…`). Required — a structured ACL change always names the
    /// owner (the form pre-fills the current value).
    #[serde(default)]
    pub owner: String,
    /// Comma-separated `allow=…` principals. Empty clears the allow set.
    #[serde(default)]
    pub allow: String,
}

/// Form body of `POST /dashboard/facts/:fact_id/validity`.
#[derive(Debug, Default, Deserialize)]
pub struct ValidityActionForm {
    /// `valid_from` bound as an `<input type="date">`/ISO value. Empty
    /// leaves the bound unchanged.
    #[serde(default)]
    pub valid_from: String,
    /// `valid_to` bound. Empty leaves the bound unchanged.
    #[serde(default)]
    pub valid_to: String,
}

/// `POST /dashboard/facts/:fact_id/acl` — structured, engine-direct
/// per-fragment ACL change.
///
/// Gated **owner-OR-admin** + **standard-wikis only** (smart wikis carry
/// wiki-level ACL, not per-fragment — see
/// smart-wikis). Calls the
/// act-first wrapper, posts the `structure_applied` notice mirroring the
/// chat paper-trail, and 303-redirects to the born-applied receipt's
/// open-in-chat page so the operator lands on the revertible receipt.
async fn acl_submit(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(fact_id_raw): AxumPath<String>,
    Form(form): Form<AclActionForm>,
) -> Result<Response> {
    let fact_id =
        FactId::parse(&fact_id_raw).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_visible_fact(&state, &user, &fact_id, reveal).await?;
    enforce_owner_or_admin(&user, &row)?;
    enforce_standard_wiki(&state, &row.wiki_id)?;

    let new_owner = form
        .owner
        .trim()
        .parse::<Principal>()
        .map_err(|e| DashboardError::Validation(format!("owner non valido: {e}")))?;
    let new_allow = split_csv(&form.allow)
        .iter()
        .map(|s| s.parse::<Principal>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| DashboardError::Validation(format!("allow non valido: {e}")))?;

    // Preserve the fact's cross-user attribution (who captured it) — a
    // re-share never rewrites it, exactly like the chat verb.
    let keep_sender = row.sender_id.clone();
    let preview = components::truncate_chars(&row.text, 120);
    let recipient = proposals::recipient_from_fact(&row.owner_id, row.sender_id.as_ref());

    let applied = operator_edits::acl_change_operator(
        &state.pool,
        &fact_id,
        &row.wiki_id,
        &new_owner,
        &new_allow,
        keep_sender.as_ref(),
        &preview,
        &user.sender_id,
        recipient.clone(),
    )
    .await
    .map_err(|e| map_operator_edit_err(&e))?;

    let widening = mwe_core::acl::widens(&row.owner_id, &row.allow_ids, &new_owner, &new_allow);
    emit_structure_applied(
        &state,
        "acl_change",
        &row.wiki_id,
        &fact_id,
        &applied,
        recipient,
        serde_json::json!({
            "changed_facts": [fact_id.as_str()],
            "widening": widening,
        }),
    )
    .await;

    tracing::info!(
        sender_id = %user.sender_id,
        fact_id = %fact_id,
        proposal_id = %applied.proposal_id,
        widening,
        "dashboard: structured ACL change applied"
    );
    Ok(redirect_to_receipt(&applied.proposal_id))
}

/// `POST /dashboard/facts/:fact_id/validity` — structured, engine-direct
/// per-fragment validity edit. **Owner-or-admin** gated (validity is the
/// subject's *update* of a fact about themselves — the write-authority model,
/// [identity and ACL](../../../../docs/concepts/identity-and-acl.md)), the
/// same owner axis as [`acl_submit`]; same standard-wiki gate + paper trail.
async fn validity_submit(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(fact_id_raw): AxumPath<String>,
    Form(form): Form<ValidityActionForm>,
) -> Result<Response> {
    let fact_id =
        FactId::parse(&fact_id_raw).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_visible_fact(&state, &user, &fact_id, reveal).await?;
    // Validity is an *update* of the fact (not a destruction): the owner's act
    // (the write-authority model), the same owner axis as ACL.
    enforce_owner_or_admin(&user, &row)?;
    enforce_standard_wiki(&state, &row.wiki_id)?;

    let valid_from = normalize_date_bound(&form.valid_from)?;
    let valid_to = normalize_date_bound(&form.valid_to)?;
    if valid_from.is_none() && valid_to.is_none() {
        return Err(DashboardError::Validation(
            "Specifica almeno una delle due date (valid_from / valid_to).".to_owned(),
        ));
    }

    let preview = components::truncate_chars(&row.text, 120);
    let recipient = proposals::recipient_from_fact(&row.owner_id, row.sender_id.as_ref());

    let applied = operator_edits::validity_edit_operator(
        &state.pool,
        &fact_id,
        &row.wiki_id,
        valid_from.as_deref(),
        valid_to.as_deref(),
        &preview,
        &user.sender_id,
        recipient.clone(),
    )
    .await
    .map_err(|e| map_operator_edit_err(&e))?;

    emit_structure_applied(
        &state,
        "validity_edit",
        &row.wiki_id,
        &fact_id,
        &applied,
        recipient,
        serde_json::json!({ "edited_facts": [fact_id.as_str()] }),
    )
    .await;

    tracing::info!(
        sender_id = %user.sender_id,
        fact_id = %fact_id,
        proposal_id = %applied.proposal_id,
        "dashboard: structured validity edit applied"
    );
    Ok(redirect_to_receipt(&applied.proposal_id))
}

/// POST `/dashboard/facts/:fact_id/delete` — tombstone a single promoted
/// fact ([`mwe_core::capture::wiki_forget`]: the `deleted_at` tombstone
/// plus the best-effort excision of the region's on-disk bytes).
///
/// **Sender-or-admin** gated (the write-authority model): only the fact's author
/// forgets it directly; a non-sender's path is a request → vote. The
/// soft-delete flips `deleted_at` so the fact leaves recall at once and
/// survives as an audit tombstone (visible under the "include inactive"
/// filter). The button only appears on active promoted facts — a fresh
/// capture has no `fact_index` row (404 here), and an already-tombstoned
/// row is a no-op (the tombstone guards on `deleted_at IS NULL`).
/// Redirects 303 back to the listing.
async fn delete_fact(
    State(state): State<DashboardState>,
    user: SessionUser,
    jar: CookieJar,
    AxumPath(fact_id_raw): AxumPath<String>,
) -> Result<Response> {
    let fact_id =
        FactId::parse(&fact_id_raw).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let reveal = crate::reveal::active(&user, &jar);
    let row = load_visible_fact(&state, &user, &fact_id, reveal).await?;
    enforce_sender_or_admin(&user, &row)?;
    let tombstoned = if let Some(memory) = state.memory.as_ref() {
        mwe_core::capture::wiki_forget(
            &memory.tree,
            &state.pool,
            memory.embedder.clone(),
            &fact_id,
            "dashboard_delete",
        )
        .await
        .map_err(|e| DashboardError::Internal(format!("wiki_forget: {e}")))?
        .tombstoned
    } else {
        // No memory handles (dashboard without a served tree): the DB half
        // still applies; the on-disk residue rides the hygiene sweep.
        fact_index::mark_forgotten(&state.pool, &fact_id, "dashboard_delete")
            .await
            .map_err(|e| DashboardError::Internal(format!("mark_forgotten: {e}")))?
            > 0
    };
    tracing::info!(
        sender_id = %user.sender_id,
        fact_id = %fact_id,
        tombstoned,
        "dashboard: fact tombstoned via delete button"
    );
    Ok(Redirect::to("/dashboard/facts").into_response())
}

/// Enforce the **owner-OR-admin** gate on a structured fact action: the
/// session must own the fact (`owner_id == user:<sender_id>`) or be admin.
/// 403 otherwise.
fn enforce_owner_or_admin(user: &SessionUser, row: &FactIndexRow) -> Result<()> {
    if owner_or_admin(user, row) {
        Ok(())
    } else {
        Err(DashboardError::Forbidden)
    }
}

/// Enforce the **sender-OR-admin** gate on a direct fact act (`delete` /
/// `validity_edit`): the session must be the fact's author
/// (`sender_id == user:<sender_id>`) or be admin. 403 otherwise.
fn enforce_sender_or_admin(user: &SessionUser, row: &FactIndexRow) -> Result<()> {
    if sender_or_admin(user, row) {
        Ok(())
    } else {
        Err(DashboardError::Forbidden)
    }
}

/// Enforce the **standard-wikis-only** gate: refuse with a clear message
/// when the fact's wiki is smart. Smart wikis carry no per-fragment ACL /
/// validity — those are wiki-level (see smart-wikis.md). A wiki that can no
/// longer be located is a `404` (the row points at a vanished wiki).
fn enforce_standard_wiki(state: &DashboardState, wiki_id: &str) -> Result<()> {
    let memory = state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })?;
    let parsed = WikiId::parse(wiki_id).map_err(|e| DashboardError::BadRequest(format!("{e}")))?;
    let handle = memory
        .tree
        .locate(&parsed)
        .map_err(|_| DashboardError::NotFound)?;
    if handle.meta().smart {
        return Err(DashboardError::Validation(
            "Smart wikis carry no per-fragment ACL / validity: their \
             governance is wiki-level. Use the smart wiki's sharing page \
             or the smart consumer's own channels."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Post the act-first `structure_applied` notice for a born-applied
/// receipt minted from the dashboard — mirroring the chat paper-trail
/// payload so the dashboard receipt surface and the in-flight badge read
/// the same shape. Best-effort: the change already stands.
async fn emit_structure_applied(
    state: &DashboardState,
    variant: &str,
    wiki_id: &str,
    fact_id: &FactId,
    applied: &DirectApplied,
    recipient: Option<String>,
    extra: serde_json::Value,
) {
    let mut payload = serde_json::json!({
        "proposal_id": applied.proposal_id,
        "variant": variant,
        "recipient_id": recipient,
        "revert_deadline": applied.revert_deadline.to_rfc3339(),
        "dashboard_path": format!("/dashboard/proposals/{}/open-in-chat", applied.proposal_id),
    });
    if let (Some(obj), serde_json::Value::Object(extra_obj)) = (payload.as_object_mut(), extra) {
        obj.extend(extra_obj);
    }
    if let Err(err) = events::insert_event(
        &state.pool,
        EventKind::StructureApplied,
        Some(wiki_id),
        Some(fact_id.as_str()),
        &payload,
    )
    .await
    {
        tracing::warn!(error = %err, %variant, "dashboard: structure_applied notice event failed");
    }
}

/// 303-redirect to the born-applied receipt's open-in-chat page so the
/// operator lands on the revertible receipt.
fn redirect_to_receipt(proposal_id: &str) -> Response {
    Redirect::to(&format!("/dashboard/proposals/{proposal_id}/open-in-chat")).into_response()
}

/// Trim a date bound and treat empty as absent. Accepts a bare
/// `YYYY-MM-DD` (from `<input type="date">`) by promoting it to midnight
/// UTC, or a full RFC3339 timestamp; rejects anything else.
fn normalize_date_bound(raw: &str) -> Result<Option<String>> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    // Full RFC3339 is accepted verbatim.
    if chrono::DateTime::parse_from_rfc3339(t).is_ok() {
        return Ok(Some(t.to_owned()));
    }
    // `<input type="date">` yields `YYYY-MM-DD` — promote to midnight UTC.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always valid")
            .and_utc();
        return Ok(Some(dt.to_rfc3339()));
    }
    Err(DashboardError::Validation(format!(
        "Data non valida: {t:?} (usa YYYY-MM-DD oppure un timestamp ISO 8601)."
    )))
}

/// Map an [`OperatorEditError`] onto a dashboard error. A vanished target
/// is a `404` (the fact disappeared between the form render and the
/// submit); a surface or receipt failure is an internal error — for the
/// receipt case the change already stands, so the operator should refresh
/// rather than retry.
fn map_operator_edit_err(err: &OperatorEditError) -> DashboardError {
    match err {
        OperatorEditError::FactVanished(_) => DashboardError::NotFound,
        OperatorEditError::Direct(_)
        | OperatorEditError::FactSurface(_)
        | OperatorEditError::BufferSurface(_) => DashboardError::Internal(err.to_string()),
    }
}

/// Look up the fact row referenced by the URL, refusing with `404` when
/// the row is absent or the connected user cannot read it under the
/// same ACL the recall layer would apply. Centralised here so the
/// `edit_form` GET and the `edit_submit` POST agree on the same
/// projection.
async fn load_visible_fact(
    state: &DashboardState,
    user: &SessionUser,
    fact_id: &FactId,
    reveal: bool,
) -> Result<FactIndexRow> {
    let row = fact_index::find_by_id(&state.pool, fact_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("fact_index::find_by_id: {e}")))?
        .ok_or(DashboardError::NotFound)?;

    // Admin reveal bypasses the read projection so the structured fact
    // actions reach another user's facts. The per-action authority gate
    // (`enforce_owner_or_admin` for ACL, `enforce_sender_or_admin` for
    // delete / validity) remains the authority on the write itself.
    if reveal {
        return Ok(row);
    }

    let sender_groups = enrollment::groups_for(&state.pool, &user.sender_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("enrollment::groups_for: {e}")))?;
    let sender_ctx = SenderContext {
        sender_id: user.sender_id.clone(),
        sender_groups,
    };
    if !fact_visible_to(&row, &sender_ctx) {
        return Err(DashboardError::NotFound);
    }
    Ok(row)
}

/// Three-state delta for the optional `fact_type` field.
///
/// The form has to distinguish three intents that a single
/// `Option<String>` cannot express without ambiguity:
///
/// - `Untouched` — the user left the input blank (the same as every
///   other "do not change" field), so the chat must keep whatever
///   value is on the row today;
/// - `Clear` — the user typed the literal sentinel `clear` to drop
///   the existing `fact_type` hint (it has no inline replacement);
/// - `Set(value)` — the user typed a new taxonomy hint to replace
///   whatever is on the row today.
///
/// The mapper turns each variant into a distinct fragment of the
/// composed instruction so the chat sees exactly what the operator
/// asked for, never "missing field" inferred from `None`.
#[derive(Debug, Default, PartialEq, Eq)]
enum FactTypeDelta {
    #[default]
    Untouched,
    Clear,
    Set(String),
}

/// Parsed, trimmed view of the supersede surfaces of the form (the ones
/// still on the form-to-chat bridge: `topics`, `fact_type`, `body`). ACL
/// and validity left the bridge for the structured engine-direct actions.
///
/// Each variant means "the user actually changed this field" — a
/// missing variant means "leave it untouched", matching the mapper's
/// three macro-cases below (metadata-only, body-only, multiple).
#[derive(Debug, Default, PartialEq, Eq)]
struct EditDelta {
    /// Replacement topic list, comma-split + trimmed. `None` means
    /// untouched; `Some(Vec::new())` means "clear the topic list".
    topics: Option<Vec<String>>,
    /// `fact_type` taxonomy delta — see [`FactTypeDelta`] for the
    /// three semantics (untouched / clear / set).
    fact_type: FactTypeDelta,
    /// Replacement body. `None` means untouched.
    body: Option<String>,
}

impl EditDelta {
    /// True when at least one field of the form actually changed.
    /// Drives the "nothing to do, refuse the submit" branch above.
    const fn is_empty(&self) -> bool {
        self.topics.is_none()
            && matches!(self.fact_type, FactTypeDelta::Untouched)
            && self.body.is_none()
    }

    /// True when only metadata fields (topics, `fact_type`) changed —
    /// drives the first macro-case of [`compose_edit_message`].
    const fn metadata_only(&self) -> bool {
        self.body.is_none() && !self.is_empty()
    }

    /// True when only the body changed — drives the second macro-case.
    const fn body_only(&self) -> bool {
        self.body.is_some()
            && self.topics.is_none()
            && matches!(self.fact_type, FactTypeDelta::Untouched)
    }
}

/// Translate the raw form payload into a normalised [`EditDelta`].
///
/// The form fields are trimmed once here; the mapper below only sees
/// post-trim values. `topics=`, `fact_type=`, `body=` empty strings are
/// treated as "field not touched" — the HTML form always submits every
/// field even when the user did not edit them, so we cannot tell "left
/// blank" apart from "untouched" without comparing to the original row.
/// The single exception is `fact_type=clear` which is the explicit signal
/// to drop the hint.
fn form_to_delta(form: &EditFactForm) -> EditDelta {
    let topics = if form.topics.trim().is_empty() {
        None
    } else {
        Some(split_csv(&form.topics))
    };
    let fact_type = match form.fact_type.trim() {
        "" => FactTypeDelta::Untouched,
        "clear" => FactTypeDelta::Clear,
        other => FactTypeDelta::Set(other.to_owned()),
    };
    let body = trimmed_some(&form.body);
    EditDelta {
        topics,
        fact_type,
        body,
    }
}

fn trimmed_some(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Deterministic mapper from the form delta to the textual instruction
/// the agentic chat panel receives.
///
/// Three macro-cases per [the memory model](../../../../docs/concepts/memory-model.md):
///
/// 1. **Metadata-only change** (topics / `fact_type`, no body): a single
///    sentence enumerating the new values. Drives
///    `wiki_supersede` keeping the body intact.
/// 2. **Body-only change**: instruction wrapping the new body inside a
///    fenced code block so the chat parser sees it as one literal,
///    not as more directives. The metadata stays the same.
/// 3. **Multiple changes**: a single sentence enumerating the metadata
///    deltas *and* the fenced body block — one supersede with the
///    full new shape.
///
/// ACL and validity are not here — they are structured engine-direct
/// actions ([`acl_submit`] / [`validity_submit`]), not chat-bridge text.
///
/// Returns `None` when [`EditDelta::is_empty`] — the caller refuses
/// the submit instead of running a confused chat turn.
fn compose_edit_message(fact_id: &FactId, row: &FactIndexRow, delta: &EditDelta) -> Option<String> {
    if delta.is_empty() {
        return None;
    }
    let header = format!("Edit fact `{}` (wiki `{}`)", fact_id.as_str(), row.wiki_id);
    if delta.metadata_only() {
        let metadata = format_metadata_changes(row, delta);
        return Some(format!("{header}: {metadata}."));
    }
    if delta.body_only() {
        let body_block = delta.body.as_deref().unwrap_or("");
        return Some(format!(
            "{header}: change the body to:\n```\n{body_block}\n```"
        ));
    }
    // Mixed change: metadata sentence + fenced body block.
    let metadata = format_metadata_changes(row, delta);
    let body_block = delta.body.as_deref().unwrap_or("");
    Some(format!(
        "{header}: {metadata}, and change the body to:\n```\n{body_block}\n```"
    ))
}

/// Render the metadata segment of the composed message — used both in
/// the metadata-only branch and as the prefix in the mixed branch.
///
/// Skips the body delta on purpose; the caller is responsible for
/// appending the fenced block when needed.
fn format_metadata_changes(row: &FactIndexRow, delta: &EditDelta) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(topics) = &delta.topics {
        let formatted = if topics.is_empty() {
            "empty".to_owned()
        } else {
            format!("[{}]", topics.join(", "))
        };
        let current = if row.topics.is_empty() {
            "empty".to_owned()
        } else {
            format!("[{}]", row.topics.join(", "))
        };
        parts.push(format!("set `topics` from {current} to {formatted}"));
    }
    match &delta.fact_type {
        FactTypeDelta::Untouched => {},
        FactTypeDelta::Clear => {
            let current = row.fact_type.as_deref().unwrap_or("none");
            parts.push(format!("remove `fact_type` (was `{current}`)"));
        },
        FactTypeDelta::Set(new_ft) => {
            let current = row.fact_type.as_deref().unwrap_or("none");
            parts.push(format!("set `fact_type` from `{current}` to `{new_ft}`"));
        },
    }
    parts.join(", ")
}

/// ACL projection mirror of [`mwe_core::recall::wiki_facts_for`].
/// Returns `true` when the sender can read the row under the same rules
/// the recall layer applies internally — delegates to
/// [`mwe_core::acl::can_read`] so the policy stays in one place.
fn fact_visible_to(row: &FactIndexRow, sender: &SenderContext) -> bool {
    let acl = Acl {
        owner: Some(row.owner_id.clone()),
        allow: row.allow_ids.clone(),
    };
    can_read(
        &acl,
        &sender.sender_id,
        &sender.sender_groups,
        row.sender_id.as_ref(),
    )
}

/// Format an ISO-8601 timestamp for the table: drop fractional seconds and
/// the timezone suffix, and use a space instead of the `T` separator
/// (`2026-06-23T16:32:45.947+00:00` → `2026-06-23 16:32:45`). All stored
/// stamps are UTC, so dropping the offset is lossless for display.
fn fmt_ts(raw: &str) -> String {
    let spaced = raw.replacen('T', " ", 1);
    match spaced.get(..19) {
        // "YYYY-MM-DD HH:MM:SS" — byte 10 is the date/time space.
        Some(s) if s.as_bytes().get(10) == Some(&b' ') => s.to_owned(),
        _ => spaced
            .trim_end_matches('Z')
            .trim_end_matches("+00:00")
            .trim_end()
            .to_owned(),
    }
}

/// A plain text cell, or a muted `—` when empty.
fn opt_cell(value: Option<&str>) -> Markup {
    match value {
        Some(s) if !s.is_empty() => html! { (s) },
        _ => html! { span.muted { "—" } },
    }
}

/// A formatted-timestamp cell, or a muted `—` when absent.
fn ts_cell(value: Option<&str>) -> Markup {
    match value {
        Some(s) if !s.is_empty() => html! { (fmt_ts(s)) },
        _ => html! { span.muted { "—" } },
    }
}

/// A comma-joined list cell, or a muted `—` when empty.
fn list_cell(items: &[String]) -> Markup {
    if items.is_empty() {
        html! { span.muted { "—" } }
    } else {
        html! { (items.join(", ")) }
    }
}

/// The `fact_id` cell: shows the first 8 chars, copies the full id on click
/// (handled by `ui.js` via the `data-fact-id` hook).
fn id_cell(id: &str) -> Markup {
    let short = id.get(..8).unwrap_or(id);
    html! {
        code.copy-id data-fact-id=(id) title=(format!("{id} · clicca per copiare l'id intero")) {
            (short) "…"
        }
    }
}

/// The lifecycle badge cell.
fn status_cell(row: &FactRow) -> Markup {
    html! {
        @if row.fresh {
            span.badge.badge-fresh
                title="Capture not yet promoted to a fact: the light dream will consolidate it shortly."
                { "consolidating" }
        } @else if row.deleted_at.is_some() {
            span.badge.badge-deleted title="Deleted fact (tombstone)" { "deleted" }
        } @else if row.superseded_at.is_some() {
            span.badge.badge-superseded title="Fact superseded by a revision" { "superseded" }
        } @else {
            span.muted { "active" }
        }
    }
}

/// A sortable column header: a link that pins this column as the sort key,
/// flipping direction when it is already active and showing a ↑/↓ marker.
/// Sorting always resets to page 1.
fn sort_header(filters: &FactsFilters, page_size: usize, token: &str, label: &str) -> Markup {
    let active = filters.sort.as_deref().map(str::trim) == Some(token);
    let active_desc = active && filters.dir.as_deref() != Some("asc");
    // Clicking an inactive column starts descending; an active one flips.
    let next_dir = if active && active_desc { "asc" } else { "desc" };
    let href = format!(
        "/dashboard/facts?{}",
        filters.to_query_string_with_sort(1, page_size, Some(token), Some(next_dir))
    );
    let arrow = if active {
        if active_desc { " ↓" } else { " ↑" }
    } else {
        ""
    };
    html! {
        th { a.sort-link href=(href) { (label) (arrow) } }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_index(
    user: &SessionUser,
    filters: &FactsFilters,
    page: usize,
    page_size: usize,
    page_rows: &[FactRow],
    total: usize,
    total_pages: usize,
    total_is_estimate: bool,
    reveal: bool,
) -> String {
    let body = html! {
        // Under reveal the list is no longer "facts you can read" — it is
        // every user's facts — so the banner replaces the default intro.
        @if reveal {
            (crate::reveal::banner())
        } @else {
            p.muted {
                "Filtered list of every fact you can read. Filters compose in AND, "
                "and " code { "topic" } " takes a single term (multi-topic upcoming). "
                "Arrowed headers sort; click a " code { "fact_id" } " to copy it."
            }
        }

        (filter_form(filters, page_size))

        @if page_rows.is_empty() {
            p.muted { "No facts to show." }
        } @else {
            table.facts-table.compact {
                thead { tr {
                    th { "actions" }
                    th { "fact_id" }
                    th { "status" }
                    (sort_header(filters, page_size, "wiki_id", "wiki_id"))
                    (sort_header(filters, page_size, "fact_type", "fact_type"))
                    (sort_header(filters, page_size, "salience", "salience"))
                    (sort_header(filters, page_size, "owner_id", "owner_id"))
                    th { "sender_id" }
                    th { "allow_ids" }
                    th { "topics" }
                    th { "body" }
                    (sort_header(filters, page_size, "valid_from", "valid_from"))
                    (sort_header(filters, page_size, "valid_to", "valid_to"))
                    th { "decay_reason" }
                    (sort_header(filters, page_size, "created_at", "created_at"))
                    (sort_header(filters, page_size, "updated_at", "updated_at"))
                    (sort_header(filters, page_size, "last_recall_at", "last_recall_at"))
                    (sort_header(filters, page_size, "recall_count_30d", "recall_30d"))
                    th { "style" }
                    th { "page_description" }
                    th { "source_ref" }
                    th { "authored_refs" }
                    th { "superseded_at" }
                    th { "superseded_by" }
                    th { "successor" }
                    th { "deleted_at" }
                    th { "deleted_reason" }
                } }
                tbody {
                    @for row in page_rows {
                        tr.fresh-row[row.fresh].inactive[!row.active] {
                            td.actions-cell { (action_cell(user, row)) }
                            td { (id_cell(&row.fact_id)) }
                            td { (status_cell(row)) }
                            td { code { (row.wiki_id) } }
                            td.muted { (opt_cell(row.fact_type.as_deref())) }
                            td { (opt_cell(row.salience.as_deref())) }
                            td { code { (row.owner_id) } }
                            td { (opt_cell(row.sender_id.as_deref())) }
                            td { (list_cell(&row.allow_ids)) }
                            td { (list_cell(&row.topics)) }
                            td.body-cell { (components::truncate_chars(&row.body, BODY_PREVIEW_CHARS)) }
                            td { (ts_cell(row.valid_from.as_deref())) }
                            td { (ts_cell(row.valid_to.as_deref())) }
                            td { (opt_cell(row.decay_reason.as_deref())) }
                            td { (fmt_ts(&row.created_at)) }
                            td { (ts_cell(row.updated_at.as_deref())) }
                            td { (ts_cell(row.last_recall_at.as_deref())) }
                            td.num {
                                @match row.recall_count_30d {
                                    Some(n) => { (n.to_string()) }
                                    None => { span.muted { "—" } }
                                }
                            }
                            td { (opt_cell(row.style.as_deref())) }
                            td { (opt_cell(row.page_description.as_deref())) }
                            td { (opt_cell(row.source_ref.as_deref())) }
                            td { (list_cell(&row.authored_refs)) }
                            td { (ts_cell(row.superseded_at.as_deref())) }
                            td {
                                @match row.superseded_by.as_deref() {
                                    Some(id) => { (id_cell(id)) }
                                    None => { span.muted { "—" } }
                                }
                            }
                            td {
                                @match row.successor_fact_id.as_deref() {
                                    Some(id) => { (id_cell(id)) }
                                    None => { span.muted { "—" } }
                                }
                            }
                            td { (ts_cell(row.deleted_at.as_deref())) }
                            td { (opt_cell(row.deleted_reason.as_deref())) }
                        }
                    }
                }
            }
        }

        (pagination_links(filters, page, page_size, total, total_pages, total_is_estimate))
    };
    layout::authenticated_page("Facts", user, &body)
}

fn filter_form(filters: &FactsFilters, page_size: usize) -> Markup {
    html! {
        form.facts-filter method="get" action="/dashboard/facts" {
            // The filter fields flex-wrap into columns on a wide screen and
            // collapse to one column on mobile (see `.field-grid`).
            div.field-grid {
                p {
                    label for="filter-wiki-id" { "wiki_id" }
                    input id="filter-wiki-id" type="text" name="wiki_id"
                        value=(filters.wiki_id.as_deref().unwrap_or(""))
                        placeholder="e.g. alice";
                }
                p {
                    label for="filter-fact-type" { "fact_type" }
                    input id="filter-fact-type" type="text" name="fact_type"
                        value=(filters.fact_type.as_deref().unwrap_or(""))
                        placeholder="e.g. preference";
                }
                p {
                    label for="filter-topic" { "topic" }
                    input id="filter-topic" type="text" name="topic"
                        value=(filters.topic.as_deref().unwrap_or(""))
                        placeholder="e.g. gardening";
                }
                p {
                    label for="filter-created-after" { "created_after (ISO 8601)" }
                    input id="filter-created-after" type="text" name="created_after"
                        value=(filters.created_after.as_deref().unwrap_or(""))
                        placeholder="2026-01-01T00:00:00Z";
                }
                p {
                    label for="filter-created-before" { "created_before (ISO 8601)" }
                    input id="filter-created-before" type="text" name="created_before"
                        value=(filters.created_before.as_deref().unwrap_or(""))
                        placeholder="2026-12-31T23:59:59Z";
                }
                p.field-narrow {
                    label for="filter-page-size" { "page_size" }
                    input id="filter-page-size" type="number" name="page_size"
                        min="1" max=(MAX_PAGE_SIZE.to_string())
                        value=(page_size.to_string());
                }
                p.facts-filter-toggle.field-wide {
                    label for="filter-include-inactive" {
                        input id="filter-include-inactive" type="checkbox"
                            name="include_inactive" value="1"
                            checked[filters.include_inactive()];
                        " include inactive (superseded / deleted)"
                    }
                }
            }
            // Preserve the active sort across a filter submit (the sort lives
            // in column-header links, not in this form).
            @if let Some(sort) = non_empty(filters.sort.as_deref()) {
                input type="hidden" name="sort" value=(sort);
                @if let Some(dir) = non_empty(filters.dir.as_deref()) {
                    input type="hidden" name="dir" value=(dir);
                }
            }
            p { button type="submit" { "Filtra" } }
        }
    }
}

fn action_cell(user: &SessionUser, row: &FactRow) -> Markup {
    let wiki_link = format!("/dashboard/wiki/{}", row.wiki_id);
    // The delete button is author-direct (the write-authority model —
    // docs/concepts/identity-and-acl.md): show it only to
    // the fact's `sender` or an admin, so a viewer who can merely read the fact
    // is not offered a "delete" that the POST would 403. The POST re-checks
    // sender-or-admin regardless. The active/promoted guard is the outer `@if`.
    // (Reveal lists every user's facts as admin → the button shows.)
    let self_principal = format!("user:{}", user.sender_id);
    let can_delete = user.is_admin || row.sender_id.as_deref() == Some(self_principal.as_str());
    html! {
        a href=(wiki_link) { "wiki" }
        // A fresh capture has no `fact_index` row yet, and an inactive
        // (superseded / deleted) fact is not a sensible edit target — the
        // edit / ACL / validity / delete actions operate on active promoted
        // facts only.
        @if !row.fresh && row.active {
            " · "
            a href=(format!("/dashboard/facts/{}/edit", row.fact_id)) { "edit" }
            @if can_delete {
                " · "
                (components::destructive_form(
                    &format!("/dashboard/facts/{}/delete", row.fact_id),
                    "delete",
                    "Delete this fact? It becomes a tombstone and disappears from recall.",
                ))
            }
        }
    }
}

/// The facts-browser pager: a `← previous` link, an **editable** page-number
/// box that jumps straight to the typed page, and a `next →` link. prev/next
/// are real links only when there is somewhere to go — otherwise a disabled
/// span — driven by the true `total_pages` the handler computed, so a single
/// page reads as an explicit "page 1 of 1" rather than a mysteriously dead
/// button. When the scan cap was hit `total_pages` is a lower bound, rendered
/// as `M+` with next kept open so the operator can walk past the cap.
fn pagination_links(
    filters: &FactsFilters,
    page: usize,
    page_size: usize,
    total: usize,
    total_pages: usize,
    total_is_estimate: bool,
) -> Markup {
    let prev_href = (page > 1).then(|| {
        format!(
            "/dashboard/facts?{}",
            filters.to_query_string(page - 1, page_size)
        )
    });
    // Exact when the whole set was scanned; when the cap was hit keep `next`
    // open so the operator can page past the lower-bound estimate.
    let has_next = page < total_pages || total_is_estimate;
    let next_href = has_next.then(|| {
        format!(
            "/dashboard/facts?{}",
            filters.to_query_string(page + 1, page_size)
        )
    });
    let pages_label = if total_is_estimate {
        format!("{total_pages}+")
    } else {
        total_pages.to_string()
    };
    let facts_label = match (total_is_estimate, total) {
        (true, n) => format!("{n}+ facts"),
        (false, 1) => "1 fact".to_owned(),
        (false, n) => format!("{n} facts"),
    };
    // Only bound the input when the count is exact; under an estimate the true
    // last page may be higher, so leave `max` off.
    let max_attr: Option<String> = (!total_is_estimate).then(|| total_pages.to_string());
    html! {
        nav.facts-pagination {
            @if let Some(href) = prev_href {
                a.pager-step href=(href) { "← previous" }
            } @else {
                span.pager-step.is-disabled aria-disabled="true" { "← previous" }
            }
            // Editable page number: a mini GET form that re-submits every active
            // filter (as hidden inputs) and jumps straight to the typed page.
            form.pager-jump method="get" action="/dashboard/facts" {
                (filter_hidden_inputs(filters, page_size))
                span.pager-of {
                    "page "
                    input.pager-page type="number" name="page"
                        min="1" max=[max_attr] value=(page.to_string())
                        inputmode="numeric" aria-label="page number";
                    " of " (pages_label)
                }
                button.pager-go type="submit" { "Go" }
            }
            @if let Some(href) = next_href {
                a.pager-step href=(href) { "next →" }
            } @else {
                span.pager-step.is-disabled aria-disabled="true" { "next →" }
            }
            span.pager-count.muted { (facts_label) }
        }
    }
}

/// Hidden inputs mirroring the active filter set (plus `page_size` and the
/// sort directive), so the pager's jump form round-trips every filter when it
/// re-submits with a new `page`. Mirrors the field set of
/// [`FactsFilters::to_query_string_with_sort`] — minus `page`, which the
/// visible number input supplies.
fn filter_hidden_inputs(filters: &FactsFilters, page_size: usize) -> Markup {
    html! {
        @if let Some(v) = non_empty(filters.wiki_id.as_deref()) {
            input type="hidden" name="wiki_id" value=(v);
        }
        @if let Some(v) = non_empty(filters.fact_type.as_deref()) {
            input type="hidden" name="fact_type" value=(v);
        }
        @if let Some(v) = non_empty(filters.topic.as_deref()) {
            input type="hidden" name="topic" value=(v);
        }
        @if let Some(v) = non_empty(filters.created_after.as_deref()) {
            input type="hidden" name="created_after" value=(v);
        }
        @if let Some(v) = non_empty(filters.created_before.as_deref()) {
            input type="hidden" name="created_before" value=(v);
        }
        @if let Some(v) = non_empty(filters.sort.as_deref()) {
            input type="hidden" name="sort" value=(v);
        }
        @if let Some(v) = non_empty(filters.dir.as_deref()) {
            input type="hidden" name="dir" value=(v);
        }
        @if filters.include_inactive() {
            input type="hidden" name="include_inactive" value="1";
        }
        input type="hidden" name="page_size" value=(page_size.to_string());
    }
}

/// Render the edit form page.
///
/// `flash` is shown above the form when set (the unchanged-submit branch
/// in [`edit_submit`] uses it to nudge the user without forcing them off
/// the page). `can_acl` (owner-or-admin) and `can_validity` (owner-or-admin)
/// gate the two structured sub-forms per the write-authority model (both the
/// owner's acts — visibility and update;
/// [identity and ACL](../../../../docs/concepts/identity-and-acl.md)), and
/// `is_smart` is the fact's wiki family — together they decide whether each
/// structured action renders as a live form or as a disabled note (smart wikis
/// carry no per-fragment ACL / validity).
fn render_edit_form(
    state: &DashboardState,
    user: &SessionUser,
    fact_id: &FactId,
    row: &FactIndexRow,
    can_acl: bool,
    can_validity: bool,
    is_smart: bool,
    flash: Option<&str>,
) -> String {
    let body = html! {
        @if let Some(message) = flash {
            (components::flash("error", message))
        }
        (fact_summary_dl(fact_id, row, &rendered_fact_body(state, row)))
        (structured_actions_section(fact_id, row, can_acl, can_validity, is_smart))
        (supersede_section(fact_id))
    };
    layout::authenticated_reading_page("Edit fact", user, &body)
}

/// The fact's canonical text rendered as safe HTML — markdown, media
/// embeds, and (when the wiki tree is reachable) wikilink click-through
/// resolved against the fact's own wiki, exactly like the page viewer.
/// The raw text was already visibility-gated by [`load_visible_fact`];
/// fact text carries no reveal wrappers and no `{{factref=…}}` markers,
/// so both stay off.
fn rendered_fact_body(state: &DashboardState, row: &FactIndexRow) -> Markup {
    let rendered = state.memory.as_ref().map_or_else(
        // Memory handles not wired (degraded boot): plain markdown, links
        // stay literal.
        || md_render::render(&row.text),
        |memory| {
            let index = wiki_view::wikilink_index(&memory.tree);
            let resolve =
                |target: &str| wiki_view::resolve_wikilink_href(&index, Some(&row.wiki_id), target);
            let ctx = md_render::PageRenderContext {
                resolve_wikilink: &resolve,
                fact_refs: false,
            };
            md_render::render_page(&row.text, false, &ctx, |_| None)
        },
    );
    PreEscaped(rendered)
}

/// The read-only "current state" summary of the fact: the full record —
/// placement (`wiki_id`), the three ACL axes (`owner` subject, `sender`
/// provenance, `allow` audience), taxonomy, validity bounds, document
/// provenance (`source_ref`) — followed by the canonical text rendered
/// as prose (see [`rendered_fact_body`]). `section.meta` picks up the
/// shared two-column definition grid.
fn fact_summary_dl(fact_id: &FactId, row: &FactIndexRow, body_html: &Markup) -> Markup {
    let allow_current = row
        .allow_ids
        .iter()
        .map(Principal::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let topics_current = row.topics.join(", ");
    let fact_type_current = row.fact_type.as_deref().unwrap_or("");
    let valid_from_current = row.valid_from.as_deref().unwrap_or("");
    let valid_to_current = row.valid_to.as_deref().unwrap_or("");
    html! {
        section.meta.fact-edit-summary {
            h2 { "Current state of fact " code { (fact_id.as_str()) } }
            dl {
                dt { "wiki_id" } dd { code { (row.wiki_id) } }
                dt { "owner" } dd { code { (row.owner_id) } }
                dt { "sender" }
                dd {
                    @if let Some(sender) = &row.sender_id {
                        code { (sender) }
                    } @else {
                        span.muted { "(unknown — legacy row)" }
                    }
                }
                dt { "allow" }
                dd {
                    @if allow_current.is_empty() {
                        span.muted { "(empty)" }
                    } @else {
                        code { (allow_current) }
                    }
                }
                dt { "topics" }
                dd {
                    @if topics_current.is_empty() {
                        span.muted { "(empty)" }
                    } @else {
                        code { (topics_current) }
                    }
                }
                dt { "fact_type" }
                dd {
                    @if fact_type_current.is_empty() {
                        span.muted { "(none)" }
                    } @else {
                        code { (fact_type_current) }
                    }
                }
                dt { "valid_from" }
                dd {
                    @if valid_from_current.is_empty() {
                        span.muted { "(none)" }
                    } @else {
                        code { (valid_from_current) }
                    }
                }
                dt { "valid_to" }
                dd {
                    @if valid_to_current.is_empty() {
                        span.muted { "(none)" }
                    } @else {
                        code { (valid_to_current) }
                    }
                }
                @if let Some(successor) = &row.successor_fact_id {
                    dt { "successor" }
                    dd {
                        // The fact that replaced this one (closure-stamped):
                        // one click to the current truth's record.
                        a href=(format!("/dashboard/facts/{}/edit", successor.as_str())) {
                            code { (successor.as_str()) }
                        }
                    }
                }
                dt { "created" } dd { code { (row.created_at) } }
                @if let Some(source_ref) = &row.source_ref {
                    dt { "source_ref" }
                    dd {
                        // A cited document must be viewable: a catalog id
                        // links to the ACL-gated media alias (view /
                        // download); any other ref (URL, legacy) stays text.
                        @if CatalogId::parse(source_ref).is_ok() {
                            a href=(format!("/dashboard/media/{source_ref}")) {
                                code { (source_ref) }
                            }
                        } @else {
                            code { (source_ref) }
                        }
                    }
                }
            }
            h3 { code { "body" } }
            div.wiki-page-view.prose { (body_html) }
        }
    }
}

/// The structured engine-direct ACL + validity sub-forms. Renders a
/// disabled note on smart wikis (no per-fragment governance); otherwise each
/// form renders only for the principal who may submit it (the write-authority
/// model):
/// the **ACL** form needs `can_acl` (owner-or-admin — visibility is the
/// subject's call), the **validity** form needs `can_validity` (owner-or-admin —
/// updating validity is the subject's act too, not a destruction). When the
/// viewer can do neither, an axis-accurate refusal note replaces both.
fn structured_actions_section(
    fact_id: &FactId,
    row: &FactIndexRow,
    can_acl: bool,
    can_validity: bool,
    is_smart: bool,
) -> Markup {
    let acl_action = format!("/dashboard/facts/{}/acl", fact_id.as_str());
    let validity_action = format!("/dashboard/facts/{}/validity", fact_id.as_str());
    let allow_current = row
        .allow_ids
        .iter()
        .map(Principal::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let owner_current = row.owner_id.to_string();
    let valid_from_current = row.valid_from.as_deref().unwrap_or("");
    let valid_to_current = row.valid_to.as_deref().unwrap_or("");
    html! {
        section.fact-structured-actions {
            h2 { "ACL and validity (structured actions)" }
            @if is_smart {
                p.muted {
                    "This wiki is "
                    strong { "smart" }
                    ": access and validity governance is wiki-level, not "
                    "per-fragment. Use the smart wiki's sharing page or the "
                    "smart consumer's own channels."
                }
            } @else if !can_acl && !can_validity {
                p.muted {
                    "Only the fact's owner (or an admin) may change its "
                    "visibility (ACL), and only its author (the "
                    code { "sender" }
                    ") or an admin may correct its validity."
                }
            } @else {
                p.muted {
                    "Applied directly to the engine (act-first), with a receipt "
                    "revertible within the undo window: after submitting you land "
                    "on the receipt, from which you can "
                    strong { "revert" }
                    "."
                }
                @if can_acl {
                    form.fact-acl method="post" action=(acl_action) {
                        h3 { "Change " code { "ACL" } }
                        p {
                            label for="acl-owner" { code { "owner" } }
                            input id="acl-owner" type="text" name="owner"
                                value=(owner_current)
                                placeholder="e.g. user:alice or group:famiglia or global";
                        }
                        p {
                            label for="acl-allow" { code { "allow=" } " (comma-separated list)" }
                            input id="acl-allow" type="text" name="allow"
                                value=(allow_current)
                                placeholder="e.g. user:bob, group:lavoro";
                            small.muted { "Clear the field to empty the list." }
                        }
                        p { button type="submit" { "Apply ACL" } }
                    }
                } @else {
                    p.muted {
                        "Only the fact's owner (or an admin) may change its "
                        "visibility (ACL)."
                    }
                }
                @if can_validity {
                    form.fact-validity method="post" action=(validity_action) {
                        h3 { "Correct the validity" }
                        p {
                            label for="validity-from" { code { "valid_from" } }
                            input id="validity-from" type="date" name="valid_from"
                                value=(date_part(valid_from_current));
                            small.muted { "Leave blank to keep this bound unchanged." }
                        }
                        p {
                            label for="validity-to" { code { "valid_to" } }
                            input id="validity-to" type="date" name="valid_to"
                                value=(date_part(valid_to_current));
                            small.muted { "Leave blank to keep this bound unchanged." }
                        }
                        p { button type="submit" { "Apply validity" } }
                    }
                } @else {
                    p.muted {
                        "Only the fact's author (the "
                        code { "sender" }
                        ") or an admin may correct its validity."
                    }
                }
            }
        }
    }
}

/// The body / topics / `fact_type` supersede form — the surface still on
/// the form-to-chat bridge.
fn supersede_section(fact_id: &FactId) -> Markup {
    let action = format!("/dashboard/facts/{}/edit/submit", fact_id.as_str());
    html! {
        section.fact-supersede {
            h2 { "Body, topics and fact_type (via chat)" }
            p.muted {
                "Edit the fields you want to change; leave blank the ones to "
                "keep. On submit we compose a textual request and pass it to "
                "the agentic chat panel, which asks for your explicit "
                "confirmation before applying the edit (form-to-chat bridge, see "
                a href="/dashboard/chat" { "chat" }
                ")."
            }
            form.fact-edit method="post" action=(action) {
                p {
                    label for="edit-topics" { "New " code { "topics" } " (comma-separated list)" }
                    input id="edit-topics" type="text" name="topics"
                        placeholder="e.g. gardening, spring";
                    small.muted { "Leave blank to keep unchanged." }
                }
                p {
                    label for="edit-fact-type" { "New " code { "fact_type" } }
                    input id="edit-fact-type" type="text" name="fact_type"
                        placeholder="e.g. preference, or 'clear' to remove it";
                    small.muted { "Leave blank to keep unchanged; write " code { "clear" } " to remove it." }
                }
                p {
                    label for="edit-body" { "New " code { "body" } }
                    textarea id="edit-body" name="body" rows="6"
                        placeholder="Leave blank to keep the fact's body unchanged." {}
                }
                p {
                    button type="submit" { "Send to chat" }
                    " · "
                    a href="/dashboard/facts" { "Cancel" }
                }
            }
        }
    }
}

/// Extract the `YYYY-MM-DD` date prefix from a stored bound so an
/// `<input type="date">` pre-fills cleanly (the column stores a full
/// RFC3339 timestamp). Returns the input verbatim when it is already a
/// bare date or empty.
fn date_part(bound: &str) -> &str {
    bound.split('T').next().unwrap_or(bound)
}

/// Percent-encode a query-string value. Keeps the implementation
/// dependency-free — we only need the unreserved set per RFC 3986 plus
/// a handful of safe punctuation, anything else gets `%HH`.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(fact_id: &str) -> FactIndexRow {
        FactIndexRow {
            authored_refs: Vec::new(),
            fact_id: FactId::parse(fact_id).expect("parse"),
            wiki_id: "alice".to_owned(),
            source_path: "wikis/alice/index.md".to_owned(),
            region_start: None,
            region_end: None,
            text: "Alice usa la bici a Milano".to_owned(),
            embedding: Vec::new(),
            owner_id: "user:alice".parse::<Principal>().expect("principal"),
            allow_ids: Vec::new(),
            sender_id: None,
            fact_type: Some("preferenza".to_owned()),
            topics: vec!["mobilita".to_owned(), "milano".to_owned()],
            created_at: "2026-01-01T10:00:00Z".to_owned(),
            updated_at: "2026-01-01T10:00:00Z".to_owned(),
            superseded_at: None,
            superseded_by: None,
            successor_fact_id: None,
            deleted_at: None,
            deleted_reason: None,
            last_recall_at: None,
            recall_count_30d: 0,
            valid_from: None,
            valid_to: None,
            decay_reason: None,
            // Inert: re-derived/non-ingest fact — no classifier placement
            // proposal to carry.
            target_page: None,
            style: None,
            page_description: None,
            salience: None,
            source_ref: None,
        }
    }

    #[test]
    fn normalise_pagination_clamps_zero_and_caps_page_size() {
        let f = FactsFilters {
            page: Some(0),
            page_size: Some(0),
            ..FactsFilters::default()
        };
        assert_eq!(normalise_pagination(&f), (1, DEFAULT_PAGE_SIZE));

        let f = FactsFilters {
            page: Some(3),
            page_size: Some(9999),
            ..FactsFilters::default()
        };
        assert_eq!(normalise_pagination(&f), (3, MAX_PAGE_SIZE));
    }

    #[test]
    fn to_query_string_preserves_filters_and_skips_empty_fields() {
        let f = FactsFilters {
            wiki_id: Some("alice".to_owned()),
            fact_type: Some(String::new()),
            topic: Some("giardinaggio".to_owned()),
            created_after: None,
            created_before: None,
            page: None,
            page_size: None,
            ..Default::default()
        };
        let qs = f.to_query_string(2, 25);
        assert!(qs.contains("wiki_id=alice"), "{qs}");
        assert!(qs.contains("topic=giardinaggio"), "{qs}");
        assert!(qs.contains("page=2"), "{qs}");
        assert!(qs.contains("page_size=25"), "{qs}");
        assert!(!qs.contains("fact_type="), "{qs}");
    }

    #[test]
    fn url_encode_percent_encodes_specials() {
        assert_eq!(url_encode("ab c+d"), "ab%20c%2Bd");
        assert_eq!(url_encode("plain"), "plain");
    }

    #[test]
    fn pager_single_page_disables_both_steps_and_pins_of_one() {
        // One page of visible facts (franz's real case): prev AND next are inert
        // spans — the bug the founder saw — but now labelled "of 1" so the dead
        // state is self-explanatory, plus an editable page box.
        let html = pagination_links(&FactsFilters::default(), 1, 50, 41, 1, false).into_string();
        assert_eq!(
            html.matches("pager-step is-disabled").count(),
            2,
            "both prev and next inert on a single page: {html}"
        );
        assert!(!html.contains("href="), "no navigable links: {html}");
        assert!(html.contains("of 1"), "shows total page count: {html}");
        assert!(html.contains("41 facts"), "shows the visible total: {html}");
        assert!(
            html.contains(r#"name="page""#) && html.contains(r#"value="1""#),
            "renders the editable page input: {html}"
        );
    }

    #[test]
    fn pager_middle_page_links_both_directions_and_preserves_filters() {
        let filters = FactsFilters {
            wiki_id: Some("alice".to_owned()),
            topic: Some("giardinaggio".to_owned()),
            ..FactsFilters::default()
        };
        let html = pagination_links(&filters, 2, 50, 130, 3, false).into_string();
        assert!(html.contains("page=1"), "previous points at page 1: {html}");
        assert!(html.contains("page=3"), "next points at page 3: {html}");
        // The jump form re-submits the active filters as hidden inputs.
        assert!(
            html.contains(r#"name="wiki_id" value="alice""#),
            "wiki_id round-trips: {html}"
        );
        assert!(
            html.contains(r#"name="topic" value="giardinaggio""#),
            "topic round-trips: {html}"
        );
        assert!(html.contains("of 3"), "shows 3 total pages: {html}");
    }

    #[test]
    fn pager_capped_scan_reads_as_lower_bound() {
        // Scan cap hit: totals become lower bounds — "M+", "N+ facts", next kept
        // open, and no `max` on the input so the operator can page past the cap.
        let html = pagination_links(&FactsFilters::default(), 100, 50, MAX_SCAN_ROWS, 100, true)
            .into_string();
        assert!(html.contains("of 100+"), "estimate marker on pages: {html}");
        assert!(
            html.contains("page=101"),
            "next stays open past the cap: {html}"
        );
        assert!(
            !html.contains("max="),
            "no upper bound under an estimate: {html}"
        );
    }

    #[test]
    fn fmt_ts_strips_fraction_and_timezone() {
        assert_eq!(
            fmt_ts("2026-06-23T16:32:45.947580466+00:00"),
            "2026-06-23 16:32:45"
        );
        assert_eq!(fmt_ts("2026-06-23T00:00:00Z"), "2026-06-23 00:00:00");
        // Degrades gracefully on a date-only / odd value.
        assert_eq!(fmt_ts("2026-06-23"), "2026-06-23");
    }

    #[test]
    fn sort_directive_parses_known_columns_and_defaults_descending() {
        // Unknown / empty → no directive (engine default created_at DESC).
        assert!(FactsFilters::default().sort_directive().is_none());
        let unknown = FactsFilters {
            sort: Some("nope".to_owned()),
            ..Default::default()
        };
        assert!(unknown.sort_directive().is_none());

        // A known column with no explicit dir defaults to descending.
        let desc = FactsFilters {
            sort: Some("recall_count_30d".to_owned()),
            ..Default::default()
        };
        let d = desc.sort_directive().expect("directive");
        assert_eq!(d.key, FactSortKey::RecallCount30d);
        assert!(d.desc);

        // Only an explicit `asc` flips the direction.
        let asc = FactsFilters {
            sort: Some("created_at".to_owned()),
            dir: Some("asc".to_owned()),
            ..Default::default()
        };
        assert!(!asc.sort_directive().expect("directive").desc);
    }

    #[test]
    fn include_inactive_toggle_maps_into_core_filters() {
        let off = FactsFilters::default();
        assert!(!off.to_core_filters(10).include_inactive);
        let on = FactsFilters {
            include_inactive: Some("1".to_owned()),
            ..Default::default()
        };
        assert!(on.to_core_filters(10).include_inactive);
    }

    #[test]
    fn form_to_delta_treats_blank_fields_as_untouched() {
        let form = EditFactForm::default();
        let delta = form_to_delta(&form);
        assert!(delta.is_empty());
    }

    #[test]
    fn form_to_delta_clear_keyword_drops_fact_type() {
        let form = EditFactForm {
            fact_type: "clear".to_owned(),
            ..EditFactForm::default()
        };
        let delta = form_to_delta(&form);
        assert_eq!(delta.fact_type, FactTypeDelta::Clear);
        assert!(!delta.is_empty());
    }

    #[test]
    fn form_to_delta_named_fact_type_lands_as_set() {
        let form = EditFactForm {
            fact_type: " preferenza ".to_owned(),
            ..EditFactForm::default()
        };
        let delta = form_to_delta(&form);
        assert_eq!(delta.fact_type, FactTypeDelta::Set("preferenza".to_owned()));
        assert!(!delta.is_empty());
    }

    #[test]
    fn form_to_delta_csv_fields_are_split_and_trimmed() {
        let form = EditFactForm {
            topics: "uno, due, ".to_owned(),
            ..EditFactForm::default()
        };
        let delta = form_to_delta(&form);
        assert_eq!(delta.topics, Some(vec!["uno".to_owned(), "due".to_owned()]));
    }

    #[test]
    fn compose_returns_none_on_empty_delta() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta::default();
        assert!(
            compose_edit_message(&row.fact_id, &row, &delta).is_none(),
            "empty delta must abort the bridge",
        );
    }

    #[test]
    fn compose_emits_metadata_only_branch() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta {
            topics: Some(vec!["nuovo-topic".to_owned()]),
            ..EditDelta::default()
        };
        let msg = compose_edit_message(&row.fact_id, &row, &delta).expect("metadata-only message");
        assert!(
            msg.contains("018f1234-5678-7abc-9def-0123456789ab"),
            "{msg}"
        );
        assert!(msg.contains("alice"), "{msg}");
        assert!(msg.contains("set `topics`"), "{msg}");
        assert!(
            !msg.contains("```"),
            "metadata-only must not include a fenced body: {msg}"
        );
    }

    #[test]
    fn compose_emits_body_only_branch_with_fenced_block() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta {
            body: Some("Alice ora usa la metro".to_owned()),
            ..EditDelta::default()
        };
        let msg = compose_edit_message(&row.fact_id, &row, &delta).expect("body-only message");
        assert!(msg.contains("change the body to:"), "{msg}");
        assert!(msg.contains("```\nAlice ora usa la metro\n```"), "{msg}");
        assert!(
            !msg.contains("set `topics`"),
            "body-only must not enumerate metadata: {msg}"
        );
    }

    #[test]
    fn compose_emits_mixed_branch_with_metadata_then_body() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta {
            topics: Some(vec!["nuovo-topic".to_owned()]),
            body: Some("Body nuovo".to_owned()),
            ..EditDelta::default()
        };
        let msg = compose_edit_message(&row.fact_id, &row, &delta).expect("mixed message");
        assert!(msg.contains("set `topics`"), "{msg}");
        assert!(
            msg.contains(", and change the body to:"),
            "mixed must chain metadata + body: {msg}"
        );
        assert!(msg.contains("```\nBody nuovo\n```"), "{msg}");
    }

    #[test]
    fn compose_clear_fact_type_is_rendered_explicitly() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta {
            fact_type: FactTypeDelta::Clear,
            ..EditDelta::default()
        };
        let msg = compose_edit_message(&row.fact_id, &row, &delta).expect("fact_type clear");
        assert!(msg.contains("remove `fact_type`"), "{msg}");
        assert!(
            msg.contains("preferenza"),
            "must echo previous value: {msg}"
        );
    }

    #[test]
    fn compose_set_fact_type_echoes_old_and_new_values() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        let delta = EditDelta {
            fact_type: FactTypeDelta::Set("bio".to_owned()),
            ..EditDelta::default()
        };
        let msg = compose_edit_message(&row.fact_id, &row, &delta).expect("fact_type set");
        assert!(msg.contains("set `fact_type`"), "{msg}");
        assert!(msg.contains("`preferenza`"), "{msg}");
        assert!(msg.contains("`bio`"), "{msg}");
    }

    // ---- structured-action helper unit tests ----

    fn session(sender: &str, is_admin: bool) -> SessionUser {
        SessionUser {
            sender_id: sender.to_owned(),
            is_admin,
            session_jti: "jti".to_owned(),
        }
    }

    /// `owner_or_admin` is the **`acl_change`** (visibility) gate — owner of
    /// the subject, or admin. The author/`sender` axis does not enter here.
    #[test]
    fn owner_or_admin_gate_admits_owner_and_admin_only() {
        let row = make_row("018f1234-5678-7abc-9def-0123456789ab"); // owner = user:alice
        assert!(owner_or_admin(&session("alice", false), &row), "owner");
        assert!(owner_or_admin(&session("bob", true), &row), "admin");
        assert!(
            !owner_or_admin(&session("bob", false), &row),
            "non-owner non-admin refused"
        );
        // The Result-returning wrapper agrees.
        assert!(enforce_owner_or_admin(&session("alice", false), &row).is_ok());
        assert!(matches!(
            enforce_owner_or_admin(&session("bob", false), &row),
            Err(DashboardError::Forbidden)
        ));
    }

    /// `sender_or_admin` is the **`delete`** (author-direct) gate — the fact's
    /// `sender`, or admin. Crucially it is independent of the `owner`/subject
    /// axis: the owner (alice) is NOT admitted to delete a fact she did not
    /// author, while the author (carol) is — even though she is not owner.
    /// (Updates — edit / validity — are the owner's, gated by `owner_or_admin`.)
    #[test]
    fn sender_or_admin_gate_admits_sender_and_admin_only() {
        // owner = user:alice, author/sender = user:carol.
        let mut row = make_row("018f1234-5678-7abc-9def-0123456789ab");
        row.sender_id = Some("user:carol".parse::<Principal>().expect("principal"));

        assert!(sender_or_admin(&session("carol", false), &row), "sender");
        assert!(sender_or_admin(&session("dave", true), &row), "admin");
        assert!(
            !sender_or_admin(&session("dave", false), &row),
            "non-sender non-admin refused"
        );
        // The owner/subject is NOT the author → refused the direct delete (her
        // path is a request → vote, opened from the dashboard).
        assert!(
            !sender_or_admin(&session("alice", false), &row),
            "owner-but-not-author refused the delete"
        );
        // The Result-returning wrapper agrees.
        assert!(enforce_sender_or_admin(&session("carol", false), &row).is_ok());
        assert!(matches!(
            enforce_sender_or_admin(&session("alice", false), &row),
            Err(DashboardError::Forbidden)
        ));
    }

    #[test]
    fn date_part_strips_the_time_component() {
        assert_eq!(date_part("2026-01-01T00:00:00+00:00"), "2026-01-01");
        assert_eq!(date_part("2026-01-01"), "2026-01-01");
        assert_eq!(date_part(""), "");
    }

    #[test]
    fn normalize_date_bound_accepts_date_and_rfc3339_and_rejects_garbage() {
        assert_eq!(normalize_date_bound("   ").unwrap(), None);
        // Bare date promotes to midnight UTC.
        let promoted = normalize_date_bound("2026-03-15").unwrap().expect("some");
        assert!(promoted.starts_with("2026-03-15T00:00:00"), "{promoted}");
        assert!(chrono::DateTime::parse_from_rfc3339(&promoted).is_ok());
        // Full RFC3339 passes through verbatim.
        assert_eq!(
            normalize_date_bound("2026-03-15T12:30:00Z").unwrap(),
            Some("2026-03-15T12:30:00Z".to_owned())
        );
        // Garbage is a validation error.
        assert!(matches!(
            normalize_date_bound("not-a-date"),
            Err(DashboardError::Validation(_))
        ));
    }

    #[test]
    fn map_operator_edit_err_classifies_vanished_as_not_found() {
        let vanished = OperatorEditError::FactVanished(
            FactId::parse("018f1234-5678-7abc-9def-0123456789ab").unwrap(),
        );
        assert!(matches!(
            map_operator_edit_err(&vanished),
            DashboardError::NotFound
        ));
    }
}
