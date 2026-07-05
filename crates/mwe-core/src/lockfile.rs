// SPDX-License-Identifier: AGPL-3.0-or-later
//! Single-writer lockfile ([single-writer lockfile](../../../wiki/design-notes/single-writer-lockfile.md)).
//!
//! Enforces the V1 invariant that **at most one `mwe-mcp` process owns a
//! given workdir at a time**. The mechanism is a kernel advisory lock
//! held on `<workdir>/.mwe-mcp.lock` via [`fs2::FileExt::try_lock_exclusive`].
//!
//! Why an advisory lock and not a manual PID-file scheme: the kernel
//! releases the lock when the file descriptor closes, including the
//! `SIGKILL` and panic paths. That gives us cross-platform orphan
//! cleanup *for free* without polling `/proc` or `OpenProcess`. The PID
//! and host metadata we write into the file are purely informational —
//! they help an operator diagnose `409 instance_running` errors but the
//! safety property does not depend on them.
//!
//! The handle returned by [`acquire`] is an RAII guard: dropping it (on
//! normal shutdown, panic, or SIGTERM after Tokio drains) closes the
//! file and releases the lock. There is no explicit `release()` method —
//! holding the guard is the only correct usage.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process,
};

use fs2::FileExt;
use thiserror::Error;

/// File name placed inside the workdir.
pub const LOCKFILE_NAME: &str = ".mwe-mcp.lock";

/// Errors specific to lockfile acquisition.
///
/// Surfaced as a dedicated enum (not via [`crate::Error`]) because the
/// caller — the `mwe-mcp serve` startup path — wants to differentiate
/// the `Held` case from generic IO failures in order to emit the
/// canonical `409 instance_running` exit code with the metadata.
#[derive(Debug, Error)]
pub enum LockError {
    /// Another process already holds the lock. The optional
    /// [`HolderInfo`] is best-effort: if the existing file is empty or
    /// malformed we still report `Held` (the kernel said the lock is
    /// taken, that is the source of truth).
    #[error("lockfile {path} already held{}", info_render(holder.as_ref()))]
    Held {
        /// Path of the contended lockfile.
        path: PathBuf,
        /// Best-effort metadata read from the lockfile body.
        holder: Option<HolderInfo>,
    },

