-- 0084_an_assistants_memory_of_you_is_yours_to_read — an assistant's own
-- memory says who may read it, instead of leaving that to a label.
--
-- An assistant keeps two kinds of memory about itself. What it IS — «my name
-- is Gandalf», the voice it speaks with, the limits it works under — is public
-- to anybody who talks to it. What it DID WITH SOMEBODY — «I helped Alice with
-- the freezer bags» — is that person's to read and nobody else's.
--
-- Both are `subject = the assistant` with an empty audience, which under
-- `acl::can_read` means the assistant alone: nobody is the assistant, so on
-- its own neither reaches a single reader. What has been standing in for the
-- second answer is a LABEL — the person's id dropped into `topics` — and a
-- label is not a permission: the nightly passes rewrite `topics`, and a fact
-- whose only gate is a word in a list has no gate at all.
--
-- This writes the answer where the read path looks for it, once, for the rows
-- already stored. The write side does the same from here on
-- (`ingest::capture_agent_self_fact`).
--
-- Both statements SKIP a row that already carries an audience: what somebody
-- chose is not this migration's to replace, and running it twice changes
-- nothing the second time. Both touch ACTIVE rows only — a superseded fact is
-- history, no read path serves it, and who may read it is not a live
-- question.

-- 1. «What I did with you» → you may read it.
--
-- Exactly ONE person among the labels, because the label means «an action WITH
-- that person» and two of them name no one reader. The subject must be an
-- assistant: a fact of a PERSON's that merely mentions another person by name
-- carries the same shape of label and is none of this.
UPDATE fact_index
   SET allow_ids = json_array(
         'user:' || (SELECT j.value FROM json_each(fact_index.topics) j
                      JOIN enrollment_users u ON u.user_id = j.value
                     WHERE u.is_agent = 0)
       )
 WHERE superseded_at IS NULL AND deleted_at IS NULL
   AND (allow_ids IS NULL OR allow_ids = '' OR allow_ids = '[]')
   AND substr(subject_id, 1, 5) = 'user:'
   AND EXISTS (SELECT 1 FROM enrollment_users u
                WHERE u.user_id = substr(fact_index.subject_id, 6) AND u.is_agent = 1)
   AND NOT (salience = 'high' OR fact_type = 'bio')
   AND (SELECT count(*) FROM json_each(fact_index.topics) j
         JOIN enrollment_users u ON u.user_id = j.value
        WHERE u.is_agent = 0) = 1;

-- 2. «What I am» → everybody who talks to me.
--
-- The same answer migration 0083 gave an assistant's standing rules, for the
-- same reason: an assistant that cannot tell anybody its own name forgets it
-- mid-conversation. Identity is `salience high` or `fact_type bio`, which is
-- the shape the write side stamps and the read side sorts by.
UPDATE fact_index
   SET allow_ids = '["global"]'
 WHERE superseded_at IS NULL AND deleted_at IS NULL
   AND (allow_ids IS NULL OR allow_ids = '' OR allow_ids = '[]')
   AND substr(subject_id, 1, 5) = 'user:'
   AND EXISTS (SELECT 1 FROM enrollment_users u
                WHERE u.user_id = substr(fact_index.subject_id, 6) AND u.is_agent = 1)
   AND (salience = 'high' OR fact_type = 'bio');
