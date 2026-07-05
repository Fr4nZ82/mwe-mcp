-- 0008_proposal_ops_log — applicative WAL for structure_proposal_apply (R.1.4)
--
-- Apply runs step-by-step: each step writes a `pending` row, performs
-- the file write + DB update, then flips to `done`. On startup, any row
-- left in `pending`/`in_progress` for more than 5 minutes triggers an
-- inverse-order rollback using the snapshot under
-- `_snapshots/proposal/<proposal_id>/`, then the proposal is marked
-- `failed reason='crash_during_apply'`.

CREATE TABLE proposal_ops_log (
    op_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_id  TEXT NOT NULL,
    step_idx     INTEGER NOT NULL,                                 -- ordered within a proposal
    kind         TEXT NOT NULL,                                    -- "file_write" | "db_update" | "marker_propagation" | "cross_link_rewrite" | …
    payload_json TEXT,                                             -- JSON args of the step (for replay/debug)
    status       TEXT NOT NULL,                                    -- "pending" | "in_progress" | "done" | "failed"
    started_at   TEXT NOT NULL,
    completed_at TEXT,
    error_msg    TEXT
);

CREATE INDEX idx_propops_status   ON proposal_ops_log(status, started_at);
CREATE INDEX idx_propops_proposal ON proposal_ops_log(proposal_id, step_idx);
