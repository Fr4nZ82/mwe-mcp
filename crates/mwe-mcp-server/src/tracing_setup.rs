// SPDX-License-Identifier: AGPL-3.0-or-later
//! `tracing` subscriber wiring for the `mwe-mcp` binary.
//!
//! Two sinks are installed:
//!
//! - `stderr` (always present).
//! - A rotating file sink under
//!   `<workdir>/<logging.file_path>` (default `logs/mwe-mcp.log`) when
//!   `logging.file_rotation` is not `disabled`. Running `mwe-mcp`
//!   detached (systemd, container, agent process) needs a `tail`-able
//!   file the operator can attach to after the fact.
//!
//! Both sinks see the same picture: the same filter and the same
//! `logging.format`. An operator who asked for JSON asked for it
//! wherever the logs come out — on a systemd host that is the journal,
//! which is this process's stderr, as much as it is the file.
//!
//! The file sink is enabled by default. Operators on read-only mounts or
//! with external log shipping wired in can flip `logging.file_rotation`
//! to `disabled` in `mwe-mcp.config.yaml` and keep stderr as the only
//! sink.
//!
//! Live in a dedicated module (not in `main.rs`) so the integration
//! tests under `tests/` can exercise the file sink end-to-end through
//! the [`build_file_appender`] helper.

use std::path::{Path, PathBuf};

use mwe_core::config::{Config, LogFileRotation, LogFormat};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Output of [`install`]: the file-appender guard the caller must hold
/// for the remainder of the process so log writes are flushed on exit.
///
/// `None` when the file sink is disabled (`logging.file_rotation:
/// disabled`) or when opening the file sink failed and we degraded to
/// stderr-only — the safe fallback.
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
    // Filter precedence: RUST_LOG > config > info
    // default. Rebuilt twice because `EnvFilter` is not `Clone`; the
    // intent is that both sinks see the same picture.
    let format = config.logging.format;
    let stderr_layer = formatted_layer(
        format,
        std::io::stderr,
        // Colour is left to the library's own rule, which honours
        // `NO_COLOR`: an operator who set that variable set it for this
        // process too, and forcing the answer here would be one more
        // place their choice quietly does not reach.
        None,
        make_filter(config),
    );

    let (file_layer, guard) = config.logging.resolved_file_path(workdir).map_or_else(
        || (None, None),
        |target| match build_file_appender(&target, config.logging.file_rotation) {
            Ok((writer, guard)) => {
                let layer = formatted_layer(
                    format,
                    writer,
                    // ANSI escapes belong on a terminal, not in a log
                    // file — strip them so `grep` / `less` produce
                    // clean output.
                    Some(false),
                    make_filter(config),
                );
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

/// One sink's layer, in the operator's chosen shape.
///
/// Boxed because the two shapes are two types: `.json()` swaps the
/// formatter, so the text and JSON arms cannot meet at a concrete type
/// and the erasure is what lets one function serve both sinks — which is
/// the point, since a second copy of this decision is how the two sinks
/// would come to disagree.
fn formatted_layer<S, W>(
    format: LogFormat,
    writer: W,
    ansi: Option<bool>,
    filter: tracing_subscriber::EnvFilter,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => {
            let layer = tracing_subscriber::fmt::layer();
            // `None` is not "off": it leaves the library's own rule in
            // place, which is the feature flag **and** `NO_COLOR`. A
            // caller that wants colour gone says so.
            let layer = match ansi {
                Some(ansi) => layer.with_ansi(ansi),
                None => layer,
            };
            layer.with_writer(writer).with_filter(filter).boxed()
        },
        // The JSON formatter writes no escapes of its own, so `ansi` has
        // nothing to say here.
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(filter)
            .boxed(),
    }
}

fn make_filter(config: &Config) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(config.logging.level.as_env_filter())
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing::info;

    use super::*;

    /// A sink the test can read back. `MakeWriter` hands out a clone per
    /// event, so the buffer is shared rather than owned by one writer.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Two events through a scoped subscriber built the way [`install`]
    /// builds each of its sinks, returned as the bytes that sink saw.
    fn captured(format: LogFormat) -> String {
        captured_with(format, Some(false))
    }

    /// As [`captured`], with the caller choosing what to say about
    /// colour — the difference between the two sinks.
    fn captured_with(format: LogFormat, ansi: Option<bool>) -> String {
        let buffer = Buffer::default();
        let layer = formatted_layer(
            format,
            buffer.clone(),
            ansi,
            tracing_subscriber::EnvFilter::new("info"),
        );
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            info!(target: "mwe_mcp_format_test", consumer = "frodo", "first event");
            info!(target: "mwe_mcp_format_test", consumer = "alice", "second event");
        });
        let mut sink = buffer.clone();
        sink.flush().expect("flush");
        let bytes = buffer.0.lock().expect("buffer").clone();
        String::from_utf8(bytes).expect("utf-8")
    }

    /// What a log shipper needs: one whole JSON object per line, and the
    /// event's structured fields carried as fields rather than baked
    /// into a sentence somebody would have to write a regex against.
    #[test]
    fn the_json_format_writes_one_object_per_line() {
        let out = captured(LogFormat::Json);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one line per event: {out:?}");
        for line in &lines {
            let value: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} is not JSON: {e}"));
            assert!(value.get("timestamp").is_some(), "{line}");
            assert_eq!(value["level"], "INFO", "{line}");
            assert_eq!(value["target"], "mwe_mcp_format_test", "{line}");
            assert!(value["fields"]["message"].is_string(), "{line}");
        }
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("JSON");
        assert_eq!(first["fields"]["consumer"], "frodo");
        assert_eq!(first["fields"]["message"], "first event");
    }

    /// `text` is the default, and it stays the prose an operator reads
    /// while it scrolls past — a default that had quietly become JSON
    /// would be the change nobody asked for.
    #[test]
    fn the_text_format_stays_prose() {
        let out = captured(LogFormat::Text);

        let first = out.lines().next().expect("an event was written");
        assert!(first.contains("first event"), "{out}");
        assert!(first.contains("consumer=\"frodo\""), "{out}");
        assert!(
            serde_json::from_str::<serde_json::Value>(first).is_err(),
            "the text format must not be JSON: {first}"
        );
    }

    /// The default carries no surprise: an installation that says
    /// nothing about the format gets the readable one.
    #[test]
    fn the_default_format_is_text() {
        assert_eq!(Config::default().logging.format, LogFormat::Text);
    }

    /// The file sink is read with `grep` and `less`, so it says colour
    /// off rather than leaving it to a rule about terminals — the file
    /// is not one.
    #[test]
    fn the_file_sink_carries_no_escape_codes() {
        let out = captured_with(LogFormat::Text, Some(false));

        assert!(out.contains("first event"), "{out:?}");
        assert!(
            !out.contains('\x1b'),
            "an escape code reached the file: {out:?}"
        );
    }
}
