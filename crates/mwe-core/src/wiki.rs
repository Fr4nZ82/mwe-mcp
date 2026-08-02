// SPDX-License-Identifier: AGPL-3.0-or-later
//! Memory-wiki filesystem surface I/O.
//!
//! `mwe-core::wiki` is the single owner of the `<workdir>/wikis/…` directory
//! tree — the memory's readable surface. Per the
//! [memory model](../../../docs/concepts/memory-model.md), authority is
//! split by wiki family: for standard wikis the `fact_index` is the
//! authoritative fact store and the files are its prose render; for smart
//! wikis the page content on disk is what gets indexed. Every internal API
//! in this module either reads the tree or rewrites a file atomically,
//! while the caller keeps the `fact_index` in step.
//!
//! ## What lives on disk
//!
//! ```text
//! <workdir>/
//!   .mwe-mcp.lock           (single-writer lockfile, see crate::lockfile)
//!   engine.db               (SQLite, see crate::db)
//!   wikis/                  (the forest container — holds no wiki of its own)
//!     <slug>/               (a top-level memory wiki: a user, a group, …)
//!       _meta.md            (YAML frontmatter, see WikiMeta)
//!       index.md            (Hub-Writer-managed hub page, optional)
//!       <leaf>.md           (leaf prose pages)
//!       <sub_slug>/         (sub-wiki, recursive)
//!         _meta.md
//!         …
//! ```
//!
//! The tree is a **forest**: `wikis/` is a plain container and every wiki is a
//! top-level (or nested) `<slug>/` directory — there is no single materialised
//! root wiki (the `WikiId::ROOT` id survives only as a defensive sentinel). A
//! `<slug>` is a [`WikiSlug`]; its `wiki_id` is recorded inside
//! `_meta.md.wiki_id` and is derived from the chain of slugs
//! ([`WikiId::child_of`]); a top-level wiki's `parent_wiki_id` is `null`.
//!
//! ## Atomic write protocol
//!
//! All writes go through [`atomic_write`]:
//!
//! 1. Acquire a `WriteMarker` on the target. While the marker is
//!    fresh, the file watcher suppresses events on the target — internal
//!    writes are expected to keep the index in sync without bouncing through
//!    the watcher.
//! 2. Create a `NamedTempFile` in the *same* directory as the target (so
//!    `rename(2)` stays on one filesystem and is atomic).
//! 3. Write payload → `sync_data` → `persist` (atomic rename).
//! 4. `fsync` the parent directory so the rename is durable across crash.
//! 5. Drop the marker.
//!
//! `WriteMarker` is RAII, so a panic during the write still cleans up.
//!
//! ## What this module does *not* do
//!
//! - Acquire the per-workdir lockfile. That is the server's job
//!   ([`crate::lockfile::acquire`]); the wiki module trusts the caller holds
//!   it for the duration of a write.
//! - Update `fact_index` or `wiki_events`. The caller (capture / supersede /
//!   forget / REM) is responsible for pairing the file write with the DB
//!   update inside the applicative WAL.
//! - Validate ACL at read time. The caller composes [`crate::render`] to
//!   apply per-sender filtering; this module returns raw page contents.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{Principal, WikiId, WikiIdParseError, WikiSlug, WikiSlugParseError};
use crate::watcher::WriteMarker;

/// Directory under the workdir that holds every memory-wiki tree.
pub const WIKIS_DIR: &str = "wikis";

/// Filename of the per-wiki manifest.
pub const META_FILENAME: &str = "_meta.md";

/// Filename of the per-wiki captures journal (`<wiki_dir>/_captures.md`).
///
/// The durable on-disk SSOT of buffered captures for a *standard* wiki: the
/// classifier writes here and the nightly compiler (Cronista) reads from it; the
/// published `.md` pages are the compiler's OUTPUT, not the capture target. It is
/// NOT a publishable page and carries no `{{f=…}}` fact regions, so it is excluded
/// from [`WikiHandle::list_pages`] and from the reindex marker sweep
/// (`crate::reindex`). See [`crate::capture_buffer`].
pub const CAPTURES_FILENAME: &str = "_captures.md";

/// Filename of the per-actor user-policy page (`<wiki_dir>/rules.md`).
///
/// A **user-facing** page (no underscore, unlike the `_meta`/`_captures`
/// plumbing) seeded with a default at actor-wiki creation. It holds the user's
/// standing policy in natural language — privacy/ACL rules the ingest honours
/// when it assigns the per-fact ACL (`owner` / `allow`), and behaviour rules
/// every consumer is shown. Its privacy/governance directives are raw prose
/// (read whole by `ingest::sender_rules`, never `fact_index` rows); the
/// behaviour-rule channel additionally writes `{{f=…}}` **behaviour-rule fact
/// regions** here via the direct path. Either way the engine never derives,
/// re-homes, or folds this page's facts: the slug `rules` is reserved from
/// placement ([`crate::planner`]) the way `index` is; `gather_standard_facts`
/// skips any fact whose page is this one ([`is_rules_page`]); the REM refile
/// sweep never nominates one; dedup pairs never cross the rules-page
/// boundary (both sides here, or neither — capture-time and REM revisor
/// alike); the REM validity sweeps fence it out too (never a contradiction
/// *satellite*, never completion *evidence* nor a completion candidate);
/// and the recall navigator never opens it (channel-only delivery) — so a
/// behaviour rule keeps living here and `recall_behaviour_rules` keeps
/// finding it, and `rules.md` survives the compile/REM cycle untouched. A
/// rule leaves the channel only via supersede, tombstone, or a closed
/// validity window — its owner's explicit closure, never collateral (the
/// channel filters validity at read time).
pub const RULES_FILENAME: &str = "rules.md";

/// True when `source_path` is a wiki's reserved policy page [`RULES_FILENAME`].
///
/// It is the rules pipeline's home for behaviour-rule facts, **outside every
/// structural sweep's perimeter** (compiler gather, refile nomination, and
/// the cross-page half of dedup all key on this predicate). Keyed on the
/// file name (`source_path` is a workdir-relative `wikis/<id>/…` path) so a
/// content page like `house_rules.md` is not caught.
#[must_use]
pub fn is_rules_page(source_path: &str) -> bool {
    std::path::Path::new(source_path)
        .file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(RULES_FILENAME))
}

/// Filename of the per-actor **project signposts** page
/// (`<wiki_dir>/projects.md`), roadmap group 48.
///
/// Home of the *signposts*: one short non-technical description per
/// project the actor owns, plus a handful of by-day activity lines. They
/// exist so a standard consumer knows those projects exist at all — the
/// smart-wiki corpora are invisible to a conversational turn's facts-only
/// recall ([`crate::recall::recall_facts`]) — and so that a surfaced
/// signpost can open its project's documentation
/// ([`crate::recall::recall_project_docs`]).
///
/// A signpost is **not a record**: the project's own wiki holds what was
/// actually done, and answering from the signpost would answer from a
/// summary of a summary. It is a pointer whose job is to cause the
/// deepening, which is why the descriptions are capped short and the
/// chronology is a rolling window rather than a log.
///
/// Written only by the deterministic channel
/// ([`crate::signposts`]) — never by the ingest classifier, never by the
/// compiler — and, like [`RULES_FILENAME`], fenced out of every structural
/// sweep ([`is_channel_page`]). Unlike `rules.md` it stays **recallable**
/// and navigable: delivery through ordinary recall is the entire point.
pub const PROJECTS_FILENAME: &str = "projects.md";

/// The owner's reserved **project diary** — one line per project per day,
/// saying what happened.
///
/// Separate from [`PROJECTS_FILENAME`] because the two have opposite
/// lifecycles, and mixing them costs the stronger of the two guarantees:
///
/// | | `projects.md` | this page |
/// |---|---|---|
/// | content | each project's **door sign** | what happened, by day |
/// | origin | *derived* — projected from `smart_wikis.description` | *accumulated* — nothing to derive it from |
/// | if lost | rebuilt by the next sweep | gone |
/// | ages out | no | yes, on a rolling window |
///
/// Keeping them apart is what makes `projects.md` **fully regenerable**:
/// everything on it comes from the registry, so there is nothing on it a
/// buggy writer could destroy that a sweep would not restore. A page that
/// also accumulated events could not make that promise, and its renderer
/// would have to interleave two things that behave in opposite ways.
///
/// Still the *owner's* page, not the project's: a diary is the only
/// cross-project view there is, and «what did I work on this week?» names
/// no project, so an answer living inside each project wiki could not be
/// found by a question that never says which one.
///
/// Written by [`crate::signposts`] alone, and fenced out of the structural
/// sweeps exactly like its sibling ([`is_channel_page`]).
pub const PROJECT_DIARY_FILENAME: &str = "project_diary.md";

/// `wiki_type` of a smart consumer's **operational wiki**.
///
/// That wiki is the consumer's own working memory, one per connection,
/// and never a project. Written by the OAuth consent flow, which also
/// stamps [`WikiMeta::is_agent`] on it — **that** marker, not this label,
/// is what downstream code keys on ("is this an agent's wiki?"), because
/// `wiki_type` is a free-form string any consumer may pass to
/// `wiki_admin_push` and can therefore be claimed by anything. This
/// constant survives as the human-readable label the dashboard shows and
/// as the compatibility fallback for an operational wiki forged before
/// the marker existed.
pub const AGENT_WIKI_TYPE: &str = "agent";

/// The `{subject}` line for an agent's wiki; empty for every other wiki.
///
/// Read by the prompts that WRITE a wiki's index — the hub writer and the
/// compiler's hub pass. Those passes narrate a wiki from the outside by
/// default, which is the voice a human's memory wants. An agent's wiki is its
/// **autobiography**: left on the default voice the same pass files the
/// agent's own memories as a third-party dossier ("l'agente ha aiutato
/// l'utente…") — the agent reads back a report about itself instead of
/// remembering. This lives next to the [`WikiMeta::is_agent`] marker it reads
/// so the two cannot drift apart, and is injected whole so a prompt that does
/// not want it simply omits the placeholder.
#[must_use]
pub const fn subject_directive(meta: &WikiMeta) -> &'static str {
    if meta.is_agent {
        "SUBJECT: this wiki is an AI AGENT's own memory — what you write is a page of ITS \
         autobiography, not a profile someone else keeps on it. Write in the FIRST PERSON \
         (\"sono…\", \"lavoro con…\", in the language named below): who I am, what I do, who I \
         work with and what I have learned. Never a third-person description of the agent, \
         never a log of services rendered."
    } else {
        ""
    }
}

/// True when `source_path` is a wiki's reserved signposts page
/// [`PROJECTS_FILENAME`]. Keyed on the file name, like [`is_rules_page`],
/// so a content page named `my_projects.md` is not caught.
#[must_use]
pub fn is_projects_page(source_path: &str) -> bool {
    std::path::Path::new(source_path)
        .file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(PROJECTS_FILENAME))
}

/// True when `source_path` is the owner's reserved project diary
/// [`PROJECT_DIARY_FILENAME`]. Keyed on the file name, like its siblings.
#[must_use]
pub fn is_project_diary_page(source_path: &str) -> bool {
    std::path::Path::new(source_path)
        .file_name()
        .is_some_and(|n| n == std::ffi::OsStr::new(PROJECT_DIARY_FILENAME))
}

/// True when `source_path` is either half of the signpost channel — the
/// door signs ([`PROJECTS_FILENAME`]) or the diary
/// ([`PROJECT_DIARY_FILENAME`]).
///
/// This is the **delivery-side** predicate: a fact surfacing from either
/// page says *this project is in play*, which is what lets recall offer to
/// open the project's documentation. A diary line is at least as good a
/// signal as a description — «what did I do on X?» surfaces the diary, and
/// the details it is asking for are in the project wiki — so the two pages
/// are equivalent here even though they are written by different paths.
#[must_use]
pub fn is_signpost_page(source_path: &str) -> bool {
    is_projects_page(source_path) || is_project_diary_page(source_path)
}

/// True when `source_path` is one of the reserved **channel pages** —
/// [`RULES_FILENAME`], [`PROJECTS_FILENAME`] or
/// [`PROJECT_DIARY_FILENAME`].
///
/// A channel page's facts are written by a dedicated deterministic path
/// and read back by a dedicated reader, so the engine's structural sweeps
/// must leave them exactly where they are: the compiler never gathers them
/// (they would orphan onto `index.md` and fall out of their channel), the
/// REM refile never nominates them, the contradiction and completion
/// sweeps never use them as satellites or evidence, and dedup pairs never
/// cross the boundary (both sides on a channel page, or neither — else a
/// restatement of a signpost by an ordinary fact would swallow it).
///
/// This is the predicate for *that perimeter only*. The two pages differ
/// on delivery — rules reach the consumer through the `rules` field and
/// never as recalled memory, signposts reach it through recall and nothing
/// else — so delivery-side filters keep keying on [`is_rules_page`].
#[must_use]
pub fn is_channel_page(source_path: &str) -> bool {
    is_rules_page(source_path) || is_signpost_page(source_path)
}

