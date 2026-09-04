---
name: rem-page-grouping
description: REM page-group → wiki cartographer — judge one engine-nominated group of pages: are they a subject area, and does it deserve a wiki of its own
version: 1.7
default_version_at_bootstrap: v1.3
---

# Prompt: rem-page-grouping

Prompt for the REM nightly **page-group → wiki** regrouping sub-pass
(the second rung of the physical-form scale), asked **once per candidate
group** across the whole memory.

**The engine nominates, the model judges.** `nominate_candidates` counts
three handles a page can share — a topic word, the name of what a fact is
about, the day a fact holds from — and every handle tying at least
`policy.auto_promote_group_min_pages` pages together becomes one question:
*these pages share this handle, are they a subject area?* The model never
sees the whole memory, which is what makes the pass affordable on a large
one; and the counting stays where a `GROUP BY` does it better and explains
itself, so the receipt can say WHY these pages were put together.

**No handle is privileged.** A wiki may emerge for anything nine pages are
about — a craft, a car, an animal, a busy day, a relative the household
looks after. The subject is one nominator of three.

A group either founds a **new wiki at the top level** (a wiki is structure,
not possession: it hangs under nothing) or moves into one that already
exists (no floor — the home is there). The orchestrator calls the prompt
through the hybrid loader [`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-page-grouping.md` wins when present, otherwise this
bundled default.

## Runtime contract

Operational specs that ship next to the prompt body so they can't drift
from it. Code is the source of truth.

**Call site**: `crates/mwe-core/src/rem.rs::judge_one_candidate` — search
for `candidate_grouping_prompt(`. The
`CompletionRequest::new(prompt).with_temperature(0.2).with_max_tokens(1_200)`
block lives a few lines below the prompt build.

**Placeholders** (substituted at render time):

- `{handle}` — the shared handle, verbatim: `salute`, `Bilbo`, `2026-06-30`
- `{nominator}` — what kind of handle it is: `topic`, `named thing`, `day`
- `{pages}` — how many pages share it
- `{existing}` — every standard wiki a group could be filed into, one per
  line, with its `_meta` summary and topic-page count
- `{inventory}` — one line per candidate page: `<wiki>/<page.md>`, its
  active-fact count, and up to two verbatim excerpts

The inventory deliberately carries **excerpts, not the stored
`page_description`**: that field is written per fact at routing time and
drifts (in a live corpus it routinely describes a neighbouring page, and
mixes languages). A wrong label is worse than no label — the filename plus
two real sentences is ground truth.

**Output schema**: strict JSON `{"groups": [ … ]}`, zero or one entry for
the candidate shown (a reply naming several answers a question nobody put;
the first is taken):

- `{"action":"create","slug":"<slug>","title":"<title>","style":"<prosa|prosa-tecnica|lista|null>","description":"<what belongs here>","pages":["<wiki>/<page.md>", …]}`
- `{"action":"move","target":"<existing wiki id>","pages":["<wiki>/<page.md>", …]}`

`pages` may name a **subset** of what was offered — the pages that really
are the subject — and naming one nobody offered rejects the group rather
than guessing. The birth floor is applied to the subset of a `create` and
**not** to a `move`, which has none; from a `move`'s pages the engine drops
the ones already in the target, since a group nominated across the memory
routinely holds some. `slug` is re-derived in Rust at apply time via
`derive_slug`; `style` and `description` are stamped onto the newborn
wiki's `_meta` so it is **not born blind** to placement and recall
navigation. Parsed by `parse_page_groups` in
`crates/mwe-core/src/rem.rs` (brace-balanced scan, tolerant to prose around
the JSON). A group missing its discriminator, its pages, or (for a birth)
its slug is **dropped**, never guessed at.

**Tool subset**: none. Pure structured-output decision.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.2` | Deterministic with a small dose of variance. |
| `max_tokens` | `1200` | The JSON lists page names — a 13-page group is ~200 tokens. |
| `think:false` | implicit | Applies when the strong slot runs on a local Qwen 3.x backend; cloud strong backends reason via `reasoning_effort` instead. |

**Upstream filter** (decides when the model sees the prompt at all): a smart
wiki's pages are never nominated (its consumer is its sole writer), the
engine's own pages (a card, a rules page) are never nominated because
moving one would silently stop it being served, and a candidate whose pages
an earlier group already claimed this night is skipped — which is what stops
two overlapping handles from minting two wikis for one argument. A candidate
that **is one wiki, whole** — all its pages in that wiki, and no other page
there carrying a fact — is dropped before the question is put
([`already_fills_a_wiki`]): the argument has its home already, and the only
answers left would be a second wiki for it or a move into the wiki the pages
are in. A group that is nine of a wiki's fifteen pages is still asked. The verdict
memo (`rem_verdicts` kind `page_grouping`) keys on the rendered prompt, so a
settled "no" re-opens by itself as soon as the group changes. Applies share
`policy.auto_promote_cap` (default `5`) with the paragraph pass.

## Prompt

**`{locale}`** — the memory's own language, not a wiki's: the wiki this
group might found does not exist yet, and the pages come from several that
need not agree. Unanimity among the enrolled or nothing
(`default_memory_locale`), and this slot **writes memory** rather than
answering a live turn, so an undeclared locale resolves to **English** —
not to the "mirror the user's message" clause the conversational slots fall
back to.

```text
You are the REM page-group cartographer for mwe-mcp.

The engine has already done the counting. It found {pages} pages whose facts all share one {nominator}: `{handle}`. Your job is the judgement it cannot make — is that a SUBJECT AREA, something a reader would look for as one thing, or just a word that happens to appear in several places?

Say yes and those pages become a wiki of their own. Say nothing and they stay where they are, which is the right answer for a word that is merely common.

Two moves are available:
- "create": these pages ARE one subject area and it has no home yet. They become a new wiki, at the top level — a wiki is a shelf, not somebody's property, so it hangs under nothing and takes its name from the subject, not from whoever the pages happened to be filed under.
- "move": their subject IS one of the wikis listed below. They go in there. Prefer this over "create" whenever a home already exists — never found a second home for something that has one.

Rules:
- A shared word is not a subject area. `salute` on nine pages may be nine different people's health; the pages have to be about ONE thing a reader would go looking for.
- **Anything can be a subject area**, not only a person: a craft, a car, an animal, a project, a single day with a lot in it. Read what the pages say, not what kind of thing you expect.
- You may keep a SUBSET. If most of these pages are one subject and two are not, name the ones that are in "pages" and leave the others out — they stay where they are. Name only pages from the list below.
- Zero groups is the correct answer, and a common one. Return {"groups": []} without apology.

For a "create", describe the new wiki so it is not born blind to future placement and recall:
- "slug": short, lowercase, hyphenated.
- "title": human-readable, in the language named under LANGUAGE below.
- "style": its DOMINANT style default — "prosa" (interconnected knowledge), "prosa-tecnica" (short points, scanned), or "lista" (atomic records). A HINT, not a rule. Use null when genuinely mixed.
- "description": what BELONGS here, in one or two lines — the sentence a later pass reads when it is deciding whether a new fact goes in this wiki. Say the subject and its edges, not what the pages happen to hold today.

Reply STRICT JSON, no prose:
{"groups":[{"action":"create","slug":"<slug>","title":"<title>","style":"prosa"|"prosa-tecnica"|"lista"|null,"description":"<what belongs here>","pages":["<wiki>/<page.md>", …]},{"action":"move","target":"<existing wiki id>","pages":["<wiki>/<page.md>", …]}]}

Existing wikis you may file into:
{existing}

The pages, each as `<wiki>/<page.md>` with its fact count and a couple of verbatim excerpts:
{inventory}

LANGUAGE: {locale}
```
