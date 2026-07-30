// SPDX-License-Identifier: AGPL-3.0-or-later
//! Test-only: a migrated `SQLite` database in a temporary directory that
//! **cleans up after itself**.
//!
//! Every unit test that needs a database needs a workdir to put it in,
//! and that directory has to outlive the helper that creates it — the
//! pool keeps using the files. The idiom that had spread through this
//! crate solved that by abandoning the guard:
//!
//! ```ignore
//! let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
//! db::open_or_init(dir.path()).await.unwrap()
//! ```
//!
//! That works, and it means the directory is never removed by anything,
//! ever. A full `cargo test --workspace` left **145** of them behind,
//! each holding a migrated database — and on the maintainer's host,
//! which is also the production host, `/tmp` is `tmpfs`: RAM. They had
//! reached ~20 000 directories / 12 GB by 2026-07-28, and when `/tmp`
//! filled, every shell on the machine began failing in ways that looked
//! like anything but a full disk (the test harness writes its
//! output-capture files there too).
//!
//! The fix is to hand the guard *back*, so the test's own scope owns it
//! — the shape this crate's sibling `mwe-dashboard` tests already use.
//! A helper returns the guard **alongside** what the test actually
//! wants:
//!
//! ```ignore
//! let (_workdir, pool) = TestWorkdir::with_db().await;
//! ```
//!
//! Deliberately plain: no `Deref` to the pool, no newtype the test has
//! to reason about. `sqlx`'s `Executor` is a generic bound rather than a
//! concrete parameter type, so a deref-coercing wrapper would not be
//! accepted by `.execute(&pool)` anyway — and an explicit binding says
//! what is going on. The `_workdir` binding is the whole mechanism:
//! while it is alive the directory exists, and when the test ends it is
//! removed.
//!
//! Reach for [`TestWorkdir`] instead of `tempfile::tempdir()` in any new
//! test that needs a database.

use sqlx::SqlitePool;
use tempfile::TempDir;

use crate::wiki::WikiTree;

/// Guard owning the temporary directories a test's database and wiki
/// tree live in. Dropping it removes them.
///
/// Not `Clone`: there is exactly one owner of a temporary directory, and
/// that is the test that made it.
#[derive(Debug)]
pub struct TestWorkdir {
    dirs: Vec<TempDir>,
}

impl TestWorkdir {
    /// A fresh migrated database in its own temporary workdir.
    pub async fn with_db() -> (Self, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db open");
        (Self { dirs: vec![dir] }, pool)
    }

    /// A fresh database **and** a [`WikiTree`] rooted at the same
    /// workdir, with `wikis/` pre-created so [`WikiTree::open`] can
    /// canonicalise it.
    pub async fn with_db_and_tree() -> (Self, SqlitePool, WikiTree) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("wikis")).expect("wikis dir");
        let pool = crate::db::open_or_init(dir.path()).await.expect("db open");
        let tree = WikiTree::open(dir.path()).expect("wiki tree");
        (Self { dirs: vec![dir] }, pool, tree)
    }

    /// As [`Self::with_db_and_tree`], but the tree lives in a **separate**
    /// workdir from the database — for the tests written that way, where
    /// the split is part of what they exercise. The guard owns both.
    pub async fn with_db_and_detached_tree() -> (Self, SqlitePool, WikiTree) {
        let db_dir = tempfile::tempdir().expect("tempdir");
        let tree_dir = tempfile::tempdir().expect("tempdir");
        let pool = crate::db::open_or_init(db_dir.path())
            .await
            .expect("db open");
        let tree = WikiTree::open(tree_dir.path()).expect("wiki tree");
        (
            Self {
                dirs: vec![db_dir, tree_dir],
            },
            pool,
            tree,
        )
    }

    /// The workdir holding the database — for tests that assert on files
    /// beside it.
    pub fn path(&self) -> &std::path::Path {
        self.dirs[0].path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property this module exists for: the directory is gone once
    /// the guard is.
    #[tokio::test]
    async fn the_workdir_is_removed_when_the_guard_drops() {
        let (workdir, pool) = TestWorkdir::with_db().await;
        let path = workdir.path().to_path_buf();
        assert!(path.join("engine.db").is_file(), "a migrated db is there");
        pool.close().await;
        drop(workdir);
        assert!(
            !path.exists(),
            "the temporary workdir must not survive its guard — leaking it \
             is what filled tmpfs on the production host"
        );
    }

    #[tokio::test]
    async fn both_directories_are_removed_for_a_detached_tree() {
        let (workdir, pool, tree) = TestWorkdir::with_db_and_detached_tree().await;
        let db_path = workdir.path().to_path_buf();
        let tree_path = tree.wikis_dir().to_path_buf();
        pool.close().await;
        drop(tree);
        drop(workdir);
        assert!(!db_path.exists(), "db workdir removed");
        assert!(!tree_path.exists(), "detached tree workdir removed");
    }

    /// The regression guard. The leak was not a slip — it was a written
    /// idiom, repeated 46 times, with a comment explaining why. So the
    /// thing to prevent is its *return*, and the only way a test can see
    /// that is by reading the sources.
    ///
    /// Deliberately narrow: it bans leaking a `tempfile` guard, and
    /// nothing else. Legitimate `Box::leak` and `mem::forget` elsewhere
    /// are none of its business.
    #[test]
    fn no_test_leaks_its_temporary_directory_again() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/")
            .to_path_buf();
        let mut offenders = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    // This module's own docs quote the bad idiom on purpose.
                    if path.file_name().is_some_and(|n| n == "test_db.rs") {
                        continue;
                    }
                    let Ok(src) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    for (i, line) in src.lines().enumerate() {
                        let leaks_a_tempdir = (line.contains("Box::leak")
                            || line.contains("mem::forget"))
                            && (line.contains("tempdir") || line.contains("TempDir"));
                        if leaks_a_tempdir {
                            let rel = path.strip_prefix(&root).unwrap_or(&path);
                            offenders.push(format!("{}:{}", rel.display(), i + 1));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a temporary directory's guard is being leaked, so the directory \
             will never be removed — return it to the caller instead (see \
             TestWorkdir). On the maintainer's host /tmp is tmpfs, so each of \
             these costs RAM on the machine serving production until reboot.\n\
             offending lines: {offenders:#?}"
        );
    }

    /// A tree rooted at the database's own workdir still resolves, and
    /// still goes away.
    #[tokio::test]
    async fn a_shared_tree_shares_the_guard() {
        let (workdir, pool, tree) = TestWorkdir::with_db_and_tree().await;
        let path = workdir.path().to_path_buf();
        assert!(tree.wikis_dir().starts_with(&path) || tree.wikis_dir().exists());
        pool.close().await;
        drop(tree);
        drop(workdir);
        assert!(!path.exists(), "shared workdir removed");
    }
}