/// Errors raised by the wiki I/O layer.
#[derive(Debug, Error)]
pub enum WikiError {
    /// Underlying filesystem error.
    #[error("wiki io error: {0}")]
    Io(#[from] std::io::Error),

    /// `_meta.md` frontmatter did not parse as YAML, or required fields
    /// were missing / typed wrong.
    #[error("invalid _meta.md frontmatter at {path}: {detail}")]
    InvalidFrontmatter {
        /// Path of the offending `_meta.md`, relative to the workdir.
        path: PathBuf,
        /// Free-form detail (English, no trailing period).
        detail: String,
    },

    /// `_meta.md` body is missing the opening or closing `---` fence.
    #[error("missing frontmatter fence in {path}")]
    MissingFrontmatterFence {
        /// Path of the offending file, relative to the workdir.
        path: PathBuf,
    },

    /// Page path escaped its containing wiki (`..` traversal, absolute
    /// path, or component outside `[A-Za-z0-9._-]`). Rejected before any
    /// filesystem access.
    #[error("page path {path:?} is not safe inside a wiki")]
    UnsafePagePath {
        /// The offending path, as received from the caller.
        path: PathBuf,
    },

    /// A required wiki directory was not found.
    #[error("wiki {id:?} not found at {path}")]
    WikiNotFound {
        /// Wiki id requested.
        id: WikiId,
        /// Expected on-disk path (relative to the workdir).
        path: PathBuf,
    },

    /// A required page file was not found.
    #[error("page {path:?} not found in wiki {wiki:?}")]
    PageNotFound {
        /// Wiki id.
        wiki: WikiId,
        /// Page path within the wiki.
        path: PathBuf,
    },

    /// Slug or id failed validation while loading a `_meta.md`.
    #[error("invalid wiki slug in {path}: {source}")]
    SlugInMeta {
        /// Path of the `_meta.md`, relative to the workdir.
        path: PathBuf,
        /// Underlying validation error.
        #[source]
        source: WikiSlugParseError,
    },

    /// Wiki id failed validation while loading a `_meta.md`.
    #[error("invalid wiki id in {path}: {source}")]
    IdInMeta {
        /// Path of the `_meta.md`.
        path: PathBuf,
        /// Underlying validation error.
        #[source]
        source: WikiIdParseError,
    },

    /// A wiki's `parent_wiki_id` chain to the root identity wiki could not
    /// be resolved within the configured hop cap. Either the chain forms a
    /// cycle (a `wiki_change_scope` bug that re-parented a wiki under one of
    /// its own descendants) or the chain is deeper than the cap suggests is
    /// reasonable.
    #[error("parent chain from {wiki:?} did not terminate within {cap} hops")]
    ScopeChainUnresolved {
        /// Wiki id whose scope principal was being resolved.
        wiki: WikiId,
        /// Hop cap that was exceeded.
        cap: usize,
    },

    /// A typed-wiki create targeted a path that already exists. Creation
    /// is strictly additive — callers route updates through their own
    /// upsert path instead.
    #[error("wiki {id:?} already exists")]
    AlreadyExists {
        /// Wiki id whose `_meta.md` was already present on disk.
        id: WikiId,
    },

    /// A child-only `wiki_type` (the smart family) was asked to
    /// materialise top-level (no
    /// `parent_wiki_id`). Such a wiki must inherit a parent's ACL scope.
    #[error("wiki_type {wiki_type} is child-only and requires a parent wiki")]
    RequiresParent {
        /// The child-only `wiki_type` that refused top-level creation.
        wiki_type: String,
    },
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, WikiError>;

// ---------- Children entries ----------

/// One entry under `_meta.md.children`.
///
/// Maintained by `_internal.wiki_forge`: when a sub-wiki is forged, the
/// parent's `_meta.md` gets a new row here; conversely a delete removes it.
/// The triple is denormalized for read-side convenience — `wiki_id` is
/// recomputable from the parent + `slug` via [`WikiId::child_of`] but
/// recording it on disk lets a tool resolve a child without re-traversing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiChildEntry {
    /// Stable id of the child wiki.
    pub wiki_id: String,
    /// Directory name of the child.
    pub slug: String,
    /// `wiki_type` of the child (e.g. `wiki-user`, `custom-cliente`).
    pub wiki_type: String,
}

// ---------- WikiMeta ----------

/// Parsed `_meta.md` frontmatter.
///
/// The canonical schema is documented in
/// the engine DB and migrations page. All required fields are
/// promoted to typed Rust fields; optional fields default to `None` / empty;
/// every key the canonical schema does *not* know about is preserved
/// verbatim in [`WikiMeta::extra`] so a forge-specific field round-trips
/// through a read + write cycle without loss.
#[derive(Debug, Clone, PartialEq, Eq)]
// The bools here mirror independent `_meta.md` YAML flags (no_archive, smart,
// is_agent) — a flat frontmatter projection, not a state machine, so collapsing
// them into an enum would only obscure the on-disk shape.
#[allow(clippy::struct_excessive_bools)]
pub struct WikiMeta {
    /// Stable id of this wiki. Derived from the path chain at forge time
    /// (`alice-acmecorp-widget-pro`) and never rewritten on rename.
    pub wiki_id: WikiId,
    /// Reference to a bundled or forged `wiki_type` definition.
    pub wiki_type: String,
    /// Parent's `wiki_id`. `None` for the wiki-root (the top of a
    /// memory-wiki tree).
    pub parent_wiki_id: Option<WikiId>,
    /// Directory name of this wiki.
    pub slug: WikiSlug,
    /// Display name (free-form unicode).
    pub title: String,
    /// Prose description of the wiki's **category** — "what goes in here" —
    /// read by the ingest/document classifier as a **placement signal**
    /// (never an ACL gate). A group wiki inherits the group's `scope`
    /// prose; an emerged sub-wiki gets prose the LLM writes at creation.
    /// `None` for the many wikis that carry no description yet. The wiki's
    /// owning **principal** (whose category it is) is no longer declared
    /// here — it is **derived from topology** via
    /// [`WikiTree::resolve_scope_principal`].
    pub scope: Option<String>,
    /// Optional sharing roster for smart-wikis.
    ///
    /// When non-empty, the listed principals get **read + notify** access
    /// to the wiki on top of the owner: they can call `wiki_search` /
    /// `wiki_read` against it, and can append items to `_briefing.md`
    /// via `wiki_admin_notify`. Write tools (`wiki_admin_push` /
    /// `wiki_admin_pull`) stay owner-only — the invariant
    /// `wiki.owner_user == token.owner_user` is **preserved**, the
    /// share only extends the read/notify perimeter.
    ///
    /// Empty for the vast majority of wikis. The dashboard manages this
    /// roster via the `/wikis/<id>/sharing` route.
    pub shared_with: Vec<Principal>,
    /// Forge-time `wiki_type` overrides (free-form YAML object).
    pub style_overrides: serde_yaml::Mapping,
    /// `wiki_type`-specific search keys (free-form YAML object).
    pub keywords: serde_yaml::Mapping,
    /// Children index maintained by `wiki_forge`. May be empty.
    pub children: Vec<WikiChildEntry>,
    /// Origin path for wikis born from auto-promotion.
    pub promoted_from: Option<String>,
    /// Opt-out from archive automation (default `false`).
    pub no_archive: bool,
    /// `true` when this is a **smart wiki** — a container a smart
    /// consumer owns and maintains via `wiki_admin_*` (never written by
    /// `wiki_ingest_message`); `false` for the standard-wiki family the
    /// narrative compiler authors. On disk the key is `smart:`, with
    /// `companion:` accepted forever as the legacy read alias (the
    /// family's pre-rename name); writes emit `smart:`. This per-wiki
    /// flag is the canonical family marker that replaced the retired
    /// `wiki_types_registry` lookup: read by the smart-family gates,
    /// stamped at actor-wiki creation.
    /// Defaults to `false`. Note: **per-fact** axes (validity, ACL,
    /// `topics`) are never here — they live in `fact_index` / the page
    /// frontmatter. The wiki-level **style default** + "what goes in here"
    /// description *do* live on `_meta` (in [`Self::extra`] under `style`
    /// / `summary`), but only as a **hint, not a gate** for homogeneous /
    /// semi-homogeneous wikis — per-page style still wins when a page
    /// deviates (stamped at emergence by
    /// `promote::apply_file_to_subwiki`).
    pub smart: bool,
    /// `true` when this `wiki-user` is a **consumer agent's own** identity
    /// wiki — the credential-less system user a standard consumer is bound to
    /// (the diagonal-identity model), not a human's. A self-describing mirror of
    /// the authoritative `consumers.system_user_id` binding, stamped at agent-wiki
    /// creation ([`IdentityKind::Agent`]) and backfilled on bot-token mint, so an
    /// agent wiki announces itself without a DB lookup — the operator sees it in
    /// Obsidian, and the dashboard / REM can spot it from the `_meta.md` alone.
    /// Round-tripped only when set (the vast majority of wikis stay lean).
    /// Defaults to `false`. SSOT for "is this an agent?" stays the binding; this
    /// is the cache. See `roadmap` item 27d / 4i.
    pub is_agent: bool,
    /// Wall-clock creation time. ISO 8601 string preserved verbatim so
    /// the wire format is stable even across `chrono` revisions.
    pub created: Option<String>,
    /// Wall-clock last-meaningful-modification time. Same string-preservation
    /// rationale as [`Self::created`].
    pub updated: Option<String>,
    /// Any frontmatter keys not listed above. Read back as-is and written
    /// back at the end of the YAML map so forges can carry extra fields.
    pub extra: serde_yaml::Mapping,
}

impl WikiMeta {
    /// The one authored line that makes this wiki reachable from a turn
    /// that does not name it — its **door sign**, mirrored into
    /// `smart_wikis.description` and from there into the owner's signpost
    /// page.
    ///
    /// Read from [`Self::scope`], which already means *"prose description
    /// of this container — what goes in here"*. On a standard wiki that
    /// prose is a placement signal for the classifier; a smart wiki is
    /// never a placement target (it is filtered out of the router window),
    /// so the field is free here and the two readings cannot collide.
    /// Reusing it is deliberate: a near-synonym field would be one more
    /// thing to keep in step for no gain in meaning.
    ///
    /// `None` — the honest answer for an undescribed wiki — in three
    /// cases, and the last two are the point:
    ///
    /// 1. no `scope`, or only whitespace;
    /// 2. **an agent's operational wiki.** It is a smart wiki too, but it
    ///    holds one agent's working notes rather than a subject anyone
    ///    would ask about, so it is nobody's door.
    ///    [`crate::signposts::status`] already declines to nudge these; the
    ///    same two markers are checked here, so a stray `scope` on an agent
    ///    wiki cannot quietly become one.
    /// 3. a **standard** wiki — its `scope` is the classifier's placement
    ///    signal and nothing to do with doors.
    #[must_use]
    pub fn door_description(&self) -> Option<String> {
        if !self.smart || self.is_agent || self.wiki_type == AGENT_WIKI_TYPE {
            return None;
        }
        self.scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// Parse a `_meta.md` body (frontmatter + optional prose body).
    ///
    /// `path` is informational — surfaced inside errors so the caller can
    /// point an operator at the offending file.
    ///
    /// # Errors
    ///
    /// - [`WikiError::MissingFrontmatterFence`] if the body does not start
    ///   with a `---` fence and contain a closing `---`.
    /// - [`WikiError::InvalidFrontmatter`] for YAML parse errors or
    ///   missing / mistyped required fields.
    pub fn parse(path: &Path, raw: &str) -> Result<(Self, String)> {
        let MarkdownDoc { frontmatter, body } =
            MarkdownDoc::parse(raw).ok_or_else(|| WikiError::MissingFrontmatterFence {
                path: path.to_path_buf(),
            })?;
        let meta = Self::from_frontmatter_str(path, &frontmatter)?;
        Ok((meta, body))
    }

    fn from_frontmatter_str(path: &Path, yaml: &str) -> Result<Self> {
        let raw: serde_yaml::Mapping =
            serde_yaml::from_str(yaml).map_err(|e| WikiError::InvalidFrontmatter {
                path: path.to_path_buf(),
                detail: format!("yaml: {e}"),
            })?;
        Self::from_mapping(path, raw)
    }

    #[allow(clippy::too_many_lines)]
    fn from_mapping(path: &Path, mut raw: serde_yaml::Mapping) -> Result<Self> {
        fn take<T: for<'de> Deserialize<'de>>(
            path: &Path,
            raw: &mut serde_yaml::Mapping,
            key: &str,
        ) -> Result<Option<T>> {
            let Some(v) = raw.remove(serde_yaml::Value::String(key.to_owned())) else {
                return Ok(None);
            };
            serde_yaml::from_value::<T>(v)
                .map(Some)
                .map_err(|e| WikiError::InvalidFrontmatter {
                    path: path.to_path_buf(),
                    detail: format!("{key}: {e}"),
                })
        }
        fn take_required<T: for<'de> Deserialize<'de>>(
            path: &Path,
            raw: &mut serde_yaml::Mapping,
            key: &str,
        ) -> Result<T> {
            take(path, raw, key)?.ok_or_else(|| WikiError::InvalidFrontmatter {
                path: path.to_path_buf(),
                detail: format!("missing required field `{key}`"),
            })
        }
        // Parse an optional YAML list of
        // `[user:..., group:..., user:..., ...]` into typed
        // `Vec<Principal>`. Each entry surfaces a `{key}[{idx}]: ...`
        // error so a typo points at the exact list slot.
        fn take_principal_list(
            path: &Path,
            raw: &mut serde_yaml::Mapping,
            key: &str,
        ) -> Result<Vec<Principal>> {
            let rows: Option<Vec<String>> = take(path, raw, key)?;
            let Some(rows) = rows else {
                return Ok(Vec::new());
            };
            let mut out = Vec::with_capacity(rows.len());
            for (idx, s) in rows.iter().enumerate() {
                let p = s
                    .parse::<Principal>()
                    .map_err(|e| WikiError::InvalidFrontmatter {
                        path: path.to_path_buf(),
                        detail: format!("{key}[{idx}] {s:?}: {e}"),
                    })?;
                out.push(p);
            }
            Ok(out)
        }

        let wiki_id_s: String = take_required(path, &mut raw, "wiki_id")?;
        let wiki_id = WikiId::parse(&wiki_id_s).map_err(|source| WikiError::IdInMeta {
            path: path.to_path_buf(),
            source,
        })?;

        let wiki_type: String = take_required(path, &mut raw, "wiki_type")?;

        let parent_wiki_id_raw: Option<serde_yaml::Value> = take(path, &mut raw, "parent_wiki_id")?;
        let parent_wiki_id = match parent_wiki_id_raw {
            None | Some(serde_yaml::Value::Null) => None,
            Some(serde_yaml::Value::String(s)) => {
                Some(WikiId::parse(&s).map_err(|source| WikiError::IdInMeta {
                    path: path.to_path_buf(),
                    source,
                })?)
            },
            Some(other) => {
                return Err(WikiError::InvalidFrontmatter {
                    path: path.to_path_buf(),
                    detail: format!(
                        "parent_wiki_id: expected string or null, got {}",
                        type_name_of(&other)
                    ),
                });
            },
        };

        let slug_s: String = take_required(path, &mut raw, "slug")?;
        let slug = WikiSlug::parse(&slug_s).map_err(|source| WikiError::SlugInMeta {
            path: path.to_path_buf(),
            source,
        })?;

        let title: String = take_required(path, &mut raw, "title")?;

        // `acl_default` is retired: the owning principal is now **derived**
        // from topology ([`WikiTree::resolve_scope_principal`]), not
        // declared. Read-and-ignore it so existing `_meta.md` files still
        // parse; it is never emitted by `to_yaml`, so it drops on the next
        // rewrite (the `companion`→`smart` legacy-alias spirit).
        let _ignored_acl_default: Option<serde_yaml::Value> = take(path, &mut raw, "acl_default")?;

        // The category's prose description (placement signal, never an ACL
        // gate). Optional and round-tripped only when present.
        let scope: Option<String> = take(path, &mut raw, "scope")?;

        // Optional `shared_with: [user:bob, group:lnprint-devs]`.
        // Parsed as `Vec<String>` for tolerant error reporting (we want a
        // bad entry to fail loudly with "shared_with[2]: invalid …"
        // instead of the generic serde mismatch), then each entry is
        // resolved through `Principal::from_str`. Empty list ⇒ no sharing.
        let shared_with = take_principal_list(path, &mut raw, "shared_with")?;

        let style_overrides: serde_yaml::Mapping =
            take(path, &mut raw, "style_overrides")?.unwrap_or_default();
        let keywords: serde_yaml::Mapping = take(path, &mut raw, "keywords")?.unwrap_or_default();
        let children: Vec<WikiChildEntry> = take(path, &mut raw, "children")?.unwrap_or_default();

        // Validate children entries individually now so a corrupt child row
        // never escapes into the rest of the system unnoticed.
        for c in &children {
            WikiId::parse(&c.wiki_id).map_err(|source| WikiError::IdInMeta {
                path: path.to_path_buf(),
                source,
            })?;
            WikiSlug::parse(&c.slug).map_err(|source| WikiError::SlugInMeta {
                path: path.to_path_buf(),
                source,
            })?;
        }

        let promoted_from: Option<String> = take(path, &mut raw, "promoted_from")?;
        let no_archive: bool = take(path, &mut raw, "no_archive")?.unwrap_or(false);
        // `smart:` is the canonical key; `companion:` is the legacy
        // alias from before the family rename and stays a valid read
        // forever (existing workdirs keep working; writes emit `smart`).
        let smart_key: Option<bool> = take(path, &mut raw, "smart")?;
        let companion_alias: Option<bool> = take(path, &mut raw, "companion")?;
        let smart: bool = smart_key.or(companion_alias).unwrap_or(false);
        let is_agent: bool = take(path, &mut raw, "is_agent")?.unwrap_or(false);
        let created: Option<String> = take_iso_date(path, &mut raw, "created")?;
        let updated: Option<String> = take_iso_date(path, &mut raw, "updated")?;

        Ok(Self {
            wiki_id,
            wiki_type,
            parent_wiki_id,
            slug,
            title,
            scope,
            shared_with,
            style_overrides,
            keywords,
            children,
            promoted_from,
            no_archive,
            smart,
            is_agent,
            created,
            updated,
            extra: raw,
        })
    }

    /// Serialize back to the canonical YAML frontmatter string (without
    /// `---` fences).
    ///
    /// Field order is fixed (`wiki_id`, `wiki_type`, `parent_wiki_id`,
    /// `slug`, `title`, then optionals in spec order — `scope` first —
    /// then [`WikiMeta::extra`]) so a `parse` → `to_yaml` round-trip
    /// keeps diffs minimal for files that did not originally carry
    /// unknown keys. The retired `acl_default` is never re-emitted.
    ///
    /// # Errors
    ///
    /// Surfaces underlying `serde_yaml` failures (in practice: only on
    /// values that contain non-string mapping keys, which should not
    /// occur for our schema).
    pub fn to_yaml(&self) -> std::result::Result<String, serde_yaml::Error> {
        let mut out = serde_yaml::Mapping::new();
        out.insert(yk("wiki_id"), yv(self.wiki_id.as_str()));
        out.insert(yk("wiki_type"), yv(&self.wiki_type));
        out.insert(
            yk("parent_wiki_id"),
            self.parent_wiki_id
                .as_ref()
                .map_or(serde_yaml::Value::Null, |id| yv(id.as_str())),
        );
        out.insert(yk("slug"), yv(self.slug.as_str()));
        out.insert(yk("title"), yv(&self.title));

        // Optional prose category description. Round-tripped only when
        // present so the vast majority of `_meta.md` stay lean.
        if let Some(scope) = &self.scope {
            out.insert(yk("scope"), yv(scope));
        }

        if !self.shared_with.is_empty() {
            let entries: Vec<serde_yaml::Value> = self
                .shared_with
                .iter()
                .map(|p| yv(&p.to_string()))
                .collect();
            out.insert(yk("shared_with"), serde_yaml::Value::Sequence(entries));
        }

        if !self.style_overrides.is_empty() {
            out.insert(
                yk("style_overrides"),
                serde_yaml::Value::Mapping(self.style_overrides.clone()),
            );
        }
        if !self.keywords.is_empty() {
            out.insert(
                yk("keywords"),
                serde_yaml::Value::Mapping(self.keywords.clone()),
            );
        }
        if !self.children.is_empty() {
            out.insert(yk("children"), serde_yaml::to_value(&self.children)?);
        }
        if let Some(p) = &self.promoted_from {
            out.insert(yk("promoted_from"), yv(p));
        }
        if self.no_archive {
            out.insert(yk("no_archive"), serde_yaml::Value::Bool(true));
        }
        // Smart is the rare case (smart-consumer-owned wikis only);
        // round-trip only when set so the vast majority of `_meta.md`
        // stay lean. Always written under the canonical `smart:` key —
        // a file read through the `companion:` alias migrates on its
        // first rewrite.
        if self.smart {
            out.insert(yk("smart"), serde_yaml::Value::Bool(true));
        }
        // Agent-wiki self-description (roadmap 27d / 4i): round-trip only when
        // set so ordinary `_meta.md` stay lean.
        if self.is_agent {
            out.insert(yk("is_agent"), serde_yaml::Value::Bool(true));
        }
        if let Some(c) = &self.created {
            out.insert(yk("created"), yv(c));
        }
        if let Some(u) = &self.updated {
            out.insert(yk("updated"), yv(u));
        }
        for (k, v) in &self.extra {
            out.insert(k.clone(), v.clone());
        }
        serde_yaml::to_string(&serde_yaml::Value::Mapping(out))
    }

    /// Render a full `_meta.md` document (frontmatter + optional body).
    ///
    /// # Errors
    ///
    /// Same surface as [`WikiMeta::to_yaml`].
    pub fn render(&self, body: &str) -> std::result::Result<String, serde_yaml::Error> {
        let yaml = self.to_yaml()?;
        Ok(format!("---\n{yaml}---\n{body}"))
    }
}

fn yk(s: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(s.to_owned())
}
fn yv(s: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(s.to_owned())
}

const fn type_name_of(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "sequence",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

/// Accept either a YAML date (which `serde_yaml` may surface as a
/// `String` `"2026-05-12"` or as a typed value depending on quoting) or
/// an ISO 8601 timestamp string. Either way, we round-trip a string.
fn take_iso_date(path: &Path, raw: &mut serde_yaml::Mapping, key: &str) -> Result<Option<String>> {
    let Some(v) = raw.remove(serde_yaml::Value::String(key.to_owned())) else {
        return Ok(None);
    };
    match v {
        serde_yaml::Value::Null => Ok(None),
        serde_yaml::Value::String(s) => Ok(Some(s)),
        // YAML date literals (without quotes) come through serde_yaml as
        // strings already, so this branch is a defensive fallback only.
        other => serde_yaml::to_string(&other)
            .map(|s| Some(s.trim().to_owned()))
            .map_err(|e| WikiError::InvalidFrontmatter {
                path: path.to_path_buf(),
                detail: format!("{key}: {e}"),
            }),
    }
}

// ---------- MarkdownDoc ----------

/// A markdown document split into optional YAML frontmatter + body.
///
/// Returned by [`MarkdownDoc::parse`]. `body` always ends with the same
/// newlines the input carried — we never strip trailing whitespace, so a
/// parse → render round-trip is byte-identical (modulo the frontmatter
/// being re-serialized when the caller chooses to do so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDoc {
    /// Raw YAML text between the `---` fences (no fences included). May be
    /// empty if the input had `---\n---\n` with nothing between.
    pub frontmatter: String,
    /// Markdown body after the closing fence.
    pub body: String,
}

impl MarkdownDoc {
    /// Parse a file body that *must* begin with a YAML frontmatter fence.
    ///
    /// Returns `None` if the body does not have a frontmatter at all (no
    /// leading `---` line) or if the opening fence is never closed. The
    /// caller decides whether absence is fatal: for `_meta.md` it is,
    /// for leaf prose pages it is fine.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        // Opening fence must be the very first line and be literally `---`
        // (possibly followed by `\r`).
        let rest = strip_fence_line(raw)?;
        // Walk line by line. The closing fence is a line that is literally
        // `---` (CRLF tolerated). The first such line ends the frontmatter.
        // Tracking start-of-line keeps us O(n) and avoids the edge case
        // where the closing fence is on the very first line (empty
        // frontmatter), which a `find("\n---")` search would miss.
        let mut line_start = 0usize;
        loop {
            let line_end = rest[line_start..]
                .find('\n')
                .map_or(rest.len(), |i| line_start + i);
            let line = &rest[line_start..line_end];
            let line_clean = line.trim_end_matches('\r');
            if line_clean == "---" {
                // Frontmatter is everything up to (but not including) the
                // newline that precedes this line. For an empty frontmatter
                // (closing fence on the first line), `line_start == 0` and
                // the slice is empty.
                let fm_end = line_start.saturating_sub(1);
                let frontmatter = if line_start == 0 {
                    ""
                } else {
                    rest[..fm_end].trim_end_matches('\r')
                };
                let body_start = if line_end < rest.len() {
                    line_end + 1
                } else {
                    rest.len()
                };
                return Some(Self {
                    frontmatter: frontmatter.to_owned(),
                    body: rest[body_start..].to_owned(),
                });
            }
            if line_end >= rest.len() {
                return None;
            }
            line_start = line_end + 1;
        }
    }
}

fn strip_fence_line(raw: &str) -> Option<&str> {
    // Match `---` then a newline (with optional `\r`).
    if let Some(rest) = raw.strip_prefix("---\n") {
        return Some(rest);
    }
    if let Some(rest) = raw.strip_prefix("---\r\n") {
        return Some(rest);
    }
    None
}

// ---------- Atomic write ----------

/// Atomically write `bytes` to `target` while honouring the marker
/// protocol (see module docstring).
///
/// Steps:
/// 1. Acquire a [`WriteMarker`] guard on `target`.
/// 2. Create a `NamedTempFile` in the parent directory.
/// 3. Write `bytes`, then `sync_data` the file.
/// 4. `persist` it onto `target` (atomic rename within the same fs).
/// 5. `fsync` the parent directory so the rename survives a crash.
///
/// # Errors
///
/// - [`WikiError::Io`] on any IO failure. The temp file is cleaned up
///   automatically; if `persist` failed the original target is left
///   untouched.
pub fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"))?;
    fs::create_dir_all(parent)?;

