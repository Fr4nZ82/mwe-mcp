# Tokens

**Nav: Tokens.** `/dashboard/tokens`

A **consumer** — the bot or assistant that talks to this memory — proves who
it is with a token. This page mints them, records who each consumer may speak
for, and revokes them.

## Issue a token

**Consumer class** is the first choice, and it changes the rest of the form:

- **Smart** — one human owner, brings its own LLM (Claude Code). It unlocks
  the `wiki_admin_*` tools, which is how such a consumer authors a project's
  own wiki. Pick the **Owner (user)**, and give a **Device id** (a free label
  for the machine or session, e.g. `cc-laptop`).
- **Standard** — one consumer serving several people, acting as each of them
  (a chat bot, mail, home automation). Instead of an owner it takes a
  **Consumer id**: lowercase letters and digits, starting with a letter, no
  hyphen. Issuing creates a credential-less identity with that id **and its
  own wiki** — that is how a consumer comes to exist here.

**Device label** is what shows up in the audit log so you can tell sessions
apart; it defaults to the id above.

**Acts as (allowed senders)** is the list of people this consumer serves —
tick each of them. A standard consumer needs at least one, or the form refuses.
Ticking **guest** lets it answer humans it cannot identify: a guest turn
recalls only public memory and stores nothing.

**TTL profile** is the token's lifetime:

- **internal — 1 year**, for a device on your own machine you trust.
- **exposed — 30 days**, for anything reachable from the public internet.

There is no self-service refresh: when an exposed token lapses, you issue a new
one here.

**Rate limit id** names the ceiling set the token is held to, `default` unless
you wrote another profile in `mwe-mcp.config.yaml`. The ceilings apply whether
or not you wrote any: out of the box a token may make **120 calls a minute and
3 000 an hour**, of which **30 a minute and 600 an hour** may spend a model or
the embedder. The dashboard's own sessions get five times the first pair and
four times the second, because a limit must never stop the screen you would use
to see what is happening.

Press **Issue token**. The receipt shows the claims and the token itself, with
the warning that matters: **copy it now, it is not stored server-side.** Only
revocations are kept here — the token's contents are recoverable from nowhere
but the holder.

## Consumer delegations

Every standard consumer you issue appears in this table with **Consumer id**,
**Allowed senders**, when and by whom it was granted, and **edit** to change
the list of people it may speak for.

## Consumers connected over OAuth

A consumer that signed in over OAuth instead of taking a token — the claude.ai
web app, Claude Code — lands in this table when somebody approves it: **User**,
**Connection**, **Wiki**, **Since**, and a **Disconnect** button. Disconnecting
stops the connection from renewing, and any live session ends within an hour.
The dedicated wiki it was given is kept.

## Revoke

**Revoke a token** takes the **JWT id (jti)** from the receipt or the audit log
and a free-text **Reason**. A revoked token stops working within a minute:
every call after that fails, and there is nothing to retry. Revoked tokens are
listed above the form.
