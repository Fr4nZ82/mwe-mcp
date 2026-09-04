---
name: rem-dedup
description: REM revisor — binary dedup confirmer between two facts (pair nominated by the jaccard band or the embedding-cosine channel), each shown with the page it lives on
version: 1.6
default_version_at_bootstrap: v1.5
---

# Prompt: rem-dedup

Prompt for the REM nightly **revisor** sub-job. Two facts
of the same wiki survived one of the two deterministic
nomination channels — the surface `policy.revisor_jaccard_min` ↔
`policy.revisor_jaccard_max` band, or the semantic
`policy.revisor_cosine_min` embedding floor (which catches a claim
restated with the subject spelled out vs elided, invisible to
n-grams) — and the `rem_dedup_semantic` / revisor slot (the low
binary-classifier tier, shared by every REM confirmer sweep) is asked
one binary question: do they encode the same fact, or are they
distinct? The orchestrator calls the prompt through the hybrid loader
[`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-dedup.md` wins when present, otherwise this
bundled default. See `crates/mwe-core/src/rem.rs` (around the
`run_revisor_jaccard` call site) for the runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth.

**Call site**: `crates/mwe-core/src/rem.rs::run_revisor_jaccard` —
search for `revisor_prompt(`. The `CompletionRequest::new(prompt)
.with_temperature(0.1).with_max_tokens(60)` block lives a few lines
below the prompt build.

**Placeholders** (substituted at render time by
`mwe_core::prompts::render`):

- `{new}` — text of the newer fact (survivor candidate)
- `{old}` — text of the older fact (loser candidate)
- `{new_page}` — where the newer fact lives (`wiki_id · source_path`),
  so a subject the page establishes and the prose elides is judged in
  context
- `{old_page}` — same, for the older fact
- `{subject_note}` — empty for an ordinary family; on an **agent's own**
  family (the scope root carries the `is_agent` marker) it carries the extra
  rubric line saying that WHO an episode was lived with is part of the fact,
  so two near-identical sentences about two different people stay two
  memories. Resolved once per wiki by `consolidation_scopes`, never per pair

**Output schema**: strict JSON, exactly one of `{"same": true}` or
`{"same": false}`. No prose. Parsed by `parse_llm_yes` in
`crates/mwe-core/src/rem.rs` (find first `{`, balance braces,
`serde_json::Value`, read `same` as bool). A parse failure means
"not the same" (fail-safe: don't merge when in doubt — preserves
information).

**Tool subset**: none. Pure binary classifier.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.1` | Binary decision, jaccard pre-filter already did the heavy lifting; the model just confirms or denies. |
| `max_tokens` | `60` | Reply is 18-20 tokens (`{"same": true}` / `{"same": false}`); 60 is comfortable headroom. |
| `think:false` | implicit | Applies when the revisor slot runs on a local Qwen 3.x backend (the local-workhorse profiles reuse the already-loaded workhorse for this slot). |

**Upstream filter** (decides when the model sees the prompt at all).
Three **structural** gates run first, and the model never sees what they
refuse — a rule it could weigh is a rule that fails on the day it matters:

- the two facts must sit on the same **class** of page. A channel page never
  pairs with an ordinary one, so a rules-page fact pairs only with another
  rules-page fact — and those pairs DO reach the model. What the gate refuses
  is the mixed pair: were the rule the loser, its content would survive only
  off `@rules.md`, outside the behaviour-rules channel;
- the would-be loser is never **identity-core** (`bio` + `salience: high`):
  a role or a relationship is changed by an explicit correction, never
  consolidated away in the background;
- the two **reader sets** must be identical (`subject ∪ allow ∪ sender`). Same
  content told by two people, or readable by two audiences, is two facts;
  merging them retires one principal's memory and leaves the survivor
  addressing the other's readers, with the loser's bytes gone from the page
  and no undo (founder, 2026-07-28).

Then the similarity nomination — the surface jaccard 6-gram
band, `policy.revisor_jaccard_min` (default `0.45`) ≤ score <
`policy.revisor_jaccard_max` (default `DEFAULT_DEDUP_THRESHOLD`), or
the semantic embedding floor, cosine ≥ `policy.revisor_cosine_min`
(default `0.80`, same-dimension non-identical vectors only). At or
above the jaccard max the pair is write-time dedup territory (the
capture scan, re-run by the light dream at promotion) and the revisor
leaves it alone; LLM confirms per cycle are capped by
`policy.revisor_examined_cap` (default `120`, logged when it trips), and
the pair scanner itself short-circuits once `RevisorReport.applied`
reaches `policy.revisor_cap` (default `30`) — the loop-break cap on
merges applied per cycle, the rest waiting for the next cycle.

## Prompt

```text
You are the REM dedup confirmer for mwe-mcp.
Two facts follow, each with the wiki page it lives on. Decide if they encode the *same* fact (paraphrase / restatement / very minor delta).
The page frames the subject: compiled prose routinely elides a subject the page itself establishes — "Born on 23 May 1984" on a person's own page states THAT person's birth date. Resolve such elisions against each fact's page before judging; two facts whose claims coincide once each subject is resolved ARE the same fact — INCLUDING when they live on different pages or wikis of the same family. Same page is NOT a precondition.
Example (split subject across pages, the flagship case): A = "He trains on Tuesday evenings" on Bruno's own page, B = "Bruno's karate class is on Tuesday evening" on the family wiki's sports page. Once each page's subject is resolved they state the SAME fact — the family scope pairs them across the two pages, so answer {"same": true}. (A ROLE or a RELATIONSHIP — "Bruno is Franz's father" — never reaches you: the engine keeps identity-core facts out of background dedup, so they change only by an explicit correction.)
{subject_note}
Reply STRICT JSON: {"same": true} or {"same": false}. No prose.

A (page: {new_page}):
{new}

B (page: {old_page}):
{old}
```