    let _marker = WriteMarker::acquire(target).map_err(|e| match e {
        crate::watcher::WatcherError::MarkerIo(io) => WikiError::Io(io),
        crate::watcher::WatcherError::Notify(n) => {
            WikiError::Io(std::io::Error::other(n.to_string()))
        },
    })?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file_mut().sync_data()?;
    tmp.persist(target).map_err(|e| WikiError::Io(e.error))?;

    // Best-effort: fsync the directory. On Windows this is a no-op (the
    // OS does not expose directory fsync), so the fallback is to leave
    // the entry's durability to the filesystem journal.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    tracing::debug!(
        target = %target.display(),
        bytes = bytes.len(),
        "wiki: atomic_write done"
    );
    Ok(())
}

// ---------- WikiTree ----------

/// Read-only handle on the entire memory-wiki tree under a workdir.
///
/// A `WikiTree` is cheap to construct (just stores the resolved paths) and
/// does not eagerly traverse the filesystem. It is `Send + Sync` so the
/// `mwe-mcp serve` process can clone the handle into per-request tasks.
#[derive(Debug, Clone)]
pub struct WikiTree {
    workdir: PathBuf,
    wikis_dir: PathBuf,
}

impl WikiTree {
    /// Open the tree at `<workdir>/wikis/`.
    ///
    /// Creates `<workdir>/wikis/` if it does not yet exist, so a fresh
    /// workdir can host its first wiki without a separate `mkdir -p`.
    /// The stored workdir is the operator-supplied path verbatim — no
    /// canonicalisation. Cross-platform path-prefix matching (`FSEvents`
    /// `/private/var/...`, Windows short-name `RUNNER~1`) is handled
    /// inside the reindex helpers so this constructor stays a
    /// no-surprises mirror of the input.
    ///
    /// # Errors
    ///
    /// Surfaces IO errors creating the directory.
    pub fn open(workdir: &Path) -> Result<Self> {
        let wikis_dir = workdir.join(WIKIS_DIR);
        fs::create_dir_all(&wikis_dir)?;
        Ok(Self {
            workdir: workdir.to_path_buf(),
            wikis_dir,
        })
    }

    /// Path of the workdir this tree lives inside.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Path of `<workdir>/wikis/`.
    #[must_use]
    pub fn wikis_dir(&self) -> &Path {
        &self.wikis_dir
    }

    /// Enumerate every wiki node anywhere in the tree, depth-first, with
    /// the absolute directory and its parsed [`WikiMeta`]. Errors on
    /// individual `_meta.md` files surface; the caller decides whether to
    /// stop or skip.
    ///
    /// # Errors
    ///
    /// IO errors during traversal and [`WikiMeta::parse`] errors on any
    /// encountered `_meta.md`.
    pub fn walk(&self) -> Result<Vec<DiscoveredWiki>> {
        let mut out = Vec::new();
        if !self.wikis_dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.wikis_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk_node(&self.wikis_dir, &entry.path(), &mut out)?;
            }
        }
        // Determinism: sort by relative path so callers can rely on the
        // order without re-sorting.
        out.sort_by(|a, b| a.rel_dir.cmp(&b.rel_dir));
        Ok(out)
    }

    /// Locate a wiki by its [`WikiId`].
    ///
    /// Implementation walks the tree and matches on the `wiki_id` stored
    /// inside each `_meta.md`. That is O(tree size) but the trees we
    /// target are typically <500 nodes so a
    /// linear scan is fine for now. The lookup can be replaced by an
    /// in-memory index later without breaking the API.
    ///
    /// # Errors
    ///
    /// [`WikiError::WikiNotFound`] when no `_meta.md` matches; other
    /// surface as during [`Self::walk`].
    pub fn locate(&self, id: &WikiId) -> Result<WikiHandle> {
        for d in self.walk()? {
            if &d.meta.wiki_id == id {
                return Ok(WikiHandle {
                    workdir: self.workdir.clone(),
                    rel_dir: d.rel_dir,
                    abs_dir: d.abs_dir,
                    meta: d.meta,
                });
            }
        }
        Err(WikiError::WikiNotFound {
            id: id.clone(),
            path: PathBuf::new(),
        })
    }

    /// Resolve the **scope principal** of a wiki — the principal whose
    /// category this wiki is — from topology.
    ///
    /// A wiki is a *category*, not an owner: its principal is no longer
    /// declared in `_meta.md` but **derived** by following `parent_wiki_id`
    /// up to the root wiki (`parent_wiki_id == None`). The root is an
    /// identity wiki, so its `wiki_type` and id give the principal:
    /// [`IDENTITY_WIKI_TYPE`] (`wiki-user`) → `Principal::User(root_id)`,
    /// [`GROUP_IDENTITY_WIKI_TYPE`] (`wiki-group`) → `Principal::Group(root_id)`.
    /// Any other root type (or a tree whose root is not an identity wiki)
    /// surfaces a [`WikiError::InvalidFrontmatter`].
    ///
    /// A [`WikiError::ScopeChainUnresolved`] is raised if the parent chain
    /// exceeds [`MAX_ACL_DEFAULT_HOPS`] hops or forms a cycle (defensive
    /// against a `wiki_change_scope` bug).
    pub fn resolve_scope_principal(&self, meta: &WikiMeta) -> Result<Principal> {
        let mut current = meta.clone();
        let mut seen = std::collections::HashSet::new();
        seen.insert(current.wiki_id.clone());
        for _ in 0..MAX_ACL_DEFAULT_HOPS {
            let Some(parent_id) = current.parent_wiki_id.clone() else {
                // Reached the root: its type + id name the principal.
                return scope_principal_of_root(&current);
            };
            if !seen.insert(parent_id.clone()) {
                return Err(WikiError::ScopeChainUnresolved {
                    wiki: meta.wiki_id.clone(),
                    cap: MAX_ACL_DEFAULT_HOPS,
                });
            }
            let parent = self.locate(&parent_id)?;
            current = parent.meta.clone();
        }
        Err(WikiError::ScopeChainUnresolved {
            wiki: meta.wiki_id.clone(),
            cap: MAX_ACL_DEFAULT_HOPS,
        })
    }
}

/// Map a **root** wiki's identity type to its scope principal.
///
/// `wiki-user` → `Principal::User(id)`, `wiki-group` → `Principal::Group(id)`.
/// A root of any other type cannot name a principal — the caller's tree is
/// malformed (a non-identity wiki sitting at the top).
fn scope_principal_of_root(root: &WikiMeta) -> Result<Principal> {
    let id = root.wiki_id.as_str().to_owned();
    match root.wiki_type.as_str() {
        IDENTITY_WIKI_TYPE => Ok(Principal::User(id)),
        GROUP_IDENTITY_WIKI_TYPE => Ok(Principal::Group(id)),
        other => Err(WikiError::InvalidFrontmatter {
            path: PathBuf::from(format!("{id}/{META_FILENAME}")),
            detail: format!(
                "root wiki `{id}` has wiki_type `{other}`, not an identity \
                 type ({IDENTITY_WIKI_TYPE} / {GROUP_IDENTITY_WIKI_TYPE}); \
                 cannot derive a scope principal"
            ),
        }),
    }
}

/// Cap on parent-chain hops [`WikiTree::resolve_scope_principal`] follows.
///
/// Real trees stay shallow (root → user → cliente → progetto is already
/// deep at 4), so 64 is comfortably permissive while still detecting
/// cycles introduced by a misbehaving `wiki_change_scope`.
pub const MAX_ACL_DEFAULT_HOPS: usize = 64;

/// One element of [`WikiTree::walk`].
#[derive(Debug, Clone)]
pub struct DiscoveredWiki {
    /// Absolute directory of the wiki.
    pub abs_dir: PathBuf,
    /// Directory relative to the workdir (e.g. `wikis/alice/acmecorp`).
    pub rel_dir: PathBuf,
    /// Parsed `_meta.md`.
    pub meta: WikiMeta,
}

fn walk_node(wikis_dir: &Path, abs_dir: &Path, out: &mut Vec<DiscoveredWiki>) -> Result<()> {
    let meta_path = abs_dir.join(META_FILENAME);
    if meta_path.exists() {
        let raw = fs::read_to_string(&meta_path)?;
        let (meta, _body) = WikiMeta::parse(&meta_path, &raw)?;
        let rel_dir = abs_dir
            .strip_prefix(wikis_dir.parent().unwrap_or(wikis_dir))
            .map_or_else(|_| abs_dir.to_path_buf(), Path::to_path_buf);
        out.push(DiscoveredWiki {
            abs_dir: abs_dir.to_path_buf(),
            rel_dir,
            meta,
        });
    }
    for entry in fs::read_dir(abs_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            walk_node(wikis_dir, &entry.path(), out)?;
        }
    }
    Ok(())
}

// ---------- WikiHandle ----------

/// Mutable handle on a single wiki node.
///
/// Carries the resolved paths and a cached [`WikiMeta`]; the meta cache is
/// read at `locate` time and the caller is expected to re-`locate` after
/// any meta mutation if it cares about freshness.
#[derive(Debug, Clone)]
pub struct WikiHandle {
    workdir: PathBuf,
    rel_dir: PathBuf,
    abs_dir: PathBuf,
    meta: WikiMeta,
}

impl WikiHandle {
    /// Directory of this wiki, absolute on the host fs.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const yet
    pub fn abs_dir(&self) -> &Path {
        self.abs_dir.as_path()
    }

    /// Directory of this wiki, relative to the workdir.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const yet
    pub fn rel_dir(&self) -> &Path {
        self.rel_dir.as_path()
    }

    /// Workdir this wiki lives inside.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const yet
    pub fn workdir(&self) -> &Path {
        self.workdir.as_path()
    }

    /// Cached `_meta.md` content (parsed at locate time).
    #[must_use]
    pub const fn meta(&self) -> &WikiMeta {
        &self.meta
    }

