---
title: Architecture overview — crates and modules
area: architecture
status: implemented
last_review: "2026-06-28"
---

# Architecture overview

mwe-mcp is a Cargo **workspace** with three crates. The split keeps the
memory engine reusable in non-MCP contexts and isolates the web
dashboard from the protocol layer.

```
mwe-mcp (workspace)
├── crates/
│   ├── mwe-core           library  — headless memory engine
│   ├── mwe-mcp-server     binary   — CLI `mwe-mcp`, MCP transports
│   └── mwe-dashboard      library  — built-in PWA, Axum routes
├── migrations/                       (sqlx — embedded at compile time)
├── schemas/                          (JSON Schema)
├── static/                           (PWA assets)
├── tailwind/                         (Tailwind sources)
└── tests/                            (integration)
```

## `mwe-core` (library)

The headless memory engine. Everything that is _not_ MCP transport and
_not_ web UI lives here. Other crates depend only on `mwe-core`.

The authoritative module set is the `pub mod` list in
[`crates/mwe-core/src/lib.rs`](../../crates/mwe-core/src/lib.rs) — when
this table and `lib.rs` disagree, `lib.rs` wins. Every module declared
there is covered below:

| Module | Purpose | Status |
|---|---|---|
| `acl` | `can_read` block-level ACL evaluator. Pure function. | implemented |
| `archive` | REM-emitted `archive_proposals` (schema in `engine-db-and-migrations.md`) — `emit_archive_proposal` writes one row per stale filesystem path with a `reason` string (`no_recall_hit_365d`, `no_modify_180d`); coarse idempotency probe `has_active_archive_proposal` skips dupes. Approval flow + apply step (filesystem move + `wiki_forget` cascade) are not yet wired into the dashboard archive view. Backing module for the REM `run_archive_detector` sub-job. | implemented |
| `audit` | Writer + searcher for `tool_executions` (schema in `engine-db-and-migrations.md`). `ToolExecutionInput` builder, SHA-256-hashed `args_hash`, `SearchFilters` (sender/tool/date range/result_status/top_k), `ResultStatus::{Success, Error}`. Backs the `tool_log_search` MCP tool + the dispatcher's per-call audit row. | implemented |
| `briefing` | Smart-wiki briefing channel. `notify(pool, tree, caller, req)` appends a `## From <source>` section to `_briefing.md` of a smart-family wiki + mirror row in `wiki_briefing_items` (validation: topic ≤200B, body ≤4KB, kind ∈ {observation, reasoning, external}, rate-limit 50/wiki/h). Backs the `wiki_admin_notify` MCP tool. A private `notify_append` is extracted; `notify_as_rem` bypasses the user ACL for REM-emitted findings. The three-layer classification is wrapped into typed `BriefingKind` + `list_items` + `counts_by_kind`. Citation IDs are threaded end-to-end (`target_cite TEXT NULL` column, `slug_from_heading` / `extract_anchors_from_markdown` / `compose_cite` / `parse_cite` utilities, autolink rendering). Notify ACL routes through `wiki_admin::resolve_read_access` so members of `shared_with` can notify. An `author_sender_id TEXT NULL` column + `dashboard_comment` source_kind value support dashboard-typed comments attributed to the user (NULL for REM-emitted rows). Dashboard comments aren't routed through `notify` — the dashboard comment form does a direct INSERT because `notify` enforces the smart-family gate (we want comments on every readable wiki) and the 50/wiki/h cap calibrated for REM traffic doesn't suit a human reviewer. Public `parse_bi_id(input: &str) -> Result<i64, BriefingError>` is the canonical `bi_<N>` ↔ raw integer SSOT, shared with `wiki_admin::push.mark_processed`. | implemented |
| `capture` | First write-side flow: `wiki_capture`, `wiki_supersede`, `wiki_forget`, `wiki_link`. Wraps body in canonical marker, embeds, dedups against active facts via jaccard 6-gram, atomically appends to the page, inserts into `fact_index`. WAL-wrap deferred indefinitely — the reindex pipeline reconciles orphan markers on next boot, so the crash window collapses to "fact briefly missing from `fact_index`". | implemented |
| `capture_buffer` | The pre-compilation staging area for the standard-wiki path: `wiki_ingest_message` parks each classified claim here as a buffered capture instead of writing straight to the published page, and the light dream drains it into `fact_index`. Backs the mid-range fresh-captures recall slot (`recall_fresh_captures`). | implemented |
| `comment_apply` | Applies a parked dashboard comment (an unprocessed `wiki_briefing_items` row anchored on a compiled standard page) as a fact-level edit — `correct` / `remove` / `add` against that page's facts — drained per wiki by the REM `briefing_processor` sub-job. Never re-scans the whole wiki. | implemented |
| `compiler` | Narrative prose stage — Il Cronista (the `cronista` LLM slot) + the Hub Writer. Consumes the planner's `CompilationPlan` and, per dirty page, compiles the assigned facts into cohesive prose (`compile_leaf_page`), renders a `lista`-style leaf with no LLM, or writes a hub overview citing child `[[wikilink]]`s. Runs in the dream compile pass; recall does not yet serve the compiled prose (see `narrative-buffer.md`). | implemented |
| `consumers` | `consumers` table writer + lookup (migration 0016). `register` (idempotent on `consumer_id`, mints a 32-byte hex secret on a fresh row, preserves it on refresh), `is_registered` (gate for `events_poll`/`ack`). Backs the `consumer_register` MCP tool. | implemented |
| `config` | Loader for `<workdir>/mwe-mcp.config.yaml`. Materialises `logging.level` (`info` / `debug`) and the `llm:` section — five canonical functions (`hub_writer` / `ingest` / `rem_promotions` / `rem_dedup_semantic` / `cronista`) with `LlmFunctionConfig::build_backend` (Ollama, Anthropic, Gemini, OpenAI, OpenRouter; an unknown tag rejected by `UnsupportedLlmBackend`) and env-var override `MWE_LLM_<FUNC>_<MODEL|BACKEND|API_KEY_ENV|BASE_URL>`. Cloud slots (`anthropic`, `gemini`, `openai`, `openrouter`) share the `MissingApiKeyEnv` path via the `resolve_cloud_api_key` helper — `api_key_env` absent/unset/empty errors at config-build time, never at first request; env-injectable `build_backend_with_env` for tests. Unknown top-level keys round-trip via `Config::extra` so the rest of the schema (embedding, rem, budget, …) can land without breaking parse. Malformed files are fatal at startup. | partial |
| `db` | sqlx connection pool and migration runner (WAL pragmas applied on connect). | implemented |
| `dedup` | Per-kind apply/revert handler for the `dedup_merge` structure proposal kind + `emit_dedup_merge` emitter used by the REM revisor. The handler marks the loser fact's `superseded_by` to the winner (DB op); the act-first direct path also excises the loser's on-disk region (`reindex::strip_fact_region`, best-effort), while a chassis-applied merge leaves the bytes to the light-dream retirement hygiene sweep. Inverse `fact_index::clear_supersede` reverses the row with a conditional `WHERE superseded_by = ?` guard to refuse cascading supersede chains. Two-way merge only; arbitrary N-way merges are not supported. | implemented |
| `delegations` | Runtime half of the `X-MWE-Act-As` consumer-impersonation flow. `DelegationCache` mirrors the whole `consumer_delegations` table in memory behind a 60s sliding TTL (`DELEGATION_REFRESH_INTERVAL`, matching the JWT blacklist cache) so the per-tool-call `is_allowed(consumer_id, sender_id)` probe never touches SQLite on the hot path; `refresh` is called explicitly by the dashboard delegation write paths so an admin edit propagates before the TTL. `upsert` mirrors the dashboard form write; a malformed `allowed_sender_ids` JSON array surfaces as `MalformedAllowedList` (distinct from a generic SQL error). Referential integrity is applicative — an `allowed_sender_id` pointing at a deleted user simply never matches. | implemented |
| `dream` | The single definition of *what a dream does* — composes reorg / light promotion + narrative compile so the interval scheduler, the `mwe-mcp rem` CLI, and the dashboard admin button stay in lockstep (`run_compile` is the compile-only composition). Also owns the one-line `summarize_*` outcome strings shared by the console and the scheduler. | implemented |
| `dream_journal` | Durable history of dream runs (`dream_runs` table) — both manual console runs and the scheduler's nightly / interval runs record a row here, bounded to the newest 100, so the admin Dream page shows a restart-surviving history with each run's full log. | implemented |
| `dream_light` | The light dream — the frequent, cheap half of the two-dream cadence. Drains the captures buffer: each buffered capture is exact-dup-checked, embedded, and inserted into `fact_index` as a permanent, recallable fact. | implemented |
| `embedder` | Trait `Embedder` (async, `Send + Sync`, dyn-safe via `async_trait`) + `OllamaEmbedder` over `reqwest` + `FakeEmbedder` for tests. | implemented |
| `enrollment` | Validator + atomic DB writer for `enrollment_users` / `enrollment_groups`. Invoked by dashboard CRUD handlers. Holds `locale_for(sender_id)` — the per-user default `locale` (the second of the three `locale` resolution sources). | implemented |
| `env_file` | Mutating helper for `<workdir>/mwe-mcp.env`. `write_key(path, key, value)` does parse-modify-write: an absent file is materialised with a one-line comment header; an existing one is scanned line-by-line and the first real assignment of `key` is replaced in place (commented hint lines `#…` skipped, every other line preserved byte-for-byte), else the assignment is appended. The body is committed via `wiki::atomic_write` and the file is force-chmod'd to `0o600` on unix on every write so a UI save can never widen the secret file's mode. Backs the dashboard LLM-config page's "set API key" write path. | implemented |
| `error` | Canonical `Error` enum + `Result` alias. | partial (variants minimal) |
| `events` | `wiki_events` queue writer + reader: typed `EventKind` (`dedup_proposed` / `structure_applied` / `archive_proposed` / `auto_applied` / `document_ingested` / `compile_failure_streak` / `recall_tuning_proposed`), `insert_event` with JSON payload + tracing, idempotency probe `find_recent_event_for(kind, fact_id, window)` (default window 30d, matches consumer-ack retention), and the consumer-side `poll_events` / `ack_events` pair driving the `wiki_events.acks` JSON map (per-consumer delivery state, filtering server-side via `json_extract`). | implemented |
| `fact_index` | `fact_index` DB layer: `NewFact` / `FactIndexRow`, insert + supersede + forget + recall-counter bumps + drop-by-source-path + find-active-in-wiki + structured `find_by_filters` (wiki/owner/type/date range/`topics_any` via JSON1) + LE-f32 BLOB (de)serialization. `find_active_by_source_path` + `update_region(fact_id, RegionUpdate)` serve the reindex consumer (in-place mutation preserves `created_at` + recall counters). | implemented |
| `faults` | `fault!(name)` macro + `MWE_TEST_FAULT_AT` parser for deterministic kill-points. No-op without the `mwe-mcp-test-faults` feature. | implemented |
| `ingest` | Flagship orchestrator for `wiki_ingest_message`: recall → enumerate wikis → single LLM call returning strict JSON (intent + capture plan) → route to `capture::wiki_capture` / `capture::wiki_supersede` / `recall::wiki_recall` snippet / structural dashboard hint / skip. Every soft failure (LLM transport, malformed JSON, invalid capture plan) demotes to `IntentKind::Skip` with a canned `suggested_seed`; only infrastructure failures surface as `IngestError`. `disambig_choice` follow-up — when the consumer round-trips the chosen candidate id, the orchestrator never re-asks (`needs_disambig` forced to false). `supersede_target` from the prompt v2.0 JSON output is honoured — `validate_supersede_target` guards against malformed `UUIDv7`s and against hallucinated ids not in `recalled_memory`; the capture branch dispatches to `wiki_supersede` instead of `wiki_capture` when the target resolves. | implemented |
| `jwt` | JWT issue/verify/revoke + blacklist cache. | implemented |
| `lint` | Backing for `wiki_lint` MCP tool — 8 advertised `Check` variants with `is_implemented()` flag. `MarkerMalformed` (re-runs the parser per page), `OrphanFacts` (fact_index rows whose marker is missing on disk), and `MetaInvalid` (unparseable `_meta.md`) are implemented; the other 5 (`BrokenCrosslinks`, `AclInconsistent`, `EmbedMissing`, `HubOutdated`, `SupersededChain`) advertise a zero-count summary entry and are not yet implemented. Output sorted `(severity desc, check asc, wiki_id asc, fact_id asc)` for deterministic diffs. | partial |
| `lockfile` | Single-writer advisory lockfile with kernel-released orphan cleanup. | implemented |
| `llm` | Trait `LlmBackend` + `CompletionRequest`/`Response`/`Usage` value types + chat surface (`ChatRequest`/`ChatResponse`/`Tool`/`ToolCall` for the agentic loop) + `OllamaBackend` (`/api/generate` + `/api/chat` with tool calling, `stream=false`, `think:false`) + `FakeLlmBackend` for tests. `AnthropicBackend` for the Messages API (`POST /v1/messages`) with `AnthropicApiKey` newtype (`Debug`-redacted, same pattern as `jwt::TokenSecret`), `complete`/`chat`/`health_check`, typed content blocks for tool calling (`tool_use`/`tool_result` round-trip via Anthropic-minted ids); `LlmError::Auth` (401/403) and `LlmError::RateLimit` (429) variants. `GeminiBackend` for the Google `generateContent` REST API (`POST /v1beta/models/{model}:generateContent?key=...`, key in query parameter — Gemini's only contract among the three) with `GeminiApiKey` newtype, `complete`/`chat`/`health_check`, function calling via `functionCall`/`functionResponse` parts (Gemini correlates by function name, not by id — `split_gemini_messages` reconstructs an `id → name` map from the prior assistant turn so the `ChatMessage::tool_call_id` contract stays uniform across providers). Three hard-coded policies against Gemini 3's combined thinking+output budget gotcha (`maxOutputTokens = 65_536` always, `thinkingLevel = "minimal"` always, `temperature = 1.0` mandatory — caller-supplied values are clamped at the boundary). `finishReason: "MAX_TOKENS"` surfaces as `LlmError::Backend` (retriable) rather than mapping to `FinishReason::MaxTokens`, because on Gemini it nearly always indicates a truncated unparseable output, not a legitimate end of generation. `OpenAiBackend` for the first-party Chat Completions API (`POST {base_url}/chat/completions`, `Authorization: Bearer`) — it shares the OpenAI-shaped wire types with `OpenRouterBackend` and differs where the docs say it does: `max_completion_tokens` instead of `max_tokens`, the system prompt on the `developer` role for a reasoning model, `reasoning_effort` as a bare string sent only to models that reason, prompt-cache hits read from `usage.prompt_tokens_details`. `ModelPolicy` resolves per (backend, model) what the request may carry — sampling parameters, reasoning headroom clamped to the documented output ceiling — from `model_catalog::capabilities_for` first, the hand-written family lists only for a model the catalog has never heard of, and a one-shot downgrade-and-retry when a provider 400s naming the parameter (or, on OpenAI, the message role). Powers the 5 canonical LLM functions; the `hybrid` / `all-api` profiles are unlocked when `ANTHROPIC_API_KEY` is set in `mwe-mcp.env`; Gemini slots are wireable per-slot via the dashboard editor with `GEMINI_API_KEY`. | implemented |
| `locale` | Locale plumbing for the v2.0 prompts. `render_language_directive(Option<&str>)` turns a resolved BCP-47 tag into the single-line `User locale: <tag>. Respond in <Language>.` directive the prompt's `{locale}` placeholder is substituted with; `None`/empty falls back to the "mirror the language of the user's message" clause. A small primary-subtag → English-name table covers the day-one languages; an unknown tag is surfaced verbatim rather than degraded to the mirror clause. The resolved locale comes from one of three sources in order — the `metadata.locale` on the `wiki_ingest_message` request, `enrollment::locale_for(sender_id)`, or the dashboard `SessionUser`. The memory-writing slots (page prose, page names, document summaries, date rewrites) take `render_memory_language_directive` instead: same directive body, but an undeclared locale resolves to **English** rather than to the mirror clause, since those slots see facts and not a user turn. Their locale is resolved per wiki (`memory_directive_for_wiki`, via the wiki's scope principal — a group's language only when its members are unanimous) or per submitter (`memory_directive_for_user`, document ingest). | implemented |
| `media` | Media catalog + content-addressed blob store behind `{{embed=<catalog_id>}}` keys: `media_catalog` DAL (the `fact_index` twin — ACL triple, kind/MIME/size, caption/description), `store_media` (sha256 addressing under `<workdir>/media/<aa>/<sha>`, blob-before-row write order, per-(hash, owner) idempotent dedup), server-side `catalog_id` minting with the closed English kind vocabulary (`photo`/`video`/`audio`/`doc`, producer-enforced), `row_visible_to` (`can_read` semantics, no admin bypass), monotone `widen_acl` (the fact-link union), `inline_safe` (the MIME safety call shared by both serving mounts). Backs `POST /media` / `GET /media/<id>` + the dashboard alias + export bundling + the `embed_missing` lint. Design SSOT: `media-pipeline.md`. | implemented |
| `meta_annotate` | Deterministic, no-LLM compile sub-stage that derives recall-navigation aids from `fact_index` and writes them into each wiki's `_meta.md` frontmatter — `keywords["topics"]` (sorted union of active facts' topics) and per-wiki navigation annotations — the producer side of the catalog / root-index data plane. | implemented |
| `parser` | Inline marker `{{owner=... allow=... f=... sender=...}}…{{/}}` parser. | implemented |
| `planner` | Topology stage of the narrative compiler. Turns the flat `fact_index` into a `CompilationPlan`: a hub→leaf page graph where every fact lives on exactly one page (one-fact-one-page), hubs hold only narrative + links, and a persistent `ConceptRegistry` keeps a concept page from being re-invented run-to-run. Never writes prose itself. | implemented |
| `promote` | Per-kind apply/revert handler for the `wiki_promote` structure proposal kind + `emit_paragraph_to_file` emitter. The structural variants: **`paragraph_to_file`** (default — move N facts from one page of a wiki to another page of the same wiki), **`pages_to_subwiki`** (a group of pages that are one subject becomes a new sub-wiki, each page carried under its own name; new id derived via `WikiId::child_of`), and **`pages_move_wiki`** (pages file into a sub-wiki that already exists). `file_to_subwiki` — the legacy single-page emergence — is no longer emitted but stays wired so receipts written before the change remain undoable. All preserve every `fact_id` verbatim — what changes is `fact_index.source_path` (always) and `wiki_id` (whenever a page crosses a wiki line). | implemented |
| `prompts` | Hybrid runtime loader for the operational system prompts. `extract_fenced_text(md, name, source)` parses the first `` ```text ``…`` ``` `` fenced block of a markdown document; `load(name, workdir, bundled_fallback)` returns the override at `<workdir>/prompts/<name>.md` when present, otherwise parses the `include_str!`-bundled default the caller passes. `substitute(body, vars)` does single-pass `{ident}` placeholder substitution (ascii-ident matcher → JSON literals `{"…": …}` pass through untouched; unknown placeholders are left verbatim instead of becoming the empty string; replacement values are **not** re-scanned, so a `{body}` containing a literal `{x}` cannot trigger a second round). `render(name, workdir, bundled_fallback, vars)` is the `load + substitute` sugar used by callers that interpolate runtime data. `seed_bundled_into(workdir, &[(name, body)])` + `SeedReport { created, preserved }` (with `merged(&self, &other)` for cross-crate aggregation) idempotently materialises every bundled prompt under `<workdir>/prompts/<name>.md` via `wiki::atomic_write`; an existing file is preserved verbatim so operator edits survive `mwe-mcp init` reruns and binary upgrades. `parse_default_version_at_bootstrap(md)` extracts the `default_version_at_bootstrap: vN.M` key from the YAML frontmatter — line-by-line scan, no `serde_yaml` dependency, handles quoted/unquoted scalars and tolerates leading whitespace; the dashboard editor uses it to surface a drift banner when the workdir file's seeded version differs from the bundled body's current version. `prompts::BUNDLED` and `mwe_dashboard::BUNDLED_PROMPTS` enumerate the bundled slices each crate ships — the binary chains them in `cmd_init` and `cmd_migrate` so the loader module stays unaware of which prompts live where. Same on-disk layout as the bundled `crates/<crate>/prompts/<name>.md` (frontmatter + fenced block) so engineering wiki, operator's edit experience, and loader share one parse. The shipped prompts are overrideable from the workdir: static — `ingest` (`crates/mwe-core/prompts/ingest.md`, wired in `ingest::wiki_ingest_message`) and `agentic-chat-panel` (`crates/mwe-dashboard/prompts/agentic-chat-panel.md`, wired in dashboard `chat::agentic_submission`); dynamic — `rem-dedup` (`{new}`, `{old}` — wired in `rem::run_revisor_jaccard`), `rem-promotions` (`{body}`, `{page_facts}`, `{recall_hits}` — wired in `rem::run_auto_promote`). `PromptError` propagates via `IngestError::Prompt` and `RemError::Prompt` (a malformed override surfaces loudly rather than silently falling back to bundled). Hot-reload is not implemented (per-call read, ~10KB filesystem read = microseconds vs LLM call). The admin-only editor `/dashboard/prompts` ships status badges, atomic save with `.bak` backup, reset-to-bundled, and the drift banner against `default_version_at_bootstrap` (see [`crates/mwe-dashboard/src/routes/prompts.rs`](../../crates/mwe-dashboard/src/routes/prompts.rs) + the dashboard table below). | implemented |
| `proposals` | Chassis for `structure_proposals` (schema in `engine-db-and-migrations.md`): reader (`list` with `ListFilters { status, kind, top_k }`, `ProposalStatus::{Pending, AppliedPendingConfirm, Applied, Reverted, Expired}` — 5 stati), state machine (`apply_proposal` (manual) / `auto_apply_proposal` (sweep) / `confirm_proposal` / `revert_proposal` with `RevertAuth::{Token, Caller}` dispatch, all atomic conditional `UPDATE … WHERE status=<prev>`), `revert_token` UUIDv4 + `REVERT_WINDOW = 7d`, two sweeps (`auto_apply_overdue_proposals` emette `AutoApplied` event; `auto_finalize_unconfirmed_proposals` flippa silente a `applied` quando `confirm_deadline` scade, no `revert_token`, no event), expire fallback (`expire_overdue_proposals` grace-gated 24h), `emit_proposal` helper. Dispatches to per-kind handlers in `promote` / `dedup`; `kind::BUNDLE` is the only kind that surfaces `KindNotYetImplemented`. The structural rungs (`wiki_promote` both variants) are **act-first** since the apply-and-notice conversion: REM applies directly and records a born-applied receipt via `emit_applied_proposal` — only `dedup_merge` still enters `pending`. No MCP exposure: consumers learn of applied changes from the `structure_applied` event; the write actions (`apply` / `confirm` / `revert`) are **dashboard-only** (the dashboard calls these functions directly without going through MCP). | implemented |
| `recall` | Two layers in one module. Pure helpers: jaccard 6-gram (`ngrams`/`jaccard_sets`/`jaccard_6gram`, used by capture dedup) + `cosine_similarity` + the wikilink parsers of the canonical link grammar: `extract_wikilinks` (`pub` — parses `[[wiki_id]]` / `[[wiki_id/page-slug]]` / `[[target|display]]` into structured hops, alias stripped; consumed by `recall_nav` for wiki- and direct page-hop candidates) and its wiki-granular projection `extract_wikilink_wiki_ids` (consumed by `wiki_multi_hop_facts` + `rem::run_backlink_reciprocity`). Async orchestrators: `_internal.wiki_search` (top-K cosine + ACL post-filter + recall-counter bump), `wiki_facts_for` (structured SQL view, no counter bump), `wiki_recall` (stable LLM-ingest call site). A dedicated vector index (`sqlite-vec`) is not yet used — search is a full-scan cosine. The deep navigator lives in the sibling [`recall_nav`](#mwe-core-library) module. | implemented |
| `reindex` | Consumer of [`watcher`](#mwe-core-library): `reindex_file` diffs a single page against `fact_index` (insert / update / orphan / file-removed buckets), `reindex_full` walks every wiki for the 5-minute safety net; both idempotent. `run_watcher_loop` / `spawn_watcher_loop` / `spawn_safety_net_loop` wire the pipeline into `mwe-mcp serve`. Per-file embed cost is bounded — re-embed only when body changes, ACL-only edits skip the embedder. Tombstone reasons `filesystem_marker_removed` and `filesystem_file_removed`. | implemented |
| `rem` | Nightly cycle runner. **`run_cycle` is the SSOT for the sub-job roster and their fixed order — read its body**; the per-sub-job contract lives in the dedicated REM cycle design note. Shape of the cycle: the proposal sweeps settle overdue `structure_proposals` first, the consolidation/hygiene write-jobs reorganise the fact set act-first, the smart-wiki-aware read-jobs post observations to the owner's `_briefing.md` (`briefing::notify_as_rem`, capped by `briefing_notify_cap` and deduped by `briefing_dedup_window`), and `hub_writer` regenerates `index.md` last so its prompt sees a stable state. (Expiry is per-fact `valid_to`, with no reminder events by default.) The write-jobs skip smart-family wikis (the smart consumer owns those writes) via the cycle-scoped `SmartWikiIndex` loaded once by `load_smart_wiki_index` (one tree walk reading each `_meta.md`'s per-wiki smart flag, keyed by `wiki_id`, no registry query); every non-smart wiki is **standard**, and the briefing-processor sub-job drains its parked comments as contained fact ops (`mwe_core::comment_apply`). Every state-mutating sub-step journaled on `rem_ops_log`; soft errors aggregated per sub-job; only infra failures bubble as `RemError`. LLM transport failure is **fatal**. | implemented |
| `render` | `render_for_sender` — redaction policy applied region-by-region. | implemented |
| `reviewer` | Deterministic, zero-LLM post-compilation QA. After the compiler runs, checks invariants over the `CompilationPlan` and the compiled bodies (empty leaf, duplicate fact home, …) and surfaces findings; non-blocking — never mutates the corpus. | implemented |
| `scope` | Hierarchical move primitive: `wiki_change_scope` re-parents a wiki (and its subtree) by validating the target (cycle / self / `scope: inherit` against new root), `fs::rename`ing the directory atomically (same filesystem), rewriting `_meta.md.parent_wiki_id` on the moved wiki, syncing both parents' `children` denormalised lists, and rebasing `fact_index.source_path` for every row inside the moved subtree via `fact_index::rebase_source_path_prefix`. `wiki_id` stays stable per the `modello-memoria.md` invariant, so existing `[[wiki_id]]` / `[[wiki_id/page]]` cross-links continue to resolve — the cross-link rewriter is a no-op here. The move is not wrapped in the applicative WAL today. | implemented |
| `skills` | Server-served skill catalog. Bundled `.md` files in `crates/mwe-core/skills/` (the SSOT for the roster and each skill's `version`, read from its own frontmatter), embedded via `rust-embed` (bundled skills only). The per-skill roster is documented in the `skill_list` row of [`mcp-tools.md`](../protocol/mcp-tools.md). API: `Skill { name, version, description, depends_on, etag, source: Bundled }`; `list_bundled()` / `fetch(name)` (bundled lookup) / `fetch_bundled(name)` (public, used by HTTP endpoint). `etag = sha256(content)[..32]` (16 bytes, 32 hex chars) surfaced to both MCP `skill_list`/`skill_fetch` and HTTP `/skills` for cache short-circuits. Lenient frontmatter parser (malformed → empty metadata, never hard-error) so a buggy body doesn't take the whole catalog down. Backs the `skill_list` + `skill_fetch` MCP tools plus the HTTP `/skills` + `/skills/<name>.md` endpoints in `mwe_mcp_server::http_skills`. | implemented |
| `slug` | NFKD slug pipeline with collision suffix `°N` capped at 99. | implemented |
| `smart` | Smart-consumer bootstrap helpers backing the smart-consumer MCP family. `bootstrap(pool, tree, caller, req) -> BootstrapResponse` surfaces every smart-family wiki the caller owns (filter via each `_meta.md`'s per-wiki smart flag — no registry probe — + `tree.resolve_scope_principal(meta) == User(caller.sender_id)`), aggregating `briefing::counts_by_kind` + `briefing::list_items` (pending only, capped per request) + latest `wiki_admin_op_log` row per wiki; sorts `(matches_project_hint desc, last_op_log_ts desc, wiki_id asc)`. `recall_core_global(pool, tree, embedder, caller, sender_groups, req) -> RecallCoreGlobalResponse` is a thin wrapper over `recall::wiki_search` with caller-owned filter + smart-family post-exclusion (overfetch ×4 to compensate, clamp `limit ∈ [1, 20]`). Both gated on `consumer_class=smart` (`SmartError::RequiresSmart`). `read_only` annotation on the two MCP tools. | implemented |
| `types` | Shared IDs and metadata structs. | implemented |
| `wal` | Application-level WAL for proposals and REM steps. Generic `OpInverse` trait + `NoopInverse` + `rollback_stale_proposals` / `rollback_stale_rems` recovery driver — shared shape between proposal_apply and REM, the dispatcher's startup path sweeps stale rows with `NoopInverse` (REM is restartable, proposal_apply rollback is stubbed; per-kind inverses are not yet implemented). | implemented |
| `watcher` | Cross-platform file watcher with per-file `.mwe-write-in-progress` marker filter, `WriteMarker` RAII guard, startup sweep of stale markers. | implemented |
| `wiki` | Filesystem surface I/O. `WikiTree::open/walk/locate`, `WikiHandle::read_page/write_page/list_pages`, frontmatter (de)serialization (`WikiMeta`, `MarkdownDoc`), `atomic_write` with `WriteMarker` integration, `_internal.wiki_read/write_page/list_pages/get_meta/catalog_list`. Capture/supersede/forget/link live in the sibling [`capture`](#mwe-core-library) module. | implemented |
| `wiki_admin_leases` | Opt-in cooperative lease coordinating `wiki_admin_push` across multiple smart consumers of the same owner. `acquire(pool, caller, wiki_id, ttl_sec?) -> AcquireOutcome` enforces `consumer_class=smart` → TTL ∈ `[1, MAX_TTL_SECS=300]` → "same `(sender_id, consumer_id)` extends existing row (renewed=true)" vs "different consumer ⇒ `AcquireError::WikiLockedByLease`". `release(pool, caller, lease_id)` requires both the lease id and a `(sender_id, consumer_id)` match (`IS` operator handles NULL `consumer_id`); already-released / expired / foreign rows → `NotHeldByCaller` (404). `active_lease_for(pool, wiki_id, now)` is the single SQL probe `wiki_admin::push` runs at the top of `Upsert` to short-circuit with `AdminError::WikiLockedByLease` (Create derives a brand-new id, nothing to coordinate). `expire_stale(pool, now, grace, retention)` is the REM `lease_expirer` engine — two passes (mark active-past-grace as released, delete released-past-retention). Lease id format `wal_<12-byte sha256[..12] over now-nanos + wiki_id + sender_id>` (no UUID dep). Backs `wiki_admin_lease_acquire` + `wiki_admin_lease_release` MCP tools (smart-only). Migration `0026_wiki_admin_leases.sql`. | implemented |
| `wiki_admin` | Smart-wiki authoritative writes for smart consumers. `push(pool, tree, caller, req)` dispatches on `PushMode::{Create, Upsert}`: `create` derives a fresh `wiki_id` via `WikiId::child_of`, stamps `scope = User(caller.sender_id)` + `project_id` into `_meta.md.extra`, refuses duplicates; `upsert` writes/deletes pages via `atomic_write` and refuses `_meta.md`. `pull(pool, tree, caller, wiki_id)` returns every page + the current `op_log_head`. Three auth gates enforced in order: `consumer_class=smart` → `wiki.owner_user == token.owner_user` (via `resolve_owner_user`; rejects `Group` / `Global` ACLs with `AmbiguousOwner`) → the wiki's smart flag in its `_meta.md`. Append-only audit row in `wiki_admin_op_log` with `payload_hash` = sha256 of canonical input (never raw content). The cross-user **read + notify** surface goes through `resolve_read_access(pool, tree, handle, caller_sender_id) -> ReadAccessOutcome`, which consults the wiki's `_meta.md.shared_with: Vec<Principal>` roster. Write tools stay owner-only. A lease gate at the top of `push` Upsert mode via `wiki_admin_leases::active_lease_for(...)` returns `423 wiki_locked_by_lease`. Upsert also enforces optimistic concurrency: a request carrying `expected_op_log_head` behind the wiki's latest `push_*` op (`pull` / `notify` rows excluded) is rejected with `AdminError::ConflictingOpLogHead` → `409 conflicting_op_log_head` before any write. `push` is parameterised over `ActorKind { SmartConsumer | Dashboard | System }`: the dashboard text editor path goes through the same API with `ActorKind::Dashboard`, relaxing the smart-family gate (op-log is unified across all wikis), populating `pre_image_json` for every Upsert. Public `op_revert(pool, tree, op_id, reverted_by)` scans the op-log for newer rows intersecting the target's pages, refuses with `RevertError::TargetChanged` (strict policy), else restores the pre-image and writes a `ActorKind::System` compensating row. `push` takes an optional `mark_processed: Vec<String>` (canonical `bi_<N>` ids) that batch-marks `wiki_briefing_items.processed_at = NOW()` atomically inside the push transaction (validation fail-fast on unknown or cross-wiki ids → `AdminError::UnknownBriefingItemId` → `400 unknown_briefing_item_id`; cap `MARK_PROCESSED_CAP_PER_PUSH = 50` → `400 too_many_briefing_items`). Backs `wiki_admin_push` + `wiki_admin_pull` MCP tools + the dashboard editor + revert button + comment write path. `snapshot_replace`, `since_op_log_id` delta-pull, and `project_id`/`administered_by_self` search filters are not yet implemented. | implemented |

Feature flag `mwe-mcp-test-faults` enables the [`faults`](../../crates/mwe-core/src/faults.rs)
module: the `fault!(name)` macro is a zero-cost no-op in production
and a deterministic kill-point in test builds. Action is configured
via `MWE_TEST_FAULT_AT=name[@abort|@panic|@sleep:Nms]` (comma-separated
for multiple points). Powers the race / recovery tests of the re-index
transactional pipeline.

## `mwe-mcp-server` (binary + library)

Produces the `mwe-mcp` CLI **and** a thin library surface
(`mwe_mcp_server::mcp`) so the dispatcher is callable from integration
tests without a transport. Its responsibilities:

1. **CLI entrypoint** — sub-commands `init`, `serve`, `token-issue`,
   `token-revoke`, `token-list`, `admin-reset`, `doctor`, `migrate`,
   and `rem` (the manual maintenance escape hatch: `rem run-cycle` /
   `run-light` / `run-compile`, each acquiring the workdir lockfile so it
   never races a running `serve`)
   ([`crates/mwe-mcp-server/src/main.rs`](../../crates/mwe-mcp-server/src/main.rs)).
   Each sub-command wires straight to the `mwe-core` modules
   (lockfile, db, jwt, wal). See
   [`development/build-run.md`](../development/build-run.md) for the
   full flag matrix.
2. **HTTP transport** — single Axum process that mounts `/mcp` and
   `/dashboard/*` on the same listener (transport contract in
   [`mcp-tools.md`](../protocol/mcp-tools.md)). `/mcp` is rmcp's
   `StreamableHttpService` running in stateless + JSON-response mode
   behind a bearer-JWT middleware
   ([`mcp::auth::jwt_auth_middleware`](../../crates/mwe-mcp-server/src/mcp/auth.rs))
   that verifies via `mwe_core::jwt::verify` (signature + `exp` +
   blacklist cache; the claim/session model is documented in
   `jwt-and-session-model.md`).
3. **MCP dispatcher** — [`mcp` module](../../crates/mwe-mcp-server/src/mcp/),
   `ServerHandler` impl exposing the public MCP tool surface. The roster
   and per-tool status live in [`mcp-tools.md`](../protocol/mcp-tools.md)
   (with the wire contract in [`tool-reference.md`](../protocol/tool-reference.md));
   `mwe_mcp_server::schemas::all_tools()` is the code SSOT for the
   registered set, so this page does not hardcode a count. See the
   dedicated dispatcher design note
   for the per-tool wiring, identity model, error mapping, and audit
   trail.
4. **HTTP media byte pair** — [`http_media` module](../../crates/mwe-mcp-server/src/http_media.rs),
   bearer-JWT Axum sub-router for the media pipeline: `POST /media`
   (multipart upload → content-addressed store + `media_catalog` row +
   minted `catalog_id`) and `GET /media/<catalog_id>` (per-media ACL
   check, then the blob streamed with a strong sha256 `ETag`). Same
   `jwt_auth_middleware` (act-as aware) as `/mcp`; the dashboard
   renders embeds through its own cookie-authenticated alias. Design
   SSOT: `media-pipeline.md`.
5. **HTTP skill catalog** — [`http_skills` module](../../crates/mwe-mcp-server/src/http_skills.rs),
   public-read Axum sub-router for the bundled skill catalog:
   `GET /skills` (JSON metadata) and `GET /skills/<name>.md`
   (raw markdown body with `ETag` + `If-None-Match` short-circuits).
   Mounted in `main.rs::cmd_serve_http` alongside `/dashboard` and
   `/mcp`. Custom skills stay MCP-only (no JWT context on the HTTP
   path → bundled-only to prevent custom-catalog enumeration).
6. **HTTP connect surface** — [`http_connect` module](../../crates/mwe-mcp-server/src/http_connect.rs),
   public-read Axum sub-router for the operator-facing onboarding
   surface. It ships the **hook bundle** sub-tree:
   `GET /connect/hooks` (JSON list of consumers) and
   `GET /connect/hooks/<consumer>.json` (full envelope with the
   `snippet` block the operator copies into their consumer's settings).
   Two templates bundled via `include_str!` from
   `crates/mwe-mcp-server/src/hooks_templates/` — `claude-code.json`
   (the single optional **token-less `SessionStart` nudge** for the
   OAuth-connected smart consumer — a fixed reminder; recall stays
   model-driven) and `codex.json` (fallback
   envelope: `status: no_native_hook_mechanism_yet`, `snippet: null`,
   pointer to the chat-level skill invocation pattern). ETag content-hash
   (`sha256[..16]`) + 304 short-circuit + `Cache-Control: public,
   max-age=300, must-revalidate`. These hook-bundle endpoints are
   the only `/connect/*` surface; consumer onboarding lives on the
   dashboard home's *Connect a consumer* card plus the *Bridges* tab
   (item 7).
7. **HTTP bridge-distribution surface** — [`mwe-dashboard::public_site_router`](../../crates/mwe-dashboard/src/routes/bridges.rs),
   a **public, anonymous** sub-router mounted at the HTTP **root** by
   `main.rs::cmd_serve_http` (next to `/cite`). It serves the product
   a slim front page `GET /` (an agent line pointing at the catalog, a
   human sign-in link), the bridge catalog `GET /bridges` +
   `GET /bridges/<consumer>` (each entry carries an *agent instructions*
   link straight to its `install.md`; the install command is tailored to
   the request `Host`), and the self-contained installers
   `GET /bridges/<consumer>/install.{sh,ps1,md}` — the bridge's plugin
   tree is embedded via `rust-embed` (from
   `agents-bridges/<consumer>/plugins/`, `__pycache__` filtered) and
   inlined into the script as heredocs / here-strings, so one
   `curl … | sh` lays everything down with no `tar`/`jq`/bundle fetch.
   Anonymous because `curl … | sh` reaches it from a box with no
   dashboard session and nothing here is secret. The **same** catalog +
   guide bodies are also mounted **authenticated** at `/dashboard/bridges`
   (the nav *Bridges* tab) via `dashboard_tab_router()`, sharing the body
   functions. The **token** lives on neither — it is issued from the
   dashboard home's *Connect a consumer* card. SSOT:
   [`agents-bridges.md`](../development/agents-bridges.md).

## `mwe-dashboard` (library)

The built-in PWA. Axum routes under `/dashboard/*`, server-side
templating with Maud, mobile-first hand-written CSS. Static assets are
embedded into the binary via `rust-embed`.

Auth uses a sliding-TTL JWT cookie that shares its shape and signing
secret with the MCP-side JWT (the claim/session model is documented in
`jwt-and-session-model.md`)
— only the TTL differs (10 minutes, re-issued on every authenticated
interaction). The session middleware fires the rule on every request
and redirects to `/dashboard/login` on any verify failure.

**Coverage** — the dashboard covers the identity console, the memory
browser, the full smart-wiki surface, the
skill catalog viewer, the onboarding *Bridges* tab + home card, the LLM-config
editor, the welcome wizard, and the admin prompt editor — all mounted.
The audit/costs pages, the PWA service worker, and the Tailwind real
build are not yet implemented (planned — see the
roadmap).

| Route | Purpose |
|---|---|
| `/` | Redirects to `/setup` / `/login` / `/home` depending on DB + cookie state |
| `/setup` | First-run wizard, gated on `enrollment_users` empty (see [`identity-and-acl.md`](../concepts/identity-and-acl.md)) — creates the single admin |
| `/login`, `/logout` | Password sign-in, session revoke |
| `/home` | Landing page with KPI counts (wikis, facts, pending proposals, MCP calls 24h, identity counts) + per-role navigation |
| `/wiki`, `/wiki/:id` | The **Standard** tab of the single "Wikis" page (one nav entry) + single-wiki view (`index.md` body + page list + meta + active-fact count); the list filters to the standard family (`_meta.md` smart flag false) so each wiki appears under exactly one tab; the Standard/Smart switch is a tab bar on the page itself (`components::tab_bar`), not a second nav link; the rendered body is projected per viewer (`render_for_sender`) and the read is gated at the wiki level by **derived visibility** (`fact_index::wiki_visible_to` — a wiki that holds facts surfaces only to a reader who can read ≥1 of them, otherwise 404; an empty wiki hides nothing so it stays visible; admin reveal bypasses), and the standard list is filtered the same way |
| `/wiki/smart`, `/wiki/:id/briefing`, `/wiki/:id/op-log`, POST `/wiki/:id/op-log/:op_id/revert`, `/wiki/:id/sharing` | Smart-wiki views. `/smart` is the **Smart** tab of the same Wikis page: filters wikis on the per-wiki smart flag (no registry query) + shows last-push (from `wiki_admin_op_log`) and unread briefing count (from `wiki_briefing_items`); `/briefing` renders `_briefing.md` body + per-kind counts; `/op-log` is the paged audit history (200 newest) with a revert button via `mwe_core::wiki_admin::op_revert` with strict conflict policy `409 op_log_target_changed_since` (no force, no merge); button hidden for `actor_kind='system'` compensation rows to avoid chained revert-of-revert; `/sharing` is owner-only edit of `_meta.md.shared_with` via `WikiMeta::parse` → mutate → `render` → `atomic_write` round-trip |
| `/wiki/:id/edit/*path`, `/wiki/:id/view/*path`, `/wiki/:id/comment/*path`, `/cite/:bi_id` | Per-page surface for any wiki (companion or global). `/edit` is the textual editor (POST = save via `wiki_admin::push` with `actor_kind=dashboard`, op-log row stamped). `/view` is the read-only renderer interpolating `wiki_briefing_items` with `target_cite` inline below the heading they point at (orphaned items in a footer); `?mode=comment` surfaces "+ Comment on #&lt;slug&gt;" affordances per heading. `/comment/*path?anchor=<slug>` is the form GET + form POST that persists the comment as a `source_kind='dashboard_comment'` row attributed to the signed-in user. `/cite/<bi_id>` (and alias `/dashboard/cite/<bi_id>`) is the public anonymous resolver that 302-redirects to the deep-link of the cited page+anchor |
| `/skills`, `/skills/:name` | Skill catalog viewer. List bundled skills (via `mwe_core::skills::list_bundled`); per-skill view renders the full markdown body via `fetch` |
| `/bridges`, `/bridges/:consumer` | **Bridges** tab — the per-consumer bridge install guide (plugins, the install command, disabling the host's built-in memory, restart), each with an *agent instructions* link to its `install.md`. Same body as the public root `/bridges` surface, rendered in the dashboard shell; the token is issued from the home, not here. The public installers + the front page live at the root (see architecture §"HTTP bridge-distribution surface") |
| `/proposals`, `/proposals/pending-confirms`, POST `/proposals/:id/{apply,confirm,revert}`, `/proposals/:id/open-in-chat`, POST `/proposals/rem-run` | Pending + recently-applied `structure_proposals` with inline apply / confirm / revert (the dashboard is the sole write surface). `POST /proposals/rem-run` is the admin-only "run a REM cycle now" trigger; `/open-in-chat` hands a proposal to the agentic chat panel |
| `/chat`, POST `/chat` | Chat page + handler that posts the message through `wiki_ingest_message` with `context_hint=DashboardCommand` and renders the response inline; chat-onnipresente widget on every authenticated page posts to the same handler |
| `/facts` | Paginated active-fact browser (`wiki_facts_for`) with single-fact correction (`wiki_supersede`) and batch forget (`wiki_forget`) |
| `/wiki/:id/briefing-items/...` | Briefing-item inbox surface — synchronous per-row drain calling the same `briefing_processor::process_briefing_item` core the REM sub-job uses |
| `/llm-config` | Admin LLM-config editor; sets per-slot backend/model and writes API keys into `<workdir>/mwe-mcp.env` via `mwe_core::env_file::write_key` (chmod `0o600`) |
| `/welcome` | First-login welcome wizard, gated on `user_credentials.profile_initialized` |
| `/users`, `/users/new`, `/users/:id`, `/users/:id/delete`, `/users/:id/reinvite` | Admin-gated CRUD; no `is_admin` toggle |
| `/accept-invite/:invitation_id` | Public landing for the invitation link; consumes the invitation atomically with the credential write |
| `/groups`, `/groups/new`, `/groups/:id`, `/groups/:id/delete` | Admin-gated CRUD with member multi-select |
| `/tokens`, `/tokens/issue`, `/tokens/revoke`, `/tokens/delegation/:consumer_id` | Issue (with optional consumer + delegation list — the `X-MWE-Act-As` model is in `jwt-and-session-model.md`), list blacklist, revoke, edit delegation |
| `/settings/me` | Self-service password change for any signed-in user |
| `/prompts`, `/prompts/:name`, POST `/prompts/:name`, POST `/prompts/:name/reset` | Admin-only operational prompt editor. List with status badge (matches default / modified / bundled only) + drift pill `vX → vY` when the workdir's `default_version_at_bootstrap` lags the bundled frontmatter; textarea editor with submission validated by `extract_fenced_text`, drift surfaced as a "Bundled default moved from vN to vM" banner with the reset copy switching from "Reverts" to "Upgrades"; atomic save via `wiki::atomic_write` with single-slot `.bak` backup; reset to bundled default doubles as the upgrade CTA. No diff / merge UI (still deferred — operator decides whether to upgrade or stay) |
| `/static/*` | Hand-written CSS embedded via `rust-embed` |

Module layout under
[`crates/mwe-dashboard/src/`](../../crates/mwe-dashboard/src/):

| Module | Purpose | Status |
|---|---|---|
| `state` | `DashboardState` (DB pool + secret + shared blacklist cache + config knobs) + `MemoryHandles` (`Option`-wrapped bundle of `WikiTree` / `Arc<dyn Embedder>` / `LlmConfig` / workdir, needed by the wiki view + chat handlers) | implemented |
| `error` | `DashboardError` enum implementing `IntoResponse` so handlers return `Result<T>` and Axum renders the right HTML status page | implemented |
| `form` | `HtmlForm<T>` extractor over `serde_html_form` for forms that need repeated keys (`members=a&members=b` → `Vec<String>`); `axum::Form`'s `serde_urlencoded` cannot do this | implemented |
| `auth::password` | Argon2id PHC hash + verify with OWASP defaults | implemented |
| `auth::session` | Session cookie issue/clear, verify, `SessionUser` + `AdminUser` extractors, `refresh_session_layer` tower middleware | implemented |
| `ui::layout`, `ui::components` | Maud `base_page` + nav + flash + form helpers + chat-onnipresente widget rendered on every authenticated page | implemented |
| `assets` | `/static/*path` handler over a `rust-embed` of `assets/app.css` | implemented |
| `routes::*` | One submodule per top-level surface — [`routes/mod.rs`](../../crates/mwe-dashboard/src/routes/mod.rs) is the SSOT for the mounted set: `briefing`, `chat`, `cite`, `smart_view`, `connect`, `facts`, `groups`, `home`, `invitations`, `llm_config`, `login`, `logout`, `prompts`, `proposals`, `redirect`, `settings`, `setup`, `skills_view`, `tokens`, `users`, `welcome`, `wiki_view`. The public (no-auth) tree mounts `redirect`/`setup`/`login`/`invitations`/`cite`/`assets`; everything else lives behind the session-refresh layer. `cite` is also re-exported as a standalone root-mounted `/cite/:bi_id` resolver | implemented |

See the [dashboard design note](../design-notes/dashboard.md) for
the auth model + identity console and
the [dashboard-memory-mvp design note](../design-notes/dashboard-memory-mvp.md)
for the memory slice.

## Runtime layout — outside the repo

The memory wikis mwe-mcp manages **at runtime** for its consumer agents
live in a **workdir** chosen by the operator, never inside this
repository. A typical invocation is:

```bash
mwe-mcp init  --workdir /var/lib/mwe-mcp
mwe-mcp serve --workdir /var/lib/mwe-mcp --bind 127.0.0.1 --port 8742
```

The workdir contains:

```
<workdir>/
├── engine.db                 # sqlite, see the [engine-db-and-migrations design note](../design-notes/engine-db-and-migrations.md)
├── .mwe-mcp.lock             # single-writer lockfile
├── mwe-mcp.config.yaml       # validated against schemas/
└── wikis/                    # memory wikis — markdown surface
    └── <wiki_id>/
        ├── _meta.md
        ├── <page>.md
        └── ...
```

Identity (users, groups, credentials) is **not** seeded from a YAML file:
the dashboard's first-run wizard at `/dashboard/setup` creates the
first admin, and subsequent admins manage users via the dashboard CRUD
(see [`identity-and-acl.md`](../concepts/identity-and-acl.md)).

`<workdir>` is operator-chosen and may be on a persistent volume, a
SSD, or a tmpfs for tests. **It is never** a directory inside this
repository: the memory wikis the engine writes live under
`<workdir>/wikis/` and are unrelated to the project documentation
in `docs/`.

## Storage layer

Three modules form the runtime safety floor:

- [`db`](../../crates/mwe-core/src/db.rs) opens the workdir-relative
  `engine.db` and applies the canonical pragmas (`journal_mode=WAL`,
  `foreign_keys=ON`, 5s `busy_timeout`). The migration runner is the
  compile-time `sqlx::migrate!()` over [`migrations/`](../../migrations/),
  so a release binary needs no extra files on disk to bootstrap a fresh
  workdir.
- [`lockfile`](../../crates/mwe-core/src/lockfile.rs) holds a kernel
  advisory lock on `<workdir>/.mwe-mcp.lock` for the lifetime of the
  process (see the [single-writer-lockfile design note](../design-notes/single-writer-lockfile.md)
  for the cleanup model).
- [`wal`](../../crates/mwe-core/src/wal.rs) journals every step of a
  `structure_proposal_apply` or REM op into `proposal_ops_log` /
  `rem_ops_log` so a crash mid-step can be detected and rolled back at
  startup (see the [applicative-wal design note](../design-notes/applicative-wal.md)).

The migration files in [`migrations/`](../../migrations/) are
append-only — schema evolution lands as new
numbered files, never as edits to existing ones — and are embedded
into the binary at compile time via `sqlx::migrate!()`. The directory
is the SSOT for the count and the SQL; the **annotated ledger** (which
migration landed which table or column, and the canonical DDL of every
table) lives in
`engine-db-and-migrations.md`
— this page does not maintain a second copy. The smart-wiki
marker is the per-wiki smart flag in each `_meta.md`. The runtime
reads it through `load_smart_wiki_index()` / `SmartWikiIndex` (keyed by
`wiki_id`) and the `AvailableWiki.smart` field — no registry query.

## What the system covers today

The roadmap
(`roadmap.md`) is the
SSOT for per-milestone status; this section is the engineering-wiki
summary of the code surface as it stands.

- **Foundations.** The pure-function layer (parser, ACL, slug, render,
  types), the storage floor (db, lockfile, wal), the identity validator
  + JWT, the CLI, the dashboard identity console, the file watcher with
  the `.mwe-write-in-progress` marker protocol, the embedder + LLM
  adapter traits with Ollama backends, and the fault-injection harness.
- **Memory engine + dashboard.** The MCP tool surface is wired through
  the `mcp` dispatcher over HTTP
  (Streamable HTTP + JWT); the three proposal-write tools (`apply` /
  `confirm` / `revert`) live off MCP, with the dashboard as their sole
  write surface. The filesystem watcher → `fact_index` round-trip
  (`reindex`), the dashboard
  memory browser
  (`dashboard-memory-mvp.md`),
  and the agentic chat panel
  (`agentic-chat.md`,
  `MAX_AGENTIC_ITERATIONS = 8`) are all in place.
- **REM + structure proposals + smart wikis.** The proposal chassis
  (`apply_proposal` / `auto_apply_proposal` / `confirm_proposal` /
  `revert_proposal` / the two overdue/unconfirmed sweeps) ships two of
  the three kind handlers (`wiki_promote`, `dedup_merge`); only `bundle`
  surfaces `not_implemented`. The REM
  cycle (`rem::run_cycle`, see
  `rem-cycle.md`) wires the full sub-job
  pipeline. The whole smart-wiki surface (`wiki_admin_*`, the
  briefing channel, citation handles, leases, the skill catalog) runs
  end-to-end through both MCP and the dashboard.

Current limitations — what the system does not yet do (planned — see the
roadmap):

- The `/mcp/token-refresh` endpoint is not implemented (transport
  contract in
  `jwt-and-session-model.md`).
- `LlmFunctionConfig::build_backend` supports Ollama, `anthropic`, and
  `gemini`; the `openai` and `openai-compatible` cloud backends are not
  yet supported.
- **Honest partials in the audit/auth path.** `cost_estimate` and the
  `rate_limit_id` JWT claim are *parsed and recorded but not enforced* —
  there is no budget cap and no rate limiter keyed off them
  (token accounting + tower-governor wiring are not yet built).
- The audit/costs dashboard pages and the PWA service worker / Tailwind
  real build are not yet built.
- `wiki_ingest_external` non-inline sources are not yet supported.
- The `bundle` kind handler in the proposal-apply engine (multi-op
  transaction coordinator) is not yet implemented.

The `error` module row stays `partial` until its variants grow with the
real tool errors, and `config` keeps `partial` until the remaining
cloud LLM backends land.
