// SPDX-License-Identifier: AGPL-3.0-or-later
//! The demo entrance — signing in as somebody, without a password.
//!
//! A memory product is not demonstrable without reading the **same page**
//! through two different people's eyes: the fragments that were
//! `[redacted]` fill in, the ones that were visible go away. Asking a
//! stranger to type an email and a password before they may see that
//! loses most of them at the door, and asking them to do it *twice*, to
//! compare, loses the rest.
//!
//! So a shown instance offers a row of buttons — *Enter as Bob*, *Enter
//! as Alice*, *Enter as Zoe* — on the sign-in page **and in the panel
//! frame**, so changing identity costs one click from wherever the
//! visitor happens to be reading. Coming back to the same page after the
//! switch is the whole comparison; a switcher that only lived on the
//! sign-in screen would make the visitor navigate back each time and the
//! demonstration would die of friction.
//!
//! # It exists only under the demo configuration
//!
//! Two config keys, both required
//! ([`mwe_core::config::InstanceConfig::demo_entrance_enabled`]):
//! `instance.read_only` **and** a non-empty `instance.demo_identities`.
//! A file that sets the second without the first does not start
//! (`ConfigError::DemoIdentitiesNeedReadOnly`) — a passwordless door on
//! a writable deployment must not be reachable by any combination of
//! settings, and a misconfiguration that quietly disabled itself would
//! be worse than one that stops the server.
//!
//! When it is off, [`router`] is never merged, so `POST /demo/enter`
//! answers **`404`**, not `403`: the route does not exist. That is the
//! property the tests pin, because "the button is not rendered" is a
//! curtain and this is a door that was never cut.
//!
//! # What a demo session is
//!
//! The smallest thing that makes the demonstration work:
//!
//! - the user id must be on the configured list — the form field is
//!   checked against it, never trusted;
//! - and must exist in `enrollment_users`, so a typo in the config mints
//!   nothing;
//! - the session is **never admin**, whatever the row says. A door with
//!   no password on it does not hand out the panel.
//!
//! Everything else — reading, ACL projection, recall — then behaves
//! exactly as it does for that person on any deployment, which is the
//! point: the visitor is not shown a mock-up of Bob, they are shown Bob.

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::REFERER;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use serde::Deserialize;

use crate::auth::session::issue_session_cookie;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::DashboardState;

/// Where a demo switch lands when the page it came from is unknown.
const FALLBACK: &str = "/dashboard/home";

/// The sign-in screen — a safe local path, and the one destination that
/// is never the right answer.
///
/// The switcher in the frame wants the page it was used on. The buttons
/// on the door are the *same* form, so they arrive with the door as
/// their `Referer`, and returning a visitor to the door lands them on a
/// screen that still offers the same three buttons and shows no sign
/// that anything happened. They click again, and again nothing appears
/// to change: the demonstration dies on its first click.
const SIGN_IN: &str = "/dashboard/login";

/// Routes for the passwordless entrance.
///
/// Merged by [`super::build`] **only** when
/// `InstanceConfig::demo_entrance_enabled` holds, so on every normal
/// installation this path does not exist.
pub fn router() -> Router<DashboardState> {
    Router::new().route("/demo/enter", post(enter))
}

/// Form body of `POST /dashboard/demo/enter`.
#[derive(Debug, Deserialize)]
pub struct EnterForm {
    /// The identity to become. Checked against the configured list; a
    /// value that is not on it is refused, so the field carries a choice
    /// and not an authorisation.
    user_id: String,
}

