-- 0081_ingest_replies — what a completed ingest turn answered, kept just long
-- enough to answer a RE-DELIVERY of that same turn with it.
--
-- A consumer redelivers a turn it is not sure arrived: the container was
-- killed mid-call, the network retried, a voice note was transcribed twice.
-- The engine already makes the second delivery harmless — nothing is filed
-- twice, nothing is closed twice — but it is not free: the turn runs the
-- classifier, the reconciler and the navigator again, two or three model calls,
-- to produce the answer it produced the first time. And that answer is usually
-- what the re-delivery was FOR: the first container died after writing, so
-- what the person never received was the reply.
--
-- A row holds the tool's whole serialized answer — the recall block, the seed,
-- the capture id, the notices, `needs_disambig`. A repeat inside the window is
-- served from here with no model call and no write; the same words said again
-- after it are a new turn, and replace the row.
--
-- ONE LIVE ROW PER PERSON, across every surface they talk to: recording a turn
-- drops their earlier ones. A retry is always of the call the consumer just
-- made, so nothing is lost by it, and it is what stops a repeat handing back an
-- answer the conversation has moved past — «cosa c'è sulla spesa?», «aggiungi
-- il pane», the same question again. The key stays the full four parts because
-- it is what says WHICH turn the live row is.
--
-- WHY THE SPEAKER IS IN THE KEY. A consumer feeds back its own reply for
-- extraction on the same surface as the user's message, and the two are
-- different turns even when the words coincide. Keying without it would let
-- one answer the other.
--
-- BOUNDED IN THE WRITE PATH, like `recent_exchanges`: every insert first drops
-- what has aged past the window, so the table cannot grow past its contract
-- even if nothing ever reads it. Never indexed, never embedded, never
-- REM-processed — it is a serving buffer, not a record.

CREATE TABLE ingest_replies (
    sender_id   TEXT NOT NULL,          -- acting user, bare id
    consumer_id TEXT NOT NULL,          -- the calling consumer ('' when none)
    author      TEXT NOT NULL,          -- 'user' | 'assistant'
    turn_hash   TEXT NOT NULL,          -- SHA-256 of the turn's input
    created_at  TEXT NOT NULL,          -- ISO-8601, when the first delivery finished
    reply       TEXT NOT NULL,          -- the serialized IngestResponse
    PRIMARY KEY (sender_id, consumer_id, author, turn_hash)
);

CREATE INDEX idx_ingest_replies_age ON ingest_replies(created_at);
