// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-gated token management.
//!
//! Canonical references: dashboard,
//! [memory model](../../../../docs/concepts/memory-model.md).
//!
//! Five handlers:
//!
//! - GET  `/tokens`                                 — landing page with
//!   the issue form, the consumer-delegations table, and the revoked
//!   `token_blacklist`. New tokens are minted from the form below the
//!   tables.
//! - POST `/tokens/issue`                           — validate, resolve
//!   the sender from the chosen **consumer class** (smart ⇒ the picked
//!   human owner; standard ⇒ the bot id, a system user created on the
//!   spot), upsert `consumer_delegations` for a standard token, sign a
//!   JWT, and render the page with the new token shown **once** (the
//!   server never persists issued tokens).
//! - POST `/tokens/revoke`                          — insert into
//!   `token_blacklist` and refresh the in-memory cache so the next
//!   verify sees it.
//! - GET  `/tokens/delegation/:consumer_id`         — edit form for an
//!   existing consumer's `allowed_sender_ids`.
//! - POST `/tokens/delegation/:consumer_id`         — apply the edit.
//!
//! Per the dashboard design the
//! `is_admin` JWT claim is **derived, never toggled in the form**: for a
//! smart token it inherits the chosen owner's `enrollment_users.is_admin`;
//! a standard token is always non-admin (its sender is a credential-less
//! bot identity). There is at most one admin per deployment, so the
//! dashboard exposes no "issue as admin" knob.

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;
use crate::ui::{components, layout};
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::Utc;
use maud::{Markup, html};
use mwe_core::enrollment;
use mwe_core::jwt::{self, ConsumerClass, DEFAULT_EXPOSED_TTL, DEFAULT_INTERNAL_TTL, TokenClaims};
use mwe_core::oauth_server;
use mwe_core::types::WikiId;
use mwe_core::wiki::{IdentityKind, create_identity_wiki, ensure_is_agent_marker};
use serde::Deserialize;

pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/tokens", get(landing).post(landing))
        .route("/tokens/issue", post(issue_submit))
        .route("/tokens/revoke", post(revoke_submit))
        .route("/tokens/connection/revoke", post(connection_revoke_submit))
        .route(
            "/tokens/delegation/:consumer_id",
            get(delegation_form).post(delegation_submit),
        )
}

#[derive(Debug)]
struct DelegationRow {
    consumer_id: String,
    allowed_sender_ids: Vec<String>,
    granted_at: String,
    granted_by: String,
}

#[derive(Debug)]
struct BlacklistEntry {
    jti: String,
    revoked_at: String,
    reason: Option<String>,
    revoked_by: Option<String>,
}

async fn fetch_delegations(state: &DashboardState) -> Result<Vec<DelegationRow>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT consumer_id, allowed_sender_ids, granted_at, granted_by
           FROM consumer_delegations
          ORDER BY consumer_id ASC",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(consumer_id, allowed, granted_at, granted_by)| DelegationRow {
                consumer_id,
                allowed_sender_ids: serde_json::from_str(&allowed).unwrap_or_default(),
                granted_at,
                granted_by,
            },
        )
        .collect())
}

async fn fetch_blacklist(state: &DashboardState) -> Result<Vec<BlacklistEntry>> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT jti, revoked_at, reason, revoked_by
           FROM token_blacklist
          ORDER BY revoked_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(jti, revoked_at, reason, revoked_by)| BlacklistEntry {
            jti,
            revoked_at,
            reason,
            revoked_by,
        })
        .collect())
}

async fn fetch_connections(state: &DashboardState) -> Result<Vec<oauth_server::Connection>> {
    oauth_server::list_connections(&state.pool)
        .await
        .map_err(|e| DashboardError::Internal(format!("list webagentoauth connections: {e}")))
}

async fn fetch_user_ids(state: &DashboardState) -> Result<Vec<String>> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM enrollment_users ORDER BY user_id ASC")
            .fetch_all(&state.pool)
            .await?;
    Ok(ids)
}

async fn landing(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let users = fetch_user_ids(&state).await?;
    let delegations = fetch_delegations(&state).await?;
    let blacklist = fetch_blacklist(&state).await?;
    let connections = fetch_connections(&state).await?;
    Ok(Html(render_landing(
        admin.session(),
        &users,
        &delegations,
        &blacklist,
        &connections,
        &FlashSlot::None,
        &IssueFormState::default(),
    )))
}

