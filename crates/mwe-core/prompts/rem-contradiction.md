---
name: rem-contradiction
description: REM contradiction sweep — given one freshly contradicted/superseded fact (and its successor, when any) plus a numbered list of OPEN candidate facts from this wiki family (the top-level wiki and every wiki nested under it), decide which candidates fall with it (the satellites of a cancelled event); strict JSON out; the cluster half of the temporal-validity model
version: 1.1
default_version_at_bootstrap: v1.1
---

# Prompt: rem-contradiction

The confirmation prompt for the REM **contradiction sweep** sub-job
(`crate::rem::run_contradiction_sweep`). Loaded via
`mwe_core::prompts::render("rem-contradiction", workdir, BUNDLED_REM_CONTRADICTION_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_contradiction_sweep`, once per freshly
  contradicted seed that has at least one similar open candidate
  (embedding-nominated, capped by `policy.contradiction_sweep_cap`).
- **Model**: the `rem_dedup_semantic` / revisor slot (low binary-classifier
  tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{contradicted_text}` (the seed — the fact that just
  fell), `{successor_text}` (what replaced it, or `(none)` for a pure
  closure), `{candidates}` (numbered open items:
  `fact_id · created_at · text`).
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. Empty `invalidated` = the cluster ends at the seed.
- **Runtime parameters**: temperature 0.1, max_tokens 400.

## Prompt

```text
You are the contradiction sweep inside mwe-mcp's nightly REM cycle. When a fact is contradicted — an event cancelled, a plan replaced — its SATELLITES often stay wrongly open: the itinerary days of a cancelled trip, the packing list, the preparations. The live ingest path closes the satellites it can see; you are the safety net with the wider view.

You receive ONE fact that was just CONTRADICTED (and, when it exists, the successor statement that replaced it), plus a numbered list of CANDIDATE facts from this wiki family (the whole wiki family: the top-level wiki and every wiki nested under it). Every candidate is still OPEN.

Decide which candidates (zero, one, or several) are INVALIDATED by the same contradiction — they only made sense while the contradicted fact held.

Rules:
- Be CONSERVATIVE. A candidate falls ONLY when its reason to exist was the contradicted fact: "Itinerario giorno 1: Louvre" falls with the cancelled Paris trip; "Galadriel è celiaca" does not fall with anything. Mere topic overlap is NOT invalidation.
- A candidate that survives the contradiction on its own merits (a durable preference, an independent plan) stays open — omit it.
- `valid_to` = when the invalidation happened, when you can say (usually the contradiction's own moment); otherwise null — the engine uses the seed's closure instant.
- `target` must be a fact_id copied EXACTLY from the candidate list. Never invent one.
- Closures here are act-first but revertable from the dashboard; when in doubt, leave the candidate open (an empty list is a fine answer).

CONTRADICTED FACT:
{contradicted_text}

REPLACED BY:
{successor_text}

CANDIDATES (open items — fact_id · created_at · text):
{candidates}

Output ONE strict JSON object, nothing else:
{"invalidated": [ { "target": "<fact_id from the list>", "valid_to": "<ISO-8601 Z>" | null }, ... ]}
```
