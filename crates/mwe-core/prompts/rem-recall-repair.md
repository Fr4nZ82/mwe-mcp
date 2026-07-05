---
name: rem-recall-repair
description: REM recall-repair sub-job — given one confirmed recall miss (the query the user had to restate, and the fact memory held but recall did not surface), decide whether re-filing the fact into a different wiki would make it reachable, or whether no local repair applies; strict JSON out; the verdict is only a CANDIDATE — a gold-set gate replay must prove it before anything commits
version: 1.0
default_version_at_bootstrap: v1.0
---

# Prompt: rem-recall-repair

The proposal prompt for the REM **recall-repair sub-job**
(`crate::rem::run_recall_repair`) — the repair stage of self-correcting
REM. Loaded via
`mwe_core::prompts::render("rem-recall-repair", workdir, BUNDLED_REM_RECALL_REPAIR_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_recall_repair`, once per pending
  `recall_misses` row (the judge-free restated-known-fact signal — the
  user re-said something memory already held and that turn's recall did
  not surface it). The miss detection is deterministic; this prompt only
  proposes the repair ([[feedback-no-hardcoded-gates-llm-decides]]).
- **The proposal is NOT the decision.** Every `move` verdict is replayed
  through the gold-set gate (`crate::recall_gate`) on a scratch copy:
  it commits only if the missed fact actually becomes reachable for the
  missed query and no gold-set case regresses.
- **Model**: the `rem_dedup_semantic` / revisor slot (low
  binary-classifier tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{query}` (the user's restatement — the turn that
  missed), `{fact_text}` (the fact recall failed to surface),
  `{home_wiki}` (where it lives: `wiki_id · page`), `{candidates}`
  (numbered non-smart wikis: `wiki_id · title — summary`).
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. An absent / empty / `"stay"` verdict = no local repair
  (the recurrence path may still queue an operator notice).
- **Runtime parameters**: temperature 0.1, max_tokens 300.

## Prompt

```text
You are the recall-repair pass inside mwe-mcp's nightly REM cycle. The memory is organised as separate wikis, each holding facts about one subject (a person, a project, a topic). Recall finds facts two ways: embedding similarity, and navigation that enters wikis whose subject or topics match the conversation.

You receive ONE confirmed recall MISS: the user asked something (QUERY) and the memory already held the answer (FACT), but recall failed to surface it — the user had to repeat themselves. The most repairable cause is a misfiled fact: it lives in a wiki that the navigation for this kind of query never enters.

Decide whether moving the FACT to a different wiki (chosen ONLY from the candidate list) would make it reachable for queries like this one, or whether it should stay where it is.

Rules:
- Be CONSERVATIVE. Propose a move ONLY when the fact plainly belongs in one of the candidate wikis — when its subject matter is that wiki's subject and its current home is why the query could not reach it. When in doubt, answer "stay" (a common, fine answer: not every miss has a filing cause).
- A fact belongs in the wiki whose SUBJECT it is primarily about — whose owner/topic the claim is fundamentally a fact OF, not merely a fact that references it.
- `dest_wiki_id` MUST be a wiki_id copied EXACTLY from the candidate list. Never invent one, and never name the home wiki.
- You choose only the destination WIKI, not a page: the fact lands on that wiki's foundation page and the wiki's own next dream files it onto the right page.
- Your verdict is a CANDIDATE only: a replay gate will verify that the move actually makes the fact reachable for this query without regressing anything, and only then does it commit (act-first, revertable from the dashboard).

QUERY (what the user asked — the turn that missed):
{query}

FACT (what memory held but recall did not surface):
{fact_text}

HOME (where the fact lives now):
{home_wiki}

CANDIDATE WIKIS (wiki_id · title — summary):
{candidates}

Output ONE strict JSON object, nothing else:
{"verdict": "move" | "stay", "dest_wiki_id": "<wiki_id from the list>" | null, "reason": "<one short sentence>"}
```