    /// Read a page by its path relative to the wiki directory
    /// (`"index.md"`, `"recipes/pasta.md"`, …). Returns the raw bytes; no
    /// frontmatter / marker parsing happens here.
    ///
    /// # Errors
    ///
    /// - [`WikiError::UnsafePagePath`] if `page` escapes the wiki dir.
    /// - [`WikiError::PageNotFound`] if the file does not exist.
    /// - [`WikiError::Io`] for any other read failure.
    pub fn read_page(&self, page: &Path) -> Result<String> {
        let abs = self.resolve_page(page)?;
        match fs::read_to_string(&abs) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(WikiError::PageNotFound {
                wiki: self.meta.wiki_id.clone(),
                path: page.to_path_buf(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically write a page within this wiki. `page` is relative to the
    /// wiki directory. Sub-directories are created as needed.
    ///
    /// # Errors
    ///
    /// - [`WikiError::UnsafePagePath`] when the path is rejected by
    ///   [`is_safe_page_path`].
    /// - [`WikiError::Io`] on filesystem errors.
    pub fn write_page(&self, page: &Path, contents: &str) -> Result<()> {
        let abs = self.resolve_page(page)?;
        atomic_write(&abs, contents.as_bytes())
    }

    /// Enumerate every `.md` file under this wiki's directory, *excluding*
    /// `_meta.md` and *excluding* files that live inside a sub-wiki (i.e.
    /// a directory containing a `_meta.md`). Sub-wikis are themselves
    /// other [`WikiHandle`]s — pages of those are queried via the
    /// sub-wiki's handle, not this one.
    ///
    /// # Errors
    ///
    /// IO errors during traversal.
    pub fn list_pages(&self) -> Result<Vec<PageInfo>> {
        list_wiki_pages(&self.abs_dir)
    }

    fn resolve_page(&self, page: &Path) -> Result<PathBuf> {
        if !is_safe_page_path(page) {
            return Err(WikiError::UnsafePagePath {
                path: page.to_path_buf(),
            });
        }
        Ok(self.abs_dir.join(page))
    }
}

/// One row of [`WikiHandle::list_pages`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    /// File path relative to the wiki directory (e.g. `recipes/pasta.md`).
    pub rel_path: PathBuf,
    /// Absolute file path.
    pub abs_path: PathBuf,
    /// Byte size of the file at enumeration time.
    pub size: u64,
}

/// Enumerate the pages of the wiki rooted at `abs_dir` — the function behind
/// [`WikiHandle::list_pages`], callable directly from a [`DiscoveredWiki`]'s
/// `abs_dir` so a caller already holding a [`WikiTree::walk`] result (e.g. the
/// recall-navigation entry-point gatherer) does not pay a second tree walk
/// per wiki for a `locate`.
///
/// # Errors
///
/// IO errors during traversal.
pub(crate) fn list_wiki_pages(abs_dir: &Path) -> Result<Vec<PageInfo>> {
    let mut out = Vec::new();
    list_pages_inner(abs_dir, abs_dir, &mut out)?;
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

fn list_pages_inner(wiki_root: &Path, cur: &Path, out: &mut Vec<PageInfo>) -> Result<()> {
    for entry in fs::read_dir(cur)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_dir() {
            // A nested directory belongs to this wiki only if it does NOT
            // itself carry a `_meta.md`. With `_meta.md` it is a sub-wiki
            // and its pages live under the sub-wiki's own handle.
            if path.join(META_FILENAME).exists() {
                continue;
            }
            list_pages_inner(wiki_root, &path, out)?;
        } else if ft.is_file() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == META_FILENAME || name == CAPTURES_FILENAME {
                continue;
            }
            // Case-insensitive `.md` filter so `INTRO.MD` from an Obsidian
            // import is still enumerated.
            if !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                continue;
            }
            let rel = path
                .strip_prefix(wiki_root)
                .map_or_else(|_| path.clone(), Path::to_path_buf);
            let size = entry.metadata().map_or(0, |m| m.len());
            out.push(PageInfo {
                rel_path: rel,
                abs_path: path,
                size,
            });
        }
    }
    Ok(())
}

/// Convert an absolute path under the workdir to the canonical
/// **POSIX-style** `source_path` we store in `fact_index.source_path` and
/// emit through `wiki_events` payloads.
///
/// Returns the workdir-relative path with `/` separators on every
/// platform — Windows `to_string_lossy()` would otherwise produce
/// `wikis\alice\intro.md`, which then fails to match the on-disk lint
/// scan + capture pipeline expectations.
///
/// When `abs_path` is not under `workdir` we fall back to the lossy
/// rendering of the full path; the caller should treat that as a
/// programming error.
#[must_use]
pub fn workdir_relative_source_path(workdir: &Path, abs_path: &Path) -> String {
    let rel = abs_path.strip_prefix(workdir).unwrap_or(abs_path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Reject paths that try to leave the wiki directory.
///
/// Accepts:
/// - one or more components, each `[A-Za-z0-9._-]+` (so `.md`, `index.md`,
///   `recipes/pasta-al-pomodoro.md`, `Setup.md` all work — smart wikis
///   imported from a local vault keep their original casing byte-for-byte)
/// - no leading separator
/// - no `..` or `.` components
///
/// This is intentionally stricter than the OS-level traversal check — we
/// also want a stable, Obsidian-friendly charset for the on-disk filenames
/// so the file watcher and re-index pipeline never have to escape weird
/// codepoints in queries.
///
/// Case is accepted but **collisions are not**: the page-creation paths
/// pair this check with [`page_path_case_hazard`] +
/// [`page_case_conflict`] so two paths differing only by ASCII case can
/// never coexist — they would be the SAME file on a smart consumer's
/// case-insensitive local mirror (Windows/macOS Obsidian).
#[must_use]
pub fn is_safe_page_path(p: &Path) -> bool {
    if p.is_absolute() {
        return false;
    }
    let mut count = 0;
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(s) => {
                count += 1;
                let Some(name) = s.to_str() else {
                    return false;
                };
                if name.is_empty() {
                    return false;
                }
                let all_ok = name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
                if !all_ok {
                    return false;
                }
                if name == "." || name == ".." {
                    return false;
                }
                if name.starts_with('.') && name != ".md" {
                    // hidden files (other than the literal ".md" filename which
                    // is silly but legal) are reserved for tooling like
                    // `.mwe-write-in-progress` markers.
                    return false;
                }
            },
            _ => return false,
        }
    }
    count > 0
}

/// Case hazards a page path carries on its own, independent of the disk.
///
/// Two shapes: a component that is an ASCII-case variant of a reserved
/// filename ([`META_FILENAME`], [`RULES_FILENAME`],
/// [`crate::briefing::BRIEFING_FILENAME`]), or a `.md` extension not
/// spelled in lowercase.
///
/// Both would be accepted by [`is_safe_page_path`] yet misbehave later:
/// `_Meta.md` is a normal page to the server but the same file as
/// `_meta.md` on a case-insensitive mirror, and `notes.MD` breaks the
/// link grammar — the `[[wiki/slug]]` convention strips and re-appends
/// a byte-exact lowercase `.md` (see `wiki_link` / `authored_refs`), so
/// a link to that page would never round-trip.
///
/// Byte-exact reserved names are deliberately NOT flagged: their
/// writability is per-caller policy (`wiki_admin_push` refuses
/// `_meta.md` but accepts `rules.md` / `_briefing.md`).
#[must_use]
pub fn page_path_case_hazard(rel: &Path) -> Option<String> {
    let reserved = [
        META_FILENAME,
        RULES_FILENAME,
        crate::briefing::BRIEFING_FILENAME,
    ];
    for comp in rel.components() {
        let std::path::Component::Normal(s) = comp else {
            continue;
        };
        let Some(name) = s.to_str() else {
            continue;
        };
        for r in reserved {
            if name != r && name.eq_ignore_ascii_case(r) {
                return Some(format!(
                    "{name:?} is a case variant of the reserved filename {r:?}"
                ));
            }
        }
    }
    let name = rel.file_name()?.to_str()?;
    if name.len() > 3 {
        let ext = &name[name.len() - 3..];
        if ext.eq_ignore_ascii_case(".md") && ext != ".md" {
            return Some(format!(
                "{name:?} spells the .md extension as {ext:?} — the link grammar strips/appends a byte-exact lowercase `.md`, so links to this page would never resolve; use `.md`"
            ));
        }
    }
    None
}

/// Does `rel` exist under `abs_dir` **with this exact spelling**?
///
/// [`Path::exists`] cannot answer that on a case-folding filesystem: it
/// says `Intro.md` exists when the file on disk is `intro.md`, because
/// the two are the same file there. Every caller that branches on "does
/// this page already exist" is really asking the byte-exact question —
/// the answer decides whether a write *appends to an existing page* or
/// *creates a new one*, and getting it wrong on macOS or Windows silently
/// merged two different pages into one.
///
/// Walks the directory listing per component, which reads the same on
/// every filesystem. One `read_dir` per level, on page-write paths only.
#[must_use]
pub fn page_exists_byte_exact(abs_dir: &Path, rel: &Path) -> bool {
    let mut cur = abs_dir.to_path_buf();
    for comp in rel.components() {
        let std::path::Component::Normal(s) = comp else {
            return false;
        };
        let Ok(entries) = std::fs::read_dir(&cur) else {
            return false;
        };
        if !entries.flatten().any(|e| e.file_name() == s) {
            return false;
        }
        cur.push(s);
    }
    true
}

/// Scan for an on-disk entry that collides with `rel` on a
/// case-insensitive filesystem: the first path component matching an
/// existing sibling ASCII-case-insensitively without being byte-equal.
///
/// The server stores wikis on a case-sensitive filesystem, but smart
/// consumers replicate them onto Windows/macOS mirrors where `Setup.md`
/// and `setup.md` are the SAME file — letting both exist server-side
/// would make the next mirror pull silently clobber one with the other.
/// Page-creation paths refuse the write and echo the existing spelling
/// back to the caller instead.
///
/// Best-effort: an unreadable directory yields `None` (the write that
/// follows will surface the real IO error).
#[must_use]
pub fn page_case_conflict(abs_dir: &Path, rel: &Path) -> Option<String> {
    let mut cur = abs_dir.to_path_buf();
    let mut prefix = PathBuf::new();
    for comp in rel.components() {
        let std::path::Component::Normal(s) = comp else {
            return None;
        };
        // Ask the *directory listing* what the real spelling is, never
        // `Path::exists`. On a case-folding filesystem (macOS, Windows)
        // `_Meta.md`.exists() is true because it resolves to `_meta.md`,
        // and trusting it made this function report "byte-exact, carry
        // on" for a name that is not on disk at all — so a capture aimed
        // at `_Meta.md` appended into the wiki's own `_meta.md`. The
        // listing is the same on both kinds of filesystem, so the guard
        // now behaves identically wherever the server runs. One `read_dir`
        // per component, on page creation only.
        let entries = std::fs::read_dir(&cur).ok()?;
        let name = s.to_string_lossy();
        let mut byte_exact = false;
        let mut folded: Option<std::ffi::OsString> = None;
        for e in entries.flatten() {
            let existing = e.file_name();
            if existing == s {
                byte_exact = true;
                break;
            }
            if folded.is_none() && existing.to_string_lossy().eq_ignore_ascii_case(&name) {
                folded = Some(existing);
            }
        }
        if byte_exact {
            cur.push(s);
            prefix.push(s);
            continue;
        }
        if let Some(existing) = folded {
            let found = prefix.join(&existing).to_string_lossy().replace('\\', "/");
            return Some(format!(
                "case-collides with existing `{found}` — a case-insensitive mirror treats them as the same file; reuse that exact spelling"
            ));
        }
        // Nothing at this level, so no deeper component can exist either.
        return None;
    }
    None
}

/// Resolve `rel` under `abs_dir` the way a case-insensitive filesystem
/// (Obsidian on Windows/macOS) would.
///
/// Byte-exact match first, else the UNIQUE ASCII-case-insensitive match
/// at each level. Returns the on-disk relative path, or `None` when a
/// component is missing, ambiguous, or the final target is not a file.
///
/// Keeps server-side wikilink resolution in lockstep with the local
/// mirror: a link whose case drifted from the filename still resolves
/// instead of silently dropping as a dead rail.
#[must_use]
pub fn resolve_page_case_insensitive(abs_dir: &Path, rel: &Path) -> Option<PathBuf> {
    let mut cur = abs_dir.to_path_buf();
    let mut resolved = PathBuf::new();
    for comp in rel.components() {
        let std::path::Component::Normal(s) = comp else {
            return None;
        };
        if cur.join(s).exists() {
            cur.push(s);
            resolved.push(s);
            continue;
        }
        let name = s.to_string_lossy();
        let mut matched: Option<std::ffi::OsString> = None;
        for e in std::fs::read_dir(&cur).ok()?.flatten() {
            let f = e.file_name();
            if f.to_string_lossy().eq_ignore_ascii_case(&name) {
                if matched.is_some() {
                    return None;
                }
                matched = Some(f);
            }
        }
        let m = matched?;
        cur.push(&m);
        resolved.push(&m);
    }
    cur.is_file().then_some(resolved)
}

// ---------- `_internal.*` thin wrappers ----------
//
// These mirror the `_internal.wiki_*` API names from `tool-reference.md`. They
// are thin adapters over [`WikiHandle`] / [`WikiTree`] so the MCP server can
// expose them by name without reaching into the tree machinery.

/// `_internal.wiki_get_meta` — fetch a wiki's `_meta.md`.
///
/// # Errors
///
/// As [`WikiTree::locate`].
pub fn wiki_get_meta(tree: &WikiTree, id: &WikiId) -> Result<WikiMeta> {
    let h = tree.locate(id)?;
    Ok(h.meta().clone())
}

/// `_internal.wiki_read` — read a single page within a wiki.
///
/// # Errors
///
/// As [`WikiHandle::read_page`] plus [`WikiTree::locate`].
pub fn wiki_read(tree: &WikiTree, id: &WikiId, page: &Path) -> Result<String> {
    tree.locate(id)?.read_page(page)
}

/// `_internal.wiki_list_pages` — enumerate pages of a wiki.
///
/// # Errors
///
/// As [`WikiHandle::list_pages`].
pub fn wiki_list_pages(tree: &WikiTree, id: &WikiId) -> Result<Vec<PageInfo>> {
    tree.locate(id)?.list_pages()
}

/// `_internal.wiki_write_page` — atomic write of a page within a wiki.
///
/// # Errors
///
/// As [`WikiHandle::write_page`].
pub fn wiki_write_page(tree: &WikiTree, id: &WikiId, page: &Path, contents: &str) -> Result<()> {
    tree.locate(id)?.write_page(page, contents)
}

/// Read the optional one-line `summary` frontmatter key — the wiki's
/// abstract, kept fresh by the compiler's abstract sync.
///
/// Carried in [`WikiMeta::extra`] until promoted to a typed field; `None` when
/// absent or not a scalar string. Shared by the catalog, the navigator's wiki
/// cards, and the recall block's identity sections (`WHO YOU ARE` / `WHO IS
/// SPEAKING`).
pub(crate) fn meta_summary(meta: &WikiMeta) -> Option<String> {
    meta.extra
        .get("summary")
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_owned)
}

/// Flatten a `keywords` frontmatter mapping into `key=value` search strings.
///
/// The value-less `key` alone is emitted when the value is not a scalar string;
/// non-string keys are skipped. Used by the catalog, the root index, and the
/// per-page card reader ([`crate::meta_annotate::read_page_card_keywords`]) so
/// every card surface exposes the same matchable shape.
pub(crate) fn flatten_keywords_mapping(keywords: &serde_yaml::Mapping) -> Vec<String> {
    keywords
        .iter()
        .filter_map(|(k, v)| {
            let key = k.as_str()?;
            Some(
                v.as_str()
                    .map_or_else(|| key.to_owned(), |val| format!("{key}={val}")),
            )
        })
        .collect()
}

/// [`flatten_keywords_mapping`] over a wiki's `_meta.md` keywords.
fn flatten_keywords(meta: &WikiMeta) -> Vec<String> {
    flatten_keywords_mapping(&meta.keywords)
}

/// `_internal.wiki_catalog_list` — group wikis by `wiki_type`.
///
/// Returns a map `wiki_type → [(WikiId, slug, title)]`, sorted within each
/// bucket by `wiki_id`. Internal / admin callers see every wiki. For
/// sender-scoped enumeration — where visibility is **derived** from the
/// per-fragment ACL (a wiki appears only when the reader can read ≥ 1 fact in
/// it) — use [`wiki_catalog_list_for`].
///
/// # Errors
///
/// As [`WikiTree::walk`].
pub fn wiki_catalog_list(tree: &WikiTree) -> Result<BTreeMap<String, Vec<CatalogEntry>>> {
    let mut by_type: BTreeMap<String, Vec<CatalogEntry>> = BTreeMap::new();
    for d in tree.walk()? {
        let e = CatalogEntry {
            wiki_id: d.meta.wiki_id.clone(),
            slug: d.meta.slug.clone(),
            title: d.meta.title.clone(),
            summary: meta_summary(&d.meta),
            keywords: flatten_keywords(&d.meta),
        };
        by_type.entry(d.meta.wiki_type).or_default().push(e);
    }
    for v in by_type.values_mut() {
        v.sort_by(|a, b| a.wiki_id.as_str().cmp(b.wiki_id.as_str()));
    }
    Ok(by_type)
}

