---
title: wiki-lookup-guide — how to navigate and maintain this wiki
area: meta
status: stable
last_review: "2026-06-07"
---

# wiki-lookup-guide

This guide is for **AI agents (and humans)** working on the mwe-mcp
codebase. It explains how to look things up in this engineering wiki and
how to keep it in sync with the code.

> If you are looking for how a *consumer* agent uses mwe-mcp at runtime
> (the MCP tool surface, families A–K), that is documented in
> [`../AGENT_INSTRUCTIONS.md`](../AGENT_INSTRUCTIONS.md) — a different
> file with a different audience.

## Two principles

1. **The wiki is the source of truth, in lockstep with code.** This wiki
   describes what the code _is_, not what _might be_. When you change
   code, update the page covering that area **in the same commit**. When
   a page becomes wrong, fix it immediately — a stale SSOT is worse than
   a missing one. (Forward-looking work — what we plan to build next —
   lives in [`roadmap.md`](roadmap.md) and the detail pages under
   [`planning/`](planning/); the per-area pages describe only the current
   state.)
2. **One concept per page.** If two concepts keep cross-referencing, they
   belong on the same page or in adjacent pages, not scattered. Prefer a
   single canonical page per mechanism and let others link to it rather
   than re-describing it (the "no second source" rule that keeps counts
   and schemas from drifting).

## Navigation

Entry point: [`index.md`](index.md). From there:

- `concepts/` — what the system is and why: the memory model, identity
  and ACL.
- `architecture/` — the crates and modules, and the runtime topology /
  cost model.
- `development/` — how to build, test, and contribute.
- `protocol/` — the MCP tool surface, the full per-tool reference, and the
  configuration schema.
- `examples/` — end-to-end worked scenarios.
- `design-notes/` — one page per mechanism (the data model, pipelines,
  REM, the marker grammar, the dashboard, …).
- [`roadmap.md`](roadmap.md) — the single forward-looking list (only what
  is still to build), with one detail page per area under `planning/`.
- [`logs.md`](logs.md) — the append-only decision log.

The wiki is the SSOT: a topic that exists in the code should have a home
here. If you can't find one, it is a gap to fill, not a pointer to chase
elsewhere.

## Frontmatter contract

Every page starts with YAML frontmatter:

```yaml
---
title: <one-line description>
area: meta | concepts | architecture | development | protocol | examples | design-notes
status: scaffold | partial | implemented | stable
last_review: YYYY-MM-DD
---
```

`status` semantics, in order of maturity:

- `scaffold` — design known, code is a stub or absent.
- `partial` — some behavior implemented, the rest stubbed.
- `implemented` — feature complete; further evolution still possible.
- `stable` — settled; do not expect changes without a roadmap entry
  justifying them.

Those four are the **per-area page ladder**. The living and forward
surfaces use their own values: `roadmap.md` and `logs.md` carry
`status: living`; each `planning/` detail page carries `status: planned`
/ `in-progress` / `gated` (blocked on a prerequisite).

When an area is removed or replaced, **rewrite or delete its page** —
do not leave a tombstone. The wiki carries only the current state.

`last_review` is **the date only** (no prose summary) and is bumped when
the page content is verified against the code (not just touched). A page
that has been merely renamed does not need a new `last_review`.

## When to create a new page vs. edit an existing one

Create new only when:
- A new module or crate appears.
- A new MCP tool family ships.
- A non-obvious mechanism lands in code and deserves a focused design
  note that no existing page can absorb cleanly.

Otherwise, edit the existing page. Splitting a long page is a last-resort
move and benefits from a brief comment at the top of the new page
explaining the split.

## Cross-links

- **Inside the wiki:** relative paths from the page —
  `[overview](architecture/overview.md)` from a root page,
  `[overview](../architecture/overview.md)` from a per-area page.
- **Wiki → code:** relative paths to `crates/...` / `migrations/...`.
  Anchor on the file (and, where stable, a symbol) rather than a line
  number — line numbers rot. The code is the ultimate SSOT for
  derived facts (counts, defaults): prefer pointing at
  `schemas::all_tools()`, `rem::run_cycle`,
  `RemPolicy::default()`, `crates/mwe-core/src/lib.rs` over hardcoding a
  number that will drift.
- **Do not** link into the gitignored `road-behind/` archive at the repo
  root — it is the frozen historical corpus, out of the tracked repo. Its
  canonical content lives in this wiki; forward work is in
  [`roadmap.md`](roadmap.md).

## What lives where (cheat-sheet)

