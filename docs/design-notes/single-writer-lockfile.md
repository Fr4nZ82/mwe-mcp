---
title: Single-writer lockfile — design notes for `mwe-core::lockfile`
area: design-notes
status: implemented
last_review: "2026-05-30"
---

# Single-writer lockfile — `mwe-core::lockfile`

This page is the canonical reference for the single-writer lockfile.
It documents both the contract — at most one process owns a workdir at
a time — and the **implementation choices** behind it.

## What the module is for

`mwe-mcp` is a **single-writer** application: at most one process owns
a given workdir at a time. The lockfile is the kernel-enforced gate
that makes that property real. Without it, two `mwe-mcp serve`
processes pointed at the same `--workdir` would race on `engine.db`
and the markdown filesystem and silently corrupt both.

The module lives at
[`crates/mwe-core/src/lockfile.rs`](../../crates/mwe-core/src/lockfile.rs).

## Mechanism — advisory lock, not manual PID-file management

A global lockfile `<workdir>/.mwe-mcp.lock` is acquired on startup. The
mechanism follows a single rule: **the kernel decides**.
`fs2::FileExt::try_lock_exclusive` asks `fcntl(F_SETLK)` on POSIX and
`LockFileEx` on Windows. Both APIs release the lock automatically when
the file descriptor closes, and the FD closes for every termination
path that matters (graceful shutdown, panic, `SIGTERM`, `SIGKILL`, OOM
kill). So an "orphan lock" — a stale file with a PID that no longer
exists — **cannot happen** with advisory locks: if the holder is dead,
the FD is closed, and `try_lock_exclusive` succeeds for the new
process. There is no manual "PID alive / PID dead" branch and no
cleanup of stale lockfiles to perform.

The PID, ISO timestamp, and hostname we write into the file body are
therefore **purely informational**. They drive the
`409 instance_running` error message ("pid=12345, started_at=…,
hostname=…") so an operator can identify the contending process. The
safety property does not depend on them — we never read the file to
*decide* whether the lock is held; the kernel told us.

This is also why the module ships no PID-liveness probe (`kill(pid, 0)`
on POSIX, `OpenProcess` on Windows). Such a probe would be a
defense-in-depth nicety against a malformed file from a previous
buggy build, but it would also be a portability burden — `nix` /
`sysinfo` / a hand-rolled `libc` call all have trade-offs — and the
kernel already makes the actual decision. We skipped it deliberately;
re-add it only if real-world incidents prove the file-body parsing
falls down.

## RAII guard — no explicit `release()`

Acquire returns `LockfileGuard`, an opaque handle that owns the `File`.
Drop on the guard runs `FileExt::unlock` and closes the FD. There is
no `release()` method — exposing one would invite "I'll unlock here
and re-lock later" patterns that conflict with the single-writer
invariant. The guard's lifetime IS the lock's lifetime.

The guard's `Drop` impl ignores the result of `unlock` because the
kernel will release on close regardless, and we do not want a teardown
path to panic over a kernel-state observation.

## Cross-platform notes

| Platform | Mechanism behind `fs2::try_lock_exclusive` | Orphan cleanup |
|---|---|---|
| Linux | `fcntl(F_SETLK, F_WRLCK)` (POSIX advisory) | Kernel releases on FD close |
| macOS | `fcntl(F_SETLK, F_WRLCK)` (POSIX advisory) | Kernel releases on FD close |
| Windows | `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK \| LOCKFILE_FAIL_IMMEDIATELY)` | Kernel releases on handle close |

The platform-specific behavior we deliberately do *not* exercise:

- **`flock(2)`** (Linux/BSD advisory locks tied to the open file table,
  not the process) — `fs2` uses `fcntl`-based locks instead. The
  semantic difference (released on every FD close to the file vs
  released on the *last* process FD close) does not matter for our
  single-FD usage.
- **POSIX `lockf(3)`** — same family as `fcntl(F_SETLK)`, no extra
  guarantees for us.
- **`flock` on NFS** — POSIX advisory locks over NFS are notoriously
  flaky. The V1 deployment target is a single-node home miniPC;
  remote-filesystem workdirs are not yet supported (planned — see the
  roadmap).

## Test surface

Six tests in [`lockfile.rs`](../../crates/mwe-core/src/lockfile.rs)
cover the matrix:

1. First acquisition writes the holder metadata.
2. Second acquisition (in a spawned thread, to avoid POSIX
   same-thread short-circuits) reports `Held` with the holder info.
3. Dropping a guard releases the lock so a fresh acquire succeeds.
4. `acquire` auto-creates a missing workdir.
5. `HolderInfo::serialize` / `::parse` round-trip.
6. A pre-existing garbled lockfile still surfaces a clean `Held` error
   on contention (we never bail out on a parse failure).

The "spawn a thread for the second acquire" pattern in tests 2 and 3
is load-bearing — POSIX `fcntl` advisory locks treat re-locks from
the same thread as a no-op success, which would mask the assertion.
This is documented inline in those tests so the next reader doesn't
"simplify" it away.

## Error surface

`LockError` has two variants:

- `Held { path, holder: Option<HolderInfo> }` — contention. The
  expected callsite is `mwe-mcp serve` startup, which converts it to
  the canonical `409 instance_running` exit code and prints the holder
  metadata.
- `Io(std::io::Error)` — anything else (permission denied, disk full,
  …). Falls through to the generic `mwe-mcp` startup error path.

Keeping these out of the global [`crate::Error`] enum is deliberate:
the startup-path branch needs to match on `Held` exhaustively to emit
the right exit code, and dragging in `io::Error` / `sqlx::Error` /
`serde_json::Error` variants there would make the match noisy.
