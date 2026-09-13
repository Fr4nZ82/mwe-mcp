# The Dream console

**Nav: Dream.** `/dashboard/dream`

Two runs keep the memory in shape, and this page triggers either of them by
hand and shows the history of both. A manual trigger runs exactly what the
scheduler runs, so it never diverges, and the scheduler's own runs land in the
same history.

- **Light** — the frequent cycle. It places the waiting captures on pages,
  writes them as facts, and rewrites only the pages that changed. It runs on
  the cheap `ingest` slot, which is what lets it fire every few minutes. Then
  it **reads those pages back** (see below), on the `rem_dedup_semantic` slot.
- **Compile** — the page-writing half on its own, over the pages that changed.
  For isolating the compiler when a page is coming out wrong.
- **Full REM** — the nightly reorganisation on the strong models: dedup,
  auto-promote, archive, apply the parked comments, then rewrite the prose.

**Reading a page back.** Both runs finish by handing whole pages to a model —
the page's own prose, with every fact on it marked — and asking what the page
itself settles about them: an errand that was true of one day and is written as
though it stood for ever, a request another fact on the page has finished, a
passing spell filed as who somebody is, and at night also a claim the page
contradicts and the same claim written twice. Each correction is made and shown
on [Proposals](../user/proposals.md) as **A page read again**, with what the
model asked for that the engine refused, and why.

It costs one model call per page that changed — not per fact — and the two
ceilings are on [REM settings](rem-settings.md).

Below the buttons, **History · last 100 runs** lists what ran, when, and how it
went; a run opens to its log.

Three settings live elsewhere and this page says so:

- **When** the two scheduled runs fire is the **dream cadence** on the
  [Settings](settings.md) page.
- **How much** one cycle may touch is [REM settings](rem-settings.md).
- **Which model** each pass runs on is [the model slots](model-slots.md).
