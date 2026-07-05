-- A closure gesture can land while its target is still a buffered capture
-- (same-day flows: "compra il latte" → "ho comprato il latte" before the
-- light dream promotes the item). The closing valid_to already rides the
-- staged validity column; this stages the WHY so promotion can stamp
-- fact_index.decay_reason. DB-only post-capture mutation — the on-disk
-- journal is not rewritten, same durability class as status/processed_at.
ALTER TABLE capture_buffer ADD COLUMN decay_reason TEXT;
