-- 0030_wiki_types_narrative_bool — mark prose-compiled (narrative) wiki types.
--
-- The narrative compiler (../wiki/design-notes/narrative-compiler.md) is reintroduced for
-- the STANDARD path. Three classes of wiki type now matter:
--
--   * companion  (companion = TRUE)            — smart-consumer-owned; excluded
--                                                from the standard ingest /
--                                                compiler path entirely.
--   * narrative  (companion = FALSE,           — prose pages the nightly Cronista
--                 lifecycle empty)               compiles; captures route to the
--                                                captures buffer and the
--                                                published .md is the compiler
--                                                OUTPUT.
--   * structured (companion = FALSE,           — wiki-lists / wiki-cron /
--                 lifecycle non-empty)           wiki-contacts; managed by the
--                                                structured lifecycle interpreter;
--                                                the Cronista SKIPS them and their
--                                                captures keep writing the .md
--                                                directly.
--
-- The split was implicit (prose_tone + lifecycle content) until now. This
-- column makes it a cheap, queryable bit parallel to `companion`, derived at
-- `wiki_type::sync_from_disk` time from `WikiTypeTemplate::is_narrative()`
-- (= !companion && lifecycle.is_empty()). The registry is a rebuildable cache,
-- so the value is recomputed on every startup sync — DEFAULT FALSE is only the
-- pre-sync placeholder.

ALTER TABLE wiki_types_registry
    ADD COLUMN narrative BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_wiki_types_narrative
    ON wiki_types_registry(narrative);
