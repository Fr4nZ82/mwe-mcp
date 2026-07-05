// SPDX-License-Identifier: AGPL-3.0-or-later
//! Hybrid runtime loader for the operational prompts.
//!
//! Each prompt file is a markdown document with a single fenced
//! `text` block (opener `` ```text ``, closer `` ``` ``) that
//! contains the verbatim prompt body. The loader returns that body.
//! The lookup order is:
//!
//! 1. `<workdir>/prompts/<name>.md` — operator override, edits land
//!    here at deploy time (no rebuild needed).
//! 2. `bundled_fallback` — the string the caller embedded at compile
//!    time via `include_str!("../prompts/<name>.md")`. The caller
//!    passes it explicitly so this module stays unaware of which
//!    prompts exist where (mwe-core ships `ingest`; mwe-dashboard
//!    ships `agentic-chat-panel`).
//!
//! ## Why a fenced-block layout and not a raw `.md` body
//!
//! The same file format is mirrored under the bundled prompt sources
//! at `crates/<crate>/prompts/<name>.md`. Keeping the on-disk shape
//! identical lets the [engineering wiki](../../../wiki/design-notes/llm-functions.md),
//! the operator's edit experience, and the loader share one parse —
//! and lets contributors look at either copy and see the same thing.
//!
//! ## Hot-reload
//!
//! Not implemented yet. The loader reads the file on every call;
//! a future milestone may cache per `(name, workdir)` once the
//! dashboard editor lands. For now the per-call cost is a single
//! `fs::read_to_string` of ~10 KB plus a `str::find` — negligible
//! next to any LLM call.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::wiki;

/// Failure modes from the prompt loader.
#[derive(Debug, Error)]
pub enum PromptError {
    /// The markdown document did not contain a `text` fenced block,
    /// or the block was malformed (no closing fence).
    #[error(
        "prompt `{name}` (from {origin}): no ```text fenced block found in the markdown document"
    )]
    NoFencedBlock {
        /// Logical name of the prompt (e.g. `"ingest"`).
        name: String,
        /// Where the document came from — either `<workdir>/prompts/<name>.md`
        /// or `<bundled>`. Helps an operator diagnose a hand-edit that
        /// dropped the fence.
        origin: String,
    },
    /// IO error reading `<workdir>/prompts/<name>.md` (the file is
    /// present — i.e. `exists()` returned `true` — but the read
    /// itself failed: permissions, mid-flight delete, etc.).
    #[error("prompt `{name}` at {path}: io error: {detail}")]
    Io {
        /// Logical name of the prompt (for diagnostics).
        name: String,
        /// Absolute path that produced the io error.
        path: PathBuf,
        /// `io::Error` rendered as a string.
        detail: String,
    },
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, PromptError>;

const FENCE_OPENER: &str = "```text";
const FENCE_CLOSER: &str = "```";

/// Extract the verbatim prompt body from a markdown document.
///
/// Looks for the first fenced `text` block (opener `` ```text ``,
/// closer `` ``` ``) in the document; returns the bytes between the
/// opening fence's newline and the closing fence. Surrounding `\n`
/// characters adjacent to the fences are trimmed so the returned
/// string starts with the first character of the prompt and ends
/// with the last (no leading / trailing blank lines).
///
/// The `name` argument is only used to enrich the error message.
///
/// # Errors
///
/// Returns [`PromptError::NoFencedBlock`] when the document is
/// missing the opening fence, missing the closing fence, or has the
/// closing fence appearing before any newline after the opener.
pub fn extract_fenced_text(md: &str, name: &str, source: &str) -> Result<String> {
    let opener_at = md
        .find(FENCE_OPENER)
        .ok_or_else(|| PromptError::NoFencedBlock {
            name: name.to_owned(),
            origin: source.to_owned(),
        })?;
    // Advance past the opener plus exactly one newline (the line
    // break that ends the ```text marker line). If the opener is not
    // followed by a newline, the document is malformed.
    let mut body_start = opener_at + FENCE_OPENER.len();
    if !md[body_start..].starts_with('\n') {
        return Err(PromptError::NoFencedBlock {
            name: name.to_owned(),
            origin: source.to_owned(),
        });
    }
    body_start += 1;
    let rel_end =
        md[body_start..]
            .find(FENCE_CLOSER)
            .ok_or_else(|| PromptError::NoFencedBlock {
                name: name.to_owned(),
                origin: source.to_owned(),
            })?;
    let mut body_end = body_start + rel_end;
    // Trim a single trailing newline so the body ends at the last
    // visible character of the prompt rather than an empty line just
    // before the closing fence.
    if body_end > body_start && md.as_bytes()[body_end - 1] == b'\n' {
        body_end -= 1;
    }
    Ok(md[body_start..body_end].to_owned())
}

