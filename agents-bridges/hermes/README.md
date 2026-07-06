# hermes bridge — mwe-mcp on hermes-agent, zero fork

This bridge wires [hermes-agent](https://github.com/nousresearch/hermes-agent)
to a running mwe-mcp server at full fidelity — the per-turn contract (v1) of
[`INTEGRATING.md`](../../INTEGRATING.md) — as a **plugin trio**, with no fork
and no upstream patch:

- **Memory half** — `plugins/memory/mwe/`, a `MemoryProvider`:
  - `prefetch()` is the one mechanical `wiki_ingest_message` per turn,
    run **synchronously** (the ratified trade-off: better memory over
    first-token latency). Its return value is the rendered block; hermes
    injects it into the current turn's user message inside
    `<memory-context>` — after the stable prompt prefix, so the provider
    prompt cache survives. `_render_block` leads with the response's
    dedicated **`rules`** field — standing behaviour directives the model
    must *apply* (how to converse / operate with this user), kept apart
    from the recalled memory in `context_snippet` (mwe-mcp roadmap 29d) and
    injected **verbatim** (the field is self-labelled server-side with the
    `YOUR RULES (…)` role header, so the bridge adds no preamble) —
    then the role-labelled recall block, reply material, and any
    disambiguation / pending-attention lines.
  - `sync_turn()` maintains the **consumer-owned recent window** locally
    and threads it through `recent_messages`; there is no server-side
    transcript and no compact to call.
  - Tools: `mwe_search` (explicit lookups), `mwe_dashboard_link`, and
    `mwe_disambig_commit` (the contract's disambiguation follow-up),
    proxied with per-sender act-as through the provider's own client pool.
  - `on_memory_write()` **one-way-mirrors** the built-in
    `MEMORY.md`/`USER.md` writes: `target='user'` is ingested act-as the
    human (their memory wiki), `target='memory'` as the bot itself. The
    self-improvement organs (background review, curator, skills) stay
    local — mechanism, not knowledge.
  - Non-primary agent contexts (`subagent`/`cron`/`flush`) leave the
    provider **fully inactive** — prefetch ingests (a write path), so
    background loops get no memory access rather than polluted capture.
- **Context half** — `plugins/context_engine/mwe-truncate/`, a
  `ContextEngine` whose `compress()` **truncates** to a bounded window
  (protected head + last N messages, tool-call pairing kept intact) with
  **no summarization pass**: in the sessionless model, per-turn recall
  replaces the summary.
- **Media half** — `plugins/gateway/mwe-media/`, a gateway hook plugin on
  hermes's `pre_gateway_dispatch` seam: incoming Telegram media (already
  downloaded by hermes into its local cache) is uploaded out of band to
  the server's `POST /media` endpoint and the minted catalog ids ride the
  next per-turn ingest as `attachments`. **Opt-in** — see §Media below;
  without it the bridge captures captions but drops the media itself.

All three are stdlib-only — no pip dependencies.

## Install

**Easiest — the served installer.** A running mwe-mcp serves a
self-contained installer for this bridge: no repo clone, no manual
symlinks. Open `/bridges/hermes` on your mwe-mcp for the command tailored
to that server's address, then run it **from inside your hermes-agent
checkout** (the context-engine plugin must land there):

```bash
curl -fsSL https://<your-mwe-mcp>/bridges/hermes/install.sh | sh
# Windows (PowerShell):  irm https://<your-mwe-mcp>/bridges/hermes/install.ps1 | iex
```

It places all three plugins — `mwe` + `mwe-media` under
`~/.hermes/plugins/`, `mwe-truncate` inside your checkout's
`plugins/context_engine/` — and prints the three steps that stay yours:
mint a token, set `memory_enabled: false` **and** `user_profile_enabled:
false`, restart. Override the runtime
dir with `HERMES_HOME` and the checkout with `HERMES_SRC`. You can also
hand it to a running hermes — paste it `Read
https://<your-mwe-mcp>/bridges/hermes/install.md and follow it` and it
installs its own bridge, then tells you those same three steps (it never
handles your token).

**From a source checkout (development).** With the mwe-mcp repo checked
out, symlink the plugins directly so `git pull` keeps them current — the
layout the installer reproduces. The memory provider and the media hook
load **out-of-tree** from `$HERMES_HOME/plugins/`; hermes's
context-engine discovery has no user directory, so the engine is dropped
into the checkout's plugin directory (a directory add, not a fork):

```bash
BRIDGE=/path/to/mwe-mcp/agents-bridges/hermes
HERMES=/path/to/hermes-agent

mkdir -p ~/.hermes/plugins
ln -s "$BRIDGE/plugins/memory/mwe"                    ~/.hermes/plugins/mwe
ln -s "$BRIDGE/plugins/gateway/mwe-media"             ~/.hermes/plugins/mwe-media
ln -s "$BRIDGE/plugins/context_engine/mwe-truncate"   "$HERMES/plugins/context_engine/mwe-truncate"
```

Keep `mwe` and `mwe-media` as **sibling directories** under
`~/.hermes/plugins/` (the layout above): the media hook reaches the
vendored HTTP client through its `mwe` sibling.

## Configure

**Recommended: run `hermes memory setup` and pick `mwe`** — it walks the
schema and writes `mwe.json` plus the `config.yaml` memory entries for you.
The manual equivalent, if you'd rather edit the files directly:

1. `$HERMES_HOME/mwe.json`:

   ```json
   {
     "url": "http://127.0.0.1:8742/mcp",
     "primaryUser": "anna",
     "senderMap": { "telegram:123456": "anna" },
     "locale": "it-IT",
     "maxWindow": 16
   }
   ```

   `primaryUser` is the mwe user id this deployment's human maps to (the
   act-as identity); `senderMap` optionally routes gateway senders
   (`<platform>:<user_id>` or bare `user_id`) to mwe user ids — anyone
   not mapped falls back to `primaryUser`.

