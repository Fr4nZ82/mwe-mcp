-- Schema lift for the unified op-log + briefing-author columns
-- (design baseline 2026-05-26).
--
-- Three additive columns across two existing tables, no destructive
-- renames or backfills. Pre-existing rows keep working because:
--   * `wiki_admin_op_log.actor_kind` gets a DEFAULT that matches the
--     only writer that existed before this baseline (the smart consumer via
--     `wiki_admin_push`);
--   * `wiki_admin_op_log.pre_image_json` is NULL-able, and a NULL
--     value cleanly encodes "no revert possible from this row" —
--     correct for both prior history and the revert handler's compensating
--     `system` rows;
--   * `wiki_briefing_items.author_sender_id` is NULL-able, and older
--     rows had no concept of a human author behind the item.
--
-- Substance refs:
--   * [memory model](../wiki/concepts/memory-model.md) — the unified
--     `wiki_admin_op_log` register of authoritative writes
--   * [tool reference](../wiki/protocol/tool-reference.md) — the
--     op-log push + revert surface

-- ─── 1. wiki_admin_op_log — actor_kind ──────────────────────────────
--
-- Three-valued enum so the dashboard `/op-log` view + the revert
-- handler can discriminate the origin of each row uniformly:
--
--   * 'smart_consumer' — a `consumer_class=smart` token issued a
--     `wiki_admin_push` (the only writer until then). The DEFAULT covers
--     all pre-existing rows: they are all `smart_consumer` writes, no
--     backfill needed.
--   * 'dashboard' — the textual editor of the dashboard saved a page
--     (the path that lifts the op-log to non-companion wikis).
--   * 'system' — a compensating row written by the revert handler
--     itself when it restores `pre_image_json` of a target
--     row. These rows do not carry their own `pre_image_json` — the
--     revert of a revert is performed by clicking the original row
--     again, not by chaining revert-of-revert.
--
-- The CHECK constraint is enforced at row-write time. SQLite supports
-- adding a CHECK in an ALTER ADD COLUMN against the new column only
-- (it gets evaluated for every subsequent INSERT/UPDATE that touches
-- the row), which is exactly what we want — no table rebuild.

ALTER TABLE wiki_admin_op_log
    ADD COLUMN actor_kind TEXT NOT NULL DEFAULT 'smart_consumer'
        CHECK (actor_kind IN ('smart_consumer', 'dashboard', 'system'));

-- ─── 2. wiki_admin_op_log — pre_image_json ──────────────────────────
--
-- JSON snapshot of the page bodies *before* the write the row
-- represents. Shape (informally documented in the
-- [memory model](../wiki/concepts/memory-model.md), formalised by the
-- revert handler):
--
--   { "pages": [
--       { "path": "<rel-path>", "content": "<utf8 body>" | null },
--       ...
--     ] }
--
-- `content: null` distinguishes "the page did not exist before this
-- write" from "the page existed and was empty". The set of paths
-- matches what the row's INSERT touched, so the revert handler can
-- restore each one individually via `atomic_write` and never has to
-- consult the filesystem to discover what to undo.
--
-- NULL is legal in two cases:
--   * legacy rows (no pre-image was captured at the time);
--   * `actor_kind = 'system'` compensating rows (per §10.2 of the
--     protocol, the revert of a revert is performed by clicking the
--     original).
--
-- In both NULL cases the dashboard renders the row without a Revert
-- button — the UI guards on the NULL.

ALTER TABLE wiki_admin_op_log
    ADD COLUMN pre_image_json TEXT;

-- ─── 3. wiki_briefing_items — author_sender_id ──────────────────────
--
-- The `sender_id` of the human author behind a briefing item, when
-- one exists. Populated for:
--   * `source_kind = 'dashboard_comment'` — the admin who
--     typed the inline comment;
--   * `source_kind = 'dashboard'` — the operator who typed a
--     standalone notify item from the dashboard;
--   * `source_kind = 'user'` and `source_kind = 'consumer'` — when a
--     standard consumer forwards a message on behalf of a known
--     `sender_id`.
--
-- NULL is legal for:
--   * `source_kind = 'rem'` — REM is a server-internal actor, no
--     human author exists;
--   * legacy rows where authorship was not captured.
--
-- This is not a foreign key into `enrollment_users(sender_id)` — the
-- column tracks attribution, not referential lifecycle: deleting a
-- user must not silently drop historical authorship from briefing
-- items, and SQLite's lack of deferrable FKs makes the trade-off
-- not worth the friction. The dashboard renders the column verbatim
-- with a defensive `_` fallback when the user no longer exists.

ALTER TABLE wiki_briefing_items
    ADD COLUMN author_sender_id TEXT;

-- ─── 4. `source_kind` enum extension — `dashboard_comment` ──────────
--
-- The inline-comment surface writes rows with the new value
-- `source_kind = 'dashboard_comment'`, alongside the existing four
-- ('user' | 'rem' | 'consumer' | 'dashboard'). This is documented as
-- a column-comment-level convention rather than a CHECK constraint:
-- migration 0023 did not introduce a CHECK on `source_kind`, and we
-- keep it that way to avoid a table rebuild for what is a pure
-- additive widening.
--
-- Rationale for picking `dashboard_comment` over reusing `dashboard`:
-- the discriminator drives a hot query pattern in the dashboard's
-- per-page view:
--
--     SELECT * FROM wiki_briefing_items
--      WHERE wiki_id = ?
--        AND target_cite LIKE 'wiki://<id>/<path>%'
--        AND source_kind = 'dashboard_comment'
--        AND processed_at IS NULL;
--
-- With a single 'dashboard' value, that query would have to disjoin
-- 'dashboard' notifies that happen to carry a `target_cite` from
-- actual inline comments, and the moment a future workflow writes a
-- targeted notify from the dashboard the two get conflated and the
-- inline-comment surface would surface unrelated items. A separate
-- enum value pre-empts the conflation cheaply.
--
-- No SQL change is necessary in this step — this comment block is
-- the authoritative record of the convention. The wire enum is
-- mirrored in `BriefingSourceKind` (Rust).
