---
title: LLM usage ledger — counting the tokens, and pricing them only where a rate exists
area: design-notes
status: implemented
last_review: "2026-07-29"
---

# LLM usage ledger

Every call to an internal LLM slot lands as one row in `llm_usage`
(migration `0066`). The dashboard's **Usage & spend** page
(`/dashboard/admin/usage`) is the only reader.

The question it exists to answer is the one a self-hoster asks first and
could not previously ask at all: *what does running this memory cost,
and which part of it is spending?*

Related pages:

- [`llm-functions.md`](llm-functions.md) — the slots being counted, and
  the prefix-cache accounting the token columns come from.
- [`engine-db-and-migrations.md`](engine-db-and-migrations.md#the-migration-ledger)
  — the `llm_usage` DDL in the migration ledger.
- [`../protocol/config-schema.md`](../protocol/config-schema.md) — the
  `usage:` and `llm_pricing:` sections.
- [`dashboard.md`](dashboard.md) — where the page sits in the console.

## Why a table, and not the training spool

The counting was never the missing part. Every completion returns a
`CompletionUsage`, it is logged, and
[`training_spool`](llm-functions.md) already wrote it out per call. What
did not exist was a way to **ask a question of it**.

The spool is the wrong store for this question in three independent
ways, and each one alone would be enough:

- It is **off by default**, so the numbers do not exist unless the
  operator enabled a feature built for something else (distillation
  datasets).
- It records **whole prompts and completions**, which on this product
  means the recalled memory of every user the deployment serves. Nobody
  should have to accept that exposure in order to learn what their own
  machine costs.
- It is per-day JSONL, so "what did June cost" means parsing gigabytes
  of prose to sum four integers per line.

A ledger row is about 120 bytes, carries **no prompt text at all**, and
is always on. One `GROUP BY` answers every question the page asks.

## Where it hooks in

`usage::maybe_wrap` decorates the built backend inside
`LlmFunctionConfig::build_backend` — the single seam every slot and
every transport (MCP ingest, REM cycle, dashboard chat, document worker)
is constructed through, the same seam the training spool uses. No call
site knows the ledger exists, and a slot added later is recorded without
anyone remembering to.

The handle is process-wide (`usage::install_global`), installed by the
server binary right after it opens `engine.db` and **before** the first
backend is built — the decorator is attached at build time, not at call
time, so an install that came later would silently record nothing.

Two things the decorator deliberately does *not* do:

- **Health probes are not recorded.** `health_check` delegates
  untouched; the dashboard pings every slot on every load of the LLM
  page, and a row per ping would drown the ledger in calls nobody made.
- **The provider's error message is not stored.** A failed call is
  recorded with its *class* (`rate_limit`, `auth`, `invalid`, …) and
  nothing else: a provider's error text can quote the prompt back, and
  no prompt text belongs in this table.

Failures *are* recorded, which is where the ledger parts company with
the spool. A spool wants pairs to learn from and a failure has no
completion; a ledger has to explain a bill, and a provider that refuses
a request is often refused again on the retry.

## What a row cannot say, on purpose

The row carries slot, backend, model, how it is paid for and which
process made the call — and nothing about content. It **cannot** tell
you which user's turn cost what. That is deliberate: attributing spend
per person would make an operator surface out of something that reads
across every wiki's ACL, and the number an operator actually needs is
the deployment's.

## The prompt is three quantities, not one

Providers price plain input, prefix-cache **reads** and prefix-cache
**writes** at three different rates. `CompletionUsage` therefore carries
both cache buckets beside the total, and the ledger stores all four
columns:

```
plain input = prompt_tokens − cached_prompt_tokens − cache_write_tokens
```

Collapsing the write into plain input is the one simplification that
matters, and it fails in both directions. A well-cached workload looks
several times more expensive than it is, because a read costs about a
tenth of input and would be charged at full rate. And the opposite,
worse case becomes invisible: **a prompt whose prefix changes on every
call pays the write premium every time and never collects the discount**
— the exact pathology the prompt-caching work warns about — while
looking identical to ordinary input once the two are added together.

`NULL` means *not reported*, `0` means *measured zero*, and the
distinction is preserved rather than flattened. Ollama reports no cache
accounting at all; Gemini caches implicitly and reports reads but never
writes.

## `billing` is not derivable from the backend tag

The same provider is reached three ways and only one of them turns
tokens into money:

| `billing` | what it means | cost |
|---|---|---|
| `api` | metered against an API key | tokens × the configured rate |
| `subscription` | the reserved `api_key_env: claude-code` login | zero |
| `local` | a model running on this machine | zero |

Tokens are counted in all three cases. On a flat plan they stop being
money and stay the **measure of load**, which is what says whether the
plan will hold; on a local model they are latency and electricity.

Resolved at build time by `LlmFunctionConfig::billing`, where both the
backend tag and the key selector are in scope. Deriving it from the
backend name alone would bill a subscription slot as if it were metered.

## The clean-month columns

A month's numbers are worth nothing when experiments are mixed into
them, and **after the fact the two cannot be told apart**. So the
separation is recorded when the call is made, never inferred later:

- `source` — `serve` (the running server), `rem-cli` (a hand-run
  `mwe-mcp rem run-*`), `eval-cli` (`mwe-mcp recall eval`), `other`.
  Free, and nobody has to remember to set it. Note that the *scheduled*
  cycles inside a running server are `serve`: the distinction is who
  asked, not what ran.
- `tag` — `MWE_USAGE_TAG` marks a whole process as a deliberate
  experiment. `NULL` for ordinary traffic.

The page's **Production traffic only** link subtracts everything that is
not untagged `serve`.

## Money is estimated, never observed

No provider tells us what it charged. The cost column is
`tokens × a rate the operator configured`, which makes it exactly as
right as the price list.

**`llm_pricing:` is empty by default and stays that way.** A bundled
price list would be wrong in three ways at once: published rates move
without warning, a real contract can differ from the published rate, and
the currency is not ours to assume. A product that invents a price the
operator never declared is guaranteed to be wrong about somebody else's
money.

So the rule the page follows is: **money is shown only where a rate was
configured.** With no price list at all it renders tokens and says
nothing about cost — not zeros, not dashes — and prints the YAML that
would turn the columns on. This is also why the public demo needs no
special case: it simply has no price list, like any fresh install, and
there is no branch anywhere that names either of them.

Two rules inside the price list, both chosen so a half-filled entry
degrades honestly:

- An omitted `cached_input` or `cache_write` **falls back to the input
  rate**, so the estimate is an upper bound rather than a discount
  nobody promised.
- The **longest matching wildcard wins**, so a specific `claude-opus-*`
  entry beats a catch-all `claude-*` whatever order they appear in. A
  config file that silently depends on line order is a trap nobody can
  see.

## Reading it back: one query, several views

`usage::buckets` groups at the finest grain the page needs — day, slot,
backend, model, billing, source, tag — and every view (by slot, by
model, by day, by month, by whose traffic) is a fold over those in
memory. One query, one pass.

The grain is chosen so that **the model is still known**, because the
model is what carries the price.

### A fold has no single model, and therefore no single price

`usage::fold` sums the counters and keeps a dimension **only when every
folded bucket agrees on it**, blanking it otherwise. That rule is not
tidiness; it is the guard on `UsageBucket::estimated_cost`.

Keeping the first bucket's dimensions — the obvious implementation, and
the one that shipped first — makes a fold across several models carry
*one* model id, so pricing it charges the whole group at whichever model
sorted first. Measured on a seeded month, that printed **€34.05 against
the correct €11.54 four lines below it**, same currency, both plausible.
Every per-day, per-month and per-traffic figure was wrong the same way;
only the per-slot rows happened to be right, because those groups held
one model each.

Blanked, the same call answers `None` — "not priced" — instead of a
confident wrong number, and a caller that wants a group's money has to
do the only correct thing: price each bucket and add
(`usage::total_cost`). The page's `cost_cell` does exactly that, and
reports what the sum leaves out (`+N unpriced`) rather than hiding it.

The defect was found by looking at the rendered page and noticing two
numbers that had to be equal and were not. No test over routes, status
codes or markup could have caught it, because nothing was malformed.

## Retention

`usage.retention_days` (default **400**) is swept by the ledger itself:
at most once per UTC day, checked with an atomic day counter so a
long-running server cannot grow without limit and an insert does not pay
for a full-table `DELETE` scan. `0` disables the sweep.

Over a year on purpose: the first question anybody asks of a spend page
after twelve months is "how does this July compare with last July".

## Days are UTC

Grouping is `substr(ts, 1, 10)` over an RFC-3339 UTC timestamp. Provider
billing periods are not local either, and a ledger that silently used
the host's timezone would file a 01:00 Rome call under the previous day
for one reader and not another. The page says so on the by-day table.

## What is not counted here

Embedding calls. The bundled embedder runs locally, and a remote
embedding backend is configured separately (`embedding:`); neither goes
through `build_backend`, so neither appears in this ledger.
