// SPDX-License-Identifier: AGPL-3.0-or-later
//! Route assembly: the public tree (no auth) and the authenticated
//! tree are merged into a single `Router` that the caller nests under
//! `/dashboard` next to `/mcp`.

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};

use crate::auth::session::refresh_session_layer;
use crate::state::DashboardState;

mod auth_link;
mod backup;
mod bridges;
mod briefing;
mod chat;
mod cite;
mod claude_login;
pub(crate) mod demo;
mod dream;
mod email_settings;
mod embedding_settings;
mod facts;
mod health;
mod keepalive;
mod smart_view;

/// Re-exported so [`crate::BUNDLED_PROMPTS`] can reference the
/// agentic-chat-panel system prompt without leaking the rest of the
/// `chat` route module's internals.
pub use chat::BUNDLED_AGENTIC_PROMPT_MD;
mod groups;
mod help;
mod home;
mod invitations;
mod llm_config;
mod login;
mod logout;
mod media;
mod password_reset;
mod prompts;
mod proposals;
mod recall_settings;
mod recall_traces;
mod redirect;
mod rem_settings;
mod sections_view;
mod server_settings;
mod settings;
mod setup;
mod skills_view;
mod tokens;
mod training_spool;
mod two_factor;
mod users;
mod webagentoauth;
mod welcome;
mod wiki_view;

/// Build the dashboard router, ready to be mounted under `/dashboard`.
pub fn build(state: DashboardState) -> Router {
    let frozen = state.config.read_only;

    let mut authenticated = Router::new()
        .route("/home", get(home::index))
        .route("/logout", post(logout::handler))
        .merge(settings::router())
        .merge(proposals::router())
        .merge(wiki_view::router())
        .merge(smart_view::router())
        .merge(skills_view::router())
        .merge(bridges::dashboard_tab_router())
        .merge(facts::router())
        .merge(sections_view::router())
        .merge(media::router())
        .merge(help::router())
        .merge(briefing::router())
        .merge(chat::router())
        .merge(recall_traces::router())
        .merge(health::router())
        .merge(keepalive::router());

    // The consoles that exist only to change things. On a frozen
    // deployment every control on these pages is refused, and a page
    // whose whole content is dead controls is worse than a page that is
    // not there: it invites a stranger to try. So they are not mounted at
    // all — see [`crate::read_only`], and keep this list in step with the
    // admin block of the top nav (`ui::layout`), which hides the same
    // entries.
    if !frozen {
        authenticated = authenticated
            .merge(users::router())
            .merge(groups::router())
            .merge(tokens::router())
            .merge(two_factor::settings_router())
            .merge(dream::router())
            .merge(prompts::router())
            .merge(llm_config::router())
            .merge(claude_login::router())
            .merge(recall_settings::router())
            .merge(rem_settings::router())
            .merge(training_spool::router())
            .merge(embedding_settings::router())
            .merge(email_settings::router())
            .merge(server_settings::router())
            .merge(backup::router())
            // The profile wizard's whole job is to write a person's first
            // facts. On a frozen instance the identities are already
            // seeded, so nobody should be sent here — and `home` no
            // longer redirects to it in this mode.
            .merge(welcome::router());
    }

    let authenticated = authenticated
        .layer(from_fn_with_state(state.clone(), refresh_session_layer))
        .with_state(state.clone());

    let public = Router::new()
        .route("/", get(redirect::root))
        .route("/setup", get(setup::form).post(setup::submit))
        .route("/login", get(login::form).post(login::submit))
        // Single-use magic-link redemption (0032). Anonymous like `cite`:
        // it verifies + burns the link token, sets the session cookie,
        // and redirects to the deep-link. Must sit OUTSIDE the auth layer.
        .merge(auth_link::router())
        .merge(invitations::router())
        // Self-service password recovery (roadmap 28). Public like
        // `accept-invite`: the one-shot `password_resets` token is the
        // guard, so it must sit OUTSIDE the auth layer.
        .merge(password_reset::router())
        // 2FA login challenge (roadmap 28). Public: it sits between a
        // verified password and the session mint, holding state in
        // `pending_2fa` keyed by an opaque cookie — no session yet.
        .merge(two_factor::challenge_router())
        // Citation-handle resolver. Anonymous on
        // purpose — auth fires on the destination `/dashboard/wiki/...`
        // page. Mounted in the dashboard public tree as the discoverable
        // alias `/dashboard/cite/:bi_id`; the canonical short form
        // `/cite/:bi_id` is mounted by `mwe-mcp-server` at the root.
        .merge(cite::router())
        // `webagentoauth` consent step (roadmap 19c). Mounted in the public tree
        // so it can verify the session itself and bounce to /dashboard/login?next=
        // when absent, rather than the middleware's context-less redirect — but it
        // still sits under /dashboard, where the session cookie (Path=/dashboard)
        // is sent. The public discovery/DCR/token endpoints live at the root via
        // `webagentoauth_public_router`.
        .merge(webagentoauth::consent_router())
        .merge(crate::assets::router())
        .with_state(state.clone());

    // The passwordless entrance, mounted only under the demo
    // configuration — so on every normal installation `POST
    // /demo/enter` is a `404`, not a `403`: there is no door to refuse
    // at. Public, because becoming somebody is how a visitor arrives.
    let public = if state.config.demo_entrance_enabled() {
        public.merge(demo::router().with_state(state.clone()))
    } else {
        public
    };

    // The freeze goes over **both** halves, and last, so it sees every
    // route in the tree — including the public ones (`/setup`,
    // `/accept-invite`, `/reset-password`, the OAuth consent) that sit
    // outside the session layer by design and would otherwise be the way
    // in. Inert unless `instance.read_only` is set.
    public
        .merge(authenticated)
        .layer(from_fn_with_state(state, crate::read_only::guard))
}

/// Standalone router exposing only the `/cite/:bi_id` resolver.
///
/// Mounted at the root of the HTTP tree (alongside `/dashboard`,
/// `/mcp`, `/skills`, `/connect`) so the canonical short URL is the
/// path the smart consumer hands to the user. Shares the same handler
/// as the in-dashboard alias above so the two mount points cannot
/// drift in behaviour.
pub fn cite_router(state: DashboardState) -> Router {
    cite::router().with_state(state)
}

/// Public, anonymous bridge-distribution router.
///
/// Serves the product front page (`/`), the bridge catalog (`/bridges`,
/// `/bridges/:consumer`), and the self-contained installers
/// (`/bridges/:consumer/install.{sh,ps1,md}`).
///
/// Stateless and mounted at the **root** of the HTTP tree by
/// `mwe-mcp-server` (next to [`cite_router`]) so the front page is `/`
/// and the install command is a clean `<origin>/bridges/<consumer>/…`.
/// See [`bridges`] for why every route here is unauthenticated.
pub fn public_site_router() -> Router {
    bridges::public_site_router()
}

/// Public `webagentoauth` OAuth router (roadmap 19).
///
/// Discovery + Dynamic Client Registration + token endpoint, mounted at the
/// **root** of the HTTP tree by `mwe-mcp-server` so the `.well-known` paths and
/// the OAuth endpoints are not hidden under `/dashboard`. The matching consent
/// step (`/dashboard/webagentoauth/authorize`) lives in the authenticated tree
/// above.
pub fn webagentoauth_public_router(state: DashboardState) -> Router {
    // Same freeze as the dashboard tree: registration and the token
    // endpoint hand out credentials, which a shown instance does not do.
    // Discovery is a `GET` and keeps answering.
    webagentoauth::public_router()
        .layer(from_fn_with_state(state.clone(), crate::read_only::guard))
        .with_state(state)
}
