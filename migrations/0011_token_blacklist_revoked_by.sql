-- 0011_token_blacklist_revoked_by — complete the revocation schema
--
-- The canonical schema is
-- `token_blacklist(jti TEXT PRIMARY KEY, revoked_at INTEGER, reason TEXT, revoked_by TEXT)`.
-- Migration 0010 stood up `(jti, revoked_at, expires_at, reason)` and
-- omitted `revoked_by`. We add it here as a nullable column so existing
-- rows (none in production yet, but the migration is forward-compatible)
-- keep their meaning and new revocations stamp the actor.
--
-- `expires_at` from 0010 is kept — not strictly required, but having
-- the original token `exp` lets a periodic GC job purge entries that
-- could no longer authenticate anyway.

ALTER TABLE token_blacklist ADD COLUMN revoked_by TEXT;
