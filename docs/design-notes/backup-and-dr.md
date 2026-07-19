---
title: Backup & disaster recovery — snapshots and staged recovery
area: design-notes
status: implemented
last_review: "2026-07-20"
---

# Backup & disaster recovery

The unit of backup is the **workdir snapshot**. Under DB-authoritative
storage neither half of a workdir is reconstructible from the other:
`engine.db` holds the facts (claim text, per-fragment ACL, validity,
embeddings, buffers, op-logs) and the `.md` tree holds their prose
renders, styles, and narrative links. File reconstruction via the
[reindex pipeline](reindex-pipeline.md) is **not** a recovery path — the
standard-wiki sweep deliberately repairs bookkeeping only and cannot
recreate rows from bare markers. Disaster recovery = restore a
snapshot, never "rebuild the DB from the pages".

Three surfaces produce and consume snapshots, all sharing the same
mechanics: the **CLI** (`mwe-mcp backup`, cron / server-off use), the
**Backup console** (`/dashboard/admin/backup` — manual snapshot,
settings, the snapshots listing, restore, memory reset), and the
**automatic scheduler** (the `backup:` config section — see
[config schema](../protocol/config-schema.md)).

## Taking a snapshot — `mwe-mcp backup`

```
mwe-mcp backup --workdir ./work --out /backups/2026-06-10
```

Implemented by
[`mwe_core::backup::snapshot_workdir`](../../crates/mwe-core/src/backup.rs).
**Hot-safe**: no lockfile is taken and the source DB is opened
read-only, so the command runs next to a live `mwe-mcp serve`. Two
steps, in a load-bearing order:

1. **DB first.** `VACUUM INTO` writes a transactionally consistent
   point-in-time copy of `engine.db` into the destination — no WAL/SHM
   sidecars travel, the copy is a self-contained database file.
2. **Files second.** The rest of the workdir is copied: `wikis/`,
   `media/` (the content-addressed blob store — see
   [media pipeline](media-pipeline.md)), `prompts/` (operator
   overrides), `mwe-mcp.env` (API keys + token secret — the snapshot is
   as sensitive as the workdir itself), `mwe-mcp.config.yaml`,
   `tokens/`. Excluded: the live `engine.db*` trio (replaced by step
   1), the `.mwe-mcp.lock` single-writer lockfile, `logs/`, a pending
   `recovery-pending.json` staged-recovery marker (a snapshot that
   embedded one would re-trigger the recovery every time it was
   restored), and in-flight `*.mwe-write-in-progress` markers. The copy
   is exclude-based, so new workdir directories ride along
   automatically. Media extends the DB-before-files invariant for free:
   uploads write the blob **before** the catalog row and blobs are
   immutable, so a `media_catalog` row in the DB image always finds its
   blob in the newer file copy (an orphan blob is harmless garbage; a
   row without its blob cannot occur in a snapshot).

The destination must be empty and disjoint from the workdir (checked
both directions).

### Why DB-before-files is the right skew

A hot snapshot is not atomic across the two halves: writes can land
between step 1 and step 2. With the file tree **at least as new as**
the DB image, every divergence the snapshot can contain is one the
engine already self-heals after a restore:

| Skew in the snapshot | What happens after restore |
|---|---|
| Marker on disk, no row (captured after step 1) | Narrative: stale render residue, rewritten at the next compile. Companion: the reindex re-creates the row from the marker. |
| Row with offsets, marker hand-deleted after step 1 | The first sweep replays the operator's forget gesture — what they asked for. |
| Row without offsets (pending render) | Never touched by the sweep; the next compile emits its region. |

The reverse order (DB image *newer* than the files) is the dangerous
one — it could contain rendered rows whose prose never made it into the
copy, which a later sweep would tombstone. That is why the snapshot
never copies files before the DB.

## The snapshots home and the automatic scheduler

