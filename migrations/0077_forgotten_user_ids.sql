-- 0077_forgotten_user_ids — an id that was erased is never handed to
-- somebody else.
--
-- Forgetting a person (`gdpr::forget_user`) leaves their id free: the
-- `enrollment_users` row goes, and nothing then stops an admin creating a
-- new account under the same id an hour later. That new person inherits
-- everything the memory still says about the old one — a fact handed to
-- another speaker keeps the erased name in `subject_external`, a page
-- somebody else wrote still names them, and every one of those sentences
-- reads as being about whoever holds the id now. The erasure would have
-- moved the material rather than removed it.
--
-- So the id is retained, alone, and its only use is to refuse. A
-- suppression list is the one thing an erasure has to keep in order to
-- honour itself: without the id there is no way to know which id must not
-- come back. Nothing else about the person is here — no email, no alias,
-- no display name, no counts of what was erased — and no read path joins
-- this table to anything.
--
-- WHY NOT A HASH. It would look more careful and buy nothing: these ids
-- are short, lowercase and drawn from the people in one household or one
-- team, so a hash of them is reversible by anybody who can guess a first
-- name. Storing the id plainly is the honest version of the same
-- retention.
--
-- NO FOREIGN KEY, deliberately. The row outlives the `enrollment_users`
-- row it is about — that is the entire point — so it cannot reference it.
--
-- THE ROW IS NOT A TOMBSTONE OF THE PERSON. It says one thing: this id is
-- spent. An admin who wants the same human back gives them a different id.

CREATE TABLE forgotten_user_ids (
    user_id      TEXT PRIMARY KEY,
    forgotten_at TEXT NOT NULL
);
