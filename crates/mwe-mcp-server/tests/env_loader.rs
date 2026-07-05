// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests for the workdir env loader.
//!
//! These live outside the crate's `forbid(unsafe_code)` so they can
//! call `std::env::set_var` to set up the "variable already present in
//! shell" case and assert that the loader does **not** override it.

use std::path::Path;

use mwe_mcp_server::env_loader::{ENV_FILE_NAME, LoadOutcome, load_workdir_env};

fn write_env(dir: &Path, body: &str) {
    std::fs::write(dir.join(ENV_FILE_NAME), body).expect("write env file");
}

/// Mutating the process env is **process-global**; concurrent test
/// threads racing on `set_var` will trample each other. We funnel
/// every test that touches `std::env` through this mutex.
fn with_env_lock<R>(f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let out = f();
    drop(guard);
    out
}

#[test]
fn shell_env_takes_precedence_over_file() {
    with_env_lock(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        // Pid-unique key so a stray re-run can't observe a stale value.
        let key = format!("MWE_ENV_LOADER_IT_PRECEDENCE_{}", std::process::id());
        // Pre-set the shell value before the loader runs.
        // SAFETY: protected by `with_env_lock`; no other test in this
        // file is observing this key concurrently.
        unsafe {
            std::env::set_var(&key, "already");
        }
        write_env(dir.path(), &format!("{key}=fromfile\n"));

        load_workdir_env(dir.path()).expect("loader ok");
        assert_eq!(
            std::env::var(&key).ok().as_deref(),
            Some("already"),
            "shell-set value must survive the loader (no override)"
        );

        unsafe {
            std::env::remove_var(&key);
        }
    });
}

#[test]
fn loads_keys_from_file_when_unset_in_shell() {
    with_env_lock(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = format!("MWE_ENV_LOADER_IT_LOAD_{}", std::process::id());
        // Ensure no leftover state from a previous test run.
        unsafe {
            std::env::remove_var(&key);
        }
        write_env(dir.path(), &format!("{key}=from_file\n"));

        let outcome = load_workdir_env(dir.path()).expect("loader ok");
        assert!(matches!(outcome, LoadOutcome::Loaded { .. }));
        assert_eq!(std::env::var(&key).ok().as_deref(), Some("from_file"));

        unsafe {
            std::env::remove_var(&key);
        }
    });
}
