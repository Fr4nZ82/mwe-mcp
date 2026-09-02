---
name: rem-refile
description: REM cross-wiki refile sweep — given one candidate fact, its HOME wiki, and the FOREIGN candidate wikis each tagged with why it is offered and carrying the pages it already holds, decide whether the fact belongs in a different wiki and on which of that wiki's pages, or stays home; strict JSON out; act-first cross-wiki move, final
version: 1.4
default_version_at_bootstrap: v1.3
---

# Prompt: rem-refile

The judgment prompt for the REM **cross-wiki refile sweep** sub-job
(`crate::rem::run_refile_sweep`). Loaded via
`mwe_core::prompts::render("rem-refile", workdir, BUNDLED_REM_REFILE_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_refile_sweep`, once per candidate fact.
  Two routes reach it and they differ: the deterministic cosine
  pre-filter (the fact embeds materially closer to a foreign wiki than to
  its home wiki), and the **reviewer bridge** — a fact last night's review
  parked as `cross_subject_bloat`, which skips the margin because the
  reviewer already nominated it. Either way the pre-filter only NOMINATES
  (a resource cap); this prompt makes the decision.
- **Model**: the `rem_dedup_semantic` / revisor slot (low binary-classifier
  tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{fact_text}` (the candidate fact's claim),
  `{home_wiki}` (the wiki it lives in now: `wiki_id · title — summary`),
  `{candidates}` (the foreign wikis, best-cosine first and not numbered, each
  line opening with the reason it is offered — `same-people`, `same-turn` or
  `near`; see `rem::ForeignReason` —
  on the bridged route that is every other wiki of the memory, ranked, not
  a qualified shortlist. Each is a `wiki_id · title — summary` line
  followed by an indented `pages:` line listing the pages that wiki
  already holds, reserved names excluded).
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. An absent / empty / `"stay"` verdict = the fact stays home
  (no-op), and so does a `dest_page` the destination does not already
  hold — the code checks it against that wiki's own page list before it
  moves anything.
- **Runtime parameters**: temperature 0.1, max_tokens 300.

## Prompt

```text
You are the cross-wiki refile sweep inside mwe-mcp's nightly REM cycle. The memory is organised as separate wikis, each holding facts about one subject (a person, a project, a topic). Sometimes a fact ends up filed in the wrong wiki — captured into wiki A when it really belongs in wiki B.

You receive ONE candidate fact, the HOME wiki it currently lives in, and a list of FOREIGN candidate wikis, each with the pages it already holds. Being on the list says nothing about whether the fact belongs there. Decide whether this fact belongs in a DIFFERENT wiki, and if so which one and on which of that wiki's existing pages (both chosen ONLY from the list you are given).

Every candidate line opens with WHY that wiki is in front of you, and the three reasons carry different weight:
- `same-people` — that wiki IS this fact's subject's own, or its author's. This is the strongest reason on the list: a fact filed away from its own subject is exactly what this sweep exists to find.
- `same-turn` — that wiki holds a fact said in the same conversation. Worth reading; two things said in one breath often belong in one place, and often do not.
- `near` — that wiki's facts merely sound like this one. This is the weakest reason: sounding alike is not belonging together, and most `near` candidates are a "stay".

Rules:
- Be CONSERVATIVE. Move a fact ONLY when it clearly belongs in one of the candidate wikis and is plainly misfiled where it is. Topical similarity is NOT misfiling: a fact that merely mentions a subject covered by another wiki still stays home if it is genuinely about its home subject. When in doubt, keep it home (a "stay" verdict is a fine, common answer).
- A fact belongs in the wiki whose SUBJECT it is primarily about — whose subject/topic the claim is fundamentally a fact OF, not merely a fact that references.
- `dest_wiki_id` MUST be a wiki_id copied EXACTLY from the candidate list. Never invent one, and never name the home wiki.
- `dest_page` MUST be one of the pages listed under that wiki, copied character for character. You may NOT invent a page name: a name that wiki does not already have is refused and the fact stays home. If none of its pages is a sensible home for this fact, answer "stay" — a fact on the wrong page of the right wiki is not an improvement.
- A wiki whose `pages:` line says `(none yet)` cannot receive the fact. Do not name it.
- Moves here are act-first and final; prefer leaving a fact home over a speculative move.

CANDIDATE FACT:
{fact_text}

HOME WIKI (where it lives now):
{home_wiki}

FOREIGN CANDIDATE WIKIS (wiki_id · title — summary, then the pages that wiki already holds):
{candidates}

Output ONE strict JSON object, nothing else:
{"verdict": "move" | "stay", "dest_wiki_id": "<wiki_id from the list>" | null, "dest_page": "<one of that wiki's listed pages>" | null, "reason": "<one short sentence>"}
```
