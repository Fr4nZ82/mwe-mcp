-- Cross-consumer recent window (group 43): a bounded, TTL'd per-user serving
-- buffer of the exchanges the per-turn ingest already receives. NOT a
-- transcript store: never indexed, never embedded, never REM-processed; the
-- hard cap and the TTL are enforced in the write path
-- (mwe_core::recent_window), so the table stays a few dozen rows per user.
CREATE TABLE recent_exchanges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,               -- acting user (act-as after senderMap), bare id
    consumer_id TEXT NOT NULL DEFAULT '', -- the consumer token's system-user ('' when unknown)
    channel TEXT NOT NULL DEFAULT '',    -- consumer-chosen surface label (metadata.channel)
    author TEXT NOT NULL,                -- 'user' | 'assistant'
    text TEXT NOT NULL,
    occurred_at TEXT NOT NULL            -- ISO-8601, the turn's semantic clock
);
CREATE INDEX idx_recent_exchanges_user ON recent_exchanges(user_id, id);
