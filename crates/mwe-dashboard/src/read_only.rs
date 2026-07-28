// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-only mode — the deployment is shown, not operated.
//!
//! Turned on by `mwe-mcp.config.yaml > instance.read_only`, which like
//! the rest of the `instance:` section has no dashboard editor: it is the
//! machine operator's switch, not the panel admin's.
//!
//! # It is not "refuse POST"
//!
//! Half the point of an instance you show to other people is reading the
//! **same page** through one person's eyes and then through another's, so
//! signing in, signing out and changing identity have to keep working —
//! and those write session state by their nature. A mode that banned
//! writes at the transport would take the demonstration with it.
//!
//! So the refusal is about *substance*, not about HTTP verbs: nothing may
//! change **memory** (facts, wiki pages, comments, proposals, dreams) or
//! **configuration** (users, groups, tokens, prompts, every YAML editor).
//! Identity, reading and navigation are untouched. [`ALLOWED_WRITES`] is
//! that list, written out by hand and short enough to read in one go.
//!
//! # Shut the door, then hide the handle — in that order
//!
//! [`guard`] is the door: one middleware over the whole dashboard tree,
//! refusing by path rather than by module, so a route added tomorrow in a
//! module nobody remembers is refused by default. Hiding controls is the
//! second, separate job ([`hides_writes`]): a button that returns an
//! error on an instance a stranger is looking at is worse than no button,
//! but hiding alone would be a curtain — the routes would still be there.
//!
//! The consoles that exist **only** to change things (users, tokens,
//! prompts, the LLM / recall / REM / backup editors, the Dream console)
//! are not merged into the router at all in this mode: a page whose every
//! control is gone is not a page worth reaching, and a route that does
//! not exist cannot be found by anybody.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

use crate::state::DashboardState;

/// The requests a frozen deployment still accepts, by exact path
/// (dashboard-relative — the tree is nested under `/dashboard`).
///
/// Everything here is identity or session: it changes **who you are**,
/// never what the instance holds. Read them as the answer to "what does a
/// visitor still need in order to look around as somebody?".
///
/// - `/login`, `/logout`, `/2fa` — sign in, sign out, and the one-time
///   code between the two. Switching identity is logging out and back in,
///   which is the whole demonstration.
/// - `/session/keepalive` — refreshes the sliding session cookie so a
///   long read does not lapse mid-page.
/// - `/settings/reveal` — sets a per-browser cookie and nothing else. No
///   server state changes, so it is not a write; whether an admin may
///   turn reveal on at all is the separate
///   `instance.admin_reveal_locked` switch ([`crate::reveal`]).
///
/// Deliberately **absent**, though they are all "just" session or
/// credential paths: `/setup` (mints the first admin), `/accept-invite`,
/// `/reset-password`, `/forgot-password` (writes a token row and sends
/// mail), and the whole `webagentoauth` credential-issuing surface. Those
/// create identities or hand out capabilities; a frozen instance hands
/// out neither.
pub const ALLOWED_WRITES: &[&str] = &[
    "/login",
    "/logout",
    "/2fa",
    "/session/keepalive",
    "/settings/reveal",
];

/// The passwordless entrance ([`crate::routes::demo`]) — the one path
/// whose verdict depends on configuration rather than on the list above.
///
/// It is the same class as `/login` (it mints a session and nothing
/// else) and it is only *routed* under the demo configuration. But a
/// blanket entry would make it answer differently from a path that does
/// not exist on a frozen instance that has no demo cast — `303` to the
/// sign-in page instead of the guard's `403` — which is exactly the tell
/// that "the route is mounted and merely refusing". So the guard refuses
/// it unless the entrance is really configured, and the door stays
/// indistinguishable from a wall.
pub const DEMO_ENTER: &str = "/demo/enter";

/// Mutating `GET`s that must still be refused.
///
/// The guard's rule of thumb is "safe methods pass", which holds for
/// every dashboard route but these: they are `GET` only because they are
/// redirect targets a browser is sent to, and they store credentials.
/// `/auth/link` is the other mutating `GET` and is deliberately *not*
/// here — it redeems a magic link into a session, which is identity.
pub const REFUSED_READS: &[&str] = &["/admin/claude-login/callback"];

/// Message shown to a human, and logged, when the mode refuses.
pub const REFUSAL: &str =
    "This instance is read-only: memory and configuration cannot be changed here.";

