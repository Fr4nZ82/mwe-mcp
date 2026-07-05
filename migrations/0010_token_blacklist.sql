-- 0010_token_blacklist — JWT revocation list
--
-- A revoked token's `jti` is inserted here at `mwe-mcp token-revoke`
-- time. The server caches the active blacklist in memory with a 60s
-- TTL so the hot-path verify call does not hit SQLite. Expired tokens
-- (`expires_at < now`) can be purged by a periodic GC job.

CREATE TABLE token_blacklist (
    jti        TEXT PRIMARY KEY,                                   -- JWT id (UUID generated at issuance)
    revoked_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,                                      -- original token `exp`; used for GC
    reason     TEXT                                                -- free text or known code
);

CREATE INDEX idx_blacklist_expires ON token_blacklist(expires_at);
