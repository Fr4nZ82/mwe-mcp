---
title: Agent bridges — the in-repo host-adapter home
area: development
status: partial
last_review: "2026-06-29"
---

# Agent bridges (`agents-bridges/`)

The **agent bridges** are in-repo deliverables — one directory per host
agent framework — that wire a host (hermes-agent, nanoclaw, …) to a running
mwe-mcp server at full fidelity: the **per-turn contract (v1)** specified in
[`INTEGRATING.md` §"The per-turn contract"](../../INTEGRATING.md). They are
a product surface of mwe-mcp, not host-repo appendices: a consumer
deployment repo *consumes* a bridge, and only deployment-specific code
stays on the consumer side.

The authoring guide — the contract pointers, the directory layout, the
manifest schema, the authoring checklist — is
[`agents-bridges/README.md`](../../agents-bridges/README.md) itself (the
seed of the future public guide). This page documents the machinery around
it.

## The home (what is built today)

```
agents-bridges/
  README.md          the authoring guide
  _harness/          shared smoke-harness material
    mwe_client.py    reference MCP client (Streamable HTTP, Bearer JWT, X-MWE-Act-As)
    stub_server.py   in-process MCP-endpoint stub for offline smokes (records every call)
    run_smokes.py    manifest validator + smoke runner (the CI entrypoint)
  hermes/            the hermes-agent bridge (below)
```

Bridges are host-native code (Python, TypeScript, …) **outside the cargo
workspace**: the Rust CI neither builds nor gates on them.

## The compat manifest

Each bridge carries a `bridge.toml` (TOML so the harness parses it with the
Python standard library, no dependencies): `bridge` (= directory name),
`description`, `contract` (the per-turn-contract version implemented),
`[upstream] name/repo/pin` (the upstream ref the bridge is tested against),
`[smoke] offline` (required, CI-runnable) and `live` (optional,
operator-run). `run_smokes.py --check` enforces the schema and rejects a
`contract` value that lags the current contract version (its
`CURRENT_CONTRACT` constant is bumped in the same commit as the
`INTEGRATING.md` stamp).

## The smoke model — two tiers

- **Offline smoke** (required): no server, no internal LLM. Drives the
  bridge through the **host framework's real plugin seams** against
  `stub_server.py`, asserting contract mechanics: one ingest per turn,
  window threading/trim, per-sender act-as header, recall-block placement,
  the disambiguation follow-up, the degradation path.
- **Live smoke** (optional, operator-run): a scripted multi-turn
  conversation against a real server on a throwaway workdir (the dogfood
  pattern). A bridge is not *functional* until its live smoke has passed.

## Distribution — served installers

A running mwe-mcp is the distribution point for its **own** bridges, so an
operator never clones the repo to install one. `mwe-dashboard`'s
[`routes/bridges.rs`](../../crates/mwe-dashboard/src/routes/bridges.rs)
(exported as `public_site_router()`, mounted at the HTTP root) serves a
**public, anonymous** surface:

- `GET /` — a slim product front page: an agent line pointing at the
  catalog, a human sign-in link.
- `GET /bridges` + `GET /bridges/<consumer>` — the catalog and the
  per-bridge instructions, followable **by hand** (first-class) or handed
  to the agent. The install command is tailored to the request `Host`.
- `GET /bridges/<consumer>/install.{sh,ps1,md}` — the machine surface.
  The shape depends on the consumer kind:
  - A **per-turn plugin bridge** (hermes) serves all three: the two
    scripts are **self-contained** — the bridge's plugin tree is embedded
    in the binary (`rust-embed` over `agents-bridges/<consumer>/plugins/`,
    `__pycache__`/`.pyc` filtered out in `plugin_files`) and inlined as
    heredocs / here-strings, so one `curl … | sh` lays everything down
    with no `tar`, no `jq`, no separate bundle. `install.md` is the
    agent-readable form ("Read … and follow").
  - A **smart-consumer onboarding** (claude-code) serves `install.md`
    **only** (`install.{sh,ps1}` → 404): there are no plugins to embed.
    The instructions register the MCP server over **OAuth** at
    `--scope user` (`claude mcp add … <origin>/mcp` → sign in) and offer
    the optional **token-less `SessionStart` nudge** — **no token to
    mint or paste**. It never bootstraps a repo automatically.

Each side knows only the half it can: the **server** knows its own URL
(from `Host`) and bakes it into the command; the **script** runs on the
consumer box and resolves the local paths there (`HERMES_HOME` defaults
to `~/.hermes`; `HERMES_SRC` is `$HERMES_SRC` or the current directory
when it looks like a checkout). The one thing the served surface never
carries is a **token** — a credential, minted in the admin-only
dashboard. For **hermes** the served path *instructs* the operator to do
the residual steps it cannot: mint the token and set
`memory_enabled: false` **and** `user_profile_enabled: false` — two
separate flags — and restart; the agent never handles the secret itself
(the token is issued from the dashboard home's *Connect a consumer*
card). **claude-code needs no token at all** — it connects over the
`webagentoauth` OAuth flow, where the user signs in to mwe-mcp inside the
CLI and approves the connection.

The **same** catalog + guide bodies are also mounted **authenticated** at
`/dashboard/bridges` (the nav *Bridges* tab) via `dashboard_tab_router()`,
sharing the body functions (a `base` prefix resolves the in-page links
under `/dashboard` there and at the root publicly) — so the operator
finds Bridges in the nav next to Wikis / Facts, while agents and `curl`
use the identical public surface. A consumer appears in the catalog only
when its bridge ships a served installer.

## Shipped bridges

### hermes (`agents-bridges/hermes/`) — plugin trio, zero fork

The proof-of-concept consumer that validated the per-turn contract live.
Three hermes-agent plugins, stdlib-only, no upstream patch:

