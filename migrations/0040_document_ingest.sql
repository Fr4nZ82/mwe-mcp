-- Document / long-form ingest: the async document-job machinery behind
-- `wiki_ingest_external` plus per-fact source provenance.
-- Engineering-wiki SSOT: wiki/design-notes/document-ingest.md
--
-- A document is a unit by default (disposition dial: consult / dossier /
-- dissolve). The job table is the checkpointed lifecycle of one ingested
-- document; the segments table is the per-segment extraction checkpoint
-- (a crashed worker resumes from the last done segment, never re-runs the
-- whole document).

-- Per-fact provenance: the catalog id / url the fact was extracted from.
-- NULL for ordinary conversational captures. DB-authoritative metadata,
-- like the rest of the per-fact columns — not rebuildable from markdown.
ALTER TABLE fact_index ADD COLUMN source_ref TEXT;

CREATE TABLE document_jobs (
    job_id                TEXT PRIMARY KEY,                -- UUIDv7
    source_kind           TEXT NOT NULL,                   -- media | inline | url
    source_ref            TEXT,                            -- catalog_id / url; NULL for inline
    text_sha256           TEXT NOT NULL,                   -- idempotency key over the resolved text
    "text"                TEXT NOT NULL,                   -- resolved document text (the worker's input)
    title_hint            TEXT,
    disposition_requested TEXT,                            -- consult | dossier | dissolve; NULL = the LLM decides
    format_requested      TEXT,                            -- prose | dialogue; NULL = the LLM decides
    occurred_at           TEXT,                            -- the document's semantic clock (ISO-8601)
    owner_id              TEXT NOT NULL,
    allow_ids             TEXT,                            -- JSON array of principals
    sender_id             TEXT,                            -- NULL when == owner
    status                TEXT NOT NULL DEFAULT 'queued',  -- queued | running | done | failed
    resolved_disposition  TEXT,                            -- classify-phase checkpoint
    resolved_format       TEXT,
    resolved_title        TEXT,
    target_wiki_id        TEXT,
    document_page         TEXT,                            -- anchor page slug (consult / dossier)
    anchor_fact_id        TEXT,                            -- anchor-phase checkpoint
    summary               TEXT,
    reduced_json          TEXT,                            -- reduce-phase checkpoint (post-merge facts)
    total_segments        INTEGER,                         -- segment-phase checkpoint
    done_segments         INTEGER NOT NULL DEFAULT 0,
    facts_extracted       INTEGER NOT NULL DEFAULT 0,
    facts_buffered        INTEGER NOT NULL DEFAULT 0,      -- file-phase progress cursor
    error                 TEXT,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    finished_at           TEXT
);

CREATE INDEX idx_document_jobs_status ON document_jobs (status, created_at);
CREATE INDEX idx_document_jobs_idem   ON document_jobs (text_sha256, owner_id);

CREATE TABLE document_job_segments (
    job_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    heading     TEXT,                                      -- heading chain (prose) / speaker block hint (dialogue)
    content     TEXT NOT NULL,
    occurred_at TEXT,                                      -- per-utterance clock (dialogue), when the transcript carries it
    status      TEXT NOT NULL DEFAULT 'pending',           -- pending | done | failed
    facts_json  TEXT,                                      -- extraction output (candidate facts)
    error       TEXT,
    PRIMARY KEY (job_id, seq)
);
