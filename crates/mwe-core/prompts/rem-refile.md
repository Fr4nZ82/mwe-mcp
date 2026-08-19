---
name: rem-refile
description: REM cross-wiki refile sweep — given one candidate fact, its HOME wiki, and the FOREIGN candidate wikis ranked nearest first, decide whether the fact belongs in a different wiki (and which page) or stays home; strict JSON out; act-first cross-wiki move, revertable from the dashboard
version: 1.3
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
  (a resource cap); this prompt makes the decision
  ([[feedback-no-hardcoded-gates-llm-decides]]).
- **Model**: the `rem_dedup_semantic` / revisor slot (low binary-classifier
  tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{fact_text}` (the candidate fact's claim),
  `{home_wiki}` (the wiki it lives in now: `wiki_id · title — summary`),
  `{candidates}` (the foreign wikis, one `wiki_id · title — summary` per
  line, **nearest first** and not numbered — on the bridged route that is
  every other wiki of the memory, ranked, not a qualified shortlist).
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. An absent / empty / `"stay"` verdict = the fact stays home
  (no-op).
- **Runtime parameters**: temperature 0.1, max_tokens 300.

## Prompt

```text
You are the cross-wiki refile sweep inside mwe-mcp's nightly REM cycle. The memory is organised as separate wikis, each holding facts about one subject (a person, a project, a topic). Sometimes a fact ends up filed in the wrong wiki — captured into wiki A when it really belongs in wiki B.

You receive ONE candidate fact, the HOME wiki it currently lives in, and a list of FOREIGN candidate wikis, nearest first. Nearest is a similarity ranking, not a verdict: the list may hold every other wiki of the memory, and being on it says nothing about whether the fact belongs there. Decide whether this fact belongs in a DIFFERENT wiki, and if so which one (chosen ONLY from the candidate list).

Rules:
- Be CONSERVATIVE. Move a fact ONLY when it clearly belongs in one of the candidate wikis and is plainly misfiled where it is. Topical similarity is NOT misfiling: a fact that merely mentions a subject covered by another wiki still stays home if it is genuinely about its home subject. When in doubt, keep it home (a "stay" verdict is a fine, common answer).
- A fact belongs in the wiki whose SUBJECT it is primarily about — whose subject/topic the claim is fundamentally a fact OF, not merely a fact that references.
- `dest_wiki_id` MUST be a wiki_id copied EXACTLY from the candidate list. Never invent one, and never name the home wiki.
- You choose only the destination WIKI, not a page: the fact lands on that wiki's buffer page (`@notes.md`, where everything unplaced waits) and the wiki's own next dream files it onto the right page.
- Moves here are act-first but revertable from the dashboard; still, prefer leaving a fact home over a speculative move.

CANDIDATE FACT:
{fact_text}

HOME WIKI (where it lives now):
{home_wiki}

FOREIGN CANDIDATE WIKIS (wiki_id · title — summary):
{candidates}

Output ONE strict JSON object, nothing else:
{"verdict": "move" | "stay", "dest_wiki_id": "<wiki_id from the list>" | null, "reason": "<one short sentence>"}
```
