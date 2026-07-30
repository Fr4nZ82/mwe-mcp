---
title: Reminders — a dated commitment announcing itself
area: design-notes
status: stable
last_review: "2026-07-30"
---

# Reminders

A commitment the memory already holds — *"Thursday at 5pm at the dentist"* —
is an ordinary fact with a validity window. Two surfaces act on it:

- **In a turn**, the recall block's `UPCOMING` slot pulls whatever falls due
  inside the look-ahead horizon, so any consumer taking a turn near the time
  surfaces it without being asked
  ([recall-pipeline.md](recall-pipeline.md)).
- **Between turns**, this: a sweep notices the commitment has come round and
  emits one `reminder_due` notice on the reverse channel, addressed to the
  person whose commitment it is.

## What this is not

**Not a scheduler.** *"Wake me at seven"*, *"post to the group every
Thursday"* are instructions to an assistant, and the assistant that was
asked owns them — mwe-mcp has no business holding a user's alarms. This
fires for exactly one thing: **a fact this memory already stores, carrying a
date, that has now come round.**

The line matters because it is also the answer to *"why not leave the whole
thing to the consumer's own scheduler, which already works?"* — and the
answer is not "ours is nicer". A job written the moment the user asked
freezes two things, the time and the wording. When a later turn says *"it
slipped to Friday"* the memory records the correction (`validity_edits`,
[ingest-pipeline.md](ingest-pipeline.md)) and the frozen job still fires on
Thursday with the old text. **Only the memory learns that the appointment
moved**, so only the memory can ring at the right time.

## What fires

`fact_type = "plan"` **with a concrete `valid_to`**, owned by a person
(`user:` — a group has no single inbox, an agent principal has none at all).

That pair is precise without any extra flag, and not by luck: the ingest
prompt forbids the one `plan` that must never ring — *"a shopping item is
NOT a TTL, it is closed later by a completing message, not by a timer"* — so
a consumable intention carries `valid_to: null` by construction. A `state`
(*"in Berlin this week"*) expires rather than falls due; a `rule` never
rings.

## When it fires — a derived instant, not a stored one

There is deliberately **no `remind_at` column**. The question was decided on
production data rather than from first principles: of the future-dated facts
on the first deployment, **87 % carried a `valid_to` on a day boundary** —
`00:00:00` or `23:59:59`, i.e. *a date with no hour in it*. Only 4 of 39
carried a real clock time.

Both obvious answers fail on that distribution. Firing *at* `valid_to` rings
at midnight for almost everything. Adding a separate firing timestamp leaves
those same facts **silent**, because nobody stated an hour to put in it and
the column would fill only for commitments restated in the future. The
missing datum was never a second timestamp — it is *whether anybody said an
hour*, and that is readable from the value already stored:

| stored `valid_to` | read as | fires |
|---|---|---|
| `00:00:00` or `23:59:59` | a **date**, no hour stated | that date at `reminders.day_hour_utc` (default 07:00) |
| any other clock time | an **instant** somebody stated | `valid_to` minus `reminders.lead_secs` (default 0) |

The hour is **UTC**, not local. The engine does no timezone arithmetic
anywhere — it hands the IANA zone to the classifier and lets the model
resolve wall-clock times — and giving this one sweep a timezone database to
itself would be the wrong place to start. For a deployment whose people
share a zone (a household, a team) one configured hour is exactly right; per
person local resolution is future work.

## What is said

The notice carries the fact's **own prose** inline, exactly as
`fact_minted_for_you` does, so the delivering agent says the thing without a
recall round-trip. This matters for the date-only case: if the turn stated
an hour that the classifier put in the prose instead of in `valid_to`, the
agent reads it there and says it. **The engine decides when to ring; the
fact decides what is said.**

Payload shape (`EventKind::ReminderDue`, wire `reminder_due`):

```jsonc
{
  "recipient_id": "user:alice",
  "due_at":   "2026-08-11T23:59:59Z",   // the stored valid_to
  "fires_at": "2026-08-11T07:00:00Z",   // what the policy resolved
  "facts": [ { "fact_id": "…", "wiki_id": "…", "body": "…" } ],
  "dashboard_path": "/dashboard/wiki/alice"
}
```

It mirrors `fact_minted_for_you` on purpose: a consumer that delivers one
delivers the other with the same parsing. Delivery scope is the reverse
channel's ordinary rule — the addressee's own token, the consumer's system
user, or a delegated sender
([tool-reference §events_poll](../protocol/tool-reference.md#events_poll-read-only)).

## Why a grace window instead of a backlog

The sweep fires only for an instant inside `(now - grace, now]`, six hours
by default. Without that bound the first run after this shipped would have
announced **every past dated commitment in the corpus** — hundreds of pings
for appointments long gone. The cost of the bound is that a window missed
entirely (the server was down) loses that reminder rather than delivering it
late, which is the right trade: a reminder that arrives the day after the
appointment is worse than none.

Firing once is the existing `(kind, fact_id)` probe
(`events::find_recent_event_for`) over a year, so a commitment rings once
even though the grace window spans many ticks.

## Runtime

`reminder_scheduler` in `mwe-mcp-server`: a 60-second tick after a 45-second
boot delay, disabled sections spawn no task at all. The policy is read once
at spawn — unlike the backup schedule there is no console editing it, so an
edit to `reminders:` takes effect on the next restart. Knobs:
[config-schema §reminders](../protocol/config-schema.md#reminders).
