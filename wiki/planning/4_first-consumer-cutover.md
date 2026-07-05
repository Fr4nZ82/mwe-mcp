---
title: First-consumer cutover
status: in-progress
---

# 4. First-consumer cutover

Drive the first real consumer integration into production to validate the memory standard, surface
edge cases, and refine the API. This area is iterative: it closes when the consumers are stable
and satisfied, not on a fixed checklist.

The integration itself is **live**: multiple real consumers run against prod over HTTP daily — the
hermes bridge (standard-conversational, feeding the single conversational ingest entry point) plus
claude.ai web and Claude Code as Smart consumers
([agents-bridges.md](../development/agents-bridges.md) ·
[web-agent-oauth.md](../design-notes/web-agent-oauth.md)). The concurrency machinery ships
(single-writer lockfile with Drop cleanup, applicative WAL, race detection), so the multi-consumer
soak is the real traffic itself. The originally-planned shadow period against the prior system was
overtaken by events — the prior system was retired before cutover, so validation is direct
dogfood.

## Steps

- **4b** — *(partial)* Validate the end-to-end per-user proposal flow: ≥20 facts on a topic page →
  sub-wiki emersion → recipient routing → magic link → receipt. Emersion and notice emission run
  live; the consumer-push delivery leg is [3j](3_context-model.md).
- **4d** — Admin-only recovery surfaces: the on-demand snapshot/Backup console and the manual REM
  trigger ship; remaining are **daily automatic snapshots**, **dashboard restore**, and **memory
  reset with an auto-safety snapshot**.
- **4f** — Performance tuning once profiled: batch embedding, a vector index, caching of metadata
  reads, incremental reindex under load.
- **4h** — Author operational docs: troubleshooting, backup/restore, disaster-recovery playbook,
  capacity planning.

> **4j landed 2026-07-05** — emerged/topic wikis' `index.md` is a never-GC'd `EmergedIndex`
> foundation node seeded at every plan build, carrying **no identity semantics** (a
> non-enrolled subject is a topic, not a user — maintainer ruling in
> [logs.md](../logs.md)). Current state:
> [narrative-compiler.md §Fonditore](../design-notes/narrative-compiler.md#stage-0--the-fonditore-deterministic-foundation).

## Open decisions

- **Phase completion semantics.** Treat this phase as a soft checkpoint whose closure metric
  (consumer stable for several weeks, latency target met, low bug rate) unblocks the public-release
  gate, while letting release documentation and examples start in parallel. Not a hard freeze.
- **Ingest output robustness.** Keep the robust JSON parsers as primary; instrument the production
  parse-failure rate during this phase and adopt a grammar-constrained `format:"json"` only if
  failures exceed a few percent. This is the one live measure-first question.
