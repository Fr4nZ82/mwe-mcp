---
title: Reindex pipeline — repairing the page ↔ fact_index bookkeeping
area: design-notes
status: implemented
last_review: "2026-07-02"
---

# Reindex pipeline

`mwe-core::reindex` is the consumer side of
[`mwe-core::watcher`](../../crates/mwe-core/src/watcher.rs): when a
third-party editor (Obsidian, an operator typing in `nvim`) touches a
markdown file under `<workdir>/wikis/**`, this module re-parses it and
reconciles the [`fact_index`](../../crates/mwe-core/src/fact_index.rs).
What "reconcile" means depends on the wiki's family:

- **Smart wikis** (smart-consumer project wikis, written verbatim via
  `wiki_admin_*` or a direct filesystem edit): plain markdown with **no
  per-fragment `{{f=…}}` markers**, so recall indexes the **content**.
  Each page is chunked into heading-delimited sections, embedded, and the
  page's `fact_index` rows are **drop-and-reinserted**; every row carries
  the wiki-level ACL from `_meta` (owner + `shared_with`) projected onto
  it. A removed page's rows are **hard-dropped** (no tombstone). This is
  the markerless half of the [per-fragment-ACL pillar](smart-wikis.md):
  per-fragment markers/ACL stay the pillar of **standard** wikis only.
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
| [`reindex_full`](../../crates/mwe-core/src/reindex.rs) | Safety-net 5-minute tick, also usable as a startup catch-up | Walk every wiki; re-section every smart-wiki `*.md` + a deleted-page sweep |
| [`strip_fact_region`](../../crates/mwe-core/src/reindex.rs) | Act-time, from every retire path with engine context (supersede, forget, dedup merge — the roster is in [redaction-policy](redaction-policy.md)) | One row lookup + one page rewrite + a `reindex_file` re-sync; refuses active rows |
| [`strip_retired_regions_on_page`](../../crates/mwe-core/src/reindex.rs) / [`sweep_retired_regions`](../../crates/mwe-core/src/reindex.rs) | The light dream's retirement hygiene sweep over **non-plan** pages ([rem-cycle](rem-cycle.md)) | Per page: one parse + one lookup per marker + at most one rewrite; sweep bounded at `RETIRED_SWEEP_MAX_PAGES`/cycle |
| [`reconcile_wiki_ids`](../../crates/mwe-core/src/reindex.rs) | Once at `serve` boot, after the tree opens | One slim full-table scan of active rows + one targeted UPDATE per divergence |

