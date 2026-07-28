---
title: Internal LLM functions — model, profiles, language policy, parsing
area: design-notes
status: implemented
last_review: "2026-06-11"
---

# Internal LLM functions

mwe-mcp ships with its own **configurable internal LLM**. This is the
model the *server* uses for its own reasoning — intent classification,
nightly reorganisation, dedup confirmation, `index.md` regeneration —
and it is entirely separate from the LLM the *consumer agent* runs to
talk to its end user. The two have separate bills and separate
configuration; in a private self-hosted deployment they happen to be
paid by the same person, but the code never conflates them.

This page documents the **canonical LLM functions** (the configurable
slots), the **deployment profiles** that wire them, the **language
policy** that governs every prompt, and the **robust-parser strategy**
the structured call sites rely on.

Related pages:

- [`../protocol/config-schema.md`](../protocol/config-schema.md) — the
  `mwe-mcp.config.yaml` knobs (`llm:`, `rem:`, `logging:`) in detail.
- [`admin-llm-config.md`](admin-llm-config.md) — the dashboard
  admin-only editor for the `llm:` slots and API key env-vars.
- [`rem-cycle.md`](rem-cycle.md) — the nightly REM cycle, which is the
  dominant consumer of the slots below (every one except `ingest`).

## 1. The canonical LLM functions

The configuration surface is the `enum LlmFunction` in
[`crates/mwe-core/src/config.rs`](../../crates/mwe-core/src/config.rs).
It is the single source of truth for the slot names — both as YAML keys
under `llm:` and as the suffix of the env-var override convention
(`MWE_LLM_<UPPER>_MODEL`, `MWE_LLM_<UPPER>_BACKEND`, …).

| Slot (`LlmFunction`) | YAML key | Status in code |
|---|---|---|
| `HubWriter` | `hub_writer` | Active — REM `regenerate_index`, plus the operational-chat fallback (see below). |
| `Ingest` | `ingest` | Active — `wiki_ingest_message` + the dashboard chat's plain (non-agentic) path. |
| `OperatorChat` | `operator_chat` | Active — the dashboard's operational agentic chat. Optional: unset falls back to `hub_writer`. |
| `RemPromotions` | `rem_promotions` | Active — REM auto-promote. |
| `RemDedupSemantic` | `rem_dedup_semantic` | Active — REM revisor (semantic dedup). |
| `Cronista` | `cronista` | Active — the narrative prose compiler. |

The `Cronista` variant is the **narrative prose-compiler
slot**. It backs Il Cronista
in [`crate::compiler::compile_leaf_page`](../../crates/mwe-core/src/compiler.rs):
once per dirty standard-wiki leaf, it rewrites that page's own facts into
cohesive prose, each claim wrapped in an inline `{{owner=… f=<fact_id>}}…{{/}}`
ACL marker (see [`narrative-compiler.md`](narrative-compiler.md)). It
wants a **strong** model — faithful fact→prose without invention or leak,
not the 9B workhorse. The slot resolves the usual way through
[`LlmConfig::cronista`](../../crates/mwe-core/src/config.rs) and is surfaced in
the dashboard admin UI like the other slots.

