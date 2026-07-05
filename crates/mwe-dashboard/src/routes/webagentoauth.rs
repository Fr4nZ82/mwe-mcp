// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inbound OAuth 2.x authorization server — HTTP surface (the
//! `webagentoauth` flow).
//!
//! Lets a bridge-less MCP client (the claude.ai web app, or any spec-compliant
//! custom connector) connect as the user's own *Smart* consumer. The data layer
//! is [`mwe_core::oauth_server`]; this module is the transport:
//!
//! - **`/.well-known/oauth-authorization-server`** + **`/.well-known/oauth-protected-resource`**
//!   — discovery (RFC 8414 / RFC 9728). Public.
//! - **`POST /webagentoauth/register`** — Dynamic Client Registration (RFC 7591),
//!   public clients (PKCE, no secret). Public.
//! - **`POST /webagentoauth/token`** — `authorization_code` + `refresh_token`
//!   grants. Mints a short-lived **smart JWT** ([`jwt::issue`]) the existing
//!   `/mcp` middleware validates unchanged, plus a rotating refresh token. Public.
//! - **`GET`/`POST /dashboard/webagentoauth/authorize`** — the consent step. Sits
//!   behind the existing dashboard login (built separately, in the authenticated
//!   route tree).
//!
//! Everything here is mounted at the **root** of the HTTP tree (so the `.well-known`
//! paths and the OAuth endpoints are not hidden under `/dashboard`), except the
//! authorize consent page which needs the session cookie.
//!
//! No OAuth scopes: the token is "smart consumer owned by user X"; the wiki-level
//! ACL + the smart-wiki owner-match govern what it can touch (see
//! [`web-agent-oauth.md`](../../../../wiki/design-notes/web-agent-oauth.md)).

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Form, Host, OriginalUri, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use maud::html;
use mwe_core::jwt::{self, ConsumerClass, ConsumerProfile, TokenClaims};
use mwe_core::types::{WikiId, WikiSlug};
use mwe_core::{consumers, oauth_server, wiki_admin};
use serde::Deserialize;
use serde_json::json;

use crate::auth::session::SESSION_COOKIE_NAME;
use crate::state::DashboardState;
use crate::ui::{components, layout};

/// Access-token lifetime. Short because this is a public endpoint exposed to a
/// third-party client; the refresh token carries the long-lived grant.
const ACCESS_TTL: Duration = Duration::from_secs(60 * 60);
/// Refresh-token lifetime (30 days), rotated on every use.
const REFRESH_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);
/// Authorization-code lifetime — just the consent→token browser round-trip, so a
/// leaked code is near-useless.
const CODE_TTL: Duration = Duration::from_secs(60 * 5);

/// The claude.ai-flavored skill served for upload, so a web user can teach their
/// claude.ai how to use this memory (recall routing + keep-it-current behavior).
/// Bundled separately from the MCP `skill_fetch` catalog because a bridge-less
/// web client never auto-fetches that; this is the file the user drag-drops into
/// claude.ai's "Upload skill". Edit the source at `assets/claude-ai-skill.md`.
const CLAUDE_AI_SKILL_MD: &str = include_str!("../../assets/claude-ai-skill.md");

/// Derive the public origin (`scheme://host`) from the request `Host` header —
/// the same heuristic the bridge-onboarding pages use. `http` for loopback,
/// `https` otherwise.
fn origin_from_host(host: &str) -> String {
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}")
}

