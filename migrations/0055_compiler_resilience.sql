-- 0055_compiler_resilience — persistent per-page compile-failure surfacing
--
-- Compiler resilience at scale: a page whose Cronista call kept failing used
-- to stay frozen while the dream run still read as plain ok — the per-page
-- error lived only inside the report dump (observed live: a 51-fact page
-- failed "output was not parseable JSON" two nightly cycles in a row,
-- silently). The degraded-mode rewrite itself is engine code
-- (crate::compiler); this migration lands the two additive surfacing shapes:
--
-- 1. `dream_runs` gains structured `pages_failed` / `pages_degraded` counts,
--    so a journal row (and the admin Dream console) can mark a run that
--    completed but left pages failed or degraded; the summary string carries
--    the same counts through the existing rendering.
--
-- 2. `compile_failures` is the per-page failure ledger: one row per page
--    (keyed by the workdir-relative source_path, e.g. `wikis/famiglia/index.md`)
--    currently in a failing streak. The compiler increments `consecutive` on
--    every failed or degraded compile of the page and deletes the row on a
--    clean full rewrite — a degraded guard-append still counts as failing (the
--    Cronista keeps failing there). At consecutive = 2, and again at 5, the
--    compiler emits one `compile_failure_streak` notice on `wiki_events`
--    (observability thresholds on a failure ledger, not semantic gates).

ALTER TABLE dream_runs ADD COLUMN pages_failed   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE dream_runs ADD COLUMN pages_degraded INTEGER NOT NULL DEFAULT 0;

CREATE TABLE compile_failures (
    source_path TEXT    PRIMARY KEY,  -- workdir-relative page path (wikis/<id>/<page>.md)
    consecutive INTEGER NOT NULL,     -- consecutive failed/degraded compiles of this page
    last_error  TEXT    NOT NULL,     -- most recent failure message (notice/operator surface)
    updated_at  TEXT    NOT NULL      -- RFC-3339 UTC of the last increment
);
