// SPDX-License-Identifier: AGPL-3.0-or-later
//! Anthropic OAuth (Claude Code subscription) login flow + credential store.
//!
//! This is the engine half of "log in with Claude Code": PKCE generation, the
//! browser authorize URL, the authorization-code → token exchange, refresh,
//! and a workdir-local credential store that hands out a *fresh* access token
//! (refreshing transparently when the short-lived one is near expiry). The
//! "Log in with Claude Code" button and the HTTP routes that redirect to the
//! authorize page and catch the callback live in the dashboard crate
//! (`mwe_dashboard::routes::claude_login`); the server installs the
//! process-wide [`OauthStore`] at startup (see [`install_global_store`] /
//! [`default_store`]). The transport that *uses* the resulting token is
//! [`crate::llm::AnthropicBackend`]'s OAuth path.
//!
//! **Test / personal use only.** This reuses the operator's own Claude
//! subscription and presents requests as the Claude CLI — never a deployed
//! product auth mode. SSOT: `docs/protocol/config-schema.md`
//! §"Anthropic Claude Code / OAuth auth".

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The public Claude Code OAuth client id (the one the CLI itself uses).
pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Browser authorization endpoint.
const AUTHORIZE_ENDPOINT: &str = "https://claude.ai/oauth/authorize";

/// Token exchange / refresh endpoint.
const TOKEN_ENDPOINT: &str = "https://console.anthropic.com/v1/oauth/token";

/// Scopes the Claude Code subscription flow requests.
const OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

/// Anthropic's out-of-band callback page (the paste fallback).
///
/// It *displays* the authorization code for the user to paste back; used as
/// the redirect when a loopback callback to our own dashboard is unavailable.
pub const OOB_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";

/// Reserved `api_key_env` value selecting the Claude Code login store.
///
/// Set on an `anthropic` slot (instead of naming a real env var) it routes the
/// slot to the workdir login store; the dashboard writes this when the
/// operator chooses "Log in with Claude Code". **Test / personal use only.**
pub const CLAUDE_CODE_LOGIN: &str = "claude-code";

/// Process-wide Claude Code login store, installed once by the server at
/// startup (it captures the workdir). `LlmFunctionConfig::build_backend` reads
/// it when a slot's `api_key_env` is [`CLAUDE_CODE_LOGIN`]. A `OnceLock`
/// because there is one store per server process (one workdir).
static GLOBAL_STORE: std::sync::OnceLock<std::sync::Arc<OauthStore>> = std::sync::OnceLock::new();

/// Install the process-wide login store (first call wins; idempotent).
pub fn install_global_store(store: std::sync::Arc<OauthStore>) {
    let _ = GLOBAL_STORE.set(store);
}

/// The process-wide login store, if [`install_global_store`] has run.
#[must_use]
pub fn global_store() -> Option<std::sync::Arc<OauthStore>> {
    GLOBAL_STORE.get().cloned()
}

/// Build a login store for `workdir` with a default (rustls) HTTP client.
///
/// The server constructs this once at startup and hands it to
/// [`install_global_store`]; keeping the `reqwest::Client` here means
/// the dashboard crate drives the whole flow without depending on
/// `reqwest` itself.
#[must_use]
pub fn default_store(workdir: &Path) -> OauthStore {
    OauthStore::new(workdir, OauthClient::new(reqwest::Client::new()))
}

/// Refresh the access token this many seconds *before* it actually expires,
/// so an in-flight request never races the expiry boundary.
const EXPIRY_SKEW_SECS: u64 = 60;

/// Fallback lifetime when the token response omits `expires_in`.
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;

/// Errors raised by the OAuth login / refresh / store path.
#[derive(Debug, Error)]
pub enum OauthError {
    /// The HTTP request to the token endpoint failed before a response.
    #[error("oauth transport error: {0}")]
    Transport(String),
    /// The token endpoint returned a non-success status (login expired,
    /// revoked, or the code/refresh token is invalid).
    #[error("oauth token endpoint returned HTTP {0}: {1}")]
    Token(u16, String),
    /// The callback `state` did not match the one we issued (CSRF guard).
    #[error("oauth `state` mismatch — possible CSRF, aborting")]
    StateMismatch,
    /// No credential is stored — the operator has not logged in yet.
    #[error("not logged in: no Claude Code credentials at {0}")]
    NotLoggedIn(PathBuf),
    /// Reading or writing the credential store failed.
    #[error("oauth credential store io error: {0}")]
    Io(String),
    /// A token response or stored credential could not be (de)serialized.
    #[error("oauth decode error: {0}")]
    Decode(String),
    /// The authorize URL could not be constructed.
    #[error("building authorize url: {0}")]
    Url(String),
    /// The OS random number generator was unavailable (PKCE / state).
    #[error("system rng unavailable: {0}")]
    Rng(String),
}

