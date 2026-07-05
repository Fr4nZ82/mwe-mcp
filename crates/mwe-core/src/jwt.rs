// SPDX-License-Identifier: AGPL-3.0-or-later
//! JWT issue / verify / revoke per [`jwt-and-session-model.md`](../../../wiki/design-notes/jwt-and-session-model.md).
//!
//! ## Design recap
//!
//! - **Payload required**: `sender_id`, `device_label`, `rate_limit_id`,
//!   `iat`, `exp`, `jti` (`UUIDv7`).
//! - **Payload optional**: `isAdmin` (UI gating only — does **not**
//!   bypass ACL), `consumer_id` (for multi-consumer ack tracking).
//! - **No scopes / no permissions**: the token only proves identity;
//!   authorization is region-level ACL via inline markers.
//! - **Signature**: HS256 default (shared secret).
//!   `MWE_TOKEN_SECRET` ≥32 bytes from env. `mwe-mcp init` generates it.
//! - **Default TTLs**: 1 year for "internal" tokens (local device),
//!   30 days for "exposed" tokens (Cloudflare Tunnel).
//! - **Blacklist**: rows in `token_blacklist` keyed by `jti`. An
//!   in-memory cache refreshes every 60s; revocation propagates within
//!   that window.
//!
//! ## Module layout
//!
//! - [`TokenClaims`] — the typed payload, serialized as-is by jsonwebtoken.
//! - [`ConsumerClass`] — smart vs standard consumer class.
//! - [`TokenSecret`] — newtype wrapper with a redacted `Debug` impl so
//!   the secret never leaks into traces.
//! - [`issue`] / [`verify_offline`] / [`verify`] — the three primitives.
//! - [`revoke`] — append a `jti` to `token_blacklist`.
//! - [`BlacklistCache`] — bounded in-memory mirror of the blacklist
//!   with a 60s TTL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Minimum acceptable length of the HMAC secret.
///
/// The spec calls for "≥32 byte random"; we enforce that floor here so
/// a misconfigured `MWE_TOKEN_SECRET` fails loudly at startup instead
/// of silently signing tokens with a weak key.
pub const MIN_SECRET_BYTES: usize = 32;

/// Default TTL for internal tokens (local device, owner-trust).
/// 1 year.
pub const DEFAULT_INTERNAL_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// Default TTL for exposed tokens (Cloudflare Tunnel, third-party
/// device). 30 days.
pub const DEFAULT_EXPOSED_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// In-memory blacklist refresh window: revocation propagates within 60s.
pub const BLACKLIST_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Class of consumer holding the token ([`tool-reference.md`]).
///
/// - `Smart`: consumer with its own LLM subscription (Claude Code,
///   Cowork, any MCP-compatible agent). Authorized for the
///   smart-wiki tool family (`wiki_admin_*`).
/// - `Standard`: conversational consumer that uses the server-side LLM
///   (openclaw, hermes, nanoclaw). Tool surface unchanged from the
///   earlier behavior; plus the read-only `wiki_admin_notify` to
///   append items to a smart-wiki `_briefing.md`.
///
/// Defaults to `Standard` when the claim is **absent** from a JWT, so
/// every token issued before the consumer-class field existed continues
/// to verify unchanged.
///
/// [`tool-reference.md`]: ../../../wiki/protocol/tool-reference.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerClass {
    /// Conversational consumer that uses the server-side LLM via
    /// `wiki_ingest_message` (openclaw, hermes, nanoclaw). Default for
    /// tokens that omit the `consumer_class` claim.
    #[default]
    Standard,
    /// Consumer with its own LLM subscription. Authorized for the
    /// smart-wiki tool family (`wiki_admin_*`).
    Smart,
}

impl ConsumerClass {
    /// Returns `true` for the [`Self::Standard`] variant. Takes
    /// `&self` so it can be used directly as serde's
    /// `skip_serializing_if` predicate, which always passes a
    /// reference; newly-issued standard tokens stay wire-identical to
    /// earlier tokens.
    #[must_use]
    pub const fn is_standard(&self) -> bool {
        matches!(self, Self::Standard)
    }