| Topic | Wiki page |
|---|---|
| what the system is / the memory model | [concepts/memory-model.md](concepts/memory-model.md) |
| identity, single-admin, block-level ACL, wiki visibility | [concepts/identity-and-acl.md](concepts/identity-and-acl.md) |
| crate map + module catalog | [architecture/overview.md](architecture/overview.md) |
| runtime topology, who-pays-what, the LLM-free hot path | [architecture/runtime-topology.md](architecture/runtime-topology.md) |
| how to build / the CLI roster | [development/build-run.md](development/build-run.md) |
| formatting / linting / MSRV | [development/conventions.md](development/conventions.md) |
| MCP tool surface (families A–K), per-tool status | [protocol/mcp-tools.md](protocol/mcp-tools.md) + [design-notes/mcp-dispatcher.md](design-notes/mcp-dispatcher.md) |
| full per-tool I/O contract + `_internal.*` APIs + agentic tools | [protocol/tool-reference.md](protocol/tool-reference.md) |
| `mwe-mcp.config.yaml` schema (LLM slots, profiles, REM knobs, budget) | [protocol/config-schema.md](protocol/config-schema.md) |
| the configurable internal LLM (functions, profiles, language policy) | [design-notes/llm-functions.md](design-notes/llm-functions.md) |
| stack choice | [design-notes/why-rust.md](design-notes/why-rust.md) |
| data model (filesystem SSOT, `_meta.md`, atomic write) | [design-notes/wiki-filesystem-ssot.md](design-notes/wiki-filesystem-ssot.md) |
| four hard-coded actor wiki kinds + the per-wiki smart flag | [concepts/memory-model.md](concepts/memory-model.md) + [design-notes/smart-wikis.md](design-notes/smart-wikis.md) |
| capture / supersede / forget / link + jaccard dedup | [design-notes/capture-and-dedup.md](design-notes/capture-and-dedup.md) |
| recall pipeline (search / facts_for / recall / navigate / multi-hop) | [design-notes/recall-pipeline.md](design-notes/recall-pipeline.md) |
| ingest pipeline (`wiki_ingest_message` LLM router) | [design-notes/ingest-pipeline.md](design-notes/ingest-pipeline.md) |
| narrative captures buffer (`_captures.md` + `capture_buffer`) | [design-notes/narrative-buffer.md](design-notes/narrative-buffer.md) |
| narrative compiler topology planner (`CompilationPlan`) | [design-notes/narrative-compiler.md](design-notes/narrative-compiler.md) |
| REM nightly cycle (orchestrator + sub-jobs) | [design-notes/rem-cycle.md](design-notes/rem-cycle.md) |
| DDL + migrations ledger + `engine.db` | [design-notes/engine-db-and-migrations.md](design-notes/engine-db-and-migrations.md) |
| marker grammar (EBNF + parser internals) | [design-notes/marker-grammar.md](design-notes/marker-grammar.md) |
| ACL algorithm + redaction (`can_read` / `render_for_sender`) | [design-notes/redaction-policy.md](design-notes/redaction-policy.md) |
| `structure_proposal` apply/revert chassis + kind handlers | [design-notes/proposal-apply-engine.md](design-notes/proposal-apply-engine.md) |
| applicative WAL (crash recovery for proposals + REM) | [design-notes/applicative-wal.md](design-notes/applicative-wal.md) |
| single-writer lockfile | [design-notes/single-writer-lockfile.md](design-notes/single-writer-lockfile.md) |
| filesystem watcher → `fact_index` consumer | [design-notes/reindex-pipeline.md](design-notes/reindex-pipeline.md) |
| hierarchical wiki move (`wiki_change_scope`) | [design-notes/scope-change.md](design-notes/scope-change.md) |
| logging (`info`/`debug`) + config precedence | [design-notes/logging.md](design-notes/logging.md) |
| MCP dispatcher (rmcp `ServerHandler` + JWT middleware + audit) | [design-notes/mcp-dispatcher.md](design-notes/mcp-dispatcher.md) |
| JWT shape + session model + `X-MWE-Act-As` | [design-notes/jwt-and-session-model.md](design-notes/jwt-and-session-model.md) |
| first-run setup + account-vs-identity + welcome wizard | [design-notes/setup-and-identity.md](design-notes/setup-and-identity.md) |
| identity validation + DB mirror | [design-notes/enrollment-loader.md](design-notes/enrollment-loader.md) |
| dashboard architecture + auth + routes | [design-notes/dashboard.md](design-notes/dashboard.md) |
| dashboard frontend (Tailwind surface, responsive contract) | [design-notes/dashboard-frontend.md](design-notes/dashboard-frontend.md) |
| dashboard memory MVP + agentic chat | [design-notes/dashboard-memory-mvp.md](design-notes/dashboard-memory-mvp.md) + [design-notes/agentic-chat.md](design-notes/agentic-chat.md) |
| admin LLM config editor + API-key panel | [design-notes/admin-llm-config.md](design-notes/admin-llm-config.md) |
| smart wikis / smart consumers / `_briefing.md` / op-log / cite | [design-notes/smart-wikis.md](design-notes/smart-wikis.md) |
| narrative scenarios | [examples/scenarios.md](examples/scenarios.md) |
| consumer agent usage spec | [`AGENT_INSTRUCTIONS.md`](../AGENT_INSTRUCTIONS.md) (repo root, canonical) |
| what is still to build (forward work) | [roadmap.md](roadmap.md) + detail pages under [planning/](planning/) |
| chronological decision log | [logs.md](logs.md) |

## Searching the historical corpus

The frozen design record — the long-form Italian planning corpus, the
chronological design log, and the audit/dogfood working docs — lives in
`road-behind/` at the repo root, which is **gitignored** (kept on disk
locally, out of the tracked repo). It is a reference of last resort: the
canonical content is in this wiki. When you grep it, remember it is
**Italian** while the code is English — search for *both* terms (e.g.
`cruscotto` **and** `dashboard`, `fatto` **and** `fact`) before
concluding something isn't there.

## Anti-patterns

- ❌ Leaving a wiki page stale after the code moved. The wiki is the SSOT;
  a wrong page misleads every reader and every agent.
- ❌ Marking a page `implemented` while the tests are stubs.
- ❌ Hardcoding a derived count (tools, sub-jobs, migrations, modules,
  templates) as a load-bearing fact. Point at the code SSOT instead; keep
  at most one canonical count where a page is pedagogically the roster
  (e.g. `protocol/mcp-tools.md`).
- ❌ Linking into the gitignored `road-behind/` archive — repoint to the
  wiki page that now owns the content, or to [`roadmap.md`](roadmap.md).
- ❌ Using bare "wiki" to mean a *memory wiki* (the consumer-side runtime
  concept). Say "memory wiki" / "consumer wiki" when you mean that.
