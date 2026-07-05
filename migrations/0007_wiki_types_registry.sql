-- 0007_wiki_types_registry — wiki_type cache (schemi.md §1.7)
--
-- Cache of every registered `wiki_type` (bundled in `wikis/_styles/*.md`
-- plus custom in `wikis/_styles/<owner>/*.md`). Updated by the
-- filesystem watcher on `_styles/` changes; enables fast lookups
-- without re-parsing the template files.

CREATE TABLE wiki_types_registry (
    type_id     TEXT PRIMARY KEY,                                  -- "wiki-cron" | "galadriel/wiki-ricetta"
    namespace   TEXT NOT NULL,                                     -- "bundled" | <user_id>
    source_path TEXT NOT NULL,
    forked_from TEXT,                                              -- type_id of source (NULL if not forked)
    version     TEXT,                                              -- semver from the template's frontmatter
    schema_hash TEXT,                                              -- hash of frontmatter shape (for dedup-lookup)
    updated_at  TEXT NOT NULL
);

CREATE INDEX idx_types_namespace ON wiki_types_registry(namespace);
