-- 0070_rename_owner_to_subject — the per-fragment ACL axis is named for what
-- it holds: the fact's SUBJECT.
--
-- WHAT THIS COLUMN ALWAYS WAS. `owner_id` never meant what "owner" means in
-- Unix or in IAM. The creator of a fact is `sender_id` (provenance); who may
-- read it beyond that is `allow_ids` (audience); and this column is the third,
-- independent axis: who or what the fact is ABOUT. A weight belongs to the
-- person it weighs, a group fact to the collective it describes, a world fact
-- to `global` and to nobody in particular.
--
-- The old name was defended on the grounds that a data subject governs who may
-- read a fact about them — an `acl_change` is subject-or-admin — so the subject
-- "owns" the datum on themselves. That is true, and it was still the wrong
-- name: it made the axis read as authorship or as visibility, which are the
-- other two columns, and it collided head-on with the FOUR unrelated owners
-- this schema also carries (see the keep-list below). The rename was declined
-- once, near release, for fear that a sweep would corrupt those four. It is
-- carried out now, per sense rather than per word.
--
-- WHAT THIS MIGRATION DELIBERATELY DOES NOT TOUCH.
--
--   * `smart_wikis.owner_id` (0062) and its index `idx_smart_wikis_owner` —
--     the WIKI's resolved scope principal, its proprietor. A genuine owner,
--     and a separate axis: a fact whose subject is `user:franz` can live in a
--     wiki owned by `group:famiglia`. Renaming it would merge two things the
--     engine keeps apart on purpose.
--   * `idx_document_jobs_idem` (0040) — its NAME carries no "owner"; SQLite
--     rewrites the index definition itself when the column moves. Dropping and
--     recreating it would briefly remove the document idempotency lookup for
--     no gain.
--   * `skills_custom.owner_user` and `idx_skills_custom_owner` (0024) — that
--     table was DROPPED by 0036. Naming it here would abort this migration at
--     startup.
--   * `idx_webagentoauth_refresh_owner` (0044) — the index name says owner but
--     it covers `(sender_id, consumer_id)`; no webagentoauth table has an
--     owner column at all.
--
-- ON-DISK AND ON-THE-WIRE COMPANIONS. The `.md` region marker and the
-- `_captures.md` journal move to `subject=` in the same release, and both
-- readers accept the old key PERMANENTLY — the journal is what a
-- `rm engine.db` rebuild replays and it holds entries from every version the
-- deployment has ever run, so a key that stops parsing there is silent data
-- loss, not an error. Same reasoning for the MCP arguments: a consumer sending
-- the old name must keep narrowing its search, never be silently widened to
-- its whole readable corpus.
--
-- SQLite ≥ 3.25 rewrites every index, view and trigger definition that names a
-- renamed column, so the two DROP/CREATE pairs below are for the index NAMES
-- only, not for correctness.

ALTER TABLE fact_index       RENAME COLUMN owner_id      TO subject_id;
ALTER TABLE capture_buffer   RENAME COLUMN owner_id      TO subject_id;
ALTER TABLE media_catalog    RENAME COLUMN owner_id      TO subject_id;
ALTER TABLE document_jobs    RENAME COLUMN owner_id      TO subject_id;
ALTER TABLE disclosure_audit RENAME COLUMN prev_owner_id TO prev_subject_id;
ALTER TABLE disclosure_audit RENAME COLUMN new_owner_id  TO new_subject_id;

DROP INDEX idx_fact_owner;
CREATE INDEX idx_fact_subject ON fact_index(subject_id);

DROP INDEX idx_media_sha256_owner;
CREATE UNIQUE INDEX idx_media_sha256_subject ON media_catalog(sha256, subject_id);
