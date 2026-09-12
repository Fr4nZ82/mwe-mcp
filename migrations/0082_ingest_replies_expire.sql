-- 0082_ingest_replies_expire — a kept turn carries its own lifetime, and what
-- it keeps is the WRITE half of that turn.
--
-- WHAT THE `reply` COLUMN HOLDS. 0081 called it the tool's whole answer, and
-- it is not: a re-delivered turn READS the memory again, because ten minutes
-- is long enough for a fact to be forgotten or a permission narrowed and a
-- block served from a row would hand the forgotten thing back. What is kept is
-- what the turn DECIDED — the intent, the seed, the id of what it filed, the
-- notices it owes, the question it asked — and nothing that describes the
-- store (`mwe_core::ingest_replay::TurnOutcome`). The column keeps its name:
-- renaming it would cost a table rewrite for a word.
--
-- 0081 keyed the buffer's life to a window read at serving time, which left
-- the rows themselves with no expiry: a deployment where nobody talks for a
-- week kept the last turn's write outcome — its notices, the question it
-- asked — for that week, because the only thing that ever removed a row was
-- the next turn writing one.
--
-- `expires_at` is stamped when the row is written, from the window in force
-- then. Three readers agree on it with no knob between them: serving refuses a
-- row past it, the write path drops what is past it on the way in, and the
-- light round sweeps by the clock so an idle install keeps nothing. A window
-- the operator changes applies to rows written after the change; the rows
-- already there keep the lifetime they were written with, which is the one
-- they were promised.
--
-- Rows that predate this column carry '' and are therefore already expired:
-- every string comparison puts them before any ISO instant. That is correct
-- for a buffer whose whole life is ten minutes — no row surviving this
-- migration was still servable.

ALTER TABLE ingest_replies ADD COLUMN expires_at TEXT NOT NULL DEFAULT '';

-- The sweep and the serving check both read `expires_at`; nothing reads
-- `created_at` in a WHERE clause any more, so its index is dead weight on
-- every insert.
DROP INDEX idx_ingest_replies_age;
CREATE INDEX idx_ingest_replies_expiry ON ingest_replies(expires_at);