/// Sender-scoped catalog enumeration, **reader-relative**.
///
/// Variant of [`wiki_catalog_list`] where wiki visibility is **derived** from
/// the per-fragment ACL: a wiki is listed iff the reader can read ≥ 1 fact in
/// it (`readable_wikis` membership, counting topic-less facts too), and each
/// surviving wiki's card is projected *for the reader* rather than at the owner
/// tier — `keywords` come from `reader_topics` (the topic union the reader can
/// read, computed from `fact_index` by
/// [`crate::meta_annotate::build_reader_card`]) instead of the `.md`
/// frontmatter, and `summary` is served only when the wiki's id is in
/// `summary_visible` (the reader's read-set covers the wiki's default
/// visibility). This is the catalog half of the reader-relative card boundary
/// ([identity-and-acl.md §The ACL card boundary](../../../docs/concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry)):
/// a reader denied a fact never sees its theme leak through the catalog card.
///
/// # Errors
///
/// As [`WikiTree::walk`].
pub fn wiki_catalog_list_for(
    tree: &WikiTree,
    readable_wikis: &BTreeSet<String>,
    reader_topics: &BTreeMap<String, Vec<String>>,
    summary_visible: &BTreeSet<String>,
) -> Result<BTreeMap<String, Vec<CatalogEntry>>> {
    let mut by_type: BTreeMap<String, Vec<CatalogEntry>> = BTreeMap::new();
    for d in tree.walk()? {
        let wiki_id = d.meta.wiki_id.as_str();
        // Derived visibility: a wiki appears only when the reader can read at
        // least one fact in it. A wiki with nothing visible to this reader is
        // skipped.
        if !readable_wikis.contains(wiki_id) {
            continue;
        }
        let e = CatalogEntry {
            wiki_id: d.meta.wiki_id.clone(),
            slug: d.meta.slug.clone(),
            title: d.meta.title.clone(),
            summary: if summary_visible.contains(wiki_id) {
                meta_summary(&d.meta)
            } else {
                None
            },
            keywords: reader_topics.get(wiki_id).cloned().unwrap_or_default(),
        };
        by_type.entry(d.meta.wiki_type).or_default().push(e);
    }
    for v in by_type.values_mut() {
        v.sort_by(|a, b| a.wiki_id.as_str().cmp(b.wiki_id.as_str()));
    }
    Ok(by_type)
}

/// One row of [`wiki_catalog_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Stable id.
    pub wiki_id: WikiId,
    /// Directory slug.
    pub slug: WikiSlug,
    /// Display title.
    pub title: String,
    /// One-line `summary` frontmatter key, when present — the abstract the
    /// root index shows so a navigator can pick a branch without reading the
    /// wiki. `None` until the compiler populates it (then carried verbatim).
    pub summary: Option<String>,
    /// The card's topic tags — what the recall navigator's card seeds match
    /// against. From [`wiki_catalog_list_for`] these are **reader-relative**
    /// (the topics the reader can read); from the admin [`wiki_catalog_list`]
    /// they are the owner-tier `.md` `keywords` flattened. Empty until populated.
    pub keywords: Vec<String>,
}

/// Render a sender-scoped catalog as a compact **root index** string.
///
/// The root index is the orientation map a recall navigator reads before
/// descending into a wiki; the input is the output of [`wiki_catalog_list`] /
/// [`wiki_catalog_list_for`]. Wikis are grouped under a `## <wiki_type>` heading
/// (the [`BTreeMap`] keys are already sorted) and listed as `[[wiki_id]]`
/// wikilinks the navigator can follow, with the display title appended when it
/// adds information. The per-sender ACL filtering is the caller's responsibility
/// — [`wiki_catalog_list_for`] already drops the wikis a sender may not see — so
/// this stays a pure, side-effect-free render. Returns an empty string for an
/// empty catalog.
///
/// The navigation-recall entry map (recall pipeline):
/// `recall_nav::navigate` renders it per turn from the sender-scoped catalog and
/// hands it to the navigator as the ROOT INDEX section of every hop.
#[must_use]
pub fn render_root_index(by_type: &BTreeMap<String, Vec<CatalogEntry>>) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (wiki_type, entries) in by_type {
        if entries.is_empty() {
            continue;
        }
        lines.push(format!("## {wiki_type}"));
        for e in entries {
            let id = e.wiki_id.as_str();
            let head = if e.title.is_empty() || e.title.as_str() == id {
                format!("- [[{id}]]")
            } else {
                format!("- [[{id}]] — {}", e.title)
            };
            lines.push(head);
            if let Some(summary) = &e.summary {
                lines.push(format!("  {summary}"));
            }
            if !e.keywords.is_empty() {
                lines.push(format!("  keywords: {}", e.keywords.join(" · ")));
            }
        }
    }
    lines.join("\n")
}

// ---------- Identity-wiki bootstrap ----------

/// `wiki_type` for a freshly created **user** identity wiki.
///
/// Generic enough to host prose, a `keywords` block, lifecycle-aware
/// regions if the owner later forges a more specific type. The admin
/// can change it via `_meta.md` edit or `wiki_change_scope` after the
/// fact. Group identity wikis use [`GROUP_IDENTITY_WIKI_TYPE`] instead —
/// see [`IdentityKind::wiki_type`].
pub const IDENTITY_WIKI_TYPE: &str = "wiki-user";

/// `wiki_type` for a freshly created **group** identity wiki.
///
/// The bundled `wiki-group` template (`smart = false`, so it is
/// writable through `wiki_ingest_message`). Group wikis once were
/// mis-stamped `wiki-user`, differing from a user wiki only in their
/// owning principal; this gives the group its own type so the catalogue
/// and the router see it for what it is — and so the scope-principal
/// derivation reads `group:<id>` straight off the root's type.
pub const GROUP_IDENTITY_WIKI_TYPE: &str = "wiki-group";

/// Whether the freshly created identity wiki belongs to a single user
/// or to a shared group, which controls the derived scope principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    /// A single user owns the wiki — a `wiki-user` root, so its scope
    /// principal derives to `user:<id>`.
    User,
    /// A shared group owns the wiki — a `wiki-group` root, so its scope
    /// principal derives to `group:<id>`.
    Group,
    /// A consumer agent's own identity wiki (roadmap 27d / 4i). Same shape as
    /// [`Self::User`] — a `wiki-user` whose scope principal derives to
    /// `user:<id>` — but stamped `is_agent: true` so it self-describes as an
    /// agent's wiki, not a human's.
    Agent,
}

impl IdentityKind {
    /// The bundled `wiki_type` a freshly created identity wiki of this
    /// kind is stamped with: [`IDENTITY_WIKI_TYPE`] (`wiki-user`) for a
    /// user, [`GROUP_IDENTITY_WIKI_TYPE`] (`wiki-group`) for a group.
    #[must_use]
    pub const fn wiki_type(self) -> &'static str {
        match self {
            // An agent wiki is a `wiki-user` like any human's — it just carries
            // the `is_agent` marker.
            Self::User | Self::Agent => IDENTITY_WIKI_TYPE,
            Self::Group => GROUP_IDENTITY_WIKI_TYPE,
        }
    }
}

/// Outcome of [`create_identity_wiki`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityWikiCreation {
    /// Stable id (matches the `user_id` / `group_id`).
    pub wiki_id: WikiId,
    /// `true` when `_meta.md` was freshly written; `false` when the
    /// directory already had one and the call was a no-op.
    pub created: bool,
}

/// Default body of a freshly-seeded [`RULES_FILENAME`].
/// The scaffold prose covers the *governance* policy the memory engine
/// honours — who may see your facts (privacy & sharing) and what must never
/// be stored (do-not-store). Per-agent behaviour rules ("address me
/// formally") are NOT here — they belong to the consumer's own wiki — but a
/// USER-GLOBAL behaviour rule (one the user sets for every assistant,
/// roadmap 42) is filed on this page as a `{{f=…}}` fact region, alongside
/// the prose. Neutral on purpose: the decided default posture is "the agent
/// decides, as now" — no conservative ACL override is baked in
/// (no hardcoded gates). The
/// body is flat prose: the ingest write-path ([`append_engine_rule`]) appends
/// each new rule as a bullet, and the file's free prose (fact regions
/// stripped) is injected as the classifier's `sender_rules`, so layout is
/// not load-bearing.
const RULES_DEFAULT_BODY: &str = "# Rules\n\n\
Your standing policy for this memory. Every assistant that uses this memory \
respects what you write here — it lives with the memory, not inside any one \
assistant. Two kinds of rules belong here: who may see your facts (privacy & \
sharing), and what must never be stored (do-not-store). Leave this file \
untouched to let the assistant decide for you; add a line to tighten it. \
Rules you set in chat for every assistant at once (\"always answer me in \
Italian, whoever you are\") are also kept here, managed automatically.\n";

/// Append one engine-rule to a wiki's [`RULES_FILENAME`], as a prose bullet.
///
/// This is the write side of the engine-rule loop: when the ingest classifier
/// marks an extraction as a standing *governance* directive (a privacy/sharing
/// policy or a do-not-store rule), the orchestrator routes it here instead of
/// filing it as a fact — the rule lives as prose the engine reads back as
/// `sender_rules`, never as a row in `fact_index`. The whole file is injected
/// into the next ingest prompt, so we simply append; section layout is not
/// load-bearing.
///
/// Reads the wiki's current `rules.md` and appends `- <rule>` after a blank
/// line. When the file is missing (a legacy wiki that never got the
/// scaffold), it starts from [`RULES_DEFAULT_BODY`] so the rule is never lost.
///
/// `rule` is trusted prose from the classifier (the rule restated as a standing
/// policy sentence); a leading `- ` is stripped if the model already bulleted
/// it, and surrounding whitespace is trimmed.
///
/// # Errors
///
/// - [`WikiError::UnsafePagePath`] (never, for the constant `rules.md`).
/// - [`WikiError::Io`] on a filesystem read/write failure.
pub fn append_engine_rule(handle: &WikiHandle, rule: &str) -> Result<()> {
    let rule = rule.trim().trim_start_matches("- ").trim();
    if rule.is_empty() {
        return Ok(());
    }
    let rules_path = Path::new(RULES_FILENAME);
    let mut body = match handle.read_page(rules_path) {
        Ok(existing) => existing,
        Err(WikiError::PageNotFound { .. }) => RULES_DEFAULT_BODY.to_owned(),
        Err(e) => return Err(e),
    };
    if !body.ends_with('\n') {
        body.push('\n');
    }
    // A blank line before the first bullet keeps the Markdown list well-formed
    // when the body ends in a paragraph; consecutive rules just stack.
    if !body.ends_with("\n\n") {
        body.push('\n');
    }
    body.push_str("- ");
    body.push_str(rule);
    body.push('\n');
    handle.write_page(rules_path, &body)
}

/// Create the on-disk scaffold for an identity wiki.
///
/// (See the wiki filesystem surface.) Writes
/// `<workdir>/wikis/<id>/_meta.md` (frontmatter) + `index.md` (placeholder
/// body) + [`rules.md`](RULES_FILENAME) (default user-policy page).
///
/// Idempotent — when the directory already has a `_meta.md`, returns
/// `Ok(IdentityWikiCreation { created: false, ... })` and does not
/// overwrite. This is what lets the enrollment loader re-run the same
/// import without producing duplicates.
///
/// # Arguments
///
/// - `tree`: open `WikiTree` rooted at the workdir
/// - `id`: stable identifier (`user_id` or `group_id`, must already be
///   slug-validated by [`crate::types::WikiId::parse`])
/// - `title`: human-readable display title (free unicode). Defaults
///   from the caller — for the setup wizard, pass the `user_id`; once
///   the CRUD has a `display_name` field, pass that.
/// - `kind`: [`IdentityKind::User`], [`IdentityKind::Group`], or
///   [`IdentityKind::Agent`] (a `wiki-user` stamped `is_agent: true`).
///
/// # Errors
///
/// - [`WikiError::Io`] for filesystem failures.
/// - YAML serialisation surface from [`WikiMeta::to_yaml`] (vanishingly
///   rare for the canonical schema we emit).
pub fn create_identity_wiki(
    tree: &WikiTree,
    id: &WikiId,
    title: &str,
    kind: IdentityKind,
) -> Result<IdentityWikiCreation> {
    let dir = tree.wikis_dir().join(id.as_str());
    let meta_path = dir.join(META_FILENAME);
    if meta_path.exists() {
        tracing::info!(
            wiki_id = id.as_str(),
            "identity wiki: already exists, preserving"
        );
        return Ok(IdentityWikiCreation {
            wiki_id: id.clone(),
            created: false,
        });
    }
    let slug = WikiSlug::parse(id.as_str()).map_err(|source| WikiError::SlugInMeta {
        path: meta_path.clone(),
        source,
    })?;
    // The scope principal is derived from this wiki's identity `wiki_type`
    // + id (it is a root: `parent_wiki_id == None`), so nothing about the
    // owner is stamped into the frontmatter any more.
    let now_iso = chrono::Utc::now().to_rfc3339();
    let meta = WikiMeta {
        wiki_id: id.clone(),
        wiki_type: kind.wiki_type().to_owned(),
        parent_wiki_id: None,
        slug,
        title: title.to_owned(),
        scope: None,
        shared_with: Vec::new(),
        style_overrides: serde_yaml::Mapping::new(),
        keywords: serde_yaml::Mapping::new(),
        children: Vec::new(),
        promoted_from: None,
        no_archive: false,
        smart: false,
        is_agent: matches!(kind, IdentityKind::Agent),
        created: Some(now_iso.clone()),
        updated: Some(now_iso),
        extra: serde_yaml::Mapping::new(),
    };
    let body = format!(
        "# {title}\n\n_Identity wiki for {id}. Capture freely._\n",
        id = id.as_str()
    );
    let meta_doc = meta.render("").map_err(|e| WikiError::InvalidFrontmatter {
        path: meta_path.clone(),
        detail: format!("rendering canonical meta: {e}"),
    })?;
    atomic_write(&meta_path, meta_doc.as_bytes())?;
    atomic_write(&dir.join("index.md"), body.as_bytes())?;
    // Seed the user-facing policy page. Only at creation (like
    // index.md); the idempotent early-return above preserves a user-edited
    // rules.md on re-runs.
    atomic_write(&dir.join(RULES_FILENAME), RULES_DEFAULT_BODY.as_bytes())?;
    tracing::info!(
        wiki_id = id.as_str(),
        kind = ?kind,
        "identity wiki: created"
    );
    // A new top-level wiki appeared → refresh the operator's Obsidian
    // collector index. Best-effort: a convenience artifact must never
    // fail the actual creation.
    if let Err(e) = write_root_collector_index(tree) {
        tracing::warn!(error = %e, "root collector index refresh failed (non-fatal)");
    }
    Ok(IdentityWikiCreation {
        wiki_id: id.clone(),
        created: true,
    })
}

/// Stamp the `is_agent` marker (roadmap 27d / 4i) on an **identity** wiki.
///
/// The self-describing mirror of the authoritative binding, written by the
/// server where an agent identity comes into being or reconnects: at creation
/// via [`IdentityKind::Agent`], on bot-token mint, and from the MCP auth
/// middleware on every standard connect — the path that heals an agent
/// enrolled through the ordinary user CRUD, whose wiki was created as a plain
/// `wiki-user`, without an operator step.
///
/// **Roots only, deliberately.** An identity wiki is always `wikis/<id>/`
/// ([`create_identity_wiki`]), so the id maps straight onto its `_meta.md` and
/// the connect path pays one `stat` — never a tree walk, which it would
/// otherwise pay on *every request* of a deployment whose bot wiki is missing.
/// A nested wiki (a smart consumer's operational wiki) is resolved by its
/// caller, which then calls [`ensure_is_agent_marker_in`]; that caller is a
/// sign-in, not a hot path, so the walk is affordable there and visible here.
///
/// Idempotent, and best-effort by contract: `Ok(false)` when the wiki is absent
/// or already marked, and the caller logs and continues on error.
///
/// # Errors
///
/// [`WikiError`] on a filesystem or frontmatter (re)serialization failure.
pub fn ensure_is_agent_marker(tree: &WikiTree, id: &WikiId) -> Result<bool> {
    ensure_is_agent_marker_in(&tree.wikis_dir().join(id.as_str()))
}

/// [`ensure_is_agent_marker`] for a wiki whose directory the caller already
/// resolved — the shape that is **not** a root.
///
/// The OAuth consent flow uses it for a smart consumer's operational wiki
/// (`franz-ubestia-cc` lives at `wikis/franz/ubestia-cc/`, so its id is not a
/// path). That wiki also carries `wiki_type: agent`, but the label is a
/// free-form string the consumer passes to `wiki_admin_push` and anything may
/// claim it; this marker is written by the server alone, which is why the
/// signpost gate and the dashboard badge key on it.
///
/// # Errors
///
/// [`WikiError`] on a filesystem or frontmatter (re)serialization failure.
pub fn ensure_is_agent_marker_in(wiki_dir: &Path) -> Result<bool> {
    let meta_path = wiki_dir.join(META_FILENAME);
    if !meta_path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&meta_path)?;
    let (mut meta, body) = WikiMeta::parse(&meta_path, &raw)?;
    if meta.is_agent {
        return Ok(false);
    }
    meta.is_agent = true;
    let meta_doc = meta
        .render(&body)
        .map_err(|e| WikiError::InvalidFrontmatter {
            path: meta_path.clone(),
            detail: format!("rendering meta for is_agent backfill: {e}"),
        })?;
    atomic_write(&meta_path, meta_doc.as_bytes())?;
    tracing::info!(
        wiki_id = meta.wiki_id.as_str(),
        "is_agent marker stamped on agent wiki (roadmap 27d / 4i)"
    );
    Ok(true)
}

