---
title: Applicative WAL — design notes for `mwe-core::wal`
area: design-notes
status: implemented
last_review: "2026-06-07"
---

# Applicative WAL — `mwe-core::wal`

The DDL for `proposal_ops_log` and `rem_ops_log` (and the migration
that introduces them) lives with the rest of the schema in
[`engine-db-and-migrations.md`](engine-db-and-migrations.md); the code
of record is [`wal.rs`](../../crates/mwe-core/src/wal.rs). The chassis
is complete: the
journaling primitives, the recovery scan, the generic `OpInverse`
trait + `NoopInverse`, and the `rollback_stale_proposals` /
`rollback_stale_rems` recovery drivers all ship. Per-kind
rollback inverses for the apply path live with the steps they invert
(`mwe-core::{promote,dedup,forge}`); today they all rely on
atomic-write idempotency rather than custom `OpInverse` impls — the
generic `NoopInverse` is the dispatcher's recovery floor and stays
correct until the `bundle` multi-op kind needs real snapshot restore.

## Why an applicative WAL on top of SQLite WAL

`SQLite`'s own WAL keeps `engine.db` consistent across crashes — any
transaction either reaches `COMMIT` or rolls back on restart. That is
exactly what we want for a *single* SQL transaction.

It is **not enough** for `structure_proposal_apply` or a REM nightly
cycle, which interleave:

1. Filesystem writes (the canonical pattern `write .tmp + fsync +
   rename`, atomic at the rename step but not before).
2. DB updates (`sqlx::Transaction`).
3. Marker propagation across sibling files.
4. Cross-link rewrites (`[[…]]` updates in linked files).

No single SQL `COMMIT` can roll those steps back together because (1)
and (3-4) are not SQL. If the process dies between step 2 and step 3,
the DB has already committed a piece of state that the filesystem does
not reflect, and there is no automatic mechanism to undo it.

The **applicative WAL** is the protocol on top: every step is
journaled in `proposal_ops_log` (or `rem_ops_log`) with `status=pending`
*before* the step executes, then flipped to `done` after. On startup
we look for rows that never reached `done` and hand them to a
**recovery driver** that knows how to invert each kind of step.

## What lives in the WAL module vs what's handler-local

| Concern | In `mwe-core::wal` | Handler-local |
|---|---|---|
| Insert a row in `Pending` | `begin_proposal_op`, `begin_rem_op` | — |
| Lifecycle transitions | `mark_in_progress`, `complete_*`, `fail_*` | — |
| Find stale rows | `scan_stale_proposal_ops`, `scan_stale_rem_ops` | — |
| Generic recovery driver | `rollback_stale_proposals`, `rollback_stale_rems` with `OpInverse` trait | — |
| Default driver | `NoopInverse` — flips stale rows to `failed` with `error_msg="rolled_back_by_startup"` | — |
| Per-kind apply inverses | — | Per-kind drivers live in `mwe-core::{promote,dedup,forge}`. Today they rely on atomic-write idempotency, not on custom `OpInverse` impls — the `NoopInverse` floor is sufficient when handler retries are themselves idempotent. |
| Snapshot writes (`_snapshots/proposal/…`) | — | Reserved for the `bundle` multi-op kind handler, which is the only kind where N filesystem writes must roll back together. Not implemented today (planned — see the [roadmap](../roadmap.md)). |
| Cross-link rewrite inverter | — | A cross-link rewriter would own its own inverse step. Not implemented today because `wiki_id` stays stable across the handlers that exist (`promote` `paragraph_to_file`, `scope::wiki_change_scope`). |

The split is deliberate. The journaling primitives are step-kind
agnostic and stable; the rollback logic is intrinsically coupled to
the step it inverts and belongs next to it.

## Status lifecycle

```
Pending  ──┬─→  InProgress  ──┬─→  Done       (happy path)
           │                  │
           │                  └─→  Failed     (live error)
           │
           └─→  Failed                        (recovery: never started or stuck mid-step)
```

`Pending` and `InProgress` are both visible to the recovery scan after
the staleness window (default 5 minutes, [`DEFAULT_STALE_AFTER`](../../crates/mwe-core/src/wal.rs)).
`Done` and `Failed` are terminal and invisible.

The reason `Pending` is its own state rather than "InProgress from
inception" is that the gap between *journaling* the intent and
*starting* the work has to be observable separately for two reasons:

