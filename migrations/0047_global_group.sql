-- 0047_global_group — seed the builtin universal `global` group
--
-- Source of truth: [identity and ACL](../wiki/concepts/identity-and-acl.md)
-- + [engine DB and migrations](../wiki/design-notes/engine-db-and-migrations.md).
--
-- `global` is the public/world ACL principal: every user is implicitly a
-- member, so a region naming it (in owner, allow, or sender) is readable by
-- anyone. It is modelled as a normal `enrollment_groups` row purely so the
-- admin can edit its `scope` from the dashboard (the prose the ingest
-- classifier consults to recognise a genuine world fact). Its `members`
-- column is unused — membership is universal, enforced in code
-- (`acl::principal_matches`), not stored. The default scope frames `global`
-- ownership as world facts, NOT as "a public personal fact" (that is the
-- `allow` visibility axis), so the classifier never collapses public
-- profile facts onto `owner=global`.
--
-- Idempotent: `INSERT OR IGNORE` never clobbers an admin-edited scope, and
-- `enrollment::ensure_global_group` re-applies the same seed at runtime so
-- the row survives an enrollment file sync that omits it.

INSERT OR IGNORE INTO enrollment_groups (group_id, members, scope)
VALUES (
  'global',
  '[]',
  'General, public-domain facts that are true for everyone and belong to no single user or group (e.g. common knowledge, weather, public events). NOT for making a personal fact public — that is visibility (put `global` in a fact''s allow-list), not ownership.'
);
