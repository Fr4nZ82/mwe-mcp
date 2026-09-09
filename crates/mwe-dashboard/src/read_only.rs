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
//! The operator's consoles — users, tokens, prompts, the LLM / recall /
//! REM / backup editors, the Dream console — stay **mounted on every
//! deployment, frozen or not**: a memory server is an operator's tool as
//! much as a reader's, and an instance that hides them shows half the
//! product. What a visitor gets is the real surface, inert. The
//! consequence is about content rather than routing: on a frozen instance
//! those pages are readable by whoever walks in, so putting one on the
//! public internet means having looked at what they print.
//!
//! # The second list: what may reach a model
//!
//! "Safe methods pass" is right for memory and wrong for money. A
//! `GET` that spends is still a `GET`, and a frozen instance is the one
//! we hand to strangers: **no model is ever called there** (founder,
//! 2026-09-09). Four reads were doing it — the two chat primers behind
//! the proposals, the per-slot reachability probe the Health page fetches
//! on its own as the page paints, and the model listing the LLM console
//! fetches to fill its picker.
//!
//! So [`COSTLY_ROUTES`] is a second list, and it is not about writing:
//! every path on it is refused **whatever its method**, before the
//! safe-method rule is reached. `*` stands for one path segment, which is
//! what [`ALLOWED_WRITES`] cannot say and what a per-proposal address
//! needs.
//!
//! Method-independence is what decides membership: a path belongs here
//! only when **every** method on it reaches a model. Three do not, and
//! [`COSTLY_ROUTES`] names them: their `GET` renders a page a frozen
//! instance keeps on purpose, and only their mutating half calls a model,
//! which the write rule already refuses.
//!
//! The list cannot defend itself: an unlisted read passes by default,
//! which is the wrong direction for this one. Two tests hold it instead —
//! one naming every entry and what it would otherwise spend, one reading
//! the crate's own sources so a module that *starts* reaching for a model
//! cannot do it quietly.

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

/// The routes that can reach a **model**, as path patterns.
///
/// Refused on a frozen deployment whatever the method, because the cost
/// of a call does not depend on the verb that started it. `*` matches
/// exactly one non-empty segment; there is no prefix wildcard, so a new
/// route under one of these prefixes is a decision somebody makes here
/// rather than one they inherit.
///
/// - `/proposals/*/open-in-chat` and `/proposals/in-flight/chat-turn` —
///   both run the agentic loop on the `operator_chat` slot to summarise
///   what is pending. The proposals page reads the same rows with SQL and
///   no model, which is what a frozen instance shows instead.
/// - `/admin/health/llm-slots` — one round-trip **per slot**, so six
///   calls, and `ui.js` fetches it by itself as the Health page paints:
///   nobody has to click anything.
/// - `/admin/ollama-models` — a listing rather than a completion, and
///   free against a local Ollama, but it is still a stranger reaching the
///   model host. `llm-config.js` degrades to an empty picker when it is
///   refused.
/// - `/chat/agentic`, `/dream/light`, `/dream/compile`, `/dream/full`,
///   `/facts/*/edit/submit` — writes, and already refused as writes. They
///   are named here anyway because a path on this list cannot be bought
///   back by an [`ALLOWED_WRITES`] exemption, and one of them is the
///   largest spend on the surface.
///
/// Deliberately **absent**, and each for the same reason: their `GET`
/// renders a page a frozen instance keeps on purpose, and only their
/// mutating half reaches a model, which the write rule refuses on its
/// own. A method-independent list cannot hold them without taking the
/// page with them.
///
/// - `/chat` — `GET` paints the chat page, `POST` runs a turn.
/// - `/welcome` — `GET` is the wizard, `POST` puts a person's first words
///   through the ingest.
/// - `/wiki/*/delete` — `GET` is the confirmation page, and the `POST`
///   starts a whole REM night when dissolving a wiki leaves facts with no
///   page (`crate::routes::wiki_view`).
///
/// Absent for a different reason: `/admin/llm-catalog/refresh` downloads
/// a catalogue of model names and prices and calls no model.
///
/// And absent because they are not language models at all, though they do
/// spend when the embedder is a remote one (the embedding backend in
/// `mwe_core::config`): `/facts/*/delete` and `/users/*/forget` re-embed
/// what they touch. Both are `POST`-only routes, so the write rule
/// already closes them on a frozen deployment and nothing more is needed
/// here; they are named so the next reader knows they were weighed rather
/// than missed.
pub const COSTLY_ROUTES: &[&str] = &[
    "/proposals/*/open-in-chat",
    "/proposals/in-flight/chat-turn",
    "/admin/health/llm-slots",
    "/admin/ollama-models",
    "/chat/agentic",
    "/dream/light",
    "/dream/compile",
    "/dream/full",
    "/facts/*/edit/submit",
];

