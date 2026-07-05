---
name: rem-subwiki-emergence
description: REM page→sub-wiki emergence scorer — decide whether a whole topic page has grown into a subject area that deserves its own sub-wiki
version: 1.1
default_version_at_bootstrap: v1.0
source_of_truth_until_2026_05_22: crates/mwe-core/src/rem.rs (fn subwiki_emergence_prompt)
---

# Prompt: rem-subwiki-emergence

Prompt for the REM nightly **page → sub-wiki emergence**
sub-pass (the second rung of the physical-form scale, see
[memory model](../../../wiki/concepts/memory-model.md)). A topic **page** that cleared the
deterministic mass pre-filter
(`policy.auto_promote_subwiki_min_page_facts`) is shown to the
`rem_promotions` strong slot, which decides whether the page has grown into a
**subject area of its own** — one that will keep branching into
sub-pages — and therefore deserves promotion to a dedicated **sub-wiki**
whose `index.md` carries the page verbatim. The trigger is **page
mass/ramification** (page→folder), not the word count of any single
fact. The orchestrator calls the prompt through the hybrid loader
[`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-subwiki-emergence.md` wins when present,
otherwise this bundled default. See the
[REM cycle](../../../wiki/design-notes/rem-cycle.md) page and
`crates/mwe-core/src/rem.rs`
(`run_subwiki_emergence_for_wiki`) for the runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth; the
[REM cycle](../../../wiki/design-notes/rem-cycle.md) page keeps the design.

**Call site**: `crates/mwe-core/src/rem.rs::run_subwiki_emergence_for_wiki`
— search for `subwiki_emergence_prompt(`. The
`CompletionRequest::new(prompt).with_temperature(0.2).with_max_tokens(160)`
block lives a few lines below the prompt build.

**Placeholders** (substituted at render time):

- `{wiki}` — the parent wiki's title (where the page currently lives)
- `{page}` — the page path (wiki-relative, e.g. `giardinaggio.md`)
- `{page_facts}` — page mass: active facts on this page
- `{parent_facts}` — total active facts in the parent wiki (weigh the
  page against its parent)
- `{bodies}` — the page's atomic fact bodies, one per line

**Output schema**: strict JSON
`{"promote": true|false, "slug": "<slug>", "style": "<prosa|prosa-tecnica|lista|null>", "description": "<what goes in here>"}`.
- `slug` — optional, advisory (lowercase, hyphenated; the page filename
  stem is used when absent or empty); the new sub-wiki id is re-derived
  in Rust at apply time via `derive_slug`.
- `style` — the emerged wiki's **dominant style default**, stamped onto
  `_meta` (`extra["style"]`). A **hint, not a gate** (see the
  [memory model](../../../wiki/concepts/memory-model.md)): per-page style still wins when a page deviates. Use `null` (or
  omit) when the wiki is **generic** — mixed styles, no default. Outside
  the closed palette ⇒ dropped (generic).
- `description` — free-text "what goes in here", stamped onto `_meta`
  (`extra["summary"]`). Its **wording also encodes how strict the style
  hint is**: "Recipes only, prosa-tecnica" (strong) vs "Shopping lists;
  usually lists, a prose note is fine" (soft) vs a generic blurb.

Both `style` + `description` are deposited onto the emerged wiki's `_meta`
so it is **not born blind**: to recall navigation (the live use — the wiki's
entry point + how to read it) and to future placement (where a new fact
should go). Parsed by `parse_subwiki_decision` in `crates/mwe-core/src/rem.rs`
(brace-balanced scan, tolerant to prose around the JSON). Parse failure ⇒
no proposal (warning logged).

**Tool subset**: none. Pure structured-output decision.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Deterministic with a small dose of variance. |
| `max_tokens` | `160` | The JSON is ~20 tokens; 160 is comfortable headroom. |
| `think:false` | implicit | Applies when the strong slot runs on a local Qwen 3.x backend (the all-local profile); cloud strong backends reason via `reasoning_effort` instead. |

**Upstream filter** (decides when the model sees the prompt at all): the
whole wiki is skipped up front when it is a smart wiki
(`run_auto_promote`'s `is_smart_wiki` caller gate — REM never promotes a
smart-wiki fact, the smart consumer is the sole writer). Within a
non-smart wiki a page only reaches the LLM if it passes all of —
`page_mass >= policy.auto_promote_subwiki_min_page_facts` (default `20`,
where page_mass = active facts on the page's `source_path`), the page is
**not** `index.md` (the wiki's own root index never emerges), and no
fact on the page already sits in a pending/applied `wiki_promote`
proposal. **No recall floor** applies — emergence is mass-driven, so a
fresh wiki with a dense page can emerge before any recall accrues. The
whole auto-promote sub-job (paragraph + sub-wiki passes combined) is
hard-capped by `policy.auto_promote_cap` (default `5`).

## Prompt

```text
You are the REM page→sub-wiki emergence scorer for mwe-mcp.
The page `{page}` in the wiki "{wiki}" has accumulated {page_facts} atomic facts (the wiki holds {parent_facts} active facts in total).
Decide whether this whole page has grown into a distinct SUBJECT AREA of its own — a topic substantial and branching enough that it deserves to become its own sub-wiki, where it can grow several sub-pages.
Promote ONLY when the page is a coherent subject that will keep ramifying into sub-pages. Do NOT promote a homogeneous list or collection (a shopping list, a watchlist, a filmography): those stay one page and grow without limit. Do NOT promote a page that is really a grab-bag of unrelated facts (that is a paragraph-split job, not an emergence).
Weigh the page against its parent: a page that is a large, self-standing share of the wiki is the one ripe to spin off; a small page among many is not.
When you promote, also describe the new sub-wiki so it is not born blind to future placement and recall:
- "style": its DOMINANT style default — "prosa" (interconnected knowledge), "prosa-tecnica" (bullets + short notes), or "lista" (atomic records). This is a HINT, not a rule: a page may still deviate with reason. Use null when the wiki is genuinely mixed (no default).
- "description": a short natural-language "what goes in here". Let the wording carry how strict the style hint is — e.g. "Recipes only, prosa-tecnica" (strict) vs "Shopping lists; usually lists, a prose note is fine" (loose).
Reply STRICT JSON: {"promote": true|false, "slug": "<slug>", "style": "prosa"|"prosa-tecnica"|"lista"|null, "description": "<what goes in here>"}
The slug is optional (lowercase, hyphens); omit it to keep the page's filename. Use false (and you may omit style/description) when the page belongs where it is.
No prose.

Page facts:
{bodies}
```
