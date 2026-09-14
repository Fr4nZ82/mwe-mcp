-- 0086_an_exclusion_is_the_fourth_term_of_a_permission — «not for her» is
-- something a fact carries, not something the audience list can express.
--
-- A person saying «I'd rather Zoe didn't know the number» is not asking for a
-- narrower audience: they are naming somebody who must not be in it, whatever
-- the audience turns out to be. The three axes a permission had — the fact's
-- SUBJECT, its audience, and whoever said it — can each let that person in,
-- and the subject is the one that does it silently: a fact about the parents
-- is readable by every parent, and she is one. Measured on the September demo
-- corpus, both facts carrying an explicit exclusion were readable by the
-- person excluded, from the moment they were written.
--
-- An exclusion is therefore a term of its own, subtracted after the other
-- three: `subject ∪ allow ∪ sender` MINUS `excluded`. It holds against the
-- group she is already in, against a group she joins next year, and against a
-- later widening that names her — because none of those says anything about
-- the wish this fact carries. Revoking it is a thing somebody says about THIS
-- fact.
--
-- The column is empty for everything already written: no fact stored before
-- this ever recorded an exclusion, so there is nothing to back-fill and the
-- migration cannot change what anybody can read today.
ALTER TABLE fact_index ADD COLUMN excluded_ids TEXT NOT NULL DEFAULT '[]';

-- The buffer has to remember it too. A capture waits there until the light
-- dream places it, and an exclusion that did not survive that wait would be an
-- exclusion the memory forgot between hearing it and writing it down.
ALTER TABLE capture_buffer ADD COLUMN excluded_ids TEXT NOT NULL DEFAULT '[]';
