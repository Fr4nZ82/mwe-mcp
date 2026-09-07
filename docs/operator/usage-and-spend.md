# Usage and spend

**Nav: Usage.** `/dashboard/admin/usage`

Every call this deployment made to a model or to the embedder, counted per
call. Tokens are the measurement; money is an **estimate** against the rates
you enter yourself, and it is shown only for the models you priced.

## Today, and the daily budget

With no budget set the page says so: today's metered spend is shown and
nothing stops it.

Set **Daily budget** and **Warn at (% of the budget)** — the warning line
defaults to 80 % — and press **Save budget**. The figure is in the currency of
your price list, counted per UTC day, and it covers **metered calls only**: a
model on a flat subscription, or one running on this machine, never moves it.
Leave the field empty to remove the budget.

Once a budget exists, the Today card shows what was spent, the percentage, and
one of four states:

- **spending** — under the warning line.
- **past the warning line** — a notice has gone out to the operator as well.
- **stopped** — paid model calls are being refused. User turns still answer:
  they degrade and say nothing was saved, and the nightly cycle skips its
  round and says why. Raise the budget, or press **Unlock for today**.
- **unlocked for today** — you lifted the stop by hand. Spending continues past
  the budget, and the budget applies again at 00:00 UTC. **Re-apply the budget
  for today** puts it back.

If some of today's calls ran on models you have not priced, the page says how
many: as far as the budget is concerned they spend nothing, so price them to
make it bind.

## History

**Today · 7 days · 30 days · 90 days · All**, showing production traffic only
— hand-run cycles and anything a tagged process produced are left out. The
ledger starts at the first model call this server makes, and every call after
that is in it. Rows are kept for **400 days** (`usage.retention_days` in
`mwe-mcp.config.yaml`, `0` to keep everything).

## Price list

Rates **per 1M tokens**, in whatever currency you are billed in. Nothing is
assumed on your behalf: published rates change and your contract may not be the
published one, so money appears only where you entered a rate. A model id may
be a `prefix*` wildcard and the longest match wins whatever order the rows are
in. An omitted cache rate falls back to the input rate, which gives you an
upper bound rather than a discount nobody promised. Clear a model id to drop
its rate.

The bundled embedder is not in this ledger at all: it runs inside this process,
so there is no request to count and no bill to explain.

Related: which model serves each slot is [The six model slots](model-slots.md);
the full prompt recorder is [the training spool](training-spool.md), which
records the prompts and not what they cost.
