-- 0029_consumers_system_user — materialise the consumer ↔ system-user binding
--
-- Part of the "diagonal identity model" (../wiki/concepts/identity-and-acl.md):
-- a *standard* (conversational) consumer is itself a user — a credential-less
-- **system user** in `enrollment_users` with its own memory wiki — and writes
-- other people's memories by acting on their behalf (`X-MWE-Act-As`). A *smart*
-- consumer, by contrast, authenticates directly as its human owner (Pattern A).
--
-- Until now the bot's identity (`sender_id`, a system user) and its deployment
-- registration (`consumer_id`, this table) were unlinked: `consumer_id` was a
-- free JWT claim, `sender_id` a separate enrollment row, with nothing tying the
-- two. This column records that link explicitly so the binding
-- consumer ↔ system-user ↔ wiki is first-class and queryable (dashboard, audit)
-- instead of conventional.
--
-- Populated by `consumers::register` (the `consumer_register` MCP tool) from the
-- caller's own `sender_id` when its token is `consumer_class = standard`. NULL
-- for pre-existing rows and for any consumer that registered before the binding
-- existed; a re-registration backfills it.

ALTER TABLE consumers
    ADD COLUMN system_user_id TEXT REFERENCES enrollment_users(user_id);
