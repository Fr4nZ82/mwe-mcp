// SPDX-License-Identifier: AGPL-3.0-or-later
//! JWT bearer middleware for the HTTP MCP transport.
//!
//! Lives in front of the rmcp `StreamableHttpService`: every request to
//! `/mcp/*` first goes through [`jwt_auth_middleware`], which reads
//! `Authorization: Bearer <token>`, verifies it via
//! [`mwe_core::jwt::verify`] (signature + `exp` + blacklist), resolves
//! the optional `X-MWE-Act-As` header against `consumer_delegations`,
//! and stashes the resulting [`IdentityProfile`] in
//! `request.extensions_mut()` so the per-tool handler can read it back
//! out via `RequestContext.extensions.get::<http::request::Parts>()`.
//!
//! Failure modes:
//!
//! - Missing / invalid bearer ⇒ `401`. The middleware never logs the
//!   bearer itself.
//! - `X-MWE-Act-As` set on a **smart-class** token ⇒ `403`
//!   (`act_as_requires_standard`). Delegation is the *standard*
//!   consumer's mechanism; a smart consumer is mono-user by design and
//!   acts as its own human owner, so it may never delegate even though
//!   it carries a `consumer_id` for the cooperative lease.
//! - `X-MWE-Act-As` set on a token without `consumer_id` ⇒ `403`. The
//!   header is a multi-user consumer feature; mono-user clients have
//!   no reason to set it and tripping this guard usually means a
//!   client misconfigured its headers.
//! - `X-MWE-Act-As` not in `consumer_delegations.allowed_sender_ids`
//!   ⇒ `403`. The bot is asking to impersonate a user the admin has
//!   not delegated to it (or has just revoked).
//! - `X-MWE-Act-As` value malformed (non-ASCII, empty after trim) ⇒
//!   `403` rather than silently falling back to the token holder —
//!   silent fallback would mask client-side header bugs.
//!
//! Every failure body is JSON of the shape `{ "error": { "code": ...,
//! "message": ... } }` so a consumer's error mapper has a single
//! shape to deserialise.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mwe_core::jwt;
use serde_json::json;
use tracing::warn;

use super::state::{IdentityProfile, McpState};

/// Canonical name of the act-as header.
///
/// Spelled `X-MWE-Act-As` in `manifesto.md §3.8` and
/// `AGENT_INSTRUCTIONS.md §3.1`. HTTP header names are
/// case-insensitive; we keep the lowercase form here because that is
/// what `axum::http::HeaderName::from_static` requires.
pub const ACT_AS_HEADER: HeaderName = HeaderName::from_static("x-mwe-act-as");

