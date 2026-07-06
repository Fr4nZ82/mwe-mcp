// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cross-platform filesystem watcher for the memory-wiki SSOT
//! (reindex pipeline).
//!
//! ## What it does
//!
//! Wraps the `notify` crate (`inotify` on Linux, `FSEvents` on
//! macOS, `ReadDirectoryChangesW` on Windows) and produces a `tokio`
//! mpsc stream of [`WatchedChange`] events that downstream consumers
//! (re-index, REM watcher safety net) can read without blocking.
//!
//! ## The marker protocol
//!
//! Concurrent writes to a markdown file by mwe-mcp itself follow the
//! pattern `write(<path>.tmp) → fsync → rename(<path>.tmp, <path>)`.
//! That sequence still fires "create" + "modify" + "rename" events
//! that the watcher would otherwise pick up and forward, causing a
//! re-index storm right after a self-write.
//!
//! The fix is the **per-file marker** R.1.2 specifies: before the
//! write, the writer touches `<path>.mwe-write-in-progress`; after
//! the atomic rename, the marker is removed (or it auto-expires after
//! [`MARKER_TTL`]). The watcher applies two filters:
//!
//! 1. Any event whose path *is* a marker file is dropped — the marker
//!    is bookkeeping, not data.
//! 2. Any event whose path has a **fresh** sibling marker (`mtime`
//!    within [`MARKER_TTL`]) is dropped — the change is from mwe-mcp
//!    itself, not from a third-party editor like Obsidian.
//!
//! Stale markers (older than [`MARKER_TTL`]) do not suppress events;
//! they survive only as orphans from a crashed writer and are cleared
//! by [`sweep_stale_markers`] at startup.
//!
//! The RAII guard [`WriteMarker`] is the canonical way for write
//! paths to honour the protocol: `acquire(path)` creates the marker,
//! `drop` removes it. If the writer panics, the FS marker remains on
//! disk but is harmless past `MARKER_TTL` and gets swept on next
//! startup.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;

/// Suffix appended to a target file to mark "writing in progress".
/// `wikis/frodo/_meta.md` → `wikis/frodo/_meta.md.mwe-write-in-progress`.
pub const MARKER_SUFFIX: &str = ".mwe-write-in-progress";

/// How long a marker file is considered "fresh" enough to suppress an
/// event on its target.
///
/// After this window the marker is treated as a crashed-writer orphan
/// and ignored — the underlying event is delivered normally. Matches
/// the R.1.2 spec (30 s).
pub const MARKER_TTL: Duration = Duration::from_secs(30);

/// Errors raised by the watcher subsystem.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// Underlying `notify` crate failure.
    #[error("notify error: {0}")]
    Notify(#[from] notify::Error),
    /// Failure writing or deleting a marker file.
    #[error("marker io error: {0}")]
    MarkerIo(#[from] std::io::Error),
}

/// Result alias for the watcher module.
pub type Result<T> = std::result::Result<T, WatcherError>;

/// Compute the canonical marker path for `target` — `<target><SUFFIX>`.
/// Pure function, does not touch the filesystem.
#[must_use]
pub fn marker_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(MARKER_SUFFIX);
    PathBuf::from(s)
}

/// Return `true` if `p` looks like a marker file (its file name ends
/// in [`MARKER_SUFFIX`]). Cheap, allocation-free.
#[must_use]
pub fn is_marker_path(p: &Path) -> bool {
    p.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.ends_with(MARKER_SUFFIX))
}

/// Return `true` if the marker at `marker` exists and was touched in
/// the last [`MARKER_TTL`]. Used by the watcher to decide whether to
/// suppress an event on the associated target.
fn is_marker_fresh(marker: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(marker) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = SystemTime::now().duration_since(modified) else {
        // Modified time is in the future (clock skew) — treat as fresh.
        return true;
    };
    elapsed <= MARKER_TTL
}

/// Sweep `root` recursively for marker files older than [`MARKER_TTL`]
/// and unlink them.
///
/// Designed to be called once at server startup. Returns the count of
/// markers removed so the caller can log the cleanup.
pub fn sweep_stale_markers(root: &Path) -> Result<usize> {
    let mut removed = 0usize;
    sweep_recursive(root, &mut removed)?;
    Ok(removed)
}

