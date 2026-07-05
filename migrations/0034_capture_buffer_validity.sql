-- 0034_capture_buffer_validity — thread the per-fact validity interval through
-- the NARRATIVE buffer→promote path (slice-E brick M4-b).
--
-- Brick 3c (migration 0033) added valid_from/valid_to to fact_index and threaded
-- them on the DIRECT capture path (capture.rs → NewFact). The NARRATIVE path
-- (buffer_capture → light dream → promote_one) dropped them: buffer_capture
-- destructured them away and promote_one minted NewFact with NULL validity, so a
-- compiled narrative fact never carried a horizon. This closes that gap: the
-- buffer stages the interval so promote_one copies it into fact_index, which
-- unblocks the dormant brick-3d validity render in the Cronista's prose.
--
-- Same shape/semantics as fact_index: valid_from NULL = unknown / since-forever,
-- valid_to NULL = OPEN ("true now, no horizon"). decay_reason is NOT staged here
-- — a buffered capture is always alive; closure happens later, on the promoted
-- fact (decay_reason lives only on fact_index).
--
-- Filesystem-SSOT invariant preserved: these columns are mirrored in the per-wiki
-- _captures.md journal entry (vf= / vt= attributes, ISO-8601, whitespace-free), so
-- `rm engine.db` + reindex regenerates them. ADDITIVE — older journal entries
-- without the attributes parse to NULL (open).
--
-- Scope note: the PLACEMENT/STYLE axis (target_page already exists; style,
-- page_description) is NOT staged here. It is delivered to the compiler in brick
-- M4-c together with target_page (one mechanism), and page_description is free
-- text the whitespace-delimited journal codec cannot carry without a redesign.
-- Keeping M4-b to the validity axis keeps the slice green and the axes unglued.

ALTER TABLE capture_buffer ADD COLUMN valid_from TEXT;
ALTER TABLE capture_buffer ADD COLUMN valid_to   TEXT;
