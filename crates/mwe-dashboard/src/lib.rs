// SPDX-License-Identifier: AGPL-3.0-or-later
//! `mwe-dashboard` — the built-in web dashboard for `mwe-mcp`.
//!
//! The dashboard is the whole operator-facing surface of mwe-mcp: it
//! bootstraps the single admin, invites regular users over single-use
//! `UUIDv7` links, manages groups and consumer-delegation-aware tokens —
//! and, beside that, browses the memory (wikis, pages, facts, sections),
//! runs the operative chat, shows the recall traces, the model usage and
//! spend, the dream history, and every engine setting an operator edits.
//!
//! Stack: Axum 0.7 + Maud 0.26 server-side templates +
//! `axum-extra` cookie jar + `argon2` PHC + `mwe-core::jwt` for the
//! sliding-TTL session cookie. No client-side framework: plain
//! `<form method="POST">` everywhere, with a handful of small vanilla
//! scripts (`ui.js`, `chat.js`, …) for the panels that need one, so the
//! surface stays reviewable.
//!
//! Auth model summary (see [`crate::auth::session`] for the full story):
//! the session cookie holds the **same JWT shape** as MCP-local tokens,
//! signed with the same `MWE_TOKEN_SECRET`, distinguished
//! only by its 60-minute sliding TTL and `device_label = "dashboard-session"`.
//! Every authenticated request re-issues the cookie with a fresh `jti`
//! and `exp` — the "sliding" part lives in the middleware, not in the
//! token.

#![forbid(unsafe_code)]

pub mod agentic;
pub mod assets;
pub mod auth;
pub mod browser_guard;
pub mod email;
pub mod error;
pub mod form;
pub mod md_render;
pub mod ratelimit;
pub mod read_only;
pub mod reveal;
pub mod routes;
pub mod state;
pub mod twofa;
pub mod ui;

pub use state::{
    BackendForError, DashboardConfig, DashboardState, LlmBackendOverrides, MemoryHandles,
    RestartHandle,
};

use axum::Router;

/// Build the dashboard router, ready to be mounted under `/dashboard`
/// alongside the `/mcp` route tree.
///
/// All routes are namespaced under the `/dashboard` prefix at the call
/// site: this function returns a `Router` whose paths start at `/` (e.g.
/// `/setup`, `/users`), so the caller nests it via
/// `Router::new().nest("/dashboard", mwe_dashboard::router(state))`.
pub fn router(state: DashboardState) -> Router {
    routes::build(state)
}

/// Standalone router exposing the citation-handle resolver.
///
/// Mounts `GET /cite/:bi_id` by `mwe-mcp-server` at
/// the root of the HTTP tree so the canonical short URL — embeddable
/// in a consumer's replies — does not need a `/dashboard/` prefix. The same
/// handler is also reachable as `/dashboard/cite/:bi_id` via
/// [`router`] so it shows up in the dashboard public tree (URL bar
/// stays tidy when navigating in-app). Both mount points share one
/// handler so their behaviour cannot drift.
///
/// The resolver itself is **anonymous** by design — auth fires on the
/// destination `/dashboard/wiki/...` page, not here. See
/// [`routes::cite_router`] (private module) for the rationale.
pub fn cite_router(state: DashboardState) -> Router {
    routes::cite_router(state)
}

/// Public, anonymous **bridge-distribution** router, mounted at the root
/// of the HTTP tree by `mwe-mcp-server` (next to [`cite_router`]).
///
/// Serves the product front page (`/`), which sends a consumer to the
/// catalog and a human to the sign-in, the bridge catalog (`/bridges`,
/// `/bridges/:consumer`), and the self-contained installers
/// (`/bridges/:consumer/install.{sh,ps1,md}`). Every route is
/// unauthenticated by design — none of it is secret, and `curl … | sh`
/// must reach the installer from a box with no dashboard session. The
/// token is the one thing that stays in the admin-only dashboard.
pub fn public_site_router() -> Router {
    routes::public_site_router()
}

