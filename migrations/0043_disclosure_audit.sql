-- 0043_disclosure_audit — append-only audit of per-fact ACL changes made
-- from the consumer chat (the `acl_changes` ingest verb; see
-- ../wiki/design-notes/ingest-pipeline.md).
--
-- The owner can broaden or narrow who reads their OWN fact straight from a
-- conversation ("esponi questa memoria a tutti", "condividila col gruppo
-- famiglia"). Because that change is applied act-first (no pre-confirm),
-- every change leaves an immutable row here: who acted, the previous and
-- the new read-set, and whether the change WIDENED disclosure (a new
-- principal entered the effective read-set — the privacy-relevant signal,
-- computed by crate::acl::widens). The dashboard fact-audit view and any
-- future compliance export read this table.
--
-- Append-only: a row is never updated except to stamp `reverted_at` when
-- the born-applied receipt's revert restores the prior ACL
-- (crate::disclosure_audit::mark_reverted). It is NOT rebuildable from the
-- filesystem — the ACL itself is DB-authoritative (fact_index), and this
-- is the change LOG of that column, so it is durable engine state like the
-- op-logs, not a cache over a journal.
--
-- Principal columns carry the wire string form ('global' | 'user:<id>' |
-- 'group:<id>'); the allow columns are JSON arrays of those strings (same
-- codec as fact_index.allow_ids). `widening` is 0/1 (SQLite boolean).

CREATE TABLE disclosure_audit (
    audit_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id         TEXT NOT NULL,
    wiki_id         TEXT NOT NULL,
    actor_id        TEXT NOT NULL,                 -- who made the change (raw session sender id)
    prev_owner_id   TEXT NOT NULL,
    prev_allow_ids  TEXT NOT NULL DEFAULT '[]',    -- JSON array of principals
    prev_sender_id  TEXT,
    new_owner_id    TEXT NOT NULL,
    new_allow_ids   TEXT NOT NULL DEFAULT '[]',    -- JSON array of principals
    new_sender_id   TEXT,
    widening        INTEGER NOT NULL DEFAULT 0,    -- 1 = the change introduced a new reader
    reverted_at     TEXT,                          -- ISO-8601, set when the receipt is reverted
    ts              TEXT NOT NULL                  -- ISO-8601, when the change was applied
);

CREATE INDEX idx_disclosure_audit_wiki ON disclosure_audit(wiki_id, ts);
CREATE INDEX idx_disclosure_audit_fact ON disclosure_audit(fact_id, ts);
