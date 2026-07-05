---
title: Per-turn context model & agent bridges
status: gated
---

# 3. Per-turn context model & agent bridges

**Shipped: the server-side half + the hermes bridge (the first live consumer).** This group
defines what gets injected into a consumer LLM's prompt **each turn** — person, recent window,
recalled memory, tools, usage instructions — under a minimal-session thesis (a small recent-window
buffer + topic-aware recall every turn, the MemGPT/Letta virtual-context pattern), and ships the
host-side half as **agent bridges**: in-repo deliverables under `agents-bridges/`, one directory
per host framework. The recall-source strategy (passthrough, no pre-fetch — «memoria migliore,
primo token più lento» is the standing tuning priority), the consumer-owned recent window, the
per-turn contract (v1), the hermes bridge, and the served-installer distribution all landed.
Current state: [agents-bridges.md](../development/agents-bridges.md),
[INTEGRATING.md](../../INTEGRATING.md), [hermes README](../../agents-bridges/hermes/README.md),
[ingest-pipeline.md](../design-notes/ingest-pipeline.md).

**Demoted to opportunistic (2026-06-21, maintainer):** the remaining bridges + the reverse channel
are built when a second host actually needs one, not on the critical path.

## Why the bridge matters — two integration tiers

The gap between these two is the product's differentiation; the bridges exist to close it:

1. **Vanilla MCP (no bridge).** Memory is **tool-driven**: capture only when the model calls the
   tool (probabilistic, silent holes), recall on-request (emergence never fires unsolicited), no
   recent-window threading, no event delivery, no prompt-cache discipline — parity with competitor
   memory-over-MCP products.
2. **The bridge (the per-turn contract).** Mechanical one-ingest-per-turn (guaranteed capture), the
   recall block injected after stable content every turn, consumer-owned window via
   `recent_messages`, per-call act-as for multi-user attribution, an events daemon for proactive
   delivery, and the sessionless model (bounded window, recall replaces the summary).

## Remaining work

- [ ] 3f — Build the **nanoclaw bridge package** + open the **reconciliation entry in the
  samvise-2.0 planning** (cross-repo). Design (verified on nanoclaw upstream v2.0.64): the **SDK MCP
  path is dropped** — upstream `McpServerConfig` is stdio-only and `container_configs` is
  per-agent_group (one shared `container.json`), so a static `X-MWE-Act-As` cannot vary per sender;
  the bridge wins over two upstream patches. The package (TypeScript): a `mwe-client` with a
  per-sender connection pool (act-as fixed at connect), the prompt-builder injection of the recall
  block (after stable content), the events daemon (`events_poll` → host channel, `events_ack`), and
  a patch guide for the fork seams (the prompt-builder hook point; **stateless-per-turn** — don't
  thread `continuation`, send only pending messages, the window is the bridge's local cache). The
  consumer fork consumes the package; only deployment-specific code stays in the fork.
- [ ] 3g — Verify prompt-cache efficiency with the volatile recall block placed after the stable
  content, measured on the first live bridge.
- [ ] 3j — Wire the **reverse channel** in the hermes bridge: a poll/ack daemon in the gateway
  service that drains `events_poll`, routes each notice to its `recipient_id` human, mints the
  `dashboard_link`, and pushes it out-of-turn. Declared by the v1 contract (`INTEGRATING.md` step 8)
  but unbuilt — today's gateway polls Telegram inbound only. Cold-initiate constraint to solve (a
  bot cannot message a user who never wrote first); nanoclaw's events daemon (3f) shares the shape.
- [ ] 3h — *(gated on the first two bridges landing)* Public bridge-authoring guide + the
  OpenClaw-compat bridge; includes re-baselining `AGENT_INSTRUCTIONS.md` (+ bundled skills) to the
  two-tier model. The `agents-bridges/README.md` is its seed.

## Notes for future bridges

- **Test identities** — one bot system-user per bridge (plain lowercase letters + digits: the id
  grammar rejects `-` and, since 2026-07-02, `_` — see
  [identity-and-acl §1.6](../concepts/identity-and-acl.md)); separate bot memory wikis, no
  cross-contamination while comparing hosts.
- **hermes endgame** — once mature, an upstream PR to hermes-agent's provider registry (mwe-mcp
  listed alongside mem0/supermemory/honcho).
- **Distribution packaging** (pip/npm) + the public authoring guide: deferred to 3h.
