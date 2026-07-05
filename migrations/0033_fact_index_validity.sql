-- 0033_fact_index_validity — per-fact temporal validity (the validity model).
--
-- The old "regime" axis (binary knowledge/state) is retired in favour of a
-- per-fact validity INTERVAL — stolen verbatim from Zep/Graphiti and extended
-- with a "completion" decay trigger no surveyed system has. Design SSOT:
-- Per-fact validity window; see ../wiki/concepts/memory-model.md.
--
-- Each fact carries an interval [valid_from, valid_to):
--   * valid_from   — when the fact became true (reference-time at capture when
--                    it is about the present). NULL = unknown / "since forever".
--   * valid_to     — when it stops being true. NULL = OPEN ("true now, no
--                    horizon") — this absorbs the old "knowledge" regime; a
--                    closed valid_to is what "state" used to mean. NULL, never
--                    a sentinel date.
--   * decay_reason — why valid_to was closed. NULL while the fact is alive;
--                    set on closure. Closed vocabulary, enforced at the Rust
--                    PRODUCER (not by a DB CHECK — same convention as
--                    deleted_reason / fact_type, see engine-db-and-migrations.md):
--                    'contradiction' | 'expired' | 'completed' | 'cancelled'.
--
-- At recall, validity is a SIGNAL, not a filter: a closed/expired fact is
-- soft-down-ranked, never hidden (the deviant/stale fact is often the gold).
-- The `now >= valid_to` comparison is REM housekeeping, NOT a recall prune, and
-- there is no TTL sweep that deletes (the hard-delete-on-expiry anti-pattern).
--
-- ADDITIVE + inert: all three columns land here so the schema is ready, but no
-- producer writes them and no reader keys off them until the slice-E writer
-- brick (modello-memoria-esecuzione.md §5, sub-bricks 3b–3d). Capture inserts
-- leave them NULL until then.

ALTER TABLE fact_index ADD COLUMN valid_from   TEXT;
ALTER TABLE fact_index ADD COLUMN valid_to     TEXT;
ALTER TABLE fact_index ADD COLUMN decay_reason TEXT;

-- Partial index over the facts that actually carry an expiry — most facts
-- leave valid_to NULL (open), so this mirrors the idx_fact_recall partial-index
-- pattern and stays small. Backs the REM `now >= valid_to` housekeeping scan
-- and the recall actuality signal.
CREATE INDEX idx_fact_valid_to ON fact_index(valid_to) WHERE valid_to IS NOT NULL;
