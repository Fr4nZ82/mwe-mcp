// SPDX-License-Identifier: AGPL-3.0-or-later
//! Skill catalog — bundled (via [`rust_embed`]).
//!
//! A "skill" is a markdown document that documents how a consumer LLM
//! agent should behave in a given mode: the always-loaded `core`, the
//! transversal `core-globalmemory`, the smart-wiki-bound
//! `smart-consumer`, the per-class `standard-conversational`, and the
//! conversion pattern `smart-codebase`.
//!
//! Bundled skills ship as `.md` files inside `crates/mwe-core/skills/`,
//! embedded into the binary at compile time. They are identical for
//! every operator.
//!
//! ## Identification
//!
//! Skills are identified by a globally-unique `name`, all reserved by
//! the bundled set (`core`, `core-globalmemory`, `smart-consumer`,
//! `standard-conversational`, `smart-codebase`).
//!
//! ## `ETag`
//!
//! Every skill carries an `etag` = `sha256(content)[..32]` (32 hex
//! chars, 16 bytes of digest). The HTTP `/skills` endpoints and the
//! MCP `skill_fetch` tool surface this so consumers can cache by
//! content. Two skills with identical body share the same etag — the
//! property the cache layer wants.
//!
//! ## Distribution
//!
//! Three modalities all read from the same in-memory + DB catalog:
//!
//! 1. **MCP tools** (`skill_list`, `skill_fetch`) — for consumers
//!    that already speak MCP.
//! 2. **HTTP endpoints** (`/skills`, `/skills/<name>.md`) — for
//!    consumers that prefer plain HTTP; ships alongside this module.
//! 3. **`InitializeResult.instructions`** — deferred to a future
//!    milestone; the pull modes above are enough for MVP.
//!
//! ## What this module does NOT do
//!
//! - It does not install skills into a consumer's framework. Each
//!   consumer (Claude Code, Codex, Cowork, …) has its own skill
//!   storage; mwe-mcp is the source, not the destination.
//! - It does not validate content semantics (only frontmatter
//!   shape). A skill that says "ignore the cardinal rule" parses
//!   fine — it's just a bad skill body.

use std::str;

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(RustEmbed)]
#[folder = "skills/"]
struct BundledSkills;

/// Errors raised by the skill catalog.
#[derive(Debug, Error)]
pub enum SkillError {
    /// Requested skill does not exist.
    #[error("skill not found: {0}")]
    NotFound(String),

    /// Bundled skill source on disk is unparseable / not utf-8.
    /// Indicates a build-time mistake (the include glob picked up
    /// something that shouldn't be there).
    #[error("malformed bundled skill {name}: {detail}")]
    MalformedBundled {
        /// Bundled file name (no `.md` suffix).
        name: String,
        /// Free-form detail.
        detail: String,
    },

    /// Underlying database error.
    #[error("skills db error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, SkillError>;

/// Catalog entry. Carries the frontmatter-derived metadata plus the
/// content hash; the full body is fetched separately via [`fetch`] so
/// `list` calls stay cheap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    /// Globally-unique skill name (no `.md` suffix).
    pub name: String,
    /// Semver-ish string from the frontmatter (free-form).
    pub version: String,
    /// One-line human description from the frontmatter.
    pub description: String,
    /// Names of skills this one depends on (parsed from
    /// `depends_on: [...]`). Empty when the field is absent.
    pub depends_on: Vec<String>,
    /// Content hash, `sha256(content)[..32]` hex.
    pub etag: String,
    /// Whether this is a bundled or custom skill.
    pub source: SkillSource,
}

/// Source of a skill in the catalog. Every skill is bundled today;
/// the enum is kept so the `source: {kind: "bundled"}` wire shape the
/// MCP/HTTP/dashboard surfaces emit stays stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SkillSource {
    /// Shipped with mwe-mcp via [`rust_embed`].
    Bundled,
}

