-- 0074_link_key — the clause around a link, as a second way to find the facts
-- beside it.
--
-- A `[[wikilink]]` is written inside a sentence, and that sentence says WHY
-- the two pages belong together: *«le abitudini e ricette che ne derivano sono
-- raccolte in [[famiglia/cucina]]»*. Today that sentence is prose and nothing
-- else — the parser reads the link out of it and the words around the link are
-- never indexed. So a fact sitting beside it can only be found by its own
-- words, and a question phrased in none of them does not reach it.
--
-- WHAT IT BUYS, MEASURED. On the external lab's corpus, indexing that span and
-- letting it stand in for the facts next to it moved one fact — a coeliac
-- diagnosis — from ranks 21/49/83/286 to 7/7/14/18 on four cooking questions
-- that say neither "coeliac" nor "gluten", with retrieval precision unchanged
-- (0.978 → 0.974). The numbers come from one corpus, one language and a
-- different embedder: they are a direction, not a setting.
--
-- THREE CONSTRAINTS, EACH MEASURED, EACH COUNTER-INTUITIVE. All three are
-- enforced in code (`link_key.rs`, `recall.rs`); they are recorded here
-- because the table's shape only makes sense with them:
--
--   1. Store the CLAUSE, not the sentence. A whole-sentence key is a blur: the
--      same coeliac fact reaches rank 21–49 instead of 7. The clause is the
--      span the link sits in, cut further at dashes, semicolons and colons.
--   2. Cover only the facts BESIDE it — the ones on this page, next to the
--      clause. Widening a key to also cover the *target page's* facts puts
--      ~19 keys on every covered fact, the max over nineteen stops
--      discriminating, and the whole gain disappears. That is why `covers` is
--      a short list and not a page reference.
--   3. Score by MAX across keys, never by a summed bonus. The additive form
--      cost 5 points of precision (0.978 → 0.928) for the same reach.
--
-- ONE HOP AND NO FURTHER. At two steps every page of that corpus had 21
-- neighbours out of 456 and "near" stopped meaning anything. There is no
-- transitive column here and there must not be one.
--
-- IT IS A CACHE, like `page_card` (0069), and derived from the same files.
-- The reindex pipeline writes it — the one path every page change already
-- flows through — and a missing row is never an error: without it a fact is
-- found by its own words, which is exactly today's behaviour. `rm engine.db`
-- plus a reindex rebuilds it.
--
-- ACL. A key never widens what a reader may see. It changes the SCORE of facts
-- the reader is already allowed to read — the visibility filter runs before
-- scoring — and the clause text itself is never returned to anyone. So a key
-- written on a page a reader cannot open still cannot show them anything.
--
-- KEY. A synthetic id, because one page carries several links and the same
-- page may carry the same target twice in two different sentences, which are
-- two different keys. `source_path` is indexed instead: the write path
-- rewrites a page's keys wholesale, and the delete path drops them by page.

CREATE TABLE link_key (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source_path   TEXT    NOT NULL,           -- workdir-relative page carrying the link
    wiki_id       TEXT    NOT NULL,           -- its wiki, so a wiki sweeps in one statement
    target        TEXT    NOT NULL,           -- `wiki_id/page-slug` the link points at
    clause        TEXT    NOT NULL,           -- the span of prose the link sits in
    covers        TEXT    NOT NULL,           -- JSON array of the fact_ids beside it
    embedding     BLOB,                       -- NULL when the embedder was down
    embedding_dim INTEGER,
    updated_at    TEXT    NOT NULL
);

-- The two write paths: rewrite one page's keys, drop a wiki's.
CREATE INDEX idx_link_key_page ON link_key(source_path);
CREATE INDEX idx_link_key_wiki ON link_key(wiki_id);