/// What the landing template shows above the issue form. Either nothing,
/// the freshly-minted token (with copy-once warning), or an error.
enum FlashSlot<'a> {
    None,
    NewToken {
        token: String,
        claims: &'a TokenClaims,
    },
    Error(&'a str),
    Success(&'a str),
}

/// Sticky values for the issue form — preserved when the form is
/// re-rendered after a validation error so the admin doesn't lose
/// their input.
///
/// The form has a single either/or axis — the consumer **class** — that
/// shapes the rest: a `smart` token binds a human `owner_id` as its
/// sender, a `standard` token derives its sender from the `consumer_id`
/// (the bot's own system user) and carries an act-as delegation list.
#[derive(Debug, Default)]
struct IssueFormState {
    /// `"smart"` or `"standard"`. Empty (the `Default`) renders as smart.
    consumer_class: String,
    /// Smart only: the human owner who is the token's sender.
    owner_id: String,
    /// Device id (smart) or bot id (standard).
    consumer_id: String,
    /// Free audit label; blank ⇒ server fills it from `consumer_id`.
    device_label: String,
    rate_limit_id: String,
    ttl_profile: String,
    /// Standard only: the humans the bot may act as.
    allowed_sender_ids: Vec<String>,
}

impl IssueFormState {
    /// Whether this form renders the smart branch. Smart is the default,
    /// so anything that is not an explicit `"standard"` reads as smart.
    fn is_smart(&self) -> bool {
        self.consumer_class != "standard"
    }
}

fn render_landing(
    session: &crate::auth::SessionUser,
    users: &[String],
    delegations: &[DelegationRow],
    blacklist: &[BlacklistEntry],
    connections: &[oauth_server::Connection],
    flash: &FlashSlot<'_>,
    form: &IssueFormState,
) -> String {
    let body = html! {
        @match &flash {
            FlashSlot::None => {},
            FlashSlot::NewToken { token, claims } => {
                section.token-card {
                    (components::flash("success", "Token issued — copy it now, it is not stored server-side."))
                    dl {
                        dt { "sender_id" } dd { code { (claims.sender_id) } }
                        dt { "device_label" } dd { code { (claims.device_label) } }
                        dt { "rate_limit_id" } dd { code { (claims.rate_limit_id) } }
                        dt { "jti" } dd { code { (claims.jti) } }
                        dt { "exp" } dd { (rfc3339(claims.exp)) " · unix " (claims.exp) }
                        dt { "isAdmin" } dd { (claims.is_admin) }
                        dt { "consumer_class" }
                        dd { code { (if claims.consumer_class.is_smart() { "smart" } else { "standard" }) } }
                        @if let Some(c) = &claims.consumer_id {
                            dt { "consumer_id" } dd { code { (c) } }
                        }
                    }
                    p { strong { "Token" } }
                    textarea readonly rows="3" class="token-display" { (token) }
                }
            }
            FlashSlot::Error(msg) => { (components::flash("error", msg)) }
            FlashSlot::Success(msg) => { (components::flash("success", msg)) }
        }

        h2 { "Issue a new token" }
        (render_issue_form(users, form))

        h2 { "Consumer delegations" }
        @if delegations.is_empty() {
            p.muted { "No consumer delegations yet. Issue a consumer token above to create one." }
        } @else {
            (render_delegations_table(delegations))
        }

        h2 { "Web agent connections" }
        (render_connections_section(connections))

        h2 { "Revoked tokens" }
        (render_blacklist_table(blacklist))
        (render_revoke_form())

        p.muted {
            "The server does not store issued tokens — only revocations live here. "
            "Token contents are recoverable only from the holder's environment."
        }
    };
    layout::authenticated_page("Tokens", session, &body)
}

/// The act-as checkbox list shared by the issue form and the delegation
/// editor: every enrolled user, then the builtin `guest` pseudo-identity
/// (delegable without enrollment — ticking it enables serving
/// unidentified humans on this consumer).
fn act_as_checkboxes(users: &[String], selected: &[String]) -> Markup {
    html! {
        fieldset.members-list {
            @for u in users {
                label.member-checkbox {
                    input type="checkbox" name="allowed_sender_ids" value=(u)
                        checked[selected.iter().any(|x| x == u)];
                    " " (u)
                }
            }
            label.member-checkbox {
                input type="checkbox" name="allowed_sender_ids" value="guest"
                    checked[selected.iter().any(|x| x == "guest")];
                " guest " span.muted { "(builtin — unidentified humans)" }
            }
        }
    }
}

fn render_issue_form(users: &[String], form: &IssueFormState) -> Markup {
    let smart = form.is_smart();
    html! {
        form id="token-issue-form" action="/dashboard/tokens/issue" method="post" {
            // The one axis that shapes the rest of the form. `tokens.js`
            // shows only the branch the chosen class needs; without JS
            // both branches are visible and the server validates per
            // class, so the form still works.
            p {
                label { "Consumer class" }
                label.radio {
                    input type="radio" name="consumer_class" value="smart" checked[smart];
                    " Smart — one human owner, brings its own LLM (Claude Code, Cowork). "
                    "Unlocks the " code { "wiki_admin_*" } " tools."
                }
                label.radio {
                    input type="radio" name="consumer_class" value="standard" checked[!smart];
                    " Standard — a multi-user bot serving several people, acting as each "
                    "(Telegram, mail, home automation)."
                }
            }

            // Smart only: the human owner is the token's sender.
            div id="owner-field" {
                p {
                    label for="owner_id" { "Owner (user)" }
                    select id="owner_id" name="owner_id" {
                        option value="" disabled selected[form.owner_id.is_empty()] { "— choose a user —" }
                        @for u in users {
                            option value=(u) selected[u == &form.owner_id] { (u) }
                        }
                    }
                    p.help.muted {
                        "The human this smart consumer belongs to. The token's isAdmin claim is "
                        "derived from this user's role (one admin per deployment, no override here)."
                    }
                }
            }

            // Shared id: a free "Device id" for smart, the "Bot id" for
            // standard (tokens.js swaps the label + help).
            p {
                label id="consumer-id-label" for="consumer_id" {
                    @if smart { "Device id" } @else { "Bot id" }
                }
                input id="consumer_id" name="consumer_id" type="text" value=(form.consumer_id) required;
                p.help.muted id="consumer-id-help" {
                    @if smart {
                        "Free identifier for this device/session (e.g. cc-laptop). Recorded so "
                        code { "wiki_admin_op_log" } " can attribute writes to a specific device."
                    } @else {
                        "The bot's own identity. Issuing creates a credential-less system user with "
                        "this id (and its wiki). Lowercase letters and digits only, start with a "
                        "letter — no hyphen."
                    }
                }
            }

            // Device label sits directly under the id and mirrors it.
            p {
                label for="device_label" { "Device label" }
                input id="device_label" name="device_label" type="text" value=(form.device_label);
                p.help.muted {
                    "A free label shown in the audit log (" code { "tool_executions" } ") so you can "
                    "tell sessions apart. Defaults to the id above; change it only if you want a "
                    "different audit name."
                }
            }

            // Standard only: the humans the bot may act as.
            div id="acts-as-field" {
                p {
                    label { "Acts as (allowed senders)" }
                    @if users.is_empty() {
                        p.muted { "No users yet." }
                    }
                    (act_as_checkboxes(users, &form.allowed_sender_ids))
                    p.help.muted {
                        "The people this bot serves. Each call delegates to one of them via "
                        code { "X-MWE-Act-As" } "; the bot writes each datum as the right human, "
                        "never as itself. Granting " code { "guest" } " lets the consumer serve "
                        "humans it cannot identify (an unrecognized voice, an unknown chat "
                        "sender): guest turns recall only public memory, store nothing, and "
                        "carry a reserved-behaviour directive."
                    }
                }
            }

            p {
                label { "TTL profile" }
                label.radio {
                    input type="radio" name="ttl_profile" value="internal"
                        checked[form.ttl_profile != "exposed"];
                    " internal — 1 year (local device, owner trust)"
                }
                label.radio {
                    input type="radio" name="ttl_profile" value="exposed"
                        checked[form.ttl_profile == "exposed"];
                    " exposed — 30 days (Cloudflare Tunnel, third-party device)"
                }
            }

            (components::text_field("rate_limit_id", "Rate limit id", "text",
                if form.rate_limit_id.is_empty() { "default" } else { &form.rate_limit_id }, true))

            (components::submit("Issue token"))
        }
        script src="/dashboard/static/tokens.js" defer {}
    }
}

fn render_delegations_table(delegations: &[DelegationRow]) -> Markup {
    html! {
        table {
            thead { tr {
                th { "Consumer id" }
                th { "Allowed senders" }
                th { "Granted at" }
                th { "Granted by" }
                th { "Actions" }
            } }
            tbody {
                @for d in delegations {
                    tr {
                        td { code { (d.consumer_id) } }
                        td { (d.allowed_sender_ids.join(", ")) }
                        td { (d.granted_at) }
                        td { (d.granted_by) }
                        td {
                            a href=(format!("/dashboard/tokens/delegation/{}", d.consumer_id)) {
                                "edit"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_blacklist_table(blacklist: &[BlacklistEntry]) -> Markup {
    html! {
        @if blacklist.is_empty() {
            p.muted { "No revoked tokens." }
        } @else {
            table {
                thead { tr {
                    th { "jti" }
                    th { "Revoked at" }
                    th { "By" }
                    th { "Reason" }
                } }
                tbody {
                    @for b in blacklist {
                        tr {
                            td { code { (b.jti) } }
                            td { (b.revoked_at) }
                            td { (b.revoked_by.clone().unwrap_or_default()) }
                            td { (b.reason.clone().unwrap_or_default()) }
                        }
                    }
                }
            }
        }
    }
}

fn render_revoke_form() -> Markup {
    html! {
        form action="/dashboard/tokens/revoke" method="post" {
            h3 { "Revoke a token" }
            (components::text_field("jti", "JWT id (jti)", "text", "", true))
            (components::text_field("reason", "Reason (free text)", "text", "", true))
            (components::submit("Revoke"))
        }
    }
}

/// The active `webagentoauth` connections (roadmap 19f), each with a Disconnect
/// button that revokes its refresh tokens. The dedicated wiki is left intact.
fn render_connections_section(connections: &[oauth_server::Connection]) -> Markup {
    html! {
        @if connections.is_empty() {
            p.muted {
                "No web-agent connections yet. One appears when a user approves a "
                code { "webagentoauth" } " connection (e.g. the claude.ai web app)."
            }
        } @else {
            table {
                thead {
                    tr {
                        th { "User" } th { "Connection" } th { "Wiki" } th { "Since" } th {}
                    }
                }
                tbody {
                    @for c in connections {
                        tr {
                            td { code { (c.sender_id) } }
                            td { code { (c.consumer_id) } }
                            td { code { (c.wiki_id) } }
                            td { (c.created_at) }
                            td {
                                form action="/dashboard/tokens/connection/revoke" method="post" {
                                    input type="hidden" name="sender_id" value=(c.sender_id);
                                    input type="hidden" name="consumer_id" value=(c.consumer_id);
                                    (components::submit("Disconnect"))
                                }
                            }
                        }
                    }
                }
            }
            p.muted {
                "Disconnecting stops the connection from renewing; any live session ends within "
                "an hour (the access token's lifetime). The dedicated wiki is kept."
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct IssueSubmission {
    /// `"smart"` or `"standard"` — the consumer class radio.
    #[serde(default)]
    pub consumer_class: String,
    /// Smart only: the human owner who becomes the token's sender.
    #[serde(default)]
    pub owner_id: String,
    /// Device id (smart) or bot id (standard).
    #[serde(default)]
    pub consumer_id: String,
    /// Free audit label. Blank ⇒ the server fills it from `consumer_id`.
    #[serde(default)]
    pub device_label: String,
    #[serde(default)]
    pub rate_limit_id: String,
    pub ttl_profile: String,
    /// Standard only: the humans the bot may act as.
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,
}

async fn issue_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(submission): HtmlForm<IssueSubmission>,
) -> Result<Response> {
    let users = fetch_user_ids(&state).await?;
    let smart = submission.consumer_class != "standard";
    let sticky = build_sticky(&submission);

    if submission.ttl_profile != "internal" && submission.ttl_profile != "exposed" {
        let msg = format!("Unknown TTL profile {:?}.", submission.ttl_profile);
        return render_issue_error(&state, admin.session(), &users, &msg, &sticky).await;
    }
    let consumer_id = submission.consumer_id.trim().to_owned();
    if consumer_id.is_empty() {
        let msg = if smart {
            "Smart consumer requires a device id."
        } else {
            "Standard consumer requires a bot id."
        };
        return render_issue_error(&state, admin.session(), &users, msg, &sticky).await;
    }

    // Resolve the sender and apply the per-class side effects: a smart
    // token binds the chosen human owner; a standard token derives its
    // sender from the bot id (a system user it creates) and records the
    // act-as delegation.
    let resolution = if smart {
        resolve_smart_sender(&state, &submission.owner_id).await?
    } else {
        resolve_standard_sender(
            &state,
            &consumer_id,
            &submission.allowed_sender_ids,
            &users,
            admin.sender_id(),
        )
        .await?
    };
    let (sender_id, is_admin) = match resolution {
        Ok(pair) => pair,
        Err(msg) => {
            return render_issue_error(&state, admin.session(), &users, &msg, &sticky).await;
        },
    };

    // device_label defaults to the id when left blank (mirrors the form's JS).
    let device_label = {
        let dl = submission.device_label.trim();
        if dl.is_empty() {
            consumer_id.as_str()
        } else {
            dl
        }
    };

    let mut claims = build_claims(
        &sender_id,
        device_label,
        &submission.rate_limit_id,
        &submission.ttl_profile,
        is_admin,
        smart,
    );
    claims.consumer_id = Some(consumer_id);

    // Diagonal identity model: same shared validator as the CLI
    // (`mwe-mcp token-issue`) so the two never drift — standard ⇒ system
    // user + consumer_id; smart ⇒ owner. See
    // `docs/concepts/identity-and-acl.md` §1.
    if let Err(msg) = enrollment::validate_token_identity(
        &state.pool,
        &claims.sender_id,
        claims.consumer_class,
        claims.consumer_id.is_some(),
    )
    .await
    {
        return render_issue_error(&state, admin.session(), &users, &msg, &sticky).await;
    }

    let token = jwt::issue(&state.secret, &claims).map_err(DashboardError::Token)?;
    tracing::info!(
        actor = admin.sender_id(),
        sender = %claims.sender_id,
        consumer = ?claims.consumer_id,
        class = ?claims.consumer_class,
        jti = %claims.jti,
        "dashboard issued token"
    );

    let delegations = fetch_delegations(&state).await?;
    let blacklist = fetch_blacklist(&state).await?;
    let connections = fetch_connections(&state).await?;
    Ok(Html(render_landing(
        admin.session(),
        &users,
        &delegations,
        &blacklist,
        &connections,
        &FlashSlot::NewToken {
            token,
            claims: &claims,
        },
        &IssueFormState::default(),
    ))
    .into_response())
}

/// Resolve a *smart* token's sender: the owner must be an existing user,
/// and the `isAdmin` claim derives from that user's role. `Ok(Err(msg))`
/// carries a user-facing validation message; the outer `Result` is for
/// SQL failures.
async fn resolve_smart_sender(
    state: &DashboardState,
    owner_id: &str,
) -> Result<std::result::Result<(String, bool), String>> {
    let owner = owner_id.trim();
    if owner.is_empty() {
        return Ok(Err("Choose the owner user for this smart consumer.".into()));
    }
    let row: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(owner)
            .fetch_optional(&state.pool)
            .await?;
    let Some(is_admin_raw) = row else {
        return Ok(Err(format!("Unknown owner {owner:?}.")));
    };
    Ok(Ok((owner.to_owned(), is_admin_raw != 0)))
}

/// Resolve a *standard* token's sender: the bot id *is* the sender — a
/// credential-less system user created on first use. The act-as list is
/// validated first so a bad request never leaves an orphan system user;
/// then the delegation is recorded. A standard token is never admin.
async fn resolve_standard_sender(
    state: &DashboardState,
    bot_id: &str,
    allowed: &[String],
    users: &[String],
    actor: &str,
) -> Result<std::result::Result<(String, bool), String>> {
    if allowed.is_empty() {
        return Ok(Err(
            "A standard consumer needs at least one allowed sender.".into(),
        ));
    }
    // The builtin `guest` pseudo-identity is delegable without being an
    // enrolled user — the grant IS the guest-feature enable switch.
    if let Some(bad) = allowed
        .iter()
        .find(|a| !users.iter().any(|u| u == *a) && !enrollment::is_guest(a))
    {
        return Ok(Err(format!("Unknown allowed sender {bad:?}.")));
    }
    if let Some(msg) = ensure_system_user(state, bot_id).await? {
        return Ok(Err(msg));
    }
    upsert_delegation(state, actor, bot_id, allowed).await?;
    Ok(Ok((bot_id.to_owned(), false)))
}

/// Make sure `bot_id` exists as a credential-less **system user** — the
/// standard consumer's own identity — creating it and its identity wiki
/// on first use. `Some(msg)` ⇒ the id is unusable (bad shape, clashes
/// with a human login account or a group); `None` ⇒ ready to bind.
async fn ensure_system_user(state: &DashboardState, bot_id: &str) -> Result<Option<String>> {
    if !enrollment::is_valid_user_id(bot_id) || !enrollment::is_filesystem_safe(bot_id) {
        return Ok(Some(
            "Bot id must be lowercase letters and digits, start with a letter, and be \
             filesystem-safe."
                .into(),
        ));
    }
    if enrollment::is_guest(bot_id) {
        return Ok(Some(
            "\"guest\" is the builtin unidentified-human pseudo-identity — it cannot be a \
             bot's system user. Delegate `guest` to the consumer instead."
                .into(),
        ));
    }
    // A bot's whole point is to own a wiki, so the id must parse as a
    // WikiId up front: an underscore passes the user-id rule but breaks
    // wiki creation, so reject it here rather than ship a wiki-less bot.
    let Ok(wiki_id) = WikiId::parse(bot_id) else {
        return Ok(Some(
            "Bot id must be lowercase letters and digits only (no underscore or hyphen) \
             so its wiki can be created."
                .into(),
        ));
    };

    // Reuse an existing system user; refuse to bind a human login account
    // (that would let the bot read and write as that one person).
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
        .bind(bot_id)
        .fetch_one(&state.pool)
        .await?;
    if exists > 0 {
        if enrollment::is_system_user(&state.pool, bot_id).await? {
            // Backfill the `is_agent` marker (roadmap 27d / 4i) on an agent wiki
            // that predates the marker — idempotent, best-effort, so an existing
            // bot self-describes from its next token mint without a reconcile job.
            if let Some(memory) = state.memory.as_ref()
                && let Err(e) = ensure_is_agent_marker(&memory.tree, &wiki_id)
            {
                tracing::warn!(bot = bot_id, error = %e, "is_agent backfill failed (non-fatal)");
            }
            return Ok(None);
        }
        return Ok(Some(format!(
            "{bot_id:?} is a human account with a login — pick a different bot id (binding it \
             would let the bot read and write as that person)."
        )));
    }
    let group_clash: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrollment_groups WHERE group_id = ?")
            .bind(bot_id)
            .fetch_one(&state.pool)
            .await?;
    if group_clash > 0 {
        return Ok(Some(format!(
            "Id {bot_id:?} clashes with an existing group — pick another bot id."
        )));
    }

    // Create the credential-less identity (no invitation: a bot never logs in).
    sqlx::query("INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES (?, '[]', 0)")
        .bind(bot_id)
        .execute(&state.pool)
        .await?;
    tracing::info!(
        bot = bot_id,
        "dashboard created system user for standard consumer"
    );

    // Materialize its wiki now; non-fatal on FS failure, same policy as
    // the user-CRUD create path.
    if let Some(memory) = state.memory.as_ref()
        && let Err(e) = create_identity_wiki(&memory.tree, &wiki_id, bot_id, IdentityKind::Agent)
    {
        tracing::error!(
            bot = bot_id,
            error = %e,
            "identity wiki creation failed for new system user; DB row is up, run mwe-mcp doctor"
        );
    }
    Ok(None)
}

fn build_sticky(submission: &IssueSubmission) -> IssueFormState {
    IssueFormState {
        consumer_class: submission.consumer_class.clone(),
        owner_id: submission.owner_id.clone(),
        consumer_id: submission.consumer_id.clone(),
        device_label: submission.device_label.clone(),
        rate_limit_id: submission.rate_limit_id.clone(),
        ttl_profile: submission.ttl_profile.clone(),
        allowed_sender_ids: submission.allowed_sender_ids.clone(),
    }
}

fn build_claims(
    sender_id: &str,
    device_label: &str,
    rate_limit_id: &str,
    ttl_profile: &str,
    is_admin: bool,
    smart: bool,
) -> TokenClaims {
    let ttl = if ttl_profile == "exposed" {
        DEFAULT_EXPOSED_TTL
    } else {
        DEFAULT_INTERNAL_TTL
    };
    let rate = rate_limit_id.trim();
    let rate = if rate.is_empty() { "default" } else { rate };
    let mut claims = TokenClaims::new(sender_id.trim(), device_label.trim(), rate, ttl);
    claims.is_admin = is_admin;
    claims.consumer_class = if smart {
        ConsumerClass::Smart
    } else {
        ConsumerClass::Standard
    };
    claims
}

async fn upsert_delegation(
    state: &DashboardState,
    actor: &str,
    consumer_id: &str,
    allowed: &[String],
) -> Result<()> {
    let allowed_json = serde_json::to_string(allowed)
        .map_err(|e| DashboardError::Internal(format!("encoding delegation: {e}")))?;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO consumer_delegations
            (consumer_id, allowed_sender_ids, granted_at, granted_by)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(consumer_id) DO UPDATE SET
            allowed_sender_ids = excluded.allowed_sender_ids,
            granted_at         = excluded.granted_at,
            granted_by         = excluded.granted_by",
    )
    .bind(consumer_id)
    .bind(&allowed_json)
    .bind(&now)
    .bind(actor)
    .execute(&state.pool)
    .await?;
    // Propagate the change to the MCP middleware now instead of
    // waiting up to DELEGATION_REFRESH_INTERVAL — the admin clicking
    // "Save" expects the next bot call to see the new list.
    state
        .delegations
        .refresh(&state.pool)
        .await
        .map_err(|e| DashboardError::Internal(format!("refreshing delegation cache: {e}")))?;
    Ok(())
}

/// Shared error rendering path for `issue_submit` — fetches the
/// supporting tables and renders the landing page with a sticky form +
/// flash. Extracted to keep `issue_submit` under the clippy line cap
/// and centralise the "fetch delegations + blacklist" boilerplate.
async fn render_issue_error(
    state: &DashboardState,
    session: &crate::auth::SessionUser,
    users: &[String],
    msg: &str,
    sticky: &IssueFormState,
) -> Result<Response> {
    let delegations = fetch_delegations(state).await?;
    let blacklist = fetch_blacklist(state).await?;
    let connections = fetch_connections(state).await?;
    Ok(Html(render_landing(
        session,
        users,
        &delegations,
        &blacklist,
        &connections,
        &FlashSlot::Error(msg),
        sticky,
    ))
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct RevokeSubmission {
    pub jti: String,
    pub reason: String,
}

async fn revoke_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(submission): HtmlForm<RevokeSubmission>,
) -> Result<Response> {
    let jti = submission.jti.trim();
    if jti.is_empty() {
        return Err(DashboardError::Validation("jti is required.".into()));
    }
    let reason = submission.reason.trim();
    if reason.is_empty() {
        return Err(DashboardError::Validation(
            "Reason is required for audit.".into(),
        ));
    }

    let exp = Utc::now().timestamp() + i64::try_from(DEFAULT_INTERNAL_TTL.as_secs()).unwrap_or(0);
    jwt::revoke(&state.pool, jti, reason, admin.sender_id(), exp).await?;
    state.blacklist.refresh(&state.pool).await?;

    tracing::info!(
        actor = admin.sender_id(),
        jti,
        reason,
        "dashboard revoked token"
    );

    let users = fetch_user_ids(&state).await?;
    let delegations = fetch_delegations(&state).await?;
    let blacklist = fetch_blacklist(&state).await?;
    let connections = fetch_connections(&state).await?;
    let msg = format!("Revoked {jti}.");
    Ok(Html(render_landing(
        admin.session(),
        &users,
        &delegations,
        &blacklist,
        &connections,
        &FlashSlot::Success(&msg),
        &IssueFormState::default(),
    ))
    .into_response())
}

/// POST `/tokens/connection/revoke` — disconnect a `webagentoauth` connection by
/// revoking its refresh tokens. The short-lived access token then lapses on its
/// own (≤ 1 h); the dedicated wiki is left untouched.
#[derive(Debug, Deserialize)]
pub struct ConnectionRevokeSubmission {
    pub sender_id: String,
    pub consumer_id: String,
}

async fn connection_revoke_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(submission): HtmlForm<ConnectionRevokeSubmission>,
) -> Result<Response> {
    let sender_id = submission.sender_id.trim();
    let consumer_id = submission.consumer_id.trim();
    if sender_id.is_empty() || consumer_id.is_empty() {
        return Err(DashboardError::Validation(
            "sender_id and consumer_id are required.".into(),
        ));
    }
    let revoked = oauth_server::revoke_connection(&state.pool, sender_id, consumer_id)
        .await
        .map_err(|e| DashboardError::Internal(format!("revoke connection: {e}")))?;
    tracing::info!(
        actor = admin.sender_id(),
        sender_id,
        consumer_id,
        revoked,
        "dashboard disconnected webagentoauth connection"
    );

    let users = fetch_user_ids(&state).await?;
    let delegations = fetch_delegations(&state).await?;
    let blacklist = fetch_blacklist(&state).await?;
    let connections = fetch_connections(&state).await?;
    let msg = format!("Disconnected {consumer_id} ({sender_id}).");
    Ok(Html(render_landing(
        admin.session(),
        &users,
        &delegations,
        &blacklist,
        &connections,
        &FlashSlot::Success(&msg),
        &IssueFormState::default(),
    ))
    .into_response())
}

#[derive(Debug)]
struct DelegationFormState {
    consumer_id: String,
    allowed: Vec<String>,
}

async fn delegation_form(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(consumer_id): Path<String>,
) -> Result<Html<String>> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT allowed_sender_ids FROM consumer_delegations WHERE consumer_id = ?",
    )
    .bind(&consumer_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(allowed_json) = row else {
        return Err(DashboardError::NotFound);
    };
    let allowed: Vec<String> = serde_json::from_str(&allowed_json).unwrap_or_default();
    let users = fetch_user_ids(&state).await?;

    Ok(Html(render_delegation_form(
        admin.session(),
        &users,
        &DelegationFormState {
            consumer_id: consumer_id.clone(),
            allowed,
        },
        None,
    )))
}

#[derive(Debug, Deserialize)]
pub struct DelegationSubmission {
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,
}

async fn delegation_submit(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Path(consumer_id): Path<String>,
    HtmlForm(submission): HtmlForm<DelegationSubmission>,
) -> Result<Response> {
    let exists: i64 =
        sqlx::query_scalar("SELECT count(*) FROM consumer_delegations WHERE consumer_id = ?")
            .bind(&consumer_id)
            .fetch_one(&state.pool)
            .await?;
    if exists == 0 {
        return Err(DashboardError::NotFound);
    }
    let users = fetch_user_ids(&state).await?;
    for s in &submission.allowed_sender_ids {
        // `guest` is delegable without enrollment (the builtin pseudo-identity).
        if !users.iter().any(|u| u == s) && !enrollment::is_guest(s) {
            return Ok(Html(render_delegation_form(
                admin.session(),
                &users,
                &DelegationFormState {
                    consumer_id: consumer_id.clone(),
                    allowed: submission.allowed_sender_ids.clone(),
                },
                Some(&format!("Unknown allowed sender {s:?}.")),
            ))
            .into_response());
        }
    }

    let allowed_json = serde_json::to_string(&submission.allowed_sender_ids)
        .map_err(|e| DashboardError::Internal(format!("encoding delegation: {e}")))?;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE consumer_delegations
            SET allowed_sender_ids = ?, granted_at = ?, granted_by = ?
          WHERE consumer_id = ?",
    )
    .bind(&allowed_json)
    .bind(&now)
    .bind(admin.sender_id())
    .bind(&consumer_id)
    .execute(&state.pool)
    .await?;
    state
        .delegations
        .refresh(&state.pool)
        .await
        .map_err(|e| DashboardError::Internal(format!("refreshing delegation cache: {e}")))?;

    tracing::info!(actor = admin.sender_id(), consumer = %consumer_id, "delegation updated");

    Ok(Redirect::to("/dashboard/tokens").into_response())
}

fn render_delegation_form(
    session: &crate::auth::SessionUser,
    users: &[String],
    form: &DelegationFormState,
    error: Option<&str>,
) -> String {
    let title = format!("Delegation — {}", form.consumer_id);
    let body = html! {
        @if let Some(msg) = error {
            (components::flash("error", msg))
        }
        p.muted {
            "Changes propagate at the next tool call from the consumer "
            "(delegation table is read per-call with a short cache TTL). "
            "The bot does not need to re-issue or restart."
        }
        form action=(format!("/dashboard/tokens/delegation/{}", form.consumer_id)) method="post" {
            p {
                label { "Allowed senders for " code { (form.consumer_id) } }
                (act_as_checkboxes(users, &form.allowed))
                p.help.muted {
                    "Granting " code { "guest" } " lets the consumer serve humans it cannot "
                    "identify: guest turns recall only public memory, store nothing, and carry "
                    "a reserved-behaviour directive."
                }
            }
            (components::submit("Save"))
        }
        p { a href="/dashboard/tokens" { "Back to tokens" } }
    };
    layout::authenticated_page(&title, session, &body)
}

/// Format a unix timestamp as RFC3339 for human display.
fn rfc3339(unix_secs: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp(unix_secs, 0)
        .map_or_else(|| "<invalid>".to_owned(), |dt| dt.to_rfc3339())
}
