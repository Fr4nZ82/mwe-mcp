-- 0012_user_credentials — password hashes for dashboard login
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md). Only users that need
-- to sign in through the browser have a row here; "system" identities
-- whose only purpose is to anchor a consumer token (e.g. `samvise-bot`)
-- can be created in `enrollment_users` without ever appearing here.
--
-- `password_hash` is a PHC string produced by Argon2id (the hash
-- carries its own salt + tuning parameters). Default cost parameters
-- live in `mwe-mcp.config.yaml`.
--
-- ON DELETE CASCADE means deleting a user from `enrollment_users` also
-- drops their credential row — the identity vanishes everywhere in
-- one shot.

CREATE TABLE user_credentials (
  user_id        TEXT PRIMARY KEY REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
  password_hash  TEXT NOT NULL,
  hashed_at      TEXT NOT NULL,
  must_change    INTEGER NOT NULL DEFAULT 0
);
