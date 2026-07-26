-- Smart-wiki content indexing moves out of `fact_index` into its own
-- pair of tables.
--
-- WHY. `fact_index` is the authoritative store for **standard**-wiki
-- facts: per-fragment ACL, supersedence chains, tombstones, validity
-- windows, cross-user attribution. Smart wikis carry none of that — a
-- smart page is chunked into heading-delimited sections whose only job
-- is RAG, whose ACL is the **wiki's** (not the section's), and which are
-- regenerable from disk at any time. Mixing the two in one table left 19
-- of 31 columns permanently NULL on section rows, forced every
-- "exclude the project docs" filter to be a post-filter (the smart flag
-- lives in `_meta.md`, so SQL could not express it), and made a sharing
-- edit rewrite one row per section instead of one row per wiki.
--
-- This migration is **DDL only**. It deliberately does not move the
-- existing rows: SQL cannot tell which `wiki_id`s are smart, because
-- that flag is on disk in each wiki's `_meta.md`. The data move is a
-- tree-aware, idempotent Rust boot pass (same precedent as
-- `reindex::reconcile_wiki_ids`), which copies embeddings verbatim — no
-- re-embedding.

-- One row per heading-delimited section of a smart-wiki page.
--
-- Identity is **positional and deterministic** — `(source_path,
-- section_ord)` — not a freshly minted UUID per reindex. The old
-- behaviour re-keyed every section on every reindex, which stranded the
-- recall history: ids logged in `recall_log` stopped resolving as soon
-- as their page was touched.
--
-- No `owner_id` / `allow_ids` / `sender_id`: read access is the wiki's,
-- resolved once per wiki from `smart_wikis` below. No `superseded_at` /
-- `deleted_at` / `valid_from` / `valid_to`: a section is not superseded
-- or forgotten, it is re-derived from the page or it disappears with it.
CREATE TABLE wiki_sections (
    source_path      TEXT    NOT NULL,          -- workdir-relative page path
    section_ord      INTEGER NOT NULL,          -- 0-based position on the page
    wiki_id          TEXT    NOT NULL,
    heading_path     TEXT,                      -- "A > B > C" chain; NULL for a page preamble
    "text"           TEXT    NOT NULL,          -- heading chain + body: exactly what was embedded
    embedding        BLOB    NOT NULL,
    embedding_dim    INTEGER NOT NULL,          -- explicit, for embedder migrations
    created_at       TEXT    NOT NULL,          -- first time this position was indexed
    updated_at       TEXT    NOT NULL,
    last_recall_at   TEXT,
    recall_count_30d INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_path, section_ord)
);

-- Page-scoped drop/reinsert is the hot write path; wiki-scoped read is
-- the hot read path.
CREATE INDEX idx_sections_wiki ON wiki_sections(wiki_id);

-- The registry the engine has never had: a **queryable projection** of
-- each smart wiki's `_meta.md`.
--
-- `_meta.md` on disk stays the single source of truth — this table is a
-- cache, rebuilt by the same tree-walking sweep that rebuilds the
-- sections, so a hand-edited `_meta.md` still wins. What it buys is that
-- "which wikis are smart?" and "who may read this wiki?" become part of
-- a SQL query instead of a post-hoc tree walk per hit.
CREATE TABLE smart_wikis (
    wiki_id      TEXT PRIMARY KEY,
    owner_id     TEXT NOT NULL,                 -- "global" | "user:X" | "group:X"
    shared_with  TEXT NOT NULL DEFAULT '[]',    -- JSON array of principals
    project_id   TEXT,
    wiki_type    TEXT NOT NULL,                 -- free-form tone/label, does not decide smart-ness
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE INDEX idx_smart_wikis_owner ON smart_wikis(owner_id);
