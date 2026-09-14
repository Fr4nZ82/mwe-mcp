-- 0085_only_a_card_claim_is_the_assistants_identity — an assistant's identity
-- is a claim its card would carry, and nothing else is everybody's.
--
-- «What the assistant IS» is the one kind of self-memory that goes to whoever
-- talks to it, and the engine has one definition of what that is: the KIND
-- says who somebody is AND it is marked always-on
-- (`fact_index::belongs_on_an_identity_card`, `bio` AND `high`). Either half
-- alone is not it — a memory of a single afternoon, marked always-on because
-- it mattered that day, is something the assistant DID, with somebody, and it
-- belongs to that person.
--
-- The pass that wrote the answer onto the rows already stored (0085's
-- predecessor) asks the looser question, `high` OR `bio`. A migration is never
-- edited once it exists — every installation has checksummed it — so this runs
-- straight after it and takes back the half it should not have given: an
-- assistant's self-memory whose audience is exactly «everybody» and which is
-- not a card claim goes back to an empty audience, which is where it started.
-- In one start-up sequence the two are one answer.
--
-- **It narrows, and that is why it is safe to run anywhere.** On an
-- installation where such a row was `global` before either pass, this takes
-- that back: an audience nobody chose deliberately, on a memory of something
-- done with one person. Narrowing what is already stored needs no permission;
-- widening would.
--
-- Two things it must not touch, and the predicate says so rather than relying
-- on the order of the two passes:
--
-- - a row whose audience somebody widened themselves — the test is `allow_ids`
--   being EXACTLY `["global"]`, the shape a pass writes, and never a list with
--   a person or a group in it;
-- - a STANDING RULE. An assistant's own directives live on its `@rules.md` and
--   are everybody's by nature (migration 0083, and for the same reason: nobody
--   is the assistant, so an empty audience there reaches nobody and a
--   household assistant forgets its own name). They are not self-memory and
--   this is not their question.

UPDATE fact_index
   SET allow_ids = '[]'
 WHERE superseded_at IS NULL AND deleted_at IS NULL
   AND allow_ids = '["global"]'
   AND substr(subject_id, 1, 5) = 'user:'
   AND EXISTS (SELECT 1 FROM enrollment_users u
                WHERE u.user_id = substr(fact_index.subject_id, 6) AND u.is_agent = 1)
   AND NOT (fact_type = 'bio' AND salience = 'high')
   AND source_path NOT LIKE '%/@rules.md'
   AND source_path NOT LIKE '%/rules.md';
