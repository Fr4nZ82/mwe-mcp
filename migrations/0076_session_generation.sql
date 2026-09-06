-- 0076_session_generation — one number per user that ends every session
-- they have open, everywhere.
--
-- Signing out could only end the session doing the signing out: the
-- dashboard cookie is a stateless JWT re-minted with a fresh `jti` on
-- every request, so there is no list of a person's live sessions to walk
-- and revoke. A phone left on a train, a browser on a shared machine, a
-- cookie copied out of a laptop — all of them kept working until they
-- expired on their own.
--
-- THE MECHANISM. Every session JWT carries the generation its user was on
-- when it was minted (`session_gen`). Verification compares that number
-- with the row here: lower means the session predates a "sign out
-- everywhere" and is refused. Ending every session is therefore one
-- UPDATE, and it takes effect on the next request of every device — no
-- blacklist row per session, and nothing to clean up afterwards.
--
-- WHY A COUNTER AND NOT A TIMESTAMP. A timestamp compared against the
-- token's `iat` needs no new claim, and it was the first design. It has a
-- one-second hole: `iat` has second resolution, so a session re-minted
-- inside the same second as the sign-out cannot be told from one minted
-- after it, and whichever way the comparison is written either that
-- session survives for ever or a fresh sign-in in the same second is
-- rejected. A counter has no such moment.
--
-- ABSENT ROW = GENERATION 0. A user who has never signed out everywhere
-- has no row, and a JWT minted without the claim reads as 0 — so the
-- upgrade does not invalidate the sessions open while it happens, and the
-- first sign-out-everywhere writes the row that starts the sequence.
--
-- WHAT IT DOES NOT COVER. MCP bearer tokens are the consumers'
-- credentials, not the person's sessions: they are revoked from the
-- dashboard's Tokens page, one by one, and no generation is consulted for
-- them.

CREATE TABLE user_session_generation (
    user_id    TEXT PRIMARY KEY REFERENCES enrollment_users(user_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