/// List every bundled skill in deterministic order (by `name`).
///
/// Cheap — the catalog has a fixed handful of entries today and the
/// metadata sits inside the binary.
///
/// # Errors
///
/// Returns a [`SkillError::MalformedBundled`] when a bundled file
/// fails to parse — that means a build-time mistake and the caller
/// should surface it loudly rather than skip silently.
pub fn list_bundled() -> Result<Vec<Skill>> {
    let mut out: Vec<Skill> = BundledSkills::iter()
        .filter_map(|name| {
            let stem = name.strip_suffix(".md")?.to_owned();
            let raw = BundledSkills::get(&name)?;
            let content = match str::from_utf8(&raw.data) {
                Ok(s) => s.to_owned(),
                Err(_) => {
                    return Some(Err(SkillError::MalformedBundled {
                        name: stem,
                        detail: "non-utf8 content".to_owned(),
                    }));
                },
            };
            Some(Ok(skill_from_content(
                &stem,
                &content,
                SkillSource::Bundled,
            )))
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Fetch the full content of a skill. Returns `(Skill, content)` —
/// the body is returned verbatim, including the frontmatter.
///
/// # Errors
///
/// [`SkillError::NotFound`] when no bundled skill has the name.
pub fn fetch(name: &str) -> Result<(Skill, String)> {
    fetch_bundled(name)?.ok_or_else(|| SkillError::NotFound(name.to_owned()))
}

/// Public form of the bundled fetch — useful for the HTTP endpoint
/// that intentionally skips the DB lookup so unauthenticated GETs
/// only ever surface bundled material.
///
/// # Errors
///
/// As [`list_bundled`] for a misshapen embedded file.
pub fn fetch_bundled(name: &str) -> Result<Option<(Skill, String)>> {
    let path = format!("{name}.md");
    let Some(raw) = BundledSkills::get(&path) else {
        return Ok(None);
    };
    let content = match str::from_utf8(&raw.data) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            return Err(SkillError::MalformedBundled {
                name: name.to_owned(),
                detail: "non-utf8 content".to_owned(),
            });
        },
    };
    let skill = skill_from_content(name, &content, SkillSource::Bundled);
    Ok(Some((skill, content)))
}

// ---------- Internal helpers ----------

fn skill_from_content(name: &str, content: &str, source: SkillSource) -> Skill {
    let (version, description, depends_on) = parse_frontmatter(content);
    Skill {
        name: name.to_owned(),
        version,
        description,
        depends_on,
        etag: content_etag(content),
        source,
    }
}

/// `sha256(content)[..32]` hex — 16 bytes of digest, 32 hex chars.
fn content_etag(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let bytes = h.finalize();
    hex::encode(&bytes[..16])
}

/// Pull `version`, `description`, `depends_on` out of the markdown
/// frontmatter. Lenient: missing fields fall back to defaults, an
/// unparseable frontmatter degrades to empties without raising.
/// The catalog is a documentation surface — a bad skill is better
/// surfaced as "empty metadata + please-fix" than as a hard error
/// that takes the whole catalog down.
fn parse_frontmatter(content: &str) -> (String, String, Vec<String>) {
    let default_version = "0.0.0".to_owned();
    let default_description = String::new();
    let Some(rest) = content.strip_prefix("---") else {
        return (default_version, default_description, Vec::new());
    };
    let Some((frontmatter, _body)) = rest.split_once("\n---") else {
        return (default_version, default_description, Vec::new());
    };
    let map: serde_yaml::Mapping = match serde_yaml::from_str(frontmatter.trim_start_matches('\n'))
    {
        Ok(m) => m,
        Err(_) => return (default_version, default_description, Vec::new()),
    };
    let version = take_string(&map, "version").unwrap_or(default_version);
    let description = take_string(&map, "description").unwrap_or(default_description);
    let depends_on = take_string_array(&map, "depends_on").unwrap_or_default();
    (version, description, depends_on)
}

fn take_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(serde_yaml::Value::String(key.to_owned()))
        .and_then(|v| v.as_str().map(str::to_owned))
}

fn take_string_array(map: &serde_yaml::Mapping, key: &str) -> Option<Vec<String>> {
    let v = map.get(serde_yaml::Value::String(key.to_owned()))?;
    let seq = v.as_sequence()?;
    Some(
        seq.iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_bundled_returns_known_stubs_in_alphabetical_order() {
        let skills = list_bundled().expect("list bundled");
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        let expected = [
            "core",
            "core-globalmemory",
            "smart-codebase",
            "smart-consumer",
            "standard-conversational",
            "web-smart-consumer",
        ];
        for want in expected {
            assert!(names.contains(&want), "missing bundled skill: {want}");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "list_bundled must be alphabetical");
    }

    #[test]
    fn bundled_skills_have_etag_and_parsed_frontmatter() {
        let skills = list_bundled().expect("list bundled");
        for s in &skills {
            assert_eq!(s.etag.len(), 32, "etag must be 32 hex chars: {}", s.name);
            assert!(
                !s.version.is_empty(),
                "version missing on bundled {}",
                s.name
            );
            assert!(
                !s.description.is_empty(),
                "description missing on bundled {}",
                s.name
            );
            assert!(matches!(s.source, SkillSource::Bundled));
        }
    }

    #[test]
    fn etag_is_stable_for_same_content() {
        let a = content_etag("hello world\n");
        let b = content_etag("hello world\n");
        assert_eq!(a, b);
        let c = content_etag("hello world");
        assert_ne!(a, c, "trailing newline must alter the etag");
    }

    #[test]
    fn fetch_bundled_returns_full_body_with_frontmatter() {
        let (skill, content) = fetch_bundled("core").expect("ok").expect("found");
        assert_eq!(skill.name, "core");
        assert!(matches!(skill.source, SkillSource::Bundled));
        assert!(content.starts_with("---\n"));
        assert!(
            content.contains("name: core\n"),
            "frontmatter must be preserved verbatim"
        );
    }

    #[test]
    fn fetch_bundled_unknown_returns_none() {
        assert!(fetch_bundled("does-not-exist").expect("ok").is_none());
    }

    #[test]
    fn fetch_unknown_returns_not_found() {
        let err = fetch("does-not-exist").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(ref n) if n == "does-not-exist"));
    }
}
