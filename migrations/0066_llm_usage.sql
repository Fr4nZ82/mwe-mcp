-- Per-call token ledger for the internal LLM slots — the store behind
-- the dashboard's Usage & spend page.
--
-- WHY A TABLE AND NOT THE TRAINING SPOOL. Every completion already
-- returns a `CompletionUsage` (prompt / completion / cached-prefix
-- tokens) and the training spool already writes it out, so the counter
-- existed — but the spool is the wrong store for this question in three
-- independent ways. It is **off by default**, so the numbers do not
-- exist unless somebody turned on a feature built for something else.
-- It records **whole prompts and completions**, which means recalled
-- memory content of every user the deployment serves — a privacy class
-- nobody should have to accept in order to learn what their memory
-- costs. And it is per-day JSONL, so answering "what did June cost"
-- means parsing gigabytes of prose to sum four integers per line.
--
-- A row here is ~120 bytes, carries no prompt text at all, and is
-- always on. One `GROUP BY` answers every question the page asks.
--
-- WHAT A ROW MEANS. One LLM call, recorded after the provider answered
-- (or refused). The decorator that writes it sits in
-- `LlmFunctionConfig::build_backend`, the single place every slot and
-- every transport is built through — the same seam the training spool
-- uses — so no call site knows this table exists.
--
-- THE FOUR TOKEN COLUMNS AND WHY THEY ARE NOT THREE. Providers split
-- the prompt three ways and price the parts differently: plain input,
-- prefix-cache **read** (roughly a tenth of input), prefix-cache
-- **write** (roughly a quarter more than input). `prompt_tokens` is
-- the total — inclusive of both cache buckets on every backend, since
-- `anthropic_usage` folds Anthropic's side-by-side accounting into
-- Gemini's inclusive convention — so plain input is
-- `prompt_tokens - cached_prompt_tokens - cache_write_tokens` and each
-- bucket can be priced at its own rate. Collapsing the write into
-- plain input would be the one mistake that matters: a prompt whose
-- prefix changes every call pays the write premium *every time*, which
-- is exactly the pathology a spend page exists to catch, and it would
-- look like ordinary input.
--
-- Any of the four is NULL when the backend did not report it (Ollama
-- reports no cache accounting at all; Gemini reports no writes). NULL
-- means "not reported", 0 means "measured zero" — the same distinction
-- `CompletionUsage` documents, preserved rather than flattened.
--
-- `billing` IS NOT DERIVABLE FROM `backend`. The same provider is paid
-- for three different ways: per token against an API key (`api`), out
-- of a flat subscription the token count does not move (`subscription`
-- — the reserved `api_key_env: claude-code` login), or not at all
-- because the model runs on this machine (`local`). Money is only
-- estimated for `api`; tokens are counted for all three, because on the
-- other two they are still the measure of load.
--
-- `source` AND `tag` ARE THE CLEAN-MONTH COLUMNS. A month's numbers are
-- worth nothing if experiments are mixed into them, and the process
-- knows which it is: `serve` is the live server, `rem-cli` / `eval-cli`
-- are an operator running a cycle or a recall evaluation by hand.
-- `tag` is the explicit marker (`MWE_USAGE_TAG`) for a deliberate
-- experiment, NULL for ordinary traffic. Neither is inferred after the
-- fact — that was the whole problem.
--
-- `error` holds the failure class of a call that never returned usage.
-- It costs one column and answers "how many calls failed", which is a
-- spend question whenever a provider refusal buys a retry.
--
-- RETENTION. Swept by `usage::UsageLedger` against
-- `usage.retention_days` (default 400 — over a year, so this July can
-- be compared with next July before the first one is dropped).

CREATE TABLE llm_usage (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                   TEXT    NOT NULL,  -- RFC-3339 UTC, written when the call returned
    slot                 TEXT    NOT NULL,  -- LlmFunction::yaml_key(): 'ingest' | 'cronista' | ...
    backend              TEXT    NOT NULL,  -- 'ollama' | 'anthropic' | 'gemini' | 'openrouter'
    model                TEXT    NOT NULL,  -- model id as the backend reports it
    kind                 TEXT    NOT NULL,  -- 'complete' | 'chat'
    billing              TEXT    NOT NULL,  -- 'api' | 'subscription' | 'local'
    source               TEXT    NOT NULL,  -- 'serve' | 'rem-cli' | 'eval-cli' | 'other'
    tag                  TEXT,              -- MWE_USAGE_TAG; NULL = ordinary traffic
    prompt_tokens        INTEGER,           -- total prompt, cache buckets included
    completion_tokens    INTEGER,
    cached_prompt_tokens INTEGER,           -- prefix-cache READ (subset of prompt_tokens)
    cache_write_tokens   INTEGER,           -- prefix-cache WRITE (subset of prompt_tokens)
    latency_ms           INTEGER,
    error                TEXT               -- failure class, NULL on success
);

-- The page groups by day and reads a trailing window, so every query
-- starts from a range scan on `ts`.
CREATE INDEX idx_llm_usage_ts ON llm_usage(ts);
