---
title: mwe-mcp engineering wiki — index
area: meta
status: implemented
last_review: "2026-06-28"
---

# mwe-mcp engineering wiki

> **What this is.** This engineering wiki is the **single source of
> truth** for what the mwe-mcp codebase is, what it does, and the logic
> underneath. It is maintained in lockstep with the code: a change to the
> code updates the relevant page in the same commit. Forward-looking work
> (roadmap, planning) is kept in the maintainer's private notes and lands
> here as it ships.

> **Terminology reminder.** Two different things share the word "wiki" —
> always write the qualified term, never a bare "wiki". The
> **engineering wiki** is this one: the docs for people working on
> mwe-mcp itself. The persistent memory that mwe-mcp manages at runtime
> for consumer agents is a **memory wiki** (or **consumer wiki**);
> memory wikis live in a configurable `--workdir` outside the repo,
> never in `mwe-mcp/wiki/`.

## What mwe-mcp does today

mwe-mcp gives an LLM agent a persistent, structured memory it reaches over
MCP. In one server:

- a **filesystem-SSOT memory model** — Obsidian-native markdown on disk,
  with a reconstructible `engine.db` index alongside;
- **per-fragment ACL** — every marked region of a page carries its own
  owner / audience / sender, authoritative in the DB (`fact_index`; the
  inline marker carries only the region key), redacted per reader at
  render time;
- **four hard-coded actor wiki kinds** (`wiki-user`, `wiki-group`,
  `wiki-companion`, `wiki-root`) created from internal logic, plus a
  per-wiki smart flag (`smart: bool`, legacy alias `companion:`) — the
  page *shape* is decided per-fact (validity,
  physical form, writing style), not by a registry of types;
- a deterministic **write side** (capture / supersede / forget / link with
  dedup) and a **hybrid read side** (lexical + semantic + wikilink
  multi-hop) — both LLM-free on the per-turn hot path;
- one conversational entry point, **`wiki_ingest_message`**, where a single
  internal-LLM call classifies and routes each message;
- a **nightly REM cycle** that self-reorganises the memory (dedup,
  promotion, archival, hub regeneration);
- **smart wikis** for smart consumers (authoritative writes, a
  `_briefing.md` handoff channel, cooperative leases, an op-log with
  revert);
- a built-in **dashboard** — identity console, memory views, an agentic
  chat that *operates on* the memory, and admin LLM config.

Every page describes what is actually built; the `status:` badge in its
frontmatter marks the maturity of that area.

Status legend used in page frontmatter:

| Status | Meaning |
|---|---|
| `scaffold` | Stub code: design known but not implemented. |
| `partial`  | Some behavior implemented, the rest stubbed. |
| `implemented` | Feature complete, may still evolve. |
| `stable`   | Settled, no near-term changes expected. |

## How to navigate

If you are an AI agent maintaining this wiki, start at
[`wiki-lookup-guide.md`](wiki-lookup-guide.md) — it documents the
navigation rules and the conventions for adding/updating pages.

If you are new, read in this order:

1. [The memory model](concepts/memory-model.md) — the conceptual front
   door: what mwe-mcp is, the four product pillars, owner-vs-sender
   attribution, the filesystem-SSOT principle.
2. [Identity & ACL](concepts/identity-and-acl.md) — users, groups,
   single-admin, block-level access control, derived wiki visibility.
3. [Runtime topology & cost model](architecture/runtime-topology.md) —
   the four actors, the two-LLMs/two-bills model, who pays for what, the
   LLM-free hot path.
4. [Architecture overview](architecture/overview.md) — the crates, the
   `mwe-core` module catalog, the dashboard routes, the storage floor.
5. [Build & run](development/build-run.md) — cargo commands, the CLI
   roster, the workdir layout, the `mwe-mcp.env` loader.
6. [MCP tools](protocol/mcp-tools.md) — the public MCP surface by family,
   with per-tool shipped status.

## Section index

### `concepts/`
- [memory-model.md](concepts/memory-model.md) — the four pillars (wiki
  not vector store, block-level ACL, Wiki-as-Component, the agent-as-user
  persona), owner/sender attribution, the region-level fact model,
  typed regions, the media catalog, supersedence chains.
- [identity-and-acl.md](concepts/identity-and-acl.md) — the identity
  model (single-admin, account-vs-identity, id-rules), block-level ACL,
  the three privilege levels, derived wiki visibility (a wiki/page
  surfaces iff the reader can read ≥1 fact in it), per-wiki `shared_with`.

### `architecture/`
- [overview.md](architecture/overview.md) — crate map, the `mwe-core`
  module catalog, the dashboard route + module tables, the storage floor
  (db / lockfile / wal), the migration roster (deferring the annotated
  ledger to `engine-db-and-migrations.md`).