/// Load an operational prompt by `name`.
///
/// Returns the override at `<workdir>/prompts/<name>.md` when
/// present, otherwise parses `bundled_fallback` (the string the
/// caller embedded via `include_str!`). Both branches go through
/// [`extract_fenced_text`].
///
/// # Errors
///
/// - [`PromptError::Io`] when the workdir file exists but cannot be
///   read.
/// - [`PromptError::NoFencedBlock`] when either the workdir file or
///   the bundled fallback is malformed (no `text` fence).
pub fn load(name: &str, workdir: &Path, bundled_fallback: &str) -> Result<String> {
    let path = workdir.join("prompts").join(format!("{name}.md"));
    if path.exists() {
        let md = fs::read_to_string(&path).map_err(|e| PromptError::Io {
            name: name.to_owned(),
            path: path.clone(),
            detail: e.to_string(),
        })?;
        return extract_fenced_text(&md, name, &path.display().to_string());
    }
    extract_fenced_text(bundled_fallback, name, "<bundled>")
}

/// Substitute `{key}` placeholders in `body` from `vars`.
///
/// A placeholder is the literal three-token sequence `{`, identifier
/// (`[a-zA-Z_][a-zA-Z0-9_]*`), `}`. Every match whose identifier
/// appears in `vars` is replaced by the corresponding value; matches
/// whose identifier is *not* in `vars` are left in place verbatim.
/// Anything that is not a syntactically valid placeholder (e.g. JSON
/// literals such as `{"same": true}`, since `"` is not an identifier
/// char) is left untouched and incurs no scanning cost — the scanner
/// rejects it on the first character past the opening brace.
///
/// The substitution is single-pass: a value that itself contains
/// `{key}` is **not** re-scanned. This keeps the helper safe to call
/// with arbitrary runtime data (e.g. user-typed text in `{body}`) —
/// curly braces inside the value cannot trigger a second round of
/// substitution.
///
/// Duplicate keys in `vars` are resolved by the first occurrence
/// (later duplicates are ignored). Callers building `vars` from a
/// fixed list of placeholders need not deduplicate.
#[must_use]
pub fn substitute(body: &str, vars: &[(&str, &str)]) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            // Fast path: copy a run of non-`{` chars in one go.
            let start = i;
            while i < bytes.len() && bytes[i] != b'{' {
                i += 1;
            }
            out.push_str(&body[start..i]);
            continue;
        }
        // bytes[i] == b'{' — try to parse an identifier followed by '}'.
        let ident_start = i + 1;
        let mut j = ident_start;
        if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
            j += 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'}' && j > ident_start {
                let key = &body[ident_start..j];
                if let Some((_, value)) = vars.iter().find(|(k, _)| *k == key) {
                    out.push_str(value);
                    i = j + 1;
                    continue;
                }
            }
        }
        // Not a known placeholder — emit the `{` literally and move on.
        out.push('{');
        i += 1;
    }
    out
}

/// Filesystem subdirectory of the workdir where operator-overridable
/// prompt files live (`<workdir>/prompts/<name>.md`).
const WORKDIR_PROMPTS_SUBDIR: &str = "prompts";

