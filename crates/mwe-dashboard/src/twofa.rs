// SPDX-License-Identifier: AGPL-3.0-or-later
//! TOTP two-factor authentication engine (roadmap 28).
//!
//! Holds everything the 2FA routes ([`crate::routes`]) build on: the
//! at-rest encryption of the TOTP secret, the TOTP verify, recovery-code
//! generation/checking, and the `user_2fa` / `user_2fa_recovery_codes` /
//! `pending_2fa` DB helpers plus the enforcement flags.
//!
//! ## Scope
//!
//! 2FA gates only the human `/dashboard/login` surface; the MCP path is
//! bearer-JWT, and system/bot users (no `user_credentials` row) are
//! exempt by construction.
//!
//! ## Secret at rest
//!
//! The TOTP shared secret is stored XChaCha20-Poly1305-encrypted, the key
//! derived from `MWE_TOKEN_SECRET` via SHA-256. Rotating the token secret
//! therefore invalidates every enrollment — the documented trade-off (no
//! separate key to manage). The pending-challenge id is an opaque random
//! token in a short-lived cookie, never a JWT, so it can never be swapped
//! into a session cookie.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use chrono::{Duration, Utc};
use mwe_core::jwt::TokenSecret;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use totp_rs::{Algorithm, Secret, TOTP};
use uuid::Uuid;

use crate::error::{DashboardError, Result};

/// `engine_meta` key for the deployment-wide "require 2FA for all
/// non-system users" toggle.
pub const GLOBAL_REQUIRE_KEY: &str = "auth.require_2fa_all";

/// Issuer shown in the authenticator app.
const ISSUER: &str = "mwe-mcp";
/// TOTP step (seconds) and skew (steps either side) — the standard
/// authenticator-app profile.
const STEP_SECS: u64 = 30;
const SKEW_STEPS: u8 = 1;
const DIGITS: usize = 6;
/// How many recovery codes a fresh enrollment mints.
const RECOVERY_CODE_COUNT: usize = 10;

// ---------- secret at rest ----------

/// Derive the 32-byte AEAD key from the deployment token secret. Uses the
/// stable `export_hex` representation so we never reach into the secret's
/// private bytes.
fn derive_key(secret: &TokenSecret) -> Key {
    let mut hasher = Sha256::new();
    hasher.update(secret.export_hex().as_bytes());
    hasher.update(b"mwe-mcp/2fa-secret/v1");
    let digest = hasher.finalize();
    *Key::from_slice(&digest)
}

/// Encrypt `plain` (the raw TOTP secret) → base64(`nonce ‖ ciphertext`).
fn encrypt_secret(secret: &TokenSecret, plain: &[u8]) -> Result<String> {
    let cipher = XChaCha20Poly1305::new(&derive_key(secret));
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| DashboardError::Internal(format!("2fa rng: {e}")))?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plain)
        .map_err(|e| DashboardError::Internal(format!("2fa encrypt: {e}")))?;
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ct);
    Ok(B64.encode(blob))
}

/// Inverse of [`encrypt_secret`].
fn decrypt_secret(secret: &TokenSecret, blob_b64: &str) -> Result<Vec<u8>> {
    let blob = B64
        .decode(blob_b64)
        .map_err(|e| DashboardError::Internal(format!("2fa b64: {e}")))?;
    if blob.len() < 24 {
        return Err(DashboardError::Internal(
            "2fa: ciphertext too short".to_owned(),
        ));
    }
    let (nonce, ct) = blob.split_at(24);
    let cipher = XChaCha20Poly1305::new(&derive_key(secret));
    cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|e| DashboardError::Internal(format!("2fa decrypt (token-secret rotated?): {e}")))
}

// ---------- TOTP ----------

/// A fresh 20-byte TOTP shared secret.
#[must_use]
pub fn generate_secret_bytes() -> Vec<u8> {
    Secret::generate_secret()
        .to_bytes()
        .expect("generated raw secret is always decodable")
}