type Result<T> = std::result::Result<T, OauthError>;

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_b64url(n_bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; n_bytes];
    getrandom::getrandom(&mut buf).map_err(|e| OauthError::Rng(e.to_string()))?;
    Ok(b64url(&buf))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A PKCE verifier/challenge pair (S256). The `verifier` is kept by the
/// initiator and replayed at code exchange; the `challenge` goes in the
/// authorize URL.
pub struct Pkce {
    /// Secret kept by the initiator and replayed at code exchange.
    pub verifier: String,
    /// S256 hash of the verifier, sent in the authorize URL.
    pub challenge: String,
}

/// Generate a PKCE pair: a 32-byte random verifier and its S256 challenge.
pub fn generate_pkce() -> Result<Pkce> {
    let verifier = random_b64url(32)?;
    let challenge = b64url(Sha256::digest(verifier.as_bytes()).as_slice());
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// A fresh random `state` value (CSRF guard), echoed back by the callback and
/// validated by the initiator.
pub fn generate_state() -> Result<String> {
    random_b64url(32)
}

/// Build the browser authorization URL.
///
/// `redirect_uri` is either a loopback callback the server owns (seamless
/// path) or [`OOB_REDIRECT_URI`] (the paste fallback); whichever is used here
/// must be replayed verbatim at code exchange (the token endpoint enforces an
/// exact match).
pub fn build_authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> Result<String> {
    let url = reqwest::Url::parse_with_params(
        AUTHORIZE_ENDPOINT,
        &[
            ("code", "true"),
            ("client_id", OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
            ("scope", OAUTH_SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
        ],
    )
    .map_err(|e| OauthError::Url(e.to_string()))?;
    Ok(url.into())
}

/// Split the value the out-of-band callback page hands the user. Anthropic
/// formats it as `code#state`; the loopback path never needs this (the code
/// and state arrive as separate query params).
#[must_use]
pub fn parse_pasted_code(pasted: &str) -> (String, Option<String>) {
    match pasted.trim().split_once('#') {
        Some((code, state)) => (code.to_owned(), Some(state.to_owned())),
        None => (pasted.trim().to_owned(), None),
    }
}

/// Live OAuth credentials, persisted to the workdir as `anthropic_oauth.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredOauth {
    /// The short-lived bearer token presented to the Anthropic API.
    pub access_token: String,
    /// The long-lived token used to mint a new `access_token` at expiry.
    pub refresh_token: String,
    /// Unix seconds at which `access_token` expires.
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// Fold the wire response into stored credentials. A refresh response may
    /// omit `refresh_token` (the prior one stays valid), so the caller passes
    /// the one it already holds as a fallback.
    fn into_stored(self, prev_refresh: Option<&str>) -> Result<StoredOauth> {
        let refresh_token = self
            .refresh_token
            .or_else(|| prev_refresh.map(str::to_owned))
            .ok_or_else(|| OauthError::Decode("token response missing refresh_token".into()))?;
        Ok(StoredOauth {
            access_token: self.access_token,
            refresh_token,
            expires_at: now_unix() + self.expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECS),
        })
    }
}

/// HTTP client for the OAuth token endpoint (code exchange + refresh). The
/// endpoint is a field so tests can point it at a mock server.
pub struct OauthClient {
    http: reqwest::Client,
    token_endpoint: String,
}

impl OauthClient {
    /// Build a client targeting the canonical Anthropic token endpoint.
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
        }
    }

    #[cfg(test)]
    fn with_token_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.token_endpoint = endpoint.into();
        self
    }

    async fn post_token(&self, body: serde_json::Value) -> Result<TokenResponse> {
        // A plain User-Agent — NOT the `claude-cli/…` one. That UA is the
        // Claude Code "passport" the Messages/inference API requires, but the
        // OAuth *token* endpoint rejects it with a routing `404 not_found`
        // (verified 2026-06-23). Any normal UA reaches the token handler.
        let resp = self
            .http
            .post(&self.token_endpoint)
            .header("content-type", "application/json")
            .header("user-agent", concat!("mwe-mcp/", env!("CARGO_PKG_VERSION")))
            .json(&body)
            .send()
            .await
            .map_err(|e| OauthError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(OauthError::Token(
                status.as_u16(),
                text.chars().take(300).collect(),
            ));
        }
        resp.json::<TokenResponse>()
            .await
            .map_err(|e| OauthError::Decode(e.to_string()))
    }

    /// Exchange an authorization `code` (with the PKCE `verifier`) for tokens.
    /// `redirect_uri` must equal the one used to build the authorize URL.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        state: &str,
    ) -> Result<StoredOauth> {
        // `state` is required in the body: Anthropic's token endpoint rejects
        // the exchange with "Invalid request format" without it (verified
        // 2026-06-23). It is the same CSRF `state` issued in the authorize URL,
        // echoed back after the `#` in the out-of-band code.
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": OAUTH_CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        });
        self.post_token(body).await?.into_stored(None)
    }

    /// Trade a refresh token for a fresh access token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<StoredOauth> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": OAUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        });
        self.post_token(body)
            .await?
            .into_stored(Some(refresh_token))
    }
}

