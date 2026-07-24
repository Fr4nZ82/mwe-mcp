// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inbound OAuth 2.x authorization-server store (the `webagentoauth` flow).
//!
//! This module is the **data layer** behind the inbound OAuth flow that lets an
//! OAuth MCP client — the claude.ai web app, or a local CLI like Claude Code —
//! connect as the user's own *Smart* consumer (see
//! `web-agent-oauth.md`).
//! It is pure storage + PKCE: no HTTP, no JWT minting. The transport layer
//! (`mwe-dashboard`) drives the endpoints, and the access token it hands back is
//! an ordinary smart JWT ([`crate::jwt::issue`]) the existing `/mcp` middleware
//! already validates — so nothing here touches the token-verification path.
//!
//! Three artefacts live in the DB (migration `0044_webagentoauth.sql`):
//!
//! - **clients** — dynamically registered (RFC 7591) *public* PKCE clients. We do
//!   not issue a client secret: claude.ai is a public client and authorises with
//!   PKCE, and the human login + consent is the real gate, not client identity.
//! - **authorization codes** — single-use, PKCE-S256-bound. Redemption deletes the
//!   row, so the primary key is the serialization point: a concurrent double
//!   redeem cannot both win.
//! - **refresh tokens** — rotating. Stored as a SHA-256 hash, never in the clear;
//!   `rotate` revokes the presented token and issues a fresh one in one
//!   transaction.
//!
//! `is_admin` is deliberately **not** persisted here. It is re-resolved from
//! `enrollment_users` each time a JWT is minted, so a role change takes effect on
//! the next token rather than being frozen at consent time.

use base64::Engine as _;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::time::Duration;
use thiserror::Error;

