// SPDX-License-Identifier: AGPL-3.0-or-later
//! Corpus consistency checks.
//!
//! Backing for the `wiki_lint` MCP tool
//! ([tool-reference.md](../../../wiki/protocol/tool-reference.md)).
//!
//! ## Scope
//!
//! The MCP `wiki_lint` tool advertises **8 checks** ([mcp-tools.md](../../../wiki/protocol/mcp-tools.md)).
//! Four ship today:
//!
//! - [`Check::MarkerMalformed`] — re-runs [`crate::parser::parse`] on
//!   every `.md` page and lifts each [`ParseWarning`] into an issue.
//! - [`Check::OrphanFacts`] — every active `fact_index` row whose
//!   `{{f=<id>}}` marker is missing from the file it claims to live in.
//! - [`Check::MetaInvalid`] — every wiki directory whose `_meta.md` is
//!   missing or fails to parse as [`WikiMeta`].
//! - [`Check::EmbedMissing`] — every `{{embed=…}}` whose catalog row or
//!   blob is gone ([media pipeline](../../../wiki/design-notes/media-pipeline.md)).
//!
//! The other four (`broken_crosslinks`, `acl_inconsistent`,
//! `hub_outdated`, `superseded_chain`) ship in later
//! milestones; calling them today returns zero issues — the
//! `summary.by_check` map still lists them so a future operator can
//! tell at a glance which checks evaluated to "nothing wrong" vs "not
//! yet implemented" (see [`Issue::message`] for the canned line).
//!
//! ## Output ordering
//!
//! Issues are sorted by `(severity desc, check asc, wiki_id asc,
//! fact_id asc)` so two consecutive runs over an unchanged corpus
//! produce byte-identical output — handy when piping to `diff` for an
//! audit.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::parser;
use crate::wiki::{WikiMeta, WikiTree};

/// Errors raised by the lint pipeline.
#[derive(Debug, Error)]
pub enum LintError {
    /// Underlying SQL failure (`fact_index` queries).
    #[error("lint db: {0}")]
    Db(#[from] sqlx::Error),
    /// Filesystem traversal error.
    #[error("lint io: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, LintError>;

/// The 8 advertised checks. Order matches
/// [mcp-tools.md](../../../wiki/protocol/mcp-tools.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// Cross-link to a `[[fact_id]]` that no longer exists. **Not yet
    /// implemented** — needs the wikilink resolver from the recall
    /// pipeline.
    BrokenCrosslinks,
    /// `{{…}}…{{/}}` marker syntax violation. **Implemented**.
    MarkerMalformed,
    /// `fact_index` row whose marker is not present in the source file.
    /// **Implemented**.
    OrphanFacts,
    /// `_meta.md` missing or unparseable. **Implemented**.
    MetaInvalid,
    /// ACL syntactically valid but semantically suspect (e.g. allow=
    /// references a non-existent group). **Not yet implemented**.
    AclInconsistent,
    /// `{{embed=c-…}}` referencing a missing catalog asset: no
    /// `media_catalog` row for the key, or a row whose blob is gone
    /// from the content-addressed store. **Implemented**.
    EmbedMissing,
    /// Wiki `index.md` older than its newest fact. **Not yet
    /// implemented**.
    HubOutdated,
    /// Cycle or broken reference in a supersedence chain. **Not yet
    /// implemented**.
    SupersededChain,
}

impl Check {
    /// All 8 checks. Used by the dispatcher when the caller omits
    /// `checks` (default = run them all).
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::BrokenCrosslinks,
            Self::MarkerMalformed,
            Self::OrphanFacts,
            Self::MetaInvalid,
            Self::AclInconsistent,
            Self::EmbedMissing,
            Self::HubOutdated,
            Self::SupersededChain,
        ]
    }

    /// Wire-stable lowercase string used in the tool's output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BrokenCrosslinks => "broken_crosslinks",
            Self::MarkerMalformed => "marker_malformed",
            Self::OrphanFacts => "orphan_facts",
            Self::MetaInvalid => "meta_invalid",
            Self::AclInconsistent => "acl_inconsistent",
            Self::EmbedMissing => "embed_missing",
            Self::HubOutdated => "hub_outdated",
            Self::SupersededChain => "superseded_chain",
        }
    }

    /// Parse a wire string back to a [`Check`]. Returns `None` for
    /// unknown strings so the dispatcher can surface
    /// `invalid_input` with a precise mention of the offending value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "broken_crosslinks" => Some(Self::BrokenCrosslinks),
            "marker_malformed" => Some(Self::MarkerMalformed),
            "orphan_facts" => Some(Self::OrphanFacts),
            "meta_invalid" => Some(Self::MetaInvalid),
            "acl_inconsistent" => Some(Self::AclInconsistent),
            "embed_missing" => Some(Self::EmbedMissing),
            "hub_outdated" => Some(Self::HubOutdated),
            "superseded_chain" => Some(Self::SupersededChain),
            _ => None,
        }
    }

    /// `true` when this check actually walks the corpus today; `false`
    /// when it ships in a later milestone (the dispatcher still lists
    /// it in the summary but returns zero issues).
    #[must_use]
    pub const fn is_implemented(self) -> bool {
        matches!(
            self,
            Self::MarkerMalformed | Self::OrphanFacts | Self::MetaInvalid | Self::EmbedMissing
        )
    }
}