/// Workdir-local store for the Claude Code OAuth credentials, with transparent
/// refresh. One per workdir; the file is owner-only (`0600`) like `mwe-mcp.env`.
pub struct OauthStore {
    path: PathBuf,
    client: OauthClient,
}

impl OauthStore {
    /// Store at `<workdir>/anthropic_oauth.json`.
    #[must_use]
    pub fn new(workdir: &Path, client: OauthClient) -> Self {
        Self {
            path: workdir.join("anthropic_oauth.json"),
            client,
        }
    }

    /// The on-disk path of the credential file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the stored credentials, or `None` if the operator has not logged in.
    pub fn load(&self) -> Result<Option<StoredOauth>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| OauthError::Decode(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OauthError::Io(e.to_string())),
        }
    }

    /// Persist credentials to the store (owner-only file mode on unix).
    pub fn save(&self, creds: &StoredOauth) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(creds).map_err(|e| OauthError::Decode(e.to_string()))?;
        write_private(&self.path, &json)
    }

    /// Remove the stored credentials (logout).
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(OauthError::Io(e.to_string())),
        }
    }

    /// Whether a logged-in credential exists.
    #[must_use]
    pub fn is_logged_in(&self) -> bool {
        matches!(self.load(), Ok(Some(_)))
    }

    /// Return a valid access token, refreshing (and persisting) if the stored
    /// one is within the expiry skew window. Errors with
    /// [`OauthError::NotLoggedIn`] when no credential is stored.
    pub async fn resolve_access_token(&self) -> Result<String> {
        let creds = self
            .load()?
            .ok_or_else(|| OauthError::NotLoggedIn(self.path.clone()))?;
        if now_unix() + EXPIRY_SKEW_SECS < creds.expires_at {
            return Ok(creds.access_token);
        }
        let refreshed = self.client.refresh(&creds.refresh_token).await?;
        self.save(&refreshed)?;
        Ok(refreshed.access_token)
    }

    /// Complete a browser login: exchange the authorization `code` for
    /// tokens (replaying the PKCE `verifier` and the exact
    /// `redirect_uri` used to build the authorize URL) and persist them.
    /// This is the one call the dashboard's "Log in with Claude Code"
    /// completion makes — it reuses the store's own HTTP client so the
    /// caller never touches `reqwest`.
    pub async fn login_with_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        state: &str,
    ) -> Result<()> {
        let creds = self
            .client
            .exchange_code(code, verifier, redirect_uri, state)
            .await?;
        self.save(&creds)
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| OauthError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| OauthError::Io(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_workdir() -> PathBuf {
        let unique = random_b64url(8).expect("rng").replace(['-', '_'], "");
        let dir = std::env::temp_dir().join(format!("mwe-oauth-test-{unique}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn pkce_pair_is_url_safe_and_challenge_is_s256_of_verifier() {
        let pkce = generate_pkce().expect("pkce");
        assert!(!pkce.verifier.is_empty() && !pkce.challenge.is_empty());
        assert_ne!(pkce.verifier, pkce.challenge);
        let expected = b64url(Sha256::digest(pkce.verifier.as_bytes()).as_slice());
        assert_eq!(pkce.challenge, expected);
        for s in [&pkce.verifier, &pkce.challenge] {
            assert!(
                !s.contains(['+', '/', '=']),
                "must be base64url no-pad: {s}"
            );
        }
        // Two pairs differ (randomness wired).
        assert_ne!(generate_pkce().expect("pkce").verifier, pkce.verifier);
    }

    #[test]
    fn authorize_url_carries_pkce_and_client() {
        let url = build_authorize_url(
            "http://localhost:8742/oauth/claude/callback",
            "the-state",
            "the-challenge",
        )
        .expect("url");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains(&format!("client_id={OAUTH_CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=the-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=the-state"));
        // redirect_uri is percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8742"));
    }

    #[test]
    fn parse_pasted_code_splits_code_and_state() {
        assert_eq!(
            parse_pasted_code("  abc123#st8  "),
            ("abc123".to_owned(), Some("st8".to_owned()))
        );
        assert_eq!(parse_pasted_code("bare"), ("bare".to_owned(), None));
    }

    #[test]
    fn store_round_trips_and_clears() {
        let dir = temp_workdir();
        let store = OauthStore::new(&dir, OauthClient::new(reqwest::Client::new()));
        assert!(!store.is_logged_in());
        let creds = StoredOauth {
            access_token: "acc".into(),
            refresh_token: "ref".into(),
            expires_at: now_unix() + 3600,
        };
        store.save(&creds).expect("save");
        assert!(store.is_logged_in());
        assert_eq!(store.load().expect("load"), Some(creds));
        store.clear().expect("clear");
        assert_eq!(store.load().expect("load"), None);
    }

    #[tokio::test]
    async fn exchange_code_posts_authorization_code_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": OAUTH_CLIENT_ID,
                "code": "the-code",
                "state": "the-state",
                "code_verifier": "the-verifier",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "acc-1",
                "refresh_token": "ref-1",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let client = OauthClient::new(reqwest::Client::new())
            .with_token_endpoint(format!("{}/v1/oauth/token", server.uri()));
        let stored = client
            .exchange_code("the-code", "the-verifier", OOB_REDIRECT_URI, "the-state")
            .await
            .expect("exchange");
        assert_eq!(stored.access_token, "acc-1");
        assert_eq!(stored.refresh_token, "ref-1");
        assert!(stored.expires_at > now_unix());
    }

    #[tokio::test]
    async fn resolve_refreshes_an_expired_token_and_persists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": "old-ref",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "acc-new",
                "refresh_token": "ref-new",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let dir = temp_workdir();
        let store = OauthStore::new(
            &dir,
            OauthClient::new(reqwest::Client::new())
                .with_token_endpoint(format!("{}/v1/oauth/token", server.uri())),
        );
        // Already-expired credential on disk.
        store
            .save(&StoredOauth {
                access_token: "acc-old".into(),
                refresh_token: "old-ref".into(),
                expires_at: now_unix().saturating_sub(10),
            })
            .expect("save");

        let token = store.resolve_access_token().await.expect("resolve");
        assert_eq!(token, "acc-new");
        // Refreshed credential was persisted.
        assert_eq!(
            store.load().expect("load").unwrap().refresh_token,
            "ref-new"
        );
    }

    #[tokio::test]
    async fn login_with_code_exchanges_and_persists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "grant_type": "authorization_code",
                "code": "the-code",
                "state": "the-state",
                "code_verifier": "the-verifier",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "acc-login",
                "refresh_token": "ref-login",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let dir = temp_workdir();
        let store = OauthStore::new(
            &dir,
            OauthClient::new(reqwest::Client::new())
                .with_token_endpoint(format!("{}/v1/oauth/token", server.uri())),
        );
        assert!(!store.is_logged_in());
        store
            .login_with_code("the-code", "the-verifier", OOB_REDIRECT_URI, "the-state")
            .await
            .expect("login");
        assert!(store.is_logged_in());
        assert_eq!(
            store.load().expect("load").unwrap().access_token,
            "acc-login"
        );
    }

    #[tokio::test]
    async fn resolve_returns_stored_token_without_refresh_when_fresh() {
        let dir = temp_workdir();
        // Token endpoint points nowhere usable; a fresh token must not call it.
        let store = OauthStore::new(
            &dir,
            OauthClient::new(reqwest::Client::new())
                .with_token_endpoint("http://127.0.0.1:1/v1/oauth/token"),
        );
        store
            .save(&StoredOauth {
                access_token: "still-good".into(),
                refresh_token: "ref".into(),
                expires_at: now_unix() + 3600,
            })
            .expect("save");
        assert_eq!(
            store.resolve_access_token().await.expect("resolve"),
            "still-good"
        );
    }

    #[tokio::test]
    async fn resolve_errors_when_not_logged_in() {
        let dir = temp_workdir();
        let store = OauthStore::new(&dir, OauthClient::new(reqwest::Client::new()));
        assert!(matches!(
            store.resolve_access_token().await,
            Err(OauthError::NotLoggedIn(_))
        ));
    }
}