    /// Returns `true` for the [`Self::Smart`] variant.
    #[must_use]
    pub const fn is_smart(&self) -> bool {
        matches!(self, Self::Smart)
    }
}

/// Connection profile — the consumer's runtime *environment*, orthogonal to
/// [`ConsumerClass`].
///
/// Lets the server tailor the surface it exposes (today the `tools/list`
/// catalog) to what the client can actually use.
///
/// - `Local`: the default. A consumer with a local filesystem / host bridge
///   (Claude Code, hermes, the CLI) — gets the full tool catalog for its class.
/// - `Web`: a bridge-less hosted MCP client with no local filesystem (the
///   claude.ai web app, connected via the `webagentoauth` flow). The server
///   trims tools that assume a local working copy or an out-of-band harness loop.
///
/// Defaults to `Local` when the claim is absent, so every token issued before
/// the field existed — and every non-web consumer — verifies and behaves
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerProfile {
    /// Local-filesystem / bridged consumer. Default; full catalog.
    #[default]
    Local,
    /// Bridge-less hosted web client (claude.ai). Reduced catalog.
    Web,
}

impl ConsumerProfile {
    /// Returns `true` for [`Self::Local`]. Doubles as serde's
    /// `skip_serializing_if` predicate, so a `local` profile never hits the
    /// wire and earlier tokens stay byte-identical.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Returns `true` for [`Self::Web`].
    #[must_use]
    pub const fn is_web(&self) -> bool {
        matches!(self, Self::Web)
    }
}

/// The JWT payload. Field names match the JSON serialization the JWT
/// spec expects (`iat`, `exp`, `jti`); the rest use serde renames to
/// keep the on-wire shape lowercased without affecting Rust naming.
///
/// `is_admin` is serialized as `isAdmin` to match the `enrollment.yaml`
/// convention and the operator-facing JWT body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// User id of the sender that owns the token.
    #[serde(rename = "sender_id")]
    pub sender_id: String,
    /// Human label of the device the token is bound to.
    pub device_label: String,
    /// Rate-limit profile id referenced from `mwe-mcp.config.yaml`.
    pub rate_limit_id: String,
    /// JWT id — `UUIDv7` string. Used as the blacklist key.
    pub jti: String,
    /// Issued-at (Unix timestamp, seconds).
    pub iat: i64,
    /// Expires-at (Unix timestamp, seconds).
    pub exp: i64,
    /// UI gating hint for the built-in dashboard. **Does not** bypass
    /// ACL.
    #[serde(rename = "isAdmin", default)]
    pub is_admin: bool,
    /// Consumer id for multi-consumer event acks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_id: Option<String>,
    /// Consumer class. Omitted on the wire when
    /// [`ConsumerClass::Standard`] so earlier tokens remain
    /// wire-identical.
    #[serde(default, skip_serializing_if = "ConsumerClass::is_standard")]
    pub consumer_class: ConsumerClass,
    /// Connection profile (local vs hosted-web). Omitted on the wire when
    /// [`ConsumerProfile::Local`] so earlier tokens stay wire-identical.
    #[serde(default, skip_serializing_if = "ConsumerProfile::is_local")]
    pub profile: ConsumerProfile,
}

impl TokenClaims {
    /// Construct a fresh claims set with a server-generated `UUIDv7`
    /// `jti` and `iat = now`, `exp = now + ttl`.
    #[must_use]
    pub fn new(
        sender_id: impl Into<String>,
        device_label: impl Into<String>,
        rate_limit_id: impl Into<String>,
        ttl: Duration,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        let exp = now + i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        Self {
            sender_id: sender_id.into(),
            device_label: device_label.into(),
            rate_limit_id: rate_limit_id.into(),
            jti: uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string(),
            iat: now,
            exp,
            is_admin: false,
            consumer_id: None,
            consumer_class: ConsumerClass::Standard,
            profile: ConsumerProfile::Local,
        }
    }
}

/// Newtype around the HMAC secret bytes. Does not implement `Display`
/// and overrides `Debug` to render `<redacted>` so a misplaced
/// `tracing::debug!` call cannot leak it.
#[derive(Clone)]
pub struct TokenSecret(Arc<Vec<u8>>);

