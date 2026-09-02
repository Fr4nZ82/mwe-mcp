---
name: rem-promotions
description: REM auto-promote scorer — per-page paragraph→page split decision (whole page in, moved facts out)
version: 2.9
default_version_at_bootstrap: v2.4
source_of_truth: crates/mwe-core/src/rem.rs (fn paragraph_split_prompt)
---

# Prompt: rem-promotions

Prompt for the REM nightly **auto-promote** sub-job, per-page
split pass. A page that passed the mass pre-filter
(`policy.auto_promote_min_page_facts`) is shown **whole** to the
`rem_promotions` strong slot — every fact annotated with its id and 30-day
recall count — and the LLM decides whether one sub-topic has outgrown its
siblings (mass) and/or is frequently recalled (recall), naming the
facts that move to a new dedicated page. The trigger is **page mass +
recall weighed together by the model**, never a hardcoded recall floor.
The orchestrator calls the prompt through the hybrid loader
[`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-promotions.md` wins when present, otherwise
this bundled default. See `crates/mwe-core/src/rem.rs` (around the
`paragraph_split_prompt` call site) for the runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth.

**Call site**: `crates/mwe-core/src/rem.rs::run_auto_promote` —
search for `paragraph_split_prompt(`. The `CompletionRequest::new(prompt)
.with_temperature(0.2).with_max_tokens(4_000)` block lives a few lines
below the prompt build.

**Placeholders** (substituted at render time by
`mwe_core::prompts::render`):

- `{page}` — the wiki-relative page path (`work.md`, `ricette.md`). Never a
  wiki's identity card: the card is served whole into every turn and is not
  offered here at all.
- `{page_facts}` — page mass: number of active facts on the page
- `{shape}` — **which metre this page was measured on**, from its testata
  `style` (`rem::shape_directive`). The floor that let the page reach this
  prompt is not one number — `prosa` 8, `prosa-tecnica` 32, `lista` never — so
  without it the model is asked whether a page "grew disproportionately" while
  the only scale it has is the fact count, and it answers the same way for a
  bullet list and for a narrative.
- `{facts}` — the whole page, one entry per fact:
  `- [n<k>] recall30d: <n>` followed by the indented fact text, where
  `n<k>` is the fact's **1-based position in this list**

**Handles, not ids** (v2.1): each fact is presented as `[n1]`, `[n2]`, …
instead of its UUID. The model never reasons over a fact id — it only
echoes one back to name what moves — and a UUID costs ~18 tokens of pure
noise per fact on the strong slot this pass runs on. `resolve_split_handle`
in `crates/mwe-core/src/rem.rs` maps the answer back by position and
**still accepts a raw fact id**, so an operator override of this prompt
that presents ids keeps working and a model that echoes an id anyway is
not mistaken for a hallucination.

**Output schema**: strict JSON
`{"split": true|false, "fact_ids": ["n1", "n3", …], "target_page": "<filename.md>"}`.
The target page goes through `planner::canonical_page_path`, which lowercases it
and folds every run of non-alphanumeric characters into one `_`, so `Acme Corp`
and `acme-corp` both land as `acme_corp.md`. The prompt asks for that spelling
outright, so the model reads back the page it actually named.
Parsed by `parse_split_decision` in `crates/mwe-core/src/rem.rs` into a
`SplitDecision { split, fact_ids, target_page }` struct (brace-balanced
scan, `serde_json::Value`, tolerant to prose around the JSON). Parse
failure ⇒ `None` ⇒ the page stays as it is (no apply, warning logged).
The named handles are re-validated in Rust: each must resolve on the page
and the set must be a **proper, non-empty subset** (moving everything
is a rename, not a split — that is the page→sub-wiki rung). On a valid
split verdict the move is **applied directly** (act-first): there is no
proposal step, and nobody is notified — the memory reorganising itself is
not news.

**Memoized**: a `{"split": false}` verdict is recorded in `rem_verdicts`
keyed by the model id plus this prompt rendered with each recall count
bucketed into a band (`none`/`low`/`medium`/`high`). The page is not
re-asked until its facts, its prompt, its model, or a recall *band*
moves — see [`mwe_core::rem_verdicts`].

**Tool subset**: none. Pure structured-output decision.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Deterministic output with a small dose of variance to avoid the classifier collapsing onto a single pattern. |
| `max_tokens` | `4000` | The JSON carries a list of fact UUIDs (~40 tokens each is generous); 4000 covers a large page's worth of moved facts with headroom. |
| `think:false` | implicit | Applies when the strong slot runs on a local Qwen 3.x backend (the all-local profile); cloud strong backends reason via `reasoning_effort` instead. |

**Upstream filter** (decides when the model sees the prompt at all):
a page reaches the LLM only when
`page_mass >= policy.auto_promote_min_page_facts` (default `8`) and no
fact on it is already covered by a `wiki_promote` receipt. That floor
is a cheap **resource** pre-filter, not a semantic gate — every
semantic judgement (which sub-topic, whether it is ripe, where it
goes) is the model's. The whole sub-job is hard-capped by
`policy.auto_promote_cap` (default `5`): a nightly cycle applies at
most five structural changes even if dozens of pages pass the filter.

## Prompt

```text
You are the REM auto-promote scorer for mwe-mcp.
You are reading the whole page `{page}`, which has accumulated {page_facts} atomic facts. Each fact below carries a short handle (`[n1]`, `[n2]`, …) and how many times it was recalled in the last 30 days.
{shape}
Decide whether ONE sub-topic on this page has outgrown its siblings — grown disproportionately in mass — and/or is frequently recalled, enough to deserve its own dedicated page. Weigh mass and recall together; a sub-topic that is both big and hot is the clearest candidate.
Split ONLY a coherent sub-topic that reads as a self-contained subject. Never name every fact on the page: a full move is not a split.
Reply STRICT JSON: {"split": true|false, "fact_ids": ["n1", "n3", ...], "target_page": "<filename.md>"}
List in fact_ids exactly the handles of the facts that move to the new page, copied as shown (`n1`, `n2`, ...) without the brackets. The target_page must end with `.md` and be lowercase words joined by underscores (`acme_corp.md`), and must never be one of the reserved page names — `profile`, `rules`, `projects`, `project_diary`, `projects_diary`, with or without the engine's `@` marker — a split that names one is refused outright and the page stays as it is, so you lose the split. Use {"split": false} when the page is fine as it is.
No prose.

Page facts:
{facts}
```
