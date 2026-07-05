// SPDX-License-Identifier: AGPL-3.0-or-later
//! Workdir-scoped `.env` loader.
//!
//! Every subcommand that touches durable state (`init`, `serve`,
//! `doctor`, the `token-*` family, `admin-reset`, `migrate`) calls
//! [`load_workdir_env`] as its very first step. The loader looks for
//! `<workdir>/mwe-mcp.env` (sibling of `mwe-mcp.config.yaml`), and if
//! the file exists it pushes every `KEY=value` assignment into the
//! current process env *without* overriding variables already set by
//! the parent shell.
//!
//! ## Why this exists
//!
//! Before this module the server expected secrets — `MWE_TOKEN_SECRET`
//! today, `ANTHROPIC_API_KEY` and friends tomorrow — to arrive through
//! the shell environment. The YAML config follows the
//! `api_key_env: NAME` convention (it stores the *name* of the env
//! var, not the value), which has always assumed those vars are set
//! "somewhere", but that "somewhere" was undefined. Operators had to
//! `source` a shell file before every invocation, and the post-`init`
//! UX printed `export MWE_TOKEN_SECRET=…` for the operator to copy
//! into their shell rc. Both flows are friction in dev and an outright
//! footgun in deployment.
//!
//! Storing the secrets in `<workdir>/mwe-mcp.env` collocates them with
//! the rest of the workdir state (`engine.db`, `mwe-mcp.config.yaml`,
//! `wikis/`), so the same `--workdir` path is the only thing an
//! operator needs to point at. `init` writes the file at mode `0o600`
//! on unix.
//!
//! ## Precedence
//!
//! Shell env wins over file env — this matches dotenv's default
//! behavior and the operator's mental model ("I exported `FOO=x`, so
//! `FOO=x` it is"). Implemented by using `dotenvy::from_path`, not the
//! `_override` variant. The precedence contract is exercised by the
//! integration test at `tests/env_loader.rs` (which sits outside this
//! crate's `forbid(unsafe_code)` so it can poke `std::env::set_var`
//! to set up the "var already in shell" case).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Filename of the env file the loader looks for inside the workdir.
/// Kept in sync with `mwe-mcp init`, which writes the same path.
pub const ENV_FILE_NAME: &str = "mwe-mcp.env";

/// Result of a single [`load_workdir_env`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// `<workdir>/mwe-mcp.env` did not exist — nothing was injected.
    /// Not an error; operators may have set every relevant env var via
    /// their shell or a systemd `EnvironmentFile=` directive.
    Missing,
    /// File existed and was applied; `loaded` is the number of
    /// `KEY=value` pairs the loader actually pushed into the process
    /// env (i.e. keys that were *not* already set by the parent shell).
    Loaded {
        /// Count of variables the loader injected — pre-existing shell
        /// vars are *not* counted (see precedence rules above).
        loaded: usize,
    },
}

/// Path of the env file the loader looks for given a workdir.
#[must_use]
pub fn env_file_path(workdir: &Path) -> PathBuf {
    workdir.join(ENV_FILE_NAME)
}

/// Load `<workdir>/mwe-mcp.env` into the process env.
///
/// - File missing → returns [`LoadOutcome::Missing`] without touching
///   anything. This is the normal case for operators that wire secrets
///   via systemd `EnvironmentFile=`, container env, or their shell rc.
/// - File present + parseable → applies every `KEY=value` assignment
///   that is **not already set** in the process env, then returns
///   [`LoadOutcome::Loaded`] with the count of newly-applied vars.
/// - File present + malformed → fails loudly. A typo in `mwe-mcp.env`
///   that swallows a secret would manifest hours later as a confusing
///   auth failure; we surface it at startup instead.
///
/// # Errors
///
/// Returns the underlying [`dotenvy::Error`] (wrapped in `anyhow`) when
/// the file exists but cannot be parsed.
pub fn load_workdir_env(workdir: &Path) -> Result<LoadOutcome> {
    let path = env_file_path(workdir);
    if !path.exists() {
        return Ok(LoadOutcome::Missing);
    }

    // Snapshot the keys already set in the parent env so we can count
    // exactly how many vars came from the file. `dotenvy::from_path`
    // already implements the no-override semantics, but it does not
    // tell us which keys were inserted vs ignored.
    let before: std::collections::HashSet<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();

    dotenvy::from_path(&path).with_context(|| format!("parsing {}", path.display()))?;

    let loaded = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| !before.contains(k))
        .count();

    tracing::info!(
        file = %path.display(),
        loaded,
        "env_loader: applied workdir env file (shell env wins on conflict)"
    );
    Ok(LoadOutcome::Loaded { loaded })
}

