-- 0018_profile_initialized — first-login profile wizard flag
--
-- Source of truth: the engineering wiki
-- [setup-and-identity.md](../wiki/design-notes/setup-and-identity.md).
--
-- After the email/password split and the auto-creation of the
-- identity wiki, there is a one-shot wizard at first login that
-- collects optional profile data — birthday, city, language, etc. —
-- and captures it as the first fact inside the user's personal wiki.
-- This flag tells the login handler whether the wizard has already
-- run for this user, so it only fires once.
--
-- Default 0 (not initialized) — every existing row is treated as
-- "wizard pending" after the migration. The admin who deployed before
-- this migration lands on `/dashboard/welcome` on next login, fills it
-- in (or skips), and the flag flips to 1. Subsequent logins go
-- straight to `/dashboard/home` as before.

ALTER TABLE user_credentials ADD COLUMN profile_initialized INTEGER NOT NULL DEFAULT 0;
