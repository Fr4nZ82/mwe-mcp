// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin ACL-reveal — the dashboard-wide "show me everything" switch.
//!
//! An admin can flip a per-browser **reveal** cookie that makes the
//! dashboard bypass the per-sender ACL projection: memory-wiki pages
//! render every fragment (highlighted) instead of `[redacted]`, and the
//! [`crate::routes::facts`] table lists **every** user's facts so the
//! owner-or-admin ACL / validity / delete actions become reachable on
//! them. It never touches the MCP tool surface (those always honour the
//! ACL); it is gated server-side on the admin role — the cookie is only
//! honoured when [`SessionUser::is_admin`] is true (see [`active`]), so a
//! forged cookie on a non-admin session does nothing.
//!
//! The toggle lives on the **Settings** page ([`crate::routes::settings`])
//! as a single control that governs every reveal-aware surface; a new
//! surface opts in simply by consulting [`active`] and skipping its ACL
//! projection when it returns true. Documented in
//! the redaction-policy design note.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{Markup, html};
use serde::Deserialize;

use crate::auth::{AdminUser, SessionUser};
use crate::error::Result;
use crate::form::HtmlForm;
use crate::state::DashboardState;

/// Name of the cookie carrying the admin's reveal preference. Path is
/// scoped to `/dashboard` like the session cookie, so the preference
/// travels on every dashboard request but never to `/mcp/*`.
const COOKIE_NAME: &str = "mwe_admin_reveal";
const COOKIE_PATH: &str = "/dashboard";

/// The Settings page — where the toggle lives and where a toggle POST
/// returns to.
const SETTINGS_PATH: &str = "/dashboard/settings/me";

/// Is admin reveal active for this request?
///
/// True only when the user is an admin **and** the reveal cookie is set —
/// the server-side admin check is the authority, the cookie is merely the
/// on/off preference. Every reveal-aware surface routes through this
/// single predicate.
#[must_use]
pub fn active(user: &SessionUser, jar: &CookieJar) -> bool {
    user.is_admin && jar.get(COOKIE_NAME).is_some_and(|c| c.value() == "1")
}

/// Build the reveal cookie set to `on` (value `"1"`) or cleared.
fn cookie(state: &DashboardState, on: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, if on { "1" } else { "" });
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path(COOKIE_PATH);
    cookie.set_secure(state.config.cookie_secure);
    if !on {
        cookie.set_max_age(time::Duration::seconds(0));
    }
    cookie
}

/// Keep `return_to` strictly local: only same-origin dashboard paths are
/// allowed as a redirect target, so the toggle cannot be turned into an
/// open redirect. Anything else falls back to the Settings page.
fn safe_return_to(raw: &str) -> String {
    if raw.starts_with("/dashboard/") && !raw.starts_with("/dashboard//") {
        raw.to_owned()
    } else {
        SETTINGS_PATH.to_owned()
    }
}

/// Form body of `POST /dashboard/settings/reveal`.
#[derive(Debug, Default, Deserialize)]
pub struct ToggleForm {
    /// Present (value `"1"`) when the checkbox is ticked; absent means
    /// "turn it off".
    #[serde(default)]
    on: Option<String>,
    /// Where to send the admin back after toggling — validated by
    /// [`safe_return_to`].
    #[serde(default)]
    return_to: String,
}

/// `POST /dashboard/settings/reveal` — admin-only.
///
/// Set or clear the reveal cookie, then redirect back to the page the
/// admin toggled from (the Settings page by default). The [`AdminUser`]
/// extractor is the gate, so a non-admin session cannot reach this
/// handler at all.
pub async fn toggle(
    State(state): State<DashboardState>,
    admin: AdminUser,
    jar: CookieJar,
    HtmlForm(form): HtmlForm<ToggleForm>,
) -> Result<Response> {
    let on = form.on.as_deref() == Some("1");
    let dest = safe_return_to(&form.return_to);
    tracing::info!(
        actor = admin.sender_id(),
        reveal = on,
        "dashboard ACL-reveal toggled"
    );
    let jar = jar.add(cookie(&state, on));
    Ok((jar, Redirect::to(&dest)).into_response())
}

/// The admin-only checkbox that flips [`active`], rendered on the
/// Settings page. Submits on change (with a no-JS fallback button);
/// `return_to` brings the admin back to where they toggled it.
#[must_use]
pub fn checkbox(on: bool, return_to: &str) -> Markup {
    html! {
        form.acl-reveal-toggle method="post" action="/dashboard/settings/reveal" {
            input type="hidden" name="return_to" value=(return_to);
            label.acl-reveal-label {
                input type="checkbox" name="on" value="1" checked[on]
                    onchange="this.form.submit()";
                " Admin reveal — bypass ACL across the dashboard (wiki pages and "
                "the facts table show every fragment / fact, including those "
                "normally hidden, so the owner-or-admin fact actions reach them)"
            }
            noscript { " " button type="submit" { "Apply" } }
        }
    }
}

/// The prominent banner shown while reveal is active, so the admin never
/// mistakes a normally-hidden fragment for a public one.
#[must_use]
pub fn banner() -> Markup {
    html! {
        p.flash.acl-reveal-banner {
            "ACL bypass active — you are seeing every fragment / fact, including "
            "those normally hidden. Revealed-private content is highlighted; "
            "turn it off in Settings."
        }
    }
}
