---
name: rem-promotions
description: REM auto-promote scorer — per-page paragraph→page split decision (whole page in, moved facts out)
version: 2.2
default_version_at_bootstrap: v2.2
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
recall weighed together by the model**, never a hardcoded recall
floor — see the [memory model](../../../docs/concepts/memory-model.md).
The orchestrator calls the prompt through the hybrid loader
[`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-promotions.md` wins when present, otherwise
this bundled default. See the
REM cycle for the narrative
and `crates/mwe-core/src/rem.rs` (around the `paragraph_split_prompt`
call site) for the runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth; the
REM cycle keeps only the
design log (changelog, narrative).

**Call site**: `crates/mwe-core/src/rem.rs::run_auto_promote` —
search for `paragraph_split_prompt(`. The `CompletionRequest::new(prompt)
.with_temperature(0.2).with_max_tokens(4_000)` block lives a few lines
below the prompt build.

**Placeholders** (substituted at render time by
`mwe_core::prompts::render`):

- `{page}` — the wiki-relative page path (`index.md`, `work.md`)
- `{page_facts}` — page mass: number of active facts on the page
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
The target page must end with `.md`, be lowercase, and use hyphens.
Parsed by `parse_split_decision` in `crates/mwe-core/src/rem.rs` into a
`SplitDecision { split, fact_ids, target_page }` struct (brace-balanced
scan, `serde_json::Value`, tolerant to prose around the JSON). Parse
failure ⇒ `None` ⇒ the page stays as it is (no apply, warning logged).
The named handles are re-validated in Rust: each must resolve on the page
and the set must be a **proper, non-empty subset** (moving everything
is a rename, not a split — that is the page→sub-wiki rung). On a valid
split verdict the move is **applied directly** (act-first) and a
`structure_applied` notice is emitted — there is no proposal step.

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
| `think:false` | implicit | Applies when the strong slot runs on a local Qwen 3.x backend (the all-local profile); cloud strong backends reason via `reasoning_effort` instead. See the REM cycle. |

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
Decide whether ONE sub-topic on this page has outgrown its siblings — grown disproportionately in mass — and/or is frequently recalled, enough to deserve its own dedicated page. Weigh mass and recall together; a sub-topic that is both big and hot is the clearest candidate.
Split ONLY a coherent sub-topic that reads as a self-contained subject. Do NOT split a homogeneous list or collection (a shopping list, a watchlist) just because it is long — those stay one page and grow without limit; split only when the page mixes separable subjects. Never name every fact on the page: a full move is not a split.
Reply STRICT JSON: {"split": true|false, "fact_ids": ["n1", "n3", ...], "target_page": "<filename.md>"}
List in fact_ids exactly the handles of the facts that move to the new page, copied as shown (`n1`, `n2`, ...) without the brackets. The target_page must end with `.md`, be lowercase, and use hyphens. Use {"split": false} when the page is fine as it is.
No prose.

Page facts:
{facts}
```
