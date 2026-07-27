---
name: rem-page-grouping
description: REM page-group → wiki cartographer — read a wiki's whole page inventory and group the pages that are already one subject area
version: 1.0
default_version_at_bootstrap: v1.0
---

# Prompt: rem-page-grouping

Prompt for the REM nightly **page-group → wiki** regrouping sub-pass
(the second rung of the physical-form scale, see
[memory model](../../../docs/concepts/memory-model.md)). Once per wiki,
the `rem_promotions` strong slot reads the wiki's **whole page
inventory** and cuts groups of pages that are **already** one subject
area. A group either founds a new sub-wiki (floor:
`policy.auto_promote_group_min_pages`, default `9`) or moves into one
that already exists (no floor).

The trigger is **evidence on disk**, not a forecast. A wiki is born
holding every page of its subject, so it can never be born with a single
page; a page that has merely accumulated mass belongs to the
*paragraph → page* split pass, which now runs unopposed. The
orchestrator calls the prompt through the hybrid loader
[`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-page-grouping.md` wins when present, otherwise
this bundled default. See the REM cycle page and
`crates/mwe-core/src/rem.rs` (`run_page_grouping_for_wiki`) for the
runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't drift
from it. Code is the source of truth; the REM cycle page keeps the
design.

**Call site**: `crates/mwe-core/src/rem.rs::run_page_grouping_for_wiki`
— search for `page_grouping_prompt(`. The
`CompletionRequest::new(prompt).with_temperature(0.2).with_max_tokens(1_200)`
block lives a few lines below the prompt build.

**Placeholders** (substituted at render time):

- `{wiki}` — the wiki's title
- `{wiki_pages}` — how many topic pages it holds (excluding `index.md`)
- `{min_pages}` — the birth floor, `policy.auto_promote_group_min_pages`
- `{existing}` — the sub-wikis already under this wiki, one per line,
  with their `_meta` summary and page count (`(none)` when there are
  none)
- `{inventory}` — one line per candidate page: name, active-fact count,
  and up to two verbatim excerpts

The inventory deliberately carries **excerpts, not the stored
`page_description`**: that field is written per fact at routing time and
drifts (in a live corpus it routinely describes a neighbouring page, and
mixes languages). A wrong label is worse than no label — the filename
plus two real sentences is ground truth.

**Output schema**: strict JSON
`{"groups": [ … ]}`, each group one of:

- `{"action":"create","slug":"<slug>","title":"<title>","style":"<prosa|prosa-tecnica|lista|null>","description":"<what goes in here>","pages":["a.md","b.md", …]}`
- `{"action":"move","target":"<existing wiki id>","pages":["c.md", …]}`

`slug` is re-derived in Rust at apply time via `derive_slug`; `style`
and `description` are stamped onto the newborn wiki's `_meta`
(`extra["style"]` validated to the closed palette, `extra["summary"]`)
so it is **not born blind** to placement and recall navigation. `style`
is a **hint, not a gate** (see the
[memory model](../../../docs/concepts/memory-model.md)): a page may
still deviate with reason, and a value outside the palette leaves the
wiki generic. Parsed by `parse_page_groups` in
`crates/mwe-core/src/rem.rs` (brace-balanced scan, tolerant to prose
around the JSON). A group missing its discriminator, its pages, or (for
a birth) its slug is **dropped**, never guessed at; parse failure of the
whole object ⇒ no groups (warning logged).

**Tool subset**: none. Pure structured-output decision.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Deterministic with a small dose of variance. |
| `max_tokens` | `1200` | The JSON lists page names — a 13-page group is ~200 tokens; 1200 covers several groups. |
| `think:false` | implicit | Applies when the strong slot runs on a local Qwen 3.x backend (the all-local profile); cloud strong backends reason via `reasoning_effort` instead. |

**Upstream filter** (decides when the model sees the prompt at all): the
whole wiki is skipped up front when it is a smart wiki
(`run_auto_promote`'s `is_smart_wiki` caller gate — REM never regroups a
smart wiki, the smart consumer is the sole writer), and again when the
wiki has **fewer than `{min_pages}` candidate pages and no existing
sub-wiki** to file into — with neither a possible birth nor a possible
move, the call would be wasted. `index.md` is never a candidate
(moving a wiki's front page out would decapitate it). The verdict memo
(`rem_verdicts` kind `page_grouping`) keys on the rendered prompt, so a
settled "no groups" re-opens by itself as soon as the inventory changes.
Applies share `policy.auto_promote_cap` (default `5`) with the
paragraph pass.

## Prompt

```text
You are the REM page-group cartographer for mwe-mcp.
The wiki "{wiki}" holds {wiki_pages} topic pages, listed below with their active-fact count and a couple of verbatim excerpts.
Find the groups of pages that are ALREADY one subject area, and give each group a home. You are not predicting what a page might grow into — you are reading what is on the shelf and grouping what is already there.

Two moves are available:
- "create": at least {min_pages} pages that together are one coherent subject area, and whose subject has no home yet. They become a new sub-wiki.
- "move": pages whose subject IS one of the existing sub-wikis listed below. They move into it. Any number of pages qualifies, even one — the home already exists, so there is nothing to justify.

Rules:
- Prefer "move" over "create" whenever an existing sub-wiki already covers the subject. Never found a second home for something that has one.
- Fewer than {min_pages} pages and no existing home means NO group. Say nothing about those pages; they stay where they are.
- Never group pages merely because they are small, recent, or awkward to place. A leftover pile is not a subject area.
- A page belongs to at most one group, and a group is a subject, not a theme you can name — if the only thing the pages share is a word, leave them alone.
- Zero groups is the correct answer for a wiki that is already tidy. Return {"groups": []} without apology.

For a "create" group, describe the new wiki so it is not born blind to future placement and recall:
- "slug": short, lowercase, hyphenated.
- "title": human-readable, in the language of the pages.
- "style": its DOMINANT style default — "prosa" (interconnected knowledge), "prosa-tecnica" (bullets + short notes), or "lista" (atomic records). A HINT, not a rule: a page may deviate with reason. Use null when genuinely mixed.
- "description": a short natural-language "what goes in here". Let the wording carry how strict the style hint is.

Reply STRICT JSON, no prose:
{"groups":[{"action":"create","slug":"<slug>","title":"<title>","style":"prosa"|"prosa-tecnica"|"lista"|null,"description":"<what goes in here>","pages":["a.md","b.md"]},{"action":"move","target":"<existing wiki id>","pages":["c.md"]}]}

Existing sub-wikis of this wiki:
{existing}

Pages:
{inventory}
```
