# agents-bridges — host adapters for mwe-mcp

This directory is the home of the **agent bridges**: thin adapters that wire a
host agent framework (hermes-agent, nanoclaw, OpenClaw, …) to a running
mwe-mcp server at full fidelity. A bridge implements the **per-turn contract
(v1)** specified in
[`INTEGRATING.md` §"The per-turn contract"](../INTEGRATING.md#the-per-turn-contract-v1--wiring-a-host-bridge);
the behaviour the host's *agent* must carry on top of it (how to use the
recall block, the disambiguation etiquette, the seed rewrite rule) is in
[`AGENT_INSTRUCTIONS.md`](../AGENT_INSTRUCTIONS.md).

Bridges are **mwe-mcp deliverables**, maintained here in lockstep with the
contract — not appendices of the host repos. A consumer deployment repo
*consumes* a bridge; only deployment-specific code (skills, channel wiring,
sender resolvers) stays on the consumer side.

## Why bridges exist — the two integration tiers

A host can reach mwe-mcp at two fidelity levels:

1. **Vanilla MCP (no bridge).** Any MCP client pointed at `/mcp` with a
   bearer token — including stdio-only hosts through a stdio→HTTP proxy.
   Memory is *tool-driven*: capture happens only when the model decides to
   call a tool (probabilistic, silent holes), recall is on-request, no
   recent-window threading, no event delivery, no prompt-cache discipline.
   It works, but none of the differentiators fire.
2. **The bridge (the per-turn contract).** Mechanical one-ingest-per-turn
   (guaranteed capture), the recall block injected after the host's stable
   prompt content on every turn, the consumer-owned sliding window threaded
   through `recent_messages`, per-call act-as for multi-user attribution,
   an events channel for proactive delivery, and the sessionless model
   (bounded window, no summarization pass — recall replaces the summary).

The gap between the tiers is the product's differentiation; the bridges
exist to close it.

## Layout

```
agents-bridges/
  README.md          ← you are here: the authoring guide
  _harness/          ← shared smoke-harness material (not a bridge)
    mwe_client.py    ← reference MCP client (Streamable HTTP, Bearer JWT, act-as)
    stub_server.py   ← in-process stub of the MCP endpoint for offline smokes
    run_smokes.py    ← manifest validator + smoke runner (the CI entrypoint)
  <bridge>/          ← one directory per host framework
    bridge.toml      ← compat manifest (required, schema below)
    README.md        ← install + configuration guide for that host
    …                ← the bridge code, in the host's native layout
```

Bridges are host-native code (Python, TypeScript, …) and live **outside the
cargo workspace** — the Rust CI does not build or gate on them; they have
their own non-blocking workflow (below).

## The compat manifest — `bridge.toml`

Every bridge directory carries a `bridge.toml` (TOML so the harness can
parse it with the Python standard library):

```toml
bridge = "hermes"                  # must equal the directory name
description = "hermes-agent memory-provider + context-engine plugin pair"
contract = 1                       # per-turn contract version implemented

[upstream]
name = "hermes-agent"
repo = "https://github.com/example/hermes-agent"
pin = "v2026.6.5"                  # upstream ref (tag or commit) the bridge is tested against

[smoke]
offline = "./smoke.sh"             # required: CI-runnable, no server, no LLM
live = "./smoke_live.sh"           # optional: operator-run, against a real server
```

- **`contract`** — the version of the per-turn contract the bridge
  implements. The contract is versioned where it is specified
  (`INTEGRATING.md`); a breaking contract change bumps that version and
  updates **every in-repo bridge in the same commit** (the lockstep rule),
  so the harness fails the manifest check when a bridge lags.
- **`upstream.pin`** — the upstream ref the bridge is tested against.
  Upgrading the pin is a deliberate act, never an automatic side effect of
  a red canary.

## The smoke harness — two tiers