/// Human label for a configured identity: the id with its first
/// character upper-cased (`bob` → `Bob`).
///
/// The config carries ids, not display names, because the id is what the
/// ACLs and the wiki paths are written in — a demo visitor who sees
/// "Bob" on a button and `user:bob` on a fragment should be able to join
/// the two without being told.
#[must_use]
pub fn label_for(user_id: &str) -> String {
    let mut chars = user_id.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// Reduce a `Referer` to a **local dashboard path**, or nothing.
///
/// The origin is dropped rather than checked: whatever host the header
/// named, the redirect that comes out is a path on this server, so a
/// forged `Referer` can at worst choose which of our own pages the
/// visitor lands on. What it must never produce is an off-site or
/// protocol-relative target, hence the `/dashboard/` prefix and the
/// explicit `//` rejection.
fn safe_local(raw: &str) -> Option<String> {
    let path = raw
        .find("://")
        .and_then(|i| raw[i + 3..].find('/').map(|j| &raw[i + 3 + j..]))
        .unwrap_or(raw);
    let path = path.split(['?', '#']).next().unwrap_or("");
    (path.starts_with("/dashboard/") && !path.starts_with("/dashboard//")).then(|| path.to_owned())
}

/// The page a switch lands on: the one it was made from, or the panel.
///
/// Two questions that are **not** the same one, which is why they are
/// two steps. [`safe_local`] answers *may we send the browser there at
/// all* — a security reduction, and the reason a forged `Referer` cannot
/// leave the site. This answers *is there anything to see when we do*,
/// and the only path that fails it is [`SIGN_IN`].
fn destination(referer: Option<&str>) -> String {
    referer
        .and_then(safe_local)
        .filter(|path| path != SIGN_IN)
        .unwrap_or_else(|| FALLBACK.to_owned())
}

/// `POST /dashboard/demo/enter` — become one of the configured
/// identities, no credentials involved.
///
/// Lands back on the page the switch was made from, so comparing the
/// same page as two people is one click and no navigation. The page is
/// taken from `Referer`, which browsers send in full for a same-origin
/// form post; when it is missing, not a local dashboard path, or the
/// sign-in screen itself, the visitor goes to the panel rather than
/// anywhere a header could name ([`destination`]).
///
/// # Errors
///
/// [`DashboardError::Forbidden`] when `user_id` is not on the configured
/// list, [`DashboardError::NotFound`] when it is listed but no such user
/// exists.
pub async fn enter(
    State(state): State<DashboardState>,
    jar: CookieJar,
    headers: HeaderMap,
    HtmlForm(form): HtmlForm<EnterForm>,
) -> Result<Response> {
    let wanted = form.user_id.trim();
    if !state.config.demo_identities.iter().any(|id| id == wanted) {
        tracing::warn!(
            requested = wanted,
            "demo entrance: identity not on the configured list"
        );
        return Err(DashboardError::Forbidden);
    }
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users WHERE user_id = ?")
        .bind(wanted)
        .fetch_one(&state.pool)
        .await?;
    if known == 0 {
        // Configured but absent: an operator typo, not a visitor's doing.
        tracing::error!(
            requested = wanted,
            "demo entrance: configured identity is not in enrollment_users"
        );
        return Err(DashboardError::NotFound);
    }

    // Never admin, whatever `enrollment_users.is_admin` says.
    let cookie = issue_session_cookie(&state, wanted, false)?;
    let landing = destination(headers.get(REFERER).and_then(|v| v.to_str().ok()));
    tracing::info!(identity = wanted, "demo entrance: session issued");
    Ok((jar.add(cookie), Redirect::to(&landing)).into_response())
}

/// The row of entrance buttons.
///
/// Rendered twice: large on the sign-in page, and small in the panel
/// frame as the identity switcher. `current` suppresses the button for
/// whoever is already signed in — offering "Enter as Bob" to Bob is a
/// control that does nothing.
#[must_use]
pub fn buttons(identities: &[String], current: Option<&str>, compact: bool) -> Markup {
    let (list_class, button_class) = if compact {
        (
            "demo-switch flex items-center gap-1",
            "px-2 py-1 text-xs border border-border rounded bg-bg-3 text-text-dim \
             hover:text-phosphor hover:border-phosphor transition-colors",
        )
    } else {
        // Below `sm` the buttons stack full width; above it they sit in
        // one row. Letting them wrap instead leaves a lone third button
        // centred under two, which reads as a mistake rather than as a
        // layout — and this is the first screen a stranger sees.
        (
            "demo-enter flex flex-col sm:flex-row gap-3 justify-center items-stretch \
             sm:items-center my-6",
            "w-full sm:w-auto px-5 py-3 text-base font-bold border border-phosphor rounded \
             bg-bg-2 text-phosphor hover:bg-bg-3 hover:text-phosphor-bright transition-colors",
        )
    };
    html! {
        div class=(list_class) {
            @if compact {
                span class="text-xs text-text-dim mr-1" { "Look as:" }
            }
            @for id in identities {
                @if current != Some(id.as_str()) {
                    // `display: contents` so each form disappears from the
                    // layout and its button becomes a direct flex child —
                    // otherwise three wrapper elements would collapse the
                    // gap and the full-width stacking.
                    @let label = if compact {
                        label_for(id)
                    } else {
                        format!("Enter as {}", label_for(id))
                    };
                    form method="post" action="/dashboard/demo/enter" class="contents" {
                        input type="hidden" name="user_id" value=(id);
                        button type="submit" class=(button_class) { (label) }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_upper_cases_only_the_first_character() {
        assert_eq!(label_for("bob"), "Bob");
        assert_eq!(label_for("mcAllister"), "McAllister");
        assert_eq!(label_for(""), "");
    }

    /// The destination that comes out is always a path on this server —
    /// never an origin, never protocol-relative — so a forged `Referer`
    /// can pick one of our pages and nothing else.
    #[test]
    fn the_destination_is_always_a_local_dashboard_path() {
        assert_eq!(
            safe_local("https://demo.example/dashboard/wiki/bob"),
            Some("/dashboard/wiki/bob".to_owned())
        );
        assert_eq!(
            safe_local("/dashboard/facts?page=2"),
            Some("/dashboard/facts".to_owned())
        );
        // A hostile origin loses its host and keeps only our path…
        assert_eq!(
            safe_local("https://evil.example/dashboard/wiki/bob"),
            Some("/dashboard/wiki/bob".to_owned())
        );
        // …and anything that could leave the site at all is refused:
        // protocol-relative, outside `/dashboard/`, empty.
        assert_eq!(safe_local("//evil.example/dashboard/x"), None);
        assert_eq!(safe_local("/dashboard//evil.example"), None);
        assert_eq!(safe_local("https://evil.example/phish"), None);
        assert_eq!(safe_local("/mcp"), None);
        assert_eq!(safe_local(""), None);
    }

    /// The frame switcher's whole point: you come back to what you were
    /// reading, as somebody else.
    #[test]
    fn a_switch_returns_to_the_page_it_was_made_from() {
        assert_eq!(
            destination(Some(
                "https://demo.example/dashboard/wiki/bob/view/index.md"
            )),
            "/dashboard/wiki/bob/view/index.md"
        );
        assert_eq!(
            destination(Some("/dashboard/facts?page=2")),
            "/dashboard/facts"
        );
    }

    /// …and the one path that rule must not honour. The buttons on the
    /// door post the same form as the switcher in the frame, so they
    /// arrive with `SIGN_IN` as their `Referer`; obeying it would put a
    /// visitor who just clicked *Enter as Bob* back on a screen offering
    /// *Enter as Bob*, with nothing on it to say they are now signed in.
    #[test]
    fn entering_from_the_door_lands_in_the_panel_and_not_back_on_the_door() {
        assert_eq!(
            destination(Some("http://127.0.0.1:8760/dashboard/login")),
            FALLBACK
        );
        assert_eq!(destination(Some("/dashboard/login")), FALLBACK);
        assert_eq!(
            destination(Some("/dashboard/login?next=/dashboard/facts")),
            FALLBACK
        );
        // The pre-existing fallbacks are unchanged.
        assert_eq!(destination(None), FALLBACK);
        assert_eq!(destination(Some("https://evil.example/phish")), FALLBACK);
    }
}
