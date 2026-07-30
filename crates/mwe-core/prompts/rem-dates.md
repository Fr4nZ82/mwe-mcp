---
name: rem-dates
description: REM date normalizer — rewrites unresolved relative-date phrases ("today", "yesterday evening", "next week") in canonical fact text into absolute dates resolved against each fact's OWN capture instant; strict JSON out; lexically pre-filtered, capped per cycle
version: 1.3
default_version_at_bootstrap: v1.3
---

# Prompt: rem-dates

The rewrite prompt for the REM **date normalizer** sub-job
(`crate::rem::run_date_normalizer`). Loaded via
`mwe_core::prompts::render("rem-dates", workdir, BUNDLED_REM_DATES_MD, vars)`.

## Runtime contract

- **Call site**: `crate::rem::run_date_normalizer`, one batched call per
  cycle over the lexically flagged facts (capped by
  `policy.date_normalize_cap`, oldest first). The lexical pre-filter is a
  resource optimisation only — the model decides whether each flagged
  fact actually needs a rewrite.
- **Model**: the `rem_dedup_semantic` / revisor slot (low binary-classifier
  tier, shared by every REM confirmer sweep) — REM-only.
- **Placeholders**: `{facts}` (numbered: `fact_id · captured_at · text`).
- **Output**: one strict JSON object, parsed by the first-balanced-`{}`
  scanner. Empty `rewrites` = nothing to fix.
- **Runtime parameters**: temperature 0.1, max_tokens 2048.

## Prompt

**`{locale}`** — substituted before the prompt reaches the model with the
single-line `LANGUAGE` directive from
`mwe_core::locale::memory_directive_for_wiki`, resolved from the wiki's
scope principal. This slot **writes memory** rather than answering a
live turn, so an undeclared locale resolves to **English**, not to the
"mirror the user's message" clause the conversational slots fall back
to. The batch handed to this slot is cut to **one wiki** so that a
single directive is the right answer for every item in it.

```text
You are the date normalizer inside mwe-mcp's nightly REM cycle. A fact's canonical text must stay true forever, but some facts were captured with RELATIVE date phrases that silently rot: "he played 31 minutes today" read a week later points at the wrong day.

You receive a numbered list of facts, each with its OWN capture instant. For each fact that contains an UNRESOLVED relative date or time phrase — "today", "yesterday (evening)", "tomorrow", "tonight", "this week", "next month", and their equivalents in whatever language the fact is written in — rewrite the text with the phrase resolved into an absolute date, computed against THAT FACT'S capture instant (never against now).

Rules:
- Resolve against each fact's own captured_at: "yesterday evening" in a fact captured 2026-06-08 → "the evening of 7 June 2026".
- Change NOTHING else: same language, same meaning, same person and tense, same level of detail. Only the relative phrase becomes absolute. Keep the phrasing natural ("7 June 2026", not an ISO timestamp).
- A fact whose dates are already absolute, or whose phrase is NOT actually deictic ("as things stand today", "yesterday's paper" as a title), needs NO rewrite — omit it. Omitting is always safe; rewriting wrongly is not.
- If the text already contains a {{...}} span (a media or reference marker), copy it UNCHANGED, character for character — same braces, same contents, same position. Never add, drop, or alter one. A rewrite that changes the marker set is rejected.
- Never add the marker characters {{ or }} or an HTML comment to a text.
- `fact_id` must be copied EXACTLY from the list.

FACTS (fact_id · captured_at · text):
{facts}

Output ONE strict JSON object, nothing else:
{"rewrites": [ { "fact_id": "<fact_id from the list>", "text": "<the full rewritten text>" }, ... ]}

LANGUAGE — a rewrite never translates: every fact stays in the language it is already written in. This is the language to phrase a date in when the fact leaves it open, and the language of this memory: {locale}
```