Automatic snapshots (and everything the Backup console lists) live in
one **snapshots home** — `backup.dir` in the YAML, defaulting to a
sibling of the workdir named `<workdir-name>-snapshots`. Snapshot
directory names carry their provenance as a prefix
([`SnapshotKind`](../../crates/mwe-core/src/backup.rs)): `auto-*`
(scheduler), `manual-*` (the console's suggested naming), `pre-restore-*`
/ `pre-reset-*` (the automatic safety snapshot a staged recovery takes).
Anything else in the home that contains an `engine.db` is listed as
`other`.

The scheduler
([`mwe_mcp_server::backup_scheduler`](../../crates/mwe-mcp-server/src/backup_scheduler.rs))
is a **due-check loop**, not a fire-on-tick interval: every five
minutes it reads the shared schedule handle (hot-swapped by the Backup
console's settings save — no restart) and compares "now" against the
last-run stamp persisted in `engine_meta`. Because the stamp lives in
the DB, a restart never re-fires a snapshot inside the interval. After
each successful run the oldest `auto-*` snapshots beyond
`backup.retention_auto` are pruned; manual, safety, and foreign
snapshots are never pruned. A failed run also advances the stamp (one
loud failure per interval instead of hammering `VACUUM INTO` against a
broken destination) and its outcome lands in `engine_meta` for the
console's status line. On by default: daily, retention 7.

## Restoring and the memory reset — staged recovery

A running server cannot safely replace its own workdir (the pool holds
`engine.db` open, the watcher holds the tree). The dashboard's
**Restore** and **Memory reset** are therefore **staged**
([`mwe_core::recovery`](../../crates/mwe-core/src/recovery.rs)): the
console writes a one-shot `recovery-pending.json` marker into the
workdir, and the next `mwe-mcp serve` boot applies it — after the
single-writer lockfile is taken, before anything opens the DB. Until
that boot the request is visible on the console and cancellable.

Common properties of both actions:

- **Safety snapshot first.** Before destroying anything, the current
  workdir is snapshotted into the home (`pre-restore-*` /
  `pre-reset-*`) — a recovery aimed at the wrong target is itself
  recoverable. If the safety snapshot fails, the action is refused and
  the workdir stays untouched.
- **One shot.** The marker is consumed *before* the action runs, so a
  failing recovery never becomes a boot loop. A failure before the
  point of no return refuses the action and boots normally; a failure
  after it aborts the boot with an error naming the safety snapshot.
- **Reported.** The outcome is persisted in `engine_meta`
  (`recovery.last`) and shown on the console after the restart.

**Restore** replaces every workdir entry except `logs/` and the live
lockfile with the snapshot's content — post-snapshot additions are
removed, so the workdir matches the snapshot exactly (any
snapshot-internal skew heals as described above). **Memory reset**
wipes the memory while keeping the installation: the memory tables
(facts, captures, events, proposals, op-logs, recall history, document
jobs, media catalog, tool log) are cleared in one transaction, then
`wikis/`, `media/`, and `training-spool/` are removed. Preserved:
enrollment, credentials (with `profile_initialized` cleared so the
welcome wizard re-seeds each profile), consumers and delegations,
tokens/2FA/OAuth state, custom skills, `engine_meta`, config, env, and
`prompts/`. Identity wikis are re-scaffolded empty for every enrolled
user, agent, and group.

**Restart-to-apply.** The console's "Restart now" button broadcasts the
same graceful shutdown as ctrl-c and then exits with the deliberate
code **75** (`EX_TEMPFAIL`): the provisioned systemd unit is
`Restart=on-failure`, so the non-zero exit relaunches the server (and a
clean stop still stays down). Unsupervised runs must be restarted by
hand — the console says so.

### Manual restore (server off / no dashboard)

1. **Stop the server** (the single-writer lockfile must be released).
2. **Replace the workdir** with the snapshot directory (move the broken
   one aside first; keep it for forensics).
3. **Start the server.** No lockfile travels with a snapshot, so no
   stale-lock cleanup is needed; the watcher and safety-net loops
   re-arm on boot.

Restoring an **old** snapshot over a newer reality loses everything
after the snapshot point in both halves consistently — there is no
partial-restore mode (per-wiki restore stays future work).

## Operational discipline

- **Cadence**: the automatic scheduler is the floor (daily, retention
  7 by default). Still snapshot manually before every risky operation
  (major upgrade, `mwe-mcp migrate`, prompt surgery).
- **The SQLite WAL is not a backup.** `journal_mode=WAL` (see
  [engine DB](engine-db-and-migrations.md)) gives crash durability for
  committed transactions on the *same* disk; it does not protect
  against disk loss, workdir deletion, or a bad migration. Never copy
  `engine.db` + `engine.db-wal` by hand while the server runs — that
  race is exactly what `VACUUM INTO` exists to avoid.
- **Secrets travel.** `mwe-mcp.env` is inside the snapshot by design
  (a restore must yield a working deployment, and losing the token
  secret would invalidate every consumer JWT). Store snapshots — and
  point `backup.dir` — somewhere with the same access discipline as
  the workdir itself.