/// Axum tower middleware in front of `/mcp`.
///
/// Pulls the bearer token, verifies it, resolves the optional
/// `X-MWE-Act-As` header against `consumer_delegations`, attaches an
/// [`IdentityProfile`] (with `sender_id` set to the effective sender)
/// to the request extensions, then calls the next service.
pub async fn jwt_auth_middleware(
    State(state): State<McpState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // RFC 9728 / MCP authorization: a 401 from the resource server points the
    // client at its protected-resource metadata via `WWW-Authenticate`, so an
    // OAuth client (the claude.ai web app) can discover the authorization server
    // without guessing the well-known path. Derived from the request `Host`.
    let challenge = www_authenticate_challenge(&req);

    let Some(token) = extract_bearer(&req) else {
        return unauthorized("missing_bearer", challenge);
    };

    let claims = match jwt::verify(&state.secret, &token, &state.pool, &state.blacklist).await {
        Ok(c) => c,
        // A revoked token gets a dedicated 401 code so the
        // smart consumer can degrade gracefully (keep working on the
        // local `.mwe/wiki/` cache, surface "token revoked — issue a
        // new one") instead of treating it as a generic auth failure.
        Err(e @ jwt::TokenError::Revoked { .. }) => {
            warn!(error = %e, "mcp auth: token revoked");
            return unauthorized("token_revoked", challenge);
        },
        Err(e) => {
            warn!(error = %e, "mcp auth: token rejected");
            return unauthorized("invalid_token", challenge);
        },
    };

    let mut profile = IdentityProfile::from_claims(claims);

    // Agent wiring (diagonal identity model — the un-deferred `is_agent` marker,
    // identity-and-acl.md §1.5). A STANDARD token's `sender_id` here, BEFORE the
    // act-as rewrite below, is the bot's own credential-less system-user.
    // Establish its consumer ↔ system-user binding + `is_agent` marker straight
    // from the token, so behaviour-rule + agent-authored-memory routing resolve
    // the agent wiki WITHOUT a separate `consumer_register` the conversational
    // bridge may skip (which had left agents like Hermes unbound). Idempotent and
    // guarded (one read after the first connect); best-effort — a memory feature
    // must never break authentication.
    if profile.consumer_class.is_standard()
        && let Some(consumer_id) = profile.consumer_id.as_deref()
        && let Err(e) =
            mwe_core::consumers::ensure_agent_identity(&state.pool, consumer_id, &profile.sender_id)
                .await
    {
        warn!(
            error = %e,
            consumer = %consumer_id,
            "mcp auth: agent-identity establish failed (non-fatal)"
        );
    }

    match resolve_act_as(&req) {
        ActAsRequest::Absent => {},
        ActAsRequest::Malformed => {
            warn!("mcp auth: malformed X-MWE-Act-As header");
            return forbidden("act_as_malformed");
        },
        ActAsRequest::Requested(target) => {
            // Diagonal identity model: act-as is the *standard* consumer's
            // mechanism for attributing a memory to the human it serves. A
            // smart consumer authenticates directly as its human owner
            // (Pattern A, mono-user by design) and may never delegate — even
            // though it carries a `consumer_id` for the cooperative lease.
            if !profile.consumer_class.is_standard() {
                warn!(
                    sender = %profile.sender_id,
                    "mcp auth: X-MWE-Act-As set on a smart-class token"
                );
                return forbidden("act_as_requires_standard");
            }
            let Some(consumer_id) = profile.consumer_id.as_deref() else {
                warn!(
                    sender = %profile.sender_id,
                    "mcp auth: X-MWE-Act-As set on a token without consumer_id"
                );
                return forbidden("act_as_requires_consumer");
            };
            let allowed = match state
                .delegations
                .is_allowed(&state.pool, consumer_id, &target)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "mcp auth: delegation cache failure");
                    return internal_error("delegation_lookup_failed");
                },
            };
            if !allowed {
                warn!(
                    consumer = %consumer_id,
                    target = %target,
                    "mcp auth: consumer not delegated for requested act-as"
                );
                return forbidden("act_as_not_delegated");
            }
            profile.sender_id = target;
        },
    }

    req.extensions_mut().insert(profile);
    next.run(req).await
}

/// Build the `WWW-Authenticate: Bearer resource_metadata="…"` value pointing at
/// this server's protected-resource metadata (roadmap 19, `webagentoauth`).
///
/// The origin is taken from the request `Host` header (`http` for loopback,
/// `https` otherwise — the same heuristic the onboarding pages use), so behind a
/// TLS-terminating tunnel the advertised URL is the public one. Returns `None`
/// only when there is no usable `Host`, in which case the 401 simply omits the
/// header (a client can still fall back to the well-known path).
fn www_authenticate_challenge(req: &Request<Body>) -> Option<HeaderValue> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())?;
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    let value = format!(
        r#"Bearer resource_metadata="{scheme}://{host}/.well-known/oauth-protected-resource""#
    );
    HeaderValue::from_str(&value).ok()
}

fn extract_bearer(req: &Request<Body>) -> Option<String> {
    let raw = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?;
    let (scheme, token) = raw.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(token.trim().to_owned())
    } else {
        None
    }
}

/// Outcome of inspecting the `X-MWE-Act-As` header.
enum ActAsRequest {
    /// Header absent — the call runs as the token holder.
    Absent,
    /// Header present but unusable (non-ASCII, empty after trim).
    Malformed,
    /// Header present with a usable user id.
    Requested(String),
}

fn resolve_act_as(req: &Request<Body>) -> ActAsRequest {
    let Some(raw) = req.headers().get(&ACT_AS_HEADER) else {
        return ActAsRequest::Absent;
    };
    let Ok(value) = raw.to_str() else {
        return ActAsRequest::Malformed;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return ActAsRequest::Malformed;
    }
    ActAsRequest::Requested(trimmed.to_owned())
}