/// Severity of a single issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Soft signal — surfacing for awareness, not blocking.
    Info,
    /// Likely a real problem; operator should investigate.
    Warning,
    /// Definitely broken (orphan row, missing meta, unparseable marker).
    Error,
}

impl Severity {
    /// Wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Scope filter for [`run`].
#[derive(Debug, Default, Clone)]
pub struct LintScope {
    /// Optional subset of wikis to lint. Empty / `None` ⇒ every wiki in
    /// the tree.
    pub wiki_ids: Option<Vec<String>>,
}

/// One reported issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    /// Severity classification.
    pub severity: Severity,
    /// Which check produced the issue.
    pub check: Check,
    /// Containing wiki, if the issue is wiki-scoped.
    pub wiki_id: Option<String>,
    /// Containing fact, if the issue is region-scoped.
    pub fact_id: Option<String>,
    /// Human-readable description, English, no trailing period.
    pub message: String,
    /// Optional remediation hint (canned text per check kind).
    pub suggested_fix: Option<String>,
}

/// Whole-run summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LintReport {
    /// All issues, sorted for deterministic output (see module doc).
    pub issues: Vec<Issue>,
    /// Total issue count = `issues.len()`, surfaced for client UI.
    pub total: usize,
    /// `severity → count`.
    pub by_severity: BTreeMap<String, usize>,
    /// `check → count` — includes a zero entry for every requested
    /// check so the consumer can distinguish "ran clean" from
    /// "not requested".
    pub by_check: BTreeMap<String, usize>,
}

/// Run the requested checks. `checks` may include checks that are not
/// yet implemented; those will simply yield no issues.
///
/// `scope.wiki_ids` is an optional whitelist by wiki id. Empty / None
/// ⇒ lint the entire tree.
///
/// # Errors
///
/// - [`LintError::Io`] on filesystem traversal failures (excluding
///   per-page parse problems, which surface as `marker_malformed`
///   issues).
/// - [`LintError::Db`] on `fact_index` query failures.
pub async fn run(
    pool: &SqlitePool,
    tree: &WikiTree,
    scope: &LintScope,
    checks: &[Check],
) -> Result<LintReport> {
    let active = active_checks(checks);
    let want = |c: Check| active.contains(&c);

    let mut issues: Vec<Issue> = Vec::new();

    // Walk the wikis dir ourselves so a malformed `_meta.md` becomes an
    // issue instead of bubbling up as a fatal error.
    let mut wiki_index: BTreeMap<String, PathBuf> = BTreeMap::new();
    walk_for_lint(
        tree.wikis_dir(),
        tree.wikis_dir(),
        scope,
        &mut wiki_index,
        &mut issues,
        want(Check::MetaInvalid),
    )?;

    // Filter by scope.wiki_ids at the wiki level (already done by the
    // walker for meta-derived ids; orphan check uses the same index).
    if let Some(allowed) = scope.wiki_ids.as_ref().filter(|v| !v.is_empty()) {
        let allowed: BTreeSet<_> = allowed.iter().cloned().collect();
        wiki_index.retain(|id, _| allowed.contains(id));
    }

    if want(Check::MarkerMalformed) {
        for (wiki_id, abs_dir) in &wiki_index {
            walk_pages_for_markers(abs_dir, wiki_id, &mut issues)?;
        }
    }

    if want(Check::OrphanFacts) {
        for (wiki_id, abs_dir) in &wiki_index {
            scan_orphans(pool, wiki_id, abs_dir, tree.workdir(), &mut issues).await?;
        }
    }

    if want(Check::EmbedMissing) {
        for (wiki_id, abs_dir) in &wiki_index {
            scan_missing_embeds(pool, wiki_id, abs_dir, tree.workdir(), &mut issues).await?;
        }
    }

    Ok(sort_and_summarise(issues, checks))
}

