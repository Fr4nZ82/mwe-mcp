-- 0045_user_email_on_enrollment — move the login email onto enrollment_users
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md).
--
-- 0017 put the login `email` on `user_credentials`, which is born at
-- accept-invite — too late for the admin to set it when *inviting*. The
-- admin now types the user's email at invite time (the "Add user" form),
-- before any credential row exists, so the durable home for the email is
-- `enrollment_users` (created at invite). Login reads it from there; the
-- `user_credentials.email` column from 0017 is retired.

ALTER TABLE enrollment_users ADD COLUMN email TEXT;

-- Same partial-unique shape as 0017: uniqueness only bites once the email
-- is set, so a (rare) NULL-email straggler does not collide. Going forward
-- the "Add user" form makes the email mandatory, so NULLs are transitional.
CREATE UNIQUE INDEX idx_enrollment_users_email
  ON enrollment_users(email) WHERE email IS NOT NULL;

-- Carry the bootstrap admin's email up from user_credentials so the one
-- existing login keeps working across the move.
UPDATE enrollment_users
   SET email = (SELECT c.email FROM user_credentials c WHERE c.user_id = enrollment_users.user_id)
 WHERE email IS NULL;

-- Retire the 0017 column: drop its unique index first, then the column.
DROP INDEX IF EXISTS idx_user_credentials_email;
ALTER TABLE user_credentials DROP COLUMN email;
