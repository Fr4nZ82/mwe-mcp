# Integrating mwe-mcp — the developer & operator guide

This guide is for **developers wiring a host agent framework to mwe-mcp** and for
**operators deciding where to run the consumer**. It assumes a server is already
up — if not, start with **[`INSTALL.md`](INSTALL.md)** (download-and-run, three
steps).

**If you just want to connect a ready-made consumer, the running server walks you
through it — you may not need this guide.** Open the **Bridges** tab in the
dashboard (or `/bridges` unauthenticated) for the copy-paste setup per supported
host; the public front page at `/` points a capable agent straight at a
machine-readable `install.md` it can run itself. Three hosts are covered
point-and-click today: **NanoClaw** (the ready-made assistant — start here if
you have no agent of your own), **Claude Code** (smart consumer, one command +
OAuth) and **Hermes** (Nous Research — a per-turn plugin bridge for a Hermes you
already run).

This guide is what's left once that path doesn't fit: the **per-turn contract** to
write a bridge for a host we don't ship, and the **deployment-security rules**
for where the consumer runs. It is **not** the runtime spec for the consumer agent itself — an
LLM agent that *talks to* mwe-mcp over MCP reads
[`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) instead.

> **What is solid vs. what is still moving.** The **per-turn contract** a host
> bridge implements (below) is concrete and true of the shipped release, and the
> three hosts in the `/bridges` catalog have working copy-paste setup. The remaining
> consumer-side detail — an end-to-end worked integration for a host we don't
> ship, the identity/delegation handshake from the consumer's point of view — is
> still being hardened against real consumers. The authoritative answer on any
> topic is the running server: `tools/list` for the surface, `mwe-mcp doctor`
> for what the deployment resolved.

---

## The shape of an integration

mwe-mcp is a standalone server. There is no SDK to embed and no library to link —
the contract is the **MCP tool surface over HTTP**. Once a server is running and
you hold a bearer token minted from its dashboard, an integration is two pieces:

- **Point your MCP client at `/mcp`** with the token in an
  `Authorization: Bearer …` header and call the tools. The per-turn entrypoint is
  `wiki_ingest_message`.
- **Pick or write a host bridge** — the thin adapter in your stack that implements
  the [per-turn contract](#the-per-turn-contract-v1--wiring-a-host-bridge) below.
  Ready-made **per-turn** bridges live under
  [`agents-bridges/`](agents-bridges/README.md): **[NanoClaw and
  Hermes](#the-ready-made-bridges)**. A *smart* consumer like
  Claude Code needs no per-turn bridge at all — it connects natively over OAuth.

**Prerequisites — all set up in [`INSTALL.md`](INSTALL.md):**

1. a **running server** (`mwe-mcp serve`) exposing `/mcp` and `/dashboard/*`;
2. the **internal LLM wired** from the dashboard (Anthropic / Gemini / a local
   Ollama workhorse / a mix — embeddings always run locally);
3. a **bearer token** minted for your agent (Admin → users / tokens). A consumer's
   class (`standard` vs. `smart`) and its identity claims are decided at mint time.

The rest of this guide is the per-turn contract a bridge implements, plus where to
run the consumer safely.

---

## stdio-only clients — bridge to the HTTP endpoint

mwe-mcp is **HTTP-only by design**: the model is one long-running, multi-user
server with per-consumer auth and governance — a per-client stdio child process
cannot be shared between consumers, authenticated per token, or reached from
another machine. Clients that only speak stdio still connect fine through a
local stdio→HTTP proxy such as
[`mcp-remote`](https://github.com/geelen/mcp-remote):

```json
{
  "mcpServers": {
    "mwe-mcp": {
      "command": "npx",
      "args": [
        "-y", "mcp-remote",
        "https://your-server:8742/mcp",
        "--header", "Authorization:${MWE_AUTH}"
      ],
      "env": { "MWE_AUTH": "Bearer <your token>" }
    }
  }
}
```

(The header rides an env var because some clients split `args` entries on
spaces.) The proxy runs on the client's machine and is just a pipe — the
security topology below is unchanged, the memory stays behind the HTTP
boundary. Clients that speak streamable HTTP natively (Claude Code, claude.ai,
and most current SDKs) skip the shim entirely:
`claude mcp add --transport http mwe-mcp https://your-server:8742/mcp --scope user`
(then sign in over OAuth — see [below](#the-ready-made-bridges)).

---

## The ready-made bridges

Two **per-turn** (standard-consumer) host bridges ship here, both wiring their
host to a running mwe-mcp at full fidelity — the per-turn contract below. You
don't wire either by hand: the server serves the installer, one command, and the
token stays a dashboard step the installer never handles.

- **[NanoClaw](https://github.com/nanocoai/nanoclaw)** — the **ready-made
  assistant**: the consumer to reach for when you have no agent of your own. The
  installer at **`/bridges/nanoclaw`** places the `mwe` agent template and the
  `add-mwe-memory` fork skill, cloning NanoClaw at the tested ref if you do not
  have it. The complete step-by-step — the template, the skill, `mwe.json`,
  `senderMap`, the reverse channel, media — is in
  **[`agents-bridges/nanoclaw/README.md`](agents-bridges/nanoclaw/README.md)**.
- **[Hermes](https://github.com/NousResearch/hermes-agent)** (Nous Research) —
  for a Hermes you already run: a plugin quartet with **no fork and no upstream
  patch**, served at **`/bridges/hermes`**. The step-by-step — plugins,
  `mwe.json`, the bot token, `config.yaml`, the Telegram gateway, media
  capture — is in
  **[`agents-bridges/hermes/README.md`](agents-bridges/hermes/README.md)**.

> **One operational rule worth repeating up front, and it binds both:** the
> host's built-in memory goes **off**, so mwe-mcp is the only memory. In Hermes
> that is two separate flags, both default on — `memory_enabled: false` (which
> gates `MEMORY.md`) **and** `user_profile_enabled: false` (which gates
> `USER.md`). In NanoClaw it is the `mwe` plugin the template stamps into a
> group: a group carrying it creates no `memory/` tree, injects nothing at
> session start, and carries no session between turns (the bridge README's
> *The switches that turn nanoclaw's own memory off* names each one).
> A second, ungoverned store accumulates stale duplicates, skips per-reader
> redaction, and (when injected globally) leaks one user's facts into another's
> prompts. Capture needs no "save" tool — the per-turn ingest *is* the capture
> path. The reasoning is in each bridge README's *Design choices*.

If your host is neither, implement the
[per-turn contract](#the-per-turn-contract-v1--wiring-a-host-bridge) below
directly; the bridge-authoring guide is
[`agents-bridges/README.md`](agents-bridges/README.md).

A **smart** consumer (it brings its own LLM and is a native MCP client) connects
differently — no per-turn plugin, no server-side LLM. **Claude Code** is served
point-and-click at **`/bridges/claude-code`** (human guide + agent `install.md`):
it registers mwe-mcp over the **`webagentoauth` OAuth flow** at user scope
(`claude mcp add --transport http mwe-mcp <origin>/mcp --scope user`, then sign in
via `/mcp` or `claude mcp login mwe-mcp`) — **no token to mint or paste** — and
authors its own wikis over `wiki_admin_*`. The loopback OAuth redirect gives it the
**`Local`** profile (the full tool catalog, including `skill_fetch`). Recall is
**model-driven** (no per-prompt hook); the only optional hook is a token-less
`SessionStart` nudge. The agent **never bootstraps a repo automatically**.

---

## Deployment security — where to run the consumer

Per-reader redaction is enforced when the server renders a response; the
markdown under the workdir is cleartext on disk. An agent framework
running **on the same host as the workdir** with shell or file tools
(most ship them) can read the raw memory wikis and bypass the governance
entirely — we observed exactly this on the first live bridge deployment.
In order of strength:

1. **Separate machines** — the consumer reaches `/mcp` over HTTP only.
   This is the recommended production topology. (Note: stdio transport is
   inherently same-host, same-principal and cannot provide this — only
   remote HTTP can.)
2. **Same machine, separate OS users** — run the agent as a user with no
   access to the workdir, owned by the mwe-mcp user and `chmod 700` (use
   `750` only if you deliberately put the agent in the workdir's group).
3. **Same user (dev/test only)** — restrict the agent's toolsets on
   end-user channels if the framework supports it; treat this as a
   mitigation, not a boundary.

How dangerous co-location actually is depends on **who owns the data on
the box**: the bypass only matters when the workdir holds fragments
belonging to a principal who *also* has OS access (a shared multi-user
wiki). A single-principal box — one agent serving one human, running as
them — re-exposes only data that human already owns. The rule that falls
out: never co-locate the workdir on a machine where a principal whose
data the ACL governs also has shell access.

The server flags a loose workdir for you rather than failing silently:
`mwe-mcp serve` warns at boot, and `mwe-mcp doctor` reports every workdir
path reachable by group or world with a `chmod` fix. It is advisory (the
server still starts). Encryption-at-rest is *not* a substitute — a
co-located process running as the same user can reach the key.

The **same-user** case (option 3) is the one `chmod 700` cannot fix — a
process running as *you* reads the bytes regardless. So `serve` goes
beyond advisory there: it **refuses to boot as a login account or root**,
and on an interactive terminal **offers to provision the dedicated-user
systemd service** (creates the `mwe-mcp` account, relocates and locks the
workdir, installs + starts `mwe-mcp.service`) — the one-prompt path to
option 2. The full walkthrough is in
[`INSTALL.md` §"Start the server"](INSTALL.md#2-start-the-server);
`--bypassdedicateduser` is the explicit opt-out for hosts where a
dedicated account is impossible (containers, some managed servers).

Two more operational rules learned the same way:

- **If the host framework has its own built-in memory, disable it** and
  let mwe-mcp be the only memory: a second, ungoverned store accumulates
  stale duplicates, skips per-reader redaction, and (when injected
  globally) leaks one user's facts into another user's prompts. Capture
  needs no "save" tool — the per-turn ingest is the capture path.
- **One poller per chat-bot token**: when reusing an existing bot token
  (e.g. a Telegram bot), stop the previous process that polled it first.

---

## Call ceilings — what `429 rate_limited` means and what to do with it

Every token is held to a ceiling on how many calls it may make, counted per
token. The built-in numbers are 120 calls a minute and 3 000 an hour, of which
30 a minute and 600 an hour may be the calls that put a model or an embedding
to work: `wiki_ingest_message`, `wiki_ingest_external`, `wiki_navigate`,
`wiki_search`, `recall_core_global`. The operator can give a consumer its own
numbers by minting its token with a `rate_limit_id` and declaring that name in
`rate_limits:` (see [`INSTALL.md`](INSTALL.md#hardening-checklist)).

A call past the ceiling comes back as an error whose `data.error_class` is
`rate_limited` and whose `data.retry_after` is the number of **seconds** after
which the same call is worth making again:

```json
{ "error_class": "rate_limited", "retry_after": 34 }
```

What a bridge should do with it: wait that long and retry the call once, and
if it is a per-turn call, tell the person the memory is busy rather than
answering as if nothing happened — a turn whose ingest was refused stored
nothing. Do not treat it as a dead token: nothing is wrong with the
credential, and retrying immediately only spends the next window as well.

---

## Per-project isolation (smart consumers)

A smart consumer like **Claude Code** registers mwe-mcp **globally** (the MCP
server at `--scope user`, connected over OAuth), so transversal personal recall is
reachable in **every** session on the machine — that is the point: personal memory
everywhere. Two switches scope it back down when a repo must stay out of your
personal memory (a client's NDA codebase, a work monorepo):

- **Opt one project out entirely.** Add a per-project MCP override in that repo's
  `.claude/settings.json`:

  ```json
  { "mcpServers": { "mwe-mcp": null } }
  ```

  Project settings win over the global file, so in that repo the `mwe-mcp` server
  does not resolve: **no recall, no bootstrap, nothing leaves the repo**. The
  `core-globalmemory` skill honours the same `null` override — when it is in
  effect, neither transversal recall nor a companion bootstrap runs, and the
  consumer works isolated without calling any `wiki_*` tool.

- **Point a repo at a different governed server.** A work repo backed by its
  employer's own mwe-mcp instead registers *that* server's origin in the repo's
  `.mcp.json` / project settings (e.g. `https://mwe-mcp.acme.internal/mcp`),
  keeping work memory on the work server and off the personal one. The two
  registrations coexist: the global personal server for everything else, the
  per-project work server inside that checkout.

This is the privacy control a work/enterprise user reaches for first; it is also
surfaced on the `/bridges/claude-code` page and in its `install.md` so the agent
proposes it rather than leaking a sensitive repo into personal memory.

---

## Onboarding an existing project wiki (smart consumers)

When a smart consumer (Claude Code) connects in a repo that **already has a wiki**
(a markdown tree of notes, decisions and runbooks), it proposes onboarding it **on
connect** and copies it up to the server as the project wiki — so the same
knowledge is reachable later from a standard consumer or the dashboard.

The copy is **one uniform step, any size**, and it does **not** run the pages
through the model. The agent writes and runs a small script (PowerShell
`Invoke-RestMethod` on Windows, `bash` + `curl` elsewhere) that walks the tree and
calls `wiki_admin_push` over `POST <server>/mcp` — the file bytes go **straight to
the server, never through the model context** (no token burn, no file-read
ceiling). `mwe-mcp` itself is **never installed on the agent's machine**; the
script only speaks HTTP to the remote server.

The one setup step is a **smart Bearer JWT** the script authenticates with:

- If your host exposes the connection's OAuth `access_token` to a shell, the
  script reuses that (short-lived, ~1 h — enough for a one-shot copy).
- Otherwise mint one once and make it available on the agent's machine (e.g. an
  `MWE_JWT` env var, alongside `MWE_SERVER` = your server origin):

  ```bash
  mwe-mcp token-issue --sender <user-id> --device onboarding-import --class smart
  ```

  (or the dashboard token page). The agent derives the `project_id` at bootstrap;
  after the copy it records `.mwe/state.json` and switches to the day-to-day loop
  (single-page pushes), with the existing wiki dir staying the local mirror.

A large `log.md`/`CHANGELOG.md` is copied **whole** (byte-exact); the agent may
later curate it (date-structuring + rotation into dated archives) as a follow-up —
it is never dropped. See the `smart-consumer` skill, "Onboarding an existing wiki".

---

## Where the rest of the detail lives

| Topic | Where the detail lives |
|---|---|
| Standing the server up, configuring its LLM, minting tokens | [`INSTALL.md`](INSTALL.md) |
| Transport (MCP Streamable HTTP), endpoints, JWT bearer | [`INSTALL.md`](INSTALL.md) §2 and §Hardening; the `WWW-Authenticate` challenge on a 401 names the OAuth discovery document |
| Token / identity flow (admin invites → user → consumer) | [`INSTALL.md`](INSTALL.md) §3 and the dashboard's Users / Tokens consoles |
| The tool surface and per-tool I/O contract | `tools/list` on the running server (every tool carries its full schema); families in [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) §6 |
| Server config + LLM profiles + secrets | the commented `mwe-mcp.config.yaml` that `mwe-mcp init` seeds, and the dashboard's Admin panels |
| Deployment topology (server and consumer on separate hosts, remote HTTP) | this document, [Deployment security](#deployment-security--where-to-run-the-consumer) |
| Consumer-agent runtime behaviour (what *your agent* must do) | [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) |
| Ready-made host bridges + the bridge-authoring guide | [`agents-bridges/README.md`](agents-bridges/README.md) |
| Smart vs. standard consumers, smart wikis | [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) §6–§8 |

---

## The per-turn contract (v1) — wiring a host bridge

A host bridge is a thin adapter in **your** stack (a prompt-builder hook, a
memory-provider plugin — whatever your agent framework calls it). There is
no mwe-mcp code to embed; the bridge implements this contract.

This contract is **versioned** — this is **v1**. Ready-made bridges for
specific host frameworks ship in this repo under
[`agents-bridges/`](agents-bridges/README.md); each declares, in its compat
manifest, the upstream version it is tested against and the contract
version it implements. If no bridge fits your host, implement the contract
below directly.

1. **One ingest call per conversational turn.** Call
   `wiki_ingest_message` with the user's text, the **recent window**, and
   (when you have it) `metadata.locale`. When you replay a backlog or
   import history, also set `metadata.occurred_at` (ISO-8601) so relative
   dates and validity windows resolve against the utterance time instead
   of the server clock. The ingest response is the
   per-turn recall source — its `context_snippet` carries the full
   **recall block** of recalled MEMORY as role-labelled sections in a
   canonical order: `WHO YOU ARE` (the agent's own identity), `WHO IS
   SPEAKING` (the sender's one-line card), `YOUR RECENT HISTORY WITH THIS
   USER`, `RELEVANT MEMORY` (the flat hits), `NAVIGATED PAGES` (the
   navigated prose), and `UPCOMING` (imminent commitments). A separate
   first-level **`rules`** field
   carries standing **behaviour directives** (how this agent should converse
   / operate with the served user — the agent's own rules plus any rule the
   user set for *every* assistant they talk to) plus
   any one-shot governance notice — kept structurally apart from the memory so
   a binding rule is never mistaken for a remembered fact. **Inject `rules`
   too, verbatim and adjacent to the block** — the field is self-labelled
   (`YOUR RULES (…)`, apply-don't-relay wording included), so add no preamble
   of your own; privacy/sharing is *not* here — it is enforced memory-side
   by the ACL, so the agent simply never recalls what it may not see.

   Two further **governance blocks** ride the same response and are injected
   beside `rules`. Both are **absent, not null**, when they do not apply, and
   neither is an answer to what the user just said — the agent raises them
   briefly, at the end of its reply.
   - **`pending_votes`** — the speaker owes a vote on a request to forget a
     fact they are part of. `count` (how many are waiting), `requests` (each
     with `proposal_id`, `fact_id`, `requester`, an RFC-3339 `deadline` and its
     own `dashboard_path`), a top-level `dashboard_path` (`/dashboard/chat`)
     and `note: "vote_no_to_block_silence_is_consent"`. The vote is cast by
     talking to the dashboard's chat and nowhere else, so prefix the path with
     the operator's base URL and hand the human a link. The block carries **no fact text** — do not let
     an agent invent it. It is pull-only and reappears every turn until the
     member votes; silence past the deadline is consent and the fact is
     forgotten. A guest turn never carries it.
   - **`document_promoted`** — the turn was document-shaped, so the server
     archived it verbatim on the media rail and queued it for document
     ingestion; what the classifier read is a bounded excerpt plus a hand-off
     note. `catalog_id` (the archived blob), `job_id` (the ingestion job) and
     `existing` (the same bytes were already queued). Tell the user their
     document is stored and will be quotable; do not ask them to send it again,
     and do not look for its contents this turn — the reading finishes in the
     background and lands on `events_poll` as `document_ingested`. A guest turn
     never carries it either.

   Do **not** build a separate pre-fetch recall path: the block's
   navigation step reuses the classifier's own routing signals, which a raw
   pre-classification search cannot reproduce. `wiki_search` remains
   available for explicit, user-visible lookups.
2. **You own the transcript.** mwe-mcp keeps no server-side raw-message
   archive; supply the sliding window via `recent_messages` (the server
   reads at most its configured cap, by default the last 16 entries) and
   trim it on your side. There is no server-side "compact" to call.
3. **Inject the recall block after your stable content.** The block
   changes every turn; placing it after the stable system-prompt prefix
   (persona, tools, standing instructions) preserves your LLM provider's
   prompt cache. Everything stable first, then the volatile block, then
   the conversation.
4. **`suggested_seed` is material, not a reply.** Have your agent rewrite
   it in the user's language and tone — never forward it verbatim.
5. **Honour disambiguation.** When `needs_disambig` is true, surface the
   `disambig_candidates` to the user and re-call with
   `metadata.disambig_choice` set to the picked id; the second turn
   commits.
6. **The response is always renderable.** Soft failures (internal LLM
   down, malformed plan) degrade to `intent=skip` with a canned seed —
   your turn never dies on a memory hiccup. `llm_used` tells audit-grade
   truth about which branch fired. The **daily budget** arrives here too:
   an operator who set one and reached it gets `intent=skip` with the same
   canned seed and `llm_used: false`, for every turn until they lift it or
   the UTC day turns over. Nothing on the wire distinguishes it — you
   relay the seed and carry on, and the operator hears about it on
   `events_poll` (`budget_threshold_reached`, below) and on their
   dashboard.
7. **Media travels out of band.** When the user sends a photo (or
   video, voice note, document), upload the bytes first with
   `POST /media` (multipart on the same origin as `/mcp`, same bearer
   JWT and `X-MWE-Act-As` headers; fields `file` + `kind` ∈
   `photo|video|audio|doc`, optional `caption`/`description`), then
   pass the returned `catalog_id` in the same turn's
   `wiki_ingest_message` `attachments` array. The turn must still
   carry non-empty `text` — for a captionless photo send the caption
   or a placeholder like `[media]` (the hermes bridge does exactly
   this). The server describes
   undescribed photos with its own vision pass and files the media as
   a described fact; pass `description` yourself when your host
   already knows what the media shows. Re-uploads of the same bytes
   are idempotent per sender. `GET /media/<catalog_id>` (same auth)
   serves the bytes back, gated by the per-media ACL. A **document the
   user wants read into memory** (a manual, a meeting transcript) does
   not ride the turn: call `wiki_ingest_external` with the `catalog_id`
   (async job receipt; completion lands on `events_poll` as
   `document_ingested`).
8. **Drain the reverse channel — the one obligation not anchored to a
   user turn.** Everything above fires when the *user* speaks; mwe-mcp
   also emits notices when *it* acts and the user should know: a
   structural change it applied (`structure_applied`), a document that
   finished ingest (`document_ingested`), a dated commitment that is due
   (`reminder_due`), and — the one notice addressed to a *different*
   human than the one who spoke — a turn or upload that minted facts
   whose subject is another enrolled user (`fact_minted_for_you`: the
   payload carries the fact bodies, so your agent delivers the content
   itself — "Alice worked out with the assistant what you should check at
   the viewing: …" — not a bare pointer). Four more kinds are addressed
   to the operator rather than a user: `archive_proposed`,
   `compile_failure_streak`, `recall_tuning_proposed`, and
   `budget_threshold_reached` — today's metered spend crossed the
   deployment's daily budget, at the warning line and then at the budget
   itself, at most once per threshold per UTC day. Its payload carries
   `threshold` (`warn` | `stop`), the `day`, `spent`, `limit`, `percent`,
   `currency`, the count of `unpriced_calls` the estimate leaves out, a
   `dashboard_path`, and `stopped` — whether paid model calls are
   actually being refused right now (they are at `stop`, unless the
   operator had already unlocked the day). While that is true, every turn
   comes back as obligation 6 describes.
   Poll them with `events_poll`, hand each to your agent, then
   `events_ack` the ids so the server stops re-delivering. A structural
   payload carries `proposal_id`, `variant`, the `closed_facts`,
   `recipient_id` (the addressed human — strip the `user:` prefix; `null`
   ⇒ operator/admin) and a `dashboard_path`; the agent mints a one-shot
   signed link with `dashboard_link` (acting as that human) and relays it —
   *"I reorganized X — see it here: [link]"*. What comes back in `url` is a
   **full address** when the operator has declared the server's public one
   (`public_base_url`), and a **path on the server** when they have not —
   in that case prefix it with the base URL you reach the server at, the
   same rule as `dashboard_path` above. A value that already carries a
   scheme is handed on as it stands. A structural change is not undone: the person steers the memory
   by talking to it. **Cadence:**
   piggyback one poll
   on each user turn (in parallel with the ingest call) as the floor;
   add a background tick (≈30 s for a chat bot, never faster than ~5 s)
   so a notice reaches a user who is **not** currently talking. That
   out-of-turn tick is what makes delivery *proactive* rather than
   next-turn, and it needs your host channel to permit server-initiated
   outbound — a Telegram bot, for one, cannot cold-message a user who
   has never written to it, so until they do the notice waits in the
   queue for the next poll. The agent-side routing and wording live in
   [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md); the bridge owns the
   poll/ack loop and an outbound path to the user.
9. **Map an unidentified human to `guest` — never to a wrong real
   identity.** When your host meets a person it cannot resolve to an
   enrolled user (an unrecognized voice on a satellite, an unknown chat
   sender), send the turn with `X-MWE-Act-As: guest` — the builtin
   pseudo-identity, available once the admin ticks `guest` in your
   consumer's delegation roster (that grant is the feature's enable
   switch; without it the call gets `403 act_as_not_delegated`). A guest
   turn is **ephemeral**: recall returns only public memory, nothing is
   stored, and the response's `rules` field carries a reserved-behaviour
   directive — inject it like any other rules payload. Skip the
   `POST /media` upload on guest turns (it answers 403). Falling back to
   a real user instead would file a stranger's words as that user's
   facts and hand the stranger that user's recall — the exact
   misattribution `guest` exists to prevent.

Structural intent (`dashboard_link`) and the *smart*-consumer
`wiki_admin_*` family sit on top of this; the full surface is catalogued
in , and the
consumer-agent runtime contract (what *the agent itself* must do with
these fields) is in [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).

**Latency note.** The recall block is computed in-line: a classifier
completion plus — on capture/recall turns — a small number of navigator
completions, all **before** your agent can compose its reply. The operator
bounds this spend from the dashboard recall-settings page (hop depth, pages
per hop, budgets).

### Still being hardened

Copy-paste client configs now ship for the three hosts in the `/bridges`
catalog; the exact identity-claim handshake from the consumer's
perspective, error/retry semantics, and versioning/compatibility
guarantees are still being driven by real consumers. The
**proactive out-of-turn delivery** in step 8 ships in both bridges
(hermes drains `fact_minted_for_you` per-recipient in the `mwe-events`
gateway hook and batches the system kinds in a daily-digest cron script;
nanoclaw's host polls on its own tick and puts a delivery instruction in
the recipient's own chat — see each bridge README §Reverse channel); a
bridge without its own poll/ack loop delivers nothing out of turn. If you're
integrating now and hit a gap, open an issue — real integration friction
is exactly what we want to capture here.
