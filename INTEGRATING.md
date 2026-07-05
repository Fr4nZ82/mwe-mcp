# Integrating mwe-mcp — the developer & operator guide

This guide is for **developers wiring a host agent framework to mwe-mcp** and for
**operators deciding where to run the consumer**. It assumes a server is already
up — if not, start with **[`INSTALL.md`](INSTALL.md)** (download-and-run, three
steps).

**If you just want to connect a ready-made consumer, the running server walks you
through it — you may not need this guide.** Open the **Bridges** tab in the
dashboard (or `/bridges` unauthenticated) for the one-command, copy-paste setup
per supported host; the public front page at `/` points a capable agent straight
at a machine-readable `install.md` it can run itself. Today the one ready-made
bridge is **Hermes** (Nous Research).

This guide is what's left once that path doesn't fit: the **per-turn contract** to
write a bridge for a host we don't ship, the **deployment-security rules** for
where the consumer runs, and the map into the engineering wiki for everything
authoritative. It is **not** the runtime spec for the consumer agent itself — an
LLM agent that *talks to* mwe-mcp over MCP reads
[`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) instead.

> **What is solid vs. what is still moving.** The **per-turn contract** a host
> bridge implements (below) is concrete and lockstep with the code. The remaining
> consumer-side detail — copy-paste client configs for specific MCP hosts, an
> end-to-end worked integration, the identity/delegation handshake from the
> consumer's point of view — is still being hardened against the first real
> consumers. For the authoritative, lockstep-with-code detail on any topic, follow
> the links into the [engineering wiki](wiki/index.md).

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
  Ready-made bridges live under [`agents-bridges/`](agents-bridges/README.md);
  today the one production bridge is **[Hermes](#the-ready-made-bridge-hermes)**.

**Prerequisites — all set up in [`INSTALL.md`](INSTALL.md):**

1. a **running server** (`mwe-mcp serve`) exposing `/mcp` and `/dashboard/*`;
2. the **internal LLM wired** from the dashboard (Anthropic / Gemini / a local
   Ollama workhorse / a mix — embeddings always run locally);
3. a **bearer token** minted for your agent (Admin → users / tokens). A consumer's
   class (`standard` vs. `smart`) and its identity claims are decided at mint time.
   How identity, delegation, and the access-control model work is in
   [`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md) and
   [`wiki/design-notes/setup-and-identity.md`](wiki/design-notes/setup-and-identity.md).

The rest of this guide is the per-turn contract a bridge implements, plus where to
run the consumer safely.

---

## The ready-made bridge (Hermes)

The one **per-turn** (standard-consumer) host bridge shipped today wires
**[Hermes](https://github.com/NousResearch/hermes-agent)** (Nous Research) to a
running mwe-mcp at full fidelity — the per-turn contract below, delivered as a
plugin trio with **no fork and no upstream patch**. You don't wire it by hand: the
server serves the installer at **`/bridges/hermes`** (one command), and the
complete step-by-step — plugins, `mwe.json`, the bot token, `config.yaml`, the
Telegram gateway, media capture — lives in
**[`agents-bridges/hermes/README.md`](agents-bridges/hermes/README.md)**. The
token stays a dashboard step (issued from the home's *Connect a consumer* card);
the installer never handles it.

> **One operational rule worth repeating up front:** turn Hermes's built-in
> memory **off** (`memory_enabled: false` **and** `user_profile_enabled: false`
> — two separate flags, both default on: the first gates `MEMORY.md`, the second
> `USER.md`) and let mwe-mcp be the only memory.
> A second, ungoverned store accumulates stale duplicates, skips per-reader
> redaction, and (when injected globally) leaks one user's facts into another's
> prompts. Capture needs no "save" tool — the per-turn ingest *is* the capture
> path. The reasoning is in the bridge README's *Design choices*.

If your host isn't Hermes, implement the
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

## Where the rest of the detail lives

| Topic | Where the detail lives |
|---|---|
| Standing the server up, configuring its LLM, minting tokens | [`INSTALL.md`](INSTALL.md) |
| Transport (MCP Streamable HTTP), endpoints, JWT bearer | [`wiki/design-notes/mcp-dispatcher.md`](wiki/design-notes/mcp-dispatcher.md), [`wiki/design-notes/jwt-and-session-model.md`](wiki/design-notes/jwt-and-session-model.md) |
| Token / identity flow (admin invites → user → consumer) | [`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md), [`wiki/design-notes/setup-and-identity.md`](wiki/design-notes/setup-and-identity.md) |
| The tool surface and per-tool I/O contract | [`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md), [`wiki/protocol/tool-reference.md`](wiki/protocol/tool-reference.md) |
| Server config + LLM profiles + secrets | [`wiki/protocol/config-schema.md`](wiki/protocol/config-schema.md) |
| Deployment topology (server and consumer on separate hosts, remote HTTP) | [`wiki/architecture/runtime-topology.md`](wiki/architecture/runtime-topology.md) |
| Consumer-agent runtime behaviour (what *your agent* must do) | [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md) |
| Ready-made host bridges + the bridge-authoring guide | [`agents-bridges/README.md`](agents-bridges/README.md) |
| Smart vs. standard consumers, smart wikis | [`wiki/design-notes/smart-wikis.md`](wiki/design-notes/smart-wikis.md) |

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
   / operate with the served user, recalled from the agent's own memory) plus
   any one-shot governance notice — kept structurally apart from the memory so
   a binding rule is never mistaken for a remembered fact. **Inject `rules`
   too, verbatim and adjacent to the block** — the field is self-labelled
   (`YOUR RULES (…)`, apply-don't-relay wording included), so add no preamble
   of your own; privacy/sharing is *not* here — it is enforced memory-side
   by the ACL, so the agent simply never recalls what it may not see. Do **not** build a
   separate pre-fetch recall path: the block's navigation step reuses the
   classifier's own routing signals, which a raw pre-classification search
   cannot reproduce. `wiki_search` remains available for explicit,
   user-visible lookups.
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
   truth about which branch fired.
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
   also emits notices when *it* acts and the user should know: REM
   applied a structural change directly (`structure_applied`), a dedup
   proposal auto-applied (`auto_applied`), a merge awaits an answer
   (`dedup_proposed`), a document finished ingest (`document_ingested`).
   Poll them with `events_poll`, hand each to your agent, then
   `events_ack` the ids so the server stops re-delivering. A structural
   payload carries `recipient_id` (the addressed human — strip the
   `user:` prefix; `null` ⇒ operator/admin), a `dashboard_path`, and a
   `revert_deadline`; the agent mints a one-shot signed URL with
   `dashboard_link` (acting as that human) and relays it — *"I
   reorganized X — undo here: [link]"*. **Cadence:** piggyback one poll
   on each user turn (in parallel with the ingest call) as the floor;
   add a background tick (≈30 s for a chat bot, never faster than ~5 s)
   so a notice reaches a user who is **not** currently talking. That
   out-of-turn tick is what makes delivery *proactive* rather than
   next-turn, and it needs your host channel to permit server-initiated
   outbound — a Telegram bot, for one, cannot cold-message a user who
   has never written to it, so until they do the notice waits for their
   next turn (where the recall block's `pending_attention` reminder
   still carries it). The agent-side routing and wording live in
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
in [`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md), and the
consumer-agent runtime contract (what *the agent itself* must do with
these fields) is in [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).

**Latency note.** The recall block is computed in-line: a classifier
completion plus — on capture/recall turns, when the `navigator` LLM slot
is wired — a small number of navigator completions, all **before** your
agent can compose its reply. The operator bounds this spend from the
dashboard recall-settings page (hop depth, pages per hop, budgets); turn
navigation off entirely by leaving the `navigator` slot unconfigured.

### Still being hardened

Copy-paste client configs for specific MCP hosts, the exact
identity-claim handshake from the consumer's perspective, error/retry
semantics, and versioning/compatibility guarantees land as the surface
stabilises toward 1.0, driven by the first real consumers. The
**proactive out-of-turn delivery** in step 8 is specified but not yet
drained by every shipped bridge — today's Hermes gateway does not poll
`events_poll`, so its structural notices currently reach the user only
via the next-turn `pending_attention` reminder; wiring the poll/push
daemon is tracked on the roadmap (§3). If you're
integrating now and hit a gap, open an issue — real integration friction
is exactly what we want to capture here.
