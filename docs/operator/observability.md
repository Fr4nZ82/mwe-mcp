# Watching it from outside

**No screen.** `GET /metrics`

[Health](health.md) is the page you open when you want to know how the server
is right now. This is the other half: the numbers your own monitoring reads on
its own schedule, so that a graph exists before you think to look at one, and
an alert reaches you when you are not looking at all.

Two addresses, both at the root of the server's address, and they answer
different questions:

- **`/health`** — *is it up?* Open to anybody, one line, described on the
  [Health](health.md) page.
- **`/metrics`** — *what is it doing?* Admin only, and the rest of this page.

## Getting in: an admin token

A scraper has no browser and no sign-in, so it holds a token in a file and
sends it on every request. Issue one, on the machine the server runs on:

```bash
mwe-mcp token-issue --sender <your admin id> --device prometheus --is-admin
```

It is an ordinary token: it shows up on the **Tokens** page, and revoking it
there stops the scraping. It has to be an admin's, because the answer below
describes the whole deployment — the models it calls, what they cost, the
credentials that talk to it — and that is not a reader's business.

Then the scrape itself:

```yaml
scrape_configs:
  - job_name: mwe-mcp
    metrics_path: /metrics
    static_configs:
      - targets: ["memory.example:8742"]
    authorization:
      credentials: "<the token>"
```

Without the token the answer is `401`; with a token that is not an admin's,
`403`. The body is Prometheus's own text format, nothing is cached, and each
scrape is a handful of small database queries — no model is called, which is
the reason there is no "are the model slots reachable" number here. Reaching a
model costs a round trip and, on a paid provider, money; a scrape happens every
few seconds with nobody watching, so it may not spend. What the slots did is
below instead, taken from the record of calls that actually happened.

## Reading the windows in the names

Everything the server records is swept on a retention window, so a running
total taken from one of those tables would drop when the sweep runs — and a
counter that drops is read as a restart. So no number here pretends to be an
all-time total, and each one says its window in its own name:

- **`…_today`** counts the current UTC day and starts again at midnight UTC.
  That is the same day the daily budget uses, so the spend you see here and the
  spend on **Usage** are the same figure. `rate()` and `increase()` handle the
  midnight restart the way they handle a server restart.
- **`mwe_dream_runs`** counts the runs the Dream console still keeps, which is
  the newest hundred — a window, not a total.
- Everything else is a reading of right now.

## What is published

**The process**

| Name | What it is |
|---|---|
| `mwe_build_info` | Always 1; the version of the running binary is the label. |
| `mwe_uptime_seconds` | Seconds since this server process started serving. |
| `mwe_database_bytes` | `engine.db` on disk together with its write-ahead sidecars. |

**What came in**, labelled by `device` — the device label on the calling
credential, which the **Tokens** page fills in with the consumer's name.

| Name | What it is |
|---|---|
| `mwe_turns_today` | Turns ingested today. |
| `mwe_tool_calls_today` | Tool calls of every kind today. |
| `mwe_tool_errors_today` | How many of those failed. |

**What the models did**, labelled by `slot`, `backend` and `model`. The
embedder is recorded under the slot name `embedding`.

| Name | What it is |
|---|---|
| `mwe_llm_calls_today` | Model calls today. |
| `mwe_llm_call_errors_today` | How many the backend refused or could not answer. |
| `mwe_llm_latency_ms_today` | Milliseconds spent waiting; divide by the calls for the mean. |
| `mwe_llm_tokens_today` | Tokens moved, `direction="prompt"` for what was sent and `"completion"` for what came back. |

**What it cost** — the same figures as **Usage**, in the price list's currency.

| Name | What it is |
|---|---|
| `mwe_spend_today` | Estimated money spent today on paid model calls. |
| `mwe_spend_limit` | Your daily budget. **Absent when you have set none** — which is how an alert tells "no ceiling" from "a ceiling of zero". |
| `mwe_spend_stopped` | 1 while paid calls are being refused because today's budget is spent. |
| `mwe_spend_unpriced_calls_today` | Calls on a model with no rate in the price list: their tokens are counted and their cost is not, so while this is above zero the budget is looser than it looks. |

**How fast memory answered**, over the newest hundred recorded recalls.

| Name | What it is |
|---|---|
| `mwe_recall_ms_p50` | The median recall, in milliseconds. |
| `mwe_recall_ms_p90` | What the slowest tenth exceeded. |
| `mwe_recall_ms_max` | The slowest one. |

**What the night pass did**, labelled by `kind` (`full`, `light`, `compile`).

| Name | What it is |
|---|---|
| `mwe_dream_last_run_timestamp_seconds` | When the most recent run of that kind finished. |
| `mwe_dream_last_run_ok` | 1 if it succeeded, 0 if it failed. |
| `mwe_dream_runs` | Runs by kind and outcome, over the hundred the console keeps. |

A family with nothing behind it is left out rather than published as zero. On a
server nobody has used today there are no `…_today` lines at all, and that
absence is the honest answer.

## The one to alert on

If you set up a single alert, make it
`mwe_dream_last_run_timestamp_seconds{kind="full"}`. A nightly cycle that quietly
stops running is the failure that shows up nowhere else — the server keeps
answering, the memory keeps growing, and nothing tidies it. Everything else on
this page you will notice eventually; that one you will not.
