-- 0044_webagentoauth — inbound OAuth 2.x authorization server (roadmap 19,
-- `webagentoauth`): lets a bridge-less MCP client (the claude.ai web app, or any
-- spec-compliant custom connector) connect as the user's own Smart consumer via
-- the standard OAuth dance, with no token minted by hand. See
-- ../wiki/planning/19_webagentoauth.md.
--
-- Three tables. The access token itself needs none — it is an ordinary smart JWT
-- (jwt::issue) validated by the existing /mcp middleware; only the OAuth-protocol
-- artefacts that have no home yet live here:
--   * webagentoauth_clients  — dynamically-registered (RFC 7591) public PKCE
--                              clients; `client_name` doubles as the default
--                              consumer label suggested at consent.
--   * webagentoauth_codes    — single-use authorization codes (PKCE S256). The
--                              row is deleted on redemption, so the PRIMARY KEY
--                              is the serialization point that makes a code
--                              single-use even under a concurrent double-redeem.
--   * webagentoauth_refresh  — rotating refresh tokens (`revoked_at IS NULL` =
--                              active). Stored as a SHA-256 hash, never in clear.
--
-- `is_admin` is intentionally NOT stored: it is re-resolved from
-- `enrollment_users` every time a JWT is minted (code exchange or refresh), so a
-- role change takes effect on the next token instead of being frozen at consent.

CREATE TABLE webagentoauth_clients (
    client_id      TEXT PRIMARY KEY,   -- server-generated opaque id
    client_name    TEXT NOT NULL,      -- from DCR; default consumer label
    redirect_uris  TEXT NOT NULL,      -- JSON array of allowed redirect URIs
    created_at     TEXT NOT NULL       -- ISO-8601
);

CREATE TABLE webagentoauth_codes (
    code_hash       TEXT PRIMARY KEY,  -- SHA-256(authorization code), hex
    client_id       TEXT NOT NULL,
    redirect_uri    TEXT NOT NULL,     -- must echo the /authorize redirect_uri
    code_challenge  TEXT NOT NULL,     -- PKCE S256 challenge (base64url, no pad)
    sender_id       TEXT NOT NULL,     -- the approving human user
    consumer_id     TEXT NOT NULL,     -- the named connection (device label)
    wiki_id         TEXT NOT NULL,     -- the dedicated smart wiki it binds to
    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL      -- short-lived (minutes)
);

CREATE TABLE webagentoauth_refresh (
    token_hash    TEXT PRIMARY KEY,    -- SHA-256(refresh token), hex
    client_id     TEXT NOT NULL,
    sender_id     TEXT NOT NULL,
    consumer_id   TEXT NOT NULL,
    wiki_id       TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    revoked_at    TEXT                 -- set on rotation / revoke; NULL = active
);

CREATE INDEX idx_webagentoauth_refresh_owner
    ON webagentoauth_refresh (sender_id, consumer_id);
