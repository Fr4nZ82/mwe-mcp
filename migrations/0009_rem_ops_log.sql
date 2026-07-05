-- 0009_rem_ops_log — applicative WAL for nightly REM (R.1.5)
--
-- Mirrors `proposal_ops_log` but for REM operations (promotion,
-- revalidate, dedup_merge proposal, archive proposal generation).
-- Each op snapshots its inputs under `_snapshots/rem/<cycle_id>/<op_id>/`
-- so a crash mid-night can roll back in-flight ops; the cycle is then
-- marked `interrupted` and the next cron run restarts from scratch
-- (REM is idempotent, candidates are recomputed).

CREATE TABLE rem_ops_log (
    op_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    cycle_id        TEXT NOT NULL,                                 -- UUIDv7 of the REM cycle
    operation_kind  TEXT NOT NULL,                                 -- "promotion" | "revalidate" | "dedup_merge" | "archive_proposal" | …
    target_wiki_id  TEXT,                                          -- NULL for global ops
    snapshot_path   TEXT,                                          -- relative to workdir
    status          TEXT NOT NULL,                                 -- "pending" | "in_progress" | "done" | "failed"
    started_at      TEXT NOT NULL,
    completed_at    TEXT,
    error_msg       TEXT
);

CREATE INDEX idx_remops_status ON rem_ops_log(status, started_at);
CREATE INDEX idx_remops_cycle  ON rem_ops_log(cycle_id, op_id);
