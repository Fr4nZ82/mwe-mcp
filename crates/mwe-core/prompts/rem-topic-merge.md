---
name: rem-topic-merge
description: REM word-merge confirmer — two near-neighbour topic words, each with how many facts carry it; say whether they name the same thing, so the loser's facts can be rewritten to the better-attested word; strict JSON out, act-first
version: 1.0
default_version_at_bootstrap: v1.0
---

# Prompt: rem-topic-merge

The system prompt for the REM **word merge** sub-job
(`crate::topic_rank::merge_near_duplicates`) — the vocabulary's own
consolidation, beside the page merge that folds two pages. Loaded via
`mwe_core::prompts::render("rem-topic-merge", workdir, BUNDLED_REM_TOPIC_MERGE_MD, vars)`;
an operator override at `<workdir>/prompts/rem-topic-merge.md` wins.

## Runtime contract

- **Call site**: `crate::topic_rank::merge_near_duplicates`, once per nominated
  pair, inside the nightly cycle. Capped by `RemPolicy::topic_merge_cap`.
- **Model**: the `rem_dedup_semantic` / revisor slot (the low binary-classifier
  tier, shared by every REM confirmer sweep).
- **Who is nominated**: every pair of topic words whose embeddings sit at
  cosine ≥ `topic_rank::MERGE_THRESHOLD`, richest pair first — the one that
  would move the most facts is worth the night's first call. A word already
  merged away tonight is not argued about again.
- **Placeholders**: none. This document is the system half; the pair rides the
  turn as two lines (`WORD A: <word> — <n> facts`, then WORD B), the **winner**
  first — the better-attested word, alphabetical on a tie.
- **Output**: `{"same": true|false}`, read by the first-`same`-boolean scanner.
  Anything else reads as `false`, which is the safe answer: this pass would
  rather leave a duplicate than lose a distinction.
- **Runtime parameters**: temperature 0.0, max_tokens 60, the system half
  cached (it is the same text on every pair of the night).
- **Effect**: act-first. The loser's facts are rewritten to the winner in
  `fact_index`; no page is touched and no compile is queued — topic words live
  in `fact_index` alone.

## System prompt

```text
You are shown two words used to tag facts in one household's memory, with how
many facts carry each. Say whether they NAME THE SAME THING.

Yes only when one is a spelling, an inflection or a wordier form of the other,
so that a reader would never choose between them on purpose: `pressione` and
`pressione arteriosa`, `finanziamento` and `finanziamento auto`, `nutrizione`
and `alimentazione`.

No when they are two different things that happen to live in the same subject.
`nutrizione` and `idratazione` are both about food and drink and are NOT the
same word. `prezzo` and `garanzia` are both about buying a car. Merging those
loses the distinction the narrower word exists to make, and nothing gives it
back.

When you are unsure, answer no. A duplicate left standing costs one wasted
word; a wrong merge costs a distinction, silently, forever.

Answer with JSON and nothing else: {"same": true|false}
```
