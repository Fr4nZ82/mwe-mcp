-- 0041_engine_meta — a tiny key/value table for engine-level state.
--
-- Some state is neither per-fact nor per-wiki and has no natural home in the
-- domain tables. `engine_meta` is the catch-all key/value store for it.
--
-- First consumer (roadmap 18g, the bundled-embedder group): the EMBEDDER
-- IDENTITY the store's vectors were built with — `embedder_model_id` and
-- `embedder_dim`. The configured embedder is compared against it at serve
-- startup, so swapping the embedding model — which would silently corrupt
-- cosine similarity (a different dimension, or a same-dim model whose
-- vectors point elsewhere) — is caught and surfaced as a reindex prompt
-- instead of degrading recall quietly. See
-- ../wiki/planning/18_bundled-embedder.md and
-- ../wiki/design-notes/reindex-pipeline.md.

CREATE TABLE IF NOT EXISTS engine_meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
