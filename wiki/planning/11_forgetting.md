---
title: Organic forgetting — graduated decay of aged memory
status: planned
---

# 11. Organic forgetting

Opened by the maintainer while deciding the registry twin for consumed list items
(2026-06-11): **forgetting is a feature, not a bug** — after months or years a
memory should not retain everything at full resolution. That decision (consumption
events move to a registry page *with expiry*; the routing shipped — see the
[registry twin](../design-notes/ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture))
is the first concrete consumer; the maintainer flagged that the same principle
plausibly applies elsewhere too.

This is distinct from the consumer-relayed **forget gesture** (shipped — a user
asks to forget something specific, now; see the
[closure verb](../design-notes/ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture))
— this group is about *organic* decay: nobody asks, time and disuse do the
asking.

## What exists today (verified against the code 2026-06-11)

- **Per-fact usage tracking** — `fact_index.last_recall_at` +
  `recall_count_30d`, bumped on every recall hit
  ([`fact_index.rs`](../../crates/mwe-core/src/fact_index.rs)).
- **The REM archive detector**
  ([`rem::run_archive_detector`](../../crates/mwe-core/src/rem.rs)) — weekly,
  whole pages with no recall hit past `policy.archive_inactivity` (default 365
  days) emit `archive_proposals` rows, capped by `policy.archive_cap`.
  **Emitter-only**: the approval flow and the apply step (the "reaper") were
  deferred and never built — today nothing ever actually archives
  ([`archive.rs`](../../crates/mwe-core/src/archive.rs) module docs).
- **Per-fact validity** — `valid_from`/`valid_to` + `decay_reason` give every
  fact a lifecycle, and the closure verbs write it since 2026-06-11: the
  [ingest closures](../design-notes/ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture),
  the supersede chokepoint, and the REM completion/contradiction sweeps.

## Proposed model — recommendation, not yet decided

Graduated compression, mirroring human memory: detail fades, the gist remains.

1. **Condense** — a REM pass over aged, closed-window, low-recall facts replaces
   N episodic facts with one gist fact written through the normal compile path
   ("through spring 2026 Galadriel handled the grocery runs"); the originals
   close with `decay_reason = "condensed"` and leave the rendered page (rows
   stay in `fact_index`, so nothing is destroyed). Act-first with the usual
   receipt + dashboard notice + revert window; the LLM judges what condenses —
   no hardcoded semantic gates, the code provides cadence and resource caps.
2. **Archive** — whole stale pages move under `_archive/` via the existing
   proposal flow (operator-gated: lower-confidence, page-sized blast radius).
3. **Delete** — never automatic; operator/GDPR tooling only (roadmap 5g).

Signals: age, closed validity windows, `recall_count_30d`/`last_recall_at` —
all already stored. The judge is the LLM; thresholds stay resource caps.

Not observable on the 23-day dogfood corpus (the windows are month/year-scale),
so this group is sequenced **after** the dogfood lot.

## Steps

- [ ] 11a — Decide the decay model (the graduated-compression recommendation
  above, or an alternative shape — a product call: how much history should the
  memory keep narrating?)
- [ ] 11b — Build the registry-page expiry — the "scadenza" half of the
  registry-twin decision, this group's first concrete consumer
- [ ] 11c — Build the archive reaper: the approval view + apply step for the
  existing `archive_proposals` emitter
- [ ] 11d — Extend the decay signals beyond whole-page inactivity (closed
  validity windows, recall counts, age) and set the act-first vs operator-gated
  line per tier

## Open decisions

- **The decay model itself (11a)** — the maintainer endorsed the principle
  ("dimenticare è una feature"), not yet this specific shape.
- **Condensation authority** — act-first like merge/auto-promote (recommended:
  it is revertible and fact-sized), while archival stays operator-gated
  (page-sized)? Or both gated?
- **Cadence + caps** for the condensation pass (the archive detector precedent
  is weekly).
- **What "expiry" means for the registry-twin pages concretely** —
  condense-then-archive is the recommendation.
