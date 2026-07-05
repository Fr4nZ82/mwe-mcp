// SPDX-License-Identifier: AGPL-3.0-or-later
//! Argon2id password hashing for the dashboard login flows
//! (see [JWT and session model](../../../../wiki/design-notes/jwt-and-session-model.md)).
//!
//! The hash format is PHC, which embeds the algorithm id, parameters,
//! and per-row salt in the string itself — so a future parameter bump
//! only affects newly hashed passwords and old hashes keep verifying
//! correctly (the `Argon2` verifier reads the cost from the stored
//! string, not from our defaults).
//!
//! Parameters default to the OWASP "Argon2id m=19456 KiB, t=2, p=1"
//! profile from the password-storage cheat sheet — sized to be
//! affordable on a homelab miniPC while still demanding ~50–100 ms on
//! commodity hardware. The defaults are hardcoded so the setup wizard
//! works out-of-the-box; making them tunable from `mwe-mcp.config.yaml`
//! is left for later.

use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};

use crate::error::DashboardError;

/// OWASP "low-memory but still expensive" baseline (cheat sheet 2025).
const DEFAULT_M_COST: u32 = 19_456; // KiB
/// Two passes — keeps the wall-clock around 50–100 ms on a miniPC.
const DEFAULT_T_COST: u32 = 2;
/// Single lane: we are CPU-bound on Argon2id, parallelism gives us
/// nothing on the homelab class of hardware.
const DEFAULT_P_COST: u32 = 1;

fn argon2() -> Argon2<'static> {
    let params = Params::new(DEFAULT_M_COST, DEFAULT_T_COST, DEFAULT_P_COST, None)
        .expect("OWASP Argon2id parameters are well-formed");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash `plaintext` into a PHC-encoded Argon2id string suitable for
/// storage in `user_credentials.password_hash`.
///
/// The salt is freshly generated from the OS RNG on every call, so two
/// identical passwords produce different PHC strings.
pub fn hash(plaintext: &str) -> Result<String, DashboardError> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = argon2()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| DashboardError::Password(format!("hash failed: {e}")))?;
    Ok(phc.to_string())
}

/// Verify `plaintext` against the previously stored `phc` string.
///
/// Returns `Ok(true)` for a match, `Ok(false)` for a mismatch, and
/// surfaces parser failures (corrupted hash, wrong algorithm tag) as
/// [`DashboardError::Password`].
pub fn verify(plaintext: &str, phc: &str) -> Result<bool, DashboardError> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| DashboardError::Password(format!("malformed stored hash: {e}")))?;
    match argon2().verify_password(plaintext.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(DashboardError::Password(format!("verify failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A password we can hash, verify against its own hash, and reject
    /// when probed with a different plaintext.
    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash("correct horse battery staple").expect("hash");
        // PHC string carries the canonical Argon2id marker.
        assert!(
            phc.starts_with("$argon2id$"),
            "expected Argon2id PHC, got {phc}"
        );

        assert!(verify("correct horse battery staple", &phc).expect("verify good"));
        assert!(!verify("Tr0ub4dor&3", &phc).expect("verify bad"));
    }

    /// Two hashes of the same plaintext must differ (fresh salt every
    /// time) and yet both must verify.
    #[test]
    fn salt_is_random_per_call() {
        let phc_a = hash("same-password-twice").unwrap();
        let phc_b = hash("same-password-twice").unwrap();
        assert_ne!(phc_a, phc_b, "fresh salt must produce a different PHC");
        assert!(verify("same-password-twice", &phc_a).unwrap());
        assert!(verify("same-password-twice", &phc_b).unwrap());
    }

    /// Malformed stored hashes surface as a `Password` error so the
    /// caller can render a sensible 500 instead of crashing.
    #[test]
    fn malformed_hash_yields_password_error() {
        let err = verify("anything", "not-a-phc-string").expect_err("must reject");
        assert!(
            matches!(err, DashboardError::Password(_)),
            "expected Password variant, got {err:?}"
        );
    }
}
