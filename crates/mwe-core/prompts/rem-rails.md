---
name: rem-rails
description: REM rail writer — given one page that almost nobody can reach and a list of candidate destinations drawn from four different sources, decide the ONE link worth writing from it, or none; strict JSON out; act-first, the compiler imposes the chosen rail on the next rewrite
default_version_at_bootstrap: v1.0
---

# `rem-rails` — the REM decides one link

The system prompt for the REM **rail writer**
(`crate::rem::run_rail_writer`). Loaded via
`mwe_core::prompts::render("rem-rails", workdir, BUNDLED_REM_RAILS_MD, vars)`;
an operator override at `<workdir>/prompts/rem-rails.md` wins.

## Runtime contract

- **Call site**: `crate::rem::run_rail_writer`, once per nominated page, inside
  the nightly cycle. Capped by `RemPolicy::rail_writer_cap` like every other
  sub-job.
- **Model**: the `rem_promotions` slot — the strong structural-judgment tier.
  `temperature` low, JSON out.
- **Who is nominated**: a page carrying **fewer links than
  `rem::PAGE_RAIL_BUDGET`**. A page with no links at all leads: it can only be
  reached by a search landing on one of its own facts, and from it a reader can
  go nowhere.
- **Placeholders**: `{page}` (the nominated page — its address, its card and
  its facts), `{links}` (the links it already carries, or `none`),
  `{candidates}` (the destinations, one per line, each tagged with the source
  that offered it — see `crate::candidates::CandidateSource`).
- **Output**: one strict JSON object
  `{ "link": "<slug>" | "none", "instead_of": "<slug>" | null, "why": "…" }`.
  `link` must be copied from the candidate list. `instead_of` may name only a
  rail **the REM itself wrote** on this page, and is ignored otherwise: a link
  the page's own prose carries belongs to whoever wrote it — the Cronista, or
  the owner editing in Obsidian — and this pass does not take a page's own
  sentences away.

## What happens to the answer

The chosen rail is parked on the compilation plan
(`planner::park_authored_rail`) and becomes a **mandatory** recommended link at
that page's next rewrite. So the model is not proposing prose: it is deciding
that the next time this page is written, it will say why it points there.

Act-first, with a receipt: the nightly cycle cannot stop and wait for an
answer, and what the REM did is read back from the receipts on the dashboard.

## Prompt

```text
You are the REM rail writer for mwe-mcp — a Markdown memory made of pages, where a reader arrives at a page because a search matched one of its facts, and from there can go ONLY where that page's [[wikilinks]] lead.

The page below is under-linked. Decide the ONE link most worth writing from it, or answer none.

WHAT MAKES A LINK WORTH WRITING — this is the whole judgement:
- A link is worth writing exactly when a search phrased in THIS page's words would NEVER have found the destination. Ask it of every candidate: would somebody asking about what is on this page have landed on that page anyway? If yes, the link buys nothing — the search already reaches it.
- The case this exists for: a page about cooking dinner says the person is lactose intolerant; the page holding the lactase-pill routine shares not one word with "dinner". No similarity will ever join them, and a reader who follows the link joins them instantly.
- Relatedness is NOT the test. Two pages that mention the same person, or that read alike, do not need a link — a search finds them both.
- **`none` is a real answer and a common one.** A link nobody needs costs a clause of prose on every future rewrite and buys nothing. Write one only when you can say who would need it and why they would never get there otherwise.

READING THE CANDIDATE LIST — each line opens with WHY that page is in front of you, and the reasons are not equal:
- `far` — it resembles this page in NOTHING. These are the candidates this rule is really about: read them first.
- `same-turn` — it holds facts said in the same conversation as this page's.
- `same-people` — it holds facts about, or told by, the same people.
- `near` — its description resembles this page's. **This is the weakest**: a search from here probably reaches it already, so a `near` link is usually the one NOT to write.

REPLACING ONE — only when this page already has links and you judge your choice better than one of them. `instead_of` may name only a link this pass wrote before (they are marked in the list below); a link the page's own prose carries is not yours to remove.

Reply STRICT JSON, no prose around it:
{"link": "<candidate slug>" | "none", "instead_of": "<slug>" | null, "why": "<one short sentence>"}

THE PAGE:
{page}

LINKS IT ALREADY CARRIES:
{links}

CANDIDATE DESTINATIONS:
{candidates}
```