impl TokenSecret {
    /// Build a secret from raw bytes. Returns an error if shorter than
    /// [`MIN_SECRET_BYTES`].
    pub fn new(bytes: Vec<u8>) -> Result<Self, TokenError> {
        if bytes.len() < MIN_SECRET_BYTES {
            return Err(TokenError::WeakSecret {
                bytes: bytes.len(),
                min: MIN_SECRET_BYTES,
            });
        }
        Ok(Self(Arc::new(bytes)))
    }

    /// Generate a fresh 32-byte random secret straight from the OS
    /// RNG (`getrandom`). The 32-byte floor matches the spec; longer
    /// secrets are accepted by [`new`] but `generate` always returns
    /// exactly [`MIN_SECRET_BYTES`].
    ///
    /// # Panics
    ///
    /// Panics if the OS RNG is unavailable — at startup time this is
    /// the right behavior: we cannot safely issue tokens without
    /// entropy, and surfacing it as a startup-time abort beats
    /// silently degrading.
    #[must_use]
    pub fn generate() -> Self {
        let mut buf = vec![0u8; MIN_SECRET_BYTES];
        getrandom::getrandom(&mut buf).expect("OS RNG must be available at startup");
        Self(Arc::new(buf))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Length of the secret in bytes. Always ≥ [`MIN_SECRET_BYTES`].
    #[must_use]
    #[allow(clippy::len_without_is_empty)] // secrets are never empty by construction
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Hex-encode the secret bytes for one-shot export at
    /// `mwe-mcp init` time. **Audit lens**: every caller of this method
    /// is shipping the secret to the operator's terminal — keep them
    /// few and obvious. The CLI uses it exactly once, in the init
    /// success path.
    #[must_use]
    pub fn export_hex(&self) -> String {
        hex::encode_upper(&*self.0)
    }
}

impl std::fmt::Debug for TokenSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSecret")
            .field("len", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

/// JWT-layer errors. Kept distinct from [`crate::Error`] so the
/// transport layer can map each variant to a specific HTTP/MCP code.
#[derive(Debug, Error)]
pub enum TokenError {
    /// Supplied secret is shorter than [`MIN_SECRET_BYTES`].
    #[error("weak secret: {bytes} bytes (minimum {min} for HS256)")]
    WeakSecret {
        /// Length of the secret that was passed in.
        bytes: usize,
        /// Minimum accepted length.
        min: usize,
    },
    /// Token does not parse as a JWT (header/payload malformed).
    #[error("malformed token: {0}")]
    Malformed(String),
    /// Token parses but signature does not match the secret, or the
    /// algorithm header does not match what we accept.
    #[error("invalid signature or algorithm")]
    InvalidSignature,
    /// Token parses but `exp` is in the past.
    #[error("token expired (exp={exp}, now={now})")]
    Expired {
        /// `exp` claim from the token.
        exp: i64,
        /// Wall clock used for the comparison.
        now: i64,
    },
    /// Token parses but its `jti` appears in `token_blacklist`.
    #[error("token revoked (jti={jti})")]
    Revoked {
        /// The blacklisted JWT id.
        jti: String,
    },
    /// Anything else from `jsonwebtoken` we did not categorize.
    #[error("jwt error: {0}")]
    Other(String),
    /// DB error while consulting the blacklist or recording a revoke.
    #[error("jwt db error: {0}")]
    Db(#[from] sqlx::Error),
}

impl From<jsonwebtoken::errors::Error> for TokenError {
    fn from(e: jsonwebtoken::errors::Error) -> Self {
        use jsonwebtoken::errors::ErrorKind;
        match e.kind() {
            ErrorKind::InvalidSignature | ErrorKind::InvalidAlgorithm => Self::InvalidSignature,
            ErrorKind::ExpiredSignature => Self::Expired { exp: 0, now: 0 },
            ErrorKind::InvalidToken
            | ErrorKind::Base64(_)
            | ErrorKind::Json(_)
            | ErrorKind::Utf8(_) => Self::Malformed(e.to_string()),
            _ => Self::Other(e.to_string()),
        }
    }
}

/// Sign `claims` with `secret` (HS256) and return the encoded token.
pub fn issue(secret: &TokenSecret, claims: &TokenClaims) -> Result<String, TokenError> {
    let header = Header::new(Algorithm::HS256);
    let key = EncodingKey::from_secret(secret.bytes());
    Ok(encode(&header, claims, &key)?)
}

/// Verify `token` against `secret` and the JWT-level rules
/// (signature, algorithm, exp). Does **not** consult the blacklist.
/// Use [`verify`] when the DB is available.
pub fn verify_offline(secret: &TokenSecret, token: &str) -> Result<TokenClaims, TokenError> {
    let mut validation = Validation::new(Algorithm::HS256);
    // We do not use `iss`/`aud` claims today; explicit empty sets keep
    // jsonwebtoken from rejecting tokens that lack them.
    validation.required_spec_claims = std::collections::HashSet::from(["exp".to_owned()]);
    validation.validate_exp = true;
    let key = DecodingKey::from_secret(secret.bytes());
    let data = decode::<TokenClaims>(token, &key, &validation)?;
    Ok(data.claims)
}

/// Verify `token` and additionally consult the blacklist via `cache`.
/// On a stale cache the cache refreshes itself from the DB.
pub async fn verify(
    secret: &TokenSecret,
    token: &str,
    pool: &SqlitePool,
    cache: &BlacklistCache,
) -> Result<TokenClaims, TokenError> {
    let claims = verify_offline(secret, token)?;
    if cache.is_revoked(pool, &claims.jti).await? {
        return Err(TokenError::Revoked {
            jti: claims.jti.clone(),
        });
    }
    Ok(claims)
}

/// Insert a revocation row in `token_blacklist`.
///
/// `original_exp` is the token's `exp` claim — stored as
/// `expires_at` so a periodic GC job can drop entries that could no
/// longer authenticate anyway. `revoked_by` is the actor (user id or
/// `"system"` for automated cleanups).
pub async fn revoke(
    pool: &SqlitePool,
    jti: &str,
    reason: &str,
    revoked_by: &str,
    original_exp: i64,
) -> Result<(), TokenError> {
    let now = chrono::Utc::now().to_rfc3339();
    let exp_iso = chrono::DateTime::<chrono::Utc>::from_timestamp(original_exp, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();
    sqlx::query(
        "INSERT OR REPLACE INTO token_blacklist (jti, revoked_at, expires_at, reason, revoked_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(jti)
    .bind(now)
    .bind(exp_iso)
    .bind(reason)
    .bind(revoked_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Single-use variant of [`revoke`]: blacklist `jti` only if absent.
///
/// Returns `Ok(true)` on a fresh insert (the caller "won" the
/// redemption) and `Ok(false)` when the row already existed (the token
/// was already redeemed / revoked).
///
/// Backs the single-use dashboard magic-link: a plain `INSERT` against
/// the `jti` primary key makes the DB the serialization point, so two
/// concurrent redemptions of the same link cannot both win. After a
/// `true`, the caller should [`BlacklistCache::refresh`] so the new
/// entry is visible to the very next `verify` (closing the 60s cache
/// window).
pub async fn revoke_once(
    pool: &SqlitePool,
    jti: &str,
    reason: &str,
    revoked_by: &str,
    original_exp: i64,
) -> Result<bool, TokenError> {
    let now = chrono::Utc::now().to_rfc3339();
    let exp_iso = chrono::DateTime::<chrono::Utc>::from_timestamp(original_exp, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();
    let res = sqlx::query(
        "INSERT INTO token_blacklist (jti, revoked_at, expires_at, reason, revoked_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(jti)
    .bind(now)
    .bind(exp_iso)
    .bind(reason)
    .bind(revoked_by)
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(dberr)) if dberr.is_unique_violation() => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// In-memory snapshot of the blacklist with a [`BLACKLIST_REFRESH_INTERVAL`] TTL.
///
/// The snapshot is the *full* blacklist (all `jti`s, including those
/// past their `expires_at`). The cache exists to make verify
/// O(1) — for the deployment sizes we target (single home miniPC, low
/// thousands of revoked tokens at most) keeping the whole set in
/// memory is fine; if it grows we can switch to a Bloom-filter +
/// negative-cache hybrid.
pub struct BlacklistCache {
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    revoked: std::collections::HashSet<String>,
    refreshed_at: Option<Instant>,
}

impl BlacklistCache {
    /// Build an empty cache. The first call to [`is_revoked`] will
    /// populate it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                revoked: std::collections::HashSet::new(),
                refreshed_at: None,
            }),
        }
    }

    /// Return `true` if `jti` is in the blacklist. Refreshes the cache
    /// from the DB when stale or empty.
    pub async fn is_revoked(&self, pool: &SqlitePool, jti: &str) -> Result<bool, TokenError> {
        let needs_refresh = {
            let guard = self.inner.lock();
            guard
                .refreshed_at
                .is_none_or(|t| t.elapsed() >= BLACKLIST_REFRESH_INTERVAL)
        };

        if needs_refresh {
            self.refresh(pool).await?;
        }

        let guard = self.inner.lock();
        Ok(guard.revoked.contains(jti))
    }

    /// Force a reload from the DB. Useful right after a revoke when
    /// the caller wants the new entry to take effect immediately
    /// (before the 60s window).
    pub async fn refresh(&self, pool: &SqlitePool) -> Result<(), TokenError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT jti FROM token_blacklist")
            .fetch_all(pool)
            .await?;
        {
            let mut guard = self.inner.lock();
            guard.revoked = rows.into_iter().collect();
            guard.refreshed_at = Some(Instant::now());
        }
        Ok(())
    }
}

impl Default for BlacklistCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_secret() -> TokenSecret {
        // 32 bytes of deterministic test material — never use this in
        // anything that actually authenticates a user.
        TokenSecret::new(vec![0xABu8; MIN_SECRET_BYTES]).expect("fixed secret")
    }

    #[test]
    fn weak_secret_is_rejected() {
        let err = TokenSecret::new(vec![1u8; 10]).expect_err("too short");
        assert!(matches!(err, TokenError::WeakSecret { bytes: 10, .. }));
    }

    #[test]
    fn generate_produces_min_length_secret() {
        let s = TokenSecret::generate();
        assert_eq!(s.bytes().len(), MIN_SECRET_BYTES);
    }

    #[test]
    fn secret_debug_does_not_leak_bytes() {
        let s = fixed_secret();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("ab"), "raw hex must not appear in Debug");
    }

    #[test]
    fn issue_then_verify_roundtrips() {
        let s = fixed_secret();
        let claims = TokenClaims::new(
            "alice",
            "claude-code-pclavoro",
            "default",
            DEFAULT_INTERNAL_TTL,
        );
        let token = issue(&s, &claims).expect("issue");
        let verified = verify_offline(&s, &token).expect("verify");

        assert_eq!(verified.sender_id, "alice");
        assert_eq!(verified.device_label, "claude-code-pclavoro");
        assert_eq!(verified.rate_limit_id, "default");
        assert_eq!(verified.jti, claims.jti);
        assert_eq!(verified.iat, claims.iat);
        assert_eq!(verified.exp, claims.exp);
        assert!(!verified.is_admin);
        assert!(verified.consumer_id.is_none());
        assert_eq!(
            verified.consumer_class,
            ConsumerClass::Standard,
            "fresh claims default to standard consumer class"
        );
    }

    #[test]
    fn wrong_secret_fails_verify() {
        let s1 = fixed_secret();
        let s2 = TokenSecret::new(vec![0x12u8; MIN_SECRET_BYTES]).unwrap();
        let claims = TokenClaims::new("alice", "dev", "default", DEFAULT_INTERNAL_TTL);
        let token = issue(&s1, &claims).unwrap();
        let err = verify_offline(&s2, &token).expect_err("must reject");
        assert!(matches!(err, TokenError::InvalidSignature));
    }

    #[test]
    fn expired_token_fails_verify() {
        let s = fixed_secret();
        let mut claims = TokenClaims::new("alice", "dev", "default", Duration::from_secs(1));
        // Force the token into the past.
        claims.iat -= 3600;
        claims.exp -= 3600;
        let token = issue(&s, &claims).unwrap();
        let err = verify_offline(&s, &token).expect_err("must reject");
        assert!(matches!(err, TokenError::Expired { .. }), "got {err:?}");
    }

    #[test]
    fn is_admin_and_consumer_id_roundtrip_when_set() {
        let s = fixed_secret();
        let mut claims = TokenClaims::new("frodo", "tg-bot", "exposed", DEFAULT_EXPOSED_TTL);
        claims.is_admin = true;
        claims.consumer_id = Some("samvise-prod".to_owned());
        let token = issue(&s, &claims).unwrap();
        let v = verify_offline(&s, &token).unwrap();
        assert!(v.is_admin);
        assert_eq!(v.consumer_id.as_deref(), Some("samvise-prod"));
    }

    #[test]
    fn consumer_class_smart_roundtrips_when_set() {
        let s = fixed_secret();
        let mut claims = TokenClaims::new(
            "user:alice",
            "claude-code home laptop",
            "default",
            DEFAULT_INTERNAL_TTL,
        );
        claims.consumer_class = ConsumerClass::Smart;
        claims.consumer_id = Some("cc-laptop".to_owned());
        let token = issue(&s, &claims).unwrap();
        let v = verify_offline(&s, &token).unwrap();
        assert_eq!(v.consumer_class, ConsumerClass::Smart);
        assert!(v.consumer_class.is_smart());
        assert_eq!(v.consumer_id.as_deref(), Some("cc-laptop"));
    }

    #[test]
    fn consumer_class_absent_in_legacy_token_deserialises_as_standard() {
        // Hand-craft a legacy JWT body (no `consumer_class` field)
        // and confirm serde fills the default `Standard` variant.
        let s = fixed_secret();
        let now = chrono::Utc::now().timestamp();
        let legacy_body = serde_json::json!({
            "sender_id": "user:legacy",
            "device_label": "legacy bot",
            "rate_limit_id": "default",
            "jti": "legacy-jti-1",
            "iat": now,
            "exp": now + 3600,
            "isAdmin": false,
        });
        // Sign through the same library so verify_offline accepts it.
        let header = Header::new(Algorithm::HS256);
        let key = EncodingKey::from_secret(s.bytes());
        let token = encode(&header, &legacy_body, &key).expect("encode legacy");

        let v = verify_offline(&s, &token).expect("verify legacy token");
        assert_eq!(
            v.consumer_class,
            ConsumerClass::Standard,
            "missing claim must default to Standard for backward compat"
        );
        assert!(v.consumer_class.is_standard());
    }

    #[test]
    fn standard_consumer_class_is_omitted_in_json_serialization() {
        // Newly-issued standard tokens must remain wire-identical (in
        // the JSON payload shape) to earlier tokens so existing
        // decoders and audit tooling do not see a spurious new field.
        let claims = TokenClaims::new("user:alice", "openclaw", "default", DEFAULT_INTERNAL_TTL);
        assert_eq!(claims.consumer_class, ConsumerClass::Standard);
        let json = serde_json::to_string(&claims).expect("serialize");
        assert!(
            !json.contains("consumer_class"),
            "standard class must skip serialization, got: {json}"
        );
    }

    #[test]
    fn smart_consumer_class_serialises_lowercase() {
        // Smart class on the wire must be the JSON string "smart"
        // (lowercase) so spec readers and external auditors see the
        // exact form documented in protocollo.md §2.
        let mut claims =
            TokenClaims::new("user:alice", "cc-laptop", "default", DEFAULT_INTERNAL_TTL);
        claims.consumer_class = ConsumerClass::Smart;
        let json = serde_json::to_string(&claims).expect("serialize");
        assert!(
            json.contains("\"consumer_class\":\"smart\""),
            "wire form must be lowercase, got: {json}"
        );
    }

    #[tokio::test]
    async fn revoke_inserts_into_blacklist() {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = crate::db::open_or_init(dir.path()).await.expect("open db");

        revoke(
            &pool,
            "jti-1",
            "manual",
            "alice",
            chrono::Utc::now().timestamp() + 3600,
        )
        .await
        .expect("revoke");

        let (jti, reason, revoked_by): (String, String, Option<String>) = sqlx::query_as(
            "SELECT jti, reason, revoked_by FROM token_blacklist WHERE jti = 'jti-1'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch");
        assert_eq!(jti, "jti-1");
        assert_eq!(reason, "manual");
        assert_eq!(revoked_by.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn revoke_once_is_a_compare_and_set() {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = crate::db::open_or_init(dir.path()).await.expect("open db");
        let exp = chrono::Utc::now().timestamp() + 3600;

        // First burn wins.
        let first = revoke_once(&pool, "link-jti", "dashboard_link_redeemed", "frodo", exp)
            .await
            .expect("first revoke_once");
        assert!(first, "first redemption must win");

        // Second burn of the same jti loses (already present).
        let second = revoke_once(&pool, "link-jti", "dashboard_link_redeemed", "frodo", exp)
            .await
            .expect("second revoke_once");
        assert!(!second, "replay must lose the compare-and-set");

        // And the jti is now blacklisted, so a verify against it fails.
        let cache = BlacklistCache::new();
        cache.refresh(&pool).await.expect("refresh");
        assert!(
            cache
                .is_revoked(&pool, "link-jti")
                .await
                .expect("is_revoked")
        );
    }

    #[tokio::test]
    async fn blacklist_cache_refreshes_on_first_call() {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = crate::db::open_or_init(dir.path()).await.expect("open db");
        let cache = BlacklistCache::new();

        revoke(
            &pool,
            "jti-x",
            "test",
            "system",
            chrono::Utc::now().timestamp() + 3600,
        )
        .await
        .expect("revoke");

        // First call populates the cache and returns true.
        assert!(cache.is_revoked(&pool, "jti-x").await.expect("revoked"));
        // Unknown jti returns false.
        assert!(
            !cache
                .is_revoked(&pool, "unknown")
                .await
                .expect("not revoked")
        );
    }

    #[tokio::test]
    async fn verify_rejects_revoked_token_via_cache() {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = crate::db::open_or_init(dir.path()).await.expect("open db");
        let secret = fixed_secret();
        let cache = BlacklistCache::new();

        let claims = TokenClaims::new("alice", "dev", "default", DEFAULT_INTERNAL_TTL);
        let token = issue(&secret, &claims).unwrap();

        // First verify works.
        verify(&secret, &token, &pool, &cache).await.expect("ok");

        // Revoke + force refresh so the new entry is visible immediately.
        revoke(&pool, &claims.jti, "test", "alice", claims.exp)
            .await
            .expect("revoke");
        cache.refresh(&pool).await.expect("refresh");

        let err = verify(&secret, &token, &pool, &cache)
            .await
            .expect_err("must reject");
        match err {
            TokenError::Revoked { jti } => assert_eq!(jti, claims.jti),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_refresh_is_explicit_so_revoke_propagates_within_60s() {
        // Revocation propagates within 60s. The cache TTL handles
        // the *upper bound*; this test asserts the explicit `refresh`
        // is available for callers that want immediate propagation.
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let pool = crate::db::open_or_init(dir.path()).await.expect("open db");
        let cache = BlacklistCache::new();

        // Populate cache with empty state.
        assert!(!cache.is_revoked(&pool, "any").await.unwrap());

        // Add a revoke without refreshing — cache still says false.
        revoke(
            &pool,
            "jti-z",
            "test",
            "system",
            chrono::Utc::now().timestamp() + 3600,
        )
        .await
        .unwrap();
        assert!(
            !cache.is_revoked(&pool, "jti-z").await.unwrap(),
            "stale cache (within 60s window) does not see the new entry"
        );

        // Explicit refresh — now it shows up.
        cache.refresh(&pool).await.unwrap();
        assert!(cache.is_revoked(&pool, "jti-z").await.unwrap());
    }
}