/// Every `{{embed=…}}` on a page (standalone or inside a region body)
/// must resolve to a `media_catalog` row whose blob is present in the
/// content-addressed store — a dangling key is dead media at render
/// and export time.
async fn scan_missing_embeds(
    pool: &SqlitePool,
    wiki_id: &str,
    abs_dir: &Path,
    workdir: &Path,
    issues: &mut Vec<Issue>,
) -> Result<()> {
    for entry in std::fs::read_dir(abs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() || !is_lintable_page(&path) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let page_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        for cid in parser::collect_embeds(&raw) {
            let problem = match crate::media::find_by_id(pool, &cid).await {
                Ok(Some(row)) => {
                    if crate::media::blob_path(workdir, &row.sha256).exists() {
                        continue;
                    }
                    "catalog row exists but its blob is missing from the store"
                },
                Ok(None) => "no media_catalog row for this catalog_id",
                Err(e) => return Err(LintError::Db(sqlx_from_media(e))),
            };
            issues.push(Issue {
                severity: Severity::Error,
                check: Check::EmbedMissing,
                wiki_id: Some(wiki_id.to_owned()),
                fact_id: None,
                message: format!("{page_name}: {{{{embed={cid}}}}} — {problem}"),
                suggested_fix: Some(
                    "re-upload the media via POST /media, or remove the dangling embed".into(),
                ),
            });
        }
    }
    Ok(())
}

/// Unwrap the media layer's error into the lint DB class (the only
/// failure it can carry here is the underlying sqlx error).
fn sqlx_from_media(e: crate::media::MediaError) -> sqlx::Error {
    match e {
        crate::media::MediaError::Db(db) => db,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}

fn active_checks(checks: &[Check]) -> BTreeSet<Check> {
    if checks.is_empty() {
        Check::all().into_iter().collect()
    } else {
        checks.iter().copied().collect()
    }
}

fn walk_for_lint(
    wikis_root: &Path,
    abs_dir: &Path,
    scope: &LintScope,
    out: &mut BTreeMap<String, PathBuf>,
    issues: &mut Vec<Issue>,
    want_meta: bool,
) -> Result<()> {
    let meta_path = abs_dir.join("_meta.md");
    if meta_path.exists() {
        match std::fs::read_to_string(&meta_path)
            .map_err(LintError::Io)
            .and_then(|raw| {
                WikiMeta::parse(&meta_path, &raw).map_err(|e| {
                    LintError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    ))
                })
            }) {
            Ok((meta, _)) => {
                let id = meta.wiki_id.as_str().to_owned();
                if scope_includes(scope, &id) {
                    out.insert(id, abs_dir.to_path_buf());
                }
            },
            Err(e) => {
                if want_meta {
                    let rel = abs_dir.strip_prefix(wikis_root).unwrap_or(abs_dir);
                    issues.push(Issue {
                        severity: Severity::Error,
                        check: Check::MetaInvalid,
                        wiki_id: None,
                        fact_id: None,
                        message: format!("_meta.md at {} failed to parse: {e}", rel.display()),
                        suggested_fix: Some(
                            "fix the YAML frontmatter or restore from the prior commit".into(),
                        ),
                    });
                }
            },
        }
    }

    for entry in std::fs::read_dir(abs_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            walk_for_lint(wikis_root, &entry.path(), scope, out, issues, want_meta)?;
        }
    }
    Ok(())
}

