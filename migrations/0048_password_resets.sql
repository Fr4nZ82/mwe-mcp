-- 0048_password_resets — single-use, short-TTL password-recovery links
--
-- Source of truth: [JWT and session model](../wiki/design-notes/jwt-and-session-model.md)
-- + [setup and identity](../wiki/design-notes/setup-and-identity.md). The
-- self-service forgot-password flow (roadmap 28) mints a row here, emails
-- the URL `…/dashboard/reset-password/<reset_id>` to the address on
-- `enrollment_users.email`, and the user picks a new password — the admin
-- is never involved. Distinct from `user_invitations`: a much shorter TTL
-- (~30 min vs 24h) and a different audit meaning (recovery, not onboarding).
--
-- `reset_id` is a random UUIDv7 — it is the only secret in the URL, so it
-- does not reveal the user id. `consumed_at IS NULL` marks a still-usable
-- link; the redeem path flips it inside the same transaction that writes
-- the new hash, so the row's UPDATE is the burn-once serialization point.

CREATE TABLE password_resets (
  reset_id     TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  created_at   TEXT NOT NULL,
  expires_at   TEXT NOT NULL,
  consumed_at  TEXT
);

CREATE INDEX idx_password_resets_expires
  ON password_resets(expires_at)
  WHERE consumed_at IS NULL;