/// Compile-time-embedded bundled prompts shipped by `mwe-core`.
///
/// Each tuple is `(name, body)` where `name` is the logical prompt
/// identifier (the stem of the materialised `<workdir>/prompts/<name>.md`
/// file) and `body` is the full markdown document (frontmatter +
/// fenced `text` block) included via `include_str!`. The bundled
/// document already carries `default_version_at_bootstrap: vN.M` in
/// its frontmatter — that is the version-of-record an operator gets
/// when `mwe-mcp init` materialises the workdir.
///
/// `mwe-dashboard` ships its own [`mwe_dashboard::BUNDLED_PROMPTS`]
/// slice (the agentic chat panel system prompt); the binary chains
/// the two and passes the result to [`seed_bundled_into`] so the
/// loader module stays unaware of which prompts live where.
pub const BUNDLED: &[(&str, &str)] = &[
    ("ingest", crate::ingest::BUNDLED_INGEST_PROMPT_MD),
    ("ingest-closures", crate::ingest::BUNDLED_INGEST_CLOSURES_MD),
    ("rem-dedup", crate::rem::BUNDLED_REM_DEDUP_MD),
    ("rem-promotions", crate::rem::BUNDLED_REM_PROMOTIONS_MD),
    ("rem-completion", crate::rem::BUNDLED_REM_COMPLETION_MD),
    ("rem-refile", crate::rem::BUNDLED_REM_REFILE_MD),
    (
        "rem-contradiction",
        crate::rem::BUNDLED_REM_CONTRADICTION_MD,
    ),
    (
        "rem-recall-repair",
        crate::rem::BUNDLED_REM_RECALL_REPAIR_MD,
    ),
    ("rem-dates", crate::rem::BUNDLED_REM_DATES_MD),
    ("rem-merge", crate::rem::BUNDLED_REM_MERGE_MD),
    (
        "rem-subwiki-emergence",
        crate::rem::BUNDLED_REM_SUBWIKI_EMERGENCE_MD,
    ),
    ("regenerate-index", crate::rem::BUNDLED_REGENERATE_INDEX_MD),
    (
        "document-classify",
        crate::document::BUNDLED_DOCUMENT_CLASSIFY_MD,
    ),
    (
        "document-extract",
        crate::document::BUNDLED_DOCUMENT_EXTRACT_MD,
    ),
    ("document-merge", crate::document::BUNDLED_DOCUMENT_MERGE_MD),
    ("cartografo", crate::planner::BUNDLED_CARTOGRAFO_MD),
    ("conciliatore", crate::planner::BUNDLED_CONCILIATORE_MD),
    ("cronista", crate::compiler::BUNDLED_CRONISTA_MD),
    (
        "comment-apply",
        crate::comment_apply::BUNDLED_COMMENT_APPLY_MD,
    ),
    ("navigator", crate::recall_nav::BUNDLED_NAVIGATOR_PROMPT_MD),
    (
        "query-seeds",
        crate::recall_nav::BUNDLED_QUERY_SEEDS_PROMPT_MD,
    ),
];

/// Outcome of one [`seed_bundled_into`] call.
///
/// A freshly
/// initialised workdir reports `created` = N and `preserved` = 0;
/// rerunning on the same workdir flips to `created` = 0 and
/// `preserved` = N. Operator hand-edits at `<workdir>/prompts/<name>.md`
/// are never overwritten — drift management is deferred to a separate
/// milestone, which will read `default_version_at_bootstrap`
/// from the existing file and compare against the bundled body.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeedReport {
    /// Number of prompt files written for the first time.
    pub created: usize,
    /// Number of prompt files that already existed at
    /// `<workdir>/prompts/<name>.md` and were left untouched.
    pub preserved: usize,
}

impl SeedReport {
    /// Merge two reports — useful when the binary seeds the
    /// `mwe-core` bundled slice and the `mwe-dashboard` bundled
    /// slice in separate calls but wants one summary line.
    #[must_use]
    pub const fn merged(&self, other: &Self) -> Self {
        Self {
            created: self.created + other.created,
            preserved: self.preserved + other.preserved,
        }
    }
}