    /// Anything else (permission denied, disk full, …).
    #[error("lockfile io error: {0}")]
    Io(#[from] std::io::Error),
}

fn info_render(info: Option<&HolderInfo>) -> String {
    info.map_or_else(String::new, |i| {
        format!(
            " (pid={}, started_at={}, hostname={})",
            i.pid, i.started_at, i.hostname
        )
    })
}

/// Metadata written into the lockfile body when the lock is acquired.
/// Best-effort informational only — see module docstring.
#[derive(Debug, Clone)]
pub struct HolderInfo {
    /// Operating-system process id that currently holds the lock.
    pub pid: u32,
    /// ISO-8601 timestamp of when the lock was acquired (UTC).
    pub started_at: String,
    /// `hostname` of the machine, or `"unknown"` when it cannot be read.
    pub hostname: String,
}

impl HolderInfo {
    fn for_self() -> Self {
        Self {
            pid: process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
            hostname: hostname(),
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let mut lines = raw.lines();
        let pid_line = lines.next()?;
        let started_at = lines.next()?.to_owned();
        let hostname = lines.next()?.to_owned();
        let pid = pid_line.strip_prefix("pid=")?.parse::<u32>().ok()?;
        Some(Self {
            pid,
            started_at: started_at.strip_prefix("started_at=")?.to_owned(),
            hostname: hostname.strip_prefix("hostname=")?.to_owned(),
        })
    }

    fn serialize(&self) -> String {
        format!(
            "pid={}\nstarted_at={}\nhostname={}\n",
            self.pid, self.started_at, self.hostname
        )
    }
}

/// Canonical lockfile path for a given workdir.
#[must_use]
pub fn lockfile_path(workdir: &Path) -> PathBuf {
    workdir.join(LOCKFILE_NAME)
}

/// RAII handle that holds the workdir lock for its lifetime.
///
/// The underlying `File` lives inside the guard and is never exposed —
/// callers cannot accidentally close or unlock it. The lock is released
/// when the guard is dropped (graceful shutdown, panic, SIGKILL).
#[derive(Debug)]
pub struct LockfileGuard {
    file: File,
    path: PathBuf,
    holder: HolderInfo,
}

impl LockfileGuard {
    /// Path of the lockfile this guard owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Metadata this process wrote into the lockfile body when it
    /// acquired the lock. Reading via this accessor sidesteps the
    /// Windows quirk where the lockfile is opened with `LockFileEx`
    /// in deny-others mode and `std::fs::read_to_string(guard.path())`
    /// fails with `ERROR_LOCK_VIOLATION` while the guard is alive.
    #[must_use]
    pub const fn holder(&self) -> &HolderInfo {
        &self.holder
    }
}

impl Drop for LockfileGuard {
    fn drop(&mut self) {
        // Best-effort unlock + close. The kernel will release on close
        // regardless of whether this call succeeds, so we deliberately
        // ignore the result rather than panicking on a teardown path.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Acquire the workdir lock or report who holds it.
///
/// On success, writes our `pid` / `started_at` / `hostname` into the
/// file body (truncating any stale content) and returns a guard that
/// must be kept alive for as long as the process expects to be the
/// single writer. Re-entering this function with the same workdir from
/// the same process while a guard is alive returns
/// [`LockError::Held`] — `fs2` advisory locks do not detect
/// same-process double-acquisition portably, but on the platforms we
/// support (Linux/macOS/Windows) the second `try_lock_exclusive` call
/// fails with `WouldBlock` because the OS treats the second open as a
/// distinct lock holder.
pub fn acquire(workdir: &Path) -> Result<LockfileGuard, LockError> {
    if !workdir.exists() {
        std::fs::create_dir_all(workdir)?;
    }

    let path = lockfile_path(workdir);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    if let Err(err) = FileExt::try_lock_exclusive(&file) {
        // Best-effort read of the existing holder metadata for the
        // operator-facing error message. We deliberately do not bail on
        // a read failure: the kernel already told us the lock is held,
        // missing metadata is not a reason to mask that.
        let mut buf = String::new();
        let holder = if file.read_to_string(&mut buf).is_ok() {
            HolderInfo::parse(buf.trim())
        } else {
            None
        };

        return if is_lock_contention(&err) {
            Err(LockError::Held { path, holder })
        } else {
            Err(LockError::Io(err))
        };
    }

    let info = HolderInfo::for_self();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(info.serialize().as_bytes())?;
    file.sync_all()?;

    Ok(LockfileGuard {
        file,
        path,
        holder: info,
    })
}

/// Discriminate kernel "lock already held" failures from generic IO
/// errors across the three platforms we support.
///
/// - POSIX (`fcntl`) surfaces contention as
///   [`std::io::ErrorKind::WouldBlock`] (EAGAIN / EWOULDBLOCK).
/// - Windows (`LockFileEx`) surfaces it as raw OS error
///   [`WINDOWS_ERROR_LOCK_VIOLATION`] (33) or
///   [`WINDOWS_ERROR_SHARING_VIOLATION`] (32), neither of which the
///   stdlib categorises with a stable `ErrorKind`.
fn is_lock_contention(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    matches!(
        err.raw_os_error(),
        Some(WINDOWS_ERROR_LOCK_VIOLATION | WINDOWS_ERROR_SHARING_VIOLATION)
    )
}

/// Windows `ERROR_LOCK_VIOLATION` raw OS error code returned by
/// `LockFileEx` when another handle already holds an overlapping lock.
const WINDOWS_ERROR_LOCK_VIOLATION: i32 = 33;
/// Windows `ERROR_SHARING_VIOLATION` raw OS error code that
/// `LockFileEx` occasionally returns instead of `LOCK_VIOLATION` when
/// the file is opened by another process without `FILE_SHARE_READ`.
const WINDOWS_ERROR_SHARING_VIOLATION: i32 = 32;

fn hostname() -> String {
    // No portable std API for hostname; fall back to env var that every
    // major OS sets. `HOSTNAME` covers Linux/macOS shells; `COMPUTERNAME`
    // covers Windows. Worst case we report "unknown" and the operator
    // still has PID + timestamp.
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First acquisition records our metadata. We assert against the
    /// `holder()` accessor instead of re-reading the file: on Windows
    /// `LockFileEx` deny-others mode makes a sibling `read_to_string`
    /// fail with `ERROR_LOCK_VIOLATION` while the guard is alive.
    #[test]
    fn first_acquire_writes_holder_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = acquire(dir.path()).expect("acquire");

        let holder = guard.holder();
        assert_eq!(holder.pid, process::id());
        assert!(!holder.started_at.is_empty());
    }

    /// A second concurrent acquisition (different OS file handle) sees
    /// the lock as held. We perform the second open in a spawned
    /// blocking thread because some platforms (notably Linux with
    /// POSIX advisory locks) treat re-locks from the same thread as
    /// success; spawning makes the distinction explicit.
    ///
    /// The contender's view of the holder metadata is asserted only on
    /// Unix: Windows `LockFileEx` deny-others mode blocks the second
    /// handle from reading the file body while the lock is alive, so
    /// `holder` legitimately surfaces as `None` there. The operator-
    /// facing error message uses `HolderInfo` opportunistically — the
    /// safety property is the `Held` discriminant, not the metadata.
    #[test]
    fn second_acquire_fails_with_holder_info() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = acquire(dir.path()).expect("first acquire");

        let path = dir.path().to_path_buf();
        let handle = std::thread::spawn(move || acquire(&path));
        let err = handle.join().expect("join").expect_err("must fail");

        match err {
            LockError::Held { holder, .. } => {
                #[cfg(unix)]
                {
                    let h = holder.expect("metadata should be present on Unix");
                    assert_eq!(h.pid, process::id());
                }
                #[cfg(not(unix))]
                {
                    let _ = holder;
                }
            },
            LockError::Io(io) => panic!("unexpected IO error: {io}"),
        }
    }

