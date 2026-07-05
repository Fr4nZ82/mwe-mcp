---
title: mwe-mcp roadmap — remaining work
status: living
---

# mwe-mcp roadmap

The single forward-looking list: **only what is still to build.** What already works is
documented in the engineering-wiki pages — each page's `status:` badge marks its maturity.
Completed work is not repeated here.

Each numbered item is one area and links to its detail page under [`planning/`](planning/).
Every listed sub-step is open by construction (partial steps are marked inline): finished work
is pruned from the list as soon as the per-area pages document it, with the trail in
[`logs.md`](logs.md). The order is by priority and dependency, nearest-term first.

## 4. First-consumer cutover — [details](planning/4_first-consumer-cutover.md)

> The integration itself is **live**: multiple real consumers run against prod over HTTP daily —
> the hermes bridge (standard-conversational) plus claude.ai web and Claude Code as Smart
> consumers — and the concurrency machinery (single-writer lockfile with Drop cleanup,
> applicative WAL, race detection) ships, so the multi-consumer soak is the real traffic itself.
> The prior-system shadow period was overtaken by events (the prior system was retired before
> cutover). Current state: [agents-bridges.md](development/agents-bridges.md) ·
> [web-agent-oauth.md](design-notes/web-agent-oauth.md).

- [ ] 4b — Validate the end-to-end per-user notice flow (page mass → emersion → recipient → magic link) *(partial — emersion and notice emission run live; the consumer-push delivery leg is 3j)*
- [ ] 4d — Admin-only recovery surfaces *(partial — the on-demand snapshot/Backup console and the manual-REM Dream trigger ship; remaining: daily automatic snapshots, dashboard restore, safe reset with auto-safety snapshot)*
- [ ] 4f — Performance tuning after profiling (batch embedding, vector index, metadata caching, incremental reindex)
- [ ] 4h — Author operational docs (troubleshooting, backup/restore, disaster recovery, capacity)

## 41. Recall block v2 — role-labelled sections — [details](planning/41_recall-block-v2.md)

> Opened 2026-07-05 (maintainer, from reading a real injected block verbatim — recall trace 54).
> The composer rework **landed 2026-07-05** (41a–41f + 41h): role-labelled sections in canonical
> order (WHO YOU ARE → WHO IS SPEAKING → YOUR RECENT HISTORY WITH THIS USER → RELEVANT MEMORY →
> NAVIGATED PAGES → UPCOMING), whole-bullet fitting replacing the flattening `truncate`, the
> self-labelled YOUR RULES field, rules.md out of the navigable set, exclusive partner tag for
> the history slot. Current state:
> [ingest-pipeline.md §recall block](design-notes/ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate).

- [ ] 41i — Live watch after the next deploy: tune the per-section budgets on real traces,
  verify the unflattened rules channel and the summary-fed identity sections

## 42. Behaviour-rule scopes — the cross-consumer rule — [details](planning/42_behaviour-rule-scopes.md)

