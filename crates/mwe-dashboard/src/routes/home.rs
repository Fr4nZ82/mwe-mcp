// SPDX-License-Identifier: AGPL-3.0-or-later
//! Authenticated landing page.
//!
//! Nothing here is a console of its own: every card sends the reader to
//! the page that does the work, and only to pages that reader can open.
//!
//! **Who the page opens on depends on who signed in.** A person lands on
//! their own memory — their wiki, the facts about them, their standing
//! rules, what was recalled for them — because that is what they came for
//! and it is the part of the memory that is theirs. The deployment-wide
//! counts and the server address a consumer connects to are the operator's
//! view of the same room: they count and expose things past the
//! per-fragment ACL, so they are read and rendered for an admin alone.
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
    // The operator's view: counts taken across the whole deployment, and
    // the address a consumer is pointed at. Read for an admin alone — see
    // [`OperatorCounts`] — so a reader's page runs none of these queries.
    let operator = if user.is_admin {
        Some((OperatorCounts::read(&state).await?, mcp_url(&headers)))
    } else {
        None
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
                    (missing_slots_phrase(missing_slots.len()))
                    " no model: " (missing_slots.join(", ")) "."
                }
                " The memory does not work until every slot has one. "
                a href="/dashboard/admin/llm-config" { "Set them →" }
            }
        }
        @if let Some((c, mcp_url)) = &operator {
            section.kpi-grid {
                div.kpi { strong { (c.wikis_with_facts) } " wikis with facts" }
                div.kpi { strong { (c.active_facts) } " active facts" }
                div.kpi { strong { (c.pending_proposals) } " pending proposals" }
                div.kpi { strong { (c.recent_calls) } " MCP calls (24h)" }
                div.kpi { strong { (c.users) } " users" }
                div.kpi { strong { (c.groups) } " groups" }
                div.kpi { strong { (c.delegations) } " consumer delegations" }
                div.kpi { strong { (c.pending_invitations) } " open invitations" }
            }

            h2 { "Connect a consumer" }
            section.endpoint-card {
                p { "Point any consumer — the bot or assistant that talks to this "
                    "memory — at this server:" }
                pre.endpoint-display { (mcp_url) }
                // A frozen deployment mints nothing, so it is shown no
                // door to the mint.
                @if !frozen {
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

            // This card is a list of things to DO, and a frozen deployment
            // does none of them: every control on the pages it links is
            // refused by [`crate::read_only`]. The consoles themselves are
            // mounted on every deployment and stay in the top bar, so an
            // admin reading a shown instance still reaches all of them —
            // what goes is the invitation to act, never the way in.
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

/// The address a consumer connects to, derived from how the operator
/// reached us (the `Host` header). Informational, so an absent header
/// never fails the page: it falls back to the relative path.
fn mcp_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host.is_empty() {
        return "/mcp".to_owned();
    }
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}/mcp")
}

/// The subject of the unconfigured-slots banner: how many of the six have
/// nothing behind them, and the verb that goes with it.
///
/// There are six slots and all six are mandatory, so `missing` runs 1 to 6
/// and the sentence says it in words — "all six model slots have", "two of
/// the six model slots have". A digit against a spelled number in the same
/// sentence ("6 of the six") reads as a defect, and the screen this
/// sentence appears on is a fresh install: the one that has to be
/// believed before anything else is.
fn missing_slots_phrase(missing: usize) -> String {
    let word = match missing {
        1 => "One",
        2 => "Two",
        3 => "Three",
        4 => "Four",
        5 => "Five",
        // Every slot: naming the count twice ("six of the six") says less
        // than saying it once.
        _ => return "All six model slots have".to_owned(),
    };
    let verb = if missing == 1 { "has" } else { "have" };
    format!("{word} of the six model slots {verb}")
}

/// The counts that belong to the operator, read for an admin alone.
///
/// Two reasons, one gate. The last five count rows on a console that
/// answers a non-admin with a 403, so printing them to a reader would
/// describe a room they cannot walk into. The first three count the whole
/// deployment past the per-fragment ACL: a reader's own view of the memory
/// is the slice they may see, so a total taken across everybody's is not a
/// number they could reproduce, or one about them.
struct OperatorCounts {
    wikis_with_facts: i64,
    active_facts: i64,
    pending_proposals: i64,
    users: i64,
    groups: i64,
    delegations: i64,
    pending_invitations: i64,
    recent_calls: i64,
}

impl OperatorCounts {
    async fn read(state: &DashboardState) -> Result<Self> {
        let wikis_with_facts: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT wiki_id) FROM fact_index
              WHERE superseded_at IS NULL AND deleted_at IS NULL",
        )
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
            wikis_with_facts,
            active_facts,
            pending_proposals,
            users,
            groups,
            delegations,
            pending_invitations,
            recent_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::missing_slots_phrase;

    /// The banner is read on a fresh install, so it counts in words all the
    /// way up: one slot or six, the number and the total are the same kind
    /// of word in the same sentence.
    #[test]
    fn the_phrase_counts_in_words_and_agrees_with_its_verb() {
        assert_eq!(missing_slots_phrase(1), "One of the six model slots has");
        assert_eq!(missing_slots_phrase(2), "Two of the six model slots have");
        assert_eq!(missing_slots_phrase(5), "Five of the six model slots have");
        assert_eq!(missing_slots_phrase(6), "All six model slots have");
    }
}