/// Would this request change memory or configuration?
///
/// Path-first and allow-list shaped on purpose: a new write route is
/// refused by default and its author has to come here to exempt it, which
/// is the direction the mistake should point.
///
/// `demo_entrance` is whether the passwordless door is configured; see
/// [`DEMO_ENTER`] for why that one path is not simply on the list.
#[must_use]
pub fn refuses(method: &Method, path: &str, demo_entrance: bool) -> bool {
    if REFUSED_READS.contains(&path) {
        return true;
    }
    // `GET` / `HEAD` / `OPTIONS`: reading and navigation, the two things
    // this mode exists to keep.
    if method.is_safe() {
        return false;
    }
    if path == DEMO_ENTER {
        return !demo_entrance;
    }
    !ALLOWED_WRITES.contains(&path)
}

/// The middleware that freezes the tree.
///
/// Layered over the whole dashboard router (public **and** authenticated
/// halves) so nothing is frozen "per module": the guard sees the path
/// after nesting has stripped `/dashboard`, matches it against
/// [`ALLOWED_WRITES`], and refuses everything else with `403`.
pub async fn guard(State(state): State<DashboardState>, request: Request, next: Next) -> Response {
    if state.config.read_only {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        if refuses(&method, &path, state.config.demo_entrance_enabled()) {
            tracing::info!(%method, %path, "read-only instance: request refused");
            return (StatusCode::FORBIDDEN, REFUSAL).into_response();
        }
    }
    next.run(request).await
}

/// Should the dashboard hide the controls it would refuse?
///
/// The same flag as [`guard`], read at every render site that owns a
/// write control. Kept as a named predicate rather than
/// `state.config.read_only` inline so the *reason* is greppable and a
/// future second reason (a per-user freeze, say) has one place to land.
#[must_use]
pub const fn hides_writes(state: &DashboardState) -> bool {
    state.config.read_only
}

/// The standing notice in the page frame.
///
/// Deliberately plain and always present rather than dismissible: a
/// visitor who cannot find the button they expected should not have to
/// wonder whether it is them.
#[must_use]
pub fn banner() -> Markup {
    html! {
        p class="read-only-banner text-xs text-text-dim border border-border rounded px-3 py-1.5 bg-bg-2" {
            "Read-only instance — you can read and navigate everything you are allowed "
            "to see, and change nothing. Memory and settings are frozen."
        }
    }
}

/// The line that replaces a write control where its absence would
/// otherwise read as a bug.
///
/// Use it where a section would collapse to nothing — an editor page, a
/// form that was the only content of its panel. Where the control sits
/// among other things (a row of buttons, a toolbar), just leave it out:
/// a sentence per missing button is worse than the missing buttons.
#[must_use]
pub fn notice() -> Markup {
    html! {
        p.muted.read-only-notice { (REFUSAL) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The third argument is "is the demo entrance configured"; the
    /// tests that do not exercise it pass `false`, the plainer posture.
    fn refused(method: &Method, path: &str) -> bool {
        refuses(method, path, false)
    }

    #[test]
    fn reading_and_navigation_always_pass() {
        for path in ["/home", "/wiki/franz", "/facts", "/admin/recall-traces"] {
            assert!(
                !refused(&Method::GET, path),
                "GET {path} must pass in read-only mode"
            );
        }
    }

    #[test]
    fn identity_still_works_but_credential_minting_does_not() {
        for path in ALLOWED_WRITES {
            assert!(
                !refused(&Method::POST, path),
                "POST {path} is the identity surface and must pass"
            );
        }
        // Same family by shape (session, credentials, "just logging in"),
        // and all refused: they create identities or hand out capabilities.
        for path in [
            "/setup",
            "/forgot-password",
            "/accept-invite/0197fa00-0000-7000-8000-000000000001",
            "/reset-password/abc",
            "/webagentoauth/authorize",
        ] {
            assert!(
                refused(&Method::POST, path),
                "POST {path} mints an identity or a capability and must be refused"
            );
        }
    }

    #[test]
    fn a_mutating_get_is_refused_even_though_get_is_safe() {
        assert!(refused(&Method::GET, "/admin/claude-login/callback"));
        // …and the other mutating GET is not, because it is identity.
        assert!(!refused(&Method::GET, "/auth/link"));
    }

    #[test]
    fn an_unknown_write_route_is_refused_by_default() {
        assert!(refused(&Method::POST, "/some/route/added/next/year"));
    }

    /// The passwordless door passes only where it is actually cut. On a
    /// frozen instance with no demo cast it is refused like any other
    /// unknown write, so it answers the same as a path that does not
    /// exist instead of betraying itself with a different code.
    #[test]
    fn the_demo_entrance_passes_only_where_it_is_configured() {
        assert!(!refuses(&Method::POST, DEMO_ENTER, true));
        assert!(refuses(&Method::POST, DEMO_ENTER, false));
        assert_eq!(
            refuses(&Method::POST, DEMO_ENTER, false),
            refuses(&Method::POST, "/no-such-route", false),
        );
    }
}