/// Does `pattern` describe `path`?
///
/// Segment by segment, with `*` standing for exactly one segment that is
/// there. An empty segment never matches the wildcard, so `/proposals//`
/// cannot walk into a pattern by having nothing where a name should be.
fn matches_route(pattern: &str, path: &str) -> bool {
    let mut pattern_segments = pattern.split('/');
    let mut path_segments = path.split('/');
    loop {
        match (pattern_segments.next(), path_segments.next()) {
            (None, None) => return true,
            (Some("*"), Some(segment)) if !segment.is_empty() => {},
            (Some(wanted), Some(got)) if wanted == got => {},
            _ => return false,
        }
    }
}

/// Can this path reach a model? See [`COSTLY_ROUTES`].
#[must_use]
pub fn reaches_a_model(path: &str) -> bool {
    COSTLY_ROUTES
        .iter()
        .any(|pattern| matches_route(pattern, path))
}

/// Mutating `GET`s that must still be refused.
///
/// The rule of thumb after [`COSTLY_ROUTES`] has had its say is "safe
/// methods pass", and today every remaining dashboard `GET` earns it:
/// none of them stores anything. **Empty is not the same as absent**: a
/// `GET` that stores something — a redirect target a provider sends a
/// browser back to, carrying a credential — belongs here the day it is
/// written, or the freeze will wave it through on the strength of its
/// method. What a `GET` *spends* is the other list's question, not this
/// one's.
///
/// `/auth/link` is a mutating `GET` and is deliberately *not* here: it
/// redeems a magic link into a session, which is identity, and a frozen
/// instance still lets people in.
pub const REFUSED_READS: &[&str] = &[];

/// Message shown to a human, and logged, when the mode refuses.
pub const REFUSAL: &str =
    "This instance is read-only: memory and configuration cannot be changed here.";

/// The refusal for a request that would have called a model.
///
/// Its own sentence because "cannot be changed here" would be a lie: the
/// request changes nothing and is refused for what it would spend.
pub const REFUSAL_COSTLY: &str =
    "This instance is read-only: it never calls a language model, so this is not available here.";

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
    // Before the safe-method rule, because this list is about what a
    // request costs and not about what it changes.
    if reaches_a_model(path) {
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
            let costly = reaches_a_model(&path);
            tracing::info!(%method, %path, costly, "read-only instance: request refused");
            let sentence = if costly { REFUSAL_COSTLY } else { REFUSAL };
            return (StatusCode::FORBIDDEN, sentence).into_response();
        }
    }
    next.run(request).await
}