/// Write the **root collector index** at `<workdir>/wikis/index.md`: a
/// marker-less Obsidian hub linking every top-level memory wiki.
///
/// This exists purely so an operator who opens `wikis/` as an Obsidian
/// vault gets a navigable landing page — it is **admin convenience, NOT a
/// mwe-mcp runtime mechanism**, and nothing in the engine reads it.
/// (Future: the dashboard graph-exploration view can reuse this hub.)
///
/// It is a *loose* file — no `_meta.md` beside it — so it stays invisible
/// to the engine: the re-index resolves it to `wiki_id = None` (zero
/// `fact_index` rows, exactly like `_styles/`) and wiki enumeration (which
/// keys on `_meta.md`) never mistakes it for a wiki. It is **distinct from
/// the recall "root index"**, which is rendered per-sender at recall
/// time and never persisted — do not conflate them.
///
/// Only **top-level** wikis are listed — direct children of `wikis/`
/// (`rel_dir` depth 2). Smart wikis and emerged sub-wikis are always
/// nested, so they never appear here; the operator follows each top-level
/// wiki's own internal links to reach them. Re-written wholesale (option
/// B, "kept fresh") whenever a top-level wiki is created and once at
/// bootstrap to realign after external edits. Entries are sorted
/// case-insensitively by title; each is an Obsidian wikilink
/// `[[<slug>/index|<Title>]]` (path resolves from the `wikis/` vault root,
/// alias is the human title).
///
/// # Errors
///
/// Filesystem failures from the tree walk or the atomic write.
pub fn write_root_collector_index(tree: &WikiTree) -> Result<()> {
    use std::fmt::Write as _;
    let mut entries: Vec<(String, String)> = tree
        .walk()?
        .into_iter()
        .filter(|d| d.rel_dir.components().count() == 2 && !d.meta.smart)
        .map(|d| (d.meta.title.clone(), d.meta.slug.as_str().to_owned()))
        .collect();
    entries.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut body = String::from("# Wikis\n\n");
    for (title, slug) in &entries {
        let _ = writeln!(body, "- [[{slug}/index|{title}]]");
    }
    atomic_write(&tree.wikis_dir().join("index.md"), body.as_bytes())
}

/// Materialise a wiki directory on disk from a fully-built [`WikiMeta`] plus
/// the body of its `index.md`.
///
/// A generic filesystem primitive (the `wiki_type` registry/template machinery
/// it once served has been removed): the caller hands over a finished
/// `WikiMeta` and this writes the four steps every creation site shares:
///
/// 1. **Dir resolution** — under the parent's directory when
///    `meta.parent_wiki_id` is `Some`, else top-level under `wikis/<wiki_id>/`.
/// 2. **Child-only gate** — when `requires_parent` is set, a parent-less create
///    is refused with [`WikiError::RequiresParent`], so the wiki always
///    inherits a parent's ACL scope.
/// 3. **Additive-only invariant** — an existing `_meta.md` at the target is
///    refused with [`WikiError::AlreadyExists`]; create never overwrites.
/// 4. **Atomic write** — `_meta.md` (canonical render) then `index.md`.
///
/// It deliberately does **not** touch `fact_index`, the op-log, or any
/// transaction: callers layer their own bookkeeping around it. Returns the
/// created wiki's absolute directory.
///
/// # Errors
///
/// - [`WikiError::RequiresParent`] — `requires_parent` set but no parent.
/// - [`WikiError::AlreadyExists`] — `_meta.md` already present at the target.
/// - [`WikiError::WikiNotFound`] — `meta.parent_wiki_id` does not resolve.
/// - [`WikiError::InvalidFrontmatter`] — `_meta.md` failed to render.
/// - [`WikiError::Io`] — filesystem error during write.
pub fn write_wiki_dir(
    tree: &WikiTree,
    meta: &WikiMeta,
    index_body: &str,
    requires_parent: bool,
) -> Result<PathBuf> {
    if requires_parent && meta.parent_wiki_id.is_none() {
        return Err(WikiError::RequiresParent {
            wiki_type: meta.wiki_type.clone(),
        });
    }

    let dir = match meta.parent_wiki_id.as_ref() {
        Some(parent_id) => tree.locate(parent_id)?.abs_dir().join(meta.slug.as_str()),
        None => tree.wikis_dir().join(meta.wiki_id.as_str()),
    };

    let meta_path = dir.join(META_FILENAME);
    if meta_path.exists() {
        return Err(WikiError::AlreadyExists {
            id: meta.wiki_id.clone(),
        });
    }

    // Materialise the wiki directory itself with private (0700) perms.
    // Otherwise atomic_write creates it via create_dir_all under the process
    // umask (commonly 0755 — world-readable), which a memory wiki must never
    // be. The page files themselves are already 0600 (tempfile default).
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }

    let meta_doc = meta.render("").map_err(|e| WikiError::InvalidFrontmatter {
        path: meta_path.clone(),
        detail: format!("rendering canonical meta: {e}"),
    })?;
    atomic_write(&meta_path, meta_doc.as_bytes())?;
    atomic_write(&dir.join("index.md"), index_body.as_bytes())?;
    Ok(dir)
}