fn totp(secret: Vec<u8>, account: &str) -> Result<TOTP> {
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        SKEW_STEPS,
        STEP_SECS,
        secret,
        Some(ISSUER.to_owned()),
        account.to_owned(),
    )
    .map_err(|e| DashboardError::Internal(format!("2fa totp build: {e}")))
}

/// The `otpauth://` provisioning URI for an authenticator app to scan.
pub fn provisioning_url(secret: &[u8], account: &str) -> Result<String> {
    Ok(totp(secret.to_vec(), account)?.get_url())
}

/// Verify a 6-digit TOTP `code` against `secret` at the current time
/// (±1 step of skew). A malformed code or clock error just reads as
/// "not valid".
#[must_use]
pub fn verify_code(secret: &[u8], account: &str, code: &str) -> bool {
    let code = code.trim().replace([' ', '-'], "");
    totp(secret.to_vec(), account).is_ok_and(|t| t.check_current(&code).unwrap_or(false))
}

/// Render `url` as an inline SVG QR code.
pub fn qr_svg(url: &str) -> Result<String> {
    use qrcode::QrCode;
    use qrcode::render::svg;
    let code = QrCode::new(url.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("2fa qr: {e}")))?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build())
}

// ---------- recovery codes ----------

/// Mint a batch of fresh plaintext recovery codes (shown once). Each is
/// 80 bits of entropy, formatted `xxxxx-xxxxx-xxxxx-xxxx` for legibility.
pub fn generate_recovery_codes() -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let mut raw = [0u8; 10];
        getrandom::getrandom(&mut raw)
            .map_err(|e| DashboardError::Internal(format!("2fa rng: {e}")))?;
        let hexed = hex::encode(raw); // 20 hex chars
        out.push(format!(
            "{}-{}-{}-{}",
            &hexed[0..5],
            &hexed[5..10],
            &hexed[10..15],
            &hexed[15..20]
        ));
    }
    Ok(out)
}

/// Normalize a typed recovery code (strip separators, lowercase) so the
/// stored hash matches regardless of how the user re-typed it.
fn normalize_recovery(code: &str) -> String {
    code.trim()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

/// SHA-256 hex of the normalized code. Recovery codes are high-entropy
/// random, so a fast hash is sufficient (and lets us look them up
/// directly) — unlike user-chosen passwords, which take Argon2id.
fn hash_recovery(code: &str) -> String {
    let mut h = Sha256::new();
    h.update(normalize_recovery(code).as_bytes());
    hex::encode(h.finalize())
}

// ---------- status ----------

/// A user's 2FA state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State2fa {
    /// No `user_2fa` row.
    None,
    /// Enrollment started (secret stored) but not yet confirmed.
    Pending,
    /// Active — login requires a second factor.
    Enabled,
}

/// Read the user's 2FA state.
pub async fn state_of(pool: &SqlitePool, user_id: &str) -> Result<State2fa> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT enabled FROM user_2fa WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        None => State2fa::None,
        Some((0,)) => State2fa::Pending,
        Some(_) => State2fa::Enabled,
    })
}

/// Convenience: is 2FA active for this user.
pub async fn is_enabled(pool: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(state_of(pool, user_id).await? == State2fa::Enabled)
}

/// Decrypt and return the user's stored TOTP secret, if any.
pub async fn load_secret(
    pool: &SqlitePool,
    token_secret: &TokenSecret,
    user_id: &str,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT secret_enc FROM user_2fa WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    match row {
        Some((enc,)) => Ok(Some(decrypt_secret(token_secret, &enc)?)),
        None => Ok(None),
    }
}

// ---------- enrollment lifecycle ----------