/// `true` when `uri` is an RFC 8252 loopback redirect — `http://` to `localhost`,
/// `127.0.0.1`, or `[::1]` on any port/path. A native MCP client (Claude Code and
/// other CLIs) catches the OAuth callback on an ephemeral loopback port; a hosted
/// web client (claude.ai) uses an `https` redirect. This is the signal that picks
/// the connection profile (loopback ⇒ `Local`) and relaxes [`redirect_matches`].
fn is_loopback_redirect(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("http://") else {
        return false;
    };
    let host = if let Some(after) = rest.strip_prefix('[') {
        // IPv6 literal: host runs up to the closing bracket (`[::1]:port/...`).
        match after.split(']').next() {
            Some(h) => h,
            None => return false,
        }
    } else {
        // host runs up to the port (`:`) or the path (`/`).
        rest.split(['/', ':']).next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Scheme-less host + path of a loopback redirect with the **port dropped** — the
/// key RFC 8252 §7.3 says a server must compare loopback redirects by, since the
/// OS assigns the callback port at runtime and it varies between attempts. Only
/// meaningful for inputs [`is_loopback_redirect`] already accepted.
fn loopback_key(uri: &str) -> String {
    let rest = uri.strip_prefix("http://").unwrap_or(uri);
    let slash = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(slash);
    // Drop the `:port`: for an IPv6 authority (`[::1]:port`) keep through the
    // bracket; otherwise keep the host before the first colon.
    let host = authority.find(']').map_or_else(
        || authority.split(':').next().unwrap_or(authority),
        |end| &authority[..=end],
    );
    format!("{host}{path}")
}

/// Whether a `presented` `redirect_uri` satisfies a `registered` one. Exact match
/// always passes; for two loopback redirects the port is ignored (RFC 8252 §7.3),
/// so an ephemeral callback port can't break the handshake. Non-loopback URIs are
/// never relaxed, and a loopback never matches a remote one.
fn redirect_matches(registered: &str, presented: &str) -> bool {
    registered == presented
        || (is_loopback_redirect(registered)
            && is_loopback_redirect(presented)
            && loopback_key(registered) == loopback_key(presented))
}

/// Pick the connection profile for a registered client. A native client (Claude
/// Code, any CLI) registers only loopback `redirect_uris` (RFC 8252) and gets the
/// full `Local` tool surface + the project-bound smart-consumer posture; a hosted
/// web client (claude.ai, https redirect) gets the trimmed `Web` surface. Resolved
/// from the registration at each mint, so it also holds across refresh with no
/// extra state to persist.
async fn profile_for_client(state: &DashboardState, client_id: &str) -> ConsumerProfile {
    match oauth_server::lookup_client(&state.pool, client_id).await {
        Ok(Some(c))
            if !c.redirect_uris.is_empty()
                && c.redirect_uris.iter().all(|u| is_loopback_redirect(u)) =>
        {
            ConsumerProfile::Local
        },
        // Unknown client, no redirects, or any non-loopback redirect: the more
        // restricted `Web` surface is the safe default.
        _ => ConsumerProfile::Web,
    }
}

/// Public OAuth router (no session): discovery + DCR + token. Mounted at the
/// root of the HTTP tree by `mwe-mcp-server`.
pub(super) fn public_router() -> Router<DashboardState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        // RFC 9728 path-insertion variant: for the protected resource at `/mcp`,
        // a client derives the metadata URL by inserting the well-known segment
        // before the resource path. Same document as the root one.
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route("/webagentoauth/register", post(register))
        .route("/webagentoauth/token", post(token))
        // The claude.ai-upload skill, downloadable so the consent page and the
        // Bridges tab can funnel the user to "Upload skill" in claude.ai.
        .route("/webagentoauth/skill.md", get(skill_md))
}

/// Serve the claude.ai skill as a downloadable markdown attachment.
async fn skill_md() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"mwe-mcp-memory.md\"",
            ),
        ],
        CLAUDE_AI_SKILL_MD,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Discovery (RFC 8414 / RFC 9728)
// ---------------------------------------------------------------------------

