-- Successor pointer on a LIVE row. `close_validity` stamps the fact that
-- replaced this one whenever the closer knows it (a contradiction
-- satellite inherits the seed's superseding fact; a completion closure
-- points at its evidence fact). Distinct from `superseded_by`, which is
-- welded to the `superseded_at` tombstone and leaves the page: a
-- `successor_fact_id` rides a closed but still-rendered fact so the
-- narrative can point the reader at the current truth ("no longer
-- current — today see [[…]]").
ALTER TABLE fact_index ADD COLUMN successor_fact_id TEXT;
