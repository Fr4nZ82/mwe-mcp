-- 0001_fact_index — region-level index (schemi.md §1.1, R.1.3 UUIDv7 fact_id)
--
-- One row per region delimited by `{{f=<UUIDv7>}}…{{/}}` markers. The body
-- is stored verbatim for re-embed + audit; the embedding column holds the
-- vector for similarity search.
--
-- Rebuildable from the markdown SSOT under `<workdir>/wikis/` via
-- `dream_trigger(mode=reindex)` or a full-restart re-index.

CREATE TABLE fact_index (
    fact_id          TEXT PRIMARY KEY,                            -- UUIDv7 lowercase with dashes (R.1.3)
    wiki_id          TEXT NOT NULL,
    source_path      TEXT NOT NULL,                               -- relative to workdir
    region_start     INTEGER,                                     -- byte offset of {{f=...}}
    region_end       INTEGER,                                     -- byte offset of {{/}}
    "text"           TEXT NOT NULL,                               -- region body (no markers); quoted because TEXT is also the column type

    embedding        BLOB NOT NULL,                               -- vector bytes
    embedding_dim    INTEGER NOT NULL,                            -- explicit, for embedding-model migrations
    owner_id         TEXT NOT NULL,                               -- "global" | "user:X" | "group:X"
    allow_ids        TEXT,                                        -- JSON array of principals
    sender_id        TEXT,                                        -- cross-user attribution (NULL when == owner)
    fact_type        TEXT,                                        -- "bio" | "preference" | ... | custom
    topics           TEXT,                                        -- JSON array of strings
    created_at       TEXT NOT NULL,                               -- ISO 8601
    updated_at       TEXT NOT NULL,
    superseded_at    TEXT,                                        -- NULL if active
    superseded_by    TEXT,                                        -- fact_id of replacement
    deleted_at       TEXT,                                        -- NULL if active
    deleted_reason   TEXT,                                        -- "filesystem_removed" | "user_request" | "gdpr_erasure" | ...
    last_recall_at   TEXT,
    recall_count_30d INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_fact_wiki_id ON fact_index(wiki_id);
CREATE INDEX idx_fact_owner   ON fact_index(owner_id);
CREATE INDEX idx_fact_created ON fact_index(created_at);
CREATE INDEX idx_fact_type    ON fact_index(fact_type) WHERE fact_type IS NOT NULL;
CREATE INDEX idx_fact_active  ON fact_index(deleted_at, superseded_at);
CREATE INDEX idx_fact_recall  ON fact_index(last_recall_at) WHERE last_recall_at IS NOT NULL;
CREATE INDEX idx_fact_path    ON fact_index(source_path, region_start);
