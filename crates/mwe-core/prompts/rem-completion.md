---
name: rem-completion
description: REM completion sweep — given one freshly captured EVIDENCE fact and a numbered list of OPEN candidate items from this wiki family (the top-level wiki and every wiki nested under it), decide which candidates the evidence closes and how — completed (the intention was spent) or retracted (it was abandoned); strict JSON out; the safety net behind the ingest closure verb
version: 1.4
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
- **Placeholders**: `{evidence_text}`, `{evidence_date}` (the instant the
  evidence began being true), `{candidates}` (numbered open items:
  `fact_id · began · text`, the claim whole rather than previewed, dated the
  same way), `{subject_note}` — empty
  for an ordinary family; on an **agent's own** family (the scope root
  carries the `is_agent` marker) it says that the corpus narrates the
  agent's service, so helping with an item never completes it. Resolved
  per family by `agent_families`, never per case.
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. Empty `completions` = nothing closes.
- **Runtime parameters**: temperature 0.1, max_tokens 400.

## Prompt

```text
You are the completion sweep inside mwe-mcp's nightly REM cycle. The live ingest path already closes open items it can see; you are the safety net with the wider view.

You receive ONE freshly captured EVIDENCE fact and a numbered list of CANDIDATE facts from this wiki family (the whole wiki family: the top-level wiki and every wiki nested under it). Every candidate is an OPEN item — a consumable intention with no end date yet: a shopping-list entry, a watchlist entry, a todo, a plan.

Decide which candidates (zero, one, or several) this evidence CLOSES, and HOW. There are exactly two ways an open intention closes, and they are opposites:

- **completed** — the intention was SPENT: the item was bought, the film was watched, the errand was done, the appointment happened.
- **retracted** — the intention was ABANDONED: it was cancelled, called off, given up, or the person said they will not do it after all. The action did NOT take place and now never will.

Both end the item. Which of the two it is changes nothing about how careful you must be, and the rules below apply to both unless one names the other.

Rules:
- Be CONSERVATIVE, and symmetrically. **completed** requires POSITIVE evidence that the action actually TOOK PLACE — it was done, bought, watched, attended, happened. **retracted** requires POSITIVE evidence that it will NOT take place — «I told him I cannot go», «we cancelled the booking», «I have given up on it». A related, similar, or restated fact is NEVER enough for either. Topical similarity is not completion, and neither is DISCUSSING, ADVISING ON, or HELPING PLAN the item: "we talked about Jumanji" and giving tips on how to organise the Jumanji evening do not complete "wants to watch Jumanji"; only "we watched Jumanji" does. Advising on a plan leaves the plan OPEN.
- RESTATEMENT is not completion. If the evidence merely says the SAME thing as the candidate — the same claim, the same need, a paraphrase or near-duplicate ("Bruno needs drainage" vs "Bruno has been indicated for drainage") — that is a DUPLICATE, not a completion; leave the candidate open (the dedup pass merges duplicates).
- A STANDING condition, decision, medical indication, or diagnosis is not a consumable intention. "has been indicated for surgery", "must undergo a test", "suffers from X" close ONLY on evidence the procedure or event actually took place ("has had the operation", "the test was carried out") — never on evidence that restates the same condition or need.
- A FUTURE plan is not completed before its time. If the candidate is about something still ahead of the evidence's date — a plan "for September", "next month", "when the baby is born" — it cannot have happened yet; leave it open no matter how much the evidence discusses or prepares for it. It CAN be retracted before its time: a plan for September cancelled in June is over in June.
- DIFFICULTY IS NOT ABANDONMENT. "it will be hard to make it", "I might not manage", "we may have to postpone" leave the item OPEN — a doubt is not a decision. Postponing is not retracting either: an item moved to another date is still going to happen. Retract only on a statement that it is off.
- An EPISODE does not complete. A candidate that is a record of something that ALREADY happened (a past event, an observation, a logged note) is not a consumable intention — only open intentions close (a shopping-list entry, a watchlist entry, a todo, a plan). If a candidate reads as history rather than a pending intention, leave it open.
- A recurring item is completed for THIS cycle, not retired forever — closing it is still correct (it reopens when restated). Never refuse a completion because the item might recur.
- `valid_to` = WHEN it happened, when the evidence says so (resolve relative phrases against the evidence's date, shown below); otherwise null — the engine then uses the evidence's own date.
- `outcome` is `"completed"` or `"retracted"` — which of the two ways above this candidate closed. Omitting it means `"completed"`.
- `valid_to` on a retraction is WHEN it was called off, not when it would have happened.
- `target` must be a fact_id copied EXACTLY from the candidate list. Never invent one.
- Closures here are act-first and final; when in doubt, leave the candidate open (empty list is a fine answer).
{subject_note}

EVIDENCE (dated {evidence_date}):
{evidence_text}

CANDIDATES (open items — fact_id · began · text):
{candidates}

Output ONE strict JSON object, nothing else:
{"completions": [ { "target": "<fact_id from the list>", "outcome": "completed" | "retracted", "valid_to": "<ISO-8601 Z>" | null }, ... ]}
```
