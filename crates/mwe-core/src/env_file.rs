// SPDX-License-Identifier: AGPL-3.0-or-later
//! Mutating helper for `<workdir>/mwe-mcp.env`.
//!
//! The dashboard admin LLM-config page writes API key values
//! into the workdir env file when the operator sets a fresh key from
//! the UI. The file is sensitive — `mwe-mcp init` creates it at chmod
//! `0o600` on unix — and it can also contain commented hint lines, a
//! mix of operator hand-edits, and existing assignments the dashboard
//! must not flatten.
//!
//! [`write_key`] therefore takes a parse-modify-write approach:
//!
//! 1. If the file is missing, materialise a minimal body with a one-
//!    line comment header + the new `KEY="value"` assignment.
//! 2. If it exists, scan line-by-line. The first line that is an
//!    assignment of `KEY` (case-sensitive, leading whitespace
//!    tolerated, commented lines `#…` skipped) is replaced verbatim.
//!    No assignment found → append `KEY="value"` at the end (after a
//!    trailing newline if the body lacks one). Every other line
//!    survives byte-for-byte, so the comment block written by
//!    `mwe-mcp init` and any subsequent hand-edit are preserved.
//! 3. The new body is committed via [`crate::wiki::atomic_write`]
//!    (temp + fsync + rename + dir-fsync), so a crash mid-write
//!    cannot leave a half-formed env file behind.
//! 4. On unix, the resulting file's mode is forced to `0o600`
//!    regardless of the previous file's mode — losing the lockdown
//!    on a save would be a footgun. On non-unix the call is a no-op.
//!
//! ## Value encoding
//!
//! The value is always emitted quoted as `KEY="..."` with a small
//! escape pass on `\` and `"` (`\\` and `\"` respectively).
//! `dotenvy` parses this back verbatim. Always-quoting trades two
//! bytes of overhead for the guarantee that any future value
//! containing whitespace, `#`, or `=` cannot accidentally become an
//! unquoted assignment with surprising parse semantics.
//!
//! ## What this module does NOT do
//!
//! - It does not validate the key name. The dashboard layer is the
//!   one that constrains keys to the `[A-Z_][A-Z0-9_]*` env-var
//!   alphabet; this helper just writes whatever it is given.
//! - It does not refresh the running process's view of the env. The
//!   shell-wins-over-file precedence of [`crate::config::Config`]
//!   means a key changed here is read by the *next* `mwe-mcp serve`
//!   invocation, not the current one. A future change will close
//!   the hot-reload gap when it ships.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::wiki::{WikiError, atomic_write};

/// Errors raised by [`write_key`]. Thin wrapper over filesystem and
/// atomic-write failures so the caller can match on the variant
/// without depending on [`WikiError`] directly.
#[derive(Debug, thiserror::Error)]
pub enum EnvFileError {
    /// Filesystem read failed (existing file present but unreadable).
    #[error("env file io: {0}")]
    Io(#[from] std::io::Error),
    /// The atomic-write step failed.
    #[error("env file atomic_write: {0}")]
    AtomicWrite(#[from] WikiError),
}

/// One-line comment header used when [`write_key`] has to create the
/// file from scratch. Mirrors the longer header that
/// `mwe-mcp init`'s `render_default_env_file` emits — short version so
/// a dashboard-created file is still self-describing without
/// duplicating the full recognized-vars table.
const HEADER_WHEN_CREATING: &str = "# mwe-mcp workdir env file (created by /dashboard/admin/llm-config)\n\
     # Shell env wins over values defined here. See `mwe-mcp init` output for\n\
     # the full list of recognized variables.\n\n";

/// Upsert `KEY="value"` into the env file at `path`.
///
/// File is created at mode `0o600` on unix if absent. Existing
/// content is preserved verbatim except for the (first) assignment of
/// `key`, which is rewritten with the new value. See module docs for
/// the full contract.
///
/// # Errors
///
/// Returns [`EnvFileError::Io`] on filesystem read failures (other
/// than `NotFound`, which triggers the create path), and
/// [`EnvFileError::AtomicWrite`] when the atomic rename pipeline
/// fails.
pub fn write_key(path: &Path, key: &str, value: &str) -> Result<(), EnvFileError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(EnvFileError::Io(e)),
    };

    let new_body = existing.map_or_else(
        || format!("{HEADER_WHEN_CREATING}{}\n", format_assignment(key, value)),
        |prior| upsert_in_body(&prior, key, value),
    );

    atomic_write(path, new_body.as_bytes())?;
    apply_secret_mode(path)?;
    Ok(())
}

/// Format a single `KEY="value"` line with `\` and `"` escaped.
fn format_assignment(key: &str, value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            other => escaped.push(other),
        }
    }
    format!("{key}=\"{escaped}\"")
}