2. The token (minted from the mwe-mcp dashboard for the bridge's **bot
   system-user**, e.g. `sam-hermes`, with its delegation list) goes in the
   environment / hermes's `.env`:

   ```bash
   MWE_TOKEN=eyJ...
   ```

3. `config.yaml`:

   ```yaml
   memory:
     provider: mwe
     memory_enabled: false        # built-in MEMORY.md (bot's own memory) OFF
     user_profile_enabled: false  # built-in USER.md (user profile) OFF — a SEPARATE flag, also defaults true (the replace decision, see below)
   context:
     engine: mwe-truncate
     mwe-truncate:          # optional knobs, defaults shown
       threshold_percent: 0.75
       protect_first_n: 3
       protect_last_n: 16
   plugins:
     enabled:
       - mwe-media          # REQUIRED for media capture — see §Media
   ```

   ⚠️ **The `plugins.enabled` line is not optional if you want media.**
   hermes loads user-installed standalone plugins only from this
   allow-list; without it the `mwe-media` hook is silently skipped and
   Telegram photos/voice/video never reach the memory — only their
   captions survive (via the ordinary text ingest).

## Connecting Telegram (the hermes gateway)

hermes ships a Telegram gateway (long polling — works from a LAN with no
public IP). With the bridge configured, every Telegram sender is routed
through `senderMap` to their mwe identity:

1. In `$HERMES_HOME/.env`:

   ```bash
   TELEGRAM_BOT_TOKEN=123456789:AA...   # from @BotFather
   TELEGRAM_ALLOWED_USERS=111111,222222 # numeric Telegram user ids (fail-closed allowlist)
   ```

2. In `config.yaml`:

   ```yaml
   platforms:
     telegram:
       enabled: true
   ```

3. In `mwe.json`, map each allowed sender (`<platform>:<numeric user id>`):

   ```json
   "senderMap": { "telegram:111111": "anna", "telegram:222222": "bruno" }
   ```

   Every mapped human must be enrolled on the server and listed in the
   bot token's act-as delegations. Unmapped senders fall back to
   `primaryUser`.

   For a sender you *cannot* attribute to a real person (a shared
   device, an unrecognized voice upstream), map it to the builtin
   **`guest`** pseudo-identity instead of letting it fall back to
   `primaryUser` — e.g. `"voice:unknown": "guest"` — and tick `guest`
   in the consumer's delegation roster on the dashboard. Guest turns
   are ephemeral server-side: recall is public memory only, nothing is
   stored, and the ingest `rules` field instructs the agent to behave
   reservedly (see `INTEGRATING.md`, per-turn contract point 9).

4. `hermes gateway` (foreground) or `hermes gateway start` (service).

Caveats: **one poller per bot token** — if the token belonged to a
previous bot, stop that process first; the gateway overwrites the bot's
Telegram command menu; a user the bot has never spoken with must message
it once before the bot can initiate (Telegram rule + the gateway's
channel directory learns the chat then).

## Media (photos, voice, video, documents)

With the `mwe-media` hook enabled, media sent to the bot becomes part of
the memory (the server-side pipeline:
the [media-pipeline design note](docs/design-notes/media-pipeline.md)).
Entry is two-phase: when a media message arrives, the hook uploads the
bytes hermes already downloaded into its cache to `POST <origin>/media`
(multipart, the same bearer token + act-as as the MCP calls) and spools
the minted catalog ids; the memory provider drains the spool on the next
`prefetch()` and attaches them to the per-turn `wiki_ingest_message` as
`attachments: [{catalog_id, kind, caption?}]`. The server stores the
bytes content-addressed (the same photo twice is one copy) and turns the
attachment into a described, recallable fact.