/// Does this filesystem tell `a.md` and `A.md` apart?
///
/// Test-only, and shared across the crate's test modules because several
/// of them build the same fixture: two files differing only by case.
/// **That fixture cannot exist on macOS or Windows** — the second write
/// lands on the first file — so the assertions that depend on it are
/// skipped there rather than deleted. Nothing is lost by skipping: the
/// guard those tests exercise ([`page_case_conflict`]) scans for a
/// sibling matching case-insensitively, and on a folding filesystem the
/// pair it protects against is unrepresentable in the first place. The
/// guard exists for the **server**, which stores wikis on a
/// case-sensitive filesystem and must not hand a mirror two files that
/// would collapse into one.
///
/// Probed at runtime rather than by `cfg!(target_os)`: a Linux box can
/// mount a case-folding directory and a Mac can mount a case-sensitive
/// volume, and it is the filesystem under the fixture that decides.
#[cfg(test)]
pub(crate) fn fs_distinguishes_case(dir: &Path) -> bool {
    let probe = dir.join("mwe-case-probe.tmp");
    if std::fs::write(&probe, b"x").is_err() {
        return false;
    }
    let distinguishes = !dir.join("MWE-CASE-PROBE.TMP").exists();
    let _ = std::fs::remove_file(&probe);
    distinguishes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------- MarkdownDoc ----------

    #[test]
    fn markdown_doc_parses_simple_frontmatter() {
        let input = "---\ntitle: hi\n---\nbody line\n";
        let doc = MarkdownDoc::parse(input).expect("frontmatter");
        assert_eq!(doc.frontmatter, "title: hi");
        assert_eq!(doc.body, "body line\n");
    }

    #[test]
    fn markdown_doc_parses_empty_frontmatter() {
        let input = "---\n---\nhello\n";
        let doc = MarkdownDoc::parse(input).expect("frontmatter");
        assert_eq!(doc.frontmatter, "");
        assert_eq!(doc.body, "hello\n");
    }

    #[test]
    fn markdown_doc_rejects_missing_open_fence() {
        assert!(MarkdownDoc::parse("body without frontmatter\n").is_none());
    }

    #[test]
    fn markdown_doc_rejects_unterminated_frontmatter() {
        assert!(MarkdownDoc::parse("---\nstuff: 1\nmore: 2\n").is_none());
    }

    #[test]
    fn markdown_doc_ignores_dashes_in_body() {
        // `---` inside the body should not be confused with the closing
        // fence as long as the actual closing fence appears first.
        let input = "---\nfoo: 1\n---\nbody\n---\nmore\n";
        let doc = MarkdownDoc::parse(input).expect("frontmatter");
        assert_eq!(doc.frontmatter, "foo: 1");
        assert_eq!(doc.body, "body\n---\nmore\n");
    }

    // ---------- WikiMeta ----------

    fn sample_meta_yaml() -> &'static str {
        "---\n\
         wiki_id: alice\n\
         wiki_type: wiki-user\n\
         parent_wiki_id: null\n\
         slug: alice\n\
         title: Alice\n\
         acl_default: 'user:alice'\n\
         ---\n"
    }

    #[test]
    fn wiki_meta_parses_minimal_root() {
        let p = PathBuf::from("wikis/alice/_meta.md");
        let (m, body) = WikiMeta::parse(&p, sample_meta_yaml()).expect("parse");
        assert_eq!(m.wiki_id.as_str(), "alice");
        assert_eq!(m.wiki_type, "wiki-user");
        assert!(m.parent_wiki_id.is_none());
        assert_eq!(m.slug.as_str(), "alice");
        assert_eq!(m.title, "Alice");
        // The legacy `acl_default` line is read-and-ignored; the owning
        // principal is derived from topology, not parsed. No `scope` prose
        // in the minimal fixture.
        assert!(m.scope.is_none());
        assert_eq!(body, "");
        assert!(m.children.is_empty());
        assert!(!m.no_archive);
        // Smart defaults to false: a `_meta.md` without the key is a
        // standard wiki.
        assert!(!m.smart);
    }

    #[test]
    fn smart_flag_round_trips() {
        let p = PathBuf::from("wikis/myproj/_meta.md");
        // A smart-wiki `_meta.md` carries `smart: true`.
        let raw = "---\n\
             wiki_id: myproj\n\
             wiki_type: wiki-companion\n\
             parent_wiki_id: alice\n\
             slug: myproj\n\
             title: My Project\n\
             acl_default: 'user:alice'\n\
             smart: true\n\
             ---\n";
        let (m, _) = WikiMeta::parse(&p, raw).expect("parse");
        assert!(m.smart);
        // Render → re-parse keeps the flag, and the canonical key is
        // emitted.
        let rendered = m.to_yaml().expect("to_yaml");
        assert!(rendered.contains("smart: true"));
        let (m2, _) = WikiMeta::parse(&p, &format!("---\n{rendered}---\n")).expect("reparse");
        assert!(m2.smart);

        // A standard wiki never emits the key (lean frontmatter).
        let (plain, _) = WikiMeta::parse(&p, sample_meta_yaml()).expect("parse");
        assert!(!plain.to_yaml().expect("to_yaml").contains("smart"));
    }

    /// The pre-rename on-disk key keeps working forever: a legacy
    /// `_meta.md` carrying `companion: true` parses as a smart wiki and
    /// migrates to the canonical `smart:` key on its first rewrite.
    #[test]
    fn companion_alias_reads_as_smart_and_migrates_on_rewrite() {
        let p = PathBuf::from("wikis/myproj/_meta.md");
        let raw = "---\n\
             wiki_id: myproj\n\
             wiki_type: wiki-companion\n\
             parent_wiki_id: alice\n\
             slug: myproj\n\
             title: My Project\n\
             acl_default: 'user:alice'\n\
             companion: true\n\
             ---\n";
        let (m, _) = WikiMeta::parse(&p, raw).expect("parse");
        assert!(m.smart, "companion: true must read as smart");
        let rendered = m.to_yaml().expect("to_yaml");
        assert!(rendered.contains("smart: true"));
        assert!(
            !rendered.contains("companion: true"),
            "rewrite must emit the canonical key only: {rendered}"
        );
    }

    /// Back-compat: a legacy root `_meta.md` carrying `acl_default` (even
    /// `inherit`, once forbidden on a root) now parses cleanly — the field
    /// is read-and-ignored and never re-emitted, so it drops on the next
    /// rewrite.
    #[test]
    fn legacy_acl_default_on_root_is_read_and_ignored() {
        let yaml = "---\n\
                    wiki_id: alice\n\
                    wiki_type: wiki-user\n\
                    parent_wiki_id: null\n\
                    slug: alice\n\
                    title: Alice\n\
                    acl_default: inherit\n\
                    ---\n";
        let p = PathBuf::from("wikis/alice/_meta.md");
        let (m, _) = WikiMeta::parse(&p, yaml).expect("legacy acl_default must parse");
        // Ignored on read, and gone from the rewrite.
        let rendered = m.to_yaml().expect("to_yaml");
        assert!(
            !rendered.contains("acl_default"),
            "acl_default must not be re-emitted: {rendered}"
        );
    }

    #[test]
    fn wiki_meta_parses_subwiki_ignoring_legacy_acl_default() {
        let yaml = "---\n\
                    wiki_id: alice-acmecorp\n\
                    wiki_type: wiki-cliente\n\
                    parent_wiki_id: alice\n\
                    slug: acmecorp\n\
                    title: Acme Corp\n\
                    acl_default: inherit\n\
                    ---\n";
        let p = PathBuf::from("wikis/alice/acmecorp/_meta.md");
        let (m, _) = WikiMeta::parse(&p, yaml).expect("parse");
        assert_eq!(m.parent_wiki_id.as_ref().unwrap().as_str(), "alice");
        let rendered = m.to_yaml().expect("to_yaml");
        assert!(!rendered.contains("acl_default"), "{rendered}");
    }

    #[test]
    fn wiki_meta_round_trip_preserves_extra_keys() {
        let yaml = "---\n\
                    wiki_id: frodo-reminder\n\
                    wiki_type: wiki-cron\n\
                    parent_wiki_id: frodo\n\
                    slug: reminder\n\
                    title: Reminder di Frodo\n\
                    acl_default: 'user:frodo'\n\
                    lead_time_notify: 30m\n\
                    ttl_done: 30d\n\
                    ---\n";
        let p = PathBuf::from("wikis/frodo/reminder/_meta.md");
        let (m, body) = WikiMeta::parse(&p, yaml).expect("parse");
        assert_eq!(body, "");
        // The forge-specific keys land in `extra`.
        assert!(m.extra.contains_key(yk("lead_time_notify")));
        assert!(m.extra.contains_key(yk("ttl_done")));
        // Round-trip: re-render and re-parse must yield an equivalent meta.
        let rendered = m.render("").expect("render");
        let (m2, _) = WikiMeta::parse(&p, &rendered).expect("reparse");
        assert_eq!(m, m2);
    }

    #[test]
    fn wiki_meta_rejects_missing_required_field() {
        let yaml = "---\n\
                    wiki_id: alice\n\
                    wiki_type: wiki-user\n\
                    ---\n";
        let p = PathBuf::from("wikis/alice/_meta.md");
        let err = WikiMeta::parse(&p, yaml).expect_err("must reject");
        assert!(matches!(err, WikiError::InvalidFrontmatter { .. }));
    }

    /// `scope` is the new optional prose field: round-tripped verbatim when
    /// present, absent from the frontmatter when `None`.
    #[test]
    fn wiki_meta_scope_round_trips() {
        let yaml = "---\n\
                    wiki_id: alice\n\
                    wiki_type: wiki-user\n\
                    parent_wiki_id: null\n\
                    slug: alice\n\
                    title: Alice\n\
                    scope: Everything about Alice\n\
                    ---\n";
        let p = PathBuf::from("wikis/alice/_meta.md");
        let (m, _) = WikiMeta::parse(&p, yaml).expect("parse");
        assert_eq!(m.scope.as_deref(), Some("Everything about Alice"));
        let rendered = m.to_yaml().expect("to_yaml");
        assert!(
            rendered.contains("scope: Everything about Alice"),
            "{rendered}"
        );
        let (m2, _) = WikiMeta::parse(&p, &format!("---\n{rendered}---\n")).expect("reparse");
        assert_eq!(m, m2);

        // A wiki without the key never emits it (lean frontmatter).
        let (plain, _) = WikiMeta::parse(&p, sample_meta_yaml()).expect("parse");
        assert!(plain.scope.is_none());
        assert!(!plain.to_yaml().expect("to_yaml").contains("scope"));
    }

    // ---------- create_identity_wiki ----------

    #[test]
    fn create_identity_wiki_writes_meta_and_index() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("franz").unwrap();
        let outcome = create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        assert!(outcome.created);
        let meta = fs::read_to_string(tree.wikis_dir().join("franz").join("_meta.md")).unwrap();
        assert!(meta.contains("wiki_id: franz"));
        // The owning principal is derived, never stamped into the meta.
        assert!(
            !meta.contains("acl_default"),
            "acl_default must not be written: {meta}"
        );
        let parsed = wiki_get_meta(&tree, &id).expect("get meta");
        assert_eq!(
            tree.resolve_scope_principal(&parsed).expect("resolve"),
            Principal::User("franz".into())
        );
        let index = fs::read_to_string(tree.wikis_dir().join("franz").join("index.md")).unwrap();
        assert!(index.contains("# Franz"));
        // A default, user-facing rules.md is seeded too — engine
        // rules only (privacy + do-not-store), no "Behaviour" section.
        let rules =
            fs::read_to_string(tree.wikis_dir().join("franz").join(RULES_FILENAME)).unwrap();
        assert!(rules.contains("# Rules"));
        assert!(rules.contains("privacy & sharing"));
        assert!(rules.contains("do-not-store"));
        assert!(
            !rules.contains("## Behaviour"),
            "behaviour rules belong to the consumer wiki, not rules.md"
        );
    }

    #[test]
    fn append_engine_rule_adds_bullet_and_creates_when_missing() {
        // Write path of the engine-rule loop: a governance directive
        // is appended to rules.md as prose, never filed as a fact.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("franz").unwrap();
        create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        let handle = tree.locate(&id).unwrap();

        append_engine_rule(
            &handle,
            "Health information is always private; never share it.",
        )
        .unwrap();
        // A model that already bulleted the rule must not double-bullet.
        append_engine_rule(&handle, "- Never store credit-card numbers.").unwrap();
        // Empty / whitespace rules are no-ops.
        append_engine_rule(&handle, "   ").unwrap();

        let rules =
            fs::read_to_string(tree.wikis_dir().join("franz").join(RULES_FILENAME)).unwrap();
        assert!(rules.contains("- Health information is always private; never share it."));
        assert!(rules.contains("- Never store credit-card numbers."));
        assert!(
            !rules.contains("- - "),
            "a pre-bulleted rule must not be double-bulleted; body was:\n{rules}"
        );
        assert_eq!(
            rules.matches("- ").count(),
            2,
            "exactly two rules appended (the whitespace one is a no-op); body was:\n{rules}"
        );
    }

    #[test]
    fn append_engine_rule_starts_from_default_when_no_file() {
        // A legacy wiki has a _meta.md but no rules.md: the helper must
        // seed from the default body rather than lose the rule.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("legacy").unwrap();
        create_identity_wiki(&tree, &id, "Legacy", IdentityKind::User).unwrap();
        let rules_path = tree.wikis_dir().join("legacy").join(RULES_FILENAME);
        fs::remove_file(&rules_path).unwrap();
        let handle = tree.locate(&id).unwrap();

        append_engine_rule(&handle, "Default everything private.").unwrap();
        let rules = fs::read_to_string(&rules_path).unwrap();
        assert!(rules.contains("# Rules"), "seeded from default body");
        assert!(rules.contains("- Default everything private."));
    }

    #[test]
    fn create_identity_wiki_idempotent_preserves_edited_rules() {
        // A user (or the wizard) edits rules.md; a re-run of enrollment must NOT
        // clobber it (the idempotent early-return preserves the whole dir).
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("franz").unwrap();
        create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        let rules_path = tree.wikis_dir().join("franz").join(RULES_FILENAME);
        fs::write(
            &rules_path,
            "# Rules\n\nnever share anything with the work group\n",
        )
        .unwrap();
        let again = create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        assert!(!again.created);
        let rules = fs::read_to_string(&rules_path).unwrap();
        assert!(rules.contains("never share anything with the work group"));
    }

    #[test]
    fn create_identity_wiki_is_idempotent() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("franz").unwrap();
        let first = create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        assert!(first.created);
        let again = create_identity_wiki(&tree, &id, "Franz redux", IdentityKind::User).unwrap();
        assert!(!again.created, "second call must report preserved");
        // Title from first call must survive the second's input.
        let meta = fs::read_to_string(tree.wikis_dir().join("franz").join("_meta.md")).unwrap();
        assert!(meta.contains("title: Franz\n") || meta.contains("title: Franz "));
    }

    #[test]
    fn create_identity_wiki_group_uses_group_acl_and_type() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("famiglia").unwrap();
        create_identity_wiki(&tree, &id, "Famiglia", IdentityKind::Group).unwrap();
        let meta = fs::read_to_string(tree.wikis_dir().join("famiglia").join("_meta.md")).unwrap();
        // The group principal is derived from the `wiki-group` root type +
        // id, not stamped into the meta.
        assert!(!meta.contains("acl_default"), "{meta}");
        let tree = WikiTree::open(dir.path()).unwrap();
        let parsed = wiki_get_meta(&tree, &id).expect("get meta");
        assert_eq!(
            tree.resolve_scope_principal(&parsed).expect("resolve"),
            Principal::Group("famiglia".into())
        );
        // A group identity wiki is typed `wiki-group`, not the
        // user default — distinct type, not just a different ACL.
        assert!(
            meta.contains(&format!("wiki_type: {GROUP_IDENTITY_WIKI_TYPE}")),
            "group wiki must be wiki-group; meta was:\n{meta}"
        );
        assert_eq!(IdentityKind::User.wiki_type(), IDENTITY_WIKI_TYPE);
        assert_eq!(IdentityKind::Group.wiki_type(), GROUP_IDENTITY_WIKI_TYPE);
        // A group actor-wiki gets the same default rules.md scaffold.
        let rules = fs::read_to_string(tree.wikis_dir().join("famiglia").join(RULES_FILENAME))
            .expect("group rules.md");
        assert!(rules.contains("# Rules"));
    }

    #[test]
    fn create_identity_wiki_agent_stamps_is_agent_marker() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("hermesbot").unwrap();
        create_identity_wiki(&tree, &id, "Hermes Bot", IdentityKind::Agent).unwrap();
        let meta = fs::read_to_string(tree.wikis_dir().join("hermesbot").join("_meta.md")).unwrap();
        // An agent wiki self-describes with `is_agent`, yet stays a normal wiki-user.
        assert!(
            meta.contains("is_agent: true"),
            "agent wiki stamps is_agent; meta:\n{meta}"
        );
        assert!(meta.contains(&format!("wiki_type: {IDENTITY_WIKI_TYPE}")));
        assert!(!meta.contains("acl_default"), "{meta}");
        assert_eq!(IdentityKind::Agent.wiki_type(), IDENTITY_WIKI_TYPE);
    }

    #[test]
    fn ensure_is_agent_marker_backfills_then_is_idempotent() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        // A wiki created before the marker existed (a plain user).
        let id = WikiId::parse("oldbot").unwrap();
        create_identity_wiki(&tree, &id, "Old Bot", IdentityKind::User).unwrap();
        let path = tree.wikis_dir().join("oldbot").join("_meta.md");
        assert!(!fs::read_to_string(&path).unwrap().contains("is_agent"));

        assert!(
            ensure_is_agent_marker(&tree, &id).unwrap(),
            "first call stamps the marker"
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("is_agent: true")
        );
        assert!(
            !ensure_is_agent_marker(&tree, &id).unwrap(),
            "second call is a no-op (already marked)"
        );

        // An absent wiki → no-op, no error.
        let missing = WikiId::parse("ghost").unwrap();
        assert!(!ensure_is_agent_marker(&tree, &missing).unwrap());
    }

    /// The other shape of agent wiki: a smart consumer's operational wiki,
    /// which is a CHILD (`wikis/franz/ubestia-cc/`), so the id does not map
    /// onto a directory under `wikis/` and the root fast-path misses it. The
    /// OAuth sign-in flow stamps exactly this shape.
    #[test]
    fn ensure_is_agent_marker_reaches_a_nested_operational_wiki() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let parent = WikiId::parse("franz").unwrap();
        create_identity_wiki(&tree, &parent, "Franz", IdentityKind::User).unwrap();
        let child_dir = tree.wikis_dir().join("franz").join("ubestiacc");
        fs::create_dir_all(&child_dir).unwrap();
        atomic_write(
            &child_dir.join(META_FILENAME),
            b"---\nwiki_id: franz-ubestiacc\nwiki_type: agent\nparent_wiki_id: franz\n\
              slug: ubestiacc\ntitle: Claude Code\nsmart: true\n---\n",
        )
        .unwrap();

        let child = WikiId::parse("franz-ubestiacc").unwrap();
        // The id is not a path, so the root-keyed entry point cannot reach it —
        // and must not go hunting for it either, since it runs per request on
        // the connect path.
        assert!(
            !ensure_is_agent_marker(&tree, &child).unwrap(),
            "the root entry point stays on the root path and finds nothing"
        );
        let dir = tree
            .locate(&child)
            .expect("locate child")
            .abs_dir()
            .to_owned();
        assert!(
            ensure_is_agent_marker_in(&dir).unwrap(),
            "the caller resolves the child, then stamps it"
        );
        let raw = fs::read_to_string(child_dir.join(META_FILENAME)).unwrap();
        assert!(raw.contains("is_agent: true"), "{raw}");
        // The parent is untouched — the marker is per-wiki, and the human who
        // owns the operational wiki is not an agent.
        let parent_raw =
            fs::read_to_string(tree.wikis_dir().join("franz").join(META_FILENAME)).unwrap();
        assert!(!parent_raw.contains("is_agent"), "{parent_raw}");
    }

    /// The `{subject}` directive the index-writing prompts substitute: present
    /// only for an agent's wiki, so a human's index keeps the voice it has.
    #[test]
    fn subject_directive_speaks_only_for_an_agent_wiki() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let human = WikiId::parse("franz").unwrap();
        let agent = WikiId::parse("hermesbot").unwrap();
        create_identity_wiki(&tree, &human, "Franz", IdentityKind::User).unwrap();
        create_identity_wiki(&tree, &agent, "Hermes", IdentityKind::Agent).unwrap();

        assert!(subject_directive(&wiki_get_meta(&tree, &human).unwrap()).is_empty());
        let directive = subject_directive(&wiki_get_meta(&tree, &agent).unwrap());
        assert!(directive.contains("FIRST PERSON"), "{directive}");
        assert!(directive.contains("autobiography"), "{directive}");
    }

    #[test]
    fn meta_round_trips_is_agent_and_lean_wikis_omit_it() {
        let agent = "---\nwiki_id: hermes\nwiki_type: wiki-user\nparent_wiki_id: null\n\
                     slug: hermes\ntitle: Hermes\nacl_default: 'user:hermes'\nis_agent: true\n---\n";
        let (meta, _) = WikiMeta::parse(Path::new("_meta.md"), agent).expect("parse agent");
        assert!(meta.is_agent);
        assert!(
            meta.render("").unwrap().contains("is_agent: true"),
            "is_agent round-trips through parse → render"
        );

        let plain = "---\nwiki_id: alice\nwiki_type: wiki-user\nparent_wiki_id: null\n\
                     slug: alice\ntitle: Alice\nacl_default: 'user:alice'\n---\n";
        let (pmeta, _) = WikiMeta::parse(Path::new("_meta.md"), plain).expect("parse plain");
        assert!(!pmeta.is_agent);
        assert!(
            !pmeta.render("").unwrap().contains("is_agent"),
            "a wiki without the key stays lean (no is_agent emitted)"
        );
    }

    #[test]
    fn create_identity_wiki_is_locatable_after_open() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let mut tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("franz").unwrap();
        create_identity_wiki(&tree, &id, "Franz", IdentityKind::User).unwrap();
        // Re-open the tree so the new wiki is picked up by the registry.
        tree = WikiTree::open(dir.path()).unwrap();
        let handle = tree.locate(&id).expect("locate");
        assert_eq!(handle.meta().wiki_type, IDENTITY_WIKI_TYPE);
    }

    // ---------- write_wiki_dir ----------

    /// Build a minimal `WikiMeta` for the helper tests.
    fn typed_meta(
        wiki_id: &WikiId,
        parent: Option<&WikiId>,
        slug: &str,
        wiki_type: &str,
    ) -> WikiMeta {
        WikiMeta {
            wiki_id: wiki_id.clone(),
            wiki_type: wiki_type.to_owned(),
            parent_wiki_id: parent.cloned(),
            slug: WikiSlug::parse(slug).unwrap(),
            title: "T".to_owned(),
            scope: None,
            shared_with: Vec::new(),
            style_overrides: serde_yaml::Mapping::new(),
            keywords: serde_yaml::Mapping::new(),
            children: Vec::new(),
            promoted_from: None,
            no_archive: false,
            smart: false,
            is_agent: false,
            created: Some("2026-06-03T00:00:00+00:00".to_owned()),
            updated: None,
            extra: serde_yaml::Mapping::new(),
        }
    }

    /// A project wiki's `scope` IS its door sign — the one line that makes
    /// it reachable from a turn that never names it.
    #[test]
    fn door_description_reads_a_project_wikis_scope() {
        let id = WikiId::parse("alice-proj").unwrap();
        let mut meta = typed_meta(&id, None, "proj", "project");
        meta.smart = true;
        meta.scope = Some("  The print-shop ordering system.  ".to_owned());
        assert_eq!(
            meta.door_description().as_deref(),
            Some("The print-shop ordering system."),
            "trimmed, and taken verbatim otherwise"
        );

        meta.scope = Some("   ".to_owned());
        assert_eq!(
            meta.door_description(),
            None,
            "whitespace is not a description — an empty column is the honest answer"
        );
    }

    /// An agent's operational notebook is a smart wiki too, and is nobody's
    /// door: it holds one agent's working notes, not a subject anyone asks
    /// about. Pinned on BOTH markers, because production carries a wiki with
    /// `wiki_type: agent` and no `is_agent` flag on disk.
    #[test]
    fn door_description_declines_an_agent_wiki_on_either_marker() {
        let id = WikiId::parse("alice-cc").unwrap();

        let mut flagged = typed_meta(&id, None, "cc", "project");
        flagged.smart = true;
        flagged.is_agent = true;
        flagged.scope = Some("my working notes".to_owned());
        assert_eq!(flagged.door_description(), None, "is_agent alone is enough");

        let mut typed = typed_meta(&id, None, "cc", AGENT_WIKI_TYPE);
        typed.smart = true;
        typed.scope = Some("my working notes".to_owned());
        assert_eq!(
            typed.door_description(),
            None,
            "wiki_type alone is enough — the on-disk marker may be missing"
        );
    }

    /// A standard wiki's `scope` is the classifier's placement signal and
    /// has nothing to do with doors; reading it as one would turn every
    /// notebook into a project.
    #[test]
    fn door_description_declines_a_standard_wiki() {
        let id = WikiId::parse("alice").unwrap();
        let mut meta = typed_meta(&id, None, "alice", "wiki-user");
        meta.scope = Some("everything about Alice".to_owned());
        assert!(!meta.smart);
        assert_eq!(meta.door_description(), None);
    }

    #[test]
    fn write_wiki_dir_top_level_writes_meta_and_index() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("contacts").unwrap();
        let meta = typed_meta(&id, None, "contacts", "wiki-contacts");
        let out = write_wiki_dir(&tree, &meta, "# Contacts\n", false).unwrap();
        assert_eq!(out, tree.wikis_dir().join("contacts"));
        let written = fs::read_to_string(out.join("_meta.md")).unwrap();
        assert!(written.contains("wiki_id: contacts"));
        assert!(written.contains("wiki_type: wiki-contacts"));
        assert!(
            fs::read_to_string(out.join("index.md"))
                .unwrap()
                .contains("# Contacts")
        );
    }

    #[test]
    fn write_wiki_dir_child_lands_under_parent() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let mut tree = WikiTree::open(dir.path()).unwrap();
        let parent = WikiId::parse("famiglia").unwrap();
        create_identity_wiki(&tree, &parent, "Famiglia", IdentityKind::Group).unwrap();
        // Re-open so the registry can resolve the parent handle.
        tree = WikiTree::open(dir.path()).unwrap();
        let child = WikiId::child_of(&parent, &WikiSlug::parse("contatti").unwrap());
        let meta = typed_meta(&child, Some(&parent), "contatti", "wiki-contacts");
        let out = write_wiki_dir(&tree, &meta, "# Contatti\n", false).unwrap();
        assert_eq!(out, tree.wikis_dir().join("famiglia").join("contatti"));
        assert!(out.join("_meta.md").exists());
    }

    #[test]
    fn root_collector_index_lists_only_top_level_marker_less() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let mut tree = WikiTree::open(dir.path()).unwrap();
        // Two top-level user wikis, created out of alpha order to exercise
        // the sort.
        create_identity_wiki(
            &tree,
            &WikiId::parse("bob").unwrap(),
            "Bob",
            IdentityKind::User,
        )
        .unwrap();
        create_identity_wiki(
            &tree,
            &WikiId::parse("alice").unwrap(),
            "Alice",
            IdentityKind::User,
        )
        .unwrap();
        tree = WikiTree::open(dir.path()).unwrap();
        // A nested sub-wiki under alice (depth 3) — must be excluded.
        let alice = WikiId::parse("alice").unwrap();
        let child = WikiId::child_of(&alice, &WikiSlug::parse("sub").unwrap());
        write_wiki_dir(
            &tree,
            &typed_meta(&child, Some(&alice), "sub", "wiki-tech"),
            "# Sub\n",
            false,
        )
        .unwrap();
        // A top-level smart wiki — must be excluded by the smart flag.
        let mut comp = typed_meta(
            &WikiId::parse("comp").unwrap(),
            None,
            "comp",
            "wiki-companion",
        );
        comp.smart = true;
        comp.title = "Comp".to_owned();
        write_wiki_dir(&tree, &comp, "# Comp\n", false).unwrap();
        tree = WikiTree::open(dir.path()).unwrap();

        write_root_collector_index(&tree).unwrap();

        let body = fs::read_to_string(tree.wikis_dir().join("index.md")).unwrap();
        assert!(body.starts_with("# Wikis\n"), "{body}");
        // Top-level only, Obsidian path+alias, sorted by title.
        let alice_at = body.find("[[alice/index|Alice]]").expect(&body);
        let bob_at = body.find("[[bob/index|Bob]]").expect(&body);
        assert!(alice_at < bob_at, "alphabetical by title: {body}");
        // Excluded: the nested sub-wiki and the smart wiki.
        assert!(!body.contains("sub/index"), "nested excluded: {body}");
        assert!(!body.contains("comp/index"), "smart wiki excluded: {body}");
        // Marker-less: never a fact carrier.
        assert!(!body.contains("{{"), "no markers: {body}");
        // Invisible to wiki enumeration: walk() keys on _meta.md, so the
        // loose index.md adds no phantom wiki (alice, bob, alice-sub, comp).
        assert_eq!(tree.walk().unwrap().len(), 4, "index.md must not be a wiki");
    }

    #[test]
    fn write_wiki_dir_refuses_parentless_when_requires_parent() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("agenda").unwrap();
        let meta = typed_meta(&id, None, "agenda", "wiki-cron");
        let err = write_wiki_dir(&tree, &meta, "# Agenda\n", /* requires_parent */ true)
            .expect_err("child-only type must refuse top-level");
        assert!(matches!(err, WikiError::RequiresParent { .. }));
        assert!(
            !tree.wikis_dir().join("agenda").exists(),
            "nothing written on refusal"
        );
    }

    #[test]
    fn write_wiki_dir_is_additive_only() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let id = WikiId::parse("contacts").unwrap();
        let meta = typed_meta(&id, None, "contacts", "wiki-contacts");
        write_wiki_dir(&tree, &meta, "# Contacts\n", false).unwrap();
        let err = write_wiki_dir(&tree, &meta, "# Overwrite\n", false)
            .expect_err("second create must refuse");
        assert!(matches!(err, WikiError::AlreadyExists { .. }));
        // First body survives.
        assert!(
            fs::read_to_string(tree.wikis_dir().join("contacts").join("index.md"))
                .unwrap()
                .contains("# Contacts")
        );
    }

    // ---------- atomic_write ----------

    #[test]
    fn atomic_write_creates_parent_and_writes() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested/sub/file.md");
        atomic_write(&target, b"hello").expect("write");
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_overwrites_existing_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.md");
        fs::write(&target, "before").unwrap();
        atomic_write(&target, b"after").expect("write");
        assert_eq!(fs::read_to_string(&target).unwrap(), "after");
    }

    #[test]
    fn atomic_write_drops_marker_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.md");
        atomic_write(&target, b"x").expect("write");
        let marker = crate::watcher::marker_path_for(&target);
        assert!(
            !marker.exists(),
            "marker must be cleaned up after successful write"
        );
    }

    // ---------- WikiTree + WikiHandle ----------

    fn write_meta(dir: &Path, meta_yaml: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(META_FILENAME), meta_yaml).unwrap();
    }

    fn root_meta(slug: &str) -> String {
        format!(
            "---\n\
             wiki_id: {slug}\n\
             wiki_type: wiki-user\n\
             parent_wiki_id: null\n\
             slug: {slug}\n\
             title: {slug}\n\
             acl_default: 'user:{slug}'\n\
             ---\n"
        )
    }

    fn child_meta(parent_id: &str, slug: &str) -> String {
        format!(
            "---\n\
             wiki_id: {parent_id}-{slug}\n\
             wiki_type: wiki-cliente\n\
             parent_wiki_id: {parent_id}\n\
             slug: {slug}\n\
             title: {slug}\n\
             acl_default: inherit\n\
             ---\n"
        )
    }

    #[test]
    fn wiki_tree_open_creates_wikis_dir() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        assert!(tree.wikis_dir().exists());
        assert!(tree.wikis_dir().ends_with("wikis"));
    }

    #[test]
    fn wiki_tree_walk_finds_root_and_subwikis() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );

        let walk = tree.walk().expect("walk");
        assert_eq!(walk.len(), 2);
        let ids: Vec<&str> = walk.iter().map(|d| d.meta.wiki_id.as_str()).collect();
        assert!(ids.contains(&"alice"));
        assert!(ids.contains(&"alice-acmecorp"));
    }

    #[test]
    fn wiki_tree_walk_skips_dirs_without_meta() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        // Directory without a _meta.md must not produce a discovery row.
        fs::create_dir_all(tree.wikis_dir().join("orphan")).unwrap();
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let walk = tree.walk().expect("walk");
        assert_eq!(walk.len(), 1);
        assert_eq!(walk[0].meta.wiki_id.as_str(), "alice");
    }

    #[test]
    fn locate_by_id_returns_handle_with_meta() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        assert_eq!(h.meta().wiki_id.as_str(), "alice");
        assert!(h.abs_dir().ends_with("wikis/alice"));
    }

    #[test]
    fn locate_unknown_id_errors() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        let err = tree
            .locate(&WikiId::parse("does-not-exist").unwrap())
            .expect_err("must error");
        assert!(matches!(err, WikiError::WikiNotFound { .. }));
    }

    #[test]
    fn handle_read_and_write_round_trip() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        h.write_page(Path::new("intro.md"), "hello world\n")
            .expect("write");
        let back = h.read_page(Path::new("intro.md")).expect("read");
        assert_eq!(back, "hello world\n");
    }

    #[test]
    fn handle_read_missing_page_errors_404() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        let err = h.read_page(Path::new("never.md")).expect_err("must 404");
        assert!(matches!(err, WikiError::PageNotFound { .. }));
    }

    #[test]
    fn handle_rejects_path_traversal_on_read() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        for bad in &[
            "../escape.md",
            "/abs/escape.md",
            "deep/../escape.md",
            "weird name.md",
            ".hidden.md",
        ] {
            let res = h.read_page(Path::new(bad));
            assert!(
                matches!(res, Err(WikiError::UnsafePagePath { .. })),
                "{bad}"
            );
        }
        // Uppercase is safe (smart wikis keep imported casing) — a
        // missing `Caps.md` is a 404, not an unsafe path.
        let res = h.read_page(Path::new("Caps.md"));
        assert!(matches!(res, Err(WikiError::PageNotFound { .. })));
    }

    // ---------- resolve_scope_principal ----------

    #[test]
    fn resolve_scope_principal_of_identity_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        let p = tree.resolve_scope_principal(h.meta()).expect("resolve");
        assert_eq!(p, Principal::User("alice".into()));
    }

    #[test]
    fn resolve_scope_principal_walks_to_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );
        let h = tree
            .locate(&WikiId::parse("alice-acmecorp").unwrap())
            .expect("locate");
        let p = tree.resolve_scope_principal(h.meta()).expect("resolve");
        assert_eq!(p, Principal::User("alice".into()));
    }

    #[test]
    fn resolve_scope_principal_walks_multi_hop_to_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );
        // Third level: acmecorp/widget.
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp/widget"),
            &child_meta("alice-acmecorp", "widget"),
        );
        let h = tree
            .locate(&WikiId::parse("alice-acmecorp-widget").unwrap())
            .expect("locate");
        let p = tree.resolve_scope_principal(h.meta()).expect("resolve");
        assert_eq!(p, Principal::User("alice".into()));
    }

    /// A **group** identity root derives a group principal.
    #[test]
    fn resolve_scope_principal_group_root() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        let famiglia = WikiId::parse("famiglia").unwrap();
        create_identity_wiki(&tree, &famiglia, "Famiglia", IdentityKind::Group).unwrap();
        let tree = WikiTree::open(dir.path()).expect("reopen");
        let h = tree.locate(&famiglia).expect("locate");
        let p = tree.resolve_scope_principal(h.meta()).expect("resolve");
        assert_eq!(p, Principal::Group("famiglia".into()));
    }

    #[test]
    fn list_pages_excludes_meta_and_descends_into_plain_subdirs_only() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        // Sub-wiki: must NOT be enumerated as a page of alice.
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );
        // Plain prose subdir: pages ARE enumerated.
        fs::create_dir_all(tree.wikis_dir().join("alice/recipes")).unwrap();
        fs::write(tree.wikis_dir().join("alice/recipes/pasta.md"), "pasta\n").unwrap();
        fs::write(tree.wikis_dir().join("alice/intro.md"), "intro\n").unwrap();
        // Files inside the sub-wiki must not appear.
        fs::write(
            tree.wikis_dir().join("alice/acmecorp/note.md"),
            "should not show\n",
        )
        .unwrap();

        let h = tree
            .locate(&WikiId::parse("alice").unwrap())
            .expect("locate");
        let pages = h.list_pages().expect("list");
        let rels: Vec<String> = pages
            .iter()
            .map(|p| p.rel_path.to_string_lossy().into_owned())
            .collect();
        assert!(rels.iter().any(|r| r == "intro.md"), "{rels:?}");
        assert!(
            rels.iter()
                .any(|r| r.ends_with("recipes/pasta.md") || r.ends_with("recipes\\pasta.md"))
        );
        assert!(!rels.iter().any(|r| r.contains("acmecorp")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.ends_with("_meta.md")), "{rels:?}");
    }

    // ---------- catalog ----------

    #[test]
    fn catalog_groups_by_wiki_type() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        write_meta(&tree.wikis_dir().join("bob"), &root_meta("bob"));
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );

        let cat = wiki_catalog_list(&tree).expect("catalog");
        assert_eq!(cat["wiki-user"].len(), 2);
        assert_eq!(cat["wiki-cliente"].len(), 1);
        assert_eq!(cat["wiki-user"][0].wiki_id.as_str(), "alice");
        assert_eq!(cat["wiki-user"][1].wiki_id.as_str(), "bob");
    }

    #[test]
    fn render_root_index_groups_by_type_and_links_wikis() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        write_meta(&tree.wikis_dir().join("alice"), &root_meta("alice"));
        write_meta(&tree.wikis_dir().join("bob"), &root_meta("bob"));
        write_meta(
            &tree.wikis_dir().join("alice/acmecorp"),
            &child_meta("alice", "acmecorp"),
        );

        let cat = wiki_catalog_list(&tree).expect("catalog");
        let rendered = render_root_index(&cat);

        // Grouped under a per-type heading, each wiki a followable [[wikilink]].
        assert!(rendered.contains("## wiki-user"), "{rendered}");
        assert!(rendered.contains("## wiki-cliente"), "{rendered}");
        assert!(rendered.contains("[[alice]]"), "{rendered}");
        assert!(rendered.contains("[[bob]]"), "{rendered}");
        // BTreeMap key order: "wiki-cliente" sorts before "wiki-user".
        let cliente = rendered.find("## wiki-cliente").expect("cliente heading");
        let user = rendered.find("## wiki-user").expect("user heading");
        assert!(cliente < user, "{rendered}");
    }

    #[test]
    fn render_root_index_empty_catalog_is_empty() {
        let by_type: BTreeMap<String, Vec<CatalogEntry>> = BTreeMap::new();
        assert!(render_root_index(&by_type).is_empty());
    }

    #[test]
    fn catalog_and_root_index_carry_summary_and_keywords() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).expect("open");
        // `summary` is an unknown key → rides in WikiMeta.extra; `keywords` is a
        // first-class mapping field.
        let yaml = "---\n\
                    wiki_id: cook\n\
                    wiki_type: wiki-user\n\
                    parent_wiki_id: null\n\
                    slug: cook\n\
                    title: Cooking\n\
                    acl_default: 'user:cook'\n\
                    summary: A wiki about recipes and meals.\n\
                    keywords:\n  topics: pasta, cena\n\
                    ---\n";
        write_meta(&tree.wikis_dir().join("cook"), yaml);

        let cat = wiki_catalog_list(&tree).expect("catalog");
        let entry = &cat["wiki-user"][0];
        assert_eq!(
            entry.summary.as_deref(),
            Some("A wiki about recipes and meals.")
        );
        assert_eq!(entry.keywords, vec!["topics=pasta, cena".to_owned()]);

        let rendered = render_root_index(&cat);
        assert!(
            rendered.contains("A wiki about recipes and meals."),
            "{rendered}"
        );
        assert!(rendered.contains("topics=pasta, cena"), "{rendered}");
    }

    // ---------- safe_page_path ----------

    #[test]
    fn safe_page_path_accepts_simple_names() {
        assert!(is_safe_page_path(Path::new("intro.md")));
        assert!(is_safe_page_path(Path::new("recipes/pasta.md")));
        assert!(is_safe_page_path(Path::new("adr-024/proposal.md")));
        assert!(is_safe_page_path(Path::new("Caps.md")));
        assert!(is_safe_page_path(Path::new("Docs/API-Reference.md")));
    }

    #[test]
    fn safe_page_path_rejects_unsafe_forms() {
        assert!(!is_safe_page_path(Path::new("")));
        assert!(!is_safe_page_path(Path::new("..")));
        assert!(!is_safe_page_path(Path::new("../escape.md")));
        assert!(!is_safe_page_path(Path::new("/abs/escape.md")));
        assert!(!is_safe_page_path(Path::new("space name.md")));
        assert!(!is_safe_page_path(Path::new(".hidden")));
        assert!(!is_safe_page_path(Path::new("città.md")));
    }

    // ---------- case hazards / conflicts ----------

    #[test]
    fn case_hazard_flags_reserved_variants_and_upper_md_extension() {
        assert!(page_path_case_hazard(Path::new("_Meta.md")).is_some());
        assert!(page_path_case_hazard(Path::new("sub/_META.md")).is_some());
        assert!(page_path_case_hazard(Path::new("RULES.md")).is_some());
        assert!(page_path_case_hazard(Path::new("_Briefing.md")).is_some());
        assert!(page_path_case_hazard(Path::new("notes.MD")).is_some());
        assert!(page_path_case_hazard(Path::new("notes.Md")).is_some());
        // Byte-exact reserved names are caller policy, not a hazard.
        assert!(page_path_case_hazard(Path::new("_meta.md")).is_none());
        assert!(page_path_case_hazard(Path::new("rules.md")).is_none());
        assert!(page_path_case_hazard(Path::new("_briefing.md")).is_none());
        assert!(page_path_case_hazard(Path::new("Setup.md")).is_none());
        assert!(page_path_case_hazard(Path::new("Docs/Overview.md")).is_none());
    }

    #[test]
    fn case_conflict_detects_existing_case_variants() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("setup.md"), "x").unwrap();
        std::fs::create_dir(dir.join("recipes")).unwrap();
        std::fs::write(dir.join("recipes/pasta.md"), "x").unwrap();

        // Same name, different case → conflict naming the existing file.
        // Asserted on every filesystem: the guard reads the directory
        // listing, so a folding one answers the same as a strict one.
        let c = page_case_conflict(dir, Path::new("Setup.md")).unwrap();
        assert!(c.contains("setup.md"), "{c}");
        // Directory component case variant → conflict too.
        assert!(page_case_conflict(dir, Path::new("Recipes/tarta.md")).is_some());
        // Byte-exact target (exists or not) → no conflict.
        assert!(page_case_conflict(dir, Path::new("setup.md")).is_none());
        assert!(page_case_conflict(dir, Path::new("recipes/pasta.md")).is_none());
        assert!(page_case_conflict(dir, Path::new("recipes/risotto.md")).is_none());
        assert!(page_case_conflict(dir, Path::new("brand-new.md")).is_none());
        // Nested path under a missing directory → nothing to collide with.
        assert!(page_case_conflict(dir, Path::new("new-dir/page.md")).is_none());
    }

    #[test]
    fn resolve_case_insensitive_prefers_exact_then_unique_fold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir(dir.join("Docs")).unwrap();
        std::fs::write(dir.join("Docs/Setup.md"), "x").unwrap();

        // Exact hit resolves as-is.
        assert_eq!(
            resolve_page_case_insensitive(dir, Path::new("Docs/Setup.md")),
            Some(PathBuf::from("Docs/Setup.md"))
        );
        // Missing target stays unresolved; a directory is not a page.
        assert!(resolve_page_case_insensitive(dir, Path::new("docs/other.md")).is_none());
        assert!(resolve_page_case_insensitive(dir, Path::new("docs")).is_none());
        // The rest is only meaningful where the filesystem keeps the
        // spellings apart: elsewhere the byte-exact probe already hits,
        // and two case-variant entries cannot both exist to be ambiguous.
        if !fs_distinguishes_case(dir) {
            return;
        }
        // Case-drifted link resolves to the on-disk spelling.
        assert_eq!(
            resolve_page_case_insensitive(dir, Path::new("docs/setup.md")),
            Some(PathBuf::from("Docs/Setup.md"))
        );
        // Two entries differing only by case → ambiguous, refuse.
        std::fs::write(dir.join("Docs/SETUP.md"), "x").unwrap();
        assert!(resolve_page_case_insensitive(dir, Path::new("docs/setup.md")).is_none());
    }
}