async fn authorization_server_metadata(Host(host): Host) -> Json<serde_json::Value> {
    let origin = origin_from_host(&host);
    Json(json!({
        "issuer": origin,
        "authorization_endpoint": format!("{origin}/dashboard/webagentoauth/authorize"),
        "token_endpoint": format!("{origin}/webagentoauth/token"),
        "registration_endpoint": format!("{origin}/webagentoauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [],
    }))
}

async fn protected_resource_metadata(Host(host): Host) -> Json<serde_json::Value> {
    let origin = origin_from_host(&host);
    Json(json!({
        "resource": format!("{origin}/mcp"),
        "authorization_servers": [origin],
    }))
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegisterReq {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

async fn register(State(state): State<DashboardState>, Json(req): Json<RegisterReq>) -> Response {
    if req.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "redirect_uris is required",
        );
    }
    let name = req.client_name.unwrap_or_default();
    match oauth_server::register_client(&state.pool, &name, &req.redirect_uris).await {
        Ok(reg) => (
            StatusCode::CREATED,
            Json(json!({
                "client_id": reg.client_id,
                "client_name": reg.client_name,
                "redirect_uris": reg.redirect_uris,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            })),
        )
            .into_response(),
        Err(e) => store_error(&e),
    }
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

async fn token(State(state): State<DashboardState>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_str() {
        "authorization_code" => token_authorization_code(&state, form).await,
        "refresh_token" => token_refresh(&state, form).await,
        other => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("grant_type {other:?} is not supported"),
        ),
    }
}

async fn token_authorization_code(state: &DashboardState, form: TokenForm) -> Response {
    let (Some(code), Some(client_id), Some(redirect_uri), Some(verifier)) = (
        form.code.as_deref(),
        form.client_id.as_deref(),
        form.redirect_uri.as_deref(),
        form.code_verifier.as_deref(),
    ) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "authorization_code grant needs code, client_id, redirect_uri and code_verifier",
        );
    };
    let grant =
        match oauth_server::consume_auth_code(&state.pool, code, client_id, redirect_uri, verifier)
            .await
        {
            Ok(g) => g,
            Err(e) => return store_error(&e),
        };
    let refresh_grant = oauth_server::RefreshGrant {
        client_id: grant.client_id,
        sender_id: grant.sender_id,
        consumer_id: grant.consumer_id,
        wiki_id: grant.wiki_id,
    };
    let refresh_token =
        match oauth_server::issue_refresh_token(&state.pool, &refresh_grant, REFRESH_TTL).await {
            Ok(t) => t,
            Err(e) => return store_error(&e),
        };
    mint_token_response(state, &refresh_grant, refresh_token).await
}

async fn token_refresh(state: &DashboardState, form: TokenForm) -> Response {
    let Some(refresh) = form.refresh_token.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token grant needs refresh_token",
        );
    };
    let (new_refresh, grant) =
        match oauth_server::rotate_refresh_token(&state.pool, refresh, REFRESH_TTL).await {
            Ok(pair) => pair,
            Err(e) => return store_error(&e),
        };
    mint_token_response(state, &grant, new_refresh).await
}

/// Mint the access JWT for `grant` and return the OAuth token response. `is_admin`
/// is re-resolved from `enrollment_users` here, never carried by the grant, so a
/// role change takes effect on the next token.
async fn mint_token_response(
    state: &DashboardState,
    grant: &oauth_server::RefreshGrant,
    refresh_token: String,
) -> Response {
    let is_admin = resolve_is_admin(state, &grant.sender_id).await;
    // device_label = the named connection (consumer_id); this is a smart token,
    // so it carries consumer_id for the cooperative lease and never delegates.
    let mut claims = TokenClaims::new(&grant.sender_id, &grant.consumer_id, "default", ACCESS_TTL);
    claims.is_admin = is_admin;
    claims.consumer_id = Some(grant.consumer_id.clone());
    claims.consumer_class = ConsumerClass::Smart;
    // Connection profile shapes the advertised tool surface: a native loopback
    // client (Claude Code, any CLI) gets the full `Local` catalog + the
    // project-bound smart-consumer posture; a hosted web client (claude.ai, https
    // redirect) gets the trimmed `Web` surface. Derived from the registered
    // redirect_uris (RFC 8252 loopback) so it also holds across refresh.
    claims.profile = profile_for_client(state, &grant.client_id).await;
    let access_token = match jwt::issue(&state.secret, &claims) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "webagentoauth: jwt mint failed");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "could not mint access token",
            );
        },
    };
    Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL.as_secs(),
        "refresh_token": refresh_token,
    }))
    .into_response()
}

/// Best-effort admin lookup. A vanished user resolves to non-admin rather than
/// failing the token exchange — the ACL still gates every read/write.
async fn resolve_is_admin(state: &DashboardState, sender_id: &str) -> bool {
    let row: std::result::Result<Option<i64>, sqlx::Error> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(sender_id)
            .fetch_optional(&state.pool)
            .await;
    matches!(row, Ok(Some(v)) if v != 0)
}

// ---------------------------------------------------------------------------
// OAuth error shaping (RFC 6749 §5.2)
// ---------------------------------------------------------------------------

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

/// Map a store error onto an OAuth wire error. Server-side failures get a generic
/// description; protocol failures keep their (non-leaky) `Display`.
fn store_error(e: &oauth_server::OAuthError) -> Response {
    let code = e.oauth_code();
    if code == "server_error" {
        tracing::error!(error = %e, "webagentoauth: store failure");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            code,
            "internal error during the OAuth exchange",
        );
    }
    oauth_error(StatusCode::BAD_REQUEST, code, &e.to_string())
}

