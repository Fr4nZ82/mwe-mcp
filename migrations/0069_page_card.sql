-- 0069_page_card — the page's one-line card, in a table you can query.
--
-- A page's CARD is its testata `description`: the single line saying what
-- belongs on that page. It is the only thing the recall navigator is shown
-- when it decides whether to open a page, and for a page no [[wikilink]]
-- points at, it is the only thing that can bring a reader there. Until now it
-- lived in exactly one place — the `.md` frontmatter — so the only way to ask
-- anything about it was to open every page and parse its YAML.
--
-- WHY A TABLE. Two questions the engine cannot answer today, and neither is
-- about speed:
--
--   * "Which pages describe something like this?" The narrative compiler's
--     page index (`compiler::page_index_block`) renders EVERY page of the
--     memory with its description, uncapped, and grows linearly forever. The
--     replacement is a similarity selection over the cards — and a card that
--     is not queryable cannot be ranked. That is what `embedding` is for; it
--     is NULL until the selection is built, and the column is here so that
--     work does not need a second migration over the same rows.
--   * "What does this page say it is for?" asked from anywhere that is not
--     already holding the file open — the dashboard, a sweep, a lint.
--
-- THE FILE STAYS THE TRUTH. This is a cache, in the same class as the smart
-- wiki projection of `_meta.md` (0062): the operator edits a testata by hand
-- in Obsidian, the nightly compile rewrites pages wholesale, and both must
-- win. So:
--
--   * the rows are (re)written by the reindex pipeline, which is the one path
--     every page change already flows through — the watcher on each edit,
--     `reindex_full` as the cold-start and missed-event safety net;
--   * a MISSING row is never an error: every reader falls back to opening the
--     page, so an empty table degrades to exactly today's behaviour and
--     `rm engine.db` + a reindex rebuilds it;
--   * a STALE row is caught before it is shown, by the stamp below.
--
-- THE STAMP (`file_mtime_ms`, `file_size`). The display path compares them
-- against the file it is about to describe and falls back to reading when
-- they disagree — a `stat` where there used to be a read plus a YAML parse.
-- It is not a hash: hashing means reading, which is the cost being avoided.
-- The gap it leaves is an edit that changes neither the size nor the mtime
-- millisecond, which for a hand-edited description means replacing it with
-- another of exactly the same length inside the same tick; the consequence is
-- one stale line for the seconds until the watcher rewrites the row, and a
-- hash would cost the whole saving on every candidate of every hop to close
-- it.
--
-- Ranking (the similarity selection) deliberately does NOT check the stamp:
-- a slightly stale description changes which pages are *offered*, never what
-- is *shown*, and the offer is approximate by nature.
--
-- KEY. `source_path` alone, workdir-relative — the same key `fact_index` and
-- `wiki_sections` use, and it is unique across the forest because it carries
-- the wiki's directory. `wiki_id` rides along as a column so a wiki can be
-- swept in one statement.

CREATE TABLE page_card (
    source_path   TEXT    NOT NULL PRIMARY KEY,  -- workdir-relative page path
    wiki_id       TEXT    NOT NULL,
    description   TEXT,                          -- the card; NULL = the page has none
    keywords      TEXT    NOT NULL DEFAULT '[]', -- JSON array, OWNER-tier (never reader-relative)
    style         TEXT,                          -- prosa | prosa-tecnica | lista, verbatim
    file_mtime_ms INTEGER,                       -- validity stamp; NULL = unstamped, always re-read
    file_size     INTEGER,
    embedding     BLOB,                          -- NULL until the card selection is built
    embedding_dim INTEGER,
    updated_at    TEXT    NOT NULL
);

-- Wiki-scoped sweep (rebuild, drop on wiki removal) is the hot write path.
CREATE INDEX idx_page_card_wiki ON page_card(wiki_id);
