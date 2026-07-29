// SPDX-License-Identifier: AGPL-3.0-or-later
//! Authenticated landing page.
//!
//! Intentionally small: a welcome panel, three
//! counts (users / groups / consumer delegations), and the deep links
//! into the CRUD pages. The dashboards-of-substance (memory explorer,
//! proposals, audit, costs, chat) ship later.

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
    let recent_calls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_executions WHERE timestamp > ?")
            .bind((chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339())
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
    let body = maud::html! {
        section.kpi-grid {
            div.kpi { strong { (wiki_count) } " wikis with facts" }
            div.kpi { strong { (active_facts) } " active facts" }
            div.kpi { strong { (pending_proposals) } " pending proposals" }
            div.kpi { strong { (recent_calls) } " MCP calls (24h)" }
            div.kpi { strong { (users) } " users" }
            div.kpi { strong { (groups) } " groups" }
            div.kpi { strong { (delegations) } " consumer delegations" }
            div.kpi { strong { (pending_invitations) } " open invitations" }
        }

        h2 { "Connect a consumer" }
        section.endpoint-card {
            p { "Point any MCP consumer at this server:" }
            pre.endpoint-display { (mcp_url) }
            @if !frozen {
                p {
                    a href="/dashboard/tokens" { "Issue a token →" }
                    " — a bearer credential; pick " strong { "smart" } " vs "
                    strong { "standard" } " at mint time."
                }
            }
            p {
                a href="/dashboard/bridges" { "Wire a consumer →" }
                " — for a bridged host like hermes, the per-consumer install "
                "instructions (plugins, install command, disabling its built-in memory)."
            }
        }

        // Navigation link-lists laid out as side-by-side cards so they use the
        // page width instead of stacking in the left column. `.home-sections`
        // is an auto-fit grid: one row on a wide screen, collapsing to a single
        // column on mobile.
        div.home-sections {
            section.home-card {
                h2 { "Memory" }
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
                    }
                }
            }
        }
    };

    Ok(Html(layout::authenticated_page(chrome, "Home", &user, &body)).into_response())
}