The first two are **idempotent**: running them twice in a row over an
unchanged tree mutates zero rows the second time. For smart wikis a page
whose section texts and projected ACL already match the stored rows is a
no-op; that property is what makes the race documented under
[marker filter](#marker-filter--inotify-race) acceptable, and what keeps
the synchronous push-path index (below) from churning on a re-push. The
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
smart page into heading-delimited sections (reusing the document
segmenter [`document::segment_document`](../../crates/mwe-core/src/document.rs),
the heading chain prefixing each section's text so the heading words are
searchable), embeds each section, and reconciles the page's rows:

| Page state vs. stored rows | Action |
|---|---|
| Section texts + projected ACL all match | **no-op** (idempotent) |
| Anything drifts (edited/added/removed section, ACL change) | replace the page's rows in **one transaction** (`fact_index::replace_source_path_rows`) — drop every row at the page's `source_path`, insert one row per **distinct** section — an unchanged section's text reuses its stored embedding, only new/changed sections are re-embedded |
| File missing | hard-drop every row pointing at the page (no tombstone) |

**Two safeguards against duplicate section rows** (a duplicate would surface
as the same block twice in a `wiki_navigate` flat hit — identical text, identical
score, distinct `fact_id`):

- **Atomic drop+insert.** The drop and the inserts run in a single transaction
  ([`fact_index::replace_source_path_rows`](../../crates/mwe-core/src/fact_index.rs)).
  Multiple reindexers can target the same page at once — the synchronous push
  index, the watcher (which *does* observe the server's own write, see the
  [marker filter](#marker-filter--inotify-race)), the safety-net sweep — and
  because SQLite serializes writers, a second reindex's drop catches the first's
  just-committed rows instead of interleaving between a separate drop and a
  separate insert (which left two copies). Embeddings are computed **before** the
  transaction opens, so the slow embed I/O never holds a write lock.
- **Section dedup.** Identical section texts on one page collapse to a single
  desired row before insertion, so a page that genuinely repeats a block indexes
  it once. `smart_page_in_sync` compares against the deduped set, so a page that
  *already* carries duplicate rows (e.g. left by a pre-fix race) is detected as
  out-of-sync and rebuilt clean on the next reindex.

Each reinserted row is markerless — `region_start`/`region_end` are
NULL (there is no on-disk span to repair) and the `fact_id` is freshly
minted per reinsert (the page carries no stable per-fact key). The
wiki-level ACL is **projected onto every row**: `owner_id` = the wiki's
resolved `scope` (the writing consumer's user), `allow_ids` = the
wiki's `_meta.shared_with` roster. Recall's per-fact visibility check
([`acl::can_read`](../../crates/mwe-core/src/acl.rs)) then honours the
wiki-level share with no recall-layer change — a `shared_with` grantee
sees the wiki's content because every section row carries them in
`allow`. Changing the wiki's ACL in `_meta` re-projects onto the rows on
the next reindex (the safety-net tick re-stamps, since the ACL drift
fails the no-op check).

## Smart wikis — synchronous indexing on push

A markerless smart page has no markers for the watcher to key on, and
recall must see pushed content immediately (and deterministically in
tests, where no watcher runs). So
[`call_wiki_admin_push`](../../crates/mwe-mcp-server/src/mcp/tools.rs)
section-indexes each touched page (writes + deletes) **synchronously
after the push commits**, by calling `reindex_file` per page. This is
best-effort: the filesystem watcher + the safety-net sweep are the
backstop, so an index hiccup never fails a committed push, and the
re-index is idempotent so a later watcher event is a no-op. When the
watcher event is not merely *later* but **overlaps** the synchronous
index, the atomic drop+insert (above) is what keeps the two from
accumulating duplicate rows.

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

`reindex_full` rebuilds every wiki's captures buffer from its durable
`_captures.md` journal
([`capture_buffer::reindex_capture_journal`](../../crates/mwe-core/src/capture_buffer.rs)
— best-effort, never indexed itself), then **section-indexes only smart
wikis** (the per-page no-op fast path keeps the tick cheap on an idle
tree) and finishes with a **deleted-page sweep**: any active row whose
page no longer exists on disk is hard-dropped (the markerless
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
  ([roadmap 19b](../roadmap.md)).

A store with no recorded identity but pre-existing vectors of a different
dimension (an upgrade, or a model swapped before the first stamp) is
caught via [`fact_index::sample_embedding_dim`](../../crates/mwe-core/src/fact_index.rs)
rather than stamped over — the dimension mismatch is the dangerous one.
For the common Ollama-`bge-m3` → bundled-`bge-m3` migration the vectors
are identical (cosine 1.0000, validated by the 18a spike), so the model
ids match and no reindex is needed.

## Tests

`reindex::tests` covers the smart-wiki section index (section-indexes a
page / idempotent on an unchanged page / re-sections on edit / projects
`shared_with` onto rows / drops a removed section / hard-drops on file
delete / `reindex_full` section-indexes smart and skips standard), the
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
third-party delete → hard drop).

## Scope-out

These capabilities are not implemented today (planned — see the
[roadmap](../roadmap.md)):

| Not yet supported | Why |
|---|---|
| Startup full re-scan (warm cache) as a config knob | Operators on slow Ollama would block startup |
| Cross-platform watcher tests (macOS, Windows) | Linux-primary platform note |
| Coalescing burst edits (Hub Writer debounce) | The pipeline survives N successive reindexes; debouncing is a Hub Writer concern |
