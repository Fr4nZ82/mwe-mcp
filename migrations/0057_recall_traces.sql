-- 0057_recall_traces — bounded journal of recall runs for the admin Traces page
--
-- A "recall trace" is the full story of one recall run: the flat/fresh/due-soon
-- hits, the entry-point fan, every navigator hop (candidates offered, the
-- decision with its one-line note, pages opened) and the block that was
-- actually injected into the consumer. Two producers write here
-- (wiki/design-notes/recall-pipeline.md §Recall traces): the ingest per-turn
-- injection (`mwe_core::ingest::wiki_ingest_message`) and the `wiki_navigate`
-- deep-recall tool. Until now the path a recall took was ephemeral — stderr
-- tracing carried counts, never the route — so the operator had no way to see
-- *why* a fact did or did not surface.
--
-- `source` is the closed set 'ingest' | 'navigate'. `payload` is versioned
-- JSON (`mwe_core::recall_trace::RecallTrace`); readers tolerate missing
-- fields so the shape can grow. Timestamps are RFC-3339 UTC.
--
-- The journal is intentionally tiny: crate::recall_trace prunes to the newest
-- 10 rows after each insert (a resource cap, not a semantic gate — the surface
-- is "the last few recalls", not an audit trail; `tool_executions` remains the
-- audit surface).

CREATE TABLE recall_traces (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,  -- RFC-3339 UTC
    source     TEXT NOT NULL,  -- 'ingest' | 'navigate'
    sender_id  TEXT NOT NULL,  -- bare user id the recall ran as
    payload    TEXT NOT NULL   -- versioned JSON: mwe_core::recall_trace::RecallTrace
);

-- Newest-first listing and the prune both order by insertion, which `id`
-- (monotonic autoincrement) already tracks — the primary key suffices.
