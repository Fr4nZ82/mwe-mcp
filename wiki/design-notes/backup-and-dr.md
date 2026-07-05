---
title: Backup & disaster recovery — the workdir snapshot
area: design-notes
status: implemented
last_review: "2026-06-12"
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
   1), the `.mwe-mcp.lock` single-writer lockfile, `logs/`, and
   in-flight `*.mwe-write-in-progress` markers. The copy is
   exclude-based, so new workdir directories ride along automatically.
   Media extends the DB-before-files invariant for free: uploads write
   the blob **before** the catalog row and blobs are immutable, so a
   `media_catalog` row in the DB image always finds its blob in the
   newer file copy (an orphan blob is harmless garbage; a row without
   its blob cannot occur in a snapshot).

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

## Restoring

Manual procedure (admin-only recovery *surfaces* — dashboard snapshots,
restore, safe reset — are roadmap item 4d):

1. **Stop the server** (the single-writer lockfile must be released).
2. **Replace the workdir** with the snapshot directory (move the broken
   one aside first; keep it for forensics).
3. **Start the server.** No lockfile travels with a snapshot, so no
   stale-lock cleanup is needed; the watcher and safety-net loops
   re-arm on boot, and any snapshot-internal skew heals as described
   above.

Restoring an **old** snapshot over a newer reality loses everything
after the snapshot point in both halves consistently — there is no
partial-restore mode (per-wiki restore is a 4d concern).

## Operational discipline

- **Cadence**: snapshot before every risky operation (major upgrade,
  `mwe-mcp migrate`, prompt surgery) and on a regular schedule sized to
  how much conversation history the operator can afford to lose.
- **The SQLite WAL is not a backup.** `journal_mode=WAL` (see
  [engine DB](engine-db-and-migrations.md)) gives crash durability for
  committed transactions on the *same* disk; it does not protect
  against disk loss, workdir deletion, or a bad migration. Never copy
  `engine.db` + `engine.db-wal` by hand while the server runs — that
  race is exactly what `VACUUM INTO` exists to avoid.
- **Secrets travel.** `mwe-mcp.env` is inside the snapshot by design
  (a restore must yield a working deployment, and losing the token
  secret would invalidate every consumer JWT). Store snapshots with
  the same care as the workdir.