- **Offline smoke** (`smoke.offline`, required) — runs in CI with no
  mwe-mcp server and no internal LLM. It drives the bridge's per-turn
  mechanics against `_harness/stub_server.py` (an in-process stub of the
  MCP endpoint that records every request) and asserts the contract
  mechanics: exactly one ingest per conversational turn, window threading
  and trimming, the act-as header per sender, recall-block placement after
  the stable content, the disambiguation follow-up, the degradation path.
  Because it exercises the bridge against the **host framework's real
  plugin seams**, it is also what the upstream canary runs.
- **Live smoke** (`smoke.live`, optional) — an operator-run scripted
  multi-turn conversation against a real server on a throwaway workdir
  (the dogfood pattern, see
  [`tests/dogfood-standard/`](../tests/dogfood-standard/instruction.md)).
  This validates memory behaviour end-to-end and costs LLM calls; a bridge
  is not *functional* until its live smoke has passed at least once.

`_harness/mwe_client.py` is the **reference client implementation**
(Streamable HTTP, Bearer JWT, `X-MWE-Act-As`, stdlib-only): use it directly
in Python smokes, and as the model for a bridge's own client in other
languages.

Run everything locally from this directory:

```bash
python3 _harness/run_smokes.py --check   # validate manifests only
python3 _harness/run_smokes.py           # validate + run all offline smokes
```

## CI — non-blocking by construction

[`.github/workflows/bridges.yml`](../.github/workflows/bridges.yml) is a
**separate workflow** from the main `ci`, so it never gates a merge:

- **push / PR** touching `agents-bridges/**`: manifest check + all offline
  smokes, each against its pinned upstream (`BRIDGE_UPSTREAM_REF=<pin>`).
- **weekly canary** (cron): the same offline smokes against upstream HEAD
  (`BRIDGE_UPSTREAM_REF=HEAD`), so upstream drift is *detected*. A red
  canary means the host's seam moved; fixing the bridge (and moving the
  pin) is a deliberate follow-up.

The smoke script receives the upstream ref in the `BRIDGE_UPSTREAM_REF`
environment variable and is itself responsible for fetching/checking out
the host framework at that ref (each host has its own way).

## Authoring a new bridge — checklist

1. **Read the contract** — `INTEGRATING.md` §"The per-turn contract (v1)" —
   and `AGENT_INSTRUCTIONS.md` for the agent-side behaviour your host's
   prompt must carry.
2. **Map the six contract points to your host's seams** (memory-provider
   hook, prompt-builder, plugin API — whatever the host offers). The prize
   criteria: one mechanical ingest per turn, recall block after stable
   content, window owned and trimmed bridge-side.
3. **Identity**: a bridged bot is a *standard* consumer — a credential-less
   system user speaking for delegated humans. Per-sender attribution rides
   the `X-MWE-Act-As` header; fix it per connection (a small client pool
   keyed by sender) rather than per call if your MCP client can't set
   per-call headers. No header = the bot's own memory wiki. Name the bot's
   user id with plain lowercase letters and digits (no underscore — see
   `INSTALL.md` §"Finish setup in the browser").
4. **If the host has a built-in memory, disable it** (the *replace*
   rule): a second, ungoverned store accumulates stale duplicates, skips
   per-reader redaction, and leaks across senders when injected globally.
   Capture needs no save tool — the per-turn ingest is the capture path.
   Rationale and live evidence: the hermes bridge's `README.md`
   §"Design choices". Also mind the **trust boundary**: a host with
   shell/file tools on the same machine can read the workdir raw
   (`INTEGRATING.md` §"Deployment security").
5. **Create `<bridge>/`** with `bridge.toml` (schema above) and a
   `README.md` that takes an operator from zero to a configured host.
6. **Write the offline smoke** against `_harness/stub_server.py`, asserting
   the contract mechanics through the host's real plugin seams.
7. **Run the live smoke** against a local `mwe-mcp serve` before declaring
   the bridge functional.
8. **Test identity**: one bot system-user per bridge (e.g. `samhermes`),
   with its own consumer token and delegation list — separate memory wikis,
   no cross-contamination between hosts under comparison.
