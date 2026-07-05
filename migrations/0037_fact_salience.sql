-- 0037_fact_salience — per-fact SALIENCE signal (the index=base-context model).
--
-- modello-memoria.md §8.bis (decision 2026-06-08): a user's actor-wiki `index.md`
-- is the always-injected recall base context, so it must carry the facts that
-- must be known in EVERY interaction. The pivot is SALIENCE, not category:
-- identity + health/safety + hard constraints are high-salience (→ index.md);
-- hobbies/colour/trivia are not. Salience was already foreseen as a future
-- per-fact column; see ../wiki/planning/6_db-authoritative-storage.md.
--
-- Each fact carries an optional salience classification:
--   * salience — how always-relevant the fact is. NULL = unspecified (treated
--                as `normal`). Closed vocabulary, enforced at the Rust PRODUCER
--                (not by a DB CHECK — same convention as fact_type / decay_reason,
--                see engine-db-and-migrations.md):
--                'high' | 'normal' | 'low'.
--                'high' = must be known in every interaction → routed to the
--                actor-wiki index.md (slice 2); the LLM decides it (no hardcoded
--                gate), kept lean by a high bar + a resource cap + the dashboard.
--
-- ADDITIVE: the column lands on fact_index AND capture_buffer (the narrative
-- buffer→promote path, mirrored in the per-wiki _captures.md journal as the
-- whitespace-free `sal=` attribute, like vf=/vt=) so the signal survives both
-- the direct and the narrative write paths. The ingest classifier populates it
-- (this same slice); no reader keys off it until the slice-2 index routing.
-- `rm engine.db` + reindex regenerates capture_buffer.salience from the journal;
-- older journal entries without `sal=` parse to NULL (unspecified).

ALTER TABLE fact_index    ADD COLUMN salience TEXT;
ALTER TABLE capture_buffer ADD COLUMN salience TEXT;