- [runtime-topology.md](architecture/runtime-topology.md) — the dynamic
  view: the four runtime actors, the two-LLMs/two-bills invariant, the
  "who pays for what" cost matrix, the hot-path-is-LLM-free rule, the
  `wiki_ingest_message` and `dashboard_link` flows, the nightly REM cycle.

### `development/`
- [build-run.md](development/build-run.md) — cargo workspace commands +
  the CLI sub-command roster + workdir layout + the `mwe-mcp.env` loader.
- [conventions.md](development/conventions.md) — formatting, linting,
  MSRV, no-unsafe, the lockstep doc rule.
- [agents-bridges.md](development/agents-bridges.md) — the in-repo home of
  the host adapters (`agents-bridges/`): compat manifests, the two-tier
  smoke harness, the non-blocking CI + weekly upstream canary, and the
  shipped hermes bridge (plugin pair, zero fork).

### `protocol/`
- [mcp-tools.md](protocol/mcp-tools.md) — the exposed MCP tool surface by
  family, with per-tool shipped status. The canonical roster and count
  live here (anchored to `schemas::all_tools()`).
- [tool-reference.md](protocol/tool-reference.md) — the full per-tool I/O
  contract (input/output shapes, error enums) for every MCP tool, the
  `_internal.*` library APIs, and the dashboard agentic-chat tool subset.
- [config-schema.md](protocol/config-schema.md) — the complete
  `mwe-mcp.config.yaml` schema as parsed by `config.rs`: logging, the LLM
  function slots + profiles + backends, embedding, the REM policy knobs,
  the (parsed-but-inert) budget section.

### `examples/`
- [scenarios.md](examples/scenarios.md) — end-to-end worked
  scenarios (a lista-style capture, a dated commitment with a validity
  window, multi-user ACL cross-attribution, REM 3-stage promotion,
  VSCode/Claude-Code smart-wiki, multi-tenant) showing the
  external-consumer vs internal-mwe-mcp split.

### `design-notes/`
- [capture-and-dedup.md](design-notes/capture-and-dedup.md) — write-side
  flow: `wiki_capture/supersede/forget/link` orchestration, jaccard
  6-gram dedup, embed→dedup→write→insert (`mwe-core::capture`).
- [reindex-pipeline.md](design-notes/reindex-pipeline.md) — filesystem
  watcher → `fact_index` consumer: per-file diff (insert / update /
  orphan / file-removed), idempotent safety-net sweep.
- [recall-pipeline.md](design-notes/recall-pipeline.md) — read-side
  orchestrators: `wiki_search`, `wiki_facts_for`, `wiki_recall`,
  `wiki_navigate`, plus `wiki_multi_hop_facts` (`mwe-core::recall`).
- [media-pipeline.md](design-notes/media-pipeline.md) — photos, video,
  audio and documents as memory: the `media_catalog` twin of
  `fact_index`, the content-addressed store, the `/media` byte pair,
  ingest attachments + vision, dashboard rendering, export bundling.
- [ingest-pipeline.md](design-notes/ingest-pipeline.md) — the flagship
  `wiki_ingest_message`: recall → enumerate wikis → single LLM call
  returning strict JSON plan → route to capture / recall snippet /
  structural dashboard hint / skip (`mwe-core::ingest`).
- [document-ingest.md](design-notes/document-ingest.md) — long-form
  content that is not a turn: the disposition dial (consult / dossier /
  dissolve), the async checkpointed job behind `wiki_ingest_external`,
  deterministic segmentation, map/reduce extraction, `source_ref`
  provenance.
- [narrative-buffer.md](design-notes/narrative-buffer.md) — the captures
  buffer: for a standard wiki, ingest stages the classified
  claim in the per-wiki `_captures.md` journal (durable SSOT) +
  `capture_buffer` rebuildable index instead of the published `.md`;
  the wiki families (standard / structured / smart); id
  stability into the future fact (`mwe-core::capture_buffer`).
- [narrative-compiler.md](design-notes/narrative-compiler.md) — the
  compilation planner: the five-stage topology pass (Fonditore →
  Cartografo → Conciliatore → Architetto → incremental orchestrator) that
  turns promoted facts into a hub→leaf `CompilationPlan` (one-fact-one-page,
  stable `fact_id`s, fixpoint GC, dirty set via `page_fingerprint`),
  persisted as a rebuildable cache under `wikis/_plan/` (`mwe-core::planner`,
  prompts `cartografo`/`conciliatore` in `prompts::BUNDLED_*`).
- [rem-cycle.md](design-notes/rem-cycle.md) — the nightly REM cycle
  orchestrator and its sub-jobs (the roster is `rem::run_cycle`).
  Per-step journaling on `rem_ops_log`; LLM transport failure is fatal.
- [llm-functions.md](design-notes/llm-functions.md) — the model of the
  configurable internal LLM: the active function slots, the deployment
  profiles, the language policy, the robust-parser strategy.
- [logging.md](design-notes/logging.md) — the two-level filter (`info`,
  `debug`) controlled by `mwe-mcp.config.yaml`, with `RUST_LOG` overrides.
