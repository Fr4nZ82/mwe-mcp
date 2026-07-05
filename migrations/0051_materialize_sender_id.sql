-- 0051_materialize_sender_id — sender_id is the immutable provenance, always materialized
--
-- The cross-user attribution (`sender_id`: who *captured* a fact, as distinct from `owner_id`:
-- whose fact it is) used to be stored sparsely — NULL meaning "sender == owner" — across
-- fact_index, capture_buffer, media_catalog and document_jobs. That collapse is safe only while
-- the owner never changes. It does change (the `acl_change` verb today; document/media ownership
-- transfer in the future), and a NULL `sender_id` is reinterpreted at read time as "== the
-- *current* owner": the moment an owner moves (e.g. a fact re-homed onto a group), the original
-- provenance silently rebinds to the new owner and is lost. This corrupts provenance-consuming
-- features (structure_proposals recipient routing, behaviour-rule recall attribution).
--
-- The model is now: `sender` and `owner` are two separate, always-materialized fields. Provenance
-- is frozen at birth and never collapsed. `sender_id = NULL` survives ONLY as the degenerate
-- "scrubbed" state (e.g. a deleted user) that gracefully falls back to owner at read time.
-- Birth-time materialization is enforced in the engine (capture / capture_buffer / document /
-- media); this migration backfills the rows that pre-date it.
--
-- Best-effort on already-moved rows: for a fact whose owner was already changed while its sender
-- was NULL, the original sender is unrecoverable, so we freeze the *current* owner. For every
-- never-moved self-fact (the dominant case) this captures the correct sender.
--
-- SCOPE: the materialized-provenance invariant is for STANDARD memory wikis (the per-fragment
-- ACL model). Smart (project) wikis are markerless — their fact_index section rows carry a
-- wiki-level ACL projected from `_meta` and MUST keep `sender_id = NULL` (no per-fragment
-- capturer), so that `reproject_wiki_acl` fully controls access via owner/allow and a revoke
-- leaves no stale sender in the read union. Standard marker regions have a byte span
-- (`region_start IS NOT NULL`); smart-wiki section rows do not — that is the discriminator on
-- fact_index. capture_buffer / media_catalog / document_jobs are standard-only surfaces, so they
-- backfill in full.

UPDATE fact_index     SET sender_id = owner_id WHERE sender_id IS NULL AND region_start IS NOT NULL;
UPDATE capture_buffer SET sender_id = owner_id WHERE sender_id IS NULL;
UPDATE media_catalog  SET sender_id = owner_id WHERE sender_id IS NULL;
UPDATE document_jobs  SET sender_id = owner_id WHERE sender_id IS NULL;