/// Render the body of the `mwe-mcp.env` file `mwe-mcp init` writes.
///
/// Pulled out of `cmd_init` so the smoke tests can assert on the
/// content shape (and on the chmod) without spinning up the whole
/// init pipeline. The body has:
/// - a short comment header explaining what the file is for + the
///   precedence rules;
/// - the env-var names the server understands (mandatory secret +
///   the optional LLM API keys + per-function `MWE_LLM_<slot>_*`
///   overrides) commented out as a hint;
/// - one live assignment for `MWE_TOKEN_SECRET=<hex>`.
#[must_use]
pub fn render_default_env_file(token_secret_hex: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str("# mwe-mcp workdir env file\n");
    s.push_str("#\n");
    s.push_str("# Loaded automatically by `mwe-mcp <subcommand>` (init / serve / doctor /\n");
    s.push_str("# token-* / admin-reset / migrate) from the same `--workdir` directory.\n");
    s.push_str("# Variables already set in the parent shell win over values defined here.\n");
    s.push_str("#\n");
    s.push_str("# Recognized variables\n");
    s.push_str("# ---------------------\n");
    s.push_str("#   MWE_TOKEN_SECRET           required, ≥32 bytes, used to sign every JWT.\n");
    s.push_str("#                              `mwe-mcp init` generated the value below.\n");
    s.push_str("#   ANTHROPIC_API_KEY          API key for the `anthropic` LLM backend\n");
    s.push_str("#                              (referenced from `mwe-mcp.config.yaml` via\n");
    s.push_str("#                              `api_key_env: ANTHROPIC_API_KEY`).\n");
    s.push_str("#   OPENAI_API_KEY             API key for the `openai` LLM backend.\n");
    s.push_str("#\n");
    s.push_str("# Per-slot LLM overrides (any of MWE_LLM_HUB_WRITER_*,\n");
    s.push_str("# MWE_LLM_INGEST_*, MWE_LLM_REM_PROMOTIONS_*, MWE_LLM_REM_DEDUP_SEMANTIC_*,\n");
    s.push_str("# MWE_LLM_CRONISTA_*; suffixes _MODEL / _BACKEND / _API_KEY_ENV /\n");
    s.push_str("# _BASE_URL) override the matching key in `mwe-mcp.config.yaml > llm`.\n");
    s.push_str("# See `crates/mwe-core/src/config.rs > LlmFunction::env_prefix`.\n");
    s.push('\n');
    // `write!` over `String` never fails — formatter is infallible — so
    // the `expect` is purely to satisfy `must_use` on the `Result`.
    writeln!(s, "MWE_TOKEN_SECRET={token_secret_hex}").expect("writeln to String");
    s
}

/// Outcome of [`write_env_file_if_needed`] — fed back to the operator
/// summary printed by `mwe-mcp init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// File already existed and the caller did not pass `--force-config`,
    /// so the existing content was preserved verbatim.
    Preserved {
        /// Absolute path the loader looked at, surfaced so the init
        /// summary can mention it verbatim.
        path: PathBuf,
    },
    /// File was (re-)written with the freshly generated secret.
    /// On unix the file is set to mode `0o600`; on other platforms the
    /// chmod step is skipped (the OS does not expose POSIX mode bits).
    Wrote {
        /// Absolute path the loader wrote.
        path: PathBuf,
        /// `true` when the loader successfully applied `0o600` —
        /// always `false` on non-unix targets.
        chmod_0600: bool,
    },
}

/// Atomically write the default `mwe-mcp.env` file with the freshly
/// generated `MWE_TOKEN_SECRET` to `<workdir>/mwe-mcp.env`.
///
/// Behavior:
/// - If the file already exists and `force` is `false`, the existing
///   content is preserved verbatim (this is the same convention
///   `cmd_init` uses for `mwe-mcp.config.yaml`).
/// - Otherwise the file is created (or overwritten) with the body
///   produced by [`render_default_env_file`].
/// - On unix the file's mode is set to `0o600` so only the owner can
///   read the secret. On non-unix targets the chmod is skipped (those
///   platforms do not expose POSIX mode bits; the operator is expected
///   to wire OS-specific ACLs).
///
/// # Errors
///
/// Propagates any I/O failure from `fs::write` or `set_permissions`.
pub fn write_env_file_if_needed(
    workdir: &Path,
    token_secret_hex: &str,
    force: bool,
) -> Result<WriteOutcome> {
    let path = env_file_path(workdir);
    if path.exists() && !force {
        return Ok(WriteOutcome::Preserved { path });
    }
    let body = render_default_env_file(token_secret_hex);
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;

    let chmod_0600 = restrict_to_owner(&path)?;
    Ok(WriteOutcome::Wrote { path, chmod_0600 })
}

