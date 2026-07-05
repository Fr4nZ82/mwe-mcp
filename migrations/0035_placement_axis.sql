-- 0035_placement_axis — thread the ingest PLACEMENT/STYLE axis onto the fact
-- (slice-E brick M4-c). Sibling of the validity axis (0033/0034).
--
-- The ingest classifier (the "Cartografo at runtime", modello-memoria §4.6)
-- already decides, per claim, WHERE it goes — `target_page` — plus the page's
-- `style` and a "what goes in here" `page_description`. On the NARRATIVE path
-- that signal was lost: `target_page` rode on the buffer but died at promotion
-- (`promote_one` minted a `NewFact` without it), and `style`/`page_description`
-- were dropped by `buffer_capture` outright. So the light dream had to re-run
-- the strong-model Cartografo to re-derive placement it already knew.
--
-- This lands the columns so the placement survives buffer → promote → fact, and
-- the LIGHT cadence can home a fact on its ingest page WITHOUT the Cartografo
-- (which becomes REM-only, §4.6). It is the storage half of M4-c; the consumer
-- (build_wiki_plan in the light cadence) lands in the next sub-brick.
--
-- Semantics (all NULL = "not proposed / unknown", inert):
--   fact_index.target_page        — the page slug/path the classifier proposed
--                                   at ingest. A HINT, not the fact's home: the
--                                   compilation plan is the authority on where a
--                                   fact actually lives, and the REM Cartografo
--                                   may re-home it. Read only in the light
--                                   cadence, only for a not-yet-planned fact.
--   fact_index.style              — proposed page writing style (closed palette
--                                   prosa | prosa-tecnica | lista, §4.3). Seeds
--                                   a freshly-created page's testata.
--   fact_index.page_description   — proposed "cosa ci va dentro" one-liner that
--                                   seeds the page's testata description (§4.5).
--   capture_buffer.style          — same, staged on the buffer so promote_one
--   capture_buffer.page_description  copies it onto the fact. (`target_page`
--                                   already exists on capture_buffer.)
--
-- Filesystem-SSOT invariant preserved on the buffer: like the validity axis
-- (0034 vf=/vt=), `style` and `page_description` are mirrored into the per-wiki
-- `_captures.md` journal so `rm engine.db` + reindex regenerates them. `style`
-- is a whitespace-free enum (rides as a bare attr); `page_description` is free
-- text, so the whitespace-delimited journal codec percent-escapes it.
--
-- ADDITIVE + inert: older rows / journal entries without the attributes parse
-- to NULL. No reader keys off the new columns until brick M4-c.2.

ALTER TABLE fact_index ADD COLUMN target_page      TEXT;
ALTER TABLE fact_index ADD COLUMN style            TEXT;
ALTER TABLE fact_index ADD COLUMN page_description TEXT;

ALTER TABLE capture_buffer ADD COLUMN style            TEXT;
ALTER TABLE capture_buffer ADD COLUMN page_description TEXT;
