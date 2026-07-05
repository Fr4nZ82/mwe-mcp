-- 0017_user_credentials_email — split email-account from user_id slug
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md).
--
-- Before this migration: login took the `user_id` slug + password directly.
-- The slug doubled as the principal in markers (`{{owner=user:franz}}`), the
-- wiki id on disk (`<workdir>/wikis/franz/`), and the human-typed username at
-- the dashboard. Conflating those three roles meant operators had to pick a
-- slug they'd type every time they logged in — and changing it later would
-- ripple through every marker on disk.
--
-- After this migration: login is by email. The slug stays as the canonical
-- principal in markers + wiki id (short, kebab-case, filesystem-safe). The
-- email/password pair is the authentication surface; the slug is the
-- domain identity.
--
-- We add `email` as a nullable UNIQUE column so existing rows (the bootstrap
-- admin created before this migration) keep working — the dashboard back-fills
-- it via a one-shot prompt on next login, see `dashboard/routes/setup.rs`.

ALTER TABLE user_credentials ADD COLUMN email TEXT;

-- Partial unique index: only enforce uniqueness on rows that already have
-- the email set. Rows with `email IS NULL` are the pre-split stragglers; the
-- dashboard fills them in on the next login and then this index starts
-- biting normally.
CREATE UNIQUE INDEX idx_user_credentials_email
  ON user_credentials(email) WHERE email IS NOT NULL;
