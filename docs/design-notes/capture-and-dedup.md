---
title: Capture pipeline — wiki_capture/supersede/forget/link + jaccard dedup
area: design-notes
status: implemented
last_review: "2026-08-06"
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
also reaches `@rules.md`, which the compiler never rewrites) and settles
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
they fire only for the **live exception**: a claim that already knows which
page it goes on is written now. Two shapes qualify — a `lista` item (adding to
a list, or creating one) and an explicitly **requested container** (a
collection / note the user asked to keep — the classifier sets the
`requested_container` flag, no hard-coded gate) — and both are written live via
`wiki_capture`, page and `fact_index` row together, so they are there
immediately. Every other classified claim is **standard**: the ingest router
stages it in the [narrative captures buffer](narrative-buffer.md), with **no
destination attached**, and the published `.md` becomes the compiler's *output*
rather than the ingest path's.

Collapsing the two — buffering the containers as well — was proposed on
2026-08-05 and rejected: the fresh recall slot is a *ranked top-K*, which
cannot serve a **whole list**, and the dedup scan below was never actually
decided twice (it is one function with two callers). The argument is kept in
[ingest-pipeline.md](ingest-pipeline.md#narrative-vs-direct-split) so it is
not re-proposed.
Narrative = non-smart — the routing keys off the single per-wiki smart flag
in `_meta.md` (`smart: bool`, legacy alias `companion:`); see
[smart-wikis.md](smart-wikis.md). The **companion (smart) perimeter is
unaffected**: smart wikis are filtered out upstream and keep their own
admin-tool write path.

`wiki_capture` / `wiki_supersede` stay load-bearing on the direct path: they
are the ingest write for a `lista` item and a requested container. On the
standard-wiki path the light dream does the equivalent work itself
(`dream_light::write_placed`), once the compilation plan has told it which page
the claim goes on — so this page's step-by-step still describes what a
standard-wiki claim *becomes*, just not when or by whom it is written. A
waiting claim is already recallable through the fresh-captures slot
([recall-pipeline.md](recall-pipeline.md)); the buffer write side is documented
in [narrative-buffer.md](narrative-buffer.md).

## `wiki_capture` step-by-step

1. **Validate**: body non-empty, no literal `{{` / `}}` (markers are
   managed by mwe-mcp, never by the caller); page path passes
   `is_safe_page_path` (`[A-Za-z0-9._-]` components, no traversal).
2. **Locate**: resolve the `wiki_id` to a `WikiHandle`. When the capture
   would **create** the page file, refuse a path that a case-insensitive
   mirror would collapse onto an existing entry or a reserved file
   (`wiki::page_path_case_hazard` + `wiki::page_case_conflict` →
   `PageCaseConflict`); appends to an existing byte-exact page skip the
   check. **"Byte-exact" is asked of the directory listing**
   (`wiki::page_exists_byte_exact`), never of `Path::exists`: on a
   case-folding filesystem the latter answers `true` for `Intro.md` when
   the file on disk is `intro.md`, which skipped the guard and appended
   one page's facts into another's.
3. **Embed**: call the supplied `Arc<dyn Embedder>` on the body. A
   remote-embedder failure short-circuits *before* any durable write.
4. **Dedup**: fetch every active fact in the wiki **whose subject is the same
   principal as the new fact's** (different subject ⇒ different fact — two senders
   adding to one `group:` page collapse to a shared item, but per-user facts
   that merely share a wiki, like an agent's behaviour rules whose subject is the
   user who dictated each one, stay distinct), **never crossing the rules-page
   boundary** (candidates pair only when both the new fact's page and the
   candidate's are `@rules.md`, or neither is — a behaviour rule dedups
   rule-vs-rule; a rule skipped as a "duplicate" of an ordinary fact would
   never reach `@rules.md`, so the behaviour-rules channel would never serve
   it, see [ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-by-scope-outside-fact-memory)),
   then compute jaccard 6-gram of `body` vs `row.text`, take the max score.
   - If `max ≥ dedup_threshold` (default
     [`DEFAULT_DEDUP_THRESHOLD = 0.85`](../../crates/mwe-core/src/recall.rs))
     → return `CaptureAction::Skipped { matched_fact_id, similarity }`
     with a freshly minted `fact_id` (so the caller's audit log has an
     anchor). Filesystem and `fact_index` are untouched.
   - Else → step 5.

   The candidate scan is the shared
   [`capture::best_dedup_candidate`](../../crates/mwe-core/src/capture.rs);
   the light dream re-runs it verbatim when it screens the queue, so a
   waiting claim gets exactly the dedup a live write gets
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
   authoritative subject / allow / sender + topics + the embedding, and
   **region offsets NULL**. Offsets mean "rendered on disk", and the
   marker is not on disk yet — an offset-less row is a *pending render*
   the [reindex existence sweep](reindex-pipeline.md) exempts.
7. **Atomic write**: append the marker on its own line, write through
   `atomic_write` (tempfile + persist +
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

## 🚨 Identical text is not a duplicate — the audience decides

**Before any similarity number matters:** two facts carrying the same content
are not necessarily one fact. The governing case is not style but governance —

> A fact that reached two users by two private routes, each holding it
> privately, must stay **two** facts. Merging them hands each user something
> they were never told, and it cannot be undone after the fact.

Founder's ruling, 2026-07-28. So similarity is a **candidate** signal, never a
sufficient one: the **audience** and the **provenance** decide. Anything that
consolidates memory must start from that, not from *identical ⇒ collapse* —
this is the per-fragment governance the product exists to sell, and a
consolidation feature that skips it sells the opposite.

Where it is enforced today: the nightly revisor's **audience gate**
([rem-cycle.md](rem-cycle.md#revisor--conciliatore-sub-job)) refuses
to nominate a pair whose reader sets differ, structurally, before the confirmer
sees it. The reader set is [`acl::reader_set`](../../crates/mwe-core/src/acl.rs)
— `subject ∪ allow ∪ sender`, read from beside `can_read` so the two cannot
drift. The write-time scan below is a different case (one author, one turn,
one audience by construction) and takes no such gate.

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
in the roadmap):

| Not done | Why |
|---|---|
| **WAL applicative wrap** | Not needed: the `fact_index` insert is the capture's **commit point** (step 6 above). A failed page write compensates by tombstoning the row; a crash between insert and write leaves a pending render (offsets NULL) that the next compile re-emits and the reindex sweep never mistakes for an orphan. Multi-step structural writes elsewhere (the REM proposal kinds) keep their `proposal_ops_log` journaling. |
| **Cross-user attribution enforcement** | Per the [memory model](../concepts/memory-model.md), when `sender ≠ subject`, the sender must have read access to the subject's own wiki — the two axes meet here: a fact about someone is written into a wiki that someone owns. The agent composing the call today is the trusted writer surface; the preventive check is not yet wired into the dashboard/server-side caller composition. |
| **In-place region edit** | `wiki_capture` only appends. Editing an existing region in place (preserving its `fact_id`) — so `wiki_ingest_message` could refine a just-captured paragraph — is not supported. |

## Error surface (`CaptureError`)

| Variant | When |
|---|---|
| `EmptyBody` | body trims to empty |
| `BodyContainsMarker` | body contains `{{` or `}}` literally |
| `UnsafePagePath { path }` | `is_safe_page_path` rejected the page |
| `PageCaseConflict { path, reason }` | creating the page would case-collide with an existing entry / reserved file on a case-insensitive mirror, or the `.md` extension is not lowercase |
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
