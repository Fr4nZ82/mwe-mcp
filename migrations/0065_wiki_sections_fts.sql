-- Lexical (exact-term) index over smart-wiki section text, to be fused
-- with the vector ranking rather than replacing it.
--
-- WHY. Recall was pure vector: every query, including one that is a bare
-- identifier, was answered by cosine distance over embeddings. That is
-- the one class of query embeddings are worst at, because an identifier
-- carries almost no meaning to encode. Measured on the production corpus
-- before writing this: the query `D-006` returned, in order, the section
-- of decision **D-001** (whose body merely cites the string "D-006"), an
-- unrelated changelog entry matching on "D-", and a third wiki's
-- "ADR-006" cross-reference — never the section actually titled `D-006`.
-- The same content asked in prose returned that section first, at 0.68,
-- across a language boundary. A project's decision log, an ADR list, a
-- ticket trail and a stack trace are made of exactly these tokens.
--
-- WHY NOT A GATE. The obvious alternative was to let the ingest
-- classifier decide per turn whether a lexical pass is warranted. It is
-- rejected on purpose, and the reasoning is worth keeping: the gate
-- would cost an LLM judgement to guard a sub-millisecond index lookup,
-- its discriminator is a *surface property of the query string* rather
-- than the invisible intent `[[feedback-no-hardcoded-gates-llm-decides]]`
-- was written for (cf. `recall::recall_signposted_project_docs`, where
-- the model does decide, because no distance separated the cases), and a
-- gate that answers "no" wrongly drops the hit with nothing to notice.
-- Both passes always run; the *ranking* decides, not a switch.
--
-- TWO COLUMNS, AND WHY IT IS NOT A DUPLICATE. `wiki_sections."text"`
-- already begins with the heading chain — it is exactly what was
-- embedded. Indexing `heading_path` again as its own column therefore
-- counts a heading term twice, and that is the point: a section whose
-- *heading* is `D-006` is the decision, a section whose *body* cites
-- `D-006` is a reference to it, and nothing else in the row distinguishes
-- the two. Measured on the production corpus (4 220 sections; the
-- telaiojs decision log, D-001…D-007, each split across 2–4 sections by
-- the chunk cap):
--
--   * one column  — the defining section ranked first for 4 of 7
--     identifiers. `D-006` returned the D-001 section first, reproducing
--     the vector failure for the same reason: bm25 rewards a short block
--     that mentions the term over a long one that is titled with it.
--   * two columns — 7 of 7, at every weight tried.
--
-- The 4.0 heading weight then buys ranks 2 and 3: at 1.0 those go to
-- unrelated sections that cite the identifier, at 4.0 to the *sibling*
-- pieces of the same decision, which is what a reader asking about D-006
-- wants next. 10.0 and 25.0 change nothing further, so the weight is a
-- plateau, not a tuned number. Prose queries are unaffected — three
-- Italian sentences against the same corpus returned the same top-3 at
-- 1.0 and at 4.0, one adjacent swap aside — which is the profile to
-- want: decisive where the embedding is blind, inert where it is not.
--
-- TOKENIZATION. Plain `unicode61`, deliberately: it splits `D-006` into
-- the tokens `d` and `006`, and the identical split applies to the query,
-- so searching `"D-006"` as a **phrase** matches only text where those
-- tokens are adjacent — which is precisely the identifier and not every
-- document containing a stray `d`. Adding `-` to `tokenchars` would keep
-- identifiers whole but would also weld `well-known` into one token that
-- a search for `known` could no longer reach. Diacritics are folded
-- (`remove_diacritics 2`) because half this corpus is Italian.
--
-- EXTERNAL CONTENT. The index stores no copy of the text: it points at
-- `wiki_sections` by rowid (`content=`), so a section's bytes live in one
-- place and cannot drift between the two. The price is that FTS5 does
-- not self-maintain, which is what the three triggers below are for —
-- put in the schema rather than in the Rust write path on purpose, so
-- that no present or future writer (the reindex sweep, the boot-time
-- reconciliation, an operator's manual repair) can bypass them.
--
-- Regenerable by construction: like the sections themselves, this index
-- is derived from disk and can be dropped and rebuilt at any time. On the
-- 4 220-section production corpus it costs 2.5 MB (a 43 MB database grows
-- to 45), and the backfill below rebuilt it in 60 ms — no embedder, no
-- model, no network. That is why it can ship as a migration rather than
-- as a maintenance window: a re-embed of the same corpus takes hours.

CREATE VIRTUAL TABLE wiki_sections_fts USING fts5(
    heading_path,
    "text",
    content = 'wiki_sections',
    content_rowid = 'rowid',
    tokenize = "unicode61 remove_diacritics 2"
);

-- `wiki_sections` upserts with ON CONFLICT DO UPDATE (never INSERT OR
-- REPLACE), so a re-indexed section keeps its rowid and the UPDATE
-- trigger — not a delete/insert pair — is what fires. All three are
-- still needed: pages are dropped whole when a file disappears.
--
-- A delete must repeat the *old* column values, not the new ones: that
-- is how FTS5 external content unwrites the terms it previously indexed.
CREATE TRIGGER wiki_sections_fts_ai AFTER INSERT ON wiki_sections BEGIN
    INSERT INTO wiki_sections_fts(rowid, heading_path, "text")
    VALUES (new.rowid, new.heading_path, new."text");
END;

CREATE TRIGGER wiki_sections_fts_ad AFTER DELETE ON wiki_sections BEGIN
    INSERT INTO wiki_sections_fts(wiki_sections_fts, rowid, heading_path, "text")
    VALUES ('delete', old.rowid, old.heading_path, old."text");
END;

CREATE TRIGGER wiki_sections_fts_au AFTER UPDATE ON wiki_sections BEGIN
    INSERT INTO wiki_sections_fts(wiki_sections_fts, rowid, heading_path, "text")
    VALUES ('delete', old.rowid, old.heading_path, old."text");
    INSERT INTO wiki_sections_fts(rowid, heading_path, "text")
    VALUES (new.rowid, new.heading_path, new."text");
END;

-- Backfill: the corpus already on disk, indexed without re-embedding
-- anything (this index needs no model).
INSERT INTO wiki_sections_fts(rowid, heading_path, "text")
SELECT rowid, heading_path, "text" FROM wiki_sections;