> Ruled 2026-07-05 (maintainer): behaviour rules come in three types — agent-wide (the
> consumer's wiki, shipped), per-user-per-consumer (the consumer's wiki, referred to the user,
> shipped), and **per-user-for-all-consumers** (the user's identity wiki — unbuilt). This group
> builds the third leg: the classifier's 3-valued scope axis, the write into the sender's own
> `rules.md`, and the rules channel reading the union. The 41g ACL sweep executed the same day
> (owner = the page's user, sender = the agent; 9 prod rows) — trail in [logs.md](logs.md).

- [ ] 42a — Classifier scope axis: `agent_wide` | `per_user` | `user_global` (wording is the signal)
- [ ] 42b — Write side: `user_global` routes to the sender's identity-wiki `rules.md`
- [ ] 42c — Read side: the rules channel unions the third source into `YOUR RULES` (order pinned)
- [ ] 42d — Docs lockstep

## 5. Public release — [details](planning/5_public-release.md)

- [ ] 5a — Comprehensive documentation (getting-started, integration guides, API reference, FAQ)
- [ ] 5b — Multi-arch Docker image with health endpoint and a compose example
- [ ] 5c — Examples bundle (chat-bot starters, a Claude Code skill, a VSCode prototype)
- [ ] 5d — Observability stack (metrics, optional tracing, structured logging, health) *(partial — structured two-sink file logging ships; metrics, trace export, and a machine-readable `/health` endpoint remain)*
- [ ] 5e — Formal testing strategy (coverage target, integration/E2E/load suites)
- [ ] 5g — GDPR/privacy tooling (forget-user, export-user, retention) *(partial — per-fact `mark_forgotten` tombstones, the `gdpr_erasure` audit reason, and event/lease retention ship; a forget-user cascade and export-user remain)*
- [ ] 5h — Security hardening of the token layer *(partial)* and the dashboard
- [ ] 5i — Dashboard i18n + language selector
- [ ] 5j — Cost guardrails (hard-stop budget, per-feature tracking, alerts) *(partial — per-call `cost_estimate` capture on each audited LLM call ships; the hard-stop budget and alerts remain)*
- [ ] 5k — Versioning + migration policy *(partial — SemVer + the pre-1.0 breaking-change policy are declared and migrations are versioned/embedded; a dedicated policy + upgrade-path doc remains)*
- [ ] 5l — Repository hygiene files (issue/PR templates, CONTRIBUTING, SECURITY) *(partial)*

> The **dashboard-first operator surface** and the **release/distribution pipeline** landed:
> `release.yml` builds prebuilt `--features local-embedder` binaries on a `v*` tag (three targets)
> with per-asset `.sha256`, and `install.sh`/[`INSTALL.md`](../INSTALL.md) are the download-and-run
> path. Current state: [build-run.md](development/build-run.md), [dashboard.md](design-notes/dashboard.md).

## 14. Production operationalization — cross-platform forms — [details](planning/14_production-operationalization.md)

> The **dev/prod split**, the **dedicated-user startup gate** (with interactive systemd
> provisioning, `network-online` ordering baked in), the **`mwe-mcp.service`** unit, the
> **optional KDE tray** (`mwe-mcp-tray`), and the **runtime housekeeping sweeps** all shipped on
> Linux. Current state:
> [runtime-topology.md §10](architecture/runtime-topology.md#10-the-trust-boundary-is-the-host-not-the-protocol)
> · [build-run.md](development/build-run.md)
> · [web-agent-oauth.md §housekeeping](design-notes/web-agent-oauth.md#housekeeping). What
> remains is the cross-platform forms.

- [ ] 14e — **Cross-platform tray (macOS, Windows) — required for v1.0** (maintainer, 2026-06-14).
  `ksni` is Linux-only; revisit the library when this lands.
- [ ] 14f — **Windows/macOS production equivalents** of the dedicated-user gate + dev/prod split +
  service supervision. **Prod only.**

## 18. Bundled local embedder (Candle) + configurable embedding backend — [details](planning/18_bundled-embedder.md)

> **Landed** — the binary bundles a local Candle (CPU) bge-m3 embedder, release artifacts build
> `--features local-embedder`, and the backend stays configurable (`ollama | bundled | openai`).
> Current state: [config-schema §embedding](protocol/config-schema.md#embedding) ·
> [reindex-pipeline](design-notes/reindex-pipeline.md). Only the optional GPU build remains.

- [ ] 18f — *(optional, deferred — maintainer 2026-06-22)* GPU opt-in build (Candle CUDA feature; CPU stays the default artifact) + document the Blackwell `sm_120` caveat. Not on the critical path: CPU bge-m3 is ~80 ms/short message, fine for the hot path; revisit only if a GPU embedder is actually wanted

## 11. Organic forgetting — [details](planning/11_forgetting.md)

> Opened 2026-06-11 (maintainer, while deciding the registry twin for consumed list items):
> forgetting is a feature, not a bug —
> after months or years a memory should not retain everything at full resolution. Half the
> skeleton exists (per-fact recall tracking; the REM archive detector emits whole-page proposals
> but nothing approves or applies them). Month/year-scale windows are not observable on a short
> corpus, so this group follows real longitudinal traffic.

- [ ] 11a — Decide the decay model (recommendation on the detail page: graduated compression — aged detail condenses into gist prose, gist archives, deletion stays operator/GDPR-only)
- [ ] 11b — Build the registry-page expiry (the "scadenza" half of the registry-twin decision — the routing shipped 2026-06-11, the expiry is this group's first concrete consumer)
- [ ] 11c — Build the archive reaper: approval view + apply step for the existing `archive_proposals` emitter
- [ ] 11d — Extend the decay signals beyond whole-page inactivity (closed validity windows, recall counts, age) and set the act-first vs operator-gated line per tier

## 15. Self-correcting REM — hindsight recall-failure repair — [details](planning/15_self-correcting-rem.md)

> Opened 2026-06-20 (maintainer, from a design conversation): REM already *maintains*
> the memory but never learns from its **own recall failures**. This group mines real
> traffic for recall misses — primarily the judge-free "user restated a fact REM held
> but didn't surface" signal from the ingest dedup hit — classifies each by
> root cause, and applies the **lowest-blast-radius** repair (re-file/alias act-first;
> a rule/prompt change only via an operator review queue). **Every** proposed fix is
> gated by replaying the
> [recall-eval gold set](design-notes/recall-pipeline.md#the-recall-eval-harness--recall_eval):
> commit only if coverage improves with no regression — the judge is the objective gold
> regression, not an LLM opinion. The deterministic, gold-gated precursor to the
> RL-learned policies of group 9b; sibling to organic forgetting (11). **The core loop
> landed 2026-07-05** (15a–15f: hindsight log, judge-free miss detector on both dedup
> surfaces, the `recall_gate` scratch-replay harness, the gated re-file sub-job with
> the `recall_tuning_proposed` operator queue, and the gold-candidates producer —
> current state:
> [recall-pipeline.md §hindsight log](design-notes/recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal)
> · [rem-cycle.md §recall-repair](design-notes/rem-cycle.md#recall-repair-sub-job--self-correcting-rems-repair-stage)).
> The gold set itself is still dogfood-seeded: candidates accumulate at
> `recall-gold-candidates.yaml`, the operator merges them into `recall-gold.yaml`.

- [ ] 15g — Optional LLM hindsight pass (wasted-slot / wrong-fact), strong slot, proposal-only, behind a resource cap — only if the judge-free signal proves insufficient (measure on the miss table + repair reports first)
- [ ] 15h — The alias/topic repair rung (a receipted topics-edit primitive) + same-wiki page moves, folding into the 15c gate like the re-file rung

## 38. Structural aftercare — topology repairs — [details](planning/38_structural-aftercare.md)

> Succession pointers (38a) and the husk-page GC (38b) landed 2026-07-02; the ex-group-32
> live-outcome watch executed 2026-07-05 on prod (identity indexes clean, dups resolved, drain
> complete) and its two non-converging expectations became the **38c repair set — landed
> 2026-07-05** (cadence-aware re-open consumption, dangling-parent heal, leaf→hub
> normalisation, the topology + oversized reviewer nominations, registry staleness GC,
> Cartografo container rule) together with the 4j emerged-index foundation node. Current state:
> [narrative-compiler.md](design-notes/narrative-compiler.md#the-reviewer) ·
> [rem-cycle.md](design-notes/rem-cycle.md). Sibling to 11/15.

- [ ] 38d — **Live-outcome watch** on the fulls after the next deploy (the whole 38c repair set
  + the 4j emerged-index absorption landed in-tree 2026-07-05): `cucina` drains and flips to
  hub, the 46-fact dossier leaf folds into the emerged index and splits by content, the
  dangling `parent_hub` and the stale registry `matteo` entry heal at the first build, the
  refile↔review ping-pong converges, the husk backlog drains past its revert windows
  (checklist in [planning/38](planning/38_structural-aftercare.md))

## 39. Engineering-wiki + docs realignment (2026-07-02 audit) — [details](planning/39_engineering-wiki-realignment.md)

> The audit punch list executed 2026-07-02 (migrations ledger + DDL, run_cycle mirrors → pointers,
> code-comment repointing, the broken-anchor cluster, history-narration purge, Italian on
> English-mandated surfaces, companion→smart residue, roadmap nits — trail in
> [logs.md](logs.md)). Only the README re-baseline remains, gated on a maintainer product call.

- [ ] 39g — README/INSTALL re-baseline — **decided 2026-07-02: (b) minimal falsehood patch now, full product-voice rewrite when the mwe-co distribution/messaging lands.** Patch done 2026-07-02 (Obsidian claims → dashboard-as-human-surface, `cargo audit` → `cargo deny`, private-repo Contributing/Releases voice; license section deliberately untouched — that call belongs to the full rewrite). Remaining (gated on mwe-co): the product-voice rewrite

## 16. Claude Code subscription login (test/personal tooling) — [details](planning/16_claude-code-login.md)

> Mostly shipped: the engine (`mwe_core::oauth`), the `AnthropicBackend` OAuth path, the
> startup store install + boot-health leniency, and the dashboard **"Log in with Claude Code"**
> panel (seamless loopback + out-of-band paste) are documented as current state in
> [config-schema.md](protocol/config-schema.md#anthropic-claude-code--oauth-auth). This group
> tracks only the residue. **Test/personal only** — a deployed product brings its own keys.

- [ ] 16a — Live-verify the seamless loopback redirect (custom callback path) against Anthropic's OAuth client; if rejected, mount an accepted path or keep paste-only
- [ ] 16b — Refresh-failure re-login UX (a revoked/expired refresh token fails slots at runtime, not as a "log in again" prompt)
- [ ] 16c — Authenticated route integration test (`start` seamless-302 vs manual page, callback/paste completion)

## 3. Per-turn context model & agent bridges — [details](planning/3_context-model.md)

> The **bridge program** (in-repo `agents-bridges/`, one directory per host framework), the
> per-turn **contract (v1)**, the **hermes bridge** (the first live consumer / dogfood dev
> deployment), and the **served-installer distribution** (a running mwe-mcp serves its own bridges)
> all shipped. Current state: [agents-bridges.md](development/agents-bridges.md),
> [INTEGRATING.md](../INTEGRATING.md), [hermes README](../agents-bridges/hermes/README.md). The
> remaining bridges + the reverse channel are **opportunistic** — built when a second host is
> actually needed, not on the critical path.

- [ ] 3f — Build the nanoclaw bridge package (per-sender client pool, prompt-builder injection, events daemon, stateless-per-turn patch guide) + reconcile the samvise-2.0 planning against the shipped contract *(cross-repo)*
- [ ] 3g — Verify prompt-cache efficiency with the volatile recall block placed after stable content *(partial — the placement discipline ships in the hermes bridge and is documented; only the empirical measurement on a live bridge remains)*
- [ ] 3j — Wire the **reverse channel** in the hermes bridge: a poll/ack daemon in the gateway service (already systemd-independent of the desktop) that drains `events_poll`, routes each notice to its `recipient_id` human, mints the `dashboard_link`, and pushes it out-of-turn (piggyback on user turns + a background tick). Declared by the v1 contract (`INTEGRATING.md` step 8) but unbuilt — today's gateway polls Telegram inbound only. Cold-initiate constraint to solve (a bot cannot message a user who never wrote first). nanoclaw's events daemon (3f) shares the shape
- [ ] 3h — Public bridge-authoring guide and the OpenClaw-compat bridge; includes re-baselining `AGENT_INSTRUCTIONS.md` (+ bundled skills) to the two-tier model — today it describes only the vanilla-MCP path, while a bridged consumer gets the mechanics from the bridge and the behavioural distillate from `system_prompt_block` *(gated on the first two bridges landing; the vocabulary half overlaps 6k)*

## 20. Claude Code bridge — [details](planning/20_claude-code-bridge.md)

> Claude Code connects as the user's own **Smart** consumer over the **`webagentoauth` OAuth flow**
> (no token; the redirect picks the profile — loopback → `Local`, https → `Web`), with
> **model-driven recall**, an **operational wiki** + the per-project wikis it authors, **3-way
> routing**, and a **`CLAUDE.md`+`AGENTS.md`** pre-existing-rules scan — all shipped (20a–f) and live
> on the deployed server. Current state: [web-agent-oauth.md](design-notes/web-agent-oauth.md),
> [agents-bridges.md](development/agents-bridges.md), [INTEGRATING.md](../INTEGRATING.md). What remains
> is the end-to-end prod dogfood.

- [ ] 20g — Live dogfood end-to-end from a **separate machine's Claude Code against prod `https://mwe.contea.casa`** (OAuth endpoints must be HTTPS, so it runs against prod, not a local `http` serve): OAuth connect → load the operational wiki → recall/capture → a project bootstrap exercising the corrected ingestion (local copy intact, `CLAUDE.md`/`AGENTS.md` scan). The served `/bridges/claude-code` page + the new behaviour are already live.

## 21. Byte/media channel for the web smart consumer — *investigate only* (no detail page yet)

> Opened 2026-06-24 (maintainer). A bridge-less web MCP client (claude.ai) can only pass JSON/text
> over the connector, so it cannot reach the media blob store (`POST /media` is HTTP, and there is no
> MCP byte-upload tool): when a user attaches an image/PDF in claude.ai, only the model's **text
> extraction** can flow in (via `wiki_ingest_external` inline) — the original file is never stored on
> the server. This is a placeholder to **investigate the possibility** of a byte channel for the web
> consumer (e.g. an MCP-side blob-ingest tool, or a guided dashboard-media upload tied to the
> connection) versus accepting text-only capture. Scope it before opening a detail page.

- [ ] 21a — Investigate whether (and how) attachment bytes could reach the media store from a bridge-less web client, and whether it's worth building vs text-only. Don't build yet.

## 27. Agent-authored memory — the consumer remembers its own turn — [details](planning/27_agent-authored-memory.md)

> **Core shipped (27a–27d):** the assistant turn is a second, special-ruled extraction source
> (`author: "assistant"`, prompt Part 12) — episodic "we discussed X", personalised advice, and
> corrections all survive, attributed to the agent (`sender=<agent>`, `owner` stays the user); the
> agent also **writes facts about itself** (`owner_id: "self"`) and every turn the recall block
> **leads with its self-context** (`recall_agent_self`). The agent wiki (the `is_agent` `_meta`
> marker) is its autobiography. Current state:
> [ingest-pipeline.md](design-notes/ingest-pipeline.md) ·
> [tool-reference §author](protocol/tool-reference.md#wiki_ingest_message).

- [ ] 27d-rem — **Deepen the emergent self.** REM consolidation already covers the agent wiki (a
  normal `wiki-user` with no agent/system-user exclusion in `dream`/`rem`), so the generic
  promote → compile → reorg pipeline consolidates its self-facts onto its index. What remains:
  **organic forgetting** (item 11) so the agent's self decays like a human's rather than only
  accreting, plus any agent-specific REM tuning that surfaces once the self-corpus grows in real use.

## 8. Cross-consumer reminder delivery — [details](planning/8_reminders.md)

> Low priority. A dated commitment is already an ordinary fact with a validity window (item 7);
> this item is only the active-fire / delivery gap on top of it — reminding when an agent is
> already in a turn is covered by the recall block's due-soon slot (group 2, shipped).

- [ ] 8a — Specify the "due-soon" surface a consumer pulls (facts with an imminent firing time)
- [ ] 8b — Decide the firing timestamp representation (distinct `remind_at` vs reusing `valid_to`)
- [ ] 8c — Define the per-user delivery-target concept and route delivery to the *current* consumer, not the creator
- [ ] 8d — Consumer-side materialization handshake (pull on session start → own scheduler) + opt-in push fallback for schedulerless consumers

## 9. Extensions (gated) — [details](planning/9_extensions.md)

> Opt-in. Activated only after the base system has been production-stable for ~6 months across
> two or more independent consumers. Nothing here starts before that gate.

- [ ] 9a — Knowledge-graph tier (predicate-typed relation queries)
- [ ] 9b — RL-learned memory policies from long-run operational data
- [ ] 9c — Cross-deployment federation of shared wiki namespaces
- [ ] 9d — Voice / multimodal native ingestion
- [ ] 9e — External calendar sync toward dated commitments (the item-8 reminder surface)
- [ ] 9f — Standalone GUI (decoupled dashboard + graph view)
- [ ] 9g — Plugin system (custom REM hooks)
- [ ] 9h — Native multi-tenancy isolation
- [ ] 9i — Local 9B recall navigator (cost/latency/privacy opt-in)
- [ ] 9j — Document-ingest extensions: import-as-pages (a document becoming its own container with
  scoped search — the oversized-manual case), server-side PDF/binary text extraction (the `text`
  seam covers it meanwhile), the `url` source, a dashboard job view beyond the completion notice

