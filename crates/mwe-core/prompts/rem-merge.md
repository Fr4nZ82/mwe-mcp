---
name: rem-merge
description: REM page-merge confirmer — are two near-synonym concept pages the same concept, and which name survives?
version: 1.5
default_version_at_bootstrap: v1.4
---

# Prompt: rem-merge

Prompt for the REM nightly **page-merge** sub-job — the cure
front of semantic page consolidation. A candidate pair of concept pages
in the same **consolidation scope** — a wiki, plus anything nested inside it
on disk — was *nominated* by structural signals (duplicate prose across the compiled bodies, or kinship between
the page names); the `rem_dedup_semantic` / revisor slot (the low-tier
confirmer shared by every REM verdict sweep) is asked the question the
signals cannot answer: are these two pages **the same concept**, and if so
which page name survives? A name resemblance is never sufficient on its
own — this call is the mandatory confirmation. The orchestrator calls the prompt through the
hybrid loader [`mwe_core::prompts::render`]: the override at
`<workdir>/prompts/rem-merge.md` wins when present, otherwise this
bundled default. See `crates/mwe-core/src/rem.rs` (around the
`merge_prompt` call site) for the runtime parameters.

## Runtime contract

**Call site**: `crates/mwe-core/src/rem.rs::run_page_merge` — search
for `merge_prompt(`. The `CompletionRequest` block lives a few lines
below the prompt build.

**Placeholders** (substituted at render time by
`mwe_core::prompts::render`):

- `{wiki_id}` — the scope label: the wiki both pages live in, or the two
  ids joined when the scope spans more than one
- `{signal}` — the structural signal that nominated the pair (audit
  context, not evidence)
- `{page_a}` — first page: wiki, slug, title, description, style, numbered facts
- `{page_b}` — second page: same shape
- `{subject_note}` — empty for an ordinary family; on an **agent's own**
  family (the scope root carries the `is_agent` marker) it forbids merging
  two per-person threads, which `slug_kinship` nominates by construction
  (`esperienze_franz` / `esperienze_bob` share a token). Resolved per
  wiki by `agent_wikis`, never per pair

**Output schema**: strict JSON, exactly
`{"merge": true, "survivor": "<slug>", "reason": "<one line>"}` or
`{"merge": false}`. `survivor` must be one of the two slugs shown.
A parse failure or an unknown survivor slug means "no merge"
(fail-safe: when in doubt, don't merge — a wrong merge is more
disruptive than a wrong split).

**Tool subset**: none. Pure classifier + name pick.

**Runtime parameters** (from the call site):

| Param | Value | Why |
|---|---|---|
| `temperature` | `0.1` | A verdict, not a composition. |
| `max_tokens` | `200` | Verdict + one-line reason. |

**Upstream filter**: candidate nomination (reviewer `duplicate_prose`
pairs + page-name kinship), capped by `policy.page_merge_cap` per
cycle — a resource cap, not a semantic gate; the semantic call is
this prompt's.

## Prompt

```text
You are the page-merge judge for mwe-mcp, a persistent memory shaped like a wiki.
Two concept pages from `{wiki_id}` follow. They were nominated by a structural signal ({signal}), which is NOT evidence by itself.

Decide whether they are the SAME concept — would a reader looking things up on one of them always want the other's content in the same place? Merge near-synonym pages about one topic (e.g. a trip's page and the same trip's planning page duplicating it). Do NOT merge pages that are merely related, or where one is a sub-topic that deserves its own page (a person vs one of their hobbies; a project vs its budget), or lists with different purposes — in particular an open-items list and its registry/log twin ("shopping" vs "shopping_log", a watchlist vs the watched log) are NEVER the same concept, however similar their records read: one holds what is still open, the other what was consumed.

Each block's `wiki:` line says where its page lives; the two are normally the same wiki. That is one memory about one subject — judge the CONCEPT exactly as above. When the same story is told twice, the page whose wiki is about that subject is the better long-term home.

{subject_note}

If they are the same concept, pick the SURVIVOR: the page whose slug is the better long-term home — the more canonical, established, well-formed name for the whole topic (usually the more general or the better-titled one, often the one with the richer description). The facts move to the survivor's page and wiki.

Reply STRICT JSON, nothing else:
{"merge": true, "survivor": "<slug of the surviving page>", "reason": "<one line>"}
or
{"merge": false}

PAGE A:
{page_a}

PAGE B:
{page_b}
```