/// Materialise the bundled prompts under `<workdir>/prompts/<name>.md`.
///
/// Idempotent: a file that already exists at the target path is left
/// untouched and counted as `preserved`, so an operator who has
/// edited their prompts never loses changes when `mwe-mcp init` (or
/// `mwe-mcp migrate`) runs again. New prompts added in a binary
/// upgrade flow into the existing workdir without disturbing the
/// pre-existing ones. Each `(name, body)` pair is written via
/// [`crate::wiki::atomic_write`] so a crash mid-write cannot leave a
/// half-formed file at the target path.
///
/// The `bundled` slice is what the caller wants seeded; per the
/// loader's "this module stays unaware of which prompts exist where"
/// invariant, both [`BUNDLED`] and the dashboard's equivalent are
/// passed in by the caller (typically the binary).
///
/// # Errors
///
/// [`PromptError::Io`] if creating `<workdir>/prompts/` or writing
/// any of the bundled files fails. The `name`/`path` fields point at
/// the file that failed.
pub fn seed_bundled_into(workdir: &Path, bundled: &[(&str, &str)]) -> Result<SeedReport> {
    let dir = workdir.join(WORKDIR_PROMPTS_SUBDIR);
    fs::create_dir_all(&dir).map_err(|e| PromptError::Io {
        name: WORKDIR_PROMPTS_SUBDIR.to_owned(),
        path: dir.clone(),
        detail: e.to_string(),
    })?;
    let mut report = SeedReport::default();
    for (name, body) in bundled {
        let dst = dir.join(format!("{name}.md"));
        if dst.exists() {
            report.preserved += 1;
            tracing::debug!(
                name = *name,
                "prompts: workdir override already present, preserved"
            );
            continue;
        }
        wiki::atomic_write(&dst, body.as_bytes()).map_err(|e| PromptError::Io {
            name: (*name).to_owned(),
            path: dst.clone(),
            detail: e.to_string(),
        })?;
        report.created += 1;
        tracing::debug!(name = *name, path = %dst.display(), "prompts: bundled default materialised");
    }
    tracing::info!(
        created = report.created,
        preserved = report.preserved,
        dir = %dir.display(),
        "prompts: bundled defaults seeded into workdir"
    );
    Ok(report)
}

/// Load `name` then substitute `{key}` placeholders from `vars`.
///
/// Sugar over [`load`] + [`substitute`] for the common "load + fill"
/// path. See [`substitute`] for the placeholder grammar and the
/// single-pass safety guarantee.
///
/// # Errors
///
/// Same as [`load`]: [`PromptError::Io`] on a present-but-unreadable
/// override and [`PromptError::NoFencedBlock`] on a malformed
/// markdown document.
pub fn render(
    name: &str,
    workdir: &Path,
    bundled_fallback: &str,
    vars: &[(&str, &str)],
) -> Result<String> {
    let body = load(name, workdir, bundled_fallback)?;
    Ok(substitute(&body, vars))
}

/// Frontmatter key carrying the bundled version a prompt file was
/// seeded from. Every shipped prompt under
/// `crates/<crate>/prompts/<name>.md` writes it; the operator
/// inherits it verbatim when `mwe-mcp init` materialises the file.
/// At drift-detection time the loader reads this
/// key out of the workdir file and compares it against the bundled
/// body's own value — a mismatch means the binary's bundled body has
/// moved past what the workdir was seeded with.
const VERSION_KEY: &str = "default_version_at_bootstrap:";