// ---------------------------------------------------------------------------
// Consent step (`GET`/`POST /dashboard/webagentoauth/authorize`)
// ---------------------------------------------------------------------------
//
// Session-gated, so it lives under `/dashboard` where the session cookie
// (Path=/dashboard) is sent. It is mounted in the dashboard *public* tree and
// verifies the session itself (rather than relying on the auth middleware) so an
// un-authenticated visitor is bounced to `/dashboard/login?next=<this>` and
// returns here after signing in.

/// Consent router, merged into the dashboard route tree (→
/// `/dashboard/webagentoauth/authorize`).
pub(super) fn consent_router() -> Router<DashboardState> {
    Router::new().route(
        "/webagentoauth/authorize",
        get(authorize_get).post(authorize_post),
    )
}

/// OAuth authorize query (RFC 6749 §4.1.1 + PKCE).
#[derive(Debug, Deserialize)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
}

/// Resolve the logged-in user from the session cookie, or `None`.
async fn session_sender(state: &DashboardState, jar: &CookieJar) -> Option<String> {
    let cookie = jar.get(SESSION_COOKIE_NAME)?;
    jwt::verify(&state.secret, cookie.value(), &state.pool, &state.blacklist)
        .await
        .ok()
        .map(|c| c.sender_id)
}

/// Validate the authorize request against the registered client. On failure
/// returns a rendered error page (we never redirect an unvalidated client).
async fn validate_client(
    state: &DashboardState,
    p: &AuthorizeParams,
) -> std::result::Result<oauth_server::ClientRegistration, Response> {
    if p.response_type != "code" {
        return Err(consent_error(
            "Unsupported response_type — only `code` is supported.",
        ));
    }
    if p.code_challenge.trim().is_empty() || p.code_challenge_method != "S256" {
        return Err(consent_error(
            "A PKCE `code_challenge` with method S256 is required.",
        ));
    }
    match oauth_server::lookup_client(&state.pool, &p.client_id).await {
        Ok(Some(client))
            if client
                .redirect_uris
                .iter()
                .any(|u| redirect_matches(u, &p.redirect_uri)) =>
        {
            Ok(client)
        },
        Ok(Some(_)) => Err(consent_error(
            "The redirect_uri does not match this client's registration.",
        )),
        Ok(None) => Err(consent_error(
            "Unknown client_id — register the client first.",
        )),
        Err(e) => {
            tracing::error!(error = %e, "webagentoauth: client lookup failed");
            Err(consent_error("Internal error validating the client."))
        },
    }
}

fn consent_error(msg: &str) -> Response {
    let body = html! {
        (components::flash("error", msg))
        p.muted { "Close this window and retry the connection from your agent." }
    };
    (
        StatusCode::BAD_REQUEST,
        Html(layout::anonymous_page("Connection error", &body)),
    )
        .into_response()
}

async fn authorize_get(
    State(state): State<DashboardState>,
    jar: CookieJar,
    OriginalUri(uri): OriginalUri,
    Query(p): Query<AuthorizeParams>,
) -> Response {
    let client = match validate_client(&state, &p).await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let Some(sender) = session_sender(&state, &jar).await else {
        // Bounce through login, returning to this exact authorize URL after.
        let next = pct_encode(&uri.to_string());
        return Redirect::to(&format!("/dashboard/login?next={next}")).into_response();
    };
    render_consent(&p, &client, &sender, None)
}

/// POST body of the consent form: the chosen `decision`, the connection name,
/// and the OAuth params echoed back as hidden fields.
#[derive(Debug, Deserialize)]
struct ConsentForm {
    #[serde(default)]
    decision: String,
    #[serde(default)]
    connection: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    state: Option<String>,
}