- **What gets uploaded** — the hermes message type gates the pipeline
  (photo → `photo`, video → `video`, audio and voice notes → `audio`,
  document → `doc`; stickers and other types are not uploaded), but each
  file's kind comes from its own mime (`image/*` → `photo`, `video/*` →
  `video`, `audio/*` → `audio`, message type as the fallback): a mixed
  photo+video album arrives as ONE event whose message type reflects
  only the first item, so per-file mime keeps every photo on the
  server's vision path. The Telegram caption travels both as the
  upload's `caption` field and on the attachment.
- **Fail-closed senders.** The hook fires *before* hermes's own
  authorization, so it uploads **only for senders with an explicit
  `senderMap` entry** in `mwe.json` (`telegram:<id>` or bare id). The
  `primaryUser` fallback that routes unmapped senders' *text* does NOT
  apply to media: an unmapped sender's bytes never leave the machine.
- **Size cap.** Files larger than the per-file cap are skipped before
  their bytes are read (default 20 MiB — Telegram's own bot-download
  ceiling; the server rejects >32 MiB).
- **The spool file.** `$HERMES_HOME/mwe-media-spool.json` is the channel
  between the hook and the provider (different plugin namespaces — the
  file is the seam). The hook appends on the gateway event-loop thread
  while the provider drains on an agent worker thread, so both sides
  hold an exclusive `flock` on the sidecar `mwe-media-spool.json.lock`
  across their whole read-modify-write — no entry is lost or
  resurrected between them. Entries expire after ~180 s: a turn that
  never fired cannot leak its attachments into an unrelated later turn.
  The file is transient state; deleting it (and the lock sidecar) costs
  at most the not-yet-drained uploads' link to their turn.
- **Native image mode.** On photo turns hermes hands the model the image
  natively and the provider receives an **empty query** — a host
  limitation that means the recall block is not injected on that turn.
  The ingest still captures: the provider detects the spooled
  attachments and fires the ingest with the caption as text (fallback
  `[media]`), so the photo becomes memory either way.
- **Degradation, as everywhere in the bridge:** any failure (server
  down, upload error, spool unwritable) logs a warning and the turn
  proceeds without media.

Optional `mwe.json` knobs (defaults shown):

```json
{
  "mediaEnabled": true,
  "mediaMaxBytes": 20971520,
  "mediaUploadBudgetSeconds": 60
}
```

`mediaEnabled: false` is the kill-switch (the hook stays loaded but
uploads nothing); `mediaMaxBytes` is the per-file size cap in bytes;
`mediaUploadBudgetSeconds` is the **aggregate per-event** upload budget —
each file is already bounded by the upload timeout, but a multi-item
album against an unresponsive server would otherwise stall the gateway
for files × timeout, so once the budget is spent the remaining files are
skipped (one warning, the turn proceeds).

## Smokes

- **Offline** (CI + canary, no server, no LLM): `./smoke.sh` — fetches
  hermes-agent at `BRIDGE_UPSTREAM_REF` (default: the manifest pin; set
  `HERMES_SRC=/path/to/checkout` to clone locally), installs the trio into
  a scratch checkout, and asserts the contract mechanics (including the
  media hook → spool → attachments path) through hermes's real plugin
  seams against the recording stub.
- **Live** (operator-run, real server, costs LLM calls):
  `HERMES_SRC=… MWE_MCP_URL=… MWE_TOKEN=… MWE_PRIMARY_USER=… python3 smoke_live.py`
  — a short scripted conversation; read the printed recall blocks to judge
  quality.

## Design choices

- **Replace, not mirror** (decided 2026-06-11, on first-live-session
  evidence): hermes's built-in memory stays **off**. This takes **two**
  switches, both of which hermes ships **on** by default: `memory_enabled:
  false` gates the bot's own `MEMORY.md` (and the foreground memory tool),
  and `user_profile_enabled: false` gates the `USER.md` user profile — a
  **separate** flag. `memory_enabled: false` **alone leaves `USER.md`
  live**: the self-improvement review writes `USER.md` directly (not
  through the memory tool, so the mirror below never catches it), so you
  must set **both** false. Turning them off is a deliberate override you
  set explicitly, not a no-op — the built-in runs *alongside* any external
  provider (its `hermes memory` CLI even reports the built-in as "always
  active"). The `mwe` provider is selected independently by
  `memory.provider` and keeps running, so disabling the built-in never
  disables the bridge itself. We
  override it because the built-in is a second, ungoverned truth channel:
  no validity windows, no REM, no per-reader redaction — and `USER.md` is
  one global file injected into **every** sender's prompt, which breaks
  per-sender governance in multi-user deployments. Nothing is lost: capture is mechanical via the per-turn
  ingest, so the model needs no save tool. The `on_memory_write` one-way
  mirror remains implemented for deployments that deliberately keep the
  built-in memory on — its writes land in the memory wikis (`user` target
  act-as the human, `memory` target as the bot) — but the recommended
  configuration is the one above. Note: hermes's self-improvement review
  writes its files directly (not through the memory tool), so the mirror
  never sees those even when active.
- **Non-primary = fully inactive** is deliberately conservative; a
  read-only recall path for subagents would need a non-capturing ingest.
