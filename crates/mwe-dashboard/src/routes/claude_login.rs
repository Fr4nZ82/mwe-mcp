// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Log in with Claude Code" routes (Admin → LLM config).
//!
//! The login panel on the model-settings page drives this flow. The
//! operator opens Claude's authorization page, approves, and pastes back
//! the `code#state` blob it shows: [`start`] renders the link and the
//! paste box, [`paste`] finishes the exchange.
//!
//! **The browser never comes back here on its own.** The `redirect_uri`
//! is [`oauth::OOB_REDIRECT_URI`], the out-of-band page on Claude's own
//! domain, because Claude Code's OAuth client accepts no callback of
//! this server's. Copying the code by hand *is* the return channel.
//!
//! The attempt lives in the single-slot [`PendingClaudeLogin`] held on
//! [`DashboardState`] (CSRF `state` + PKCE verifier), consumed by
//! [`paste`]. All routes are admin-only. **Test / personal use only** —
//! never a deployed product auth mode; see [`mwe_core::oauth`].

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::post;
use maud::html;
use mwe_core::oauth;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::{DashboardState, PendingClaudeLogin};
use crate::ui::layout;

use super::llm_config;

/// Sub-router for the Claude Code login flow. Mounted inside the
/// authenticated (admin-gated) tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/claude-login/start", post(start))
        .route("/admin/claude-login/paste", post(paste))
        .route("/admin/claude-login/logout", post(logout))
}

/// Begin a login: mint PKCE + CSRF state, stash the pending attempt, and
/// render the authorize link plus the paste box.
async fn start(State(state): State<DashboardState>, admin: AdminUser) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let pkce = oauth::generate_pkce().map_err(|e| oauth_err(&e))?;
    let csrf = oauth::generate_state().map_err(|e| oauth_err(&e))?;

    let authorize_url = oauth::build_authorize_url(oauth::OOB_REDIRECT_URI, &csrf, &pkce.challenge)
        .map_err(|e| oauth_err(&e))?;

    *state.claude_login.lock() = Some(PendingClaudeLogin {
        verifier: pkce.verifier,
        state: csrf,
    });

    Ok(Html(render_manual_page(chrome, admin.session(), &authorize_url)).into_response())
}

#[derive(Debug, serde::Deserialize)]
struct PasteForm {
    pasted: String,
}

/// Out-of-band return channel: the operator pastes the `code#state` blob
/// Claude's authorization page displayed.
async fn paste(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<PasteForm>,
) -> Result<Response> {
    let (code, returned_state) = oauth::parse_pasted_code(&form.pasted);
    if code.is_empty() {
        return status_page(
            &state,
            &admin,
            "error",
            "Paste the full code shown by Claude's authorization page.",
        );
    }
    finish_login(&state, &admin, &code, returned_state.as_deref()).await
}

/// Drop the stored credentials (and any in-flight attempt).
async fn logout(State(state): State<DashboardState>, admin: AdminUser) -> Result<Response> {
    *state.claude_login.lock() = None;
    if let Some(store) = oauth::global_store() {
        store
            .clear()
            .map_err(|e| DashboardError::Internal(format!("clearing Claude Code login: {e}")))?;
    }
    status_page(&state, &admin, "success", "Logged out of Claude Code.")
}

/// Shared completion: take the pending attempt, verify the CSRF state,
/// exchange the code through the login store, and render the result.
async fn finish_login(
    state: &DashboardState,
    admin: &AdminUser,
    code: &str,
    returned_state: Option<&str>,
) -> Result<Response> {
    // Take the single pending attempt — drop the guard before the await
    // on the network exchange (the guard is not held across `.await`).
    let pending = state.claude_login.lock().take();
    let Some(pending) = pending else {
        return status_page(
            state,
            admin,
            "error",
            "No Claude Code login is in progress — start it again from this page.",
        );
    };
    // CSRF: the state the provider echoes must match the one we issued.
    // Only enforced when a state came back (a bare pasted code has none).
    if let Some(returned) = returned_state
        && returned != pending.state
    {
        return status_page(
            state,
            admin,
            "error",
            "Claude Code login state mismatch — aborted for safety. Start the login again.",
        );
    }

    let store = oauth::global_store().ok_or_else(|| {
        DashboardError::Internal(
            "Claude Code login store is not installed — restart `mwe-mcp serve`.".to_owned(),
        )
    })?;

    match store
        .login_with_code(
            code,
            &pending.verifier,
            oauth::OOB_REDIRECT_URI,
            &pending.state,
        )
        .await
    {
        Ok(()) => {
            tracing::info!("claude-login: logged in via dashboard (token persisted)");
            status_page(
                state,
                admin,
                "success",
                "Logged in with Claude Code — anthropic `claude-code` slots are live.",
            )
        },
        Err(e) => status_page(
            state,
            admin,
            "error",
            &format!("Claude Code token exchange failed: {e}"),
        ),
    }
}

/// Render the LLM-config page with a flash (the page the login lives on).
fn status_page(
    state: &DashboardState,
    admin: &AdminUser,
    kind: &'static str,
    msg: &str,
) -> Result<Response> {
    let chrome = layout::Chrome::of(state);
    let memory = llm_config::require_memory(state)?;
    Ok(Html(llm_config::render_status_page(
        chrome,
        admin.session(),
        memory,
        kind,
        msg,
    ))
    .into_response())
}

fn render_manual_page(
    chrome: layout::Chrome,
    session: &SessionUser,
    authorize_url: &str,
) -> String {
    let body = html! {
        h2 { "Log in with Claude Code" }
        p.muted {
            "Test / personal use only — this signs requests with your own Claude "
            "subscription, presented as the Claude CLI."
        }
        ol {
            li {
                "Open Claude's authorization page: "
                a href=(authorize_url) target="_blank" rel="noopener noreferrer" {
                    "authorize mwe-mcp"
                }
                "."
            }
            li { "Approve access — Claude shows you a short code." }
            li { "Copy it and paste it below. Claude does not send you back here; "
                 "carrying the code across is the last step." }
        }
        form action="/dashboard/admin/claude-login/paste" method="post" {
            input type="text" name="pasted" placeholder="paste the code…" autocomplete="off" required;
            button type="submit" { "Finish login" }
            p.help.muted { "Finishing lands you back on Models & keys, where the "
                           "Claude Code card says whether it worked." }
        }
        p { a href="/dashboard/admin/llm-config" { "← back to LLM config" } }
    };
    layout::authenticated_page(chrome, "Claude Code login", session, &body)
}

fn oauth_err(e: &oauth::OauthError) -> DashboardError {
    DashboardError::Internal(format!("claude-login: {e}"))
}