async fn authorize_post(
    State(state): State<DashboardState>,
    jar: CookieJar,
    Form(f): Form<ConsentForm>,
) -> Response {
    let p = AuthorizeParams {
        response_type: f.response_type.clone(),
        client_id: f.client_id.clone(),
        redirect_uri: f.redirect_uri.clone(),
        code_challenge: f.code_challenge.clone(),
        code_challenge_method: f.code_challenge_method.clone(),
        state: f.state.clone(),
    };
    let client = match validate_client(&state, &p).await {
        Ok(c) => c,
        Err(page) => return page,
    };
    let Some(sender) = session_sender(&state, &jar).await else {
        return Redirect::to("/dashboard/login").into_response();
    };

    if f.decision != "approve" {
        return redirect_back(
            &f.redirect_uri,
            "error",
            "access_denied",
            f.state.as_deref(),
        );
    }

    // The connection name (default = the client name) shapes both the device
    // label and the dedicated wiki slug.
    let connection = connection_slug(&f.connection, &client.client_name);
    let Ok(slug) = WikiSlug::parse(&connection) else {
        return render_consent(
            &p,
            &client,
            &sender,
            Some("That connection name can't be used — try letters, digits and dashes."),
        );
    };

    let wiki_id =
        match ensure_dedicated_wiki(&state, &sender, &connection, &slug, &client.client_name).await
        {
            Ok(id) => id,
            Err(page) => return page,
        };

    // Register the consumer identity idempotently (smart consumers authenticate
    // as their human owner, so no system_user binding).
    if let Err(e) = consumers::register(
        &state.pool,
        &consumers::RegisterRequest {
            consumer_id: &connection,
            display_name: Some(&client.client_name),
            callback_url: None,
            kinds_subscribed: None,
            metadata: None,
            system_user_id: None,
        },
    )
    .await
    {
        tracing::error!(error = %e, "webagentoauth: consumer_register failed");
        return consent_error("Could not register the connection.");
    }

    let grant = oauth_server::AuthCodeGrant {
        client_id: f.client_id.clone(),
        redirect_uri: f.redirect_uri.clone(),
        code_challenge: f.code_challenge.clone(),
        sender_id: sender,
        consumer_id: connection,
        wiki_id: wiki_id.as_str().to_owned(),
    };
    match oauth_server::issue_auth_code(&state.pool, &grant, CODE_TTL).await {
        Ok(code) => redirect_back(&f.redirect_uri, "code", &code, f.state.as_deref()),
        Err(e) => {
            tracing::error!(error = %e, "webagentoauth: issue_auth_code failed");
            consent_error("Could not issue the authorization code.")
        },
    }
}

/// Forge the dedicated smart wiki on first connection (markerless, smart,
/// owned by `sender`), or reuse it on re-auth. Returns the bound wiki id, or a
/// rendered error page.
async fn ensure_dedicated_wiki(
    state: &DashboardState,
    sender: &str,
    connection: &str,
    slug: &WikiSlug,
    client_name: &str,
) -> std::result::Result<WikiId, Response> {
    let Some(memory) = state.memory.as_ref() else {
        return Err(consent_error(
            "This deployment has no memory subsystem to host the wiki.",
        ));
    };
    let Ok(parent) = WikiId::parse(sender) else {
        return Err(consent_error(
            "Your identity wiki id is invalid; cannot bind a wiki.",
        ));
    };
    let wiki_id = WikiId::child_of(&parent, slug);
    if memory.tree.locate(&wiki_id).is_ok() {
        return Ok(wiki_id);
    }
    let caller = wiki_admin::AdminCaller {
        sender_id: sender.to_owned(),
        consumer_id: Some(connection.to_owned()),
        consumer_class: ConsumerClass::Smart,
    };
    let req = wiki_admin::PushRequest {
        mode: wiki_admin::PushMode::Create,
        wiki_id: None,
        parent_wiki_id: Some(parent),
        slug: Some(connection.to_owned()),
        title: Some(format!("{client_name} ({connection})")),
        wiki_type: Some("agent".to_owned()),
        smart: true,
        project_id: None,
        pages: vec![wiki_admin::PushPage {
            path: "index.md".to_owned(),
            content: format!(
                "# {connection}\n\nDedicated memory wiki for the {client_name} connection.\n"
            ),
        }],
        deletes: Vec::new(),
        mark_processed: Vec::new(),
        expected_op_log_head: None,
    };
    match wiki_admin::push(
        &state.pool,
        &memory.tree,
        &caller,
        wiki_admin::ActorKind::SmartConsumer,
        req,
    )
    .await
    {
        Ok(_) => Ok(wiki_id),
        Err(e) => {
            tracing::error!(error = %e, wiki = %wiki_id, "webagentoauth: forge wiki failed");
            Err(consent_error("Could not create the dedicated wiki."))
        },
    }
}

