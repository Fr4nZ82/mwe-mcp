-- 0054_dream_runs — persisted history of dream runs for the admin Dream console
--
-- A "dream" is one of the three memory-consolidation compositions
-- (crate::dream / wiki/design-notes/rem-cycle.md): the cheap `light` promotion,
-- the `compile` narrative pass, and the `full` nightly REM. Until now the only
-- trace of a run was an in-memory one-liner on the dashboard state (lost on
-- restart) plus stderr tracing — so an operator had no record of the nightly /
-- scheduled runs at all. This table is the durable journal both the dashboard
-- (manual triggers) and the server scheduler (nightly / interval triggers) write
-- to, so the admin Dream page can show a history and open each run's log.
--
-- `kind` is the closed set 'light' | 'compile' | 'full'; `trigger_source` is
-- 'manual' (operator clicked the console button) | 'scheduled' (the interval /
-- nightly loop). `ok` is the run outcome; `summary` is the one-line shown in the
-- table row (or the error message when ok=0); `log_text` is the full report dump
-- shown in the per-row modal (or the error detail when ok=0). Both timestamps
-- are RFC-3339 UTC.
--
-- The journal is intentionally bounded: crate::dream_journal prunes to the
-- newest 100 rows after each insert (a resource cap, not a semantic gate). A
-- scheduled light tick that scanned nothing is not recorded, so the history
-- stays meaningful rather than flooding with no-op promotions.

CREATE TABLE dream_runs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    kind           TEXT    NOT NULL,  -- 'light' | 'compile' | 'full'
    trigger_source TEXT    NOT NULL,  -- 'manual' | 'scheduled'
    ok             INTEGER NOT NULL,  -- 0 | 1
    summary        TEXT    NOT NULL,  -- one-line outcome (table row), or error message when ok=0
    log_text       TEXT    NOT NULL,  -- full report dump (modal), or error detail when ok=0
    started_at     TEXT    NOT NULL,  -- RFC-3339 UTC
    finished_at    TEXT    NOT NULL   -- RFC-3339 UTC
);

-- The console lists newest-first and the prune keeps the newest 100; both order
-- by completion time, which `id` (monotonic autoincrement) already tracks — so
-- the primary key index suffices and no secondary index is needed.
