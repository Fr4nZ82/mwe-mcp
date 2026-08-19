-- 0071_capture_buffer_no_destination — a fact waiting in the buffer does not
-- know where it will be written.
--
-- The buffer is the queue of claims waiting to be SORTED and then written as
-- prose. Sorting them is the light dream's job: it reads the buffer, looks at
-- the memory as it stands at that moment, and decides which wiki and which
-- page each claim belongs on. Founder, 2026-08-18: *«i fatti nel buffer non
-- devono avere info su dove andranno messi, perché verrà deciso al momento
-- della lettura da parte del dream light»*.
--
-- Both columns were the classifier's guess, taken at capture time and carried
-- forward as a "hint":
--
--   * `wiki_id`      — derived from the subject the moment the message
--                      arrived. But a fact about one person can belong in a
--                      group's wiki, or in one that has not emerged yet, and
--                      the placement pass is offered the whole forest anyway.
--   * `target_page`  — the classifier is shown no prose pages, so any name it
--                      proposed was a guess the consolidation had to undo. In
--                      practice every buffered row already carried the same
--                      value, the wiki's buffer page.
--
-- The one shape that genuinely knows its page — a `lista` item, and a
-- container the user asked for this turn — never reaches this table: it is
-- written live, page and row together, in the turn that said it.
--
-- Nothing is lost. What the buffer still carries is everything about the CLAIM
-- (subject, sender, allow, type, topics, validity, salience, provenance), and
-- that is what the light dream sorts on. The provisional wiki a promoted fact
-- needs for its "no page yet" address is derived at promotion from the
-- subject, which — unlike the container — never moves.
--
-- The index goes first: SQLite refuses to drop a column an index is built on.

DROP INDEX IF EXISTS idx_capture_buffer_wiki;

ALTER TABLE capture_buffer DROP COLUMN wiki_id;
ALTER TABLE capture_buffer DROP COLUMN target_page;
