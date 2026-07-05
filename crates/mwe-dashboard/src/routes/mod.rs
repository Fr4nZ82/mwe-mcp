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
mod settings;
mod setup;
mod skills_view;
mod tokens;
mod two_factor;
mod users;
mod webagentoauth;
mod welcome;
mod wiki_view;

/// Build the dashboard router, ready to be mounted under `/dashboard`.
pub fn build(state: DashboardState) -> Router {
    let authenticated = Router::new()
        .route("/home", get(home::index))
        .route("/logout", post(logout::handler))
        .merge(users::router())
        .merge(groups::router())
        .merge(tokens::router())
        .merge(settings::router())
        .merge(two_factor::settings_router())
        .merge(proposals::router())
        .merge(wiki_view::router())
        .merge(smart_view::router())
        .merge(skills_view::router())
        .merge(bridges::dashboard_tab_router())
        .merge(dream::router())
        .merge(facts::router())
        .merge(media::router())
        .merge(help::router())
        .merge(briefing::router())
        .merge(chat::router())
        .merge(prompts::router())
        .merge(llm_config::router())
        .merge(claude_login::router())
        .merge(recall_settings::router())
        .merge(recall_traces::router())
        .merge(rem_settings::router())
        .merge(embedding_settings::router())
        .merge(email_settings::router())
        .merge(health::router())
        .merge(backup::router())
        .merge(welcome::router())
        .merge(keepalive::router())
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
        .with_state(state);

    public.merge(authenticated)
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
    webagentoauth::public_router().with_state(state)
}