fn scope_includes(scope: &LintScope, wiki_id: &str) -> bool {
    scope
        .wiki_ids
        .as_ref()
        .filter(|v| !v.is_empty())
        .is_none_or(|v| v.iter().any(|s| s == wiki_id))
}

fn walk_pages_for_markers(abs_dir: &Path, wiki_id: &str, issues: &mut Vec<Issue>) -> Result<()> {
    for entry in std::fs::read_dir(abs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            // Sub-wikis are processed via their own iteration in `run`;
            // skip recursion so the issue is filed against the right
            // wiki_id.
            continue;
        }
        if !is_lintable_page(&path) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let out = parser::parse(&raw);
        for w in out.warnings {
            issues.push(Issue {
                severity: Severity::Error,
                check: Check::MarkerMalformed,
                wiki_id: Some(wiki_id.to_owned()),
                fact_id: None,
                message: format!(
                    "{}: {} at offset {} ({:?})",
                    path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                    w.detail,
                    w.offset,
                    w.kind
                ),
                suggested_fix: Some(
                    "balance the `{{…}}…{{/}}` markers on the offending line".into(),
                ),
            });
        }
    }
    Ok(())
}

fn is_lintable_page(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    if !ext.eq_ignore_ascii_case("md") {
        return false;
    }
    // Skip the meta file (its body is YAML-only) — meta_invalid handles it.
    if stem == "_meta" {
        return false;
    }
    true
}

async fn scan_orphans(
    pool: &SqlitePool,
    wiki_id: &str,
    abs_dir: &Path,
    workdir: &Path,
    issues: &mut Vec<Issue>,
) -> Result<()> {
    let rows = crate::fact_index::find_active_in_wiki(pool, wiki_id)
        .await
        .map_err(map_fact_err)?;
    if rows.is_empty() {
        return Ok(());
    }

    // Build a set of (relative source_path, fact_id) markers actually
    // present on disk under this wiki dir.
    let mut on_disk: HashSet<(String, String)> = HashSet::new();
    collect_markers(abs_dir, workdir, &mut on_disk)?;

    for row in rows {
        let key = (row.source_path.clone(), row.fact_id.as_str().to_owned());
        if !on_disk.contains(&key) {
            issues.push(Issue {
                severity: Severity::Error,
                check: Check::OrphanFacts,
                wiki_id: Some(wiki_id.to_owned()),
                fact_id: Some(row.fact_id.as_str().to_owned()),
                message: format!(
                    "fact {fact} active in fact_index but no `{{{{f={fact}}}}}` marker in {path}",
                    fact = row.fact_id.as_str(),
                    path = row.source_path,
                ),
                suggested_fix: Some(
                    "either restore the marker in the source file or call `wiki_forget`".into(),
                ),
            });
        }
    }
    Ok(())
}

fn collect_markers(
    abs_dir: &Path,
    workdir: &Path,
    out: &mut HashSet<(String, String)>,
) -> Result<()> {
    for entry in std::fs::read_dir(abs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_markers(&path, workdir, out)?;
            continue;
        }
        if !is_lintable_page(&path) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = crate::wiki::workdir_relative_source_path(workdir, &path);
        let parsed = parser::parse(&raw);
        for ev in parsed.events {
            if let parser::ParseEvent::Region { attrs, .. } = ev
                && let Some(f) = attrs.fact_id
            {
                out.insert((rel.clone(), f.as_str().to_owned()));
            }
        }
    }
    Ok(())
}

