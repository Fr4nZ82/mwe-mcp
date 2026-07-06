// SPDX-License-Identifier: AGPL-3.0-or-later
//! `tracing` subscriber wiring for the `mwe-mcp` binary.
//!
//! Two sinks are installed:
//!
//! - `stderr` (the only sink under decision O.1) — always present.
//! - A rotating file sink under
//!   `<workdir>/<logging.file_path>` (default `logs/mwe-mcp.log`) when
//!   `logging.file_rotation` is not `disabled`. The rationale is
//!   documented in logging:
//!   running `mwe-mcp` detached (systemd, container, agent process)
//!   needs a `tail`-able file the operator can attach to after the
//!   fact.
//!
//! The file sink is enabled by default — see the rationale in
//! logging. Operators on
//! read-only mounts or with external log shipping wired in can flip
//! `logging.file_rotation` to `disabled` in `mwe-mcp.config.yaml` to
//! recover the original stderr-only floor.
//!
//! Live in a dedicated module (not in `main.rs`) so the integration
//! tests under `tests/` can exercise the file sink end-to-end through
//! the [`build_file_appender`] helper.

use std::path::{Path, PathBuf};

use mwe_core::config::{Config, LogFileRotation};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Output of [`install`]: the file-appender guard the caller must hold
/// for the remainder of the process so log writes are flushed on exit.
///
/// `None` when the file sink is disabled (`logging.file_rotation:
/// disabled`) or when opening the file sink failed and we degraded to
/// stderr-only — the documented safe fallback per decision O.1.
pub type Guard = Option<WorkerGuard>;

/// Install the global `tracing` subscriber stack on the current
/// process.
///
/// Returns a [`Guard`] the caller must keep alive (dropping it
/// flushes pending writes from the non-blocking file appender).
///
/// Failures while opening the file sink are reported on stderr and
/// the file sink is silently dropped; stderr-only operation remains
/// installed so the program is never log-blind.
///
/// # Panics
///
/// Panics if a global subscriber is already installed in the current
/// process. The binary calls this exactly once from `main`; tests
/// that need to exercise the file sink should use
/// [`build_file_appender`] directly with a scoped subscriber (see the
/// integration test
/// `crates/mwe-mcp-server/tests/file_logging.rs`).
pub fn install(workdir: &Path, config: &Config) -> Guard {
    // Filter precedence (decision O.1): RUST_LOG > config > info
    // default. Rebuilt twice because `EnvFilter` is not `Clone`; the
    // intent is that both sinks see the same picture.
    let stderr_filter = make_filter(config);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter)
        .boxed();

    let (file_layer, guard) = config.logging.resolved_file_path(workdir).map_or_else(
        || (None, None),
        |target| match build_file_appender(&target, config.logging.file_rotation) {
            Ok((writer, guard)) => {
                let file_filter = make_filter(config);
                let layer = tracing_subscriber::fmt::layer()
                    // ANSI escapes belong on a terminal, not in a log
                    // file — strip them so `grep` / `less` produce
                    // clean output.
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(file_filter)
                    .boxed();
                (Some(layer), Some(guard))
            },
            Err(e) => {
                eprintln!(
                    "mwe-mcp: failed to open log file {target}: {e} — falling back to stderr only",
                    target = target.display()
                );
                (None, None)
            },
        },
    );

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();
    guard
}

/// Build the non-blocking writer + worker guard for the rotating file
/// appender at `target` using the chosen `rotation`.
///
/// Exposed (not just an inner helper of [`install`]) so the
/// integration tests under `tests/` can drive the file sink against a
/// scoped subscriber without colliding with the process-global one
/// `install` would set.
///
/// Creates the parent directory on demand. Returns the canonical
/// `(NonBlocking writer, WorkerGuard)` pair `tracing-appender` exports.
///
/// # Errors
///
/// - [`std::io::ErrorKind::InvalidInput`] when `target` has no
///   filename component.
/// - Any `std::fs::create_dir_all` failure when the parent directory
///   cannot be materialised.
pub fn build_file_appender(
    target: &Path,
    rotation: LogFileRotation,
) -> std::io::Result<(NonBlocking, WorkerGuard)> {
    let directory = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let filename = target
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "logging.file_path {target} has no filename component",
                    target = target.display()
                ),
            )
        })?
        .to_owned();
    std::fs::create_dir_all(&directory)?;
    let rotation = match rotation {
        LogFileRotation::Daily => Rotation::DAILY,
        LogFileRotation::Hourly => Rotation::HOURLY,
        // `Never` and `Disabled` both fall through to `Rotation::NEVER`
        // here; `Disabled` is filtered out one level up so we never
        // actually reach this branch with it, but the exhaustive match
        // keeps the next reader from chasing a "what about Disabled?"
        // mystery.
        LogFileRotation::Never | LogFileRotation::Disabled => Rotation::NEVER,
    };
    let appender = RollingFileAppender::new(rotation, directory, filename);
    Ok(tracing_appender::non_blocking(appender))
}

fn make_filter(config: &Config) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(config.logging.level.as_env_filter())
    })
}