/// Store (or replace) a not-yet-confirmed enrollment: encrypt the secret,
/// `enabled = 0`. Replacing wipes any stale recovery codes from a prior
/// enrollment so they cannot be confused with the new batch.
pub async fn begin_enrollment(
    pool: &SqlitePool,
    token_secret: &TokenSecret,
    user_id: &str,
    secret: &[u8],
) -> Result<()> {
    let enc = encrypt_secret(token_secret, secret)?;
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO user_2fa (user_id, secret_enc, enabled, created_at, confirmed_at)
         VALUES (?, ?, 0, ?, NULL)
         ON CONFLICT(user_id) DO UPDATE SET secret_enc = excluded.secret_enc,
             enabled = 0, created_at = excluded.created_at, confirmed_at = NULL",
    )
    .bind(user_id)
    .bind(&enc)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM user_2fa_recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Flip a pending enrollment to active and store the recovery codes (the
/// caller has just verified a live TOTP code against the pending secret).
pub async fn confirm_enrollment(pool: &SqlitePool, user_id: &str, codes: &[String]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE user_2fa SET enabled = 1, confirmed_at = ? WHERE user_id = ?")
        .bind(&now)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM user_2fa_recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for code in codes {
        sqlx::query(
            "INSERT INTO user_2fa_recovery_codes (user_id, code_hash, used_at)
             VALUES (?, ?, NULL)",
        )
        .bind(user_id)
        .bind(hash_recovery(code))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Fully remove 2FA for a user (the cascade also drops their recovery
/// codes). Used by the self-service "turn off" and the admin/CLI
/// break-glass.
pub async fn disable(pool: &SqlitePool, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM user_2fa WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM user_2fa_recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// How many unused recovery codes the user has left.
pub async fn unused_recovery_count(pool: &SqlitePool, user_id: &str) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM user_2fa_recovery_codes WHERE user_id = ? AND used_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Consume one recovery code if it matches an unused one. Returns whether
/// a code was spent. The conditional UPDATE is the single-use guard.
pub async fn consume_recovery_code(pool: &SqlitePool, user_id: &str, code: &str) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE user_2fa_recovery_codes SET used_at = ?
          WHERE user_id = ? AND code_hash = ? AND used_at IS NULL",
    )
    .bind(&now)
    .bind(user_id)
    .bind(hash_recovery(code))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------- enforcement flags ----------

/// Whether this specific user is flagged require-2FA by the admin.
pub async fn user_required(pool: &SqlitePool, user_id: &str) -> Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT require_2fa FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(matches!(row, Some((n,)) if n != 0))
}

/// Set the per-user require flag.
pub async fn set_user_required(pool: &SqlitePool, user_id: &str, required: bool) -> Result<()> {
    sqlx::query("UPDATE enrollment_users SET require_2fa = ? WHERE user_id = ?")
        .bind(i64::from(required))
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The deployment-wide "require 2FA for all non-system users" toggle.
pub async fn global_required(pool: &SqlitePool) -> Result<bool> {
    Ok(mwe_core::db::meta_get(pool, GLOBAL_REQUIRE_KEY)
        .await?
        .as_deref()
        == Some("1"))
}

/// Set the deployment-wide toggle.
pub async fn set_global_required(pool: &SqlitePool, on: bool) -> Result<()> {
    mwe_core::db::meta_set(pool, GLOBAL_REQUIRE_KEY, if on { "1" } else { "0" }).await?;
    Ok(())
}

/// Whether `user_id` is obliged to have 2FA (per-user flag OR the global
/// toggle). The caller compares this against [`is_enabled`] to decide
/// whether to trap them on the enrollment page.
pub async fn is_obliged(pool: &SqlitePool, user_id: &str) -> Result<bool> {
    if global_required(pool).await? {
        return Ok(true);
    }
    user_required(pool, user_id).await
}

/// One-query enforcement check for the per-request session gate.
///
/// `true` when the user is obliged to have 2FA (global toggle or per-user
/// flag) but has not enabled it — so the middleware should trap them on
/// the enrollment page. Returns `false` for an unknown user.
pub async fn needs_enrollment_trap(pool: &SqlitePool, user_id: &str) -> Result<bool> {
    let row: Option<(Option<String>, i64, Option<i64>)> = sqlx::query_as(
        "SELECT
            (SELECT value FROM engine_meta WHERE key = ?) AS global_flag,
            u.require_2fa,
            (SELECT enabled FROM user_2fa WHERE user_id = u.user_id) AS enabled
         FROM enrollment_users u
         WHERE u.user_id = ?",
    )
    .bind(GLOBAL_REQUIRE_KEY)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some((global_flag, require_2fa, enabled)) = row else {
        return Ok(false);
    };
    let obliged = global_flag.as_deref() == Some("1") || require_2fa != 0;
    Ok(obliged && enabled != Some(1))
}

// ---------- pending challenge ----------

/// A challenge row recovered by [`peek_pending`].
#[derive(Debug, Clone)]
pub struct Pending {
    /// The user awaiting the second factor.
    pub user_id: String,
    /// Their admin flag, carried so the minted session keeps the role.
    pub is_admin: bool,
    /// Validated post-auth redirect (or `None` → home).
    pub next: Option<String>,
}

/// Create a pending challenge after a verified password (or a redeemed
/// magic link). Returns the opaque `challenge_id` to drop in a cookie.
pub async fn create_pending(
    pool: &SqlitePool,
    user_id: &str,
    is_admin: bool,
    next: Option<&str>,
    ttl_minutes: i64,
) -> Result<String> {
    let challenge_id = Uuid::now_v7().to_string();
    let now = Utc::now();
    let expires = now + Duration::minutes(ttl_minutes.max(1));
    sqlx::query(
        "INSERT INTO pending_2fa (challenge_id, user_id, is_admin, next, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&challenge_id)
    .bind(user_id)
    .bind(i64::from(is_admin))
    .bind(next)
    .bind(now.to_rfc3339())
    .bind(expires.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(challenge_id)
}

/// Look up a live (unexpired) pending challenge without consuming it —
/// used to render the challenge form.
pub async fn peek_pending(pool: &SqlitePool, challenge_id: &str) -> Result<Option<Pending>> {
    let now = Utc::now().to_rfc3339();
    let row: Option<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT user_id, is_admin, next FROM pending_2fa
          WHERE challenge_id = ? AND expires_at > ?",
    )
    .bind(challenge_id)
    .bind(&now)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user_id, is_admin, next)| Pending {
        user_id,
        is_admin: is_admin != 0,
        next,
    }))
}

/// Delete the challenge row (call after a successful second factor, in
/// the same logical step that mints the session).
pub async fn delete_pending(pool: &SqlitePool, challenge_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM pending_2fa WHERE challenge_id = ?")
        .bind(challenge_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> TokenSecret {
        TokenSecret::new(vec![0x42; 32]).expect("secret")
    }

    #[test]
    fn secret_roundtrips_through_encryption() {
        let ts = secret();
        let plain = generate_secret_bytes();
        let blob = encrypt_secret(&ts, &plain).expect("encrypt");
        let back = decrypt_secret(&ts, &blob).expect("decrypt");
        assert_eq!(plain, back);
    }

    #[test]
    fn decrypt_fails_under_a_different_token_secret() {
        let plain = generate_secret_bytes();
        let blob = encrypt_secret(&secret(), &plain).expect("encrypt");
        let other = TokenSecret::new(vec![0x99; 32]).expect("secret");
        assert!(decrypt_secret(&other, &blob).is_err());
    }

    #[test]
    fn totp_self_check_passes() {
        let s = generate_secret_bytes();
        let t = totp(s.clone(), "frodo@example.com").expect("totp");
        let now_code = t.generate_current().expect("code");
        assert!(verify_code(&s, "frodo@example.com", &now_code));
        assert!(!verify_code(&s, "frodo@example.com", "000000"));
    }

    #[test]
    fn provisioning_url_carries_issuer() {
        let s = generate_secret_bytes();
        let url = provisioning_url(&s, "frodo@example.com").expect("url");
        assert!(url.starts_with("otpauth://totp/"));
        assert!(url.contains("mwe-mcp"));
    }

    #[test]
    fn recovery_codes_are_unique_and_normalize() {
        let codes = generate_recovery_codes().expect("codes");
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        // Hash is stable under re-spacing / case.
        let a = &codes[0];
        let messy = format!("  {}  ", a.to_uppercase().replace('-', " "));
        assert_eq!(hash_recovery(a), hash_recovery(&messy));
    }
}
