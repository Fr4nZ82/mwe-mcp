-- Self-correcting REM, the detection floor: the per-turn hindsight recall
-- log and the judge-free restated-known-fact miss records.

-- One lean row per LLM-routed ingest turn: the fact ids the recall block
-- surfaced (flat + fresh + due-soon) and the navigated pages' source
-- paths. The offline half of the miss signal — a dedup hit at
-- buffered-capture promotion looks back at what the original turn
-- surfaced. Age-pruned on write (see mwe_core::recall_log).
CREATE TABLE recall_log (
    log_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,                 -- ISO-8601, the turn's clock
    sender_id  TEXT NOT NULL,
    fact_ids   TEXT NOT NULL DEFAULT '[]',    -- JSON array of surfaced ids
    page_paths TEXT NOT NULL DEFAULT '[]'     -- JSON array of navigated pages (workdir-relative)
);
CREATE INDEX idx_recall_log_created ON recall_log(created_at);

-- One row per detected miss: memory held the fact, that turn's recall did
-- not surface it, and the user restated it — the dedup hit is the proof
-- (no LLM verdict anywhere in the signal).
CREATE TABLE recall_misses (
    miss_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    TEXT NOT NULL,
    sender_id     TEXT NOT NULL,
    fact_id       TEXT NOT NULL,              -- the fact recall failed to surface
    wiki_id       TEXT NOT NULL,              -- its home at detection time
    source_path   TEXT NOT NULL,
    surface       TEXT NOT NULL,              -- 'direct' | 'promotion'
    similarity    REAL,                       -- the dedup score that proved the restatement
    restated_text TEXT NOT NULL,              -- what the user re-said
    log_id        INTEGER                     -- the turn's recall_log row (NULL: legacy/untracked)
);
CREATE INDEX idx_recall_misses_fact ON recall_misses(fact_id);
CREATE INDEX idx_recall_misses_created ON recall_misses(created_at);

-- Turn linkage for promotion-time detection: which recall_log row was the
-- buffered capture's turn. NULL on pre-feature rows — detection skips them.
ALTER TABLE capture_buffer ADD COLUMN recall_log_id INTEGER;