    /// Dropping the guard releases the lock so a fresh acquire works.
    #[test]
    fn dropping_guard_releases_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _g = acquire(dir.path()).expect("first acquire");
        }
        // Re-acquire from a different thread to avoid same-thread
        // POSIX-advisory short-circuits, same as the test above.
        let path = dir.path().to_path_buf();
        let g2 = std::thread::spawn(move || acquire(&path))
            .join()
            .expect("join")
            .expect("reacquire after drop");
        drop(g2);
    }

    /// `acquire` creates the workdir if it does not yet exist — useful
    /// for the `mwe-mcp init` path that runs before the operator
    /// pre-creates anything.
    #[test]
    fn acquire_creates_workdir() {
        let parent = tempfile::tempdir().expect("tempdir");
        let workdir = parent.path().join("nested/new-workdir");
        assert!(!workdir.exists());
        let _guard = acquire(&workdir).expect("acquire on fresh path");
        assert!(workdir.is_dir());
    }

    /// The serializer/parser are mirror images: parsing what we
    /// serialize round-trips. Guards against silent format drift.
    #[test]
    fn holder_info_roundtrip() {
        let info = HolderInfo {
            pid: 4242,
            started_at: "2026-05-17T12:00:00Z".to_owned(),
            hostname: "test-host".to_owned(),
        };
        let parsed = HolderInfo::parse(info.serialize().trim()).expect("parse");
        assert_eq!(parsed.pid, info.pid);
        assert_eq!(parsed.started_at, info.started_at);
        assert_eq!(parsed.hostname, info.hostname);
    }

    /// A lockfile from a previous run with garbled contents still
    /// surfaces a clean `Held` error (no metadata) when contended.
    /// Verifies we never bail out on a parse failure.
    #[test]
    fn malformed_existing_file_still_reports_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(lockfile_path(dir.path()), "garbage\n").expect("seed garbage");

        let _g = acquire(dir.path()).expect("acquire over garbage");

        let path = dir.path().to_path_buf();
        let err = std::thread::spawn(move || acquire(&path))
            .join()
            .expect("join")
            .expect_err("must fail");
        assert!(matches!(err, LockError::Held { .. }));
    }
}