fn sort_and_summarise(mut issues: Vec<Issue>, requested: &[Check]) -> LintReport {
    // Severity DESC, then check ASC, then wiki_id ASC, then fact_id ASC.
    issues.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.check.cmp(&b.check))
            .then_with(|| a.wiki_id.cmp(&b.wiki_id))
            .then_with(|| a.fact_id.cmp(&b.fact_id))
    });

    let mut by_severity: BTreeMap<String, usize> = BTreeMap::new();
    for sev in [Severity::Info, Severity::Warning, Severity::Error] {
        by_severity.insert(sev.as_str().to_owned(), 0);
    }
    let mut by_check: BTreeMap<String, usize> = BTreeMap::new();
    for c in active_checks(requested) {
        by_check.insert(c.as_str().to_owned(), 0);
    }
    for issue in &issues {
        *by_severity
            .entry(issue.severity.as_str().to_owned())
            .or_insert(0) += 1;
        *by_check.entry(issue.check.as_str().to_owned()).or_insert(0) += 1;
    }

    let total = issues.len();
    LintReport {
        issues,
        total,
        by_severity,
        by_check,
    }
}

fn map_fact_err(e: crate::fact_index::FactIndexError) -> LintError {
    match e {
        crate::fact_index::FactIndexError::Db(db) => LintError::Db(db),
        other => LintError::Io(std::io::Error::other(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::fact_index;
    use std::path::PathBuf;

    async fn fresh_pool_and_tree() -> (sqlx::SqlitePool, WikiTree, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = db::open_or_init(dir.path()).await.expect("db");
        let tree = WikiTree::open(dir.path()).expect("tree");
        (pool, tree, dir)
    }

    fn write_meta(abs_dir: &PathBuf, wiki_id: &str, slug: &str) {
        std::fs::create_dir_all(abs_dir).unwrap();
        let body = format!(
            "---\nwiki_id: {wiki_id}\nslug: {slug}\nwiki_type: wiki-user\n\
             parent_wiki_id: null\ntitle: {slug}\nacl_default: 'user:alice'\n---\n"
        );
        std::fs::write(abs_dir.join("_meta.md"), body).unwrap();
    }

    #[tokio::test]
    async fn marker_malformed_reports_unclosed_markers() {
        let (pool, tree, _td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("alice");
        write_meta(&wiki_dir, "alice", "alice");
        std::fs::write(wiki_dir.join("notes.md"), "stuff {{owner=user:alice").unwrap();

        let r = run(
            &pool,
            &tree,
            &LintScope::default(),
            &[Check::MarkerMalformed],
        )
        .await
        .unwrap();
        assert!(r.total >= 1);
        assert!(r.issues.iter().any(|i| i.check == Check::MarkerMalformed));
        assert_eq!(*r.by_check.get("marker_malformed").unwrap(), r.total);
    }

    #[tokio::test]
    async fn meta_invalid_reports_unparseable_meta() {
        let (pool, tree, _td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("bogus");
        std::fs::create_dir_all(&wiki_dir).unwrap();
        std::fs::write(wiki_dir.join("_meta.md"), "not valid yaml ::: ---").unwrap();

        let r = run(&pool, &tree, &LintScope::default(), &[Check::MetaInvalid])
            .await
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.issues[0].check, Check::MetaInvalid);
        assert_eq!(r.issues[0].severity, Severity::Error);
    }

    #[tokio::test]
    async fn orphan_facts_reports_missing_marker() {
        let (pool, tree, _td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("alice");
        write_meta(&wiki_dir, "alice", "alice");
        std::fs::write(wiki_dir.join("notes.md"), "no markers here").unwrap();

        let fact_id = crate::types::FactId::parse(
            &uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string(),
        )
        .unwrap();
        let source_path = "wikis/alice/notes.md".to_owned();
        fact_index::insert(
            &pool,
            &fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "alice".into(),
                source_path: source_path.clone(),
                region_start: None,
                region_end: None,
                text: "no markers".into(),
                embedding: vec![0.1],
                owner_id: "user:alice".parse().unwrap(),
                allow_ids: vec![],
                sender_id: None,
                fact_type: None,
                topics: vec![],
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no classifier
                // placement proposal to carry.
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let r = run(&pool, &tree, &LintScope::default(), &[Check::OrphanFacts])
            .await
            .unwrap();
        assert!(r.total >= 1);
        let orphan = r
            .issues
            .iter()
            .find(|i| i.check == Check::OrphanFacts)
            .expect("orphan issue must surface");
        assert_eq!(orphan.fact_id.as_deref(), Some(fact_id.as_str()));
    }

    #[tokio::test]
    async fn orphan_facts_silent_when_marker_present() {
        let (pool, tree, _td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("alice");
        write_meta(&wiki_dir, "alice", "alice");
        let fact_id = crate::types::FactId::parse(
            &uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::ContextV7::new())).to_string(),
        )
        .unwrap();
        let body = format!(
            "{{{{owner=user:alice f={id}}}}}hello{{{{/}}}}",
            id = fact_id.as_str()
        );
        std::fs::write(wiki_dir.join("notes.md"), &body).unwrap();
        let source_path = "wikis/alice/notes.md".to_owned();
        fact_index::insert(
            &pool,
            &fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fact_id.clone(),
                wiki_id: "alice".into(),
                source_path,
                region_start: None,
                region_end: None,
                text: "hello".into(),
                embedding: vec![0.1],
                owner_id: "user:alice".parse().unwrap(),
                allow_ids: vec![],
                sender_id: None,
                fact_type: None,
                topics: vec![],
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no classifier
                // placement proposal to carry.
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();

        let r = run(&pool, &tree, &LintScope::default(), &[Check::OrphanFacts])
            .await
            .unwrap();
        assert!(
            !r.issues.iter().any(|i| i.check == Check::OrphanFacts),
            "marker present ⇒ no orphan issue",
        );
    }

    #[tokio::test]
    async fn summary_includes_zero_entries_for_requested_unimplemented_checks() {
        let (pool, tree, _td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("alice");
        write_meta(&wiki_dir, "alice", "alice");

        let r = run(
            &pool,
            &tree,
            &LintScope::default(),
            &[Check::AclInconsistent, Check::EmbedMissing],
        )
        .await
        .unwrap();
        assert_eq!(r.total, 0);
        assert_eq!(*r.by_check.get("acl_inconsistent").unwrap(), 0);
        assert_eq!(*r.by_check.get("embed_missing").unwrap(), 0);
    }

    #[tokio::test]
    async fn embed_missing_reports_dangling_keys_and_passes_resolvable_ones() {
        let (pool, tree, td) = fresh_pool_and_tree().await;
        let wiki_dir = tree.wikis_dir().join("alice");
        write_meta(&wiki_dir, "alice", "alice");

        // A resolvable embed: catalogued, blob present.
        let stored = crate::media::store_media(
            &pool,
            td.path(),
            crate::media::NewMedia {
                bytes: b"jpegbytes".to_vec(),
                kind: crate::media::kind::PHOTO.to_owned(),
                mime: "image/jpeg".to_owned(),
                owner: "user:alice".parse().unwrap(),
                uploaded_by_consumer: None,
                caption: None,
                description: None,
                original_filename: Some("p.jpg".to_owned()),
            },
        )
        .await
        .expect("store");
        let good = stored.row.catalog_id;
        std::fs::write(
            wiki_dir.join("notes.md"),
            format!(
                "ok {{{{embed={good}}}}}\n\ndangling {{{{embed=c-2020-01-01-photo-009.jpg}}}}\n"
            ),
        )
        .unwrap();

        let r = run(&pool, &tree, &LintScope::default(), &[Check::EmbedMissing])
            .await
            .unwrap();
        assert_eq!(r.total, 1, "{:?}", r.issues);
        assert_eq!(r.issues[0].check, Check::EmbedMissing);
        assert!(
            r.issues[0].message.contains("c-2020-01-01-photo-009.jpg"),
            "{}",
            r.issues[0].message
        );
    }

    #[test]
    fn check_wire_strings_roundtrip() {
        for c in Check::all() {
            assert_eq!(Check::parse(c.as_str()).unwrap(), c);
        }
        assert!(Check::parse("not_a_check").is_none());
    }

    // Touch FakeEmbedder so the unused-import lint stays quiet — the
    // module is shared infrastructure other tests use; here we just
    // ensure the wiring compiles.
    #[test]
    fn fake_embedder_compiles_in_lint_tests() {
        let _ = FakeEmbedder::new("test", 1);
    }
}
