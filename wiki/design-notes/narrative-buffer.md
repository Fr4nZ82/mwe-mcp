---
title: Narrative captures buffer — the pre-compilation staging area
area: design-notes
status: partial
last_review: "2026-07-02"
---

# Narrative captures buffer

[`mwe-core::capture_buffer`](../../crates/mwe-core/src/capture_buffer.rs)
is the **write side** of the narrative compiler. For a
**narrative** wiki, [`wiki_ingest_message`](ingest-pipeline.md) does not
write the classified claim into the published `.md` page; it stages the
claim in a per-wiki captures buffer and returns. The nightly compiler
turns the buffer into prose later. This page documents the buffer write
path and its filesystem-SSOT invariant, and the
[light-dream drain](#promotion--the-light-dream) that promotes
buffered captures into recallable facts; the prose compiler itself
is still pending (see [Not yet](#not-yet)).

The split that decides whether a capture goes to the buffer is a single
per-wiki bit — the smart flag in `_meta.md` (see
[smart-wikis.md](smart-wikis.md)); the conceptual rationale
for the buffer→compiler model is the narrative-compiler milestone in the
[roadmap](../roadmap.md). This note is the
runtime SSOT for the buffer itself.

## Why the buffer exists

If the `Capture` arm of `wiki_ingest_message` appended each classified
claim straight into the published page through
[`wiki_capture`](capture-and-dedup.md), the published page would become a
**raw marker log** — a flat stack of `{{owner=…}}…{{/}}` regions, one per
claim, grouped by owner, with no synthesis, no narrative dedup, no topic
organisation. The page would accrete; it would never get *written*.

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
  ([smart-wikis.md](smart-wikis.md)). They are filtered out of
  `available_wikis` upstream of the ingest router, so the routing decision
  never even sees them. **The smart / companion perimeter is untouched.**
- **narrative** wikis are everything else — `wiki-root`, user, group, and
  every emerged sub-wiki. Their pages are written by the compiler; their
  captures land in this buffer.

## The ingest routing change

The `Capture` arm of
[`ingest::wiki_ingest_message`](../../crates/mwe-core/src/ingest.rs) now
branches on the target wiki's class:

1. Resolve the target wiki from `available_wikis` (smart wikis are
   already filtered out of this set by their `_meta.md` `companion`
   flag).
2. If the target is narrative (`companion == false`) **and** the
   classifier did not flag a live `requested_container` →
   [`capture_buffer::buffer_capture`](../../crates/mwe-core/src/capture_buffer.rs)
   with the classifier's `supersede_target` carried through as
   `supersede_hint`. **No** `.md` write, **no** `fact_index` row.
3. Otherwise (a `requested_container` the user asked to keep live) → the
   direct-write path: `capture::wiki_supersede` when the classifier
   proposed a supersede target, else `capture::wiki_capture`.

The crucial asymmetry is the supersede. On the direct-write path a
supersede happens *now* (it rewrites the page and chains the
`fact_index` rows). On the standard-wiki path the supersede target is only
**recorded as a hint** on the buffered capture; the actual supersede is
deferred to
[promotion time](#promotion--the-light-dream), because there is no
`fact_index` row to chain against until the claim is promoted.

Either way the ingest call returns a `capture_id` that anchors the
consumer's audit row — for standard wikis that id is the buffered
capture's id (which, by the [id-stability](#id-stability) invariant, is
also the future fact id).

## The `_captures.md` journal — durable SSOT

The durable source of truth for a buffered capture is a per-wiki on-disk
journal, `<wiki_dir>/_captures.md`
([`crate::wiki::CAPTURES_FILENAME`](../../crates/mwe-core/src/wiki.rs)).
This keeps the buffer faithful to the project-wide invariant that
[the filesystem is the SSOT](wiki-filesystem-ssot.md) and the DB is a
rebuildable cache. The journal is written through the same
[`atomic_write`](wiki-filesystem-ssot.md) protocol as every other page
(tempfile + persist + parent-dir fsync + `WriteMarker` guard).

The file is YAML frontmatter followed by one entry per capture; each
entry is delimited by HTML comments, with the body held verbatim
(possibly multi-line) between an open and a close comment:

```markdown
---
kind: capture_journal
wiki_id: alice
---

<!-- mwe-capture id=0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d40 ts=2026-05-31T10:00:00+00:00 page=index.md type=preference status=buffered owner=user:alice allow= sender= sup= topics=food vf=2026-05-31T10:00:00+00:00 vt= style=prosa desc=Cosa%20piace%20ad%20Alice -->
Alice loves pasta.
<!-- /mwe-capture -->

<!-- mwe-capture id=0190f3c2-9b71-7d88-a4e0-7c2b9f0a1e22 ts=2026-05-31T10:01:12+00:00 page=recipes/dinner.md type=plan status=buffered owner=group:famiglia allow=user:bob sender=user:alice sup= topics=dinner vf=2026-05-31T10:01:12+00:00 vt=2026-06-05T19:00:00+00:00 style=prosa-tecnica desc=Cene%20coi%20Brandibuck -->
Cena con i Brandibuck venerdì sera.
<!-- /mwe-capture -->
```

The open-comment attributes carry the classifier's full output: the
capture `id`, the timestamp `ts`, the proposed `page`, the fact `type`,
the lifecycle `status`, the `owner` / `allow` / `sender` ACL triple, the
`sup` supersede hint, the `topics` CSV, and the per-fact validity
interval `vf` / `vt` (`valid_from` / `valid_to`,
ISO-8601, whitespace-free; empty `vt` = an OPEN horizon), and the ingest
placement style axis `style` / `desc` (the proposed
page `style` rides as a bare enum token; the free-text `page_description`
is percent-escaped into `desc` so it stays one token in this
whitespace-delimited list). Empty optional
fields are written as bare `key=` (see `allow=`, `sender=`, `sup=`,
`vt=` above). Because
the comment grammar is structural, a body may not contain `{{`, `}}`, or
`<!--`; `buffer_capture` rejects such a body with `BodyContainsReserved`
(and an empty body with `EmptyBody`), mirroring the capture path's
validation. As in `wiki_capture`, a `sender` equal to the `owner` is
dropped to `None`.

## The `capture_buffer` table — a rebuildable index

Migration 0031 adds the `capture_buffer` table; migration 0034 adds the
`valid_from` / `valid_to` validity columns, 0035
the `style` / `page_description` placement columns, and 0038 the
`decay_reason` closure column.
**It is a cache/index over the journal, not the SSOT.** Its columns mirror
the `fact_index` classifier/ACL columns so promotion can be a straight copy:
`capture_id` (primary key), `wiki_id`, `target_page`, `body`,
`owner_id`, `allow_ids` (JSON), `sender_id`, `fact_type`, `topics`
(JSON), `supersede_hint`, `status`, `captured_at`, `processed_at`,
`resolved_fact_id`, `source_kind`, `source_ref`, the validity
interval `valid_from` / `valid_to` (mirrored in the journal as `vf` /
`vt`), and the placement style axis `style` / `page_description` (mirrored
as `style` / `desc`) — all so `rm engine.db` + reindex regenerates them.

`decay_reason` is the one **post-capture mutation** among them: it stays
`NULL` at buffer time (a fresh capture is alive) and is stamped — together
with the closing `valid_to` — only when a **closure gesture lands while
the target is still buffered** (the same-day flow: the item is bought
before the light dream promotes it; `capture_buffer::close_validity`).
Like `status` / `processed_at` it is DB-only, never written back to the
journal: a full `rm engine.db` rebuild regenerates the row as alive, the
normal reindex (`ON CONFLICT … DO NOTHING`) keeps the closed row.
[`promote_one`](../../crates/mwe-core/src/dream_light.rs) stamps the
staged reason onto the freshly promoted fact right after the insert (the
insert itself keeps its fresh-fact invariant).
Two indexes serve
the drain: `idx_capture_buffer_wiki` (per-wiki lookup) and a partial
`idx_capture_buffer_pending` over `status` filtered
`WHERE status = 'buffered'` (the pending backlog). The `status` column is
one of `buffered` / `promoted` / `skipped_dup`, decoded through
`CaptureStatus`.

Note there is **no `journal_path` column**: a capture for wiki `W` always
lives in `W`'s `_captures.md`, so the journal location is derived from
the tree, never stored.

The read side exposes `find_all_buffered` (the light-dream drain query,
oldest first, capped at the cycle limit), `find_buffered_in_wiki`
(per-wiki lookup), and `count_buffered` (the global pending backlog — the
threshold signal for the light dream). The write side adds `mark_promoted`
and `mark_skipped_dup`, the two terminal status transitions the light
dream stamps.

### Filesystem-SSOT invariant

The table is regenerable from the journals alone. `buffer_capture`
writes the journal entry first, then upserts the table row with
`ON CONFLICT(capture_id) DO NOTHING`, so the journal is the durable
record and the row is a derived projection. On a cold start,
[`reindex::reindex_full`](reindex-pipeline.md) calls
`capture_buffer::reindex_capture_journal` per wiki, which reads
`_captures.md`, parses each entry, and re-inserts the rows (idempotently;
malformed individual entries are skipped, not fatal). The practical
consequence: **`rm engine.db` followed by `serve` regenerates every
buffered row from disk** — exactly the guarantee the rest of the system
relies on for the `fact_index`.

Conversely, the journal must never be mistaken for published content.
`_captures.md` is excluded from
[`WikiHandle::list_pages`](wiki-filesystem-ssot.md) and from the reindex
marker sweep: `reindex::is_capture_journal` guards both
`enumerate_pages` and `reindex_file`, so the journal's entries are never
indexed as facts and never surface as a page.

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

The buffer's read side is drained by the **light dream**,
[`mwe-core::dream_light::run_light_cycle`](../../crates/mwe-core/src/dream_light.rs)
— the frequent, cheap half of the "two dream" cadence (the
nightly REM full reorg, [`rem::run_cycle`](rem-cycle.md), is the other).
It promotes each `buffered` capture into a `fact_index` row, after which
the capture is **recallable**. Promotion is fully deterministic: it
embeds and copies, applies the classifier's recorded decision, and never
calls an LLM. Semantic judgement is left to the REM night.

Per buffered capture, in order:

1. **Dedup skip — the direct path's own scan, deferred.** The same
   jaccard 6-gram scan a live
   [`capture::wiki_capture`](capture-and-dedup.md) runs
   ([`capture::best_dedup_candidate`](../../crates/mwe-core/src/capture.rs):
   same-owner scope, rules-page boundary, embed-set guard, the same
   `dedup_threshold` default) is re-run here against the wiki's active
   facts. At or above the threshold the capture resolves to the
   survivor — its row is stamped `skipped_dup` with `resolved_fact_id`
   = the survivor — and **no new fact is created**. Parity is the
   point: a buffered capture gets exactly the dedup it would have
   gotten written live; without it the buffered path had *no*
   similarity dedup anywhere in its lifecycle (promotion collapsed
   exact strings only, and the Revisor deliberately skips pairs above
   the threshold as write-time territory). Sub-threshold paraphrases
   stay the REM night's job (the Revisor sub-job). Same-owner scoping
   also means the same text under two owners promotes as two facts —
   per-fragment ownership is never folded across principals. The
   capture is excluded from its own comparison, so a retry after a
   partial promotion does not skip a capture against the fact it itself
   minted. Still no LLM here — the scan is pure CPU. A fold is also the
   **offline half of the restated-known-fact miss signal**: when the
   buffered row carries its turn's `recall_log_id` linkage and that turn
   never surfaced the survivor, one `recall_misses` row lands
   (best-effort telemetry — see
   [recall-pipeline.md](recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal)).
2. **Embed + insert.** Otherwise the body is embedded (bge-m3) and
   inserted through
   [`fact_index::insert_if_absent`](capture-and-dedup.md) as a fact whose
   `fact_id` **is** the `capture_id` — the [id-stability](#id-stability)
   invariant made concrete.
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
cost guard on the embedder) and carries the promotion-time
`dedup_threshold` (default `recall::DEFAULT_DEDUP_THRESHOLD`, the same
knob as the direct path's `CaptureRequest::dedup_threshold`); the
overflow stays `buffered` for the next cycle. The cycle returns a `LightCycleReport`
(`scanned` / `promoted` / `skipped_dup` / `superseded` / `errors`).
**Per-capture failures are soft** — a transient embed error, or a wiki
that vanished between buffering and promotion, leaves that capture
`buffered` and is collected into `errors`; only infrastructure failures
(DB, tree walk) bubble and abort the cycle.

### Where the promoted-but-uncompiled fact lives

A fact promoted by the light dream has **no published page yet** — the
Cronista writes pages later. So its `source_path` points at the
wiki's `_captures.md` journal and its `region_start` / `region_end`
offsets are `NULL`. Because the journal is excluded from the reindex
marker sweep ([`reindex::is_capture_journal`](reindex-pipeline.md)),
these facts are never orphaned by a page reindex; recall serves them
straight from `fact_index.text`. When the Cronista compiles a fact into a
page it repoints `source_path` + offsets onto the published `.md`.

### Idempotency & crash-safety

The light dream only ever advances `buffered` rows; the insert is
`insert_if_absent`, `mark_superseded` no-ops on an already-superseded
row, and the status updates are guarded on `status = 'buffered'`. So a
crash mid-cycle, or a re-run after `rm engine.db` — which rebuilds the
buffer rows as `buffered` from the journal (the
[filesystem-SSOT invariant](#filesystem-ssot-invariant)) — simply
re-promotes idempotently. The stable `capture_id == fact_id` is what
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
turns both off. Because promotion is deterministic it needs no LLM bag,
so the light dream runs even when the REM LLM slots are unconfigured.
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
remaining stage is tracked in the [roadmap](../roadmap.md):

| Stage | What it adds | Status |
|---|---|---|
| **light dream (promotion)** | Drains `buffered` captures into `fact_index` (`fact_id == capture_id`), applying the `supersede_hint` and the capture-parity dedup skip (jaccard ≥ threshold, same-owner); flips `status` to `promoted` / `skipped_dup`. | **landed** ([above](#promotion--the-light-dream)) |
| **Cronista (compilation)** | Compiles the promoted facts into the published prose `.md` pages on the nightly cadence. The `.md` becomes the compiler's output, and `source_path` + offsets are repointed off `_captures.md` onto it. | **landed** ([`narrative-compiler.md`](narrative-compiler.md)) |
| **recall over compiled prose** | Recall navigates and serves the compiled standard pages rather than the raw promoted fact body. | planned |

The `source_kind` values beyond `ingest` (e.g. `shadow_diff` for the
shadow-diff stage) exist in the schema today so the later stages slot in
without a migration, but stay inert until those stages write them.
