-- 0013_user_invitations — single-use 24h invite links
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md). The admin creates
-- a user row in `enrollment_users` plus a row here, hands the URL
-- `…/dashboard/accept-invite/<invitation_id>` to the new user out of
-- band, and the new user picks their own password — the admin never
-- sees it.
--
-- TTL is 24h by default (overridable in `mwe-mcp.config.yaml`).
-- `consumed_at IS NULL` marks an invite that is still usable; the
-- partial index keeps the lookup of "live invites about to expire"
-- cheap without dragging in already-consumed rows.

CREATE TABLE user_invitations (
  invitation_id  TEXT PRIMARY KEY,
  user_id        TEXT NOT NULL REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  created_at     TEXT NOT NULL,
  expires_at     TEXT NOT NULL,
  consumed_at    TEXT,
  invited_by     TEXT NOT NULL
);

CREATE INDEX idx_invitations_expires
  ON user_invitations(expires_at)
  WHERE consumed_at IS NULL;