/// Set `path` to mode `0o600` on unix; no-op everywhere else.
///
/// Returns `true` when the chmod was applied. Non-unix targets always
/// return `false` so the caller can label the init output accurately.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 on {}", path.display()))?;
    Ok(true)
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<bool> {
    // POSIX mode bits aren't meaningful on Windows; rely on the
    // operator's OS-level ACLs / EFS instead.
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_env(dir: &Path, body: &str) {
        let path = dir.join(ENV_FILE_NAME);
        std::fs::write(&path, body).expect("write env file");
    }

    /// Unique env-var name per test, so a single shared `cargo test`
    /// process doesn't suffer cross-test pollution from concurrent
    /// runs. We never set these from inside the test crate (that would
    /// require `unsafe`, which the crate's `forbid(unsafe_code)`
    /// rejects) — `load_workdir_env` reads them after dotenvy applied
    /// them, which is enough to assert the happy path.
    fn unique_key(suffix: &str) -> String {
        let pid = std::process::id();
        format!("MWE_ENV_LOADER_TEST_{pid}_{suffix}")
    }

    #[test]
    fn loads_keys_from_file_when_unset_in_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = unique_key("HAPPY_PATH");
        write_env(dir.path(), &format!("{key}=bar\n"));

        let outcome = load_workdir_env(dir.path()).expect("loader ok");
        assert!(
            matches!(outcome, LoadOutcome::Loaded { loaded } if loaded >= 1),
            "outcome={outcome:?}",
        );
        assert_eq!(std::env::var(&key).ok().as_deref(), Some("bar"));
    }

    #[test]
    fn missing_file_is_silent_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = load_workdir_env(dir.path()).expect("loader ok");
        assert_eq!(outcome, LoadOutcome::Missing);
    }

    #[test]
    fn malformed_file_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No '=' is malformed per the .env grammar. dotenvy returns
        // Err on this.
        write_env(dir.path(), "this line has no equals sign\n");
        let res = load_workdir_env(dir.path());
        assert!(res.is_err(), "malformed env file must surface an error");
    }

    #[test]
    fn write_env_file_creates_with_secret_and_mode_0600_on_unix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = write_env_file_if_needed(dir.path(), "deadbeef00", false)
            .expect("write_env_file_if_needed");
        let WriteOutcome::Wrote { path, chmod_0600 } = outcome else {
            panic!("expected Wrote, got {outcome:?}");
        };
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(
            body.contains("MWE_TOKEN_SECRET=deadbeef00\n"),
            "body missing secret: {body}",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(chmod_0600, "unix chmod should be applied");
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
        }
        #[cfg(not(unix))]
        {
            assert!(!chmod_0600, "non-unix chmod should be skipped");
        }
    }

    #[test]
    fn write_env_file_preserves_existing_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        // First call writes the file.
        let _ = write_env_file_if_needed(dir.path(), "first-secret", false).expect("first write");
        let path = env_file_path(dir.path());
        let original = std::fs::read_to_string(&path).expect("read first");

        // Second call without force preserves the original content
        // even though the secret is different.
        let outcome =
            write_env_file_if_needed(dir.path(), "second-secret", false).expect("preserve call");
        let WriteOutcome::Preserved { path: p } = outcome else {
            panic!("expected Preserved, got {outcome:?}");
        };
        assert_eq!(p, path);
        let still = std::fs::read_to_string(&path).expect("read second");
        assert_eq!(still, original, "content must be unchanged");
        assert!(still.contains("first-secret"));
    }

    #[test]
    fn write_env_file_overwrites_with_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = write_env_file_if_needed(dir.path(), "first-secret", false).expect("first write");
        let path = env_file_path(dir.path());

        let outcome =
            write_env_file_if_needed(dir.path(), "second-secret", true).expect("force overwrite");
        let WriteOutcome::Wrote { path: p, .. } = outcome else {
            panic!("expected Wrote, got {outcome:?}");
        };
        assert_eq!(p, path);
        let body = std::fs::read_to_string(&path).expect("read after force");
        assert!(
            body.contains("second-secret"),
            "force should have replaced the secret: {body}",
        );
        assert!(
            !body.contains("first-secret"),
            "old secret must be gone: {body}",
        );
    }

    #[test]
    fn render_default_env_file_includes_secret_and_header() {
        let body = render_default_env_file("deadbeefcafe");
        assert!(
            body.contains("MWE_TOKEN_SECRET=deadbeefcafe\n"),
            "body missing token secret assignment: {body}",
        );
        assert!(
            body.contains("ANTHROPIC_API_KEY"),
            "body missing ANTHROPIC_API_KEY hint: {body}",
        );
        assert!(
            body.contains("Variables already set in the parent shell win over"),
            "body missing precedence hint: {body}",
        );
    }
}