fn render_consent(
    p: &AuthorizeParams,
    client: &oauth_server::ClientRegistration,
    sender: &str,
    err: Option<&str>,
) -> Response {
    let default_conn = connection_slug("", &client.client_name);
    let wiki_preview = format!("{sender}-{default_conn}");
    let body = html! {
        @if let Some(e) = err { (components::flash("error", e)) }
        p {
            "The application " strong { (client.client_name) } " wants to connect to your "
            "memory as a " strong { "smart agent" } ", acting as " strong { (sender) }
            ". It will read and write its own dedicated wiki."
        }
        form action="/dashboard/webagentoauth/authorize" method="post" {
            (components::text_field("connection", "Name this connection", "text", &default_conn, true))
            p.muted {
                "A dedicated wiki is created for it (e.g. " code { (wiki_preview) }
                ") — the name above shapes its id."
            }
            input type="hidden" name="client_id" value=(p.client_id);
            input type="hidden" name="redirect_uri" value=(p.redirect_uri);
            input type="hidden" name="code_challenge" value=(p.code_challenge);
            input type="hidden" name="code_challenge_method" value=(p.code_challenge_method);
            input type="hidden" name="response_type" value="code";
            @if let Some(s) = &p.state { input type="hidden" name="state" value=(s); }
            button type="submit" name="decision" value="approve" { "Approve" }
            " "
            button type="submit" name="decision" value="deny" { "Deny" }
        }
        hr;
        @if is_loopback_redirect(&p.redirect_uri) {
            p.muted {
                "This is a local CLI agent (e.g. Claude Code): it loads its mwe-mcp "
                "skill itself (via " code { "skill_fetch" } ") and recalls / saves on "
                "its own — nothing to upload. Just approve and it is connected."
            }
        } @else {
            p.muted {
                "One more step for the best results: teach claude.ai how to use this "
                "memory. "
                a href="/webagentoauth/skill.md" download="mwe-mcp-memory.md" {
                    "Download the mwe-mcp skill"
                }
                ", then in claude.ai open " strong { "Settings → Capabilities → Upload skill" }
                " and drop it in. It tells the assistant when to recall and to keep your "
                "memory current on its own. (Set the memory tools to auto-allow there so it "
                "doesn't ask every time.)"
            }
        }
    };
    Html(layout::anonymous_page("Approve connection", &body)).into_response()
}

/// Pick the connection slug: the user's input when given, else the client name;
/// slugified, with a stable fallback so it always parses as a `WikiSlug`.
fn connection_slug(input: &str, client_name: &str) -> String {
    let base = if input.trim().is_empty() {
        client_name
    } else {
        input
    };
    let s = slugify(base);
    if s.is_empty() { "agent".to_owned() } else { s }
}

/// Lowercase, collapse non-alphanumerics to single dashes, trim edge dashes —
/// yields a string that satisfies [`WikiSlug::parse`] (charset `[a-z0-9-]`, no
/// edge/double dash) whenever the result is non-empty.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Redirect back to the client's `redirect_uri` with one OAuth param plus the
/// echoed `state`. Values are percent-encoded; the separator adapts to whether
/// the redirect URI already carries a query string.
fn redirect_back(redirect_uri: &str, key: &str, value: &str, state: Option<&str>) -> Response {
    let mut query = format!("{key}={}", pct_encode(value));
    if let Some(s) = state {
        query.push_str("&state=");
        query.push_str(&pct_encode(s));
    }
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{redirect_uri}{sep}{query}")).into_response()
}

