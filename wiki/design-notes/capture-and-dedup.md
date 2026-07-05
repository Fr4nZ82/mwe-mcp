---
title: Capture pipeline — wiki_capture/supersede/forget/link + jaccard dedup
area: design-notes
status: implemented
last_review: "2026-07-02"
---

# Capture pipeline

[`mwe-core::capture`](../../crates/mwe-core/src/capture.rs) is the
write-side flow that ties the three foundational floors (parser, filesystem
SSOT, `fact_index`) into the four atomic internal APIs the LLM ingest
and the dashboard ultimately call.

## The four operations

| API | Touches filesystem? | Touches `fact_index`? | Result |
|---|---|---|---|
| `wiki_capture` | ✓ (append marker region) | ✓ (insert row) | `CaptureAction::{Captured, Skipped}` |
| `wiki_supersede` | ✓ (append new region + excise the old one) | ✓ (insert + mark old superseded) | `CaptureAction::{Superseded, Skipped}` |
| `wiki_forget` | ✓ (excise the retired region, best-effort) | ✓ (`mark_forgotten`) | `ForgetOutcome { tombstoned }` |
| `wiki_link` | ✓ (append wikilink, no marker) | – | `LinkOutcome { source_path, link_start, link_end }` |

`wiki_supersede` and `wiki_forget` both own the retirement **disk half**:
after the authoritative DB tombstone lands, `reindex::strip_fact_region`
excises the retired region's bytes from its page (the one cleanup that
also reaches `rules.md`, which the compiler never rewrites) and settles
the row's offsets to NULL. The strip is **best-effort** — it refuses an
active row, soft-skips a missing page, and a failure is logged without
failing the caller; residue redacts fail-closed meanwhile and the
light-dream hygiene sweep converges it (the full map of strip sites is in
[redaction-policy](redaction-policy.md)). "Undelete" (`bundle` restore, a
dedup-merge revert) reactivates the row as a pending render: the next
compile re-renders its prose from the DB-authoritative claim text.

## Two ingest write paths: direct (requested containers) vs buffered (standard)

The four operations above are *direct-write* primitives: each touches the
published `.md` and the `fact_index` synchronously. On the ingest path
they fire only for the **live exception**: an explicitly **requested
container** (a list / collection / note the user asked to keep — the
classifier sets the `requested_container` flag, no hard-coded gate) is
written live via `wiki_capture` so it is there immediately. Every other
classified claim is **standard**: the ingest router stages it in the
per-wiki [narrative captures buffer](narrative-buffer.md) instead, and
the published `.md` becomes the nightly compiler's *output* rather than
the ingest path's. Narrative = non-smart — the routing keys off the
single per-wiki smart flag in `_meta.md` (`smart: bool`, legacy alias `companion:`); see
[smart-wikis.md](smart-wikis.md). The **companion (smart)
perimeter is unaffected**: smart wikis are filtered out upstream and
keep their own admin-tool write path.

`wiki_capture` / `wiki_supersede` stay load-bearing on both sides of the
fork. On the direct path they remain the ingest write for requested
containers. On the standard-wiki path they are the **promotion primitive**
the light dream reuses to turn a buffered capture into a `fact_index`
fact — so this page's step-by-step still describes what a standard-wiki
capture *becomes* once it is promoted, just not when it is written. A
buffered capture is already recallable before promotion through the
fresh-captures slot ([recall-pipeline.md](recall-pipeline.md)); the
buffer write side is documented in
[narrative-buffer.md](narrative-buffer.md).

## `wiki_capture` step-by-step

1. **Validate**: body non-empty, no literal `{{` / `}}` (markers are
   managed by mwe-mcp, never by the caller); page path passes
   [`is_safe_page_path`](wiki-filesystem-ssot.md).
2. **Locate**: resolve the `wiki_id` to a [`WikiHandle`](wiki-filesystem-ssot.md).
3. **Embed**: call the supplied `Arc<dyn Embedder>` on the body. A
   remote-embedder failure short-circuits *before* any durable write.