/// Rewrite the first assignment of `key` in `body`; append at the end
/// if none is found. Comments (`#…`) and other assignments survive
/// verbatim, including trailing whitespace, so an operator's hand-
/// formatted env file is not flattened on save.
fn upsert_in_body(body: &str, key: &str, value: &str) -> String {
    let new_line = format_assignment(key, value);
    let mut out = String::with_capacity(body.len() + new_line.len());
    let mut replaced = false;
    for line in body.split_inclusive('\n') {
        // `split_inclusive` keeps the trailing `\n` on each line, so
        // every survived line round-trips byte-for-byte.
        if !replaced && is_assignment_of(line, key) {
            out.push_str(&new_line);
            // Preserve whatever trailing newline the original line
            // carried (could be `\n`, `\r\n`, or nothing for an
            // unterminated last line).
            let trailing = trailing_newline(line);
            out.push_str(trailing);
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        // Ensure a newline separates the existing body from the
        // appended assignment, then write the new line + newline.
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&new_line);
        out.push('\n');
    }
    out
}

/// True when `line` (which may include a trailing newline) is the
/// assignment of `key`. Leading whitespace tolerated; comments and
/// arbitrary other content rejected.
fn is_assignment_of(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return false;
    }
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

/// Slice the trailing newline characters off `line` so the
/// replacement can preserve them verbatim.
fn trailing_newline(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// Force `path` to mode `0o600` on unix. No-op on other platforms —
/// Windows ACLs are not modelled here, and the dashboard documents
/// the unix-only chmod behavior next to the API key editor.
#[cfg(unix)]
fn apply_secret_mode(path: &Path) -> Result<(), EnvFileError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
const fn apply_secret_mode(_path: &Path) -> Result<(), EnvFileError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_new_file_with_header_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        write_key(&path, "ANTHROPIC_API_KEY", "sk-ant-fake").expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("dashboard"), "header missing: {body}");
        assert!(
            body.contains("ANTHROPIC_API_KEY=\"sk-ant-fake\""),
            "assignment missing: {body}"
        );
    }

    #[test]
    fn updates_existing_assignment_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        std::fs::write(
            &path,
            "# header\n\
             MWE_TOKEN_SECRET=deadbeef\n\
             ANTHROPIC_API_KEY=old-value\n\
             OPENAI_API_KEY=other\n",
        )
        .expect("seed");
        write_key(&path, "ANTHROPIC_API_KEY", "new-value").expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        // Old assignment replaced.
        assert!(!body.contains("old-value"), "{body}");
        assert!(body.contains("ANTHROPIC_API_KEY=\"new-value\""), "{body}");
        // Surrounding lines preserved verbatim.
        assert!(body.contains("MWE_TOKEN_SECRET=deadbeef"), "{body}");
        assert!(body.contains("OPENAI_API_KEY=other"), "{body}");
        assert!(body.starts_with("# header\n"), "{body}");
    }

    #[test]
    fn appends_when_key_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        std::fs::write(&path, "# header\nMWE_TOKEN_SECRET=deadbeef\n").expect("seed");
        write_key(&path, "ANTHROPIC_API_KEY", "sk-ant-fake").expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.starts_with("# header\nMWE_TOKEN_SECRET=deadbeef\n"),
            "{body}"
        );
        assert!(body.contains("ANTHROPIC_API_KEY=\"sk-ant-fake\""), "{body}");
    }

    #[test]
    fn skips_commented_assignment_of_same_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        // `mwe-mcp init` emits the recognized vars as commented hints
        // — a `# ANTHROPIC_API_KEY API key for...` line must NOT be
        // mistaken for an assignment and overwritten in place.
        std::fs::write(
            &path,
            "# ANTHROPIC_API_KEY    API key for the anthropic backend\n\
             MWE_TOKEN_SECRET=deadbeef\n",
        )
        .expect("seed");
        write_key(&path, "ANTHROPIC_API_KEY", "sk-ant-fake").expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        // Comment hint untouched.
        assert!(
            body.contains("# ANTHROPIC_API_KEY    API key for the anthropic backend\n"),
            "{body}"
        );
        // New assignment appended at end (not inline-overwriting the comment).
        assert!(body.contains("ANTHROPIC_API_KEY=\"sk-ant-fake\""), "{body}");
    }

    #[test]
    fn escapes_backslash_and_double_quote_in_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        write_key(&path, "WEIRD_KEY", "has \"quote\" and \\back").expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains("WEIRD_KEY=\"has \\\"quote\\\" and \\\\back\""),
            "{body}"
        );
    }

    // dotenvy is the parser the mwe-mcp-server crate uses for its
    // `env_loader::load_workdir_env` call; its round-trip with the
    // values [`write_key`] produces is covered by the integration
    // test on the dashboard side (which sets up a real binary).
    // We deliberately do not add `dotenvy` as a dev-dep here just to
    // re-test the parse — the format itself is exercised above.

    #[cfg(unix)]
    #[test]
    fn forces_chmod_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        // Seed with a world-readable file to prove the helper clamps
        // the mode back down.
        std::fs::write(&path, "MWE_TOKEN_SECRET=x\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_key(&path, "ANTHROPIC_API_KEY", "sk-ant-fake").expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600 after write, got {mode:o}");
    }
}
