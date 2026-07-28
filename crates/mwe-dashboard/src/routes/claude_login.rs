// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Log in with Claude Code" routes (Admin → LLM config).
//!
//! The login panel on the model-settings page drives this flow. Two
//! return channels share one PKCE exchange:
//!
//! - **Manual / out-of-band — the active flow.** The credential card
//!   always posts `manual=1`, so [`start`] renders the authorize link plus
//!   a paste box; the operator approves on Claude, copies the `code#state`
//!   blob the out-of-band page shows, and pastes it into [`paste`]. Its
//!   `redirect_uri` is [`oauth::OOB_REDIRECT_URI`] — the one Claude Code's
//!   OAuth client accepts.
//! - **Seamless (loopback) — dormant.** Without `manual`, on a loopback
//!   host [`start`] would 302 to the authorize page with a `redirect_uri`
//!   back to [`callback`] on this server and exchange with no copy-paste.
//!   But **Claude Code's OAuth client does not whitelist this server's
//!   custom loopback path** (it rejects the `redirect_uri`), so the
//!   dashboard never selects this channel. The code is kept intact behind
//!   that UI default for a future attempt at a correct loopback
//!   `redirect_uri` (e.g. the CLI's own `localhost`/`callback` format).
//!
//! Both consume the single-slot [`PendingClaudeLogin`] held on
//! [`DashboardState`] (CSRF `state` + PKCE verifier + the exact
//! `redirect_uri`, which the token endpoint requires verbatim at
//! exchange). All routes are admin-only. **Test / personal use only** —
//! never a deployed product auth mode; see [`mwe_core::oauth`].

use axum::Router;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::html;
use mwe_core::oauth;

use crate::auth::{AdminUser, SessionUser};
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::{DashboardState, PendingClaudeLogin};
use crate::ui::layout;

use super::llm_config;

/// Path the seamless loopback redirect comes back to (must exist as a
/// route below and match the `redirect_uri` recorded in the pending
/// login).
const CALLBACK_PATH: &str = "/dashboard/admin/claude-login/callback";

/// Sub-router for the Claude Code login flow. Mounted inside the
/// authenticated (admin-gated) tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/claude-login/start", post(start))
        .route("/admin/claude-login/callback", get(callback))
        .route("/admin/claude-login/paste", post(paste))
        .route("/admin/claude-login/logout", post(logout))
}

#[derive(Debug, Default, serde::Deserialize)]
struct StartForm {
    /// When `"1"`, force the out-of-band paste flow even on loopback.
    #[serde(default)]
    manual: Option<String>,
}

/// Begin a login: mint PKCE + CSRF state, stash the pending attempt, and
/// either 302 to the authorize page (seamless loopback) or render the
/// authorize link + paste box (out-of-band).
async fn start(
    State(state): State<DashboardState>,
    admin: AdminUser,
    headers: HeaderMap,
    HtmlForm(form): HtmlForm<StartForm>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let pkce = oauth::generate_pkce().map_err(|e| oauth_err(&e))?;
    let csrf = oauth::generate_state().map_err(|e| oauth_err(&e))?;

    let host = host_header(&headers);
    let want_manual = form.manual.as_deref() == Some("1");
    let seamless = !want_manual && host.as_deref().is_some_and(is_loopback_host);

    let redirect_uri = if seamless {
        // Safe: `seamless` is only true when `host` is Some.
        format!("http://{}{CALLBACK_PATH}", host.unwrap_or_default())
    } else {
        oauth::OOB_REDIRECT_URI.to_owned()
    };

    let authorize_url = oauth::build_authorize_url(&redirect_uri, &csrf, &pkce.challenge)
        .map_err(|e| oauth_err(&e))?;

    *state
        .claude_login
        .lock()
        .expect("claude_login mutex poisoned") = Some(PendingClaudeLogin {
        verifier: pkce.verifier,
        state: csrf,
        redirect_uri,
    });

    if seamless {
        Ok(Redirect::to(&authorize_url).into_response())
    } else {
        Ok(Html(render_manual_page(chrome, admin.session(), &authorize_url)).into_response())
    }
}

#[derive(Debug, serde::Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Seamless return channel: Claude redirects the browser here with
/// `code` + `state` (or an `error`). Exchange and land back on the
/// LLM-config page with a flash.
async fn callback(
    State(state): State<DashboardState>,
    admin: AdminUser,
    Query(q): Query<CallbackQuery>,
) -> Result<Response> {
    if let Some(err) = q.error {
        let detail = q.error_description.unwrap_or(err);
        return status_page(
            &state,
            &admin,
            "error",
            &format!("Claude Code authorization was declined: {detail}"),
        );
    }
    let (Some(code), Some(returned_state)) = (q.code, q.state) else {
        return status_page(
            &state,
            &admin,
            "error",
            "Claude Code callback was missing the code or state — start the login again.",
        );
    };
    finish_login(&state, &admin, &code, Some(&returned_state)).await
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
    *state
        .claude_login
        .lock()
        .expect("claude_login mutex poisoned") = None;
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
    // on the network exchange (std mutex is not held across `.await`).
    let pending = state
        .claude_login
        .lock()
        .expect("claude_login mutex poisoned")
        .take();
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
            &pending.redirect_uri,
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
            li { "Paste the code below and finish." }
        }
        form action="/dashboard/admin/claude-login/paste" method="post" {
            input type="text" name="pasted" placeholder="paste the code…" autocomplete="off" required;
            button type="submit" { "Finish login" }
        }
        p { a href="/dashboard/admin/llm-config" { "← back to LLM config" } }
    };
    layout::authenticated_page(chrome, "Claude Code login", session, &body)
}

fn host_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// True when `host` (a `Host` header value, with or without a port) names
/// a loopback interface — the precondition for the seamless redirect.
fn is_loopback_host(host: &str) -> bool {
    let bare = host.strip_prefix('[').map_or_else(
        // host or host:port — strip a trailing :port.
        || host.split(':').next().unwrap_or(host),
        // bracketed IPv6 — [::1] or [::1]:port.
        |rest| rest.split(']').next().unwrap_or(rest),
    );
    matches!(bare, "localhost" | "127.0.0.1" | "::1")
}

fn oauth_err(e: &oauth::OauthError) -> DashboardError {
    DashboardError::Internal(format!("claude-login: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised() {
        for h in [
            "localhost",
            "localhost:8742",
            "127.0.0.1",
            "127.0.0.1:8742",
            "[::1]",
            "[::1]:8742",
        ] {
            assert!(is_loopback_host(h), "expected loopback: {h}");
        }
    }

    #[test]
    fn non_loopback_hosts_are_rejected() {
        for h in [
            "mwe.example.com",
            "mwe.example.com:8742",
            "10.0.0.5:8742",
            "192.168.1.10",
        ] {
            assert!(!is_loopback_host(h), "expected non-loopback: {h}");
        }
    }
}
