-- 0083_assistant_rules_are_everybodys — a rule an assistant holds about
-- itself is everybody's, and this writes that down in its audience.
--
-- A standing directive lives as a `rule` fact on a wiki's `@rules.md`, and who
-- it reaches is the question `acl::can_read` answers of every fact: subject ∪
-- allow ∪ narrator. Under it a rule whose subject is the ASSISTANT is readable
-- by the assistant alone, since no person is ever the assistant — so an
-- assistant's own directives, filed with an empty audience, would reach
-- nobody, and a household assistant would forget its own name mid-conversation.
--
-- Their audience is everybody: that is what an agent-wide rule means, it is
-- what every one of these rows was already being served as, and writing it
-- into `allow_ids` is what lets the rules channel ask `can_read` like every
-- other read path without anybody losing a rule they rely on. The write side
-- sets `["global"]` on every agent-wide rule from here on
-- (`ingest::file_behaviour_rule`); this is the same answer for the rows that
-- were written before it did.
--
-- A PERSON's rule is not touched. One with an empty audience is read by its
-- subject, who is that person — exactly who was getting it — so those rows
-- keep the audience they were written with: narrowing what is already stored
-- is not a migration's business (founder, 2026-09-14).
--
-- `is_agent` is the engine-written marker a consumer's identity carries, so
-- «whose subject is an assistant» is asked of the enrolment and not guessed
-- from a name.

UPDATE fact_index
   SET allow_ids = '["global"]'
 WHERE fact_type = 'rule'
   AND deleted_at IS NULL
   AND (allow_ids IS NULL OR allow_ids = '' OR allow_ids = '[]')
   AND substr(subject_id, 1, 5) = 'user:'
   AND EXISTS (
         SELECT 1 FROM enrollment_users u
          WHERE u.user_id = substr(fact_index.subject_id, 6)
            AND u.is_agent = 1
       );
