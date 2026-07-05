---
name: query-seeds
description: Query-seed extractor — turns a free-text memory-search query into topic and entity seeds that steer the recall navigator's entry-point fan when the caller supplies none; strict one-JSON-object output
version: 1.0
default_version_at_bootstrap: v1.0
---

# Prompt: query-seeds

The system prompt for the `query-seeds` LLM function — the `wiki_navigate`
seed extractor. Loaded via
`mwe_core::prompts::render("query-seeds", workdir, BUNDLED_QUERY_SEEDS_PROMPT_MD, vars)`:
the bundled default embedded by `include_str!` is the floor; an override at
`<workdir>/prompts/query-seeds.md` wins when present.

## Runtime contract

- **Call site**: `crates/mwe-core/src/recall_nav.rs::extract_query_seeds` —
  the `wiki_navigate` tool's seed cascade, fallback **B**: used when the caller
  supplies no explicit `topics`/`owners` (**C**), and before degrading to
  principal + RAG seeds only (**A**). One completion per `wiki_navigate` call,
  not one per hop. Ingest gets these seeds from its classifier; the standalone
  search tool has no classifier in the loop, so this is a small dedicated
  extraction, not the heavy ingest classifier.
- **Model**: the `navigator` LLM slot — **strong-but-cheap** tier (a light
  per-query extraction on the same slot the funnel uses).
- **Placeholders**: none — rendered with an empty variable list.
- **Output schema**: one strict JSON object —
  `{ "topics": [ "…" ], "entities": [ "…" ] }`. The Rust binding is
  `QuerySeedsJson` in `recall_nav.rs`; extracted entity names are resolved
  against enrollment (user id / alias → `user:`, group id → `group:`), and a
  name that does not resolve folds into `topics` where it can still
  substring-match a card. Best-effort by contract: any load / LLM / parse
  failure returns empty seeds and the caller degrades to fallback **A**.
- **ACL**: the extractor sees only the caller's own query text — no memory
  content and no markers ever reach it.

## Prompt body

```text
You extract recall seeds from a user's memory-search query, to steer a wiki
navigator toward the right starting pages.

Return STRICT JSON and nothing else:

{"topics": ["..."], "entities": ["..."]}

- `topics`: the salient subjects the query is about — short words or phrases to
  look up (e.g. "birthday", "work project", "allergies"). Lowercase. Omit filler
  words and the verbs of asking ("tell me", "what about").
- `entities`: names of specific people, groups, or named things the query refers
  to, written as they appear (e.g. "Morgana", "the family", "Acme Corp"). Names
  only.

If the query has none of a kind, return an empty array for it. Never invent
topics or entities that are not grounded in the query.
```