/// Public, anonymous **install-claim** router, mounted at the root of the
/// HTTP tree by `mwe-mcp-server` beside [`public_site_router`].
///
/// Serves `POST /bridges/nanoclaw/claim`, where the served nanoclaw
/// installer trades the single-use claim an admin minted on the Bridges
/// tab for the standard consumer token it writes into the fork. It is the
/// one bridge route that needs the database, which is why it is separate
/// from the stateless distribution surface, and it is unauthenticated
/// because the box running `curl … | sh` has no dashboard session — the
/// claim is the credential, and it is burned on first use.
pub fn bridge_claim_router(state: DashboardState) -> Router {
    routes::bridge_claim_router(state)
}

/// Public, anonymous **`webagentoauth`** OAuth router, mounted at
/// the root of the HTTP tree by `mwe-mcp-server` alongside [`public_site_router`].
///
/// Serves OAuth discovery (`/.well-known/oauth-authorization-server` +
/// `/.well-known/oauth-protected-resource`), Dynamic Client Registration
/// (`POST /webagentoauth/register`) and the token endpoint
/// (`POST /webagentoauth/token`). These must be reachable without a dashboard
/// session — they are the credential-issuing surface a remote MCP client (the
/// claude.ai web app) drives. The consent step that *does* require login is the
/// `/dashboard/webagentoauth/authorize` page in [`router`].
pub fn webagentoauth_public_router(state: DashboardState) -> Router {
    routes::webagentoauth_public_router(state)
}

/// Dashboard version string, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Compile-time-embedded bundled prompts shipped by `mwe-dashboard`.
///
/// Counterpart to [`mwe_core::prompts::BUNDLED`]: the binary chains
/// both slices and passes them to [`mwe_core::prompts::seed_bundled_into`]
/// during `mwe-mcp init`, so an operator gets every overridable
/// prompt materialised at `<workdir>/prompts/<name>.md` in one
/// idempotent pass. Today the dashboard ships exactly one prompt
/// (the agentic chat panel system prompt); the slice exists so
/// future dashboard-side prompts grow in a place the seeder already
/// reaches.
pub const BUNDLED_PROMPTS: &[(&str, &str)] =
    &[("agentic-chat-panel", routes::BUNDLED_AGENTIC_PROMPT_MD)];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dashboard-shipped bundled prompt must parse through the
    /// fence extractor. A bundled prompt missing its `text` code fence
    /// is `NoFencedBlock` at load time, and best-effort call sites
    /// degrade silently instead of failing loud. Mirror of mwe-core's
    /// `extract_fenced_text_parses_every_shipped_bundled`, over this
    /// crate's slice of the roster.
    #[test]
    fn extract_fenced_text_parses_every_shipped_bundled_prompt() {
        for (name, md) in BUNDLED_PROMPTS {
            let body = mwe_core::prompts::extract_fenced_text(md, name, "<bundled>")
                .unwrap_or_else(|err| panic!("bundled prompt `{name}` must parse: {err}"));
            assert!(
                !body.trim().is_empty(),
                "bundled prompt `{name}` parsed to an empty body"
            );
        }
    }

    /// The chat panel answers the operator in natural language, so it
    /// is a prose slot and must carry the `{locale}` placeholder — the
    /// same property `mwe_core::prompts::PROSE_REGISTRY` pins for the
    /// core roster, applied to this crate's slice. A dashboard prompt
    /// added here without a directive would answer in the language of
    /// its own examples, which are Italian.
    #[test]
    fn every_shipped_bundled_prompt_carries_the_locale_placeholder() {
        for (name, md) in BUNDLED_PROMPTS {
            let body = mwe_core::prompts::extract_fenced_text(md, name, "<bundled>")
                .expect("bundled prompt parses");
            assert!(
                body.contains(mwe_core::prompts::LOCALE_PLACEHOLDER),
                "dashboard prompt `{name}` has no `{}` in its body",
                mwe_core::prompts::LOCALE_PLACEHOLDER
            );
        }
    }
}
