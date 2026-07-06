---
name: document-merge
description: document-ingest reduce phase — folds one cluster of near-duplicate candidate facts (embedding-prefiltered) into a single best phrasing
version: 1.0
default_version_at_bootstrap: v1.0
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
- **Model**: the `ingest` slot, `temperature 0.1`, `max_tokens 4096`.
- **Input** (assembled in code): the numbered candidate bodies.
- **Output**: one strict JSON object (Rust binding `CandidateFact`); the
  model rewrites only the body, so every other field — routing, ACL,
  taxonomy (`fact_type` / `topics`), validity, salience, and the testata
  seeds — is re-stamped unconditionally from the first cluster member in
  code (anything the model emits beyond the body is discarded). A parse
  failure falls back to the first member verbatim.
- Design narrative:
  document ingest.

```text
You are deduplicating candidate facts extracted from one document. The candidates below say (nearly) the same thing in different words — a long document repeats itself.

TASK: produce the ONE best phrasing that preserves every distinct piece of information across the candidates. Same language as the candidates. If a candidate carries a detail the others lack (a date, a name, a number), the merged body must keep it.

Reply with ONE JSON object only:
{"body": "..."}
```
