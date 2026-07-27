---
title: Reindex pipeline — repairing the page ↔ index bookkeeping
area: design-notes
status: implemented
last_review: "2026-07-26"
---

# Reindex pipeline

`mwe-core::reindex` is the consumer side of
[`mwe-core::watcher`](../../crates/mwe-core/src/watcher.rs): when a
third-party editor (Obsidian, an operator typing in `nvim`) touches a
markdown file under `<workdir>/wikis/**`, this module re-parses it and
reconciles the index. **Which** index depends on the wiki's family —
[`fact_index`](../../crates/mwe-core/src/fact_index.rs) for standard
wikis, [`wiki_sections`](../../crates/mwe-core/src/sections.rs) for smart
ones — and so does what "reconcile" means:

- **Smart wikis** (smart-consumer project wikis, written verbatim via
  `wiki_admin_*` or a direct filesystem edit): plain markdown with **no
  per-fragment `{{f=…}}` markers**, so recall indexes the **content**.
  Each page is chunked into heading-delimited sections, embedded, and
  written to **`wiki_sections`** — its own table, not `fact_index`
  ([`mwe_core::sections`](../../crates/mwe-core/src/sections.rs)). A
  section carries **no ACL of its own**: read access belongs to the wiki
  and is held once in the `smart_wikis` registry. A removed page's
  sections are **hard-dropped** (no tombstone). This is the markerless
  half of the [per-fragment-ACL pillar](smart-wikis.md): per-fragment
  markers/ACL stay the pillar of **standard** wikis only.
