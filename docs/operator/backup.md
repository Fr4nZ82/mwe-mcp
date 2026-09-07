# Backup, restore, and what is kept

**Nav: Backup.** `/dashboard/admin/backup`

## Automatic snapshots

The scheduler takes a hot snapshot of the whole workdir into the snapshots
home on a fixed interval, prunes the oldest automatic ones beyond the
retention, and remembers when it last ran, so a restart never re-fires a
snapshot inside the interval.

- **Mode** — *interval (on)* or *disabled*. Manual snapshots always work.
- **Interval (seconds)** — distance between automatic snapshots; 86400 (daily)
  out of the box.
- **Retention (auto snapshots)** — how many to keep, 7 out of the box; `0`
  keeps everything. Manual and safety snapshots are never pruned.
- **Snapshots home** — must be **outside** the workdir. Empty means the
  sibling default. A snapshot contains the secrets and the cleartext memory:
  keep the directory owner-only.
- **Initial delay (seconds)** — warm-up before the first due-check, from the
  next restart.

**Save backup settings** writes them to `mwe-mcp.config.yaml`.

## Snapshot now

**Destination directory** must be empty and outside the workdir; the field
arrives pre-filled with a timestamped path. **Back up now** takes a hot,
consistent point-in-time copy of `engine.db` plus the Markdown tree, the config
and `mwe-mcp.env` — safe next to the live server, and identical to the CLI
`mwe-mcp backup --out`.

## Snapshots on disk

The table lists **Snapshot**, **Kind**, **Taken**, **Size**, **Files** and two
actions.

**Restore…** does not restore anything on the spot: it **stages** the workdir
to be replaced by that snapshot at the next server start, taking an automatic
safety snapshot first. Until then the page carries a banner naming the pending
recovery, who asked for it and when, with **Cancel pending recovery** and
**Restart now and apply**.

**Delete** removes a snapshot from disk.

## Memory reset

The danger zone at the bottom wipes **every memory**: facts, wikis, media,
captures, proposals, recall history, the training spool. It keeps the
installation: accounts, enrollment, consumers and their tokens, two-factor,
OAuth state, config, environment and prompt overrides. Identity wikis are
re-created empty and every user goes through the welcome wizard again. Type
`RESET` to confirm; like a restore, it is staged and applies at the next server
start, after an automatic safety snapshot.

## What is kept, and for how long

Five things grow with use, and each has its own window in
`mwe-mcp.config.yaml`. Every window is in days, and `0` on any of them means
*keep for ever*. A daily sweep applies them.

| What | Key | Out of the box |
|---|---|---|
| The per-call audit trail | `retention.audit_days` | 90 days |
| The page bodies that undo a consumer's push | `retention.undo_days` | 30 days |
| A deleted wiki waiting in `trash/` | `retention.trash_days` | 30 days |
| The per-call token ledger behind [Usage](usage-and-spend.md) | `usage.retention_days` | 400 days |
| The recall-trace journal | `recall.trace_retention_days` | 90 days |

Two of those are promises somebody will rely on. `undo_days` is the window in
which a consumer's push can still be reverted from a wiki's operation log:
after it the row stays — who pushed what, when — and only the undo is gone.
`trash_days` is how long a deleted wiki can be put back. The recall-trace
journal holds recalled memory verbatim, which is why it has a window of its
own.
