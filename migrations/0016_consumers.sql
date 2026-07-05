-- 0016_consumers — registry of consumer agents (orchestrators, bots, plugins)
--
-- Source of truth: [tool reference](../wiki/protocol/tool-reference.md).
-- A row here marks a consumer (`samvise-orchestrator`, a discord bot, a vscode
-- plugin) as known to this deployment. Every `events_poll` / `events_ack` call
-- checks the row exists before serving events; missing row ⇒ `consumer_not_registered`.
--
-- Identity model:
-- - `consumer_id` is chosen by the consumer at registration time (`consumer_register`
--   tool input), stable across restarts. It is also the key the JWT carries in the
--   optional `consumer_id` claim — kept in sync so an agent can prove who it is.
-- - `display_name` / `metadata` are informational, stamped at registration and
--   refreshed on re-registration (idempotent by `consumer_id`).
-- - `kinds_subscribed` is a JSON array of event kinds the consumer cares about.
--   Empty / NULL ⇒ subscribed to all kinds (the default).
-- - `consumer_secret` is a random hex string returned to the consumer once; it is
--   the verification key for webhook delivery (`callback_url`) in a future phase.
--   Stored as plain hex because there is no offline verifier — the secret is used
--   by the consumer to sign its own webhook receipts.
-- - `callback_url` is optional; absent ⇒ pull-only via `events_poll`.

CREATE TABLE consumers (
    consumer_id      TEXT PRIMARY KEY,
    display_name     TEXT,
    callback_url     TEXT,
    kinds_subscribed TEXT,                                          -- JSON array of strings
    metadata         TEXT,                                          -- JSON object, free shape
    consumer_secret  TEXT NOT NULL,                                 -- hex-encoded random bytes
    registered_at    TEXT NOT NULL,
    last_seen_at     TEXT                                           -- last events_poll/ack timestamp
);

CREATE INDEX idx_consumers_last_seen ON consumers(last_seen_at)
    WHERE last_seen_at IS NOT NULL;
