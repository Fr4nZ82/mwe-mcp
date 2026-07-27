---
title: Configuration reference — mwe-mcp.config.yaml + mwe-mcp.env
area: protocol
status: implemented
last_review: "2026-07-20"
---

# Configuration reference

mwe-mcp is configured by two files that live **in the workdir** (the
directory the operator passes with `--workdir`), side by side:

| File | Contents | Committed? |
|---|---|---|
| `mwe-mcp.config.yaml` | Non-secret operational config: logging, LLM slots, embedding, REM scheduler, and forward-compat passthrough. | Safe to commit per-deployment, but normally lives only in the workdir. |
| `mwe-mcp.env` | Secrets: `MWE_TOKEN_SECRET` (JWT signing key) and the cloud LLM API keys. `chmod 0600` on unix. | **Never** commit. |

Both are seeded by `mwe-mcp init`. The config file is **optional** — a
fresh workdir with no `mwe-mcp.config.yaml` boots on the built-in
defaults (and logs the absence at `info`). The env file's
`MWE_TOKEN_SECRET` line, by contrast, is **required** to serve.

The authoritative parser is
[`crates/mwe-core/src/config.rs`](../../crates/mwe-core/src/config.rs)
(the `Config` struct and its sub-structs). The env file loader is
[`crates/mwe-mcp-server/src/env_loader.rs`](../../crates/mwe-mcp-server/src/env_loader.rs).
Everything documented below is verified against those two modules — the
code is canonical.