- [marker-grammar.md](design-notes/marker-grammar.md) — the full
  `{{…}}…{{/}}` marker grammar (EBNF + parse semantics) as implemented by
  `mwe-core::parser` (zero regex).
- [redaction-policy.md](design-notes/redaction-policy.md) — the
  `can_read` ACL predicate and the `render_for_sender` region-by-region
  projection (callout for regions, silent drop for prose/embeds).
- [single-writer-lockfile.md](design-notes/single-writer-lockfile.md) —
  why `mwe-core::lockfile` relies on the kernel's advisory lock.
- [applicative-wal.md](design-notes/applicative-wal.md) — the journaling
  protocol behind `proposal_ops_log` and `rem_ops_log` that makes
  structure-proposal apply and REM cycles crash-recoverable.
- [proposal-apply-engine.md](design-notes/proposal-apply-engine.md) — the
  `structure_proposal` chassis + the shipped kind handlers (`wiki_promote`,
  `dedup_merge`); `bundle` remains not-yet-implemented.
- [scope-change.md](design-notes/scope-change.md) — the hierarchical wiki
  move (`mwe-core::scope::wiki_change_scope`): filesystem rename + DB
  rebase + parent children sync, `wiki_id` stable.
- [engine-db-and-migrations.md](design-notes/engine-db-and-migrations.md) —
  the full `engine.db` DDL, the `_meta.md` schema, and the annotated
  migration ledger (`mwe-core::db`).
- [backup-and-dr.md](design-notes/backup-and-dr.md) — the workdir
  snapshot as the unit of backup (`mwe-mcp backup`): hot `VACUUM INTO` +
  file copy, the DB-before-files skew rule, the restore procedure.
- [enrollment-loader.md](design-notes/enrollment-loader.md) — the
  `mwe-core::enrollment` identity validator + DB mirror behind the
  dashboard's user/group CRUD (`validate`, `mirror_to_db`, `groups_for`).
- [setup-and-identity.md](design-notes/setup-and-identity.md) — the
  first-run setup wizard, the account-vs-identity split, the welcome wizard.
- [jwt-and-session-model.md](design-notes/jwt-and-session-model.md) — the
  unified JWT shape for MCP and dashboard, the "token = identity"
  contract, `X-MWE-Act-As`, middleware behaviour for `/mcp/*` and
  `/dashboard/*`.
- [web-agent-oauth.md](design-notes/web-agent-oauth.md) — the inbound
  OAuth 2.1 authorization server (`webagentoauth`) that lets a bridge-less
  web MCP client (claude.ai) connect as the user's own smart consumer:
  discovery/DCR/authorize/token, the dedicated per-agent wiki, the
  mirror-less skill, and the `Web` tool profile.
- [dashboard.md](design-notes/dashboard.md) — the canonical dashboard
  reference: routes, sliding-TTL session, single-admin invariant, what
  ships today.
- [dashboard-frontend.md](design-notes/dashboard-frontend.md) — the
  user-facing HTML/CSS/JS layer: the phosphor-terminal Tailwind surface,
  page anatomy, the responsive contract, the chat-panel state machine.
- [dashboard-memory-mvp.md](design-notes/dashboard-memory-mvp.md) — the
  memory slice of the dashboard: state extension, wiki view, proposals
  tray, omnipresent chat.
- [agentic-chat.md](design-notes/agentic-chat.md) — the registry contract
  for the dashboard chat panel: the `AgenticTool` variants (read + write),
  `MAX_AGENTIC_ITERATIONS`, system prompt structure, the `hub_writer`
  dependency.
- [admin-llm-config.md](design-notes/admin-llm-config.md) — the admin-only
  LLM config editor at `/dashboard/admin/llm-config`: the LLM-slot table
  with atomic YAML save, and the API-key panel via `env_file::write_key`.
- [mcp-dispatcher.md](design-notes/mcp-dispatcher.md) — the `mcp`
  dispatcher: the rmcp `ServerHandler` impl + per-tool handlers, JWT
  bearer middleware, per-call audit row, error-class mapping, WAL recovery.
- [smart-wikis.md](design-notes/smart-wikis.md) — smart wikis
  + smart consumers: the `wiki_admin_*` authoritative-write surface, the
  `_briefing.md` handoff channel, leases, the op-log (extended to all
  wikis) with revert under a strict conflict policy, the `/cite/<bi_id>`
  resolver, and inline anchored comments via `target_cite`.
- [why-rust.md](design-notes/why-rust.md) — why rmcp + axum + sqlx +
  rust-embed and not the Node/TypeScript alternative.

---

This wiki aims to cover the full current state of the code — the data
model, the runtime behaviour, the protocol surface, the configuration,
and worked examples. Forward-looking work (roadmap, planning, the
decision log) is maintained privately and lands here as it ships. The
frozen historical corpus is kept out-of-repo in the gitignored
`road-behind/` archive.