- If we crash between insert and step-start, the recovery driver has
  nothing to roll back (no side effect yet) — it just marks the row
  `Failed` and moves on. Differentiating `Pending` from `InProgress`
  makes that path explicit.
- For audit, knowing "the apply path was about to run this step but
  never got to it" is different from "the step ran partially". Both
  end up `Failed`, but the operator log preserves the distinction.

## Timestamps and ISO 8601

The `started_at` / `completed_at` columns are stored as TEXT in
ISO 8601 (`chrono::Utc::now().to_rfc3339()`). We deliberately do not
use SQLite's native `DATETIME` type — sqlx's chrono feature is not
currently enabled in this workspace, and TEXT-as-ISO-8601 sorts
correctly lexicographically (`'2026-05-17T12:00:00Z' < '2026-05-17T13:…'`)
so range queries on `started_at` still work without a type-conversion
detour.

If/when we want typed timestamps end-to-end, the migration is mechanical:
add `chrono` to the sqlx feature list, change the column type in the
migration, swap the bindings from `String` to `chrono::DateTime<Utc>`.

## Recovery scan contract

`scan_stale_proposal_ops(pool, older_than)` returns every row whose
`status IN ('pending', 'in_progress')` and whose `started_at < now -
older_than`. The expected caller is the startup path of `mwe-mcp serve`
which:

1. Calls the scan with `DEFAULT_STALE_AFTER` (or a configurable
   override).
2. Groups returned rows by `proposal_id`.
3. For each proposal, walks the steps in **reverse order** (`step_idx`
   DESC — already pre-sorted by the scan) and asks the per-kind
   rollback driver to invert each one.
4. After every step in a proposal is inverted (or marked unfixable),
   calls `fail_proposal_op` on each row to close the audit trail.
5. Marks the parent `structure_proposals.status = 'failed'` with
   `reason='crash_during_apply'`.

The REM equivalent (`scan_stale_rem_ops`) follows the same shape, but
the cycle ID — not the step index — is the grouping key, and the
inversion order is op-id ascending (REM ops within a cycle are
independent; we just need to undo each one's snapshot).

## Tests

The unit tests live in [`wal.rs`](../../crates/mwe-core/src/wal.rs)
`mod tests` — that module is the count of record. They cover the
journaling matrix and the generic recovery driver:

- `status_roundtrip` — `OpStatus::as_str` ↔ `OpStatus::parse` round-trip
  on all four variants + the unknown-status error path.
- `proposal_op_happy_path` — full `begin → in_progress → done` cycle,
  verifies `completed_at` is stamped.
- `scan_picks_up_only_stale_rows` — backdates one row by 1h via an
  UPDATE so the scan can find it without sleeping; verifies a fresh
  row is *not* returned.
- `fail_records_reason_and_clears_from_scan` — `fail_*` stamps
  `error_msg` and removes the row from the scan output.
- `rem_op_happy_path` — equivalent of the proposal happy path for REM.
- `rem_scan_returns_target_wiki_and_snapshot_path` — verifies the
  REM-specific columns (`target_wiki_id`, `snapshot_path`) come back
  populated.
- `rollback_stale_proposals_with_noop_flips_to_failed` /
  `rollback_stale_rems_with_noop_flips_to_failed` — the `NoopInverse`
  floor flips each stale row to `Failed` with
  `reason="rolled_back_by_startup"` and reports it as `rolled_back`.
- `rollback_stale_proposals_records_inverse_failures` — an `OpInverse`
  that returns `Err` records `rollback_failed: <detail>` in `error_msg`
  and counts as `failed_rollbacks` without aborting the sweep.

The "backdate via UPDATE" pattern in the scan/rollback tests avoids the
5-minute real-world window — every test ships against a fresh
tempdir-backed DB created by `db::open_or_init`, so the staleness floor
is observable within milliseconds via a single SQL update.

## What is intentionally not in the module

- **No transaction wrapper.** Journaling and the actual step run in
  *separate* transactions on purpose: the journal row must be visible
  to other connections (in particular, the recovery scan) even if the
  step's transaction rolls back. Wrapping both in one transaction
  would defeat the recovery protocol.
- **No retry logic.** A failed step is failed; the recovery driver
  inverts it and the upper-layer state machine (in `wiki.rs` for
  proposals, in `rem.rs` for REM cycles) decides whether to schedule a
  retry. WAL is the *log*, not the *scheduler*.
- **No GC.** Closed rows (`Done`, `Failed`) live forever for audit. A
  later housekeeping job can prune them based on age + retention
  policy, but today there is no automatic reaper.
