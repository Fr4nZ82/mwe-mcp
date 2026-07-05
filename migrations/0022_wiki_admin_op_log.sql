-- Audit/op log for `wiki_admin_push` / `wiki_admin_pull` /
-- (future) `wiki_admin_notify`.
--
-- Schema modeled after `proposal_ops_log` (0008) and
-- `tool_executions` (0003): one row per top-level operation the smart
-- consumer performs on a companion-wiki. Persists who pushed/pulled
-- what + when, for:
--
-- 1. **Audit + UI**: `/wikis/<id>/op-log` dashboard page lists
--    the rows in reverse chronological order. The most recent push is
--    revertable from there.
-- 2. **Optimistic concurrency**: `expected_op_log_head` on
--    `wiki_admin_push` compares against the latest `op_id`; mismatch
--    → `409 conflicting_op_log_head`. (Wired in a follow-up — the first
--    cut writes the rows but doesn't gate on them yet, since
--    the only smart consumer at MVP-time is single-device.)
-- 3. **Replay / delta pull**: `wiki_admin_pull(since_op_log_id)` walks
--    the log to derive the pages touched after that point. Same
--    deferral as point 2 — MVP does full pull only.
--
-- Privacy: `payload_hash` is sha256 of the canonical serialised
-- input (paths + sorted page-checksum manifest), never raw page
-- content. `pages_affected` is the count, not the list, so the row
-- size stays bounded.

CREATE TABLE wiki_admin_op_log (
    op_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    wiki_id         TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    consumer_id     TEXT,                 -- nullable: older consumer tokens may omit
    op_kind         TEXT NOT NULL,        -- 'push_create' | 'push_upsert' | 'push_snapshot_replace' | 'pull' | 'notify'
    op_mode         TEXT,                 -- mirror of `mode` for pushes, NULL otherwise
    payload_hash    TEXT NOT NULL,        -- sha256 of canonical input
    pages_affected  INTEGER NOT NULL DEFAULT 0,
    ts              TEXT NOT NULL         -- ISO 8601 UTC, stamped by the handler
);

CREATE INDEX idx_wiki_admin_op_log_wiki_ts
    ON wiki_admin_op_log(wiki_id, ts DESC);

CREATE INDEX idx_wiki_admin_op_log_sender_ts
    ON wiki_admin_op_log(sender_id, ts DESC);