fn sweep_recursive(dir: &Path, removed: &mut usize) -> Result<()> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            sweep_recursive(&path, removed)?;
        } else if ft.is_file() && is_marker_path(&path) && !is_marker_fresh(&path) {
            match std::fs::remove_file(&path) {
                Ok(()) => *removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// RAII guard around a marker file. Built via [`WriteMarker::acquire`]
/// and removes the marker on `drop`. Holding one across a write loop
/// is the canonical way to honour the R.1.2 protocol.
pub struct WriteMarker {
    path: PathBuf,
}

impl WriteMarker {
    /// Create the marker file for `target` and return a guard. The
    /// marker body is a single line `session_uuid iso_timestamp` for
    /// human inspection — semantics are conveyed entirely by the
    /// `mtime`.
    ///
    /// Idempotent against existing marker files: if one is already
    /// there, we overwrite it (its `mtime` resets). This matters when
    /// a previous writer crashed without cleanup.
    pub fn acquire(target: &Path) -> Result<Self> {
        let path = marker_path_for(target);
        let session = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new()));
        let body = format!("{session} {}", chrono::Utc::now().to_rfc3339());
        std::fs::write(&path, body)?;
        Ok(Self { path })
    }

    /// Path of the marker file this guard owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WriteMarker {
    fn drop(&mut self) {
        // Best-effort cleanup; if the unlink fails (e.g. the marker is
        // already gone) we accept it: the next startup sweep will pick
        // up any orphan that survives past MARKER_TTL.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One element of the watcher's output stream.
///
/// Collapses the rich `notify::EventKind` taxonomy into the three
/// shapes the re-index pipeline actually cares about.
///
/// The original `notify::Event` is *not* re-exposed: if a downstream
/// needs the full detail it should consume `notify` directly (the
/// watcher is the only canonical consumer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchedChange {
    /// File created or modified. The re-index pipeline re-parses the
    /// target on this kind.
    Touched(PathBuf),
    /// File or directory removed.
    Removed(PathBuf),
    /// Rename, with both endpoints when the platform reports them.
    /// On Linux some renames split into separate `RenameMode::From` /
    /// `RenameMode::To` events; we surface them as
    /// `Renamed { from, to: from.clone() }` and `Renamed { from: to,
    /// to: to }` respectively so the consumer sees both endpoints
    /// without us having to keep state.
    Renamed {
        /// Source path of the rename.
        from: PathBuf,
        /// Destination path. Same as `from` on partial-rename platforms.
        to: PathBuf,
    },
}

/// Active filesystem watcher.
///
/// `WikiWatcher::start` spawns the underlying `notify` watcher in its
/// own thread; the returned receiver is a tokio mpsc, ready to be
/// consumed by an async task. Dropping the [`WikiWatcher`] tears down
/// the watcher thread.
pub struct WikiWatcher {
    _inner: RecommendedWatcher,
}

impl WikiWatcher {
    /// Start watching `root` recursively. The returned receiver
    /// produces [`WatchedChange`] events filtered against the marker
    /// protocol. The internal channel is unbounded — the watcher does
    /// not back-pressure the OS event loop, since dropping events on
    /// re-index is much worse than ballooning the queue briefly.
    pub fn start(root: &Path) -> Result<(Self, mpsc::UnboundedReceiver<WatchedChange>)> {
        let (tx, rx) = mpsc::unbounded_channel::<WatchedChange>();
        let inner = build_watcher(root, tx)?;
        Ok((Self { _inner: inner }, rx))
    }
}

fn build_watcher(
    root: &Path,
    tx: mpsc::UnboundedSender<WatchedChange>,
) -> Result<RecommendedWatcher> {
    let root_for_filter = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| match res {
            Ok(event) => dispatch_event(&root_for_filter, &tx, &event),
            Err(e) => tracing::warn!(error = %e, "notify watcher error"),
        },
    )?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Decide which subset of paths to forward as [`WatchedChange`] for
/// the given raw event.
fn dispatch_event(_root: &Path, tx: &mpsc::UnboundedSender<WatchedChange>, event: &notify::Event) {
    use notify::event::{ModifyKind, RenameMode};

    let changes: Vec<WatchedChange> = match event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_)) => event
            .paths
            .iter()
            .filter_map(|p| filter_target(p).map(|p| WatchedChange::Touched(p.to_owned())))
            .collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
            let from = &event.paths[0];
            let to = &event.paths[1];
            if filter_target(from).is_some() || filter_target(to).is_some() {
                vec![WatchedChange::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                }]
            } else {
                Vec::new()
            }
        },
        EventKind::Modify(ModifyKind::Name(_)) => {
            // Partial rename — surface as a touch on the (still-)existing
            // endpoint. Consumers can dedupe by path if both From/To fire.
            event
                .paths
                .iter()
                .filter_map(|p| filter_target(p).map(|p| WatchedChange::Touched(p.to_owned())))
                .collect()
        },
        EventKind::Remove(_) => event
            .paths
            .iter()
            .filter_map(|p| filter_target(p).map(|p| WatchedChange::Removed(p.to_owned())))
            .collect(),
        _ => Vec::new(),
    };

    for change in changes {
        if tx.send(change).is_err() {
            // Receiver dropped — the consumer doesn't care anymore.
            return;
        }
    }
}

