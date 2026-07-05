-- 0039_media_catalog — the per-media twin of fact_index (the media pipeline).
--
-- A media item enters memory as an ordinary described fact whose body
-- carries a bare `{{embed=<catalog_id>}}` marker; the marker is the KEY,
-- and everything behind it — including the per-media ACL — is authoritative
-- here, exactly like the per-fact metadata in fact_index. The bytes live in
-- the global content-addressed store at <workdir>/media/<aa>/<sha256>
-- (sha256 hex, sharded by the first two hex chars); dedup of identical
-- bytes comes free from the addressing. Design SSOT:
-- ../wiki/design-notes/media-pipeline.md.
--
-- NOT rebuildable from the markdown: like the fact ACL after the
-- DB-authoritative move, the catalog metadata has no filesystem mirror —
-- the workdir snapshot (backup.rs) is the recovery story, and a dogfood
-- reset must wipe <workdir>/media/ together with engine.db.

CREATE TABLE media_catalog (
    -- `c-YYYY-MM-DD-<kind>-NNN.<ext>` — minted server-side at upload, the
    -- only token that ever appears in a memory-wiki page.
    catalog_id  TEXT PRIMARY KEY,

    -- Lowercase hex sha256 of the blob — the content address. Two rows may
    -- share a hash (same bytes uploaded by different owners); one (hash,
    -- owner) pair never repeats (idempotent re-upload returns the row).
    sha256      TEXT NOT NULL,

    -- 'photo' | 'video' | 'audio' | 'doc'. Closed vocabulary enforced at
    -- the Rust producer (media::kind), not by a DB CHECK — same convention
    -- as fact_type / decay_reason.
    kind        TEXT NOT NULL,

    -- MIME type as declared at upload (after a light server-side sanity
    -- pass) — what GET /media serves as Content-Type.
    mime        TEXT NOT NULL,

    size_bytes  INTEGER NOT NULL,

    -- ACL triple, twin of fact_index: owner is always explicit (no
    -- acl_default inheritance for catalog rows — fail closed), allow_ids
    -- is a JSON array of principal strings, sender_id NULL when == owner.
    -- Provisional at upload (owner = the act-as-resolved principal);
    -- monotonically WIDENED to the union of every linked fact's ACL when
    -- ingest files the facts — widening only, never narrowing.
    owner_id    TEXT NOT NULL,
    allow_ids   TEXT,
    sender_id   TEXT,

    -- The consumer that performed the upload (token claim), for audit;
    -- NULL when the principal uploaded with its own token.
    uploaded_by_consumer TEXT,

    -- Operator/consumer-supplied annotations; the described FACT remains
    -- the recall surface — these only feed the dashboard (alt text) and
    -- the export manifest.
    caption     TEXT,
    description TEXT,

    -- Original client filename from the multipart part, when present.
    original_filename TEXT,

    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Content-address lookup (upload dedup walks hash → rows).
CREATE INDEX idx_media_sha256 ON media_catalog(sha256);

-- Idempotent re-upload: same bytes + same owner = same catalog row.
CREATE UNIQUE INDEX idx_media_sha256_owner ON media_catalog(sha256, owner_id);

-- The per-day-per-kind NNN minting scans the catalog_id PRIMARY KEY by
-- prefix (LIKE 'c-<date>-<kind>-%'); no created_at index is needed.
