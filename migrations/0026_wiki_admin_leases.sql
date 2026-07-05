-- Optional cooperative lease for `wiki_admin_push`
-- coordination across multiple smart consumers of the same owner
-- (e.g. the user opens the same project from both laptop and desktop
-- with two Claude Code sessions).
--
-- The lease is **opt-in**: without one, `wiki_admin_push` is still
-- subject to the existing optimistic-concurrency story
-- (`expected_op_log_head`, deferred). With one, push from
-- a different consumer fails with `423 wiki_locked_by_lease` until
-- the lease is released or expires.
--
-- Design ([tool reference](../wiki/protocol/tool-reference.md)):
--
-- - `released_at IS NULL` means the lease is currently active (subject
--   to `expires_at > now` for time-bound validity). The lease module
--   enforces "at most one active row per `wiki_id`" via a check inside
--   the transaction — SQLite cannot express the constraint with a
--   plain `UNIQUE (wiki_id) WHERE released_at IS NULL` partial index
--   here because lease re-acquisition by the same consumer wants to
--   touch the row instead of inserting a sibling.
-- - Lease re-acquire from the same `(sender_id, consumer_id)` extends
--   the existing row (`expires_at` bumped, `released_at` stays NULL).
--   The lease is "I am authoritative on this wiki right now", not a
--   syntactic mutex.
-- - REM's nightly `lease_expirer` sub-job (added in this milestone)
--   prunes rows where `released_at IS NULL AND expires_at < now - 1h`
--   (treated as crashed-without-release) and rows that have been
--   `released_at IS NOT NULL` for more than 7 days (audit retention).

CREATE TABLE wiki_admin_leases (
    lease_id       TEXT PRIMARY KEY,
    wiki_id        TEXT NOT NULL,
    sender_id      TEXT NOT NULL,
    consumer_id    TEXT,                  -- nullable: smart-tokens may omit
    acquired_at    TEXT NOT NULL,         -- ISO 8601 UTC
    expires_at     TEXT NOT NULL,         -- ISO 8601 UTC; acquired_at + ttl_sec
    released_at    TEXT                   -- ISO 8601 UTC when released; NULL while active
);

-- Active-lease lookup: "is there a not-released, not-expired lease
-- on this wiki?". The lease module filters `released_at IS NULL`
-- and compares `expires_at` to `now` at query time.
CREATE INDEX idx_wiki_admin_leases_wiki_active
    ON wiki_admin_leases(wiki_id, expires_at)
    WHERE released_at IS NULL;

-- Expirer sweep: order by acquired_at so the REM sub-job can run a
-- bounded batch in time order without a full table scan.
CREATE INDEX idx_wiki_admin_leases_expires_at
    ON wiki_admin_leases(expires_at);