/// Return `Some(path)` only when the event for `path` should be
/// forwarded to the consumer. Filters out:
///
/// - Events on marker files themselves.
/// - Events on data files whose sibling marker is fresh (our own
///   write in progress).
fn filter_target(path: &Path) -> Option<&Path> {
    if is_marker_path(path) {
        return None;
    }
    let marker = marker_path_for(path);
    if is_marker_fresh(&marker) {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Long enough for `notify` to register the watch and deliver the
    /// first event on every supported platform; short enough that test
    /// failures fail fast.
    const RX_TIMEOUT: Duration = Duration::from_secs(2);

    /// Wait for a `WatchedChange` whose path matches `pred`, ignoring
    /// any unrelated traffic the OS may emit during the dance.
    async fn next_matching<F>(
        rx: &mut mpsc::UnboundedReceiver<WatchedChange>,
        mut pred: F,
    ) -> Option<WatchedChange>
    where
        F: FnMut(&Path) -> bool,
    {
        loop {
            match timeout(RX_TIMEOUT, rx.recv()).await {
                Ok(Some(change)) => {
                    let matched = match &change {
                        WatchedChange::Touched(p) | WatchedChange::Removed(p) => pred(p),
                        WatchedChange::Renamed { from, to } => pred(from) || pred(to),
                    };
                    if matched {
                        return Some(change);
                    }
                },
                Ok(None) | Err(_) => return None,
            }
        }
    }

    #[test]
    fn marker_path_appends_suffix() {
        let p = marker_path_for(Path::new("/tmp/wikis/frodo/_meta.md"));
        assert_eq!(
            p,
            Path::new("/tmp/wikis/frodo/_meta.md.mwe-write-in-progress")
        );
    }

    #[test]
    fn is_marker_path_recognises_suffix() {
        assert!(is_marker_path(Path::new("foo.md.mwe-write-in-progress")));
        assert!(!is_marker_path(Path::new("foo.md")));
        assert!(!is_marker_path(Path::new("foo.mwe-write-in-progress.md")));
    }

    #[test]
    fn write_marker_creates_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("note.md");
        std::fs::write(&target, "hello").unwrap();

        let marker_path = {
            let guard = WriteMarker::acquire(&target).expect("acquire");
            let m = guard.path().to_path_buf();
            assert!(m.exists(), "marker must exist while guard is alive");
            assert!(is_marker_fresh(&m), "fresh marker must be marked fresh");
            m
        };
        assert!(
            !marker_path.exists(),
            "marker must be cleaned up on guard drop"
        );
    }

    #[test]
    fn sweep_stale_markers_removes_old_files_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        let stale = nested.join("old.md.mwe-write-in-progress");
        std::fs::write(&stale, "stale").unwrap();
        // Backdate the mtime so the sweeper considers it stale.
        let stale_time = SystemTime::now() - Duration::from_secs(60 * 60);
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(stale_time))
            .expect("set mtime");

        let fresh = nested.join("new.md.mwe-write-in-progress");
        std::fs::write(&fresh, "fresh").unwrap();

        let not_a_marker = nested.join("plain.md");
        std::fs::write(&not_a_marker, "data").unwrap();

        let removed = sweep_stale_markers(dir.path()).expect("sweep");
        assert_eq!(removed, 1);
        assert!(!stale.exists(), "stale marker must be removed");
        assert!(fresh.exists(), "fresh marker must survive");
        assert!(not_a_marker.exists(), "data files must survive");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_delivers_create_event_on_untouched_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_w, mut rx) = WikiWatcher::start(dir.path()).expect("start");

        let target = dir.path().join("note.md");
        std::fs::write(&target, "hello").expect("write");

        let change = next_matching(&mut rx, |p| p.file_name() == Some("note.md".as_ref()))
            .await
            .expect("event for note.md");
        match change {
            WatchedChange::Touched(p) => assert!(p.ends_with("note.md")),
            WatchedChange::Renamed { from, to } => {
                assert!(from.ends_with("note.md") || to.ends_with("note.md"));
            },
            WatchedChange::Removed(_) => panic!("unexpected removed event"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_ignores_marker_file_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_w, mut rx) = WikiWatcher::start(dir.path()).expect("start");

        // Write a marker file. The watcher must not forward an event
        // for the marker itself.
        let target = dir.path().join("note.md");
        let marker = marker_path_for(&target);
        std::fs::write(&marker, "x").unwrap();

        // Allow the watcher to drain any in-flight irrelevant events.
        let _ = timeout(Duration::from_millis(200), rx.recv()).await;
        let marker_name = marker.file_name().unwrap().to_owned();
        let saw_marker_event = matches!(
            timeout(Duration::from_millis(400), async {
                next_matching(&mut rx, |p| p.file_name() == Some(marker_name.as_os_str())).await
            })
            .await,
            Ok(Some(_))
        );
        assert!(!saw_marker_event, "marker events must be suppressed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_suppresses_target_events_while_marker_is_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_w, mut rx) = WikiWatcher::start(dir.path()).expect("start");

        let target = dir.path().join("note.md");
        let _marker = WriteMarker::acquire(&target).expect("acquire");

        // Perform a write while the marker is held. The watcher must
        // suppress the event on `note.md`.
        std::fs::write(&target, "from-mwe-mcp").expect("write");

        let target_name = target.file_name().unwrap().to_owned();
        let saw_target_event = matches!(
            timeout(Duration::from_millis(500), async {
                next_matching(&mut rx, |p| p.file_name() == Some(target_name.as_os_str())).await
            })
            .await,
            Ok(Some(_))
        );
        assert!(
            !saw_target_event,
            "target event must be suppressed while marker is fresh"
        );
    }
}