/// Errors raised by the OAuth store.
///
/// The transport layer maps these onto the OAuth wire error codes
/// (`invalid_grant`, `invalid_client`, …) via [`OAuthError::oauth_code`].
#[derive(Debug, Error)]
pub enum OAuthError {
    /// Underlying SQL failure.
    #[error("oauth db: {0}")]
    Db(#[from] sqlx::Error),
    /// JSON (de)serialisation of the `redirect_uris` list.
    #[error("oauth json: {0}")]
    Json(#[from] serde_json::Error),
    /// The authorization code or refresh token is unknown, already used, or
    /// expired.
    #[error("invalid or expired grant")]
    InvalidGrant,
    /// The PKCE `code_verifier` does not match the stored challenge.
    #[error("pkce verification failed")]
    PkceMismatch,
    /// The redemption `client_id` does not match the one the code was issued to.
    #[error("client mismatch on code redemption")]
    ClientMismatch,
    /// The redemption `redirect_uri` does not match the one bound to the code.
    #[error("redirect_uri mismatch on code redemption")]
    RedirectMismatch,
    /// The OS RNG was unavailable while generating a token.
    #[error("os rng unavailable: {0}")]
    Rng(String),
}

impl OAuthError {
    /// The OAuth `error` code a token-endpoint response should carry for this
    /// failure (RFC 6749 §5.2).
    ///
    /// Internal failures collapse to `invalid_grant` rather than leaking a
    /// server error to the client.
    #[must_use]
    pub const fn oauth_code(&self) -> &'static str {
        match self {
            Self::ClientMismatch => "invalid_client",
            Self::Db(_) | Self::Json(_) | Self::Rng(_) => "server_error",
            _ => "invalid_grant",
        }
    }
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, OAuthError>;

// ---------------------------------------------------------------------------
// Token / PKCE primitives
// ---------------------------------------------------------------------------

/// Mint a fresh, URL-safe opaque secret (256 bits of OS entropy, base64url,
/// no padding). Used for authorization codes and refresh tokens — values that
/// travel to the client and must be unguessable.
fn new_secret() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| OAuthError::Rng(e.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// SHA-256 of a secret, hex-encoded — what we persist, so a DB read never
/// exposes a usable token.
fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Verify a PKCE `code_verifier` against a stored S256 `code_challenge`
/// (`challenge == base64url(sha256(verifier))`). Only S256 is accepted —
/// the plain method is refused upstream.
#[must_use]
pub fn verify_pkce_s256(verifier: &str, challenge: &str) -> bool {
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    // Length is part of the value; a direct compare is fine (the inputs are not
    // secret to each other — both are presented by the same client).
    computed == challenge
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn expiry_iso(ttl: Duration) -> String {
    let secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

/// `true` when `iso` (an RFC-3339 timestamp) is at or before now. An
/// unparseable timestamp counts as expired — fail closed.
fn is_expired(iso: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(iso).map_or(true, |t| t <= chrono::Utc::now())
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

/// A registered OAuth client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRegistration {
    /// Server-generated opaque client id.
    pub client_id: String,
    /// Human-readable name the client declared (e.g. `"Claude"`); used as the
    /// default consumer label suggested on the consent screen.
    pub client_name: String,
    /// The redirect URIs the client may receive the authorization code at.
    pub redirect_uris: Vec<String>,
    /// ISO-8601 registration time.
    pub created_at: String,
}

/// Register a new public PKCE client, generating an opaque `client_id`.
///
/// No client secret is issued (public client). `client_name` falls back to
/// `"client"` when blank so the consent screen always has a label to show.
pub async fn register_client(
    pool: &SqlitePool,
    client_name: &str,
    redirect_uris: &[String],
) -> Result<ClientRegistration> {
    let client_id = new_secret()?;
    let name = {
        let trimmed = client_name.trim();
        if trimmed.is_empty() {
            "client".to_owned()
        } else {
            trimmed.to_owned()
        }
    };
    let uris_json = serde_json::to_string(redirect_uris)?;
    let created_at = now_iso();
    sqlx::query(
        "INSERT INTO webagentoauth_clients (client_id, client_name, redirect_uris, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(&client_id)
    .bind(&name)
    .bind(&uris_json)
    .bind(&created_at)
    .execute(pool)
    .await?;
    Ok(ClientRegistration {
        client_id,
        client_name: name,
        redirect_uris: redirect_uris.to_vec(),
        created_at,
    })
}

/// Look up a registered client by id. `Ok(None)` when unknown.
pub async fn lookup_client(
    pool: &SqlitePool,
    client_id: &str,
) -> Result<Option<ClientRegistration>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT client_name, redirect_uris, created_at FROM webagentoauth_clients
         WHERE client_id = ?",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    let Some((client_name, uris_json, created_at)) = row else {
        return Ok(None);
    };
    let redirect_uris: Vec<String> = serde_json::from_str(&uris_json)?;
    Ok(Some(ClientRegistration {
        client_id: client_id.to_owned(),
        client_name,
        redirect_uris,
        created_at,
    }))
}

// ---------------------------------------------------------------------------
// Authorization codes (single-use, PKCE-bound)
// ---------------------------------------------------------------------------

/// Everything the `/authorize` consent step pins to an authorization code, and
/// (minus the PKCE challenge, already verified) everything the `/token` step
/// reads back out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCodeGrant {
    /// Client the code was issued to.
    pub client_id: String,
    /// Redirect URI the code was bound to.
    pub redirect_uri: String,
    /// PKCE S256 challenge captured at `/authorize`.
    pub code_challenge: String,
    /// The approving human user (becomes the token's `sender_id` / `owner_id`).
    pub sender_id: String,
    /// The named connection (becomes the token's `consumer_id` / device label).
    pub consumer_id: String,
    /// The dedicated smart wiki this connection authors.
    pub wiki_id: String,
}

/// Issue a single-use authorization code, store it (hashed) with `ttl`, and
/// return the plaintext code to redirect back to the client.
///
/// Redemption deletes the row (single-use), so only **abandoned** flows
/// leave rows behind — purged opportunistically here on every issue (and
/// by the boot-time [`crate::housekeeping`] sweep).
pub async fn issue_auth_code(
    pool: &SqlitePool,
    grant: &AuthCodeGrant,
    ttl: Duration,
) -> Result<String> {
    sqlx::query("DELETE FROM webagentoauth_codes WHERE expires_at < ?")
        .bind(now_iso())
        .execute(pool)
        .await?;
    let code = new_secret()?;
    let code_hash = hash_secret(&code);
    sqlx::query(
        "INSERT INTO webagentoauth_codes
            (code_hash, client_id, redirect_uri, code_challenge, sender_id,
             consumer_id, wiki_id, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&code_hash)
    .bind(&grant.client_id)
    .bind(&grant.redirect_uri)
    .bind(&grant.code_challenge)
    .bind(&grant.sender_id)
    .bind(&grant.consumer_id)
    .bind(&grant.wiki_id)
    .bind(now_iso())
    .bind(expiry_iso(ttl))
    .execute(pool)
    .await?;
    Ok(code)
}

/// Redeem an authorization code: atomically delete it (single-use), then verify
/// the client, redirect URI, PKCE verifier and expiry.
///
/// Any mismatch is an error, and the code is already burned — the legitimate
/// client never trips these, and burning on a failed attempt is the safe
/// single-use behaviour.
pub async fn consume_auth_code(
    pool: &SqlitePool,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<AuthCodeGrant> {
    let code_hash = hash_secret(code);
    // DELETE ... RETURNING makes redemption atomic and single-use: only one
    // caller can delete the row, and it gets the fields back in the same step.
    let row: Option<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "DELETE FROM webagentoauth_codes WHERE code_hash = ?
         RETURNING client_id, redirect_uri, code_challenge, sender_id, consumer_id,
                   wiki_id, expires_at",
    )
    .bind(&code_hash)
    .fetch_optional(pool)
    .await?;
    let Some((
        stored_client_id,
        stored_redirect_uri,
        code_challenge,
        sender_id,
        consumer_id,
        wiki_id,
        expires_at,
    )) = row
    else {
        return Err(OAuthError::InvalidGrant);
    };
    if is_expired(&expires_at) {
        return Err(OAuthError::InvalidGrant);
    }
    if stored_client_id != client_id {
        return Err(OAuthError::ClientMismatch);
    }
    if stored_redirect_uri != redirect_uri {
        return Err(OAuthError::RedirectMismatch);
    }
    if !verify_pkce_s256(code_verifier, &code_challenge) {
        return Err(OAuthError::PkceMismatch);
    }
    Ok(AuthCodeGrant {
        client_id: stored_client_id,
        redirect_uri: stored_redirect_uri,
        code_challenge,
        sender_id,
        consumer_id,
        wiki_id,
    })
}

// ---------------------------------------------------------------------------
// Refresh tokens (rotating)
// ---------------------------------------------------------------------------

/// The identity bits a refresh token carries — enough to re-mint a smart JWT
/// (minus `is_admin`, re-resolved from `enrollment_users` at mint time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshGrant {
    /// Client the refresh token belongs to.
    pub client_id: String,
    /// Owner / `sender_id` of the minted JWT.
    pub sender_id: String,
    /// Named connection / `consumer_id`.
    pub consumer_id: String,
    /// The dedicated smart wiki this connection authors.
    pub wiki_id: String,
}

/// Issue a fresh refresh token for `grant` with `ttl`, returning the
/// plaintext token (stored hashed).
///
/// The connection's stale rows (revoked or expired) are pruned in the
/// same breath — the fresh active row supersedes them as the
/// connection's wiki-binding record.
pub async fn issue_refresh_token(
    pool: &SqlitePool,
    grant: &RefreshGrant,
    ttl: Duration,
) -> Result<String> {
    let token = new_secret()?;
    insert_refresh(pool, &token, grant, ttl).await?;
    prune_connection_stale(pool, grant).await?;
    Ok(token)
}

/// Delete a connection's stale (revoked or expired) refresh rows. Called
/// wherever the connection just gained a fresh active row, so nothing of
/// value is lost: the active row carries the same grant. Keeps rotation
/// from accumulating one revoked row per turn (34 observed on one client
/// before this existed); connections that go stale *without* a fresh row
/// are the boot-time [`crate::housekeeping`] sweep's job, which preserves
/// one binding row per connection.
async fn prune_connection_stale(
    executor: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    grant: &RefreshGrant,
) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM webagentoauth_refresh
          WHERE sender_id = ? AND consumer_id = ? AND wiki_id = ?
            AND (revoked_at IS NOT NULL OR expires_at < ?)",
    )
    .bind(&grant.sender_id)
    .bind(&grant.consumer_id)
    .bind(&grant.wiki_id)
    .bind(now_iso())
    .execute(executor)
    .await?;
    Ok(res.rows_affected())
}

async fn insert_refresh(
    executor: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    token: &str,
    grant: &RefreshGrant,
    ttl: Duration,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO webagentoauth_refresh
            (token_hash, client_id, sender_id, consumer_id, wiki_id, created_at,
             expires_at, revoked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
    )
    .bind(hash_secret(token))
    .bind(&grant.client_id)
    .bind(&grant.sender_id)
    .bind(&grant.consumer_id)
    .bind(&grant.wiki_id)
    .bind(now_iso())
    .bind(expiry_iso(ttl))
    .execute(executor)
    .await?;
    Ok(())
}

/// Rotate a refresh token in one transaction.
///
/// Validates it (present, active, unexpired), revokes it, issues a
/// replacement, and prunes the connection's stale rows. Returns the new
/// plaintext token and the grant it carries.
pub async fn rotate_refresh_token(
    pool: &SqlitePool,
    refresh_token: &str,
    ttl: Duration,
) -> Result<(String, RefreshGrant)> {
    let token_hash = hash_secret(refresh_token);
    // Write-first (validates then revokes/issues): IMMEDIATE avoids the
    // read→write snapshot upgrade that would surface as `database is locked`.
    let mut tx = crate::db::begin_immediate(pool).await?;
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT client_id, sender_id, consumer_id, wiki_id, expires_at
         FROM webagentoauth_refresh
         WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(&token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((client_id, sender_id, consumer_id, wiki_id, expires_at)) = row else {
        return Err(OAuthError::InvalidGrant);
    };
    if is_expired(&expires_at) {
        return Err(OAuthError::InvalidGrant);
    }
    sqlx::query("UPDATE webagentoauth_refresh SET revoked_at = ? WHERE token_hash = ?")
        .bind(now_iso())
        .bind(&token_hash)
        .execute(&mut *tx)
        .await?;
    let grant = RefreshGrant {
        client_id,
        sender_id,
        consumer_id,
        wiki_id,
    };
    let new_token = new_secret()?;
    insert_refresh(&mut *tx, &new_token, &grant, ttl).await?;
    // The fresh active row is in place: drop the just-revoked row and any
    // older stale leftovers of this connection in the same commit.
    prune_connection_stale(&mut *tx, &grant).await?;
    tx.commit().await?;
    Ok((new_token, grant))
}

/// Revoke every active refresh token for a `(sender_id, consumer_id)` pair —
/// the "disconnect this connection" action of the dashboard connections view.
/// Returns the number of tokens revoked.
pub async fn revoke_connection(
    pool: &SqlitePool,
    sender_id: &str,
    consumer_id: &str,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE webagentoauth_refresh SET revoked_at = ?
         WHERE sender_id = ? AND consumer_id = ? AND revoked_at IS NULL",
    )
    .bind(now_iso())
    .bind(sender_id)
    .bind(consumer_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// An active web-agent connection — one row per `(sender, consumer, wiki)` with a
/// live (un-revoked) refresh token. Backs the dashboard connections list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The owning user.
    pub sender_id: String,
    /// The named connection / device label.
    pub consumer_id: String,
    /// The dedicated smart wiki this connection authors.
    pub wiki_id: String,
    /// The client the connection was issued to.
    pub client_id: String,
    /// ISO-8601 timestamp of the most recent (rotated) refresh token.
    pub created_at: String,
}

/// List the active web-agent connections (one per `(sender, consumer, wiki)`),
/// most recently rotated `created_at` kept. Revoked rows are excluded.
pub async fn list_connections(pool: &SqlitePool) -> Result<Vec<Connection>> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT sender_id, consumer_id, wiki_id, client_id, MAX(created_at)
         FROM webagentoauth_refresh
         WHERE revoked_at IS NULL
         GROUP BY sender_id, consumer_id, wiki_id, client_id
         ORDER BY sender_id, consumer_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(sender_id, consumer_id, wiki_id, client_id, created_at)| Connection {
                sender_id,
                consumer_id,
                wiki_id,
                client_id,
                created_at,
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db");
        (pool, dir)
    }

    fn pkce_pair(verifier: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    fn grant(client_id: &str, challenge: &str) -> AuthCodeGrant {
        AuthCodeGrant {
            client_id: client_id.to_owned(),
            redirect_uri: "https://claude.ai/api/mcp/auth_callback".to_owned(),
            code_challenge: challenge.to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "claude".to_owned(),
            wiki_id: "franzclaude".to_owned(),
        }
    }

    #[tokio::test]
    async fn register_then_lookup_roundtrips() {
        let (pool, _dir) = pool().await;
        let uris = vec!["https://claude.ai/cb".to_owned()];
        let reg = register_client(&pool, "Claude", &uris).await.unwrap();
        assert_eq!(reg.client_name, "Claude");
        let back = lookup_client(&pool, &reg.client_id).await.unwrap().unwrap();
        assert_eq!(back, reg);
        assert!(lookup_client(&pool, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn blank_client_name_defaults() {
        let (pool, _dir) = pool().await;
        let reg = register_client(&pool, "  ", &[]).await.unwrap();
        assert_eq!(reg.client_name, "client");
    }

    #[tokio::test]
    async fn auth_code_happy_path() {
        let (pool, _dir) = pool().await;
        let challenge = pkce_pair("the-verifier");
        let g = grant("cid", &challenge);
        let code = issue_auth_code(&pool, &g, Duration::from_secs(300))
            .await
            .unwrap();
        let consumed = consume_auth_code(
            &pool,
            &code,
            "cid",
            "https://claude.ai/api/mcp/auth_callback",
            "the-verifier",
        )
        .await
        .unwrap();
        assert_eq!(consumed.sender_id, "franz");
        assert_eq!(consumed.consumer_id, "claude");
        assert_eq!(consumed.wiki_id, "franzclaude");
    }

    #[tokio::test]
    async fn auth_code_is_single_use() {
        let (pool, _dir) = pool().await;
        let challenge = pkce_pair("v");
        let code = issue_auth_code(&pool, &grant("cid", &challenge), Duration::from_secs(300))
            .await
            .unwrap();
        let redirect = "https://claude.ai/api/mcp/auth_callback";
        consume_auth_code(&pool, &code, "cid", redirect, "v")
            .await
            .unwrap();
        // Second redemption of the same code fails — the row is gone.
        let err = consume_auth_code(&pool, &code, "cid", redirect, "v")
            .await
            .unwrap_err();
        assert!(matches!(err, OAuthError::InvalidGrant));
    }

    #[tokio::test]
    async fn auth_code_rejects_wrong_verifier_client_and_redirect() {
        let (pool, _dir) = pool().await;
        let challenge = pkce_pair("right");
        let redirect = "https://claude.ai/api/mcp/auth_callback";

        // Wrong verifier.
        let code = issue_auth_code(&pool, &grant("cid", &challenge), Duration::from_secs(300))
            .await
            .unwrap();
        assert!(matches!(
            consume_auth_code(&pool, &code, "cid", redirect, "wrong")
                .await
                .unwrap_err(),
            OAuthError::PkceMismatch
        ));

        // Wrong client.
        let code = issue_auth_code(&pool, &grant("cid", &challenge), Duration::from_secs(300))
            .await
            .unwrap();
        assert!(matches!(
            consume_auth_code(&pool, &code, "other", redirect, "right")
                .await
                .unwrap_err(),
            OAuthError::ClientMismatch
        ));

        // Wrong redirect.
        let code = issue_auth_code(&pool, &grant("cid", &challenge), Duration::from_secs(300))
            .await
            .unwrap();
        assert!(matches!(
            consume_auth_code(&pool, &code, "cid", "https://evil/cb", "right")
                .await
                .unwrap_err(),
            OAuthError::RedirectMismatch
        ));
    }

    #[tokio::test]
    async fn expired_auth_code_is_rejected() {
        let (pool, _dir) = pool().await;
        let challenge = pkce_pair("v");
        let code = issue_auth_code(&pool, &grant("cid", &challenge), Duration::from_secs(300))
            .await
            .unwrap();
        // Force the row into the past.
        sqlx::query("UPDATE webagentoauth_codes SET expires_at = ?")
            .bind("2000-01-01T00:00:00+00:00")
            .execute(&pool)
            .await
            .unwrap();
        let err = consume_auth_code(
            &pool,
            &code,
            "cid",
            "https://claude.ai/api/mcp/auth_callback",
            "v",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OAuthError::InvalidGrant));
    }

    fn refresh_grant() -> RefreshGrant {
        RefreshGrant {
            client_id: "cid".to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "claude".to_owned(),
            wiki_id: "franzclaude".to_owned(),
        }
    }

    #[tokio::test]
    async fn refresh_rotate_revokes_old_and_returns_grant() {
        let (pool, _dir) = pool().await;
        let g = refresh_grant();
        let first = issue_refresh_token(&pool, &g, Duration::from_secs(3600))
            .await
            .unwrap();
        let (second, returned) = rotate_refresh_token(&pool, &first, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(returned, g);
        assert_ne!(first, second);
        // The old token no longer rotates (revoked).
        assert!(matches!(
            rotate_refresh_token(&pool, &first, Duration::from_secs(3600))
                .await
                .unwrap_err(),
            OAuthError::InvalidGrant
        ));
        // The new one does.
        rotate_refresh_token(&pool, &second, Duration::from_secs(3600))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn revoke_connection_kills_active_tokens() {
        let (pool, _dir) = pool().await;
        let g = refresh_grant();
        let token = issue_refresh_token(&pool, &g, Duration::from_secs(3600))
            .await
            .unwrap();
        let n = revoke_connection(&pool, "franz", "claude").await.unwrap();
        assert_eq!(n, 1);
        assert!(matches!(
            rotate_refresh_token(&pool, &token, Duration::from_secs(3600))
                .await
                .unwrap_err(),
            OAuthError::InvalidGrant
        ));
    }

    #[tokio::test]
    async fn list_connections_shows_active_and_drops_revoked() {
        let (pool, _dir) = pool().await;
        let a = RefreshGrant {
            client_id: "cid".to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "claude".to_owned(),
            wiki_id: "franz-claude".to_owned(),
        };
        let b = RefreshGrant {
            client_id: "cid".to_owned(),
            sender_id: "franz".to_owned(),
            consumer_id: "chatgpt".to_owned(),
            wiki_id: "franz-chatgpt".to_owned(),
        };
        issue_refresh_token(&pool, &a, Duration::from_secs(3600))
            .await
            .unwrap();
        issue_refresh_token(&pool, &b, Duration::from_secs(3600))
            .await
            .unwrap();
        // Rotating a connection keeps exactly one active row for it (the listing
        // must not double-count the rotated-away token).
        let first = issue_refresh_token(&pool, &a, Duration::from_secs(3600))
            .await
            .unwrap();
        rotate_refresh_token(&pool, &first, Duration::from_secs(3600))
            .await
            .unwrap();

        let conns = list_connections(&pool).await.unwrap();
        assert_eq!(conns.len(), 2, "one row per (sender, consumer, wiki)");
        let ids: Vec<&str> = conns.iter().map(|c| c.consumer_id.as_str()).collect();
        assert!(ids.contains(&"claude") && ids.contains(&"chatgpt"));

        revoke_connection(&pool, "franz", "claude").await.unwrap();
        let conns = list_connections(&pool).await.unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].consumer_id, "chatgpt");
    }

    #[test]
    fn pkce_s256_matches_known_pair() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_pair(verifier);
        assert!(verify_pkce_s256(verifier, &challenge));
        assert!(!verify_pkce_s256("other", &challenge));
    }
}
