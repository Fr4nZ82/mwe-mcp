---
title: Hierarchical wiki move — mwe-core::scope::wiki_change_scope
area: design-notes
status: implemented
last_review: "2026-06-07"
---

# Hierarchical wiki move — `mwe-core::scope`

[`mwe-core::scope`](../../crates/mwe-core/src/scope.rs) implements
`_internal.wiki_change_scope`. It re-parents a
wiki (and its entire subtree) inside the workdir while keeping the
moved wiki's `wiki_id` stable per the
[memory model](../concepts/memory-model.md) `wiki_id`-stability
invariant. It is the first primitive in the codebase that mutates
`parent_wiki_id`, and the first composite filesystem+DB write that
exists outside the `structure_proposals` apply chassis.

## Where it lives

- **Module**:
  [`crates/mwe-core/src/scope.rs`](../../crates/mwe-core/src/scope.rs)
  exports `wiki_change_scope(tree, pool, source_id, new_parent_id)`
  and the `ChangeScopeOutcome` value type.
- **Agentic tool wiring**:
  [`crates/mwe-dashboard/src/agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)
  exposes the primitive to the dashboard chat panel as the
  `wiki_change_scope` write tool (see
  [`agentic-chat.md`](agentic-chat.md)).
- **Composite-write semantics**: the validate-then-mutate sequence is
  documented in [What the call does](#what-the-call-does) below; it
  reuses the same per-step apply pattern as the
  [proposal-apply engine](proposal-apply-engine.md). The ACL-widening
  warning surface is not yet implemented.

## What the call does

```text
wiki_change_scope(tree, pool, source_id, new_parent_id) →
   1. plan_move          validate (cycle / self-move / inherit-root) before any mutation
   2. fs::rename         move <wikis>/<old-parent-chain>/<slug> → <wikis>/<new-parent-chain>/<slug>
   3. rewrite _meta.md   on the moved wiki: parent_wiki_id := new_parent_id
   4. sync parents       remove from old parent's children; insert into new parent's children
   5. rebase fact_index  UPDATE fact_index SET source_path = REPLACE(source_path, <old>, <new>)
   6. ChangeScopeOutcome { wiki_id (unchanged), old/new rel dirs, new_parent_wiki_id, facts_rebased }
```

`new_parent_id = None` **promotes** the wiki to a top-level root
(`<wikis>/<slug>` directly). A `new_parent_id = source_id` (or any
descendant of `source_id`) refuses pre-mutation with
`WikiError::InvalidFrontmatter`.

## Validation gates — all before any filesystem mutation

`plan_move` runs three preconditions in order. The function never
touches the disk on failure: if any gate trips, the call returns
`Err(...)` and the wiki tree is byte-identical to its pre-call state.

1. **Inherit-root** — a wiki promoted to root with `scope:
   inherit` would yield a structurally invalid `_meta.md` (the root
   has no parent to inherit from). The handler refuses with a
   descriptive error so the caller (dashboard, chat tool) can prompt
   the user to set a concrete principal first. This is the first
   concrete instance of the dashboard ACL-widening warning.
2. **Self-move** — moving a wiki under itself is a no-op pretending to
   be progress; refuse explicitly.
3. **Cycle detection** — moving a wiki under one of its own
   descendants would orphan part of the tree under itself. The check
   `new_parent_handle.rel_dir().starts_with(&source_rel_dir)` covers
   it.

A fourth case, **no-op** (the source already lives at the requested
target), short-circuits past steps 2–5 and returns a
`ChangeScopeOutcome` with `facts_rebased = 0` and unchanged dirs.

## Why a dedicated module (and not inside `mwe-core::wiki`)

`wiki.rs` is the single owner of the on-disk tree and explicitly does
not touch `fact_index`. `wiki_change_scope` is intrinsically a
composite write — it must touch both — so it lives next door rather
than inside the filesystem-only module. This mirrors the pattern of
the per-kind apply handlers (`promote`, `dedup`, `forge`) sitting
alongside the chassis in `proposals.rs` rather than inside it.

## `fact_index` rebasing

The new helper
[`fact_index::rebase_source_path_prefix(pool, old_prefix, new_prefix)`](../../crates/mwe-core/src/fact_index.rs)
runs a single bulk
`UPDATE fact_index SET source_path = REPLACE(source_path, ?, ?)
WHERE source_path LIKE ? || '%'`. Important properties:

- The prefix match is exact: `old_prefix` always ends with `/` so a
  wiki named `acme` cannot accidentally match facts whose
  `source_path` starts with `acmecorp/`.
- The update preserves `created_at`, `last_recall_at`,
  `recall_count_30d`, and the embedding BLOB. The same fact lives in
  the same row — only its filesystem coordinate changes.
- The row count is returned as `facts_rebased` so the dashboard /
  audit log can surface "you just moved a wiki affecting N
  facts" — useful UX feedback for a primitive that otherwise feels
  invisible.

## Cross-link rewriter — explicit no-op

The hierarchical move keeps `wiki_id` stable, so existing
`[[wiki_id]]` and `[[wiki_id/page]]` cross-links continue to resolve
through `WikiTree::locate(id)` unchanged. The cross-link
rewriter — which would scan-and-rewrite every page that mentions the
moved wiki — is therefore an explicit no-op for this primitive
today. If a path-based link format ever ships (e.g. an Obsidian
backwards-compat `[[some/relative/path]]`), the rewriter would live
here.

## Crash semantics — deferred WAL wrap

The current implementation is **not** wrapped in
[`proposal_ops_log`](applicative-wal.md). A crash between filesystem rename and the
`fact_index` rebase leaves rows pointing at the *old* `source_path`;
they still resolve (the marker on disk has moved with the directory,
but the row's stored `source_path` is stale). The next watcher
[reindex](reindex-pipeline.md) tick reconciles by re-reading the
marker from the new location and updating the row in place, so the
inconsistency window is bounded by the watcher cadence (5-minute
safety net at worst).

The applicative-WAL wrap that would make the multi-step write
atomic across crashes is not yet implemented, alongside the
`bundle`-kind handler in the proposal-apply chassis (the two share
the same per-step WAL shape, so one implementation serves both)
(planned — see the [roadmap](../roadmap.md)).

## Tests

Seven unit tests in
[`mwe-core::scope::tests`](../../crates/mwe-core/src/scope.rs) cover
the happy path plus the three validation gates plus the rebase
counter plus the inherit-root refusal plus the no-op shortcut. Two
additional tests in
[`mwe-core::fact_index::tests`](../../crates/mwe-core/src/fact_index.rs)
cover the prefix-rebase helper proper (empty-prefix refusal +
non-matching prefix returning zero rows).