fn unauthorized(code: &'static str, challenge: Option<HeaderValue>) -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "error": {
                "code": code,
                "message": match code {
                    "missing_bearer" => "Authorization: Bearer <jwt> required for /mcp",
                    "invalid_token" => "JWT signature / expiry / algorithm check failed",
                    // Distinct from `invalid_token` so the smart
                    // consumer can surface "token revoked — issue a new
                    // one" and keep working on its local `.mwe/wiki/`
                    // cache rather than wiping state.
                    "token_revoked" => "JWT was revoked (jti is in token_blacklist); issue a fresh token from the dashboard. Local writes can stay queued — they will reconcile via wiki_admin_pull + wiki_admin_push on the next session.",
                    _ => "request denied",
                },
            }
        })
        .to_string(),
    )
        .into_response();
    // Point an OAuth client at the protected-resource metadata (RFC 9728).
    if let Some(value) = challenge {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    resp
}

fn forbidden(code: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "error": {
                "code": code,
                "message": match code {
                    "act_as_requires_standard" =>
                        "X-MWE-Act-As is a standard-consumer feature; a smart consumer acts as its own human owner and may not delegate",
                    "act_as_requires_consumer" =>
                        "X-MWE-Act-As requires a token that carries a consumer_id claim",
                    "act_as_not_delegated" =>
                        "consumer is not delegated to act-as the requested sender_id",
                    "act_as_malformed" =>
                        "X-MWE-Act-As header value is empty or not valid ASCII",
                    _ => "request denied",
                },
            }
        })
        .to_string(),
    )
        .into_response()
}