It is also the one caller that sets
[`CompletionRequest::cache_system`](../../crates/mwe-core/src/llm.rs) (via
`with_cached_system`): its system prompt is the standing brief plus the page
index, **byte-identical for every page of one compile run**, so a backend that
prices repeated prefixes can bill it once. The flag is a *claim about the
caller's own prompt*, not a knob to sprinkle: on a system prompt that varies
per call it buys a cache write per call and never a read. Honoured today only
by the Anthropic backend (`cache_control` on the last system block); every
other backend ignores it. See
[narrative-compiler §the cacheable split](narrative-compiler.md#the-cacheable-split--why-the-page-comes-last).

### 1.1 `hub_writer` — index regeneration (+ operational-chat fallback)

`hub_writer`'s primary consumer is the REM `regenerate_index` sub-job;
it is also the **fallback** backend for the operational chat (§1.1b)
when the dedicated `operator_chat` slot is unset.

**REM `regenerate_index`.** During the nightly cycle, the Hub Writer
sub-job (`run_hub_writer` in
[`crates/mwe-core/src/rem.rs`](../../crates/mwe-core/src/rem.rs))
regenerates the `index.md` summary of every non-smart parent wiki
whose children changed. It calls `regenerate_index`, which renders the
`regenerate-index` prompt and issues a single `complete` against the
`hub_writer` backend. The prompt body lives in
[`crates/mwe-core/prompts/regenerate-index.md`](../../crates/mwe-core/prompts/regenerate-index.md);
its `## Runtime contract` pins `temperature: 0.2` and `max_tokens:
800` (target output is 6-12 lines of reference prose). Smart wikis
are skipped — the smart consumer crafts its own hub pages via
`wiki_admin_push`, so REM never rewrites them.

| Property | Value |
|---|---|
| Trigger | REM cycle, a non-smart parent wiki with ≥1 child + ≥1 active fact. |
| Quality tier | Workhorse (low-to-medium — short summaries). |
| Runtime params | `temperature 0.2`, `max_tokens 800` (pinned by the prompt's runtime contract). |
| `think:false` | **Mandatory** for the local Qwen workhorse (see §4). |
| Fallback role | When `operator_chat` is unset, also backs the operational chat (§1.1b). |

### 1.1b `operator_chat` — the dashboard operational chat

The dashboard's **operational agentic chat** (the maintainer's tool on
their own memory) runs against `LlmFunction::OperatorChat`, resolved by
`MemoryHandles::backend_for_chat()` which falls back to `hub_writer`
(§1.1) when the dedicated slot is unconfigured. When an operator types a
structural command ("merge these two facts", "elimina il fatto `<id>`",
"move this wiki"), the `agentic_submission` handler in
[`crates/mwe-dashboard/src/routes/chat.rs`](../../crates/mwe-dashboard/src/routes/chat.rs)
drives a tool-calling loop: it hands the model the whitelisted
`_internal.*` descriptors (the `AgenticTool` registry in
[`crates/mwe-dashboard/src/agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)),
replays a bounded recent `{user, assistant}` window so a confirmation
resolves across turns, and iterates up to `MAX_AGENTIC_ITERATIONS`. The
chat is operational, not conversational — it operates *on* the memory
(CRUD, structure), it does not chat about it. See
[`agentic-chat.md`](agentic-chat.md).

> The same chat route has a **second**, separate entry point:
> `process_submission` (the plain `wiki_ingest_message` path) resolves
> `LlmFunction::Ingest`, not the chat slot. Only the tool-calling agentic
> loop uses `operator_chat` / `hub_writer`.

Why a dedicated slot? The chat is a different workload from
`regenerate_index`: interactive, multi-step function-calling, and it must
handle fact ids faithfully. It wants a **strong** tool-calling model,
whereas index regen is a cost-bound summary. Decoupling lets an operator
raise the chat's tier without inflating the nightly index-regen cost; the
fallback keeps existing deployments unchanged with no new YAML key.

| Property | Value |
|---|---|
| Trigger | Operator types into the dashboard agentic chat panel. |
| Quality tier | Strong, with reliable function-calling (the 9B struggles with multi-tool reasoning + ids). |
| Runtime params | Operator-set defaults via `apply_defaults_to_chat` (nothing pinned by the loop); defaults follow the same fallback via `chat_defaults()`. |
| `think:false` | **Mandatory** when this slot points at the local Qwen workhorse (see §4). |

### 1.2 `ingest` — every conversational turn

`ingest` backs `wiki_ingest_message`, the default and only MCP tool for
a conversational turn (standard consumers). For each raw message the
server does its own recall (embeddings + ACL-filtered filesystem),
then calls the `ingest` model once to classify intent, route the
target wiki, and decide capture / supersede / skip. The entry point is
`wiki_ingest_message` in
[`crates/mwe-core/src/ingest.rs`](../../crates/mwe-core/src/ingest.rs).

The same slot also backs the dashboard chat's plain
`process_submission` path (see §1.1), and the REM cycle's
**standard-wiki comment application**: turning a parked dashboard comment into
precise fact ops is the same class of judgment as ingesting a message, so
[`comment_apply`](narrative-compiler.md#human-edits-on-compiled-pages)
reuses this slot rather than adding a new one (the REM bag borrows the same
backend it uses for the auto-apply sweep). One slot, three callers.

**This slot's recommended quality tier leans toward a strong
model.** The classifier now carries structural / scope / cross-user
judgments the 9B workhorse under-triggers: it can split a turn into
several atomic facts (the `extractions` array on `LlmIngestPlan`, looped
over in `wiki_ingest_message`'s capture arm), resolve cross-user
attribution against the injected `known_users` roster
(`enrollment::list_users`), and route a fact into a group's shared memory
by reading the group `scope`. The dogfood runs showed the local 9B
under-triggering exactly these (the public-fact, structural-intent, and
group-routing recognitions), so the recommended pick is a strong model
on this slot. This is a **config-profile
choice on the existing slot — no new plumbing**: point `llm.ingest` at a
strong backend (the `hybrid`/`all-api` presets already make `ingest` a
candidate for the bigger model) and the same call site, parser, and
prompt run against it unchanged.

One nuance worth stating: the **multi-fact split itself is prompt-driven,
not a model-capability gain**. The ingest prompt makes `extractions[]` the sole
fact container and leads with "extract every atomic fact", so even a
Flash-tier model splits reliably. The
*strong* model earns its keep on the **judgment** calls — structural
intent, public→`global`, the group/wiki `scope` audience signals, and
cross-user `owner_id` — not on the split. See
[ingest-pipeline.md](ingest-pipeline.md).

| Property | Value |
|---|---|
| Trigger | Every `wiki_ingest_message` call; the dashboard chat plain path. |
| Quality tier | **Strong** recommended (structural / scope / cross-user judgments — multi-fact extraction, known-users attribution, group-scope routing — that the 9B workhorse under-triggers). A config-profile choice on this slot; no new plumbing. The 9B workhorse stays serviceable for the narrow group-scope matching / recall path. |
| Runtime params | `temperature 0.1` (classifier determinism), `max_tokens 4096` — pinned at the call site, so operator per-slot defaults do **not** override them. On a **Gemini** slot both are ignored: the backend forces `temperature 1.0` + `maxOutputTokens 65536` (Gemini-mandated; sub-1.0 loops/degrades), so the call-site values bind on Ollama and Anthropic — except `temperature`, which the newer Anthropic families (Opus 4.7+, Fable / Mythos) reject, so it is dropped there. The token cap was bumped 800→4096 so a multi-fact array is not clipped there. |
| Output | JSON plan, parsed by `parse_plan` (see §5). |
| `think:false` | **Mandatory** when this slot points at the local Qwen workhorse. |

### 1.3 `rem_promotions` — nightly structural decisions

`rem_promotions` is the **strong** slot. Two REM sub-jobs share it
(both wired to `RemLlms::auto_promote`, which `build_backends` fills
from `LlmFunction::RemPromotions`):

- **auto-promote** (`run_auto_promote`) — reads each over-floor page
  **whole** (every fact annotated with its 30-day recall count) and
  decides whether one sub-topic outgrew its siblings, naming the facts
  that move to a new page. Gated only by the page-mass resource
  pre-filter (`auto_promote_min_page_facts`) and capped at
  `auto_promote_cap` per cycle. Output parsed by `parse_split_decision`;
  the split applies directly (act-first + `structure_applied` notice).

This is where the quality of the internal LLM matters most: a
badly-decided promotion produces disordered wiki structure that the
next cycle then has to reason around. The slot is **optional** —
leaving `llm.rem_promotions` unset disables the sub-job (the
`RemLlms::auto_promote` field is an `Option`).

| Property | Value |
|---|---|
| Trigger | REM cycle (capped per night). |
| Quality tier | **Strong** (long-text analysis, irreversible structural calls). |
| Runtime params | Generative; the preset constructors pin `reasoning_effort: "extra-high"` on this slot. |
| Output | JSON, parsed by `parse_split_decision` (see §5). |

### 1.4 `rem_dedup_semantic` — semantic dedup confirmation

`rem_dedup_semantic` backs the REM **revisor** (`run_revisor_jaccard`,
wired to `RemLlms::revisor`). After a cheap deterministic jaccard
pre-pass narrows the candidate pairs to a gray zone
(`revisor_jaccard_min` .. `revisor_jaccard_max`), the model is asked a
binary "are these the same fact?" question. Its answer is read by
`parse_llm_yes`, which looks for a `{"same": true|false}` object. A
confirmed duplicate becomes a `dedup_merge` `structure_proposal`.

| Property | Value |
|---|---|
| Trigger | REM cycle, jaccard gray-zone pairs only. |
| Quality tier | Low (binary yes/no classifier). |
| Runtime params | `temperature 0.1` (decision-boundary stability), terse `max_tokens` pinned at the call site. |
| Output | `{"same": bool}`, parsed by `parse_llm_yes` (see §5). |
| `think:false` | **Mandatory** for the local Qwen workhorse. |

### 1.5 Slot → REM sub-job mapping

The REM scheduler builds one backend per slot in
[`crates/mwe-mcp-server/src/rem_scheduler.rs`](../../crates/mwe-mcp-server/src/rem_scheduler.rs)
(`build_backends`) and projects them into the borrow-shaped
`RemLlms<'_>` bag (defined in `rem.rs`) the cycle consumes:

| `RemLlms` field | Config slot | REM sub-jobs |
|---|---|---|
| `hub_writer` | `hub_writer` | `regenerate_index` (Hub Writer). **Mandatory** — without it `build_backends` returns `None` and the scheduler is skipped. |
| `revisor` | `rem_dedup_semantic` | revisor (semantic dedup). **Mandatory.** |
| `auto_promote` | `rem_promotions` | auto-promote (REM). Optional. |
| `apply` | `ingest` | the workhorse/Flash-tier backend the **light dream**'s compile pass reuses (`dream.rs`: `let flash = llms.apply`). Optional. |

Note the `apply` field reuses the `ingest` slot: it is the
workhorse/Flash-tier backend the **light dream**'s compile pass runs on
(the strong model is reserved for the nightly REM). No `structure_proposal`
kind needs an LLM at apply time.

## 2. Deployment profiles

`mwe-mcp init` seeds the `llm:` section from one of three presets (plus
a `custom` empty skeleton). The presets are
[`LlmProfile`](../../crates/mwe-core/src/config.rs) and the per-slot
picks are built by `LlmProfile::build`. The `profile:` string written
into YAML is **informational only — not enforced**: nothing stops an
operator from hand-editing individual slots away from the preset.

### `all-local` — zero API cost

Every slot points to a local Ollama instance. The current presets seed
a uniform `qwen3.5:9b-q8_0` across all five slots (fits ~10 GB VRAM
alongside the ~5 GB `bge-m3` embedder, so it runs on a 16 GB GPU);
`rem_promotions` carries `reasoning_effort: "extra-high"`. Operators
with more VRAM may swap a `qwen3:32b` into `rem_promotions` /
`cronista` for higher-quality nightly decisions. Suited to
privacy-first deployments and offline environments.

### `hybrid` — recommended default

Conversational and frequent slots stay on the local workhorse (zero
latency, no API cost); the irreversible nightly structural decisions go
to a cloud model:

- `hub_writer`, `ingest`, `rem_dedup_semantic` → local
  `qwen3.5:9b-q8_0`. (`rem_dedup_semantic` reuses the already-loaded
  workhorse rather than opening a second VRAM tenant for a yes/no
  classifier.)
- `rem_promotions`, `cronista` → Anthropic `claude-opus-4-7`
  (`rem_promotions` with `reasoning_effort: "extra-high"`), keyed off
  `ANTHROPIC_API_KEY`.

### `all-api` — maximum quality, single provider

Every slot on Anthropic: Haiku for the bandwidth-heavy `hub_writer`,
Sonnet for `ingest` (intent classification benefits from the bigger
model), Opus 4.7 with `extra-high` effort for the strong slots, Haiku
for the cheap dedup pass.

### Backends materialised today

`LlmFunctionConfig::build_backend` constructs **`ollama`,
`anthropic`, and `gemini`** today. `openai` is *parsed* (so an
operator's existing YAML survives an upgrade) but materialising it
raises `ConfigError::UnsupportedLlmBackend`; an `openai` adapter is not
implemented today. Cloud backends require a non-empty `api_key_env`
naming an env-var that is set at boot, or the boot health-check fails
with `ConfigError::MissingApiKeyEnv`. The embedder (`bge-m3` on
local Ollama) is always local and is configured separately from these
five LLM slots.

The dashboard admin editor exposes these same knobs per slot —
backend, model, `api_key_env`, `temperature`, `max_tokens`,
`reasoning_effort`, `base_url` — with hot-reload. See
[`admin-llm-config.md`](admin-llm-config.md).

## 3. Language policy

Every operational prompt in mwe-mcp is **authored in English
internally**; the model is **instructed to respond in the consumer's
locale**. This is canonical policy.

### 3.1 Why English-internal

1. **Token efficiency** — English is ~20% more concise than Italian on
   the Qwen/Llama tokenizers, leaving more context budget for few-shot
   examples and tool descriptions.
2. **Model compliance** — Qwen 3.5 and same-size models follow English
   instructions more reliably (training imbalance); Italian system
   prompts empirically leaked non-Latin characters.
3. **Prompt reuse** — mwe-mcp is an open-source standalone product;
   English prompts let non-Italian contributors and forks read and
   extend them.
4. **MCP idiom** — the MCP standard, tool descriptions, JSON schemas,
   and error codes are already in English; the prompts match.

### 3.2 Three-tier locale resolution

The output language is decided per turn by resolving a locale through
three sources, in order. The resolution lives in `wiki_ingest_message`
([`ingest.rs`](../../crates/mwe-core/src/ingest.rs)) and the renderer
in [`crates/mwe-core/src/locale.rs`](../../crates/mwe-core/src/locale.rs):

1. **`metadata.locale`** — the consumer passes an explicit BCP-47 tag
   (`it-IT`, `en-US`, …) in the `wiki_ingest_message` request. Wins
   when present.
2. **`sender_id → locale`** — when `metadata.locale` is absent, the
   per-user default the admin configured in `enrollment_users.locale`
   is looked up via `enrollment::locale_for(sender_id)`.
3. **Mirror fallback** — when neither is available, the prompt
   instructs the model to mirror the language of the user's own
   message.

The resolved locale is turned into a `LANGUAGE` directive block by
`locale::render_language_directive` and injected into the prompt via
the `{locale}` placeholder. With a known locale the block reads:

```
User locale: it-IT. Respond in Italian. Never mix languages in a single
response. Never use non-Latin alphabets unless the user's text
explicitly uses them. The tool names, JSON keys and argument enums
above stay in English; only the natural-language replies follow the
user's locale.
```

With no locale it falls back to the mirror clause ("Mirror the language
of the user's message. …"). The directive always exempts the
wire-form vocabulary (tool names, JSON keys, argument enums) from
translation — only the natural-language replies follow the locale.

### The memory-writing slots take the same directive with a different fallback

`ingest` and the chat panel answer a live turn, so mirroring the user's
words is a sane last resort. The slots that **compile memory** — page
prose, page and sub-wiki names, coined titles and descriptions,
document summaries, the date normaliser's rewrites — never see a user
turn: they are handed facts. For them the mirror clause is not a
fallback but a coin toss the prompt's own few-shot examples decide, and
those examples are written in Italian.

`locale::render_memory_language_directive` renders the same directive
body from the same table and resolves an undeclared locale to
**English** instead. The declared language always wins; English is only
what a deployment gets until the locale is set, from the users page,
on the people whose memory it is.

Two resolvers feed it, because these slots belong to two different
things:

| resolver | used by | the language of record |
| --- | --- | --- |
| `locale::memory_directive_for_wiki` / `…_for_wiki_meta` | `cronista`, `regenerate-index`, `comment-apply`, `rem-page-grouping` | the target wiki's **scope principal** — a `wiki-user` line speaks its owner's declared locale; a `wiki-group` line speaks the one **every** member declared, and has none when they disagree or anyone left it blank (`enrollment::locale_for_principal`) |
| `locale::memory_directive_for_user` | `document-classify`, `document-extract`, `document-merge` | the person who submitted the document — which is why an English PDF read by an Italian user lands in memory in Italian |

Both are best-effort: an unresolvable scope chain or a DB failure logs
a warning and yields the English fallback rather than failing the
compile.

### The registry that keeps this from rotting

`prompts::PROSE_REGISTRY` classifies **every** bundled prompt as
`Prose` (its reply becomes natural language a person reads) or
`Internal` (verdicts, ids, numbers, enum choices, existing page
identifiers). Three tests hold the line: the registry must name every
prompt in `prompts::BUNDLED` and nothing else, a `Prose` body must
carry `{locale}`, and an `Internal` body must not. A new slot therefore
cannot be merged without answering the language question, which is the
failure this whole section exists to prevent.

The dashboard agentic chat path resolves the signed-in operator's
locale the same way (tier 2 → tier 3; there is no per-message
`metadata.locale` on that path) and injects the same directive into
the `agentic-chat-panel` prompt.

### 3.3 English policy for bundled skills and operational prompts

The English-internal policy extends to the **bundled skills** in
[`crates/mwe-core/skills/`](../../crates/mwe-core/skills/) and the
operational system prompts (`ingest`, the compiler's Cronista / Hub
Writer, …): the internal LLM reads them as authoring instructions, so
their prose stays English regardless of the user's locale.

**The exception — verbatim machine-parsed keys.** A few identifiers stay
in their current (English) form regardless of locale because the engine
parses them: the `fact_type` vocabulary (`bio` / `state` / `preference` /
`rule` / `plan` / `episode` / `other`), the writing-style palette
(`prosa` / `prosa-tecnica` / `lista`), and the validity `decay_reason`
set (`contradiction` / `expired` / `completed` / `cancelled`). A fact's
free-text body and a page's description follow the user's locale.

**Smart family is excluded from REM write-jobs.** A `smart: true`
wiki is deliberately *not* a target of the REM write-jobs (auto-promote,
Hub Writer, archive detector). The smart consumer owns the authorship of
its smart wikis and pushes already-shaped markdown via
`wiki_admin_push`; REM must not silently rewrite it. See
[`smart-wikis.md`](smart-wikis.md) and
[`rem-cycle.md`](rem-cycle.md).

## 4. `think:false` is mandatory for the local workhorse

Every call into the local Ollama workhorse forces `think: false` on
the wire. The Ollama transport in
[`crates/mwe-core/src/llm.rs`](../../crates/mwe-core/src/llm.rs)
hardcodes `think: false` on **both** the single-prompt `/api/generate`
path (`complete`) and the tool-calling `/api/chat` path (`chat`).

The reason is a property of the thinking-capable Qwen 3.x models, not
of any one slot: left free to "think", the 9B-q8 workhorse consumes its
`num_predict` budget inside the reasoning block and returns an empty or
truncated `response`, which breaks every structured caller (`ingest`,
the REM dedup/promote/forge parsers) that expects parseable output.
Observed symptoms include non-Latin character leakage and anomalous
latency. Forcing `think: false` makes the output deterministic and
parseable.

This is a Qwen/Ollama knob only — a hard `think:false` **suppression**.
The Anthropic and Gemini backends have no such flag; their reasoning runs
the other way, **enabled** per slot via `reasoning_effort` (Anthropic →
extended-thinking `budget_tokens` on the `complete` path; Gemini →
`thinkingConfig.thinkingLevel`). The Ollama `think` flag is not per-call
configurable today, and there is no dashboard surface for the reasoning
trace on any backend.

## 5. Transport errors never carry the request URL

Every `reqwest` failure is mapped to `LlmError::Transport` through one
helper in [`crates/mwe-core/src/llm.rs`](../../crates/mwe-core/src/llm.rs)
that strips the request URL from the message (`reqwest`'s
`Error::without_url`). A provider URL can embed the API key in its
query string — Gemini's `?key=…` — and transport errors are logged
verbatim by every caller, so the URL must never reach the log stream.

> Gemini has a *separate* knob: `thinkingConfig.thinkingLevel`, mapped
> from the slot's `reasoning_effort`. It defaults to `"minimal"`
> (`GEMINI_THINKING_LEVEL`), which Gemini 3.x **Pro** rejects — a Pro
> slot must carry a non-minimal `reasoning_effort` or it fails the boot
> health-check. This is unrelated to the Qwen `think` flag.

### 4.1 Gemini function-calling: the `thoughtSignature` round-trip

Gemini 3 (with thinking) attaches an opaque, server-generated
`thoughtSignature` to **each `functionCall` part** of an assistant
turn, and **requires it echoed back verbatim** when that turn is
replayed in a later request. Omitting it is a hard
`HTTP 400 INVALID_ARGUMENT` — *"Function call is missing a
thought_signature in functionCall parts"*. Because the dashboard's
agentic loop (see [`agentic-chat.md`](agentic-chat.md)) holds a
`Vec<ChatMessage>` and replays the assistant turn on every subsequent
iteration, the **second** request of any multi-step tool-using chat
turn would 400 if the signature were dropped.

The Gemini backend in
[`crates/mwe-core/src/llm.rs`](../../crates/mwe-core/src/llm.rs)
carries the signature across the loop on the provider-agnostic
[`ToolCall::thought_signature`] field:

- **Capture** — the inbound response→`ToolCall` conversion reads the
  `thoughtSignature` that sits as a **sibling of `functionCall`** in
  the part object (not nested inside it) and stores it on the
  `ToolCall`.
- **Echo** — `split_gemini_messages` re-emits it as the same sibling
  field on the outbound `functionCall` part when the assistant turn is
  serialised. A `ToolCall` with no signature omits the key entirely
  (it is not serialised as `null`).

The field is `None` for providers that have no such concept (Ollama,
Anthropic), so the round-trip is a no-op there. The behaviour is
locked by a unit test (`gemini_chat_round_trips_thought_signature_on_function_call`)
that proves both capture (inbound part → `ToolCall`) and echo
(`ToolCall` → outbound sibling field) without a live API call.

## 5. The robust-parser strategy

Every structured LLM call site parses the model's output with a
**brace-balancing scanner** rather than constraining the wire format.
Four such parsers exist, one per structured slot:

| Parser | Slot | Module | Returns |
|---|---|---|---|
| `parse_plan` | `ingest` | `ingest.rs` | `LlmIngestPlan` |
| `parse_split_decision` | `rem_promotions` | `rem.rs` | `SplitDecision` |
| `parse_llm_yes` | `rem_dedup_semantic` | `rem.rs` | `bool` (reads `{"same": …}`) |

All four follow the same algorithm: scan for the **first `{`**, then
walk forward tracking brace depth and string state (so a `}` inside a
JSON string literal does not close the object) until depth returns to
zero, and feed that balanced slice to `serde_json::from_str`. This
tolerates the prose, markdown fences, and "thinking-aloud" preambles
that LLMs reliably wrap structured output in. A parse failure degrades
gracefully — `ingest` falls back to a skip response and logs a
warning; the REM parsers return `None` and the candidate is left
untouched.

### `format:"json"` / GBNF is deliberately NOT used today

The structured prompts do **not** set `format: "json"` and do **not**
constrain a server-side GBNF grammar. The robust parsers are the
primary and only strategy in the shipped code. The Anthropic and
Gemini backends likewise have no native grammar constraint, so a single
parsing strategy keeps the call sites backend-agnostic.

This is the **one live open question** in the prompt area
(`prompt-output-robustness`): strategy α (robust parser only) versus
strategy β (robust parser **plus** `format:"json"` belt-and-suspenders
on backends that support grammar constraint). The decision is
conditional on a measurement that has not been taken yet — the
production parse-failure rate on the *real* shipped prompts. The eval
that flagged `format:"json"` as a "necessary fix" was run on
*synthetic* prompts written for the test, not the production ones. The
direction is: keep the robust parser as primary; instrument the
parse-failure rate in production; if it exceeds 5%, adopt
`format:"json"` as a second line of defence, scoped to the backends
that support it.

## 6. The training spool — teacher traces for local-slot distillation

The slots are narrow, repetitive, structured tasks — exactly the shape
where a small local model fine-tuned on traces from a strong API
"teacher" approaches the teacher's quality. The blocker for such a
fine-tune is the dataset, and production generates it for free: every
API-backed slot call is a ready-made `(prompt, completion)` training
pair. [`mwe-core::training_spool`](../../crates/mwe-core/src/training_spool.rs)
captures them.

- **Seam** — `LlmFunctionConfig::build_backend` wraps every backend it
  builds in a recording decorator (`SpoolingBackend`) when the server
  has installed the process-wide spool at startup (same `OnceLock`
  idiom as the OAuth login store). No call site knows about it; every
  slot and transport (MCP ingest, REM cycle, dashboard chat) is
  covered.
- **Record** — one JSON line per successful call into
  `<workdir>/training-spool/YYYY-MM-DD.jsonl`: slot, backend tag,
  model id, the full request (system + prompt, or messages + tools on
  the chat path), the full response, finish reason, token usage.
  Prompts are recorded verbatim — a truncated prompt is useless as a
  training pair. Image attachments ride as MIME types only. Health
  probes and failed calls are never recorded.
- **Toggle** — `training_spool.enabled` in the YAML (default **off**),
  hot-flippable from the dashboard Training-spool panel
  ([dashboard.md](dashboard.md)); the decorator checks the flag per
  call. Recording is best-effort: an I/O failure logs a warning and
  never fails the turn (same doctrine as the recall-trace journal).
- **Privacy** — the spool embeds recalled memory content of every user
  the deployment serves. It never leaves the machine, but the operator
  must treat the directory like the wikis themselves and scrub before
  a dataset leaves the host.

The consumers of the spool (dataset filtering, the golden-set eval
harness, the distillation run itself) are offline tooling, not part of
the server — this page documents only the recorder that ships.
