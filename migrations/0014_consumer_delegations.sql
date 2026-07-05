-- 0014_consumer_delegations — act-as authorization for multi-user bots
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md). A row here lets the
-- consumer named by `consumer_id` (e.g. `samvise-prod`) impersonate any
-- of the user ids listed in `allowed_sender_ids` (JSON array) when it
-- presents the HTTP header `X-MWE-Act-As: <user_id>` on a tool call.
--
-- No foreign key into `enrollment_users(user_id)`: the delegation is a
-- JSON array, so referential integrity is enforced applicatively at
-- write time (the dashboard form) and at the per-call lookup the auth
-- middleware does on every call.
--
-- The delegation deliberately does **not** ride inside the bot's JWT.
-- The token carries only `consumer_id`; this table is re-read on every
-- tool call (with a short TTL cache, mirroring `BlacklistCache`), so
-- edits propagate without forcing token re-issuance.

CREATE TABLE consumer_delegations (
  consumer_id          TEXT PRIMARY KEY,
  allowed_sender_ids   TEXT NOT NULL,
  granted_at           TEXT NOT NULL,
  granted_by           TEXT NOT NULL
);
