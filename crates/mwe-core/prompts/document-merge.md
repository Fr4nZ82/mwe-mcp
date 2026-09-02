---
name: document-merge
description: document-ingest reduce phase — folds one cluster of near-duplicate candidate facts (embedding-prefiltered) into a single best phrasing
version: 1.1
default_version_at_bootstrap: v1.1
---

# Prompt: document-merge

The system prompt for the document-ingest **reduce** phase
(`crate::document::reduce_candidates`). Loaded via
`mwe_core::prompts::load("document-merge", workdir, BUNDLED_DOCUMENT_MERGE_MD)`.

## Runtime contract

- **Call site**: `crate::document::reduce_candidates` — one call per
  multi-member cluster (clusters come from a deterministic
  embedding-cosine prefilter at `merge_threshold`; singletons never spend
  a call).
- **A cluster is always one audience.** The prefilter refuses to join two
  candidates whose reader sets differ, before you see them, so the members
  you are given are readable by exactly the same people. This is why the
  re-stamp below is safe, and it is not your judgement to make: if two
  sentences say the same thing to different people, they never arrive here
  together.
- **Model**: the `ingest` slot, `temperature 0.1`, `max_tokens 4096`.
- **Input** (assembled in code): the numbered candidate bodies.
- **Output**: one strict JSON object (Rust binding `CandidateFact`); the
  model rewrites only the body, so every other field — routing, ACL,
  taxonomy (`fact_type` / `topics`), validity, salience, and the `style`
  seed — is re-stamped unconditionally from the first cluster member in
  code (anything the model emits beyond the body is discarded). A parse
  failure falls back to the first member verbatim.

**`{locale}`** — substituted before the prompt reaches the model with the
single-line `LANGUAGE` directive `crate::locale::render_memory_language_directive`
builds from the job's **subject principal** (`enrollment::locale_for_principal`
— a user's own declared locale, or the one every member of a group declared).
`document::process_job` resolves it once and hands the same directive to all
three document slots, which is why a foreign-language document still lands in
memory in the language of the person it is about. This slot **writes memory**
rather than answering a live turn, so an undeclared locale resolves to
**English** — not to the "mirror the user's message" clause the conversational
slots fall back to.

```text
You are deduplicating candidate facts extracted from one document. The candidates below say (nearly) the same thing in different words — a long document repeats itself.

TASK: produce the ONE best phrasing that preserves every distinct piece of information across the candidates. If a candidate carries a detail the others lack (a date, a name, a number), the merged body must keep it.

Reply with ONE JSON object only:
{"body": "..."}

LANGUAGE: {locale}
```
