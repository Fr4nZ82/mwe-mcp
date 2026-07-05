-- 0046_drop_enrollment_blurbs — retire the cosmetic free-prose blurbs
--
-- Source of truth: [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md).
--
-- `enrollment_users.profile` (0006) was a free-prose "how to address the
-- user" note that nothing read at runtime: it only seeded the identity
-- wiki title at creation, so the title now falls back to the `user_id`.
-- `enrollment_groups.description` (0006) was the same cosmetic shape for a
-- group and is fully subsumed by `scope` (the prose the ingest classifier
-- actually routes on); the planner's group_theme summary now derives from
-- the `group_id`. Both columns are dropped — the surviving content channels
-- are the per-user welcome primer (a fact in the wiki) and the group `scope`.

ALTER TABLE enrollment_users DROP COLUMN profile;
ALTER TABLE enrollment_groups DROP COLUMN description;
