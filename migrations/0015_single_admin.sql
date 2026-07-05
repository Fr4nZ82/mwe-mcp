-- 0015_single_admin — enforce "exactly one admin per deployment"
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md).
-- mwe-mcp targets the single-operator homelab model: one operator, one admin.
-- The first-run dashboard wizard at /dashboard/setup is the *only* producer of
-- a row with `is_admin = 1`; every other identity flow (invitation links,
-- user edit) creates regular users.
--
-- The partial unique index makes the invariant a hard DB constraint, not just
-- an application-layer convention. Any INSERT or UPDATE that would land a
-- second row with `is_admin = 1` fails at sqlx level — including manual
-- tampering with the DB outside the dashboard.
--
-- Rows with `is_admin = 0` are not constrained: the index only covers the
-- filtered partition `WHERE is_admin = 1`, so the table can hold any number
-- of regular users.

CREATE UNIQUE INDEX idx_single_admin
    ON enrollment_users(is_admin) WHERE is_admin = 1;
