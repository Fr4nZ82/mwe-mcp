---
title: Cross-consumer reminder delivery
status: planned
---

# 8. Cross-consumer reminder delivery

**Low priority.** A dated commitment ("Monday 9:30 dentist") is already modelled with
no special machinery: it is an ordinary `plan` fact carrying a **validity window**
(`valid_to` ≈ the deadline). Recall prioritizes it while imminent and down-prioritizes it
once past (the [recall validity down-rank](../design-notes/recall-pipeline.md)) — **no timer, no sweep** on the memory
side. A *recurring* commitment ("Matteo has karate every Mon/Thu 18:00") is a **stable** fact
(always true), not a decaying one. So the schedule needs no cron type and no lifecycle engine.

Because the commitment lives in the **user's memory wiki**, not in the consumer that created
it, the schedule is **consumer-agnostic by construction**. This item is only about the
**active-fire / delivery** gap on top of that.

## The two cases

- **Case A — an agent is already in a turn near the time.** Covered by the recall block's
  **due-soon slot** ([ingest-pipeline.md](../design-notes/ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate), group 2 — complete): facts with an imminent
  firing/validity window are pulled deterministically by closeness to *now*, so any consumer
  doing recall surfaces the commitment and can remind the user. Nothing to build here beyond 2d.
- **Case B — nobody is talking to the user at the due time, but a ping is still wanted.** This
  needs an **active fire** at the due time — something must "wake up". The memory side is
  deliberately passive (no sweep), so the fire is **delegated**.

## Decided framing

- mwe-mcp stays the **durable home** of the *what / when / recurrence* and exposes a
  **"what's due soon"** surface; it is **not** the timer.
- The **active consumer materializes fires into its own scheduler** (precedent: the
  OpenClaw → NanoClaw cron migration treats cron jobs as portable data). **Delivery** — a PC
  notification, TTS, a chat message — is the consumer's own skill, not mwe-mcp's concern.
- Push **from** mwe-mcp (a POST to the consumer's webhook) is an **opt-in fallback**, only for
  consumers that have no scheduler of their own.
- **Correction to the earlier "default-off" stance:** delivery routes to the user's **current
  delivery consumer**, *not* the consumer that created the commitment. A reminder created via a
  standard (voice) consumer on Saturday must reach the user through whichever consumer is active
  on Monday (e.g. the smart agent). The concept is a per-user **delivery target**, never
  per-creator.

## Open decisions

- **`due_at` / `remind_at` vs `valid_to`.** They are not the same: `valid_to` = "the fact stops
  being true"; a reminder *fires at a precise time*, possibly **before** `valid_to`. Likely a
  distinct firing timestamp on the fact, not a reuse of `valid_to`.
- **Designating the "current delivery consumer"** for a user — last-active, or an explicit
  operator setting?
- **Fire mechanism** — pull-and-materialize on session start (smart consumers with their own
  scheduler) vs mwe-mcp opt-in push (consumers without one), or both as layered fallback.
- **Shape of the "due-soon" surface** a consumer pulls (a recall query over facts with an
  imminent firing time).

## Dependencies

Builds on the shipped temporal-validity model ([memory-model.md](../concepts/memory-model.md), especially the recall-side
validity signal 7c), the shipped due-soon slot (group 2 — [recall-pipeline.md](../design-notes/recall-pipeline.md)) and
[item 3](3_context-model.md) (per-turn context / host adapters, where delivery would be
wired). Gated on a concrete consumer actually needing the active fire — Case A covers most of
the value without it.
