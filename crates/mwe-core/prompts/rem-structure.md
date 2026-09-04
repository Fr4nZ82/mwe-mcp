---
name: rem-structure
description: REM structural review — shown the whole forest from above (every standard wiki, its pages, each page's card and the principal its facts are mostly about), name the pages that sit in the wrong wiki and where each belongs; strict JSON out; act-first page re-home, with a receipt
version: 1.1
default_version_at_bootstrap: v1.0
---

# Prompt: rem-structure

The judgment prompt for the REM **structural review** sub-job
(`crate::rem::run_structure_review`). Loaded via
`mwe_core::prompts::render("rem-structure", workdir, BUNDLED_REM_STRUCTURE_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_structure_review`, **once per cycle** — this
  is the one pass that is not per-wiki or per-fact, because the mistake it
  looks for is only visible from above.
- **Model tier**: `rem_promotions` (the strong one). It is shown more than any
  other sub-job and it is the only one asked to weigh a whole forest.
- **Placeholders**: `{forest}` (every standard wiki with its pages — the
  inventory below), `{dropped}` (what the cap left out, named so the model
  knows its view is partial), `{cap}` (`RemPolicy::structure_review_cap`, the
  number of moves the applier will take — rendered rather than written into the
  body so the sentence the model reads and the number the code enforces are one
  value). **No locale directive**: the answer is a page
  address, a wiki id and one short sentence that never leaves the receipt —
  internal by the `PromptOutput` rule, exactly like its sibling `rem-refile`.
- **Runtime parameters**: temperature 0.1, max_tokens 900.
- **Effect**: each confirmed move is applied act-first via
  `promote::apply_pages_rehome_direct` with a receipt. The move stands — a
  structural change is never undone. Nothing is deleted and no fact is
  rewritten: the page's file moves, its rows follow, and the links that pointed
  at it are retargeted.

## Prompt

```text
You are the structural review inside mwe-mcp's nightly REM cycle. Every other pass looks at one wiki, one page or one fact. You are shown the whole memory from above, once, and you are asked one question: IS ANY PAGE IN THE WRONG WIKI?

WHY THIS CAN HAPPEN AT ALL. A page joins a wiki the moment it is born, and the engine picks that wiki from the first of its facts that happened to be listed. That is a guess made in one second with no view of anything else. You have the view. You are the only pass that can say the guess was wrong about the page AS A WHOLE — everything else moves one fact at a time, which asks the same question over and over and answers it differently each time.

WHAT A WIKI IS. Each one belongs to somebody: a person, or a group. A person's wiki holds what is about that person. A group's wiki holds what is about the collective and what its members share. The `subjects` line under each page tells you who that page's facts are actually about — that is the evidence, not the page's name and not the wiki it is sitting in.

WHAT COUNTS AS WRONG, and it is a short list:

1. **The page is about somebody else.** Its facts are mostly about a principal that is not this wiki's, and there is a wiki that IS theirs. A page about one person's medical treatment sitting in another person's wiki is the plain case.
2. **The page is shared life filed as private.** Its facts are mostly a group's — the household's shopping, the family's plans, what the members hold in common — and it sits in one member's wiki instead of the group's.
3. **The page is one person's, filed as the group's.** The mirror of 2: a page about one member alone, sitting in the group wiki.

WHAT IS NOT WRONG, and refusing these matters more than finding the ones above:

- **A page about a NON-ENROLLED person or thing is not misplaced by that fact alone.** Somebody's father, the dog, the car — they have no wiki of their own, so their pages live wherever the person who keeps them lives. Move such a page only when it clearly belongs to a group that carries responsibility for that subject, and the `subjects` line says so.
- **Topic overlap is not misplacement.** Two wikis holding pages about cooking is normal; each holds its own.
- **A page the regrouping pass moved is not misplaced.** A page that sits with the others on its subject is where that pass put it deliberately. Leave it.
- **Being large, badly named, or badly written is not being in the wrong wiki.** Other passes own those.
- **Doubt.** A move rewrites paths and retargets links. If you cannot say plainly why the page belongs elsewhere, it stays. An empty answer is the right answer most nights, and it is never a failure.

RULES:
- A page moves WHOLE or not at all. You cannot split it here; if only part of a page belongs elsewhere, leave the page alone and say nothing — the pass that moves facts will get to it.
- `to_wiki` must be a wiki_id copied EXACTLY from the inventory. Never invent one, never name a smart wiki (they are not in the list), never name the wiki the page is already in.
- `reason` is one short sentence saying what the page is about and whose wiki that makes it. It is kept on the receipt, which is what a person reads to judge whether the move was right — the move itself stands, so "misplaced" tells them nothing.
- Name at most {cap} moves. If more look wrong, take the {cap} you are surest of; the next cycle sees the rest.

{dropped}

THE FOREST:
{forest}

Output ONE strict JSON object, nothing else:
{"moves": [ { "page": "<wiki_id/page-file from the inventory>", "to_wiki": "<wiki_id from the inventory>", "reason": "<one sentence>" }, ... ]}
```