> **No JSON-Schema validation pass.** There is no
> `schemas/mwe-mcp.config.schema.json` file (`schemas/` holds only
> `.gitkeep`). Validation is done in Rust by `config.rs`: it is targeted
> (specific enum fields are checked explicitly) rather than
> whole-document-strict, and unknown top-level keys are **preserved, not
> rejected** (see [Forward compatibility](#forward-compatibility) below).

---

## Load order and failure modes

The config path is **fixed** at `<workdir>/mwe-mcp.config.yaml`.
`Config::load` always derives it from the workdir; there is **no
`--config <path>` flag** on `serve` (or any subcommand) to point the
server at a config elsewhere. `mwe-mcp serve`'s flags are `--bind`,
`--port`, and `--bypassdedicateduser` (plus the global `--workdir`) —
none of them a config path. To run two configurations you run two
workdirs.

`Config::load(workdir)` does the following:

1. Look for `<workdir>/mwe-mcp.config.yaml`.
   - **Absent** → return `Config::default()` and log the absence at
     `info` (normal for a fresh workdir).
   - **Present but malformed YAML** → **fatal** `ConfigError::Parse`;
     the server refuses to start. Silent fallback to defaults would mask
     an operator's typo, so a broken file is always a hard stop.
2. Validate the three enum-typed fields explicitly, with operator-
   readable errors (not generic serde messages):
   - `logging.level` must be `info` or `debug` → else
     `InvalidLogLevel`.
   - `logging.file_rotation` must be `daily` / `hourly` / `never` /
     `disabled` → else `InvalidLogFileRotation`.
   - `rem.schedule.mode` must be `interval` or `disabled` → else
     `InvalidRemScheduleMode`.
3. Deserialize into `Config`. Known sections (`logging`, `llm`, `rem`)
   become typed structs; **every other top-level key is captured
   verbatim** in `Config::extra`.
4. Apply the `MWE_LLM_*` environment-variable overrides on top of the
   parsed `llm` section (see [Env-var overrides](#env-var-overrides)).

There is **no hot-reload**: every field requires a server restart to
take effect. `rem.schedule`, rate limits, and budget are not
hot-reloadable today.

---

## Top-level sections

Only nine sections are materialised into typed Rust structs today:
`logging`, `llm`, `embedding`, `email`, `rem`, `recall`, `document`,
`training_spool`, `backup`. The schema documents several more —
`deployment_id`, `storage`, `features`, `http`, `budget`,
`rate_limits` — and these **parse without error** but are currently
carried opaquely in `Config::extra` (forward-compat passthrough),
**not** read or validated by `config.rs`. "Carried in `extra`" does not
imply "inert," though: `storage` describes where the workdir state
lives, and `http.bind` / `http.port` are honoured through the `serve`
CLI flags rather than through the config loader. Each section below
carries an explicit honesty note on exactly what is and isn't wired.

### `logging`

Maps to `LoggingConfig`. Two-level filter plus an optional rotating file
sink.

| Key | Type | Default | Notes |
|---|---|---|---|
| `level` | `info` \| `debug` | `info` | The only two choices. `warn` / `error` always pass through; `trace` is not exposed. `info` = boundary events (capture/supersede/forget, REM start/end, startup/shutdown, ACL denies). `debug` = boundary events **plus** internal step detail (jaccard scores, embedding dims, parser warnings, file-watcher events, slow SQL). |
| `file_rotation` | `daily` \| `hourly` \| `never` \| `disabled` | `daily` | Cadence of the rotating file sink. `disabled` installs only the stderr writer (stderr-only logging, no file sink). `never` = one ever-growing file (for read-only mounts with external log shipping). |
| `file_path` | path | `logs/mwe-mcp.log` | Relative paths are resolved against the workdir; absolute paths are used verbatim (so you can point at an external mount). Ignored when `file_rotation: disabled`. |

The chosen level is applied as a `tracing` `EnvFilter` scoped to the
mwe-mcp crates only — a chatty dependency like `sqlx` or `notify` stays
pinned at `warn` so the operator's terminal is not buried. Tracing
always writes to **stderr**; the file sink, when enabled, is an
additional writer. (The stderr discipline is a convention, not a
transport requirement: mwe-mcp is HTTP-only by design — there is no
stdio JSON-RPC channel reserving stdout. The `serve` binary prints only
its readiness line to stdout.)
See ../the [logging design note](../design-notes/logging.md) for the
wiring. Editable from the **Logging** section of the Settings page
(`/dashboard/settings/me`, admin-only); the tracing stack is installed
once at startup, so a save applies at the next restart.

The level **resolves by precedence** (`tracing_setup::make_filter`):

```
RUST_LOG env var      →  wins always (operator override)
config logging.level  →  info | debug, read from mwe-mcp.config.yaml
default               →  info
```

`RUST_LOG` is read first (`EnvFilter::try_from_default_env`); only when
it is unset does the loader fall back to `logging.level`, and only when
*that* is unset does it default to `info`. So an operator can flip on
surgical per-module tracing for one shell session
(`RUST_LOG=warn,mwe_core=trace,sqlx=info mwe-mcp serve`) without
touching the config, while `logging.level: debug` is the durable
default committed next to the workdir. Either way, **`warn` and `error`
always pass through** — both are below `info` in severity and no
config or `RUST_LOG` floor can silence them, so a typo can never make
the server log-blind to its own errors.

```yaml
logging:
  level: info
  file_rotation: daily
  # file_path: logs/mwe-mcp.log   # default; uncomment to relocate
```

### `llm`

Maps to `LlmConfig`. This is the substantive section. mwe-mcp drives
LLMs through a small set of **canonical functions** (slots); each slot
names a backend, a model, and optional tuning knobs. A slot left out of
YAML means "this function is not wired in this deployment".

#### The canonical functions

| Slot | YAML key | What it does | Status |
|---|---|---|---|
| Hub Writer | `hub_writer` | Regenerates `index.md` hub summaries when a wiki's children change (and as the last REM sub-job). Also the **fallback** backend for the operational chat when `operator_chat` is unset. | active |
| Ingest | `ingest` | Backs `wiki_ingest_message` and the dashboard chat's plain (non-agentic) path — intent classification + multi-fact capture plan. Required whenever `wiki_ingest_message` is in use. The recommendation is to point this slot at a **strong** model (see the note below). | active |
| Operator Chat | `operator_chat` | The dashboard's **operational agentic chat** (the maintainer's tool on their own memory): a multi-step tool-calling loop. Wants a **strong** model with reliable function-calling (faithful fact-id handling). **Optional** — unset falls back to `hub_writer`, so existing deployments need no new key. | active |
| REM Promotions | `rem_promotions` | The nightly "strong" structural slot: paragraph→file / file→wiki / wiki promotions, forge clustering, archive decisions. | active |
| REM Dedup (semantic) | `rem_dedup_semantic` | The yes/no semantic-equivalence classifier that runs **after** the cheap jaccard pre-pass during REM dedup. | active |
| Cronista | `cronista` | The narrative prose compiler: rewrites each dirty standard-wiki leaf from its facts into prose. | active — invoked by the compiler, not dashboard-exposed |
| Navigator | `navigator` | The per-turn recall navigator: reads the root index + destination cards and decides which wikis/pages to open next (recall pipeline). Wants a **strong-but-cheap** model — per-turn latency/cost bound, but its link choices are the recall quality bar. | configured — the navigator funnel that consumes it is being built |

> **The `cronista` slot is the active narrative prose-compiler
> slot.** The `LlmFunction::Cronista` variant backs Il Cronista in
> `crate::compiler::compile_leaf_page`: once per dirty standard-wiki leaf, it
> rewrites that page's own facts into cohesive prose, each claim wrapped
> in an inline `{{owner=… f=<fact_id>}}…{{/}}` ACL marker. It wants a
> **strong** model — faithful fact→prose without invention or leak, not
> the 9B workhorse. The value loads into `LlmConfig::cronista` and is
> read at runtime by the compiler, but — unlike the other five slots — it
> is **not** surfaced in the dashboard slot editor (it is still
> `#[deprecated]`, pending the compiler's promotion to a full REM
> sub-job), and recall does not yet serve the compiled prose. Configure it
> via the `llm.cronista:` YAML key directly. See
> [`narrative-compiler.md`](../design-notes/narrative-compiler.md).

> **Recommendation: point `ingest` at a strong model.** The
> `ingest` slot drives a heavier classification: cross-user
> attribution against the injected `known_users` roster, group-scope
> routing, public-fact recognition, structural-intent detection, and
> multi-fact `extractions` (one atomic fact per element — see
> [`LlmIngestPlan`](../../crates/mwe-core/src/ingest.rs) and the bundled
> [`prompts/ingest.md`](../../crates/mwe-core/prompts/ingest.md)). The local 9B workhorse under-triggers these structural and
> scope judgements; a strong model (e.g. Gemini 3 Flash, or an Anthropic
> slot) handles them reliably. This is **purely a config-profile choice
> on the existing `ingest` slot** — no new plumbing, no new YAML key:
> just give `ingest` a stronger `backend`/`model` than the canned local
> default the presets seed. The window and roster caps the classifier
> works against (recent-message window, `known_users` cap, group cap)
> live in `IngestPolicy::default()` in
> [`crates/mwe-core/src/ingest.rs`](../../crates/mwe-core/src/ingest.rs)
> — they are **pinned in code, not exposed as YAML keys** today, so
> there is nothing to set for them here.
>
> **Vision rides the same slot.** When a turn carries photo
> attachments (media pipeline),
> their bytes ride the ingest call as inline image parts — there is no
> separate vision slot. Whether the model can *see* them is a
> deployment property of the `ingest` slot: the API presets
> (Gemini Flash, Sonnet) are natively multimodal; a text-only local
> workhorse ignores the images and the pipeline degrades to the
> caption / consumer-supplied `description` path — the turn never
> fails on it.

#### Per-slot fields (`LlmFunctionConfig`)

Each slot accepts:

| Field | Type | Required | Notes |
|---|---|---|---|
| `backend` | `ollama` \| `anthropic` \| `gemini` \| `openrouter` \| `openai` | yes | The provider tag. `openai` parses but is **not buildable** in this build (see [Backends](#supported-backends)). |
| `model` | string | yes | The model id passed to the backend. |
| `api_key_env` | string | cloud backends only | **The name of the env-var** holding the API key — not the key itself. Anthropic / Gemini require it; Ollama ignores it. For Anthropic the value may instead be a **Claude Code / OAuth token** (auto-detected by prefix → subscription auth; see [Anthropic Claude Code / OAuth auth](#anthropic-claude-code--oauth-auth)). |
| `base_url` | string (URL) | no | Override the backend's base URL — self-hosted Ollama on a custom port, a regional Gemini gateway, an Anthropic-compatible proxy. |
| `reasoning_effort` | string | no | Backend-specific reasoning hint. Common values: `low`, `medium`, `high`, `extra-high`. Adapter-specific mapping below. |
| `temperature` | float | no | A **default** sampling temperature filled in *only when the caller leaves it unset*. Call sites that pin temperature for determinism (ingest at `0.1`, the REM revisor) are unaffected. Omitted from serialized YAML when unset. |
| `max_tokens` | int | no | A **default** `max_tokens` ceiling, same fill-only-if-unset semantics as `temperature`. Omitted from serialized YAML when unset. |

`reasoning_effort` maps onto each backend differently:

- **Anthropic** (extended thinking): mapped onto a `budget_tokens` value
  by `AnthropicBackend::with_reasoning_effort`, applied on the single-shot
  `complete` path only (the strong slots that use it — `rem_promotions`,
  `cronista`). Anthropic has no `minimal` tier — its budget floor is 1024
  — so unset / `minimal` send **no** `thinking` field at all:

  | `reasoning_effort` | `budget_tokens` |
  |---|---|
  | unset / `""` / `minimal` | none (thinking off) |
  | `medium` | 4096 |
  | `high` | 8192 |
  | `extra-high` | 16384 |
  | `low` **and any other value (incl. typos)** | 2048 |

  Two behaviours mirror Gemini: a misspelt effort floors to `low` (2048)
  rather than erroring, and `extra-high` is the top of the ladder. The
  budget is **stacked on top of** the caller's `max_tokens` (Anthropic's
  `max_tokens` is a *combined* thinking+output budget, so the output
  ceiling survives intact and `budget_tokens < max_tokens` holds), and a
  custom `temperature` is dropped while thinking is on (Anthropic rejects
  it). The agentic `chat` path does **not** apply thinking — it cannot
  round-trip the `thinking` blocks Anthropic requires alongside `tool_use`.
  Independently of thinking, the newer Anthropic families that dropped the
  sampling parameters (Opus 4.7+, Fable / Mythos — sending `temperature`
  returns HTTP 400 `invalid_request_error`) have it stripped on every call;
  the boot liveness probe pins no `temperature` at all (and asks for a
  small but non-trivial `max_tokens`, since a one-token probe can come back
  with zero content blocks), so an unlisted future family only risks a 400
  on a deliberately configured `temperature`, never on boot.
- **OpenAI** o-series: forwarded as `reasoning_effort` *(the OpenAI
  backend is gated and not buildable today)*.
- **OpenRouter**: mapped onto `reasoning.effort` by
  `OpenRouterBackend::with_reasoning_effort` — `low` / `medium` / `high`;
  `extra-high` clamps to `high`, unset / `minimal` sends **no**
  `reasoning` block at all, and an unrecognised value floors to `medium`.
- **Gemini**: mapped onto `thinkingConfig.thinkingLevel` by
  `GeminiBackend::with_reasoning_effort`. The mapping is **not** a
  pass-through — it collapses the string onto Gemini's four levels:

  | `reasoning_effort` | `thinkingLevel` |
  |---|---|
  | unset / `""` / `minimal` | `minimal` |
  | `medium` | `medium` |
  | `high` / `extra-high` | `high` |
  | `low` **and any other value (incl. typos)** | `low` |

  Two consequences worth internalising: `extra-high` is **clamped to
  `high`** (Gemini has no level above `high`), and an unrecognised
  string does **not** error — it floors to `low`, a deliberately safe
  non-`minimal` value, so a Pro slot with a misspelled effort still
  runs instead of crashing. When **unset** the level is `minimal`, and
  **Gemini 3.x Pro rejects `minimal`** — so a Pro slot must set a
  non-minimal `reasoning_effort`.
- **Ollama**: ignored.

```yaml
llm:
  profile: hybrid                     # informational label; not enforced
  hub_writer:
    backend: ollama
    model: qwen3.5:9b-q8_0
  ingest:
    backend: ollama
    model: qwen3.5:9b-q8_0
  # operator_chat:                      # optional — the dashboard agentic chat;
  #   backend: anthropic                # unset falls back to hub_writer. Wants a
  #   model: claude-sonnet-4-6          # strong tool-calling model.
  #   api_key_env: ANTHROPIC_API_KEY
  rem_promotions:
    backend: anthropic
    model: claude-opus-4-7
    api_key_env: ANTHROPIC_API_KEY
    reasoning_effort: extra-high
  rem_dedup_semantic:
    backend: ollama
    model: qwen3.5:9b-q8_0
  cronista:                           # narrative prose compiler — strong model
    backend: anthropic
    model: claude-opus-4-7
    api_key_env: ANTHROPIC_API_KEY
  navigator:                          # per-turn recall navigator — strong-but-cheap
    backend: anthropic
    model: claude-haiku-4-5-20251001
    api_key_env: ANTHROPIC_API_KEY
```

The `profile` key is purely informational — it records which preset the
config was seeded from (`all-local` / `hybrid` / `all-api` / `custom`),
but the engine does not re-derive slots from it at load time. The slots
as written in YAML are authoritative.

### `rem`

Maps to `RemConfig`. Drives the scheduler that runs the nightly REM
self-reorganization cycle (../the [rem-cycle design note](../design-notes/rem-cycle.md))
inside the long-lived server.

| Key | Type | Default | Notes |
|---|---|---|---|
| `rem.schedule.mode` | `interval` \| `disabled` | `interval` | `interval` runs `run_cycle` on a tokio ticker; `disabled` keeps the binary inert (use when an external scheduler invokes `mwe-mcp rem run-cycle`). A wall-clock `cron` mode is a future enhancement, **not** wired. **`mode` governs both schedulers**: `disabled` turns off the REM full cycle *and* the light dream below. |
| `rem.schedule.interval_secs` | int (seconds) | `86400` (24 h) | Distance between consecutive **full** REM cycle runs in `interval` mode (the nightly strong-LLM reorg). |
| `rem.schedule.initial_delay_secs` | int (seconds) | `300` (5 min) | Delay before the **first** full cycle fires after startup, to let LLM health-checks, the embedder, and the dashboard warm up. |
| `rem.schedule.light_interval_secs` | int (seconds) | `3600` (1 h) | The **light dream** — distance between consecutive light-dream runs (the captures→facts promotion that makes a buffered standard-wiki capture recallable). Far more frequent than `interval_secs` because the light dream is cheap and latency-sensitive. |
| `rem.schedule.light_initial_delay_secs` | int (seconds) | `60` (1 min) | Delay before the **first** light-dream run after startup. |
| `rem.schedule.light_backlog_threshold` | int | `20` | Buffered-capture backlog that triggers a light-dream run **ahead of** `light_interval_secs` (the "timer + threshold" cadence — whichever fires first). `0` disables the early trigger, leaving the timer as the only cadence. |
| `rem.policy.auto_promote_min_page_facts` | int \| _unset_ | _unset_ → `RemPolicy` default `8` | Min active facts on a page (page mass) before the per-page split pass shows it to the LLM. The only deterministic gate — a resource pre-filter, not a semantic one. Lower it to exercise auto-promotion in a small deployment. |
| `rem.policy.auto_promote_group_min_pages` | int \| _unset_ | _unset_ → default `9` | Pages of one subject the regrouping pass must find before a **new** sub-wiki is born. Birth only — filing pages into a sub-wiki that already exists has no floor. |
| `rem.policy.auto_promote_cap` | int \| _unset_ | _unset_ → default `5` | Max structural changes the auto-promote sub-job **applies** per cycle (both rungs share it). |
| `rem.policy.page_merge_cap` | int \| _unset_ | _unset_ → default `3` | Max page-merge confirmation calls the page-merge sub-job spends per cycle; `0` disables it. |
| `rem.policy.completion_sweep_cap` | int \| _unset_ | _unset_ → default `8` | Max evidence facts the completion sweep sends to the LLM per cycle; `0` disables it. |
| `rem.policy.contradiction_sweep_cap` | int \| _unset_ | _unset_ → default `8` | Max freshly contradicted seeds the contradiction sweep sends to the LLM per cycle; `0` disables it. |
| `rem.policy.date_normalize_cap` | int \| _unset_ | _unset_ → default `16` | Max lexically flagged facts the date normalizer sends to the LLM per cycle; `0` disables it. |
| `rem.policy.provenance_hygiene_cap` | int \| _unset_ | _unset_ → default `32` | Max trailing-source-pointer facts the provenance-hygiene sweep repairs per cycle (deterministic — embedder spend only); `0` disables it. |
| `rem.policy.briefing_processor_grace_secs` | int (seconds) \| _unset_ | _unset_ → default `900` (15 min) | How long a fresh briefing row is left alone before the briefing processor interprets it (the operator might still be editing the comment). The synchronous dashboard Submit bypasses the grace, the cycle does not. |
| `rem.policy.husk_gc_cap` | int \| _unset_ | _unset_ → default `4` | Max plan-absent husk page files (every fact tombstoned or superseded past the revert window) the husk-page GC removes per full cycle; `0` disables it. |
| `rem.policy.recall_repair_cap` | int \| _unset_ | _unset_ → default `3` | Max pending recall misses the recall-repair sub-job judges per cycle (each costs a proposal completion + a gold-set gate replay on a scratch snapshot); `0` disables it. |
| `rem.policy.recall_tuning_recurrence` | int \| _unset_ | _unset_ → default `3` | Miss count on the same fact at which an unrepaired miss queues the `recall_tuning_proposed` operator notice (never auto-applied). |

The default profile is **enabled**: a fresh deployment auto-organises
its memory without the operator flipping a switch. There are **two**
schedulers under one `mode`. The **full** REM cycle runs on a 24-hour
cadence (`interval_secs`) and drives the strong-LLM structural reorg
(../the [rem-cycle design note](../design-notes/rem-cycle.md)). The
**light dream** ([`crate::dream_light`](../../crates/mwe-core/src/dream_light.rs))
runs on the far-more-frequent `light_*` cadence: it drains the captures
buffer, embedding each buffered capture into `fact_index` so a standard-wiki
capture becomes recallable within `light_interval_secs` (or sooner once
the backlog crosses `light_backlog_threshold`). Promotion is
**deterministic — it needs no LLM**, so the light dream runs even when
the REM LLM slots are unconfigured; only `mode: disabled` stops it. The
CLI counterpart is `mwe-mcp rem run-light` (the deterministic, embedder-
only sibling of `mwe-mcp rem run-cycle`).

```yaml
rem:
  schedule:
    mode: interval
    interval_secs: 86400          # full REM cycle — nightly strong-LLM reorg
    initial_delay_secs: 300
    light_interval_secs: 3600     # light dream — captures→facts promotion
    light_initial_delay_secs: 60
    light_backlog_threshold: 20   # early trigger on backlog; 0 disables it
  policy:                         # cap / threshold overrides (omit → RemPolicy defaults)
    # auto_promote_min_page_facts: 3          # default 8 — lower it to exercise the split pass in a small deployment
    # auto_promote_group_min_pages: 6         # default 9 — pages needed to found a sub-wiki
    # auto_promote_cap: 5
    # page_merge_cap: 3                       # 0 disables the sub-job
    # completion_sweep_cap: 8                 # 0 disables
    # contradiction_sweep_cap: 8              # 0 disables
    # date_normalize_cap: 16                  # 0 disables
    # provenance_hygiene_cap: 32              # 0 disables (deterministic sweep, embedder spend only)
    # briefing_processor_grace_secs: 900      # default 900 (15 min) — briefing-processor edit grace
    # husk_gc_cap: 4                          # 0 disables — husk page files removed per full cycle
    # recall_repair_cap: 3                    # 0 disables — recall misses judged (and gate-replayed) per cycle
    # recall_tuning_recurrence: 3             # miss count that queues the operator recall-tuning notice
```

The `rem.policy:` knobs are editable from the dashboard at
**`/dashboard/admin/rem-settings`** (admin-only, mirroring the
recall-settings editor): the save rewrites this section atomically
**and hot-swaps the running policy** — the interval scheduler snapshots
the shared handle at each cycle start and the Dream console at each
trigger, so no restart is needed. The `rem.schedule:` cadence is the
**Dream cadence** section of the Settings page
(`/dashboard/settings/me`, admin-only); the schedulers are built once
at boot, so a cadence save applies at the next restart.

#### REM policy knobs — `RemPolicy` (auto-promote thresholds configurable)

The scheduler controls *when* the cycle runs. The cycle's **behaviour**
knobs — caps, thresholds, windows — live in `RemPolicy`, whose defaults
are pinned in code
([`crates/mwe-core/src/rem.rs`](../../crates/mwe-core/src/rem.rs),
`impl Default for RemPolicy`). Most are not exposed as YAML; the
exceptions are the knobs `RemConfig::resolved_policy` mirrors from
`rem.policy:` (the `rem.policy.*` rows above — the auto-promote trio,
the per-cycle sweep caps, and the briefing-processor grace; the code
SSOT is `RemPolicyConfig` in
[`config.rs`](../../crates/mwe-core/src/config.rs)), which the
scheduler, the `rem run-cycle` CLI, **and** the Dream console all honour
— and which the `/dashboard/admin/rem-settings` panel edits live.
The rest an operator retunes by editing the default or driving
`run_cycle` programmatically. Documented here because they are the real
contract of a cycle. Defaults verified against `RemPolicy::default()`:

| Knob | Default | Meaning |
|---|---|---|
| `hub_writer_cap` | 10 | Max wikis whose `index.md` is regenerated per cycle (Hub Writer is the most expensive sub-job per call). |
| `revisor_cap` | 30 | Max **act-first** `dedup_merge` merges the semantic revisor applies per cycle (born-applied receipt + 7-day revert window — see rem-cycle.md). |
| `revisor_jaccard_min` | 0.45 | Lower bound of the jaccard pre-pass band: pairs below are dismissed without asking the LLM. |
| `revisor_jaccard_max` | `recall::DEFAULT_DEDUP_THRESHOLD` | Upper bound: pairs at/above were already deduped at capture time, so the revisor works the **interesting band** in between. |
| `auto_promote_cap` | 5 | Max structural changes the auto-promote sub-job applies per cycle (both rungs) — REM never carpet-bombs the user with notices. **YAML: `rem.policy.auto_promote_cap`.** |
| `auto_promote_min_page_facts` | 8 | Min page mass before the per-page split pass shows the page to the LLM (the only deterministic gate — no recall floor). **YAML: `rem.policy.auto_promote_min_page_facts`.** |
| `auto_promote_group_min_pages` | 9 | Pages of one subject needed to found a sub-wiki (birth only; filing into an existing one has no floor). **YAML: `rem.policy.auto_promote_group_min_pages`.** |
| `page_merge_cap` | 3 | Max page-merge confirmation calls per cycle; `0` disables the sub-job. **YAML: `rem.policy.page_merge_cap`.** |
| `completion_sweep_cap` | 8 | Max evidence facts the completion sweep sends to the LLM per cycle; `0` disables. **YAML: `rem.policy.completion_sweep_cap`.** |
| `refile_sweep_cap` | 5 | Max misfiled-fact candidates the cross-wiki refile sweep sends to the LLM per cycle; `0` disables. |
| `closure_sweep_window` | 48 h | How far back the completion / contradiction sweeps look for fresh seeds. |
| `contradiction_sweep_cap` | 8 | Max freshly contradicted seeds the contradiction sweep sends to the LLM per cycle; `0` disables. **YAML: `rem.policy.contradiction_sweep_cap`.** |
| `date_normalize_cap` | 16 | Max lexically flagged facts the date normalizer sends to the LLM per cycle; `0` disables. **YAML: `rem.policy.date_normalize_cap`.** |
| `provenance_hygiene_cap` | 32 | Max trailing-source-pointer facts the provenance-hygiene sweep repairs per cycle (deterministic, embedder spend only); `0` disables. **YAML: `rem.policy.provenance_hygiene_cap`.** |
| `archive_cap` | 10 | Max `archive_proposals` emitted per (weekly) cycle. |
| `archive_inactivity` | 365 days | A fact becomes an archive candidate when its `last_recall_at` (or `created_at`) is older than this. |
| `briefing_notify_cap` | 10 | Per-wiki cap on notifications from the smart-wiki sub-jobs (Briefing dispatcher + Backlink reciprocity). |
| `briefing_stale_draft_age` | 14 days | A `status: draft` fact older than this triggers a stale-draft notify. |
| `briefing_recall_hot_threshold` | 20 | A fact with `recall_count_30d` at/above this triggers a recall-hot notify. |
| `briefing_dedup_window` | 7 days | Same `(wiki_id, source_ref)` is not re-emitted within this window. |
| `lease_expirer_grace` | 1 hour | An active lease whose `expires_at` is older than this (past expiry) is treated as crashed-without-release and marked released. |
| `lease_expirer_retention` | 7 days | Released lease rows older than this are deleted (also the dashboard op-log visibility budget). |
| `briefing_processor_enabled` | `true` | Master switch for the non-smart briefing-processor sub-job. |
| `briefing_processor_grace_secs` | `900` (15 min) | Seconds a fresh briefing row is left alone before the processor interprets it (the operator might still be editing); YAML-overridable under `rem.policy:`. The synchronous dashboard Submit (structured wikis only) bypasses it. |
| `husk_gc_cap` | 4 | Max plan-absent husk page files (every fact tombstoned or superseded past the revert window) the husk-page GC removes per full cycle; `0` disables. **YAML: `rem.policy.husk_gc_cap`.** |
| `recall_repair_cap` | 3 | Max pending recall misses the recall-repair sub-job judges per cycle; `0` disables. **YAML: `rem.policy.recall_repair_cap`.** |
| `recall_tuning_recurrence` | 3 | Miss count on the same fact at which the `recall_tuning_proposed` operator notice queues. **YAML: `rem.policy.recall_tuning_recurrence`.** |

### `recall`

Maps to `RecallConfig`. The operator's **recall settings**: the resource
knobs of the per-turn recall block (flat slot, navigator funnel, due-soon
slot — see the recall pipeline and
the ingest pipeline).
Every key is optional and mirrors its Rust field name; an omitted key
keeps the default from `IngestPolicy` / `NavigatorPolicy`
(`RecallConfig::resolved_ingest_policy` builds the per-turn policy).
Only **resources** live here — semantic judgment (which link to open,
when to stop) is the `navigator` prompt's job, never a knob.

Editable from the dashboard at **`/dashboard/admin/recall-settings`**
(admin-only): the save rewrites this section atomically **and hot-swaps
the running settings** — both transports (MCP dispatcher and dashboard
chat) read the shared handle per turn, so no restart is needed (unlike
the LLM slots, which the MCP transport clones at boot). The one
exception is `ingest_timezone`, which is not a knob on that panel — it
is the **Ingest timezone** section of the Settings page
(`/dashboard/settings/me`), hot-swapped through the same handle; and it
is only the deployment **default** — a per-user zone on the enrollment
row wins (see the table row above).

| Key | Type | Default | Notes |
|---|---|---|---|
| `recall.recall_top_k` | int \| _unset_ | _unset_ → `5` | Flat-slot size: vector-recall hits fetched per turn (classifier context + RAG entry seeds). |
| `recall.recall_fresh_top_k` | int \| _unset_ | _unset_ → `3` | Fresh-slot size: un-promoted buffered captures surfaced per turn. `0` disables the slot. |
| `recall.max_hops` | int \| _unset_ | _unset_ → `2` | Navigator depth dial: decisions per turn, clamped to the hard hop cap (`recall::MULTI_HOP_HARD_LIMIT`). |
| `recall.pages_per_hop` | int \| _unset_ | _unset_ → `3` | Pages the navigator may open per decision. |
| `recall.char_budget` | int \| _unset_ | _unset_ → `8000` | Total sender-projected prose a navigation may collect. |
| `recall.max_candidates` | int \| _unset_ | _unset_ → `16` | Candidate pages offered to the navigator per hop. |
| `recall.decision_max_tokens` | int \| _unset_ | _unset_ → `600` | Token cap per navigator completion (cost guard). |
| `recall.due_soon_top_k` | int \| _unset_ | _unset_ → `3` | `UPCOMING` (due-soon) slot size: imminent-validity facts surfaced per turn. `0` disables the slot. |
| `recall.due_soon_horizon_hours` | int \| _unset_ | _unset_ → `168` (7 days) | Look-ahead window of the due-soon pull. |
| `recall.max_agent_identity_chars` | int \| _unset_ | _unset_ → `900` | Budget of the recall block's `WHO YOU ARE` section (whole-bullet fitting). |
| `recall.max_agent_history_chars` | int \| _unset_ | _unset_ → `1400` | Budget of the `YOUR RECENT HISTORY WITH THIS USER` section. |
| `recall.ingest_timezone` | IANA name \| _unset_ | _unset_ | Deployment-wide **default** zone for reference-time stamping (a spoken "domani alle 9" resolves in this zone). The sender's own `enrollment_users.timezone` (users page / welcome wizard) wins over it; unset both → spoken times read as UTC. The `MWE_INGEST_TIMEZONE` env var is the fallback when the key is unset. |
| `recall.recent_window_entries` | int \| _unset_ | _unset_ → `32` | Per-user cap of the cross-consumer recent window's buffer. `0` disables the window. |
| `recall.recent_window_ttl_hours` | int \| _unset_ | _unset_ → `4` | How long an exchange stays servable (short by design — the window serves the thread of discourse, not history). |
| `recall.recent_window_chars` | int \| _unset_ | _unset_ → `1200` | Char budget of the rendered `recent_window` section. `0` stops serving while buffering continues. |

```yaml
recall:                       # every key optional — omit to keep the Rust default
  # max_hops: 4               # deeper navigation on a strong navigator tier
  # char_budget: 12000
  # due_soon_horizon_hours: 72
```

The related dials that do **not** live here: the navigator's *model
tier* is the [`navigator` LLM slot](#the-canonical-functions); the dream
cadence is [`rem.schedule`](#rem).

### `document`

Maps to `DocumentConfig` → `document::DocumentPolicy`. Resource knobs of
the document-ingest pipeline
(`wiki_ingest_external` + the document worker). **Resources only** —
the disposition and the extraction are LLM judgments, never a knob.
Every key is optional; an omitted key keeps the `DocumentPolicy`
default. Editable from the **Document pipeline** section of the
Settings page (`/dashboard/settings/me`, admin-only); the policy is
resolved once at boot, so a save applies at the next restart.

| Key | Type | Default | Notes |
|---|---|---|---|
| `poll_secs` | int | `10` | Worker poll cadence. |
| `segment_target_chars` | int | `3000` | Segment packing target. |
| `segment_max_chars` | int | `4500` | Hard per-segment cap (oversized paragraphs split here). |
| `max_segments` | int | `400` | A document segmenting past this is refused at enqueue (no silent truncation). |
| `max_facts_per_segment` | int | `12` | Extraction output cap per segment. |
| `classify_sample_chars` | int | `6000` | Document prefix the disposition classifier sees. |
| `merge_threshold` | float | `0.90` | Embedding cosine at/above which candidates cluster for the reduce merge. |
| `max_document_chars` | int | `1500000` | Hard input cap at enqueue. |

### `training_spool`

Maps to `TrainingSpoolConfig` and gates
[`mwe-core::training_spool`](../../crates/mwe-core/src/training_spool.rs)
— the prompt/completion recorder behind every LLM slot. When enabled,
each internal-LLM exchange (any slot, any backend) is appended as one
JSON line — slot, backend tag, model id, full request, full response,
token usage — to a per-day file under `<workdir>/training-spool/`.
Purpose: the strong API slots act as teachers and their traces become
the distillation dataset for fine-tuning the local slot models. Health
probes and failed calls are never recorded; image attachments are
recorded as MIME types only (no base64 payload).

```yaml
training_spool:
  enabled: false              # default off — the spool holds raw prompts
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Hot-toggleable from the dashboard [Training-spool panel](../design-notes/dashboard.md) — the save rewrites this key and flips the running recorder in place, no restart. |

The spool holds raw prompts, which embed recalled memory content of
every user the deployment serves — treat the directory like the wikis
themselves (deliberate backups, deliberate pruning; scrub before
sharing a dataset). Wiring: `LlmFunctionConfig::build_backend` wraps
every backend it builds in the recording decorator whenever the server
has installed the process-wide spool handle at startup; the enabled
flag is checked per call.

### `backup`

Maps to `BackupConfig` and drives the automatic-snapshot scheduler
([`mwe_mcp_server::backup_scheduler`](../../crates/mwe-mcp-server/src/backup_scheduler.rs))
— the daily hot workdir snapshot plus retention pruning. Full story:
[backup & DR](../design-notes/backup-and-dr.md). Editable from the
dashboard [Backup console](../design-notes/dashboard.md); a save
hot-swaps the running schedule (the scheduler re-reads it at each
five-minute due-check), except `initial_delay_secs`, which is read once
at boot.

```yaml
backup:
  mode: interval              # default on — daily snapshots
  interval_secs: 86400
  retention_auto: 7
  # dir: /mnt/backups/mwe     # default: <workdir-name>-snapshots sibling
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `mode` | `interval` \| `disabled` | `interval` | Automatic snapshots on/off. Manual snapshots (console, CLI) always work. An unknown value is a load-time error. |
| `interval_secs` | int (seconds) | `86400` (24 h) | Distance between automatic snapshots. The last-run stamp is persisted in `engine_meta`, so a restart never re-fires a snapshot inside the interval. |
| `initial_delay_secs` | int (seconds) | `600` (10 min) | Warm-up before the scheduler's first due-check after startup. Boot-only (not hot-swapped). |
| `dir` | path \| _unset_ | _unset_ → `<workdir-name>-snapshots` sibling | The snapshots home: automatic (`auto-*`), console-suggested manual (`manual-*`), and staged-recovery safety (`pre-restore-*` / `pre-reset-*`) snapshots. Must be **outside** the workdir. Snapshots contain secrets and cleartext memory — keep it owner-only. |
| `retention_auto` | int | `7` | `auto-*` snapshots kept; older ones are pruned after each successful run. `0` keeps everything. Manual, safety, and foreign snapshots are never pruned. |

### `embedding`

Selects the [`Embedder`](../../crates/mwe-core/src/embedder.rs) backend
that drives recall / capture / dedup. As of roadmap 18d this is a
**typed** struct ([`EmbeddingConfig`](../../crates/mwe-core/src/config.rs)),
parsed and honoured by the single factory
`EmbeddingConfig::build_embedder` that every server construction site
calls. An absent section deserializes to the default — Ollama `bge-m3` on
localhost — so existing deployments are unchanged.

```yaml
embedding:
  backend: ollama                   # ollama (default) | bundled | openai
  model: bge-m3                     # model id (ollama wire name; bundled model_id)
  base_url: http://localhost:11434  # ollama endpoint override (optional)
  device: cpu                       # bundled only: cpu (default) | gpu
  dimensions: 1024                  # vector size (ollama; bge-m3 = 1024)
  model_dir: /opt/models/bge-m3     # bundled only: offline weights dir
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `backend` | `ollama` \| `bundled` \| `openai` | `ollama` | `bundled` = the in-binary Candle embedder ([`local_embedder`](../../crates/mwe-core/src/local_embedder.rs)), available only on a build with the `local-embedder` feature (roadmap 18); `openai` is reserved and currently refused with `UnsupportedEmbeddingBackend`. |
| `model` | string | `bge-m3` | Ollama: the model name sent on the wire. Bundled: the stable `model_id` used in cache keys + reindex checks. |
| `base_url` | string (URL) | `http://localhost:11434` | `ollama` endpoint override (remote host / custom port). Ignored by other backends. |
| `device` | `cpu` \| `gpu` | `cpu` | `bundled` only. `gpu` needs a CUDA build (roadmap 18f) — on a CPU-only binary it is refused with `EmbeddingUnavailable`. |
| `dimensions` | int | `1024` | Vector size the model emits, sanity-checked on every embed. Used by `ollama`; the `bundled` backend reads its own dimension from the model config. |
| `model_dir` | string (path) | — | `bundled` only: directory holding `config.json` / `tokenizer.json` / `pytorch_model.bin` — the offline / air-gapped path, trusted as-is. When omitted, the bge-m3 weights **auto-download** (rustls, pinned-SHA-256 verified) to `$XDG_CACHE_HOME/mwe-mcp/models/bge-m3` on first use, at serve startup. |

**Reindex on change.** Switching backend or model can change the vector
distribution. The store records the embedder it was built with
(`engine_meta`: `embedder_model_id` / `embedder_dim`) and
`reindex::check_embedder_identity`
compares it at **serve startup** — a mismatch is warned loudly (recall is
wrong until a full reindex re-embeds every fact), never silently applied.
For the common Ollama-`bge-m3` → bundled-`bge-m3` move the vectors are
identical (cosine 1.0000, validated by the 18a spike), so the model ids
match and no reindex is needed.

### `email`

The SMTP backend for **self-service password recovery** (roadmap 28),
a typed struct ([`EmailConfig`](../../crates/mwe-core/src/config.rs))
edited from the admin-only Email section of the dashboard Settings page
(`/dashboard/settings/me`). **Off by
default** — with `enabled: false` (or any required field unset) the login
page hides *Forgot your password?* and the recovery route is inert, so a
fresh deployment is unchanged. The client is `lettre` over rustls (no
OpenSSL). The SMTP password is **never** in the YAML: `password_env` names
an env-var (read from `mwe-mcp.env`), mirroring the cloud LLM keys.

```yaml
email:
  enabled: true                       # master switch (default false)
  smtp_host: smtp.fastmail.com
  smtp_port: 587                       # 587 STARTTLS (default) | 465 implicit | 25 plaintext
  tls: starttls                        # starttls (default) | implicit | none
  from_address: noreply@example.com
  from_name: mwe-mcp                    # optional From display name
  username: smtp-user                  # optional; blank → no SMTP AUTH
  password_env: MWE_SMTP_PASSWORD       # env-var holding the password (default)
  public_base_url: https://mwe.example.com  # optional; else derived from the request host
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. Recovery is sent only when `true` **and** `smtp_host` + `from_address` resolve. |
| `smtp_host` | string | — | Relay hostname. Required when enabled. |
| `smtp_port` | int | `587` | `587` STARTTLS · `465` implicit TLS · `25` plaintext. |
| `tls` | `starttls` \| `implicit` \| `none` | `starttls` | `none` = plaintext, localhost/dev only. |
| `from_address` | string (email) | — | `From:` address. Required when enabled. |
| `from_name` | string | — | Optional `From:` display name. |
| `username` | string | — | SMTP AUTH username. Blank → no authentication. |
| `password_env` | string | `MWE_SMTP_PASSWORD` | Env-var (in `mwe-mcp.env`) holding the SMTP password. Never the YAML. |
| `public_base_url` | string (URL) | — | Origin used to build the reset link in the email. Blank → derived from the request `Host` + forwarded scheme. |

Two-factor authentication (TOTP) needs no config — it is enrolled
per-user from `/dashboard/settings/2fa`, with per-user and deployment-wide
(`auth.require_2fa_all` in `engine_meta`) enforcement toggles in the
dashboard. See
JWT & session model §recovery and 2FA.

### `budget` *(PARSED-BUT-INERT — not a working limiter)*

```yaml
budget:
  monthly_eur_cap: 20                 # env override MWE_MAX_API_EUR_MONTH; parsed into `extra`, NOT enforced
  alert_at_pct: 50
```

**Be honest about this one:** `monthly_eur_cap` (whose documented
env-var override is `MWE_MAX_API_EUR_MONTH`) is round-tripped through
`Config::extra` and is **not**
enforced anywhere. The env-var name is reserved by the schema but is
equally inert today — nothing in `config.rs` reads either the key or the
override. There is no live cost meter that trips at the cap, and no
request is refused because the month's spend crossed a threshold. The companion `cost_estimate` /
`rate_limit_id` columns in the `tool_executions` audit table are
likewise **recorded fields, not enforced limits** — they support after-
the-fact analysis, not a runtime limiter. Budget enforcement is future
work; today this section is purely advisory documentation.

### Other passthrough sections

The keys below all land in `Config::extra` — `config.rs` does not type
them, so it neither validates nor reads them. But "carried in `extra`"
is **not** the same as "inert": some of these describe real runtime
state (`storage`) or are consumed by a different layer of the server
(`http` via the CLI), while others are genuinely parsed-but-inert.
Each entry below says which it is. Do not treat a key here as
load-bearing without checking the consuming code the entry points at.

#### `deployment_id`

```yaml
deployment_id: family-baggins-home   # string; spec default: derived from hostname
```

A free-text identifier for the deployment. It parses into
`Config::extra` and **nothing in the engine reads it today** — there is
no derive-from-hostname fallback wired and no consumer of the value.
Treat it as a reserved, parsed-but-inert label; it is documented here
only so an operator's `deployment_id:` line is not mistaken for an
unknown key that will be stripped (it round-trips intact).

#### `storage` — *parsed-but-inert in `config.rs`, but describes where workdir state lives*

```yaml
storage:
  engine_db: data/engine.db                 # SQLite engine database
  wiki_root: wiki/                          # memory-wiki markdown tree
  archive_root: _archive/                   # archived facts tree
```

These three keys describe the on-disk layout of the workdir state (paths
are relative to the workdir unless absolute). `config.rs` does not parse
them into a typed struct, so the engine's actual paths come from the
storage layer's own wiring — verify there before relying on an override
here.

#### `features` — boolean toggles

```yaml
features:
  cronista: false     # enable the narrative prose compiler (the llm.cronista slot)
  events: true        # events_poll / events_ack — required for the lifecycle event flow
  dashboard: true     # the built-in web UI / dashboard
```

Three boolean flags. They parse into `Config::extra`; check the
consuming code (event flow, dashboard wiring) before treating a flip as
authoritative. One clarification the schema is explicit about:
**`wiki_ingest_message` is NOT a feature flag.** It is the always-on
conversational default, fed by the `llm.ingest` slot — you
cannot disable it from `features`. To take a deployment fully
non-conversational, disconnect the consumer rather than toggling a flag.

#### `http` — bind / port *(wired via the `serve` CLI, not via `config.rs`)*

```yaml
http:
  bind: 127.0.0.1   # localhost by default; 0.0.0.0 to expose behind a tunnel/proxy
  port: 8742        # default TCP port
```

This is the one passthrough section that genuinely takes effect — just
not through `config.rs`. `bind` / `port` mirror the **defaults of the
`mwe-mcp serve --bind / --port` CLI flags** (`127.0.0.1` and `8742`; see
`Serve` in `main.rs`). `Config::load` does not read `http`, so the
effective bind address is whatever `serve` receives on the command line
(the flags carry the defaults, not the YAML). Document your intended
bind/port in `http:` for clarity, but pass the values on the `serve`
command line to actually change them.

#### `rate_limits` — *parsed-but-inert; documented defaults only*

```yaml
rate_limits:
  defaults:
    reads_per_hour: 1000
    writes_per_hour: 50
  exposed_tokens:
    cost_eur_per_day: 5   # per-token cap for MCP tokens exposed via a tunnel
```

These default numbers are part of the documented schema but **no live
limiter enforces them**. As with `budget`, the `rate_limit_id` column in
the audit table is a recorded field, not an enforced cap (see
[`budget`](#budget-parsed-but-inert--not-a-working-limiter) above).
Rate-limit enforcement is future work; today this section is advisory.

---

## Supported backends

`LlmFunctionConfig::build_backend` is what actually materialises a live
backend from a slot. Four backends build; one is gated.

| Backend tag | Buildable? | API key | Notes |
|---|---|---|---|
| `ollama` | ✅ | none locally; optional Bearer for remote/cloud | Local, remote, or [Ollama Cloud](https://ollama.com). Endpoint precedence: per-role `base_url` > the provider-level `OLLAMA_BASE_URL` (default `http://localhost:11434`, `DEFAULT_OLLAMA_URL`). `OLLAMA_API_KEY`, when set, rides every request as `Authorization: Bearer …` for a cloud / proxied daemon. The zero-cost local workhorse. |
| `anthropic` | ✅ | required via `api_key_env` | Console API key (`x-api-key`) **or** a Claude Code / OAuth token (Bearer — **test/personal only**, see below). Extended thinking via `reasoning_effort` → `budget_tokens`. Optional `base_url` for a compatible proxy. |
| `gemini` | ✅ | required via `api_key_env` | Hard policies enforced at the boundary — see below. |
| `openrouter` | ✅ | required via `api_key_env` | OpenAI-compatible aggregator: one key (`OPENROUTER_API_KEY`), models as `vendor/model` slugs (`anthropic/claude-sonnet-4-6`, `google/gemini-3-pro`, …). `complete` + tools-enabled `chat` + vision. `reasoning_effort` → `reasoning.effort`. Optional `base_url` for a proxy. |
| `openai` | ❌ **gated** | — | Parses without error so an existing config does not break on upgrade, but `build_backend` returns `ConfigError::UnsupportedLlmBackend` — the operator's mistake surfaces **at startup**, not on the first request. The OpenAI adapter is not implemented today (planned — see the roadmap). |

A config that names a cloud backend but is **missing `api_key_env`**, or
names an env-var that is **unset or empty** in the process environment,
fails with `ConfigError::MissingApiKeyEnv` — and the error message names
the offending env-var so the operator can find it in `mwe-mcp.env`
without diffing files. This check runs at boot (via the LLM-slot health
check), so a missing key never silently corrupts the first request.
Whitespace-only values count as unset.

### Anthropic Claude Code / OAuth auth

The `anthropic` backend accepts **either** a Console API key (`x-api-key`,
pay-per-token) **or** a Claude Code / OAuth subscription token — it routes by
the credential's prefix (`is_anthropic_oauth_token`): `sk-ant-api…` is a
Console key; `sk-ant-…` (setup tokens `sk-ant-oat-…`), `eyJ…` (OAuth JWTs)
and `cc-…` (Claude Code access tokens) are OAuth. No config-schema change —
point the slot's `api_key_env` at the token (e.g. `CLAUDE_CODE_OAUTH_TOKEN`,
minted with `claude setup-token`) and the value itself selects the path.

On the OAuth path the request authenticates with `Authorization: Bearer`
plus the Claude Code fingerprint Anthropic's subscription endpoint requires:
the `claude-code-…` / `oauth-…` beta headers, a `claude-cli/<version>
(external, cli)` user-agent (probed from the local `claude` binary, with a
pinned fallback), `x-app: cli`, and a leading `"You are Claude Code,
Anthropic's official CLI for Claude."` system block (without it the
credential is rejected). Our own task prompt follows that block; the API-key
path sends the prompt verbatim with **no** identity prefix.

**The OAuth token exchange is the exception to that fingerprint.** The
code→token POST (and refresh) against `console.anthropic.com/v1/oauth/token`
must use a **plain** user-agent — the `claude-cli/…` one above makes that
endpoint return a routing `404 not_found`. The Claude Code fingerprint belongs
only on the inference (Messages) calls, not on the token endpoint. The exchange
body must also carry the **`state`** field (the CSRF value from the authorize
URL, echoed after the `#` in the out-of-band code) — without it the endpoint
returns `400 Invalid request format`.

There are two ways to supply the token. A **static** one — a `setup-token`
or any token pasted into the env var `api_key_env` names — is read once at
startup and never refreshed. Alternatively, the reserved value
`api_key_env: claude-code` (`mwe_core::oauth::CLAUDE_CODE_LOGIN`) routes the
slot to the **login store** at `<workdir>/anthropic_oauth.json` (`0600`),
whose short-lived access token is resolved and **refreshed automatically** on
every request.

The store is populated by the **"Log in with Claude Code"** panel on the
dashboard LLM-config page (Admin → LLM config), driven by the
`/dashboard/admin/claude-login/*` routes (`mwe_dashboard::routes::claude_login`).
The button starts a PKCE authorization-code flow with two return channels that
share one exchange:

- **Seamless (loopback)** — when the dashboard is reached over a loopback host
  (`localhost` / `127.0.0.1` / `::1`), the browser is redirected to Claude's
  authorize page with a `redirect_uri` back to the server's own callback; the
  `code` + `state` come back as query params and the token is exchanged and
  persisted with no copy-paste.
- **Manual (out-of-band)** — otherwise, or on demand ("Log in with a code
  instead"), the page shows the authorize link plus a paste box; the operator
  approves, copies the `code#state` blob Claude displays, and pastes it back.
  This is the channel that works for a **remote** dashboard, where the loopback
  redirect is unavailable.

Because the dashboard is where you log in but the server must already be up to
serve it, a `claude-code` slot that is **not yet authenticated does not block
boot**: the LLM health-check warns and skips it (the slot's feature is
unavailable until login), instead of refusing to start.

> **Test / personal use only.** This reuses *your own* Claude subscription
> for local dogfooding (no second bill) and presents requests as the Claude
> CLI — it is **not** a production auth mode and must never ship in a deployed
> product (operators bring their own Console keys). It also needs the token
> (or a logged-in `claude`) on the same host, so it does not fit the
> remote-server topology.

### Gemini 3 hard policies

The Gemini adapter enforces three non-negotiable boundary policies on
**every** request, regardless of what the caller asks for. They are
pinned as constants in
[`crates/mwe-core/src/llm.rs`](../../crates/mwe-core/src/llm.rs):

- **`maxOutputTokens = 65536`** (`GEMINI_MAX_OUTPUT_TOKENS`). Gemini 3's
  output budget is *shared* between the internal thinking trace and the
  visible output. Sending anything smaller starves one side; we always
  send the model max.
- **`thinkingLevel = minimal`** (`GEMINI_THINKING_LEVEL`) by default —
  unless a slot sets `reasoning_effort`, which overrides to
  `low`/`medium`/`high` per the collapse table in
  [Per-slot fields](#per-slot-fields-llmfunctionconfig) (note
  `extra-high` clamps to `high`, and an unknown string floors to `low`,
  never `minimal`). With the model's own default (`high`) it burns the
  whole budget reasoning and returns truncated text with
  `finishReason: MAX_TOKENS`; `minimal` keeps the reasoning trace small
  so structured callers (ingest, dedup, hub-writer JSON) get parseable
  output. **Gemini 3.x Pro rejects `minimal`** and therefore *requires*
  a non-minimal `reasoning_effort`.
- **`temperature = 1.0`** (`GEMINI_TEMPERATURE`). Gemini 3 documentation
  is explicit that values below 1.0 cause loops and degraded reasoning;
  the adapter clamps any caller-supplied temperature to 1.0 at the
  boundary (logging a debug note when it overrides) and never sends
  `0.0`, even from the health-check probe.

The REST path is pinned to `v1beta` (`GEMINI_API_VERSION`) because the
Gemini 3 models live there; the base URL defaults to
`https://generativelanguage.googleapis.com` and is overridable per slot
via `base_url`. On Gemini, `MAX_TOKENS` is surfaced as a **retriable
backend error** (not a soft truncation) so the upstream retry layer can
back off and reissue rather than serialize a half-JSON.

---

## Preset profiles

`mwe-mcp init` can seed one of three canned profiles (plus an empty
`custom`). They are constructed by `LlmProfile::build`. The model picks
below are the **current defaults in code** — they may drift as models are
re-pinned, so the YAML the init writer emits is always the source of
truth for a given install.

| Profile | hub_writer | ingest | rem_promotions | rem_dedup_semantic | cronista | navigator |
|---|---|---|---|---|---|---|
| **all-local** | ollama | ollama | ollama (`extra-high`) | ollama | ollama | ollama |
| **hybrid** *(default)* | ollama | ollama | anthropic (`extra-high`) | ollama | anthropic | anthropic (Haiku) |
| **all-api** | anthropic (Haiku) | anthropic (Sonnet) | anthropic (Opus, `extra-high`) | anthropic (Haiku) | anthropic (Opus) | anthropic (Haiku) |
| **custom** | — | — | — | — | — | — |

`operator_chat` is **omitted from every preset** (it falls back to
`hub_writer`), so the operational chat works out of the box on the
profile's `hub_writer` model. Set it explicitly — from the dashboard
Roles section or YAML — only to give the chat a stronger tool-calling
model than `hub_writer`.

The rationale baked into the presets:

- **all-local** — every slot on a local Ollama instance. Privacy-first;
  uniform Qwen 3.5 9B Q8 fits ~10 GB VRAM alongside the bge-m3 embedder
  (~5 GB) on a 16 GB GPU.
- **hybrid (recommended default)** — the conversational and frequent
  slots (`hub_writer`, `ingest`) stay on the local workhorse for zero
  latency and zero API cost; the nightly *structural* decisions
  (`rem_promotions`) go to a strong API model with `extra-high` effort
  where quality is worth the cost; `rem_dedup_semantic` reuses the local
  workhorse rather than opening a second VRAM tenant just for a yes/no
  classifier. **Caveat:** the preset still seeds `ingest` on
  the local 9B, but the classifier's structural / scope / cross-user
  judgements want a stronger model — see the
  [recommendation](#the-canonical-functions) above; override the
  `ingest` slot when classification quality matters more than latency.
- **all-api** — single-provider deployment: a cheap model for the
  bandwidth-heavy `hub_writer`, a mid model for `ingest`, a strong model
  for the structural slots, and the cheap model again for dedup.
- **custom** — an empty skeleton; the operator wires every slot by hand.
- `navigator` (all profiles) — strong-but-cheap tier: it runs on every
  turn (latency + cost bound) but its link choices set recall quality.
  Haiku on the API-bearing profiles; on all-local it shares the
  workhorse (a dedicated local navigator tune is a tracked
  extension).

> The presets reference Anthropic for the strong slots; they build
> cleanly when `ANTHROPIC_API_KEY` is set. No canned profile pins Gemini
> — an operator who wants Gemini wires it slot-by-slot (in YAML or the
> dashboard editor) with `api_key_env: GEMINI_API_KEY` by convention.

---

## Env-var overrides

After parsing the YAML, the `llm` section is overlaid with environment
variables following the pattern:

```
MWE_LLM_<FUNCTION>_<FIELD>
```

where `<FUNCTION>` is the upper-snake slot name (`HUB_WRITER`, `INGEST`,
`REM_PROMOTIONS`, `REM_DEDUP_SEMANTIC`, `CRONISTA`, `NAVIGATOR`) and
`<FIELD>` is one of:

| Suffix | Overrides | Parse rule |
|---|---|---|
| `_MODEL` | `model` | string |
| `_BACKEND` | `backend` | string |
| `_API_KEY_ENV` | `api_key_env` | string (the *name* of another env-var) |
| `_BASE_URL` | `base_url` | string |
| `_TEMPERATURE` | `temperature` | float; a malformed value logs a warning and is **ignored** (the YAML default carries on) — an operator typo on one knob does not tear down an otherwise-healthy config. |
| `_MAX_TOKENS` | `max_tokens` | int; same lenient parse as temperature. |

Examples:

```bash
# Point the ingest slot at a different local model without editing YAML:
export MWE_LLM_INGEST_MODEL=qwen3.5:9b-q8_0
export MWE_LLM_INGEST_BACKEND=ollama

# Route REM promotions to Gemini for one run:
export MWE_LLM_REM_PROMOTIONS_BACKEND=gemini
export MWE_LLM_REM_PROMOTIONS_MODEL=gemini-3.1-pro-preview
export MWE_LLM_REM_PROMOTIONS_API_KEY_ENV=GEMINI_API_KEY
export MWE_LLM_REM_PROMOTIONS_REASONING_EFFORT=high   # Pro rejects minimal
```

An override that targets a **function not present in YAML** *creates*
the slot. (If only `_BACKEND` is set, `model` falls back to the empty
string — set `_MODEL` too.) The override coverage today is limited to
the `llm.*` subtree; other subtrees (e.g. `rem.schedule.interval_secs`)
are **not** env-overridable yet.

Overrides are **runtime-only** — they are never written back to the
YAML. Anything that persists the config (the dashboard section editors)
round-trips through `Config::load_raw`, which skips the overlay, so a
save from an unrelated panel cannot bake an ephemeral override into the
file. The one deliberate exception is the LLM-config editor itself:
its form renders the *effective* runtime values, and saving persists
exactly what the operator saw on screen.

> Note the distinction: `api_key_env` (and `MWE_LLM_*_API_KEY_ENV`)
> holds the **name** of the variable that holds the key. The key itself
> lives in `mwe-mcp.env`. Two layers of indirection on purpose — the
> config never contains a secret.

---

## Secrets — `mwe-mcp.env`

`mwe-mcp.env` sits next to `mwe-mcp.config.yaml` in the workdir and is
loaded into the process environment by every `mwe-mcp` subcommand
(`init` / `serve` / `doctor` / `token-*` / `admin-reset` / `migrate`)
**before** anything reads `std::env`. Precedence: a variable already set
in the parent shell **wins** over the file (shell beats file, the same
rule the secrets layer uses everywhere). A present-but-malformed
`mwe-mcp.env` fails loudly — a typo there should not silently drop a
secret.

Recognized variables:

| Variable | Required | Purpose |
|---|---|---|
| `MWE_TOKEN_SECRET` | **yes** | The HMAC key used to sign every JWT. Must be **≥ 32 bytes** (`MIN_SECRET_BYTES`); a shorter value is rejected at construction time. `mwe-mcp init` generates a fresh 32-byte random secret and writes it here. |
| `ANTHROPIC_API_KEY` | when an `anthropic` slot is used | Referenced from a slot via `api_key_env: ANTHROPIC_API_KEY`. |
| `GEMINI_API_KEY` | when a `gemini` slot is used | Same, by convention. |
| `OPENAI_API_KEY` | n/a today | Documented for the gated `openai` backend; inert until that adapter ships. |
| `MWE_SMTP_PASSWORD` | when `email.username` is set | The SMTP password for password-recovery mail (roadmap 28). Named by `email.password_env` (this is the default); never stored in the YAML. |
| `MWE_LLM_<slot>_*` | no | The per-slot overrides from the previous section can also live here. |

`mwe-mcp init` writes the file with a commented header documenting these
variables, one live `MWE_TOKEN_SECRET=<hex>` assignment, and (on unix)
`chmod 0600`. An existing `mwe-mcp.env` is **preserved, not rotated**, on
re-init unless `--force-config` is passed.

Example `mwe-mcp.env`:

```sh
# mwe-mcp workdir env file — secrets. NEVER commit. chmod 0600.
# Shell-exported variables win over values defined here.

MWE_TOKEN_SECRET=4f3c…e91a            # ≥32 bytes; generated by `mwe-mcp init`

# Cloud LLM keys — referenced from mwe-mcp.config.yaml via `api_key_env:`
ANTHROPIC_API_KEY=sk-ant-…
# GEMINI_API_KEY=AIza…
```

---

## Forward compatibility

The `Config` struct ends with a `#[serde(flatten)] pub extra:
serde_yaml::Mapping`. The practical consequences:

- **Unknown top-level keys round-trip.** A config that adds a section
  the current binary does not understand (a future top-level feature
  block, a `storage:` override) is
  **preserved verbatim** through a load → serialize cycle, never
  stripped and never rejected. This is what makes the
  parsed-but-inert sections (`budget`, `features`, …) load cleanly.
- **Malformed YAML is still fatal.** Forward-compat applies to *unknown
  keys*, not to *broken syntax*. A file that does not parse as YAML, or
  that gives a wrong type to one of the three explicitly-validated enum
  fields (`logging.level`, `logging.file_rotation`,
  `rem.schedule.mode`), is a hard startup error.

So the safe mental model is: **typos in a typed enum field are caught
loudly; brand-new sections are tolerated and preserved; broken syntax
stops the server.**

---

## Complete annotated example

A full `mwe-mcp.config.yaml` exercising every documented section. Slots
not needed in a deployment can be omitted entirely.

```yaml
# ── identity (PARSED-BUT-INERT: round-trips via `extra`, unread today) ─
deployment_id: family-baggins-home   # free-text label; no consumer reads it yet

# ── storage (PARSED-BUT-INERT in config.rs; describes workdir layout) ──
storage:
  engine_db: data/engine.db
  wiki_root: wiki/
  archive_root: _archive/

# ── logging ───────────────────────────────────────────────────────────
logging:
  level: info                 # info | debug  (default: info)
  file_rotation: daily        # daily | hourly | never | disabled
  # file_path: logs/mwe-mcp.log   # relative → workdir; absolute → verbatim

# ── llm ───────────────────────────────────────────────────────────────
# Six canonical slots. `cronista` is the narrative prose
# compiler (crate::compiler::compile_leaf_page) — wire it to a strong
# model when a standard wiki is in use; leave it out otherwise.
llm:
  profile: hybrid             # all-local | hybrid | all-api | custom (label only)

  hub_writer:                 # regenerates index.md hub summaries
    backend: ollama
    model: qwen3.5:9b-q8_0
    base_url: http://localhost:11434   # default; shown for clarity

  ingest:                     # backs wiki_ingest_message + dashboard chat
    backend: ollama
    model: qwen3.5:9b-q8_0

  rem_promotions:             # nightly strong structural slot
    backend: anthropic
    model: claude-opus-4-7
    api_key_env: ANTHROPIC_API_KEY     # NAME of the env-var, not the key
    reasoning_effort: extra-high
    # temperature / max_tokens: optional per-slot generative defaults
    # (filled only when the caller leaves them unset)

  rem_dedup_semantic:         # yes/no classifier after the jaccard pre-pass
    backend: ollama
    model: qwen3.5:9b-q8_0

  cronista:                   # narrative prose compiler — strong model
    backend: anthropic
    model: claude-opus-4-7
    api_key_env: ANTHROPIC_API_KEY

  navigator:                  # per-turn recall navigator — strong-but-cheap
    backend: anthropic
    model: claude-haiku-4-5-20251001
    api_key_env: ANTHROPIC_API_KEY

  # Example Gemini slot (no canned profile pins it; wire by hand):
  # rem_promotions:
  #   backend: gemini
  #   model: gemini-3.1-pro-preview
  #   api_key_env: GEMINI_API_KEY
  #   reasoning_effort: high   # Gemini 3.x Pro REJECTS the default `minimal`

# ── rem scheduler ─────────────────────────────────────────────────────
# `mode` governs BOTH schedulers below — disabled turns off the full
# cycle AND the light dream.
rem:
  schedule:
    mode: interval               # interval | disabled  (cron not wired)
    interval_secs: 86400         # full REM cycle (strong-LLM reorg): 24h
    initial_delay_secs: 300      # 5 min warm-up before the first full cycle
    light_interval_secs: 3600    # light dream (captures→facts): 1h, no LLM
    light_initial_delay_secs: 60 # 1 min warm-up before the first light run
    light_backlog_threshold: 20  # early light-dream trigger on backlog; 0 disables
  # The RemPolicy behaviour knobs (caps, thresholds, windows) are NOT
  # YAML keys today — they live in RemPolicy::default() in rem.rs.

# ── embedding (typed; absent ⇒ Ollama bge-m3 on localhost) ────────────
embedding:
  backend: ollama             # ollama (default) | bundled | openai
  model: bge-m3
  base_url: http://localhost:11434  # ollama endpoint override (optional)
  device: cpu                 # bundled only: cpu (default) | gpu
  dimensions: 1024            # vector size (ollama; bge-m3 = 1024)
  # model_dir: /opt/models/bge-m3   # bundled only: offline weights dir

# ── training spool (off by default; holds raw prompts) ────────────────
training_spool:
  enabled: false              # record LLM prompt/completion pairs to
                              # <workdir>/training-spool/ (distillation
                              # dataset; dashboard-toggleable at runtime)

# ── automatic snapshots (on by default; see backup-and-dr.md) ──────────
backup:
  mode: interval              # interval | disabled
  interval_secs: 86400        # daily
  initial_delay_secs: 600     # warm-up before the first due-check (boot-only)
  retention_auto: 7           # auto-* snapshots kept; 0 keeps all
  # dir: /mnt/backups/mwe     # default: <workdir-name>-snapshots sibling

# ── features (PARSED-BUT-INERT in config.rs: check the consuming code) ─
features:
  cronista: false             # narrative prose compiler (the llm.cronista slot)
  events: true                # events_poll / events_ack — needed for the lifecycle flow
  dashboard: true             # built-in web UI
  # NOTE: wiki_ingest_message is NOT a feature flag — it is always-on,
  # fed by llm.ingest. Disable conversation by disconnecting the consumer.

# ── http (bind/port take effect via the `serve` CLI flags, not config) ─
http:
  bind: 127.0.0.1             # 0.0.0.0 to expose behind a tunnel/proxy
  port: 8742                  # mirrors `mwe-mcp serve --bind/--port` defaults

# ── budget (PARSED-BUT-INERT: monthly_eur_cap is NOT enforced) ─────────
budget:
  monthly_eur_cap: 20         # advisory only — no live limiter trips on it
  alert_at_pct: 50

# ── rate_limits (PARSED-BUT-INERT: documented defaults, no live limiter) ─
rate_limits:
  defaults:
    reads_per_hour: 1000
    writes_per_hour: 50
  exposed_tokens:
    cost_eur_per_day: 5       # per exposed MCP token; not enforced today

# Any other top-level section a future binary adds is preserved verbatim
# through `Config::extra` rather than rejected.
```

Secrets for this example live in the sibling `mwe-mcp.env`:
`MWE_TOKEN_SECRET` (≥32 bytes, generated by init) and
`ANTHROPIC_API_KEY` (the value `api_key_env` points at).

---

## See also

- ../the [logging design note](../design-notes/logging.md) — how
  `logging.level` / `file_rotation` wire into the `tracing` subscriber.
- ../the [rem-cycle design note](../design-notes/rem-cycle.md) — what a
  REM cycle actually does with the scheduler and `RemPolicy` knobs.
- ../the [admin-llm-config design note](../design-notes/admin-llm-config.md)
  — the dashboard admin page that edits the `llm` slots live.
- ../the [jwt-and-session-model design note](../design-notes/jwt-and-session-model.md)
  — what `MWE_TOKEN_SECRET` signs and the ≥32-byte floor.
- [../development/build-run.md](../development/build-run.md) — the
  `mwe-mcp init` walkthrough that seeds both files.
