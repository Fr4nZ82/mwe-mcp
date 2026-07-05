---
title: Self-correcting REM — hindsight recall-failure repair
status: in-progress
---

# 15. Self-correcting REM

Opened by the maintainer from a design conversation (2026-06-20): REM already
*maintains* the memory (dedup, promote, merge, completion/contradiction sweeps —
[rem-cycle.md](../design-notes/rem-cycle.md)), but it never learns from its **own
recall failures**. The idea: a REM pass that, with hindsight over the day's real
traffic, finds where recall surfaced the wrong facts — or missed a fact it already
held — classifies each failure by root cause, and applies the **lowest-blast-radius**
repair, with every proposed fix gated by replaying the
[recall-eval gold set](../design-notes/recall-pipeline.md#the-recall-eval-harness--recall_eval).
**The core loop shipped 2026-07-05** (15a–15f); this page keeps the model rationale
and the residue (15g + the alias-repair rung). Current state:
[recall-pipeline.md §hindsight log](../design-notes/recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal)
· [rem-cycle.md §recall-repair](../design-notes/rem-cycle.md#recall-repair-sub-job--self-correcting-rems-repair-stage).

The discipline that makes it safe — and that distinguishes it from the
[RL-learned policies of 9b](9_extensions.md) — is that **the judge of a fix is never
an LLM's opinion; it is the gold-set regression**. A pass that cannot prove it
improved recall without regressing it does not commit. "Consolidation that cannot
make recall worse", extended from *don't lose facts* to *repair the ones recall
fumbled*.

Distinct from [organic forgetting (11)](11_forgetting.md) — that decays aged memory;
this repairs recall *quality* on live memory. Sibling REM evolutions, both observable
only with longitudinal real traffic, so both follow the first-consumer cutover
([group 4](4_first-consumer-cutover.md)).

## The building blocks that already exist (per the SSOT pages)

- **The recall-eval gold harness** —
  [`recall_eval`](../design-notes/recall-pipeline.md#the-recall-eval-harness--recall_eval)
  replays a YAML gold set through both recall paths and scores
  coverage / deviating / combined / missing, using `wiki_search_unrecorded` (no
  counter bump, read-only, safe next to a live `serve`). **This is the gate.** Today
  the gold set is filled by hand from the dogfood; this group is its first automated
  *producer* (15f).
- **Per-fact usage tracking** — `fact_index.last_recall_at` + `recall_count_30d`,
  bumped on every returned hit ([recall-pipeline.md](../design-notes/recall-pipeline.md)).
- **Ingest-time dedup / supersede** — the capture path already recognises when a new
  message restates or contradicts an existing fact
  ([capture-and-dedup.md](../design-notes/capture-and-dedup.md),
  [ingest-pipeline.md](../design-notes/ingest-pipeline.md)). This is the **judge-free
  miss signal** below.
- **Act-first structural movers** — auto-promote (paragraph→page, page→sub-wiki) and
  page-merge already move facts between pages with born-applied receipts, a 7-day
  revert window, a `structure_applied` notice, and the dashboard as the undo surface
  ([rem-cycle.md](../design-notes/rem-cycle.md),
  [proposal-apply-engine.md](../design-notes/proposal-apply-engine.md)). A re-file
  repair reuses this machinery wholesale.
- **The emitter → approval → reaper precedent** — the archive flow
  ([11c](11_forgetting.md)) is the template for "propose a change, gate it, apply it"
  when the apply is *not* act-first.

## The model — built as recommended (detect → repair under the gate)

A three-stage REM pass — **detect → classify → repair under the gate**. Like every
other sub-job: *semantics in the prompt, resources in the knobs*; no hardcoded
per-case fixes (the maintainer's standing "no HOTFIX blocks" rule).

### 1. Detect — where did recall fail?

Two signals, cheapest first:

- **Judge-free (recommended primary): the restated-known-fact miss.** When ingest
  dedups/supersedes a new capture against an existing fact X *and* X was absent from
  that same turn's recall block, recall demonstrably failed to surface a fact it
  held. No LLM verdict — the ground truth is the user's own behaviour (they re-said
  something already stored). Catches **misses** (false negatives), the failure that
  hurts most. Needs the turn's recall output logged alongside the capture (15a).
- **Optional (LLM hindsight pass): wasted-slot + wrong-fact detection.** A strong-slot
  pass over the day's conversations asks, with the full transcript visible, which
  injected facts went unused and which needed facts were missing. Richer (catches
  false positives too) but noisy and costed — strictly secondary, and its output is
  only ever a *proposal* the gate still has to clear.

### 2. Classify — what kind of failure?

Each detected failure is routed by **root cause**, because the cause dictates the
only correct repair layer:

| Class | Meaning | Repair layer |
|---|---|---|
| **Content gap** | the fact wasn't there, or was stale/contradicted | ingest / close — already covered by capture + the completion/contradiction sweeps |
| **Mis-filed / mis-aliased** | the fact existed but on a page the entry-point fan never reached, or under the wrong alias | **move / re-file / add alias** |
| **Consumption / format** | the fact *was* in the block but the consumer ignored or misread it | **navigator / ingest prompt or recall-settings** |

### 3. Repair — lowest blast radius first

Ordered by blast radius — the cheaper, more local, more reversible repair is always
preferred; a global lever is the last resort:

1. **Re-file / move / alias a fact (default).** Data, not logic — local to one fact,
   reversible (versioned), fixes the root cause for every future query. **Act-first**,
   reusing the promote/merge receipt + revert + dashboard-undo machinery. Most repairs
   land here.
2. **Propose a rule / policy or prompt change (rare).** A generalisation is global and
   accretes into an unmaintainable special-case pile — justified only when the **same
   class** of miss recurs (≥ N times) and the rule names no specific fact. These are
   **never autonomous**: they land in an operator review queue (the archive-reaper
   pattern), because a prompt is the highest-blast-radius lever in the system — one
   edit touches every recall for every user.

**The gate (the whole point):** every proposed repair — act-first or queued — is first
replayed against the
[recall-eval gold set](../design-notes/recall-pipeline.md#the-recall-eval-harness--recall_eval)
on a scratch copy. It commits only if coverage / deviating improve and nothing
regresses; otherwise it is discarded (or, for queued items, shown with its score
delta). The big model may propose a bad fix — the **objective gold-set regression**
stops it before commit, exactly as the act-first movers lean on revertibility.

**Producer for the harness.** A confirmed miss is also a candidate **new gold-set
case** (the restated fact = the expectation, the turn = the query) — so the same loop
that repairs recall also *grows the gold set the dogfood fills by hand today*.

## Steps

- [x] 15a — **Landed 2026-07-05.** The hindsight log (`recall_log`, migration `0058` +
  `mwe_core::recall_log`): one lean row per LLM-routed ingest turn — surfaced fact ids
  (flat + fresh + due-soon) and the navigated pages' source paths — written best-effort
  at the end of the turn, age-pruned (30 days). Freshly buffered captures carry the
  row's id (`capture_buffer.recall_log_id`, DB-only — never in the journal codec).
  Decisions pinned: dedicated bounded side table (not the WAL/event stream), retention
  30/90 days. Current state:
  [recall-pipeline.md §hindsight log](../design-notes/recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal).
- [x] 15b — **Landed 2026-07-05.** The deterministic detector on both dedup surfaces:
  the direct path judges its write-time dedup skips in-memory at the end of the same
  turn (after navigation, so a fact surfaced as navigated prose is not a false miss);
  the light dream's promotion fold looks back through the buffered row's turn linkage.
  One `recall_misses` row per hit (fact, home, score, restatement, turn ref; 90-day
  prune). Scope notes: supersede hits are **not** misses by construction (the
  classifier's supersede target must come from the recall window), and rules-page facts
  are out of scope (channel-delivered). The signal is deliberately conservative — it
  under-detects (a restatement filed to a different wiki never meets the same-owner,
  same-wiki dedup scan), which is the right bias for a repair queue.
- [x] 15c — **Landed 2026-07-05.** `mwe_core::recall_gate`: the reusable
  apply-on-scratch → replay → keep-if-no-regression harness. Scratch = `VACUUM INTO`
  + `wikis/` copy; the **target check** is fact-id-based (flat top-K, or the fact's
  home page among the navigated fragments — robust to prose restyling), replayed with
  the miss's own classifier topic seeds; the **regression check** replays
  `<workdir>/recall-gold.yaml` through `recall_eval` before/after (per-query coverage
  must not drop). Empty gold set → target-only gate; malformed → the sub-job skips
  loudly. Current state:
  [rem-cycle.md §recall-repair](../design-notes/rem-cycle.md#recall-repair-sub-job--self-correcting-rems-repair-stage).
- [x] 15d — **Landed 2026-07-05.** The `run_recall_repair` full-cycle sub-job:
  proposal by the revisor-slot LLM (`rem-recall-repair` prompt, closed
  move/stay vocabulary, roster-vetted destination), repair = the **re-file** rung
  reusing `promote::apply_fact_refile_direct` wholesale (born-applied receipt, 7-day
  revert, `structure_applied` notice, land-on-`index.md`), committed only through the
  15c gate; migration `0059` adds the miss lifecycle
  (`new → repaired | queued | discarded | stale`). `RemLlms` grows the `navigator`
  slot for the gate replay. *Residue: the **alias/topic** repair rung (a receipted
  topics edit does not exist yet as a primitive) and same-wiki page moves — both fold
  in when their movers exist; the miss classes they serve currently discard or queue.*
- [x] 15e — **Landed 2026-07-05** as the notice-queue shape: a fact missing
  `recall_tuning_recurrence` (default 3) times with no provable local repair emits ONE
  `recall_tuning_proposed` event per cycle with the evidence (miss count, sample
  query, gate outcome) and the miss resolves `queued`. Never auto-applied — the
  operator holds every rule/prompt/knob lever. *A dedicated review view (the
  archive-reaper pattern) can join alongside [11c](11_forgetting.md); today the
  notice rides `wiki_events` / `events_poll` like its siblings.*
- [x] 15f — **Landed 2026-07-05.** Every real miss appends a candidate gold case to
  `<workdir>/recall-gold-candidates.yaml` (same YAML shape as the gold file, extra
  `fact_id` provenance key tolerated by the parser; deduped per fact). The operator
  reviews, distils the expectation snippets, and merges into `recall-gold.yaml` —
  candidates are **never** auto-promoted into the judge.
- [ ] 15g — Optional LLM hindsight pass (wasted-slot / wrong-fact), strong slot,
  proposal-only, behind a resource cap — added only if the judge-free signal proves
  insufficient in practice (measure first: the miss table + the repair report are the
  instrument).
- [ ] 15h — The **alias/topic repair rung**: a receipted topics-edit primitive (so an
  added recall alias is act-first + revertable like every mover) and same-wiki page
  moves, both folding into the 15c gate exactly like the re-file rung.

## A live case waiting for this group

An unreconciled cross-wiki contradiction no current sweep can see (the family-scope
consolidation pairs only within its scope; arbitrary cross-wiki pairs stay outside):
`morgana/index.md` says the show was Baricco's *Novecento*, `hermes1/esperienze_agente.md`
says *Oceano Mare*. A recall-failure detector with hindsight over real traffic is the natural
owner of this class. (Inherited from the subject-locality watch, 2026-07-05.)

## Open decisions

- **Is the judge-free miss signal enough on its own?** Shipped on the deterministic
  signal alone (15b–15f); 15g joins only if the measured misses prove it insufficient.
- ~~Recurrence threshold N~~ — pinned: `recall_tuning_recurrence` policy knob
  (default 3), and rule/prompt proposals **never** auto-apply (the notice queue is
  the ceiling).
- ~~Cadence~~ — pinned: detection rides the turn and the promotion fold (no cycle
  needed); the repair sub-job runs in the **full** cycle, capped by
  `recall_repair_cap` (the gate replay is the cost, so the cap is small and the gold
  set stays the scaling dial).