/// Extract `default_version_at_bootstrap: vN.M` from a markdown
/// document's YAML frontmatter, if present.
///
/// The frontmatter is the YAML block delimited by `---` at the top of
/// the document — same shape as every bundled prompt. This helper
/// scans line-by-line
/// rather than parsing full YAML so it stays decoupled from
/// `serde_yaml` (the bundled prompts use the field as a plain
/// scalar; quoting / `'` / `"` is stripped). Returns `None` when:
///
/// - the document has no `---`-delimited frontmatter,
/// - the frontmatter does not carry the key,
/// - the value is empty after trimming.
///
/// The drift banner treats `None` on the workdir file as drift
/// when the bundled body carries a version — meaning the operator
/// removed the key by hand-editing the frontmatter — and as
/// "no drift to show" when the bundled body also has no version
/// (defensive against bundled prompts that opt out of versioning).
#[must_use]
pub fn parse_default_version_at_bootstrap(md: &str) -> Option<String> {
    let rest = md.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for raw in frontmatter.lines() {
        let line = raw.trim_start();
        let Some(value) = line.strip_prefix(VERSION_KEY) else {
            continue;
        };
        let trimmed = value.trim();
        // YAML scalar with optional single / double quoting — strip
        // exactly one pair if present, then trim again so something
        // like `default_version_at_bootstrap: "v1.2"` returns `v1.2`.
        let unquoted = if trimmed.len() >= 2
            && let Some(stripped) = trimmed
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| {
                    trimmed
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                }) {
            stripped
        } else {
            trimmed
        };
        let value = unquoted.trim();
        if value.is_empty() {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SAMPLE_MD: &str = "---\nname: sample\n---\n\n# Prompt: sample\n\nblah blah\n\n## System prompt\n\n```text\nHello, world.\nThis is the body.\n```\n";

    #[test]
    fn extract_fenced_text_returns_body() {
        let body = extract_fenced_text(SAMPLE_MD, "sample", "<test>").expect("extract");
        assert_eq!(body, "Hello, world.\nThis is the body.");
    }

    #[test]
    fn extract_fenced_text_errors_when_opener_missing() {
        let md = "---\nname: x\n---\n\njust prose, no fence.\n";
        let err = extract_fenced_text(md, "x", "<test>").expect_err("must fail");
        assert!(matches!(err, PromptError::NoFencedBlock { .. }));
    }

    #[test]
    fn extract_fenced_text_errors_when_closer_missing() {
        let md = "```text\nopen but no close\n";
        let err = extract_fenced_text(md, "y", "<test>").expect_err("must fail");
        assert!(matches!(err, PromptError::NoFencedBlock { .. }));
    }

    /// The opener must be followed by `\n` — `text` glued to text on
    /// the same line is not the markdown the loader expects.
    #[test]
    fn extract_fenced_text_errors_when_opener_inline() {
        let md = "```text foo bar\n```\n";
        let err = extract_fenced_text(md, "z", "<test>").expect_err("must fail");
        assert!(matches!(err, PromptError::NoFencedBlock { .. }));
    }

    /// Bundled fallback path: no workdir override file ⇒ extract from
    /// the in-process string.
    #[test]
    fn load_falls_back_to_bundled_when_workdir_file_absent() {
        let dir = TempDir::new().unwrap();
        let body = load("sample", dir.path(), SAMPLE_MD).expect("load must fall back to bundled");
        assert_eq!(body, "Hello, world.\nThis is the body.");
    }

    /// Override path: `<workdir>/prompts/<name>.md` present and
    /// well-formed ⇒ its body wins over the bundled fallback.
    #[test]
    fn load_prefers_workdir_override_over_bundled() {
        let dir = TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("sample.md"),
            "---\nname: sample\n---\n\n```text\nOverridden body.\n```\n",
        )
        .unwrap();
        let body = load("sample", dir.path(), SAMPLE_MD).expect("load must read override");
        assert_eq!(body, "Overridden body.");
    }

    /// Override exists but is corrupted (no fenced block) ⇒ the loader
    /// surfaces a `NoFencedBlock` error pointing at the override path
    /// (NOT a silent fallback to bundled — the operator hand-edited
    /// the file and the failure should be loud).
    #[test]
    fn load_errors_when_workdir_override_is_malformed() {
        let dir = TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let override_path = prompts_dir.join("sample.md");
        std::fs::write(&override_path, "no fence here at all").unwrap();
        let err =
            load("sample", dir.path(), SAMPLE_MD).expect_err("malformed override must fail loud");
        match err {
            PromptError::NoFencedBlock { name, origin } => {
                assert_eq!(name, "sample");
                assert!(
                    origin.contains("sample.md"),
                    "origin must point at the override path: {origin}"
                );
            },
            other @ PromptError::Io { .. } => {
                panic!("expected NoFencedBlock, got {other:?}")
            },
        }
    }

    /// Every shipped bundled prompt must parse through the fence
    /// extractor. A bundled prompt missing its `text` code fence is
    /// `NoFencedBlock` at load time, and best-effort call sites
    /// degrade silently instead of failing loud — so the whole
    /// roster is pinned here, not just a hand-picked pair.
    #[test]
    fn extract_fenced_text_parses_every_shipped_bundled() {
        for (name, md) in BUNDLED {
            let body = extract_fenced_text(md, name, "<bundled>")
                .unwrap_or_else(|err| panic!("bundled prompt `{name}` must parse: {err}"));
            assert!(
                !body.trim().is_empty(),
                "bundled prompt `{name}` parsed to an empty body"
            );
        }
    }

    /// Content spot-checks for individual shipped prompts. Fence
    /// coverage for the whole roster lives in
    /// `extract_fenced_text_parses_every_shipped_bundled`; the tests
    /// below additionally pin recognisable openings and body shape
    /// for prompts whose call sites are load-bearing.
    #[test]
    fn extract_fenced_text_handles_shipped_ingest_default() {
        let md = include_str!("../prompts/ingest.md");
        let body = extract_fenced_text(md, "ingest", "<bundled>").expect("ingest parses");
        assert!(
            body.starts_with("You are the `ingest` classifier"),
            "first 60 chars: {:?}",
            &body[..60.min(body.len())]
        );
        // The body must be non-trivially long — a regression where
        // the closing fence is moved inside the body would surface
        // here as a sudden length collapse.
        assert!(body.len() > 5_000, "ingest body length is {}", body.len());
    }

    /// Regression guard for the `query-seeds` prompt envelope: the bundled
    /// document must carry a `text` fence so the loader returns the body
    /// instead of `NoFencedBlock`. A prompt file missing its fence parses to
    /// nothing and silently degrades the `wiki_navigate` seed cascade, so pin
    /// that the shipped default is a well-formed, non-empty prompt.
    #[test]
    fn extract_fenced_text_handles_shipped_query_seeds_default() {
        let md = include_str!("../prompts/query-seeds.md");
        let body = extract_fenced_text(md, "query-seeds", "<bundled>").expect("query-seeds parses");
        assert!(
            body.starts_with("You extract recall seeds"),
            "first 60 chars: {:?}",
            &body[..60.min(body.len())]
        );
        // The extractor's JSON contract must survive the wrap.
        assert!(
            body.contains("\"topics\"") && body.contains("\"entities\""),
            "query-seeds body lost its JSON schema"
        );
    }

    // ---------- substitute / render ----------

    #[test]
    fn substitute_replaces_known_placeholders() {
        let body = "Hi {name}, age {age}.";
        let out = substitute(body, &[("name", "Ada"), ("age", "36")]);
        assert_eq!(out, "Hi Ada, age 36.");
    }

    /// Unknown placeholders are left verbatim (no panic, no empty
    /// substitution) — this is important so a prompt rolled forward
    /// to expect a new variable still renders something the operator
    /// can read while the binary is being updated.
    #[test]
    fn substitute_leaves_unknown_placeholders_in_place() {
        let body = "Hi {name}, {unknown}.";
        let out = substitute(body, &[("name", "Ada")]);
        assert_eq!(out, "Hi Ada, {unknown}.");
    }

    /// JSON literals inside the body must survive untouched. The
    /// guard is that `"` is not an identifier character, so the
    /// scanner aborts the placeholder match on the first character
    /// after the brace.
    #[test]
    fn substitute_does_not_touch_json_literals() {
        let body = r#"reply {"same": true} or {"same": false}; body: {body}"#;
        let out = substitute(body, &[("body", "X")]);
        assert_eq!(out, r#"reply {"same": true} or {"same": false}; body: X"#);
    }

    /// A value that itself contains `{key}` syntax must not be
    /// re-scanned — protects against accidental re-entrance when the
    /// runtime data happens to look like a placeholder.
    #[test]
    fn substitute_does_not_re_scan_replacement_values() {
        let body = "before {x} after";
        let out = substitute(body, &[("x", "{y}"), ("y", "BUG")]);
        assert_eq!(out, "before {y} after");
    }

    /// Empty braces (`{}`) and braces around a non-identifier (a
    /// digit, punctuation) are not placeholders — pass through.
    #[test]
    fn substitute_passes_through_malformed_braces() {
        let body = "empty {} digit {1} dotted {a.b} dashed {a-b}";
        let out = substitute(body, &[("a", "X")]);
        assert_eq!(out, "empty {} digit {1} dotted {a.b} dashed {a-b}");
    }

    /// Identifier accepts underscores and digits after the first
    /// alphabetic / underscore character (mirrors a Rust ident).
    #[test]
    fn substitute_accepts_underscore_and_digit_idents() {
        let body = "{_v} {v1} {v_2} {V}";
        let out = substitute(body, &[("_v", "A"), ("v1", "B"), ("v_2", "C"), ("V", "D")]);
        assert_eq!(out, "A B C D");
    }

    /// Lone `{` characters that never close — pass through verbatim
    /// (don't truncate the body by accidentally swallowing the
    /// remainder).
    #[test]
    fn substitute_passes_through_unclosed_brace() {
        let body = "trailing {oops";
        let out = substitute(body, &[]);
        assert_eq!(out, "trailing {oops");
    }

    /// `render` round-trip: workdir override wins, placeholders
    /// substituted from the override body.
    #[test]
    fn render_substitutes_from_workdir_override() {
        let dir = TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("sample.md"),
            "---\nname: sample\n---\n\n```text\nhello {who}!\n```\n",
        )
        .unwrap();
        let out = render(
            "sample",
            dir.path(),
            "```text\nbundled {who}\n```",
            &[("who", "world")],
        )
        .expect("render");
        assert_eq!(out, "hello world!");
    }

    /// `render` from bundled fallback when no override is present.
    #[test]
    fn render_substitutes_from_bundled_fallback() {
        let dir = TempDir::new().unwrap();
        let out = render(
            "sample",
            dir.path(),
            "```text\nbundled {who}\n```",
            &[("who", "world")],
        )
        .expect("render");
        assert_eq!(out, "bundled world");
    }

    // ---------- seed_bundled_into / SeedReport ----------

    /// Fresh workdir: every bundled prompt is written; report counts
    /// match the input slice; the materialised file holds the bundled
    /// bytes verbatim (so `default_version_at_bootstrap` from the
    /// frontmatter survives the trip — the marker drift detection will read).
    #[test]
    fn seed_bundled_into_writes_every_bundled_on_fresh_workdir() {
        let dir = TempDir::new().unwrap();
        let bundled: &[(&str, &str)] = &[
            (
                "alpha",
                "---\nname: alpha\ndefault_version_at_bootstrap: v0.1\n---\n\n```text\nA\n```\n",
            ),
            (
                "beta",
                "---\nname: beta\ndefault_version_at_bootstrap: v0.2\n---\n\n```text\nB\n```\n",
            ),
        ];
        let report = seed_bundled_into(dir.path(), bundled).expect("seed");
        assert_eq!(report.created, 2);
        assert_eq!(report.preserved, 0);
        let alpha = std::fs::read_to_string(dir.path().join("prompts/alpha.md")).unwrap();
        assert_eq!(alpha, bundled[0].1);
        let beta = std::fs::read_to_string(dir.path().join("prompts/beta.md")).unwrap();
        assert_eq!(beta, bundled[1].1);
    }

    /// Re-seeding is idempotent: existing files are preserved
    /// untouched, the report flips from `created=N preserved=0` to
    /// `created=0 preserved=N`. An operator hand-edit at
    /// `<workdir>/prompts/<name>.md` survives `mwe-mcp init` reruns.
    #[test]
    fn seed_bundled_into_is_idempotent_and_preserves_operator_edits() {
        let dir = TempDir::new().unwrap();
        let bundled: &[(&str, &str)] = &[(
            "alpha",
            "---\nname: alpha\n---\n\n```text\nbundled body\n```\n",
        )];
        // First call materialises the file.
        let first = seed_bundled_into(dir.path(), bundled).expect("seed");
        assert_eq!(
            first,
            SeedReport {
                created: 1,
                preserved: 0
            }
        );
        // Operator edits the file.
        let override_path = dir.path().join("prompts/alpha.md");
        std::fs::write(
            &override_path,
            "---\nname: alpha\n---\n\n```text\nedited by operator\n```\n",
        )
        .unwrap();
        // Re-seed: file is preserved verbatim.
        let second = seed_bundled_into(dir.path(), bundled).expect("re-seed");
        assert_eq!(
            second,
            SeedReport {
                created: 0,
                preserved: 1
            }
        );
        let after = std::fs::read_to_string(&override_path).unwrap();
        assert!(after.contains("edited by operator"));
        assert!(!after.contains("bundled body"));
    }

    /// New prompt added in a binary upgrade lands cleanly: the
    /// pre-existing one is preserved, the new one is created.
    #[test]
    fn seed_bundled_into_creates_only_missing_files() {
        let dir = TempDir::new().unwrap();
        let v1: &[(&str, &str)] = &[("alpha", "```text\nA\n```")];
        seed_bundled_into(dir.path(), v1).expect("v1 seed");
        let v2: &[(&str, &str)] = &[
            ("alpha", "```text\nA new\n```"),
            ("beta", "```text\nB\n```"),
        ];
        let report = seed_bundled_into(dir.path(), v2).expect("v2 seed");
        assert_eq!(report.created, 1);
        assert_eq!(report.preserved, 1);
        let alpha = std::fs::read_to_string(dir.path().join("prompts/alpha.md")).unwrap();
        // alpha was preserved, not overwritten with the new body.
        assert_eq!(alpha, "```text\nA\n```");
        let beta = std::fs::read_to_string(dir.path().join("prompts/beta.md")).unwrap();
        assert_eq!(beta, "```text\nB\n```");
    }

    /// `SeedReport::merged` combines two reports — used by the
    /// binary when it seeds `mwe-core` and `mwe-dashboard` slices
    /// separately and wants one summary line.
    #[test]
    fn seed_report_merged_sums_both_sides() {
        let a = SeedReport {
            created: 3,
            preserved: 1,
        };
        let b = SeedReport {
            created: 1,
            preserved: 4,
        };
        let m = a.merged(&b);
        assert_eq!(
            m,
            SeedReport {
                created: 4,
                preserved: 5
            }
        );
    }

    /// Round-trip: materialise the bundled `mwe-core` slice into a
    /// fresh workdir, then `load("ingest", workdir, BUNDLED)`. The
    /// loader must pick the just-written override at
    /// `<workdir>/prompts/ingest.md` (override path wins per the
    /// loader contract) and parse it cleanly. This is the regression
    /// guard against a divergence between the file shape `init`
    /// writes and the shape `load` expects to read.
    #[test]
    fn seed_then_load_round_trip_for_shipped_ingest() {
        let dir = TempDir::new().unwrap();
        let report = seed_bundled_into(dir.path(), BUNDLED).expect("seed");
        assert_eq!(report.created, BUNDLED.len());
        let body = load("ingest", dir.path(), "<should-not-be-used>").expect("load");
        assert!(body.starts_with("You are the `ingest` classifier"));
    }

    // ---------- parse_default_version_at_bootstrap ----------

    #[test]
    fn parse_default_version_returns_plain_value() {
        let md = "---\nname: x\ndefault_version_at_bootstrap: v1.2\n---\n\nbody";
        assert_eq!(
            parse_default_version_at_bootstrap(md).as_deref(),
            Some("v1.2")
        );
    }

    /// The version key may be quoted (single or double). YAML accepts
    /// both; the parser must strip exactly one matching pair so the
    /// drift comparator sees the plain scalar.
    #[test]
    fn parse_default_version_strips_quoting() {
        let dq = "---\ndefault_version_at_bootstrap: \"v2.0\"\n---\n";
        let sq = "---\ndefault_version_at_bootstrap: 'v2.0'\n---\n";
        assert_eq!(
            parse_default_version_at_bootstrap(dq).as_deref(),
            Some("v2.0")
        );
        assert_eq!(
            parse_default_version_at_bootstrap(sq).as_deref(),
            Some("v2.0")
        );
    }

    /// Documents without a frontmatter block (or with a malformed one
    /// — opener but no closer) carry no version. The dashboard treats
    /// a `None` workdir version against a `Some` bundled version as
    /// drift (operator wiped the key by hand), so this branch matters.
    #[test]
    fn parse_default_version_returns_none_without_frontmatter() {
        assert_eq!(
            parse_default_version_at_bootstrap("no frontmatter here"),
            None
        );
        assert_eq!(
            parse_default_version_at_bootstrap("---\nopen but no close\n"),
            None
        );
    }

    /// Empty value (`key:` followed by whitespace) returns None — an
    /// operator who cleared the value gets the same drift treatment
    /// as one who removed the line.
    #[test]
    fn parse_default_version_returns_none_for_empty_value() {
        let md = "---\ndefault_version_at_bootstrap:   \n---\n";
        assert_eq!(parse_default_version_at_bootstrap(md), None);
    }

    /// Indented key (YAML allows leading whitespace inside the
    /// frontmatter) is still found.
    #[test]
    fn parse_default_version_tolerates_leading_whitespace() {
        let md = "---\n  default_version_at_bootstrap: v3.0\n---\n";
        assert_eq!(
            parse_default_version_at_bootstrap(md).as_deref(),
            Some("v3.0")
        );
    }

    /// Every shipped bundled prompt carries the version key — the
    /// drift surface reads exactly this value at boot. A
    /// regression that drops the key from a freshly added bundled
    /// prompt would silently disable drift detection for that file,
    /// so guard against it here.
    #[test]
    fn parse_default_version_present_on_every_shipped_bundled() {
        for (name, body) in BUNDLED {
            let v = parse_default_version_at_bootstrap(body);
            assert!(
                v.is_some(),
                "bundled prompt `{name}` must carry default_version_at_bootstrap"
            );
        }
    }
}