/// Minimal percent-encoder for query-string values (RFC 3986 unreserved set
/// kept, everything else `%XX`). Dependency-free — the dashboard pulls in no URL
/// crate, and the inputs here (a local path, an auth code, an opaque `state`)
/// don't warrant one.
fn pct_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            },
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use mwe_core::db;
    use mwe_core::jwt::{BlacklistCache, TokenSecret};
    use std::sync::Arc;
    use tower::ServiceExt;

    // Canonical PKCE S256 pair from RFC 7636 Appendix B — lets the tests bind a
    // challenge without pulling sha2/base64 into the dashboard crate.
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    async fn state() -> (DashboardState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let secret = TokenSecret::new(vec![0x11u8; 32]).expect("secret");
        let st = DashboardState::new(
            pool,
            secret,
            Arc::new(BlacklistCache::new()),
            Arc::new(mwe_core::delegations::DelegationCache::new()),
        );
        (st, dir)
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn discovery_advertises_endpoints() {
        let (st, _dir) = state().await;
        let app = public_router().with_state(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .header("host", "mem.example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["issuer"], "https://mem.example.org");
        assert_eq!(
            j["token_endpoint"],
            "https://mem.example.org/webagentoauth/token"
        );
        assert_eq!(j["code_challenge_methods_supported"][0], "S256");
    }

    #[tokio::test]
    async fn protected_resource_metadata_served_at_root_and_suffixed_path() {
        let (st, _dir) = state().await;
        let app = public_router().with_state(st);
        // RFC 9728: a client may look for the metadata at the root or at the
        // path-insertion variant for the `/mcp` resource — both must answer.
        for uri in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", "mem.example.org")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let j = body_json(resp).await;
            assert_eq!(j["resource"], "https://mem.example.org/mcp", "{uri}");
        }
    }

    #[tokio::test]
    async fn authorization_code_grant_mints_smart_jwt() {
        let (st, _dir) = state().await;
        let secret = st.secret.clone();
        // A code as the consent step would have issued it.
        let redirect = "https://claude.ai/api/mcp/auth_callback";
        let grant = oauth_server::AuthCodeGrant {
            client_id: "cid".to_owned(),
            redirect_uri: redirect.to_owned(),
            code_challenge: RFC_CHALLENGE.to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "claude".to_owned(),
            wiki_id: "franzclaude".to_owned(),
        };
        let code = oauth_server::issue_auth_code(&st.pool, &grant, Duration::from_secs(300))
            .await
            .unwrap();

        let app = public_router().with_state(st);
        let form = format!(
            "grant_type=authorization_code&code={code}&client_id=cid\
             &redirect_uri={redirect}&code_verifier={RFC_VERIFIER}"
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webagentoauth/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["token_type"], "Bearer");
        assert!(j["refresh_token"].is_string());
        // The access token is a smart JWT the /mcp middleware will accept.
        let access = j["access_token"].as_str().unwrap();
        let claims = jwt::verify_offline(&secret, access).unwrap();
        assert_eq!(claims.sender_id, "franz");
        assert_eq!(claims.consumer_id.as_deref(), Some("claude"));
        assert!(claims.consumer_class.is_smart());
        assert!(
            claims.profile.is_web(),
            "web connection must carry the web profile"
        );
    }

    #[tokio::test]
    async fn wrong_verifier_is_invalid_grant() {
        let (st, _dir) = state().await;
        let redirect = "https://claude.ai/api/mcp/auth_callback";
        let grant = oauth_server::AuthCodeGrant {
            client_id: "cid".to_owned(),
            redirect_uri: redirect.to_owned(),
            code_challenge: RFC_CHALLENGE.to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "claude".to_owned(),
            wiki_id: "franzclaude".to_owned(),
        };
        let code = oauth_server::issue_auth_code(&st.pool, &grant, Duration::from_secs(300))
            .await
            .unwrap();
        let app = public_router().with_state(st);
        let form = format!(
            "grant_type=authorization_code&code={code}&client_id=cid\
             &redirect_uri={redirect}&code_verifier=WRONG"
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webagentoauth/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let j = body_json(resp).await;
        assert_eq!(j["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn dcr_registers_and_returns_client_id() {
        let (st, _dir) = state().await;
        let app = public_router().with_state(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webagentoauth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"client_name":"Claude","redirect_uris":["https://claude.ai/cb"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let j = body_json(resp).await;
        assert!(j["client_id"].is_string());
        assert_eq!(j["client_name"], "Claude");
        assert_eq!(j["token_endpoint_auth_method"], "none");
    }

    #[tokio::test]
    async fn skill_md_is_served_for_download() {
        let (st, _dir) = state().await;
        let app = public_router().with_state(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/webagentoauth/skill.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/markdown"), "got: {ct}");
        assert!(
            resp.headers()
                .get("content-disposition")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("attachment"),
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("name: mwe-mcp-memory"), "skill frontmatter");
        assert!(body.contains("recall"), "skill body");
    }

    #[tokio::test]
    async fn authorize_get_rejects_unknown_client() {
        let (st, _dir) = state().await;
        let app = consent_router().with_state(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/webagentoauth/authorize?response_type=code&client_id=nope\
                          &redirect_uri=https://claude.ai/cb&code_challenge=abc&code_challenge_method=S256")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authorize_get_bounces_to_login_when_unauthenticated() {
        let (st, _dir) = state().await;
        let redirect = "https://claude.ai/cb";
        let client = oauth_server::register_client(&st.pool, "Claude", &[redirect.to_owned()])
            .await
            .unwrap();
        let app = consent_router().with_state(st);
        let uri = format!(
            "/webagentoauth/authorize?response_type=code&client_id={}\
             &redirect_uri={redirect}&code_challenge={RFC_CHALLENGE}&code_challenge_method=S256",
            client.client_id
        );
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        // No session cookie → redirect to login carrying the encoded `next`.
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert!(loc.starts_with("/dashboard/login?next="), "got: {loc}");
        assert!(
            loc.contains("webagentoauth%2Fauthorize"),
            "next not encoded: {loc}"
        );
    }

    #[test]
    fn slugify_yields_valid_wiki_slugs() {
        assert_eq!(slugify("Claude"), "claude");
        assert_eq!(slugify("ChatGPT"), "chatgpt");
        assert_eq!(slugify("My Bot 2!"), "my-bot-2");
        assert_eq!(slugify("  --Weird__Name-- "), "weird-name");
        assert_eq!(slugify("***"), "");
        // connection_slug falls back when slugify empties out.
        assert_eq!(connection_slug("", "***"), "agent");
        assert_eq!(connection_slug("  ", "Claude"), "claude");
        // Whatever connection_slug returns must parse as a WikiSlug.
        assert!(WikiSlug::parse(&connection_slug("My Bot 2!", "x")).is_ok());
    }

    #[test]
    fn pct_encode_keeps_unreserved_escapes_rest() {
        assert_eq!(pct_encode("abcXYZ-_.~123"), "abcXYZ-_.~123");
        assert_eq!(
            pct_encode("/dashboard/x?a=b&c"),
            "%2Fdashboard%2Fx%3Fa%3Db%26c"
        );
    }

    #[test]
    fn loopback_redirects_recognised() {
        assert!(is_loopback_redirect("http://localhost:8080/callback"));
        assert!(is_loopback_redirect("http://127.0.0.1:53117/callback"));
        assert!(is_loopback_redirect("http://[::1]:9000/cb"));
        assert!(is_loopback_redirect("http://localhost/cb")); // no explicit port
        assert!(!is_loopback_redirect(
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(!is_loopback_redirect("http://evil.example/cb"));
        assert!(!is_loopback_redirect("http://localhostx/cb")); // not the loopback host
    }

    #[test]
    fn redirect_match_is_exact_then_loopback_port_agnostic() {
        // Exact still matches (the common registered-then-presented case).
        assert!(redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback",
        ));
        // Loopback: same host + path, different port → matches (RFC 8252 §7.3).
        assert!(redirect_matches(
            "http://localhost:5000/callback",
            "http://localhost:61234/callback",
        ));
        assert!(redirect_matches(
            "http://127.0.0.1:1/cb",
            "http://127.0.0.1:2/cb"
        ));
        // Loopback but different path → no match.
        assert!(!redirect_matches(
            "http://localhost:5000/callback",
            "http://localhost:5000/other",
        ));
        // Different loopback host → no cross-host relaxation (conservative).
        assert!(!redirect_matches(
            "http://localhost:5000/cb",
            "http://127.0.0.1:5000/cb",
        ));
        // Non-loopback never relaxes — the port matters.
        assert!(!redirect_matches(
            "https://claude.ai/cb",
            "https://claude.ai:8443/cb"
        ));
        // Loopback never matches a remote redirect.
        assert!(!redirect_matches(
            "http://localhost:5000/cb",
            "http://evil.example:5000/cb",
        ));
    }

    #[tokio::test]
    async fn profile_is_local_for_loopback_client_web_for_https() {
        let (st, _dir) = state().await;
        let native = oauth_server::register_client(
            &st.pool,
            "Claude Code",
            &["http://localhost:53117/callback".to_owned()],
        )
        .await
        .unwrap();
        let web = oauth_server::register_client(
            &st.pool,
            "Claude",
            &["https://claude.ai/api/mcp/auth_callback".to_owned()],
        )
        .await
        .unwrap();
        assert!(
            profile_for_client(&st, &native.client_id).await.is_local(),
            "a loopback (native CLI) client gets the full Local surface"
        );
        assert!(
            profile_for_client(&st, &web.client_id).await.is_web(),
            "a hosted https client stays on the trimmed Web surface"
        );
        // Unknown client → Web (the safe default).
        assert!(profile_for_client(&st, "nope").await.is_web());
    }
}
