-- 0002_wiki_events — event queue (schemi.md §1.2)
--
-- Emitted by REM and lifecycle policies. Polled by consumers via
-- `events_poll`, acked via `events_ack`. The `acks` column is a JSON map
-- `{ consumer_id: timestamp_ack }`; multi-consumer ack tracking is the
-- driver for the optional retention sweep (default 30d).

CREATE TABLE wiki_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,                  -- "reminder_due" | "archive_proposal" | … (extensible)
    wiki_id    TEXT,
    fact_id    TEXT,
    payload    TEXT,                           -- JSON, shape per-kind
    created_at TEXT NOT NULL,
    acks       TEXT NOT NULL DEFAULT '{}'      -- JSON: { consumer_id: ack_ts }
);

CREATE INDEX idx_events_kind    ON wiki_events(kind);
CREATE INDEX idx_events_created ON wiki_events(created_at);
