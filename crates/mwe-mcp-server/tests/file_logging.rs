// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smoke test for the rotating file sink wired by
//! `mwe-mcp-server::tracing_setup`.
//!
//! The global subscriber slot is single-shot, so this test cannot use
//! `tracing_setup::install` directly without colliding with other
//! integration tests. Instead it drives the same `build_file_appender`
//! helper through a scoped subscriber (`with_default`) which keeps
//! the test process clean: a single `info!()` is emitted, the worker
//! guard is explicitly dropped to flush the non-blocking writer, and
//! the contents of the rolling log file are asserted on.

use std::path::PathBuf;

use mwe_core::config::{Config, LogFileRotation};
use mwe_mcp_server::tracing_setup;
use tempfile::tempdir;
use tracing::info;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

#[test]
fn install_uses_workdir_logs_directory_by_default() {
    // The default-loaded `Config` (no `mwe-mcp.config.yaml` on disk)
    // must resolve the file sink at `<workdir>/logs/mwe-mcp.log` — the
    // friendly default documented in [logging](../../../wiki/design-notes/logging.md).
    let dir = tempdir().expect("tempdir");
    let cfg = Config::default();
    let resolved = cfg
        .logging
        .resolved_file_path(dir.path())
        .expect("default rotation enables the file sink");
    assert_eq!(resolved, dir.path().join("logs/mwe-mcp.log"));
}

#[test]
fn build_file_appender_writes_event_to_workdir_log() {
    let dir = tempdir().expect("tempdir");
    let target: PathBuf = dir.path().join("logs/smoke.log");

    let (writer, guard) =
        tracing_setup::build_file_appender(&target, LogFileRotation::Never).expect("appender");

    // Scoped subscriber — does not touch the process-global slot.
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(tracing_subscriber::EnvFilter::new("info")),
    );

    tracing::subscriber::with_default(subscriber, || {
        info!(target: "mwe_mcp_smoke", marker = "frodo-smoke", "rotating file sink reachable");
    });

    // Dropping the guard flushes the non-blocking writer's queue.
    drop(guard);

    // Rotation::NEVER writes to `<directory>/<filename>` verbatim (no
    // timestamp suffix), so we can read the file at the exact path the
    // operator configured.
    let body = std::fs::read_to_string(&target)
        .unwrap_or_else(|e| panic!("log file at {target:?} should exist: {e}"));
    assert!(
        body.contains("rotating file sink reachable"),
        "log file did not record the expected message: {body:?}"
    );
    assert!(
        body.contains("frodo-smoke"),
        "log file did not capture the structured marker field: {body:?}"
    );
}

#[test]
fn build_file_appender_creates_parent_directory_on_demand() {
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("nested/subdir/mwe-mcp.log");
    assert!(
        !target.parent().unwrap().exists(),
        "precondition: parent directory must not yet exist"
    );

    let (_writer, guard) = tracing_setup::build_file_appender(&target, LogFileRotation::Daily)
        .expect("appender must materialise the parent directory");
    drop(guard);

    assert!(
        target.parent().unwrap().exists(),
        "build_file_appender must mkdir -p the parent directory"
    );
}
