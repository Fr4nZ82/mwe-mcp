-- `_briefing.md` items DB mirror.
--
-- One row per `wiki_admin_notify` call. The `_briefing.md` file at
-- the wiki root remains the SSOT (Obsidian-readable, smart-consumer
-- editable); this table is a derivable cache that powers:
--
-- 1. **Rate limit** (`50 notify/wiki/h`): a single `SELECT COUNT(*)`
--    over a time window beats parsing the file.
-- 2. **Dashboard `/wikis/<id>/briefing`**: list / group-by-kind
--    / "mark as read" actions hit this table, not the markdown.
-- 3. **REM dispatcher** (`Briefing dispatcher` sub-job): finds
--    stale-drafts / backlink-reciprocity gaps and writes its own
--    notify rows; the table is the natural queue.
--
-- `processed_at` is set when the smart consumer "drains" the item
-- during `smart_bootstrap` (rotates the markdown to
-- `_briefing.archive.md` + flips the row). NULL = pending. The
-- table is append-only otherwise — no UPDATE except `processed_at`.

CREATE TABLE wiki_briefing_items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    wiki_id       TEXT NOT NULL,
    source_kind   TEXT NOT NULL,    -- 'user' | 'rem' | 'consumer' | 'dashboard'
    source_ref    TEXT NOT NULL,    -- e.g. 'user:fr4nz', 'rem:briefing_dispatcher', 'cc-laptop'
    topic         TEXT NOT NULL,    -- short topic line (≤ 200 chars)
    body          TEXT NOT NULL,    -- markdown body (≤ 4 KB)
    kind          TEXT,             -- observation|reasoning|external (NULL = default observation)
    ts            TEXT NOT NULL,    -- ISO 8601 UTC, stamped by the handler or caller
    processed_at  TEXT              -- set by smart_bootstrap after drain
);

CREATE INDEX idx_wiki_briefing_items_wiki_ts
    ON wiki_briefing_items(wiki_id, ts DESC);

CREATE INDEX idx_wiki_briefing_items_pending
    ON wiki_briefing_items(wiki_id, processed_at)
    WHERE processed_at IS NULL;
