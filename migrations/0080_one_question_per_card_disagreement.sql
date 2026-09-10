-- 0080_one_question_per_card_disagreement — one pending question per
-- (stored fact, card box, claimed value).
--
-- A claim that refills a box of somebody's identity card is put to the person
-- whose card it is, and the engine looks first for a question already waiting
-- on the same disagreement so the same private value is not placed in front of
-- them once per turn somebody restates it. That lookup is a read followed by a
-- write, and two turns can both read "nothing pending" before either writes.
--
-- WHAT THE INDEX IS FOR. It is the fence under the lookup, not a substitute
-- for it: the lookup answers the common case and tells the speaker what they
-- are always told, and this refuses the second row when two turns raced. The
-- loser's INSERT fails, the turn logs it and stands, and the claim it had
-- already parked is dropped by the sweep that collects parked claims with no
-- pending question (`capture_buffer::sweep_orphan_held`).
--
-- WHY THE THREE PARTS. The stored fact and the box say WHICH box of WHOSE card
-- is being argued over. The claimed value — folded to letters and digits, so
-- one number written two ways is one value — is what makes two DIFFERENT
-- claims two questions: keyed on the pair alone, a second person asserting a
-- third value found the first question standing and was swallowed by it, and
-- the card's owner was shown a choice between two values one of which nobody
-- had said.
--
-- PENDING ONLY. A question that has been answered or has expired is history,
-- and the same disagreement may honestly be raised again later; the partial
-- clause is what lets the table keep every one of them.
--
-- A row whose context carries no `asserted_key` indexes as NULL, and SQLite
-- holds NULLs distinct in a unique index, so such rows constrain nothing and
-- the index can be created on a database that already holds them.

CREATE UNIQUE INDEX idx_slot_conflict_one_question
    ON structure_proposals(
        json_extract(context, '$.kept_fact_id'),
        json_extract(context, '$.slot'),
        json_extract(context, '$.asserted_key')
    )
 WHERE kind = 'slot_conflict' AND status = 'pending';
