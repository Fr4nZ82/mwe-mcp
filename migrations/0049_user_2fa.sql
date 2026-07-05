-- 0049_user_2fa — TOTP two-factor auth for the dashboard (roadmap 28)
--
-- Source of truth: [JWT and session model](../wiki/design-notes/jwt-and-session-model.md).
-- 2FA gates only the human `/dashboard/login` surface — the MCP path is
-- bearer-JWT with no interactive login, and system/bot users have no
-- `user_credentials` row, so they are exempt by construction.
--
-- `user_2fa.secret_enc` is the TOTP shared secret encrypted at rest
-- (XChaCha20-Poly1305, key derived from MWE_TOKEN_SECRET) — rotating the
-- token secret therefore invalidates every enrollment, which is the
-- documented trade-off. `enabled = 0` marks an enrollment that has been
-- started but not yet confirmed with a live code; `1` marks it active.

CREATE TABLE user_2fa (
  user_id      TEXT PRIMARY KEY REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  secret_enc   TEXT NOT NULL,
  enabled      INTEGER NOT NULL DEFAULT 0,
  created_at   TEXT NOT NULL,
  confirmed_at TEXT
);

-- Single-use recovery codes (high-entropy → SHA-256-hashed, looked up
-- directly). One row per code; `used_at` marks a spent code.
CREATE TABLE user_2fa_recovery_codes (
  user_id    TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  code_hash  TEXT NOT NULL,
  used_at    TEXT,
  PRIMARY KEY (user_id, code_hash)
);

-- Pending second-factor challenge between a verified password and the
-- session mint. The `challenge_id` is an opaque random id carried in a
-- short-lived cookie — deliberately NOT a JWT, so it can never be
-- mistaken for (or swapped into) a session cookie.
CREATE TABLE pending_2fa (
  challenge_id TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  is_admin     INTEGER NOT NULL DEFAULT 0,
  next         TEXT,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL
);

CREATE INDEX idx_pending_2fa_expires ON pending_2fa(expires_at);

-- Per-user enforcement: an admin can require a specific user to set up
-- 2FA. The deployment-wide "require 2FA for all non-system users" toggle
-- lives in `engine_meta` (key `auth.require_2fa_all`), not here.
ALTER TABLE enrollment_users ADD COLUMN require_2fa INTEGER NOT NULL DEFAULT 0;