fn internal_error(code: &'static str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "error": {
                "code": code,
                "message": "internal_error during request authorization",
            }
        })
        .to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use mwe_core::config::LlmConfig;
    use mwe_core::db;
    use mwe_core::delegations::{self, DelegationCache};
    use mwe_core::embedder::FakeEmbedder;
    use mwe_core::jwt::{BlacklistCache, TokenClaims, TokenSecret};
    use mwe_core::wiki::WikiTree;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn build_state() -> (McpState, TokenSecret, sqlx::SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        let secret = TokenSecret::new(vec![0xABu8; 32]).expect("secret");
        let blacklist = Arc::new(BlacklistCache::new());
        let delegations = Arc::new(DelegationCache::new());
        let embedder: Arc<dyn mwe_core::embedder::Embedder> =
            Arc::new(FakeEmbedder::new("fake", 4));
        let state = McpState {
            pool: pool.clone(),
            tree,
            embedder,
            secret: secret.clone(),
            blacklist,
            delegations,
            llm_config: LlmConfig::default(),
            recall: Arc::new(std::sync::RwLock::new(
                mwe_core::config::RecallConfig::default(),
            )),
            workdir: dir.path().to_path_buf(),
            document_policy: mwe_core::document::DocumentPolicy::default(),
        };
        (state, secret, pool, dir)
    }

    fn echo_router(state: McpState) -> Router {
        Router::new()
            .route(
                "/echo",
                get(|req: Request<Body>| async move {
                    let profile = req.extensions().get::<IdentityProfile>().cloned();
                    match profile {
                        Some(p) => (StatusCode::OK, p.sender_id).into_response(),
                        None => (StatusCode::INTERNAL_SERVER_ERROR, "no profile").into_response(),
                    }
                }),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                jwt_auth_middleware,
            ))
            .with_state(state)
    }

    fn consumer_token(secret: &TokenSecret, sender: &str, consumer: &str) -> String {
        let mut claims = TokenClaims::new(sender, "test-bot", "default", Duration::from_secs(60));
        claims.consumer_id = Some(consumer.to_owned());
        jwt::issue(secret, &claims).unwrap()
    }

    fn mono_user_token(secret: &TokenSecret, sender: &str) -> String {
        let claims = TokenClaims::new(sender, "cli", "default", Duration::from_secs(60));
        jwt::issue(secret, &claims).unwrap()
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn middleware_rejects_missing_bearer() {
        let (state, _secret, _pool, _dir) = build_state().await;
        let app = echo_router(state);
        let resp = app
            .oneshot(Request::builder().uri("/echo").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_sets_www_authenticate_on_401() {
        let (state, _secret, _pool, _dir) = build_state().await;
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::HOST, "mwe.contea.casa")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("WWW-Authenticate present")
            .to_str()
            .unwrap();
        assert!(www.starts_with("Bearer "), "got: {www}");
        assert!(
            www.contains(
                "resource_metadata=\"https://mwe.contea.casa/.well-known/oauth-protected-resource\""
            ),
            "got: {www}"
        );
    }

    #[tokio::test]
    async fn middleware_rejects_bogus_token() {
        let (state, _secret, _pool, _dir) = build_state().await;
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, "Bearer not.a.real.jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_passes_identity_through_for_valid_token() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = mono_user_token(&secret, "frodo");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert_eq!(body, "frodo");
    }

    #[tokio::test]
    async fn middleware_rejects_act_as_on_mono_user_token() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = mono_user_token(&secret, "frodo");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(&ACT_AS_HEADER, "galadriel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_text(resp).await;
        assert!(
            body.contains("act_as_requires_consumer"),
            "unexpected body: {body}"
        );
    }

    #[tokio::test]
    async fn middleware_rejects_undelegated_act_as() {
        let (state, secret, pool, _dir) = build_state().await;
        // Delegation grants only `frodo`; bot will request `galadriel`.
        delegations::upsert(&pool, "samvise-prod", &["frodo".to_owned()], "frodo")
            .await
            .unwrap();
        state.delegations.refresh(&pool).await.unwrap();

        let token = consumer_token(&secret, "samvise-bot", "samvise-prod");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(&ACT_AS_HEADER, "galadriel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_text(resp).await;
        assert!(
            body.contains("act_as_not_delegated"),
            "unexpected body: {body}"
        );
    }

    #[tokio::test]
    async fn middleware_rewrites_sender_to_act_as_when_delegated() {
        let (state, secret, pool, _dir) = build_state().await;
        delegations::upsert(
            &pool,
            "samvise-prod",
            &["frodo".to_owned(), "galadriel".to_owned()],
            "frodo",
        )
        .await
        .unwrap();
        state.delegations.refresh(&pool).await.unwrap();

        let token = consumer_token(&secret, "samvise-bot", "samvise-prod");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(&ACT_AS_HEADER, "galadriel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert_eq!(body, "galadriel", "effective sender should be the act-as");
    }

    #[tokio::test]
    async fn middleware_rejects_act_as_on_smart_token() {
        // Diagonal identity model: a smart consumer is mono-user by design and
        // may not delegate, even though it carries a `consumer_id` (which the
        // old consumer_id-presence gate would have waved through). The grant
        // below is present on purpose to prove the class gate fires *before*
        // any delegation lookup — otherwise this act-as would be allowed.
        let (state, secret, pool, _dir) = build_state().await;
        delegations::upsert(&pool, "cc-laptop", &["galadriel".to_owned()], "frodo")
            .await
            .unwrap();
        state.delegations.refresh(&pool).await.unwrap();

        let mut claims = TokenClaims::new("frodo", "cc-laptop", "default", Duration::from_secs(60));
        claims.consumer_id = Some("cc-laptop".to_owned());
        claims.consumer_class = mwe_core::jwt::ConsumerClass::Smart;
        let token = jwt::issue(&secret, &claims).unwrap();

        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(&ACT_AS_HEADER, "galadriel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_text(resp).await;
        assert!(
            body.contains("act_as_requires_standard"),
            "unexpected body: {body}"
        );
    }

    #[tokio::test]
    async fn middleware_rejects_empty_act_as_header() {
        let (state, secret, _pool, _dir) = build_state().await;
        let token = consumer_token(&secret, "samvise-bot", "samvise-prod");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(&ACT_AS_HEADER, "   ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_text(resp).await;
        assert!(body.contains("act_as_malformed"), "unexpected body: {body}");
    }

    #[tokio::test]
    async fn middleware_keeps_consumer_token_holder_when_act_as_absent() {
        let (state, secret, _pool, _dir) = build_state().await;
        // A consumer token with no act-as header runs as the bot's own
        // sender_id — useful for debug or for tools that don't need
        // delegation (e.g. `consumer_register`).
        let token = consumer_token(&secret, "samvise-bot", "samvise-prod");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert_eq!(body, "samvise-bot");
    }

    /// Agent wiring (the un-deferred `is_agent` marker): a STANDARD token
    /// connecting establishes its consumer↔system-user binding and stamps
    /// `is_agent` straight from the token — no separate `consumer_register`. This
    /// is what lets behaviour-rule + agent-authored-memory routing resolve the
    /// agent wiki for a conversational bridge (like Hermes) that never registers.
    #[tokio::test]
    async fn middleware_establishes_agent_identity_from_standard_token() {
        let (state, secret, pool, _dir) = build_state().await;
        // The bot's system-user identity exists (credential-less); nothing bound yet.
        sqlx::query("INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('samvise-bot', '[]', 0)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            mwe_core::consumers::system_user_for(&pool, "samvise-prod")
                .await
                .unwrap()
                .is_none(),
            "no binding before the first connect"
        );
        assert!(
            !mwe_core::enrollment::is_agent(&pool, "samvise-bot")
                .await
                .unwrap()
        );

        let token = consumer_token(&secret, "samvise-bot", "samvise-prod");
        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Established from the token: the binding resolves, and the identity is marked.
        assert_eq!(
            mwe_core::consumers::system_user_for(&pool, "samvise-prod")
                .await
                .unwrap()
                .as_deref(),
            Some("samvise-bot"),
            "the consumer↔system-user binding is established on connect"
        );
        assert!(
            mwe_core::enrollment::is_agent(&pool, "samvise-bot")
                .await
                .unwrap(),
            "the bot identity is stamped is_agent on connect"
        );
    }

    /// A SMART token never establishes an agent identity — its `sender_id` is the
    /// human owner, not a credential-less bot, so neither the binding nor the
    /// `is_agent` marker is written.
    #[tokio::test]
    async fn middleware_does_not_mark_smart_consumer_as_agent() {
        let (state, secret, pool, _dir) = build_state().await;
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('frodo', '[]', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut claims = TokenClaims::new("frodo", "cc-laptop", "default", Duration::from_secs(60));
        claims.consumer_id = Some("cc-laptop".to_owned());
        claims.consumer_class = mwe_core::jwt::ConsumerClass::Smart;
        let token = jwt::issue(&secret, &claims).unwrap();

        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        assert!(
            mwe_core::consumers::system_user_for(&pool, "cc-laptop")
                .await
                .unwrap()
                .is_none(),
            "a smart consumer must not be bound as an agent"
        );
        assert!(
            !mwe_core::enrollment::is_agent(&pool, "frodo")
                .await
                .unwrap(),
            "a smart consumer's human sender must not be marked is_agent"
        );
    }

    /// A revoked token's `401` must use the dedicated
    /// `token_revoked` wire code (NOT the generic `invalid_token`) so
    /// the smart consumer can degrade gracefully: surface the
    /// "issue a new token" prompt, keep the local `.mwe/wiki/` cache
    /// intact, replay queued writes via `wiki_admin_pull` +
    /// `wiki_admin_push mode=upsert` on the next session.
    #[tokio::test]
    async fn middleware_returns_token_revoked_code_for_blacklisted_token() {
        let (state, secret, pool, _dir) = build_state().await;
        let claims = TokenClaims::new("frodo", "cli", "default", Duration::from_secs(60));
        let jti = claims.jti.clone();
        let exp = claims.exp;
        let token = jwt::issue(&secret, &claims).unwrap();

        // Revoke the token, then bust the cache so the next is_revoked
        // call sees the row instead of the empty boot snapshot.
        jwt::revoke(&pool, &jti, "rotated", "frodo", exp)
            .await
            .unwrap();
        state.blacklist.refresh(&pool).await.unwrap();

        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_text(resp).await;
        assert!(
            body.contains("\"token_revoked\""),
            "expected the dedicated wire code, got: {body}"
        );
        assert!(
            !body.contains("\"invalid_token\""),
            "must NOT fall back to invalid_token for the revoke path: {body}"
        );
    }

    /// Defense-in-depth: invalid-signature tokens stay on the generic
    /// `invalid_token` code — only blacklisted JTIs get the
    /// `token_revoked` path.
    #[tokio::test]
    async fn middleware_keeps_invalid_token_code_for_signature_failures() {
        let (state, _secret, _pool, _dir) = build_state().await;
        // Forge a token with a different secret → signature mismatch.
        let other_secret = TokenSecret::new(vec![0xCDu8; 32]).unwrap();
        let claims = TokenClaims::new("frodo", "cli", "default", Duration::from_secs(60));
        let token = jwt::issue(&other_secret, &claims).unwrap();

        let app = echo_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_text(resp).await;
        assert!(body.contains("\"invalid_token\""), "got: {body}");
        assert!(
            !body.contains("\"token_revoked\""),
            "signature failure must not be misclassified as a revoke: {body}"
        );
    }
}
