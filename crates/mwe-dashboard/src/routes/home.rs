// SPDX-License-Identifier: AGPL-3.0-or-later
//! Authenticated landing page.
//!
//! A row of counts read straight off the database, the MCP endpoint a
//! consumer connects to, and the deep links into the rest of the
//! dashboard. Nothing here is a console of its own: every card sends the
//! reader to the page that does the work — and only to pages the reader
//! can open, which is why the operator's counts and the token link are
//! read and rendered for an admin alone.
//!
//! The one thing it says loudly is a model slot with nothing behind it —
//! all six are mandatory, so a missing one is an unfinished install and
//! the banner is the first thing an admin sees.

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::auth::SessionUser;
use crate::error::Result;
use crate::state::DashboardState;
use crate::ui::layout;

/// GET `/dashboard/home`.
#[allow(
    clippy::too_many_lines,
    reason = "one large maud page body; splitting it would hurt readability"
)]
pub async fn index(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    user: SessionUser,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    // First-run guard: a user who has not finished (or skipped) the
    // profile wizard is sent back to it. This is the return path — the
    // brand logo (`/dashboard/` → home) and the Home tab both land here,
    // so an accidental click away from the wizard never strands them.
    //
    // A frozen deployment skips the redirect. The wizard is mounted like
    // every other page and can be walked into deliberately, but its whole
    // job is to write a person's first facts: sending a visitor there on
    // arrival would open the demonstration on the one screen that cannot
    // work, before they have seen anything that does.
    if !crate::read_only::hides_writes(&state)
        && !crate::routes::welcome::user_already_initialized(&state, &user.sender_id).await?
    {
        return Ok(Redirect::to("/dashboard/welcome").into_response());
    }
    // The operator's counts. Every one of them counts rows behind a
    // console that answers a non-admin with a 403, so they are read only
    // for somebody who could open the page the number is about.
    let operator_counts = if user.is_admin {
        Some(OperatorCounts::read(&state).await?)
    } else {
        None
    };
    let active_facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fact_index WHERE superseded_at IS NULL AND deleted_at IS NULL",
    )
    .fetch_one(&state.pool)
    .await?;
    let pending_proposals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM structure_proposals WHERE status = 'pending'")
            .fetch_one(&state.pool)
            .await?;
    let wiki_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT wiki_id) FROM fact_index
          WHERE superseded_at IS NULL AND deleted_at IS NULL",
    )
    .fetch_one(&state.pool)
    .await?;
    // The MCP endpoint URL is informational; derive it from how the
    // operator reached us (Host header), but never fail the home page if
    // the header is absent — fall back to a relative path.
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mcp_url = if host.is_empty() {
        "/mcp".to_string()
    } else {
        let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{host}/mcp")
    };

    let frozen = crate::read_only::hides_writes(&state);
    // The one thing an admin must see before anything else: a model role
    // with nothing behind it, because the memory does not work until all
    // six have one.
    let missing_slots: Vec<&'static str> = if user.is_admin {
        state
            .memory
            .as_ref()
            .map(|m| super::llm_config::unconfigured_slots(&m.llm_config_snapshot()))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let body = maud::html! {
        @if !missing_slots.is_empty() {
            p.flash.flash-error {
                strong {
                    (missing_slots.len()) " of the six model slots "
                    (if missing_slots.len() == 1 { "has" } else { "have" })
                    " no model: " (missing_slots.join(", ")) "."
                }
                " The memory does not work until every slot has one. "
                a href="/dashboard/admin/llm-config" { "Set them →" }
            }
        }
        section.kpi-grid {
            div.kpi { strong { (wiki_count) } " wikis with facts" }
            div.kpi { strong { (active_facts) } " active facts" }
            div.kpi { strong { (pending_proposals) } " pending proposals" }
            @if let Some(c) = &operator_counts {
                div.kpi { strong { (c.recent_calls) } " MCP calls (24h)" }
                div.kpi { strong { (c.users) } " users" }
                div.kpi { strong { (c.groups) } " groups" }
                div.kpi { strong { (c.delegations) } " consumer delegations" }
                div.kpi { strong { (c.pending_invitations) } " open invitations" }
            }
        }

        h2 { "Connect a consumer" }
        section.endpoint-card {
            p { "Point any consumer — the bot or assistant that talks to this "
                "memory — at this server:" }
            pre.endpoint-display { (mcp_url) }
            // Minting a token is an operator console; a link to it from
            // a reader's home page is a 403 with extra steps.
            @if !frozen && user.is_admin {
                p {
                    a href="/dashboard/tokens" { "Issue a token →" }
                    " — a bearer credential; pick " strong { "smart" } " vs "
                    strong { "standard" } " at mint time."
                }
            }
            p {
                a href="/dashboard/bridges" { "Wire a consumer →" }
                " — the ready-made assistant (NanoClaw), and the per-consumer "
                "install instructions for any other bridged host (install "
                "command, disabling its built-in memory)."
            }
        }

        // Navigation link-lists laid out as side-by-side cards so they use the
        // page width instead of stacking in the left column. `.home-sections`
        // is an auto-fit grid: one row on a wide screen, collapsing to a single
        // column on mobile.
        div.home-sections {
            // Your own things first, because most people signing in are
            // here for their own memory and nothing else. Every link is
            // to a page the reader can open: the identity wiki and its
            // `@rules.md` are created with the account.
            section.home-card {
                h2 { "Your memory" }
                ul {
                    li {
                        a href=(format!("/dashboard/wiki/{}", user.sender_id)) {
                            "Your own wiki"
                        }
                    }
                    li {
                        // What the memory holds *about* them, which is
                        // not the same set as what is filed in their own
                        // wiki: a fact about them can live on a group's
                        // page, and their wiki holds facts about others.
                        a href=(format!("/dashboard/facts?subject=user:{}", user.sender_id)) {
                            "The facts about you"
                        }
                    }
                    li {
                        a href=(format!("/dashboard/wiki/{}/view/@rules.md", user.sender_id)) {
                            "Your standing rules"
                        }
                    }
                    li { a href="/dashboard/recall-traces" { "What was recalled for you" } }
                }
            }

            section.home-card {
                h2 { "All the memory" }
                ul {
                    li { a href="/dashboard/wiki" { "Browse wikis" } }
                    li { a href="/dashboard/facts" { "Browse facts" } }
                    // The chat is how pending changes get acted on, and a
                    // frozen deployment neither mounts it nor has anything
                    // pending to act on.
                    @if !frozen {
                        li { a href="/dashboard/chat" { "Review pending changes in the chat" } }
                    }
                }
            }

            // Every entry below leads to a console a frozen deployment
            // does not mount (`routes::build`), so the whole card goes.
            @if user.is_admin && !frozen {
                section.home-card {
                    h2 { "Admin actions" }
                    ul {
                        li { a href="/dashboard/users" { "Manage users" } }
                        li { a href="/dashboard/groups" { "Manage groups" } }
                        li { a href="/dashboard/tokens" { "Manage tokens" } }
                        li { a href="/dashboard/prompts" { "Edit operational prompts" } }
                        li { a href="/dashboard/admin/llm-config" { "Configure LLM slots + API keys" } }
                        li { a href="/dashboard/admin/recall-settings" { "Tune recall settings" } }
                    }
                }
            }

            @if !frozen {
                section.home-card {
                    h2 { "Your account" }
                    ul {
                        li { a href="/dashboard/settings/me" { "Change your password" } }
                        li { a href="/dashboard/settings/2fa" { "Two-factor sign-in" } }
                    }
                }
            }
        }
    };

    Ok(Html(layout::authenticated_page(chrome, "Home", &user, &body)).into_response())
}

/// The counts that belong to the operator's consoles.
///
/// Read only for an admin: each one counts rows on a page a non-admin is
/// refused, so printing them to a reader would describe a room they
/// cannot walk into.
struct OperatorCounts {
    users: i64,
    groups: i64,
    delegations: i64,
    pending_invitations: i64,
    recent_calls: i64,
}

impl OperatorCounts {
    async fn read(state: &DashboardState) -> Result<Self> {
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users")
            .fetch_one(&state.pool)
            .await?;
        let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_groups")
            .fetch_one(&state.pool)
            .await?;
        let delegations: i64 = sqlx::query_scalar("SELECT count(*) FROM consumer_delegations")
            .fetch_one(&state.pool)
            .await?;
        let pending_invitations: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM user_invitations WHERE consumed_at IS NULL AND expires_at > ?",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .fetch_one(&state.pool)
        .await?;
        let recent_calls: i64 =
            sqlx::query_scalar("SELECT count(*) FROM tool_executions WHERE timestamp > ?")
                .bind((chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339())
                .fetch_one(&state.pool)
                .await?;
        Ok(Self {
            users,
            groups,
            delegations,
            pending_invitations,
            recent_calls,
        })
    }
}