- **Standard wikis** (compiler/capture output — "standard" = "not
  smart", keyed off the per-wiki smart flag in `_meta.md`):
  the **DB is the authoritative fact store** and pages are its prose
  render, so the sweep shrinks to **offset-and-existence repair**. Rows
  are never created or rewritten from disk markers; region offsets are
  repaired after hand edits; a hand-deleted marker or page still
  tombstones its *rendered* rows (the operator's forget gesture).

Reserved underscore-pages are never indexable content — both families
skip them (`is_reserved_page`): `_meta.md` (wiki config), `_captures.md`
(the buffered-capture journal), and `_briefing.md` / `_briefing.archive.md`
(the smart consumer's feedback inbox, addressed *to* the consumer, not
knowledge it authored).

## Public entry points

| Function | When | Cost |
|---|---|---|
| [`reindex_file`](../../crates/mwe-core/src/reindex.rs) | Single `WatchedChange` event — the hot path | Smart: segment the page + ≤N embed calls (unchanged sections reuse their stored vector); standard: one parse + one `find_active_by_source_path` |
| [`reindex_full`](../../crates/mwe-core/src/reindex.rs) | Safety-net 5-minute tick, also usable as a startup catch-up | Refresh the `smart_wikis` registry; walk every wiki; re-section every smart-wiki `*.md` + a deleted-page sweep |
| [`project_smart_wiki_registry`](../../crates/mwe-core/src/reindex.rs) | `serve` boot + every `reindex_full` tick | One tree walk + one upsert per smart wiki |
| [`backfill_smart_sections`](../../crates/mwe-core/src/reindex.rs) | `serve` boot (one-time migration tail, idempotent) | One pass over each smart wiki's legacy rows; embeddings copied, never recomputed |
| [`strip_fact_region`](../../crates/mwe-core/src/reindex.rs) | Act-time, from every retire path with engine context (supersede, forget, dedup merge — the roster is in [redaction-policy](redaction-policy.md)) | One row lookup + one page rewrite + a `reindex_file` re-sync; refuses active rows |
| [`strip_retired_regions_on_page`](../../crates/mwe-core/src/reindex.rs) / [`sweep_retired_regions`](../../crates/mwe-core/src/reindex.rs) | The light dream's retirement hygiene sweep over **non-plan** pages ([rem-cycle](rem-cycle.md)) | Per page: one parse + one lookup per marker + at most one rewrite; sweep bounded at `RETIRED_SWEEP_MAX_PAGES`/cycle |
| [`reconcile_wiki_ids`](../../crates/mwe-core/src/reindex.rs) | Once at `serve` boot, after the tree opens | One slim full-table scan of active rows + one targeted UPDATE per divergence |

The first two are **idempotent**: running them twice in a row over an
unchanged tree mutates zero rows the second time. For smart wikis a page
whose section texts already sit in the same positions is a
no-op; that property is what makes the race documented under
[marker filter](#marker-filter--inotify-race) acceptable, and what keeps
the push-path index queue (below) from churning on a re-push. The
retirement strips are idempotent by convergence (an excised region's row
settles its offsets to NULL, so re-runs find nothing), and the boot
reconcile is idempotent by construction (a consistent row is never
touched; each fix is guarded on the divergent value it corrects).

## Boot-time `wiki_id` reconcile

[`reconcile_wiki_ids`](../../crates/mwe-core/src/reindex.rs) is the
belt-and-braces pass `serve` runs once at startup (right after the tree
opens, before the watcher arms): for every **active** fact row it derives
wiki-of(`source_path`) against the discovered wiki set by **longest
directory prefix** — sub-wikis nest (`wikis/famiglia/bruno-battaglia/…`
belongs to `famiglia-bruno-battaglia`, not `famiglia`), which is why this
is a tree-aware Rust pass and not a SQL migration — and repoints
`wiki_id` where the two diverged (`fact_index::set_wiki_id`, guarded on
the divergent value; each fix logged at INFO plus a final count). A row
whose `source_path` falls under no discovered wiki is left untouched and
logged at WARN. The compiler prevents *new* divergences at write time;
this pass heals any row a compile would never revisit (its page never
turns dirty). Non-fatal: a reconcile failure logs and the boot proceeds.

## Smart wikis — content indexing (markerless)

[`section_index_page`](../../crates/mwe-core/src/reindex.rs) chunks a
smart page into heading-delimited sections (through the document
segmenter [`document::segment_document`](../../crates/mwe-core/src/document.rs),
the heading chain prefixing each section's text so the heading words are
searchable), embeds each section, and reconciles the page's rows in
`wiki_sections`:

| Page state vs. stored sections | Action |
|---|---|
| Same texts in the same positions | **no-op** (idempotent) |
| Anything drifts (edited/added/removed/reordered section) | [`sections::replace_page_sections`](../../crates/mwe-core/src/sections.rs) in **one transaction** — upsert by position, then drop any tail position the new content no longer reaches. An unchanged section's text reuses its stored embedding; only new/changed sections are re-embedded |
| File missing | hard-drop every section of the page (no tombstone) |

**The lexical index needs nothing from this path.** `wiki_sections_fts`
(migration `0065`, the exact-term half of section ranking) is maintained
by triggers on `wiki_sections`, so every row this reconciliation writes,
rewrites or drops updates it in the same transaction. That is why the
triggers live in the schema and not here: this sweep, the push-enqueued
index, the boot-time reconciliation and an operator's manual `UPDATE` all
get it right without knowing the index exists.

**The cut is sized for retrieval, not for extraction.** Sections use
[`DocumentPolicy::for_sections`](../../crates/mwe-core/src/document.rs) —
target `SECTION_TARGET_CHARS` (1 200), hard cap `SECTION_MAX_CHARS`
(2 000) — not the document-ingest defaults (3 000 / 4 500) they were
borrowed from until roadmap 48h. The two jobs want opposite things: an
ingest segment is read whole by an extractor, where wide context helps;
a section is ranked by **one** embedding and quoted verbatim into a
bounded recall slot, and at ingest sizes both degrade — one vector
averaged over several topics matches every query mediocrely, and one
oversized hit exhausts the slot's char budget alone (the budget admits
whole sections only, and always admits the first). Measured on the
production corpus before the change: 25 % of sections were larger than
the entire slot, the largest 6 994 characters.

**The same cut answers "how will this page retrieve?"**
[`document::page_shape`](../../crates/mwe-core/src/document.rs) walks a
page through the identical block splitter and reports what the index will
do to it: how many sections, how many of them carry the *same* heading
chain as their predecessor (packed, not opened by a heading), how many
source blocks exceed the hard cap, and what share of the page those hold.
It is a pure function of the bytes — no DB, no queue, no embedder — which
is what makes it usable at the two moments it is needed: on the
`wiki_admin_push` response (`warnings[]`, per pushed page) and on
`wiki_admin_pull` with `shape: true` (the whole wiki, no content), while
this pipeline's own work is still queued. `PageShape::needs_repair` fires
on **density, not size**: at least `DENSE_PAGE_BLOCKS` (3) over-cap
blocks, or over-cap blocks holding at least `DENSE_PAGE_SHARE` (25 %) of
the page. A long page written in ordinary paragraphs is fine and must not
be flagged, or the warning is noise and gets ignored (roadmap 51f).

Changing these numbers needs **no migration**. A section's stored text is
compared against what the current policy would produce, so the next
[safety-net sweep](#safety-net--smart-wiki-pages-only) re-cuts and
re-embeds any page whose chunking no longer matches; unchanged text
keeps its stored vector, so
only genuinely new chunks cost an embed. The heading path is prefixed
*after* the cut, so a stored section is at most `SECTION_MAX_CHARS` plus
its header line.

**Identity is positional.** A section is keyed by `(source_path,
section_ord)`, the primary key of the table. Two consequences:

- **Duplicate rows are not expressible.** The old design minted a fresh
  id per reinsert, so an interleaved drop-then-insert from two concurrent
  reindexers could leave two copies of the same block (identical text,
  identical score, different id) — visible as the same hit twice in
  `wiki_navigate`. The primary key now rules that out structurally; the
  single transaction remains, so a concurrent pass converges instead of
  interleaving. Embeddings are computed **before** the transaction opens,
  so the slow embed I/O never holds a write lock.
- **Recall history survives an edit.** A position whose text is unchanged
  keeps its `created_at` and its `last_recall_at` / `recall_count_30d`; a
  position whose text changed keeps the slot but resets the counters,
  because the history belonged to the old content. Under the minted-id
  design every id changed on every reindex, so a `recall_log` entry
  stopped resolving as soon as its page was touched.

**Section dedup.** Identical section texts on one page collapse to a
single desired row before the write, so a page that genuinely repeats a
block indexes it once.

Each row is markerless: there is no on-disk span to repair, and **no
ACL**. Read access to a section is the *wiki's*, held once in
`smart_wikis` and resolved per wiki by
[`recall::search_sections`](../../crates/mwe-core/src/recall.rs) — see
[recall-pipeline.md](recall-pipeline.md#the-two-corpora). That is what
makes a `shared_with` edit a **single-row** write
([`sections::upsert_smart_wiki`](../../crates/mwe-core/src/sections.rs))
where it used to re-stamp one row per indexed section.

## The `smart_wikis` registry

[`project_smart_wiki_registry`](../../crates/mwe-core/src/reindex.rs)
walks the tree and mirrors every smart wiki's `_meta.md` into the
`smart_wikis` table: resolved owner (the scope principal), `shared_with`,
`project_id`, `wiki_type`. Rows whose wiki is gone or no longer smart are
dropped, so the projection is self-healing.

`_meta.md` on disk stays the **source of truth** — this table is a cache,
which is why the projection re-runs at boot *and* on every safety-net
tick: a hand-edited `smart:` flag or roster still wins, and a revoke made
by editing the file closes the recall window on the next tick without
anyone touching the dashboard.

What it buys is that "which wikis are smart?" and "who may read this
wiki?" become part of a SQL query. Before it existed, the smart flag was
reachable only through the on-disk tree, so every "exclude the project
docs" filter had to run *after* the ranking — silently shrinking the
caller's result set whenever the excluded family dominated the top-K.

## The one-time backfill

[`backfill_smart_sections`](../../crates/mwe-core/src/reindex.rs) is the
migration tail that moves legacy smart-wiki rows out of `fact_index`.
Embeddings are copied verbatim, so the move costs one pass and no
re-embedding. Legacy rows carry no ordinal, so each page's rows are
ordered by `fact_id` — `UUIDv7` is time-ordered, so that is their
original insertion order; any residual drift is corrected free of charge
by the next reindex, which re-derives the true order from disk and reuses
each section's stored vector by text.

Idempotent, and safe to re-run on a partially migrated store: a page
already present in `wiki_sections` is not re-copied, only its legacy rows
are dropped. Runs at `serve` boot alongside the registry projection
(`boot_smart_wiki_passes`); both are best-effort and non-fatal.

## Smart wikis — indexing on push (queued)

The marker protocol hides the server's own `atomic_write`s from the
watcher, so push-written pages would otherwise wait for the safety-net
sweep. Instead
[`call_wiki_admin_push`](../../crates/mwe-mcp-server/src/mcp/tools.rs)
**enqueues** each touched page (writes + deletes, as
`WatchedChange::Touched` — `reindex_file` re-derives from disk either
way) onto the same channel the watcher feeds (`McpState.reindex_tx`, the
second producer handle returned by `spawn_reindex_pipeline`) and acks
immediately, reporting `"section_indexing": "queued"` in the response.

Off-request-path indexing is load-bearing for bulk imports: embedding a
100+ section page inline held the HTTP response for minutes, tripping
proxy timeouts (Cloudflare cuts at ~100 s) on a push the origin had
already committed — the client saw an error, retried, and every
concurrent retry re-embedded the same sections from scratch. The single
queue worker serialises those retries instead: by the time a duplicate
queue entry runs, the first pass has stored its vectors and the re-run
is an idempotent no-op.

Still best-effort: the safety-net sweep is the backstop, an index hiccup
never fails a committed push, and the atomic drop+insert (above) keeps
an overlapping watcher event (the out-of-window marker race) from
accumulating duplicate rows. Without a queue handle (`reindex_tx: None`
— tests, degraded boot without a watcher) the handler falls back to the
old inline synchronous indexing and reports `"section_indexing":
"inline"`, which keeps recall-after-push deterministic in tests.

## Diff algorithm — standard wikis (offset-and-existence repair)

The canonical claim text for a standard-wiki fact is produced by the
buffer→promote→compile chain, and the compiler keeps `fact_index` in
sync after each page write via
[`fact_index::move_region`](../../crates/mwe-core/src/fact_index.rs):
the row's `text` stays the canonical claim used for embedding/dedup
while the marker wraps a **prose span** — deliberately a different
string. The reindex therefore never copies disk bodies into rows:

| Marker on disk? | Row in DB? | Action |
|---|---|---|
| Yes (no row) | No | **nothing** — stale render residue or a hand-pasted marker; facts cannot be authored by editing narrative prose, and the next compile rewrites the page |
| Yes (`fact_id` known) | Yes, offsets equal | **no-op** |
| Yes (`fact_id` known) | Yes, offsets drift | `fact_index::move_region` — offsets repaired, `text` untouched, no re-embed |
| No (file readable) | Yes, **with** offsets | `mark_forgotten_at(REASON_MARKER_REMOVED)` — the operator deleted a rendered region: the forget gesture (guarded, see below) |
| No / file missing | Yes, offsets **NULL** | **spared** — a pending render, see below |
| File missing | Yes, with offsets | `mark_forgotten(REASON_FILE_REMOVED)` |

Markers without an `f=<UUIDv7>` attribute are skipped on purpose: those
are region-level ACL wrappers, not indexable facts.

**The pending-render invariant.** Region offsets in `fact_index` mean
"rendered on disk". [`wiki_capture`](capture-and-dedup.md) commits the
row with offsets NULL and stamps them only after the page write; the
comment-channel `add` inserts offset-less rows the next compile weaves
into prose. An offset-less row is a **committed fact whose prose does
not exist yet** — the existence sweep must not mistake it for an
orphan, so both tombstone paths skip it. The forget gesture applies
exactly to what the operator could see in the file.

Neither tombstone path triggers the retirement page-strip
(`strip_fact_region`): here the missing file/marker is the **trigger**
of the tombstone, not a leftover to clean — there are no bytes to
excise.

## The orphan-sweep guard

The marker-removed tombstone (standard wikis only) goes through
[`fact_index::mark_forgotten_at`](../../crates/mwe-core/src/fact_index.rs),
whose UPDATE is guarded on `source_path = <swept file>`. The sweep
compares the file's markers against a **snapshot** of the rows claiming
that `source_path`, and a concurrent
[promote apply](proposal-apply-engine.md) can legitimately repoint a
row to another page between the snapshot and the tombstone — the
`WriteMarker` suppression is best-effort (the guard is dropped before
the event is consumed), so the watcher does observe the server's own
writes. The guard makes the stale observation harmless: a row that
moved away no longer matches the swept path and survives; a row that
still claims the swept page and lost its marker is a real hand
deletion and is forgotten. Together with the promote handler's
DB-first ordering (rows repointed as pending renders **before** the
markers leave the source page on disk), a REM move can never be
mistaken for a forget gesture.

The **compiler** applies the same DB-first ordering to **plan moves**
([`prepoint_plan_moves`](../../crates/mwe-core/src/compiler.rs)): when a
new compilation plan reassigns a fact from page A to page B, the row is
repointed onto B as a pending render before either page is written, so
A's rewrite (which drops the marker) never strands a row where this
sweep would read it as a forget gesture — and a destination compile
that soft-fails leaves a sweep-exempt pending render, not a tombstone.
See [the cross-page commit point](narrative-compiler.md#cross-page-moves--the-db-first-commit-point).

## Watcher glue

[`run_watcher_loop`](../../crates/mwe-core/src/reindex.rs) consumes the
`tokio::mpsc` channel exposed by [`WikiWatcher::start`] and routes every
`WatchedChange` through `reindex_file`:

- `Touched(path)` and `Removed(path)` → one `reindex_file` call.
- `Renamed { from, to }` → one call per endpoint (the marker filter
  already suppresses the rename pair when it originated from
  `atomic_write`; the consumer treats them as two independent touches
  so a rename from `intro.md` to `intro-2.md` correctly drops the
  source path's rows and — on smart wikis — re-sections the
  destination page).

[`spawn_watcher_loop`](../../crates/mwe-core/src/reindex.rs) wraps
`tokio::spawn` and is how
[`mwe-mcp serve`](../../crates/mwe-mcp-server/src/main.rs) wires the
loop at startup.

## Safety net — smart-wiki pages only

`notify` can drop events under NFS, a suspended laptop, or a brief
crash window. `run_safety_net_loop` ticks every [`SAFETY_NET_INTERVAL`]
(default 5 minutes) and runs `reindex_full`. The first tick is
discarded so a fresh startup does not slam the embedder before any
edit had a chance to fire.

`reindex_full` first refreshes the `smart_wikis` registry (so a
hand-edited `smart:` flag or `shared_with:` roster lands within a tick),
rebuilds every wiki's captures buffer from its durable
`_captures.md` journal
([`capture_buffer::reindex_capture_journal`](../../crates/mwe-core/src/capture_buffer.rs)
— best-effort, never indexed itself), then **section-indexes only smart
wikis** (the per-page no-op fast path keeps the tick cheap on an idle
tree) and finishes with a **deleted-page sweep**: any indexed page that
no longer exists on disk has its sections hard-dropped (the markerless
counterpart of the standard orphan tombstone, recovering a `Removed`
event the watcher missed). Standard pages are excluded from the
periodic tick even though `reindex_file` is standard-wiki-safe per
event: unlike the watcher, the tick has no own-write suppression, so it
could observe a mid-compile window (a fact moved off page A whose row is
repointed only when page B compiles) and tombstone a live row.
Narrative repair is strictly event-driven — which matches its mandate:
third-party hand edits.

The startup boot path also calls
[`watcher::sweep_stale_markers`](../../crates/mwe-core/src/watcher.rs)
before arming the watcher, so a crashed writer's orphan
`*.mwe-write-in-progress` cannot keep suppressing legitimate
post-restart edits.

## Marker filter & `inotify` race

The `WriteMarker` suppresses events on `<path>` whose sibling
`<path>.mwe-write-in-progress` is fresh. Inside `atomic_write` the
marker is dropped as soon as the function returns, but the kernel may
deliver the rename event to the watcher thread some milliseconds
later. The window is small but non-zero.

This is fine because `reindex_file` is idempotent: a stray event after
the marker is gone simply re-reads the file, finds it already
consistent with `fact_index`, and updates zero rows. The cost is one
segment/parse + one `find_active_by_source_path` + zero writes —
bounded and rare.

The unit test
[`watcher_suppresses_target_events_while_marker_is_fresh`](../../crates/mwe-core/src/watcher.rs)
covers the in-window filter; the integration test
[`write_with_held_marker_is_suppressed_by_filter`](../../crates/mwe-core/tests/watcher_reindex_roundtrip.rs)
covers the held-marker case end-to-end through the consumer; the
out-of-window race is documented but not asserted (it would be flaky).

## Server wiring

[`bootstrap_state`](../../crates/mwe-mcp-server/src/main.rs):

1. `sweep_stale_markers` runs before the watcher is armed.
2. `WikiWatcher::start` creates the `notify` watch on
   `<workdir>/wikis/`; the watcher value is leaked (`Box::leak`) so its
   `Drop` does not tear down the `notify` thread before process exit.
   It also hands back a second producer into the event channel — stored
   as `McpState.reindex_tx`, the queue `wiki_admin_push` enqueues its
   own written pages on (see [indexing on push](#smart-wikis--indexing-on-push-queued)).
3. `spawn_watcher_loop` + `spawn_safety_net_loop` are fired with a
   shared `Arc<WikiTree>` + `Arc<dyn Embedder>`.

The HTTP transport gets the pipeline through the `bootstrap_state`
helper.

## Embedder-identity guard (roadmap 18g)

The vectors in `fact_index` are only comparable to a query embedding when
both come from the **same** embedding model. Swapping the configured
embedder (a different model, or a different vector dimension) would
silently corrupt cosine similarity — a dimension change breaks it
outright. [`reindex::check_embedder_identity`](../../crates/mwe-core/src/reindex.rs)
catches it.

The store records the embedder it was built with in the `engine_meta`
key/value table ([0041](engine-db-and-migrations.md#migration-ledger)):
`embedder_model_id` + `embedder_dim`. At serve startup, after the
embedder is built, the check compares the configured embedder against the
recorded identity and returns one of:

- **`Stamped`** — no identity recorded (a fresh store, or one upgraded
  from before the guard with a consistent dimension); the configured
  embedder is written as the store's identity.
- **`Match`** — the configured embedder matches what the store was built
  with.
- **`Mismatch`** — they differ; similarity search is wrong until a full
  reindex re-embeds every fact. The recorded identity is **never**
  overwritten on a mismatch — the operator must reindex. `cmd_serve_http`
  surfaces it as a loud `warn!` (non-fatal: the operator may be
  mid-migration); the dashboard health page is the richer surface
  (roadmap 19b).

A store with no recorded identity but pre-existing vectors of a different
dimension (an upgrade, or a model swapped before the first stamp) is
caught via [`fact_index::sample_embedding_dim`](../../crates/mwe-core/src/fact_index.rs)
rather than stamped over — the dimension mismatch is the dangerous one.
For the common Ollama-`bge-m3` → bundled-`bge-m3` migration the vectors
are identical (cosine 1.0000, validated by the 18a spike), so the model
ids match and no reindex is needed.

## Tests

`reindex::tests` covers the smart-wiki section index (section-indexes a
page and creates **no** `fact_index` row / idempotent on an unchanged
page / re-sections on edit / drops a removed section / drops a stale tail
position / hard-drops on file delete / `reindex_full` section-indexes
smart and skips standard), the registry projection (holds the wiki-level
ACL while the sections hold none / drops a wiki that stopped being
smart), the one-time backfill (legacy rows moved with their embeddings,
ordered by `fact_id`, then idempotent), the
standard-wiki repair (never-creates-rows, offsets-repaired-without-
touching-text, pending-render rows spared on both tombstone paths), the
retirement disk half (region excised + neighbours re-synced, active fact
refused, page-level strip robust to stale offsets, sweep cleans non-plan
pages / skips plan pages / converges), the boot `wiki_id` reconcile
(divergent row fixed by longest prefix, nested sub-wiki, consistent /
unknown-path / retired rows untouched, idempotent), and the shared
plumbing (path-outside-tree, wiki picking).
`tests/watcher_reindex_roundtrip.rs` proves the wiring end-to-end
through a real `notify` watcher + `tokio::spawn` consumer on a smart
wiki (third-party write → section row, held-marker suppression,
third-party delete → hard drop). `sections::tests` covers the store
itself (positional upsert, recall history preserved on an unchanged
position and reset on a changed one, tail drop, idempotence, wiki-scoped
candidates, registry round-trip).

## Scope-out

These capabilities are not implemented today (planned — see the
roadmap):

| Not yet supported | Why |
|---|---|
| Startup full re-scan (warm cache) as a config knob | Operators on slow Ollama would block startup |
| Cross-platform watcher tests (macOS, Windows) | Linux-primary platform note |
| Coalescing burst edits (Hub Writer debounce) | The pipeline survives N successive reindexes; debouncing is a Hub Writer concern |
