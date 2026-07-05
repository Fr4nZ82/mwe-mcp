-- Self-correcting REM, the repair stages: the miss lifecycle and the
-- query-side seeds the gold-set gate replays with.

-- The classifier's topic seeds of the logged turn (JSON array) — the
-- query side of a faithful gate replay (production gathers entry points
-- from these seeds; the replay must too).
ALTER TABLE recall_log ADD COLUMN topics TEXT NOT NULL DEFAULT '[]';

-- Miss lifecycle: new → repaired | queued | discarded | stale.
ALTER TABLE recall_misses ADD COLUMN status TEXT NOT NULL DEFAULT 'new';
-- Resolution anchor: the repair receipt id, or a short reason tag.
ALTER TABLE recall_misses ADD COLUMN resolution TEXT;
-- The turn's classifier topic seeds, carried onto the miss.
ALTER TABLE recall_misses ADD COLUMN seed_topics TEXT NOT NULL DEFAULT '[]';

CREATE INDEX idx_recall_misses_status ON recall_misses(status);
