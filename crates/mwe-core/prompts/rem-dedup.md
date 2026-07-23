---
name: rem-dedup
description: REM revisor — binary dedup confirmer between two facts (pair nominated by the jaccard band or the embedding-cosine channel), each shown with the page it lives on
version: 1.2
default_version_at_bootstrap: v1.2
---

# Prompt: rem-dedup

Prompt for the REM nightly **revisor** sub-job. Two facts
in the same family scope survived one of the two deterministic
nomination channels — the surface `policy.revisor_jaccard_min` ↔
`policy.revisor_jaccard_max` band, or the semantic
`policy.revisor_cosine_min` embedding floor (which catches a claim
restated with the subject spelled out vs elided, invisible to
n-grams) — and the `rem_dedup_semantic` / revisor slot (the low
binary-classifier tier, shared by every REM confirmer sweep) is asked
one binary question: do they encode the same fact, or are they
distinct? The orchestrator calls the
prompt through the hybrid loader [`mwe_core::prompts::render`]: the
override at `<workdir>/prompts/rem-dedup.md` wins when present,
otherwise this bundled default. See the
REM cycle page
for the narrative and `crates/mwe-core/src/rem.rs` (around the
`run_revisor_jaccard` call site) for the runtime parameters.

## Runtime contract

Operational specs that ship next to the prompt body so they can't
drift from it. Code is the source of truth; the
REM cycle page keeps the
design narrative.

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
| `think:false` | implicit | Applies when the revisor slot runs on a local Qwen 3.x backend (the local-workhorse profiles reuse the already-loaded workhorse for this slot); see the REM cycle page, runtime section. |

**Upstream filter** (decides when the model sees the prompt at all):
either deterministic nomination channel — the surface jaccard 6-gram
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
The page frames the subject: compiled prose routinely elides a subject the page itself establishes — "È nato il 23 maggio 1984" on a person's own page states THAT person's birth date. Resolve such elisions against each fact's page before judging; two facts whose claims coincide once each subject is resolved ARE the same fact — INCLUDING when they live on different pages or wikis of the same family. Same page is NOT a precondition.
Example (split identity across pages, the flagship case): A = "È il padre di Franz" on the family wiki's own page, B = "Bruno è il padre di Franz" on Bruno's sub-wiki page. Once each page's subject is resolved they state the SAME fact — the family scope pairs them across the two pages, so answer {"same": true}.
Reply STRICT JSON: {"same": true} or {"same": false}. No prose.

A (page: {new_page}):
{new}

B (page: {old_page}):
{old}
```