- **`mwe` memory provider** (out-of-tree, `$HERMES_HOME/plugins/mwe/`):
  `prefetch()` is the one mechanical `wiki_ingest_message` per turn —
  synchronous by design (the ratified latency trade-off) — and returns the
  recall block, which hermes injects into the current turn's user message
  after the stable prompt prefix; `sync_turn()` keeps the consumer-owned
  window locally; `mwe_search` / `mwe_dashboard_link` /
  `mwe_disambig_commit` are proxied through a per-sender act-as client
  pool (`senderMap` routes gateway senders, `primaryUser` is the
  fallback); non-primary agent contexts (`subagent`/`cron`/`flush`) leave
  the provider fully inactive. Errors degrade to an empty block — the host
  turn never dies on a memory hiccup. The recommended mode is **replace**:
  hermes's built-in memory stays off (`memory_enabled: false` **and**
  `user_profile_enabled: false` — `memory_enabled` gates `MEMORY.md`,
  `user_profile_enabled` gates `USER.md`; both default on) and mwe is
  the only knowledge memory — capture is mechanical via the per-turn
  ingest, so no save tool is needed; an `on_memory_write()` one-way mirror
  into the memory wikis remains implemented for deployments that
  deliberately keep the built-in memory on.
- **`mwe-truncate` context engine** (in-tree by host constraint —
  hermes's engine discovery has no user directory, so install is a
  directory add into `plugins/context_engine/`): `compress()` truncates
  to a bounded window (protected head + last N, tool-call pairing kept
  intact) with no summarization pass — recall replaces the summary.
- **`mwe-media` gateway hook plugin** (standalone, opt-in via
  `plugins.enabled: [mwe-media]`): intercepts hermes's documented
  `pre_gateway_dispatch` seam, where Telegram media already sit in the
  host cache as local files. Fail-closed sender gate (an explicit
  `senderMap` entry is required — the hook fires before hermes's own
  authorization), per-file size cap, synchronous upload to
  `POST /media` with act-as = the mapped sender, then a TTL-pruned
  spool file under `$HERMES_HOME` hands the minted catalog ids to the
  memory provider, which attaches them to the turn's ingest as
  `attachments` ([media pipeline](../design-notes/media-pipeline.md)).
  The spool also closes the host's native-image-mode hole: an
  empty-text turn with spooled attachments still fires exactly one
  ingest (caption as text), so a photo turn is never a memory bypass
  (the recall block itself is still not injected on native turns — a
  host limitation). A spool entry older than its TTL is dropped, so a
  turn that never fired cannot leak attachments into a later one.

The offline smoke loads the plugins **through hermes's real discovery
seams** from a scratch checkout of the pinned upstream and drives the
contract mechanics — including the media hook → upload → spool →
ingest-attachments chain — against the recording stub; it prints its
own assertion count (the SSOT — don't mirror it here). The live smoke
(`smoke_live.py`) scripts a short conversation against a real server.
Operator docs: [`agents-bridges/hermes/README.md`](../../agents-bridges/hermes/README.md).

### claude-code — served OAuth smart-consumer onboarding (no in-repo adapter)

Claude Code is a **smart** consumer (own subscription LLM, native MCP
client), so its bridge is **not** an `agents-bridges/<consumer>/` per-turn
adapter and ships **no plugins and no smoke** — it is the served
`/bridges/claude-code` onboarding only. It connects over the
**`webagentoauth` OAuth flow** — the same inbound authorization server
claude.ai uses ([web-agent-oauth](../design-notes/web-agent-oauth.md)) —
with **no token to mint or paste**. The catalog entry serves a human
guide + an agent `install.md` that registers the MCP server over OAuth at
`--scope user` (`claude mcp add --transport http mwe-mcp <origin>/mcp
--scope user`, then sign in via `/mcp` or `claude mcp login mwe-mcp`). A
**loopback** redirect makes the mint stamp the **`Local`** profile (the
full tool catalog, including `skill_fetch` + the leases), as opposed to
claude.ai's `Web` profile. Recall is **model-driven**; the only hook is an
optional **token-less `SessionStart` `command` hook** that emits a fixed
reminder so the model calls `smart_bootstrap` itself (no `UserPromptSubmit`
recall hook). Skills load on demand via `skill_fetch`, **not** written to
`~/.claude/skills/`. Runtime behaviour (the operational wiki + per-project
wikis, the 3-way routing, the pre-bootstrap split, the `CLAUDE.md` +
`AGENTS.md` documentation-rules scan, the user-initiated ingestion that
leaves the local copy intact, per-project isolation) lives in the
`smart-consumer` / `smart-codebase` / `core-globalmemory` skills and
[`INTEGRATING.md`](../../INTEGRATING.md). Bootstrap is never automatic.

## Keeping bridges current

- **In-repo lockstep** (mwe-mcp → bridge): a change to the contract or the
  tool surface updates every bridge in the same commit — the same rule as
  this wiki.
- **Pinned upstream + canary** (upstream → bridge):
  [`.github/workflows/bridges.yml`](../../.github/workflows/bridges.yml) is
  a separate, **non-blocking** workflow. On push/PR it runs the manifest
  check + offline smokes against each pin; a weekly cron runs the same
  smokes against upstream HEAD (`BRIDGE_UPSTREAM_REF=HEAD`) so drift is
  detected. Moving a pin is a deliberate act, never an automatic follow-up
  of a red canary.

The forward work — the nanoclaw bridge, the prompt-cache measurement, and
the public authoring guide + OpenClaw bridge — is roadmap group 3
([`planning/3_context-model.md`](../planning/3_context-model.md)). The
served-installer **distribution packaging** shipped 2026-06-22 (3i; see
*Distribution* above).
