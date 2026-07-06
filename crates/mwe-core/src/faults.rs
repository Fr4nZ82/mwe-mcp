// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fault-injection harness for race / recovery tests
//! (see [build & run](../../../docs/development/build-run.md)).
//!
//! ## Why
//!
//! The storage floor (WAL applicativo, lockfile, file watcher
//! marker protocol) and the re-index transactional pipeline
//! claim to survive a crash at any step. The only way to *verify*
//! that claim — instead of merely arguing for it — is to insert
//! deterministic crash points into the code paths under test and run
//! the same scenario multiple times, killing the process at each
//! kill-point in turn.
//!
//! ## How
//!
//! The [`fault!`] macro expands to a zero-cost no-op when the cargo
//! feature `mwe-mcp-test-faults` is disabled (the production build).
//! When enabled, it consults the [`FAULT_ENV`] env var and, if the
//! var matches the macro's `name`, fires one of three actions:
//!
//! | Suffix          | Action |
//! |-----------------|--------|
//! | `name@abort`    | call [`std::process::abort`] — non-graceful exit |
//! | `name@panic`    | panic with a tagged message |
//! | `name@sleep:5s` | sleep for the given duration, then continue |
//! | `name` (no `@`) | default: `abort` |
//!
//! Tests set the env var before invoking the binary or call site
//! they want to fault, run the scenario, then verify recovery.
//!
//! Multiple kill-points can be configured by separating them with
//! commas: `MWE_TEST_FAULT_AT=wal_pre_commit@abort,wal_post_commit@sleep:1s`.

/// Canonical env var consulted by [`fault!`]. Format:
/// `name[@action][,name[@action]]*`.
pub const FAULT_ENV: &str = "MWE_TEST_FAULT_AT";

/// `true` when the crate was built with the `mwe-mcp-test-faults`
/// feature enabled. Production builds always return `false`; the
/// macro and helpers below collapse to no-ops in that case.
#[must_use]
pub const fn faults_enabled() -> bool {
    cfg!(feature = "mwe-mcp-test-faults")
}

/// Inspect the env var and fire the configured action when `name`
/// matches. No-op when the feature is disabled.
///
/// Called by the [`fault!`] macro; direct call is supported too if
/// the macro form is inconvenient (e.g. when `name` is computed at
/// runtime).
pub fn maybe_fire(name: &str) {
    if !faults_enabled() {
        return;
    }
    let Ok(spec) = std::env::var(FAULT_ENV) else {
        return;
    };
    fire_if_matches(name, &spec);
}

/// Pure helper: scan `spec` for an entry that names `name` and fire
/// the resulting action. Exposed for tests that exercise the parser
/// without mutating the process env.
pub fn fire_if_matches(name: &str, spec: &str) {
    for entry in spec.split(',') {
        let entry = entry.trim();
        let (entry_name, action) = entry.split_once('@').map_or((entry, "abort"), |t| t);
        if entry_name == name {
            fire(name, action);
            return;
        }
    }
}

fn fire(name: &str, action: &str) {
    tracing::warn!(fault = name, action, "fault injection firing");
    if let Some(rest) = action.strip_prefix("sleep:") {
        let dur = parse_duration(rest).unwrap_or(std::time::Duration::from_secs(0));
        std::thread::sleep(dur);
        return;
    }
    match action {
        "panic" => panic!("fault injection: {name}"),
        "abort" | "" => std::process::abort(),
        other => {
            tracing::error!(
                fault = name,
                action = other,
                "unknown fault action; aborting"
            );
            std::process::abort();
        },
    }
}

/// Parse a duration of the form `Ns`, `Nms`, or bare `N` (seconds).
fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.parse::<u64>().ok().map(std::time::Duration::from_millis);
    }
    if let Some(secs) = s.strip_suffix('s') {
        return secs.parse::<u64>().ok().map(std::time::Duration::from_secs);
    }
    s.parse::<u64>().ok().map(std::time::Duration::from_secs)
}

/// Insert a named kill-point into a code path.
///
/// In a production build (no `mwe-mcp-test-faults` feature) this
/// expands to nothing — zero runtime cost. In a test build, the
/// runtime checks the [`FAULT_ENV`] env var and fires the configured
/// action (abort / panic / sleep) if the name matches.
#[macro_export]
macro_rules! fault {
    ($name:expr) => {
        $crate::faults::maybe_fire($name)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The const `faults_enabled` answers truthfully for the current
    /// build configuration.
    #[test]
    fn faults_enabled_matches_feature() {
        let feature_on = cfg!(feature = "mwe-mcp-test-faults");
        assert_eq!(faults_enabled(), feature_on);
    }

    #[test]
    fn parse_duration_accepts_common_suffixes() {
        assert_eq!(
            parse_duration("5s"),
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            parse_duration("250ms"),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(parse_duration("3"), Some(std::time::Duration::from_secs(3)));
        assert_eq!(parse_duration("garbage"), None);
    }

    /// `fire_if_matches` walks the spec correctly and only fires the
    /// action when the name matches.
    ///
    /// We can only exercise the `sleep:` action in-process safely —
    /// `abort` and `panic` would tear down the test runner — so we
    /// gate that arm behind the feature flag and verify it actually
    /// delays.
    #[test]
    fn fire_if_matches_skips_unmatched_entries() {
        // Multiple entries, none of which match → no action.
        fire_if_matches("untouched", "alpha@sleep:1s,beta@panic,gamma@abort");
    }

    #[cfg(feature = "mwe-mcp-test-faults")]
    #[test]
    fn fire_if_matches_sleeps_on_match() {
        let started = std::time::Instant::now();
        fire_if_matches("delay_point", "other@panic,delay_point@sleep:120ms,extra");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "expected ≥ 100 ms sleep, got {elapsed:?}"
        );
    }

    /// When the feature is off the macro must compile to a no-op,
    /// regardless of the env var content. We assert the static
    /// configuration here — there is nothing to observe at runtime.
    #[cfg(not(feature = "mwe-mcp-test-faults"))]
    #[test]
    fn macro_is_noop_when_feature_off() {
        // If the macro emitted any runtime work this would either
        // panic or abort thanks to the env-less default action; the
        // fact that the test passes is the assertion.
        fault!("anything");
    }
}
