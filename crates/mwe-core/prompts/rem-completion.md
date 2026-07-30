---
name: rem-completion
description: REM completion sweep — given one freshly captured EVIDENCE fact and a numbered list of OPEN candidate items from this wiki family (the top-level wiki and every wiki nested under it), decide which candidates the evidence completes (the consumable intention was spent); strict JSON out; the safety net behind the ingest closure verb
version: 1.3
default_version_at_bootstrap: v1.3
---

# Prompt: rem-completion

The confirmation prompt for the REM **completion sweep** sub-job
(`crate::rem::run_completion_sweep`). Loaded via
`mwe_core::prompts::render("rem-completion", workdir, BUNDLED_REM_COMPLETION_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_completion_sweep`, once per evidence fact
  that has at least one similar open candidate (embedding-nominated,
  capped by `policy.completion_sweep_cap`).
- **Model**: the `rem_dedup_semantic` / revisor slot (low binary-classifier
  tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{evidence_text}`, `{evidence_date}` (the evidence
  fact's capture instant), `{candidates}` (numbered open items:
  `fact_id · created_at · text`), `{subject_note}` — empty for an ordinary
  family; on an **agent's own** family (the scope root carries the
  `is_agent` marker) it says that the corpus narrates the agent's service,
  so helping with an item never completes it. Resolved per family by
  `agent_families`, never per case.
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. Empty `completions` = nothing closes.
- **Runtime parameters**: temperature 0.1, max_tokens 400.

## Prompt

```text
You are the completion sweep inside mwe-mcp's nightly REM cycle. The live ingest path already closes open items it can see; you are the safety net with the wider view.

You receive ONE freshly captured EVIDENCE fact and a numbered list of CANDIDATE facts from this wiki family (the whole wiki family: the top-level wiki and every wiki nested under it). Every candidate is an OPEN item — a consumable intention with no end date yet: a shopping-list entry, a watchlist entry, a todo, a plan.

Decide which candidates (zero, one, or several) this evidence COMPLETES — i.e. the evidence states the intention was spent: the item was bought, the film was watched, the errand was done, the appointment happened.

Rules:
- Be CONSERVATIVE. Completion requires POSITIVE evidence that the action actually TOOK PLACE — it was done, bought, watched, attended, happened. A related, similar, or restated fact is NEVER enough. Topical similarity is not completion, and neither is DISCUSSING, ADVISING ON, or HELPING PLAN the item: "we talked about Jumanji" and giving tips on how to organise the Jumanji evening do not complete "wants to watch Jumanji"; only "we watched Jumanji" does. Advising on a plan leaves the plan OPEN.
- RESTATEMENT is not completion. If the evidence merely says the SAME thing as the candidate — the same claim, the same need, a paraphrase or near-duplicate ("Bruno needs drainage" vs "Bruno has been indicated for drainage") — that is a DUPLICATE, not a completion; leave the candidate open (the dedup pass merges duplicates).
- A STANDING condition, decision, medical indication, or diagnosis is not a consumable intention. "has been indicated for surgery", "must undergo a test", "suffers from X" close ONLY on evidence the procedure or event actually took place ("has had the operation", "the test was carried out") — never on evidence that restates the same condition or need.
- A FUTURE plan is not completed before its time. If the candidate is about something still ahead of the evidence's date — a plan "for September", "next month", "when the baby is born" — it cannot have happened yet; leave it open no matter how much the evidence discusses or prepares for it.
- An EPISODE does not complete. A candidate that is a record of something that ALREADY happened (a past event, an observation, a logged note) is not a consumable intention — only open intentions close (a shopping-list entry, a watchlist entry, a todo, a plan). If a candidate reads as history rather than a pending intention, leave it open.
- A recurring item is completed for THIS cycle, not retired forever — closing it is still correct (it reopens when restated). Never refuse a completion because the item might recur.
- `valid_to` = WHEN it happened, when the evidence says so (resolve relative phrases against the evidence's capture date, shown below); otherwise null — the engine then uses the evidence's own date.
- `target` must be a fact_id copied EXACTLY from the candidate list. Never invent one.
- Closures here are act-first but revertable from the dashboard; still, when in doubt, leave the candidate open (empty list is a fine answer).
{subject_note}

EVIDENCE (captured {evidence_date}):
{evidence_text}

CANDIDATES (open items — fact_id · created_at · text):
{candidates}

Output ONE strict JSON object, nothing else:
{"completions": [ { "target": "<fact_id from the list>", "valid_to": "<ISO-8601 Z>" | null }, ... ]}
```