4. **Dedup**: fetch every active fact in the wiki **owned by the same
   principal as the new fact** (different owner ⇒ different fact — two senders
   adding to one `group:` page collapse to a shared item, but per-user facts
   that merely share a wiki, like an agent's behaviour rules each owned by the
   user who dictated it, stay distinct), **never crossing the rules-page
   boundary** (candidates pair only when both the new fact's page and the
   candidate's are `rules.md`, or neither is — a behaviour rule dedups
   rule-vs-rule; a rule skipped as a "duplicate" of an ordinary fact would
   never reach `rules.md`, so the behaviour-rules channel would never serve
   it, see [ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-to-the-consumers-own-wiki)),
   then compute jaccard 6-gram of `body` vs `row.text`, take the max score.
   - If `max ≥ dedup_threshold` (default
     [`DEFAULT_DEDUP_THRESHOLD = 0.85`](../../crates/mwe-core/src/recall.rs))
     → return `CaptureAction::Skipped { matched_fact_id, similarity }`
     with a freshly minted `fact_id` (so the caller's audit log has an
     anchor). Filesystem and `fact_index` are untouched.
   - Else → step 5.

   The candidate scan is the shared
   [`capture::best_dedup_candidate`](../../crates/mwe-core/src/capture.rs);
   the light dream re-runs it verbatim at promotion, so a buffered
   capture gets exactly the dedup a live write gets
   ([narrative-buffer §promotion](narrative-buffer.md#promotion--the-light-dream)).
5. **Render marker**: the bare runtime form `{{f=<UUIDv7>}}body{{/}}` —
   region key only. The ACL is **not** written into the marker: it goes
   into the `fact_index` columns at step 6, which are the authoritative
   source the render path gates by
   ([redaction-policy.md](redaction-policy.md), [marker
   grammar §0](marker-grammar.md)). The full attributed form exists
   only as the export serializer (`render_full_marker`). The new page
   contents and the region's byte offsets are computed here, before any
   durable write.
6. **Insert — the commit point**: `fact_index::insert` with the
   authoritative owner / allow / sender + topics + the embedding, and
   **region offsets NULL**. Offsets mean "rendered on disk", and the
   marker is not on disk yet — an offset-less row is a *pending render*
   the [reindex existence sweep](reindex-pipeline.md) exempts.
7. **Atomic write**: append the marker on its own line, write through
   [`atomic_write`](wiki-filesystem-ssot.md) (tempfile + persist +
   parent-dir fsync + `WriteMarker` guard). On failure the capture
   **compensates** — the row is tombstoned with
   `capture_file_write_failed` — so the caller's error response and the
   store agree.
8. **Stamp offsets**: `fact_index::move_region` records the rendered
   byte offsets. Best-effort: the capture is already durable, so a
   hiccup here leaves a pending render that the reindex offset repair
   or the next compile repoint heals.

The order matters in two ways:
- *Embed → dedup → insert → write* keeps the failure modes ordered
  from cheapest-to-reverse to costliest. A network blip on the
  embedder backs out for free.
- The **DB row commits before the file write**. The DB is the
  authoritative fact store: a crash between the two loses only the
  render (the next compile re-emits the region from the row); it can
  never resurrect a fact with a degraded ACL from the marker alone —
  the risk a file-first order would carry.

## `wiki_supersede` semantics

`wiki_supersede(old_fact_id, req)` calls `wiki_capture` with
`dedup_threshold = 1.01` (effectively off — supersede is explicit
intent, never accidental dedup), then calls
`fact_index::mark_superseded(old, new)` to chain the rows. Returns
`CaptureAction::Superseded { previous_fact_id, … }`.

`mark_superseded` is also the **contradiction closure** of the
[temporal-validity model](../concepts/memory-model.md): the same UPDATE
closes the predecessor's window — `valid_to = COALESCE(valid_to, now)`,
so an earlier concrete end (a dated commitment) is never *extended* —
and stamps `decay_reason = COALESCE(decay_reason, 'contradicted')`
([`fact_index::decay`](../../crates/mwe-core/src/fact_index.rs)). One
chokepoint serves the direct path and the buffered path alike (the
light dream applies the staged supersede hint through the same
function), so a superseded fact can no longer be left looking open.

If `old_fact_id` is unknown, the call errors with
`CaptureError::PreviousFactNotFound` *before* any write — the agent
gets a clean "you superseded a phantom" diagnostic.

## Jaccard 6-gram dedup

`recall::jaccard_6gram(a, b)` is character-level (window = 6), case-
folded, with whitespace runs collapsed to a single space. The choice
of 6 is empirical: the legacy MWE plugin landed on it after
*"manca il latte"* vs *"manca il pane"* (different groceries, must NOT
dedup) and *"manca il latte"* vs *"Manca il latte."* (same item,
trailing punctuation, MUST dedup). Both invariants are tested.

Why character 6-grams over word tokenization:
- robust on Italian compound forms ("fammelo sapere" tokenises
  differently from "fammi sapere" but their 6-grams overlap heavily);
- robust to typos and minor reword;
- O(n) to compute, O(min(|A|, |B|)) to intersect — cheap enough to run
  unbatched against every active fact in a wiki.

The capture loop computes the needle's n-gram set once and reuses it
via `jaccard_sets(&needle, &hay)` so the per-candidate cost is one
`HashSet` build + one intersection.

## Current limitations

The capture path does not yet do the following (planned work is tracked
in the [roadmap](../roadmap.md)):

| Not done | Why |
|---|---|
| **WAL applicative wrap** | Not needed: the `fact_index` insert is the capture's **commit point** (step 6 above). A failed page write compensates by tombstoning the row; a crash between insert and write leaves a pending render (offsets NULL) that the next compile re-emits and the reindex sweep never mistakes for an orphan. Multi-step structural writes elsewhere (the REM proposal kinds) keep their `proposal_ops_log` journaling. |
| **Cross-user attribution enforcement** | Per the [memory model](../concepts/memory-model.md), when `sender ≠ owner`, the sender must have read access to the owner's wiki. The agent composing the call today is the trusted writer surface; the preventive check is not yet wired into the dashboard/server-side caller composition. |
| **In-place region edit** | `wiki_capture` only appends. Editing an existing region in place (preserving its `fact_id`) — so `wiki_ingest_message` could refine a just-captured paragraph — is not supported. |

## Error surface (`CaptureError`)

| Variant | When |
|---|---|
| `EmptyBody` | body trims to empty |
| `BodyContainsMarker` | body contains `{{` or `}}` literally |
| `UnsafePagePath { path }` | `is_safe_page_path` rejected the page |
| `PreviousFactNotFound(FactId)` | `wiki_supersede` against an unknown id |
| `Wiki(WikiError)` | underlying filesystem error |
| `FactIndex(FactIndexError)` | underlying DB error |
| `Embedder(EmbedderError)` | embedder backend failed |
| `Db(sqlx::Error)` | direct DB calls (for `wiki_link` hooks) |
| `Io(io::Error)` | low-level filesystem read |
| `GeneratedFactIdInvalid(FactIdParseError)` | `uuid` crate produced a non-canonical v7 (defensive — should not happen) |

## Test coverage

`fact_index::tests` (insert / find / supersede / forget / drop-by-path /
counters / encodings), `recall::tests` (n-grams + jaccard invariants,
including the groceries dedup floor), `capture::tests` (validation,
marker rendering, happy path, dedup, supersede/forget/link, the
write-order compensation when the page write fails). The counts live in
the code, not here.
