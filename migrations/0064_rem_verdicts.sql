-- Negative-verdict memo for the REM cycle's LLM confirmers.
--
-- WHY. Every REM confirmer sweep (dedup pair, page split, sub-wiki
-- emergence, page merge, completion, contradiction, refile) asks the
-- model a question whose subject is a *stable* piece of the corpus, and
-- the overwhelmingly common answer is "no". Nothing recorded that "no",
-- so every night re-bought the same verdict at the same price: measured
-- on the dogfood workdir, the revisor spent its whole 120-confirm budget
-- re-judging the same pairs (156 nominable pairs corpus-wide, 2 merges),
-- which ALSO meant the 36 pairs past the cap were never examined even
-- once. The paragraph-split pass re-sent 19 byte-identical pages to the
-- strong model for the same "no" it gave the night before.
--
-- A row here means: this exact question, on this exact content, judged
-- by this exact model and prompt, already came back negative. The
-- sub-job skips it and the budget goes to questions nobody has asked
-- yet — cheaper AND better coverage, since the caps stop being burned on
-- settled ground.
--
-- Only NEGATIVE verdicts are memoized. A positive one mutates the corpus
-- (facts merge, move, or close), which changes the content the key is
-- derived from — positives self-invalidate and need no bookkeeping.
--
-- `key_hash` is a SHA-256 over (model id, prompt body, subject content).
-- Editing a prompt, switching the model, or touching a single character
-- of a fact's text all yield a different key, so a memo can never
-- outlive the thing it was a verdict about. `subject_ref` is a
-- human-readable breadcrumb for `rem run-cycle` debugging only — never
-- read by the lookup path.
--
-- Bounded by a TTL sweep at cycle start (`RemPolicy::verdict_memo_ttl`,
-- default 90 days) so a corpus that churns cannot grow the table
-- without limit, and so a verdict eventually gets a second opinion even
-- when nothing about its subject changed.

CREATE TABLE rem_verdicts (
    kind        TEXT NOT NULL,  -- sub-job family: 'dedup_pair' | 'page_split' | ...
    key_hash    TEXT NOT NULL,  -- SHA-256 hex of (model id, prompt, subject)
    subject_ref TEXT,           -- debugging breadcrumb (page path, fact ids); never matched on
    created_at  TEXT NOT NULL,  -- RFC-3339 UTC, drives the TTL sweep
    PRIMARY KEY (kind, key_hash)
) WITHOUT ROWID;

-- The TTL sweep is the only scan that does not go through the primary
-- key, and it runs once per cycle over the whole table.
CREATE INDEX idx_rem_verdicts_created ON rem_verdicts(created_at);