/// The still-live write paths, as a JS global for `read-only.js`.
///
/// The frozen chrome renders every write control and then disables it
/// ([`crate::ui::layout`]); this is the exemption list it consults, and
/// it is **generated from [`ALLOWED_WRITES`]** rather than restated in
/// the script. A hand-copied list in JavaScript would drift the first
/// time somebody exempts a path here, and it would drift silently — the
/// visible result being a control that looks usable and is not, or the
/// reverse.
///
/// Paths are emitted absolute (`/dashboard/…`) because that is the form
/// a form `action` takes in the rendered HTML; the guard sees them after
/// nesting has stripped the prefix.
#[must_use]
pub fn live_writes_js() -> String {
    let paths: Vec<String> = ALLOWED_WRITES
        .iter()
        .chain(std::iter::once(&DEMO_ENTER))
        .map(|p| format!("\"/dashboard{p}\""))
        .collect();
    format!("window.__mweLiveWrites=[{}];", paths.join(","))
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
        for path in ["/home", "/wiki/franz", "/facts", "/recall-traces"] {
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

    /// Redeeming a magic link is a mutating `GET`, and it passes: a
    /// frozen instance still lets people in. The point of the exception
    /// list is that admitting somebody is the *only* mutating `GET` we
    /// are willing to admit.
    #[test]
    fn the_one_mutating_get_that_passes_is_the_one_that_admits_somebody() {
        assert!(!refused(&Method::GET, "/auth/link"));
        assert!(
            REFUSED_READS.is_empty(),
            "a mutating GET was added to the list without a test saying why"
        );
    }

    #[test]
    fn an_unknown_write_route_is_refused_by_default() {
        assert!(refused(&Method::POST, "/some/route/added/next/year"));
    }

    /// Every route that can reach a model, named one by one with the
    /// address a request would really arrive on, and refused on both a
    /// safe method and a mutating one.
    ///
    /// The safe half is the point: each of these four `GET`s used to pass
    /// on the strength of its method while spending a model call behind
    /// it. The other five are writes and were already refused; they are
    /// asserted here so the one list stays the answer to "what can reach a
    /// model".
    #[test]
    fn every_route_that_reaches_a_model_is_refused_whatever_the_method() {
        // Left: the real address. Right: what it would have spent.
        let costly = [
            (
                "/proposals/0197fa00-0000-7000-8000-000000000001/open-in-chat",
                "an agentic turn on the operator_chat slot",
            ),
            (
                "/proposals/in-flight/chat-turn",
                "an agentic turn on the operator_chat slot",
            ),
            (
                "/admin/health/llm-slots",
                "one reachability probe per slot, six of them, fetched as the page paints",
            ),
            (
                "/admin/ollama-models",
                "a listing request to the model host",
            ),
            ("/chat/agentic", "an agentic turn"),
            ("/dream/light", "a whole light dream"),
            ("/dream/compile", "a whole compile pass"),
            ("/dream/full", "a whole REM night"),
            (
                "/facts/0197fa00-0000-7000-8000-000000000001/edit/submit",
                "an agentic turn on the operator_chat slot",
            ),
        ];
        for (path, spends) in costly {
            for method in [Method::GET, Method::POST] {
                assert!(
                    refused(&method, path),
                    "{method} {path} would have cost {spends}"
                );
            }
        }
        assert_eq!(
            costly.len(),
            COSTLY_ROUTES.len(),
            "a pattern was added to COSTLY_ROUTES without an address that exercises it"
        );
    }

    /// And the neighbours still pass, which is the half that would break
    /// first if the patterns grew a prefix wildcard.
    ///
    /// `/chat` and `/welcome` are the two that have to be read carefully:
    /// their `GET` renders a page a frozen instance keeps on purpose, and
    /// only their `POST` reaches a model — which the write rule refuses on
    /// its own.
    #[test]
    fn the_pages_beside_a_costly_route_still_open() {
        for path in [
            "/proposals",
            "/proposals/0197fa00-0000-7000-8000-000000000001",
            "/proposals/in-flight-count",
            "/chat",
            "/welcome",
            "/admin/health",
            "/admin/llm-config",
            "/dream",
            "/dream/status",
            "/dream/runs/7",
            "/facts/0197fa00-0000-7000-8000-000000000001/edit",
            "/wiki/alice/delete",
        ] {
            assert!(!refused(&Method::GET, path), "GET {path} must still open");
        }
        // The three whose write half is refused for being a write rather
        // than for what it costs, so the sentence a visitor gets is the
        // ordinary one. Their `GET` is a page, asserted above and here.
        for path in ["/chat", "/welcome", "/wiki/alice/delete"] {
            assert!(!reaches_a_model(path), "{path} is not on the costly list");
            assert!(!refused(&Method::GET, path), "GET {path} is a page");
            assert!(refused(&Method::POST, path), "POST {path}");
        }
    }

    /// The wildcard stands for one segment that is there, and for nothing
    /// else — not for a walked-in empty one, and not for two.
    #[test]
    fn the_wildcard_is_exactly_one_segment() {
        assert!(matches_route(
            "/proposals/*/open-in-chat",
            "/proposals/x/open-in-chat"
        ));
        assert!(!matches_route(
            "/proposals/*/open-in-chat",
            "/proposals//open-in-chat"
        ));
        assert!(!matches_route(
            "/proposals/*/open-in-chat",
            "/proposals/a/b/open-in-chat"
        ));
        assert!(!matches_route("/proposals/*/open-in-chat", "/proposals/x"));
        assert!(!matches_route(
            "/proposals/*/open-in-chat",
            "/proposals/x/open-in-chat/more"
        ));
    }

    /// Every source file of this crate that names one of `doors`, as its
    /// path under `src/`, sorted.
    ///
    /// Reads the crate's own tree at test time: that is what makes the
    /// audit below a net rather than a list somebody has to remember.
    /// This file is skipped because it names the doors in order to look
    /// for them.
    fn modules_touching(doors: &[&str]) -> Vec<String> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: Vec<String> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("the crate's own sources are readable") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let name = path
                    .strip_prefix(&root)
                    .expect("under src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                if name == "read_only.rs" {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("source is UTF-8");
                if doors.iter().any(|door| body.contains(door)) {
                    found.push(name);
                }
            }
        }
        found.sort();
        found
    }

    /// The net under [`COSTLY_ROUTES`], because the list cannot defend
    /// itself: a read is allowed by default, so a new route that calls a
    /// model would pass until somebody remembered this file.
    ///
    /// So the test reads the crate's own sources and asks which modules
    /// touch a model at all — by the handle type, by the chokepoint that
    /// resolves one, and by the three helpers a route uses to reach a
    /// model without ever naming one: the two chat submissions, and the
    /// call that hands a whole night to the Dream console. A module that
    /// starts doing any of that fails here until somebody adds it below
    /// **and** decides what its routes are.
    ///
    /// The needles are part of the contract: a third way into a model is a
    /// third needle, and the day somebody writes one this test is where
    /// they say so.
    #[test]
    fn no_module_reaches_a_model_without_this_file_knowing() {
        const DOORS: &[&str] = &[
            "LlmBackend",
            "backend_for",
            "OllamaBackend",
            "agentic_submission",
            "process_submission",
            "spawn_dream",
            // The embedder is not a language model, but a remote one is
            // billed the same way, so the same net watches it.
            "memory.embedder",
            "Arc<dyn Embedder>",
        ];
        // Every module that touches one of the doors, and the routes of
        // its that reach a model — `none` where the module holds the
        // machinery but mounts no route of its own.
        const AUDITED: &[(&str, &str)] = &[
            (
                "agentic.rs",
                "none — it is the tool dispatcher the chat routes run",
            ),
            ("lib.rs", "none — it re-exports the handle types"),
            (
                "state.rs",
                "none — it is the chokepoint that resolves a backend",
            ),
            ("routes/chat.rs", "POST /chat and POST /chat/agentic"),
            (
                "routes/dream.rs",
                "POST /dream/light, /dream/compile, /dream/full",
            ),
            ("routes/health.rs", "GET /admin/health/llm-slots"),
            ("routes/llm_config.rs", "GET /admin/ollama-models"),
            (
                "routes/facts.rs",
                "POST /facts/:id/edit/submit, and POST /facts/:id/delete for the embedder",
            ),
            (
                "routes/proposals.rs",
                "GET /proposals/:id/open-in-chat and GET /proposals/in-flight/chat-turn",
            ),
            (
                "routes/users.rs",
                "POST /users/:id/forget — it re-embeds what it removes",
            ),
            ("routes/welcome.rs", "POST /welcome"),
            (
                "routes/wiki_view.rs",
                "POST /wiki/:id/delete — it starts a whole REM night",
            ),
        ];

        let found = modules_touching(DOORS);
        let mut audited: Vec<&str> = AUDITED.iter().map(|(file, _)| *file).collect();
        audited.sort_unstable();
        assert_eq!(
            found, audited,
            "a module started reaching for a model, or stopped: decide what its routes are, \
             put them in COSTLY_ROUTES if any of them can be reached by a GET, and name it here"
        );
        // The description is half the entry, and the half that says what
        // was decided. Every `GET` named in one has to be on the costly
        // list, which is the whole invariant this file exists for: a safe
        // method that reaches a model is refused, and saying so in prose
        // without doing it would be worse than saying nothing.
        for (file, routes) in AUDITED {
            assert!(
                !routes.is_empty(),
                "{file} is audited with nothing said about its routes"
            );
            for word in routes
                .split_whitespace()
                .skip_while(|w| *w != "GET")
                .skip(1)
            {
                if !word.starts_with('/') {
                    continue;
                }
                // `:id` in a description stands for a real segment.
                let concrete = word
                    .split('/')
                    .map(|seg| if seg.starts_with(':') { "x" } else { seg })
                    .collect::<Vec<_>>()
                    .join("/");
                assert!(
                    reaches_a_model(&concrete),
                    "{file} says it serves GET {concrete}, which is not on COSTLY_ROUTES"
                );
            }
        }
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
