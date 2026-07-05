-- 0031_capture_buffer — the narrative captures buffer
-- (see [narrative buffer](../wiki/design-notes/narrative-buffer.md)).
--
-- The narrative compiler (../wiki/design-notes/narrative-compiler.md) restores the buffer + nightly-compiler pipeline
-- for the standard/narrative path: a message is classified and written to a
-- per-wiki captures BUFFER, never directly into the published .md. The .md is
-- the OUTPUT of the nightly Cronista; the buffer is the pre-compilation source.
--
-- Filesystem-SSOT invariant: the durable SSOT of a buffered capture is the
-- per-wiki on-disk journal `<wiki_dir>/_captures.md` (marker-style entries). THIS
-- table is a rebuildable cache/index over that journal — `rm engine.db` followed
-- by a reindex of the journal regenerates every row (see crate::capture_buffer,
-- wired into reindex::reindex_full). No `journal_path` column is stored: a
-- capture for wiki W always lives in W's `_captures.md`, derived from the tree.
--
-- Lifecycle (status):
--   buffered     — written by wiki_ingest_message, awaiting the light dream.
--   promoted     — the light dream embedded + inserted a fact_index row
--                  with fact_id == capture_id; resolved_fact_id == capture_id.
--   skipped_dup  — the light dream found an exact/near duplicate; resolved_fact_id
--                  points at the surviving fact.
--
-- capture_id is a UUIDv7 minted at buffer time and REUSED verbatim as the
-- fact_id on promotion, so a claim keeps one stable id across
-- buffer -> fact -> compiled-page (the correctness hinge for incremental
-- compilation). Columns mirror fact_index's classifier/ACL columns so the
-- promotion is a straight copy.

CREATE TABLE capture_buffer (
    capture_id        TEXT PRIMARY KEY,                 -- UUIDv7; becomes fact_id on promotion
    wiki_id           TEXT NOT NULL,
    target_page       TEXT NOT NULL,                    -- page the classifier proposed (compiler hint)
    body              TEXT NOT NULL,                    -- captured claim prose, verbatim, no markers
    owner_id          TEXT NOT NULL,                    -- 'global' | 'user:<id>' | 'group:<id>'
    allow_ids         TEXT NOT NULL DEFAULT '[]',       -- JSON array of principals
    sender_id         TEXT,                             -- cross-user attribution; NULL when == owner
    fact_type         TEXT,                             -- bio | state | preference | rule | plan | episode | other
    topics            TEXT NOT NULL DEFAULT '[]',       -- JSON array
    supersede_hint    TEXT,                             -- fact_id the classifier flagged as superseded (optional)
    status            TEXT NOT NULL DEFAULT 'buffered', -- buffered | promoted | skipped_dup
    captured_at       TEXT NOT NULL,                    -- ISO-8601
    processed_at      TEXT,                             -- ISO-8601, set when the light dream resolves the row
    resolved_fact_id  TEXT,                             -- fact this capture became (==capture_id) or deduped into
    source_kind       TEXT NOT NULL DEFAULT 'ingest',   -- ingest | shadow_diff | dashboard
    source_ref        TEXT
);

CREATE INDEX idx_capture_buffer_wiki ON capture_buffer(wiki_id);
CREATE INDEX idx_capture_buffer_pending ON capture_buffer(status) WHERE status = 'buffered';
