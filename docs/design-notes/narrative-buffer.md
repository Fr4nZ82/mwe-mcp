---
title: Narrative captures buffer — the pre-compilation staging area
area: design-notes
status: partial
last_review: "2026-08-06"
---

# Narrative captures buffer

[`mwe-core::capture_buffer`](../../crates/mwe-core/src/capture_buffer.rs)
is the **write side** of the narrative compiler. For a
**narrative** wiki, [`wiki_ingest_message`](ingest-pipeline.md) does not
write the classified claim into the published `.md` page; it stages the
claim in the captures buffer and returns. The buffer is the queue of claims
waiting to be **sorted and then written as prose**, and it holds **no
destination**: which wiki and which page a claim belongs on is decided when
the light dream reads the queue, against the memory as it stands at that
moment (founder, 2026-08-18; migration `0071`). This page documents the buffer
write path and the
[light-dream drain](#promotion--the-light-dream) that promotes
buffered captures into recallable facts; the prose compiler itself
is still pending (see [Not yet](#not-yet)).

The split that decides whether a capture goes to the buffer is a single
per-wiki bit — the smart flag in `_meta.md` (see
[smart-wikis.md](smart-wikis.md)); the conceptual rationale
for the buffer→compiler model is the narrative-compiler milestone in the
roadmap. This note is the
runtime SSOT for the buffer itself.

## Why the buffer exists

If the `Capture` arm of `wiki_ingest_message` appended each classified
claim straight into the published page through
[`wiki_capture`](capture-and-dedup.md), the published page would become a
**raw marker log** — a flat stack of `{{subject=…}}…{{/}}` regions, one
per claim, grouped by subject, with no synthesis, no narrative dedup, no
topic organisation. The page would accrete; it would never get *written*.

Authoring is **mwe-mcp's** job, not the consumer's. mwe-mcp is
**agent-agnostic**: a workhorse classifier (or a voice consumer, or a
dumb client) cannot be relied on to author publishable prose, and the
memory must read well regardless of which agent fed it. So the consumer
contributes classified *claims*; the engine compiles them into prose on
its own cadence:

```text
message → archive → classifier → BUFFER(captures) → facts → wiki(.md compiled)
```

The captures buffer is the `BUFFER` stage. The published `.md` is the
compiler's **output**, not the ingest path's. Prose is not cosmetic
here — it is the mechanism by which recall stays accurate, so the
compile step is load-bearing, and it needs a durable place to stage
claims between turns. That place is this buffer.

## Two wiki families: standard vs smart

The routing decision is a single per-wiki bit — the **smart flag**
in the target wiki's `_meta.md`, read directly (no registry). **"narrative"
= "not smart"**:

| Class | Predicate | Captures go to | Compiled by the Cronista? |
|---|---|---|---|
| **smart** | smart flag `true` | unchanged (smart path) | no — excluded from ingest/compiler entirely |
| **standard** | smart flag `false` | **this buffer** | yes |

- **smart** wikis are smart-consumer-owned and authoritatively
  managed through the family-H `wiki_admin_*` tools
  ([smart-wikis.md](smart-wikis.md)). They are filtered out of the wikis a
  capture may be filed into, upstream of the router, so every arm of the
  destination derivation inherits the exclusion.
  **The smart / companion perimeter is untouched.**
- **narrative** wikis are everything else — `wiki-root`, user, group, and
  every emerged sub-wiki. Their pages are written by the compiler; their
  captures land in this buffer.

## The ingest routing change

The `Capture` arm of
[`ingest::wiki_ingest_message`](../../crates/mwe-core/src/ingest.rs) now
branches on the target wiki's class:

1. Derive the target wiki
   ([`ingest::derive_target_wiki`](../../crates/mwe-core/src/ingest.rs) —
   the classifier is no longer shown one; see
   [ingest-pipeline.md](ingest-pipeline.md)). Smart wikis are already
   filtered out of the candidate set by their `_meta.md` flag.
2. If the target is narrative (`companion == false`) **and** the
   classifier did not flag a live `requested_container` →
   [`capture_buffer::buffer_capture_staged`](../../crates/mwe-core/src/capture_buffer.rs)
   with the classifier's `supersede_target` carried through as
   `supersede_hint`. **No** `.md` write, **no** `fact_index` row.
3. Otherwise (a `requested_container` the user asked to keep live) → the
   direct-write path: `capture::wiki_supersede` when the classifier
   proposed a supersede target, else `capture::wiki_capture`. Buffering
   these as well was proposed on 2026-08-05 and rejected — this slot's
   ranked top-K cannot serve a whole list; see
   [narrative-vs-direct](ingest-pipeline.md#narrative-vs-direct-split).

**What rides with the claim.** `buffer_capture_staged` stages two things
beside the fact's own fields, both optional and both absent-tolerant:

- the **embedding**, computed once here over the marker-stripped body. It is
  read by the fresh recall slot and by promotion, which previously computed
  the same vector over the same text independently — the read side once per
  turn per pending capture. `NULL` is a first-class state: a transient
  embedder fault must not cost the capture, and both readers fall back to
  computing it.
- the **origin fingerprint** — a hash of the conversational turn the claim
  was extracted from, so the fresh slot can avoid restating, as a fact,
  something the agent is already reading in the message that produced it (see
  [recall-pipeline.md](recall-pipeline.md#the-mid-range-bridge--the-fresh-slot)).

The crucial asymmetry is the supersede. On the direct-write path a
supersede happens *now* (it rewrites the page and chains the `fact_index`
rows). On the standard-wiki path the supersede target is only **recorded as a
hint** on the waiting claim; the actual supersede is deferred to the
[drain](#promotion--the-light-dream), because there is no `fact_index` row to
chain against until the claim is written. Dedup is the same scan on both paths
— one function, `capture::best_dedup_candidate`, called at write time on one
and at screening time on the other.

Either way the ingest call returns a `capture_id` that anchors the
consumer's audit row — for standard wikis that id is the buffered
capture's id (which, by the [id-stability](#id-stability) invariant, is
also the future fact id).

## The retired `_captures.md` journal

Until 2026-08-18 every capture was **also** appended to a per-wiki on-disk
journal, `<wiki_dir>/_captures.md`, declared the durable source of truth with
the `capture_buffer` table as its rebuildable cache. It is gone: nothing writes
one, nothing reads one, and `capture_buffer` is the source of truth.

**Why it went** (founder, 2026-08-18). The rule it embodied belongs to the
file-authoritative era. The product's principle has since been the opposite —
*authority follows the author*: engine-curated memory is DB-authoritative and
the pages are its render, only consumer-authored documentation is
file-authoritative ([memory-model.md](../concepts/memory-model.md)). A capture
is engine-curated: the classifier extracted it. And the journal had stopped
delivering what it claimed:

- **Every capture rewrote the whole file.** `append_entry` read the journal,
  appended one entry and rewrote all of it atomically; the thousandth capture
  rewrote a thousand entries.
- **Nothing ever pruned it.** No rotation, no removal on promotion — entries
  from months back, whose facts had long been compiled onto pages carrying the
  same data inline, stayed for ever.
- **It was written once and never updated.** Each entry carried a `status=`
  attribute frozen at `buffered`: the light dream stamped promotion on the
  table, never on the file. The declared source of truth was therefore stale
  for every capture it had ever processed, while the real state lived in what
  the doc called the cache.
- **The five-minute safety-net reindex re-parsed all of it**, per wiki, to
  re-insert rows that already existed.

What it did buy, honestly: between the capture and the compile, a claim's
structured fields (subject, audience, sender, validity, type) existed nowhere
on disk but the journal, since the page did not exist yet. That window is now
covered the same way as everything else the DB holds — the tombstones of
forgotten facts, the recall traces, the vectors, the buffer's own status — by
the [workdir snapshot](../../crates/mwe-core/src/backup.rs), which takes the
DB image and the file tree together precisely because neither reconstructs the
other.

A leftover `_captures.md` in an old workdir is inert: still excluded from
`WikiHandle::list_pages`, from the reindex marker sweep and from the export,
so it is never published, indexed or shipped. Deleting it by hand is safe.

## The `capture_buffer` table — where a pending capture lives

Migration 0031 adds the `capture_buffer` table; 0034 adds the `valid_from` /
`valid_to` validity columns, 0035 the `style` column,
0038 the `decay_reason` closure column, 0068 the `embedding` /
`embedding_dim` / `origin_message_hash` staging columns, and **0071 takes the
destination away** — `wiki_id` and `target_page` are gone, and with them the
`idx_capture_buffer_wiki` index.

**The table is the source of truth for a pending capture**; there is no second
copy of it anywhere. Its columns mirror the `fact_index` classifier/ACL columns
so promotion can be a straight copy: `capture_id` (primary key), `body`,
`subject_id`, `allow_ids` (JSON), `sender_id`, `fact_type`, `topics` (JSON),
`supersede_hint`, `status`, `captured_at`, `processed_at`, `resolved_fact_id`,
`source_kind`, `source_ref`, the validity interval `valid_from` / `valid_to`,
and `style`.

Every one of those describes the **claim**. `style` is the closest thing to
placement left, and it is not placement: it says what shape the *material* has
— a list, technical prose, ordinary prose — never which page holds it. A page
takes its style from the majority of the facts on it, so the arrow points the
other way.

The page's **card** — the one-line `description:` saying what belongs on it —
went with the destination (migration `0072`): it is a property of the page, it
lives on the page's testata and in `page_card`, and a claim with no page has
none to describe.

`decay_reason` is the one **post-capture mutation**: it stays `NULL` at buffer
time (a fresh capture is alive) and is stamped — together with the closing
`valid_to` — only when a **closure gesture lands while the target is still
buffered** (the same-day flow: the item is bought before the light dream
promotes it; `capture_buffer::close_validity`).
[`promote_one`](../../crates/mwe-core/src/dream_light.rs) stamps the staged
reason onto the freshly promoted fact right after the insert (the insert itself
keeps its fresh-fact invariant).

`embedding` / `embedding_dim` stage the vector computed once at buffer time,
over the marker-stripped body; both readers — the fresh recall slot and
`promote_one` — recompute when it is `NULL`, so the column is an optimisation
and never part of what makes a capture valid. `origin_message_hash` fingerprints
the conversational turn the claim came from, so a capture already quoted in the
current context is not offered back to it.

One index serves the drain: the partial `idx_capture_buffer_pending` over
`status` filtered `WHERE status = 'buffered'`. The `status` column is one of
`buffered` / `promoted` / `skipped_dup`, decoded through `CaptureStatus`.

The read side exposes `find_all_buffered` (the light-dream drain query, oldest
first, capped at the cycle limit), `find_recent_buffered` (newest first — every
reader that asks *what was just said and is not on a page yet*: the recall
fresh slot, the dashboard's consolidating list), and `count_buffered` (the
global pending backlog — the threshold signal for the light dream). There is no
per-wiki query, and there cannot be one: a buffered capture is in no wiki. The
write side adds `mark_promoted` and `mark_skipped_dup`, the two terminal status
transitions the light dream stamps.

## Id stability

The `capture_id` is a `UUIDv7` minted at buffer time. When the light
dream promotes the capture, that same id is **reused verbatim as the
`fact_id`** — a claim keeps one stable id across
buffer → fact → compiled-page. This is the correctness hinge for
incremental compilation (planned): the compiler's per-page
fingerprints key on `fact_id`s, so an id that survives the whole
pipeline lets the compiler tell "this page is unchanged" from "a new
claim landed" without re-reading prose.

## Promotion — the light dream

The buffer's read side is drained by the **light dream**
([`mwe-core::dream_light`](../../crates/mwe-core/src/dream_light.rs)) — the
frequent, cheap half of the "two dream" cadence (the nightly REM full reorg,
[`rem::run_cycle`](rem-cycle.md), is the other). It runs in **two halves with
the compilation plan between them**, because a claim becomes a fact only once
somebody has decided which page it goes on:

```text
screen_queue → build_wiki_plan → materialise → compile_dirty_pages
 (dedup)        (which page?)     (the rows)      (the prose)
```

Both halves are fully deterministic — they embed, copy, and apply the
classifier's recorded decision, and never call an LLM. Semantic judgement is
left to the placement stage and to the REM night.

Per waiting claim, in order — steps 1 in `screen_queue`, steps 2-4 in
`materialise`:

1. **Dedup skip — the direct path's own scan, deferred.** The same
   jaccard 6-gram scan a live
   [`capture::wiki_capture`](capture-and-dedup.md) runs
   ([`capture::best_dedup_candidate`](../../crates/mwe-core/src/capture.rs):
   same-subject scope, rules-page boundary, embed-set guard, the same
   `dedup_threshold` default) is re-run here against the wiki's active
   facts. At or above the threshold the capture resolves to the
   survivor — its row is stamped `skipped_dup` with `resolved_fact_id`
   = the survivor — and **no new fact is created**. Parity is the
   point: a buffered capture gets exactly the dedup it would have
   gotten written live; without it the buffered path had *no*
   similarity dedup anywhere in its lifecycle (promotion collapsed
   exact strings only, and the Revisor deliberately skips pairs above
   the threshold as write-time territory). Sub-threshold paraphrases
   stay the REM night's job (the Revisor sub-job). Same-subject scoping
   also means the same text about two subjects promotes as two facts —
   one fragment's subject is never folded into another principal's. The
   capture is excluded from its own comparison, so a retry after a
   partial promotion does not skip a capture against the fact it itself
   minted.

   **And against the other claims in the same queue.** None of them is a
   `fact_index` row yet — they become rows only after the plan — so the DB scan
   cannot see them, and two identical claims arriving in one interval would
   both be written. The intra-queue comparison uses the same jaccard 6-gram
   over the marker-stripped body, the same audience test (`same_audience`), and
   the same embed-set guard.

   Still no LLM here — the scan is pure CPU. A fold is also the
   **offline half of the restated-known-fact miss signal**: when the
   buffered row carries its turn's `recall_log_id` linkage and that turn
   never surfaced the survivor, one `recall_misses` row lands
   (best-effort telemetry — see
   [recall-pipeline.md](recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal)).
2. **Embed + insert, on the page the plan chose.** Otherwise the body is
   embedded (bge-m3 — normally already staged at buffer time) and inserted
   through [`fact_index::insert_if_absent`](capture-and-dedup.md) as a fact
   whose `fact_id` **is** the `capture_id` — the
   [id-stability](#id-stability) invariant made concrete — addressed to
   `wikis/<wiki>/<page>`, the page the plan just gave it. A claim the plan
   could not place is **not** inserted: it keeps waiting.
3. **Supersede hint.** If the buffered capture carried a `supersede_hint`
   (the classifier's `supersede_target`, recorded at ingest) and that
   target fact is **still active**, it is marked superseded by the new
   fact. This applies the classifier's decision deterministically — the
   deferred half of the [ingest asymmetry](#the-ingest-routing-change):
   on the standard-wiki path the supersede was only *recorded* at ingest
   because there was no `fact_index` row to chain against until now.
   An applied hint also performs the retirement **disk half**: the
   superseded fact's rendered region (if any) is excised from its page
   via `reindex::strip_fact_region`, best-effort
   ([redaction-policy](redaction-policy.md)).
4. **Status stamp.** The `capture_buffer` row is flipped to `promoted`
   (`resolved_fact_id` = the new `capture_id`) or `skipped_dup`.

`LightPolicy` caps one cycle at `max_promotions_per_cycle` captures (a
cost guard on the embedder) and carries the
`dedup_threshold` (default `recall::DEFAULT_DEDUP_THRESHOLD`, the same
knob as the direct path's `CaptureRequest::dedup_threshold`); the
overflow stays `buffered` for the next cycle. The cycle returns a `LightCycleReport`
(`scanned` / `promoted` / `skipped_dup` / `superseded` / `errors`).
**Per-capture failures are soft** — a transient embed error, or a wiki
that vanished between buffering and promotion, leaves that capture
`buffered` and is collected into `errors`; only infrastructure failures
(DB, tree walk) bubble and abort the cycle.

### A fact is born knowing its page

There is no *promoted-but-unplaced* state any more, and there is no phantom
file to name it with (migration `0072`, founder 2026-08-18: *«è illogico
scriverci `_pending.md`, un file che non esiste; io credo sia giusto scrivere
l'entry nel db dei fatti quando si è già deciso dove mettere il fatto in
attesa, parallelamente alla scrittura sulla prosa»*).

The three steps run in this order, with the compilation plan in the middle:

1. **`screen_queue`** — the claims minus the duplicates, projected for the plan
   with no page.
2. **`build_wiki_plan`** — the placement stage judges them beside the facts
   already on pages, and gives each one a page.
3. **`materialise`** — each placed claim becomes a `fact_index` row addressed
   to `wikis/<wiki>/<page>`, moments before the compile writes that page.

So a row's `source_path` is always a real page's, from birth. What it does *not*
have yet is `region_start` / `region_end`: those stay `NULL` until the compile
writes the page and `repoint_facts` stamps them — the same *pending render*
state a live capture passes through between its insert and its page write.
Recall serves such a fact straight from `fact_index.text`, and the reindex
existence sweep exempts offset-less rows.

A claim the plan could **not** place (no home page at all) keeps waiting in the
buffer. It is not made into a fact nobody renders, and the recall fresh slot
keeps offering it meanwhile.

### Idempotency & crash-safety

The light dream only ever advances `buffered` rows; the insert is
`insert_if_absent`, `mark_superseded` no-ops on an already-superseded
row, and the status updates are guarded on `status = 'buffered'`. So a
crash mid-cycle simply re-promotes idempotently. The stable `capture_id == fact_id` is what
makes that safe: a second promotion of the same capture finds the fact
already present and the row already `promoted`, and does nothing.

### Cadence and the CLI

In the long-lived HTTP server,
[`rem_scheduler::spawn_light`](rem-cycle.md) drives the light dream on a
**timer + threshold** cadence: a poll loop runs a cycle when either
`light_interval_secs` has elapsed since the last run (default 1h) *or*
the buffered backlog has reached `light_backlog_threshold` (the early
trigger; `0` disables it). It is wired in `cmd_serve_http` alongside the
REM full-cycle scheduler and **shares `rem.schedule.mode`** — `disabled`
turns both off.

**The models are not optional** — see
[admin-llm-config.md](admin-llm-config.md#the-models-are-mandatory): a
deployment without them is a half-installed product, and the `ingest` role is
enforced at onboarding. What the light dream *does* survive is a missing prose
writer: with no `cronista` slot it drains the queue deterministically
(`dream_light::drain_deterministically` — every page the user's own turn named,
the parking page for the rest, renders pending), because the alternative is a
queue that grows for ever while the recall fresh slot, a ranked top-K, quietly
stops offering the older half of it. That is damage control, not a supported
configuration.
The full cycle's `interval_secs` (default 24h) is unchanged; the light
dream is the far more frequent of the two. Operators driving REM
externally run one cycle synchronously with `mwe-mcp rem run-light`
(lockfile-guarded, embedder only) — the deterministic sibling of
`mwe-mcp rem run-cycle`. See [rem-cycle.md](rem-cycle.md) for the
scheduler.

## Not yet

With the light dream landed, a narrative capture is **recallable once it
promotes it** — within `light_interval_secs`, or sooner if the
buffered backlog crosses `light_backlog_threshold`. The "durably
buffered but not yet recallable" gap is **closed**: the window now is
just the time between buffering and the next light cycle. The prose
compilation has also landed — the Cronista compiles each promoted fact
into a published standard page and repoints `fact_index` onto it (see
[`narrative-compiler.md`](narrative-compiler.md)). What remains for the
full narrative experience is the **recall side**: recall still returns
the promoted **fact body**, not yet the compiled standard page. That
remaining stage is tracked in the roadmap:

| Stage | What it adds | Status |
|---|---|---|
| **light dream (drain)** | Screens `buffered` claims (capture-parity dedup, jaccard ≥ threshold, same-subject, plus intra-queue), lets the plan place the survivors, then writes each as a `fact_index` row (`fact_id == capture_id`) addressed to its page and applies the `supersede_hint`; flips `status` to `promoted` / `skipped_dup`. | **landed** ([above](#promotion--the-light-dream)) |
| **Cronista (compilation)** | Compiles the promoted facts into the published prose `.md` pages on the nightly cadence. The `.md` becomes the compiler's output, and `source_path` + offsets are repointed off the *no page yet* address onto it. | **landed** ([`narrative-compiler.md`](narrative-compiler.md)) |
| **recall over compiled prose** | Recall navigates and serves the compiled standard pages rather than the raw promoted fact body. | planned |

The `source_kind` values beyond `ingest` (e.g. `shadow_diff` for the
shadow-diff stage) exist in the schema today so the later stages slot in
without a migration, but stay inert until those stages write them.
