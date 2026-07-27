// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smart-wiki authoritative writes (H family of MCP tools).
//!
//! This module is the headless core of `wiki_admin_push` /
//! `wiki_admin_pull`: a smart consumer (Claude Code, Cowork, any
//! MCP-compatible agent with its own LLM subscription) takes
//! ownership of a smart-family wiki and writes pre-rendered
//! pages to it without invoking the server-side LLM.
//!
//! ## Authorisation invariants
//!
//! Every entry point enforces three gates documented in
//! [`protocollo.md §2 + §H`] and [`modello-memoria.md §9`]:
//!
//! 1. `token.consumer_class == Smart` — `wiki_admin_*` is the
//!    smart-consumer surface. Standard consumers get
//!    [`AdminError::RequiresSmart`].
//! 2. `wiki.owner_user == token.owner_user` — a smart consumer is
//!    custodian of writes only for wikis its own user owns. Cross-user
//!    write attempts yield [`AdminError::WikiOwnedByOtherUser`].
//! 3. The target wiki's `_meta` smart flag is `true` — the per-wiki bool
//!    that replaced the retired `wiki_types_registry`, derived on
//!    `create` from the `wiki-companion` type-string prefix. Standard
//!    wikis continue to accept writes via `wiki_ingest_message` only;
//!    [`AdminError::WikiTypeNotAdminWritable`] is the rejection here.
//!
//! ## Op log
//!
//! Every push / pull writes a row into `wiki_admin_op_log` (migration
//! `0022_wiki_admin_op_log.sql`). The MVP records who/what/when. The
//! `expected_op_log_head` optimistic-concurrency check **is enforced**
//! on `upsert` (a push carrying it is rejected with
//! [`AdminError::ConflictingOpLogHead`] when a newer write op landed in
//! the gap — see [`push_upsert`]); the `since_op_log_id` delta-pull
//! described in [`tool-reference.md §H`] is still deferred (full pull
//! works; the optimisation matters once wikis grow past hundreds of
//! pages).
//!
//! ## What is intentionally NOT here (MVP scope)
//!
//! - `snapshot_replace` mode (additive complexity beyond `upsert` —
//!   a snapshot is `upsert + deletes` for the missing paths; the
//!   smart consumer can compose it on its side).
//! - `since_op_log_id` delta pull (full pull works; the optimisation
//!   matters once wikis grow past hundreds of pages).
//! - `folder_structure.recommended` deviation warnings (the spec lists
//!   them as `warnings[]` on the response — the folder-shape validator
//!   is still unwritten). The field itself is **no longer empty**: it
//!   carries the per-page density warnings of [`shape_warnings`].
//! - `fact_index` re-indexing — the watcher pipeline catches the
//!   file changes asynchronously. For test setups without a watcher,
//!   the consumer can drive a manual reindex; we don't block writes
//!   on it.
//!
//! [`protocollo.md §2 + §H`]: ../../../docs/protocol/tool-reference.md
//! [`modello-memoria.md §9`]: ../../../docs/concepts/memory-model.md
//! [`tool-reference.md §H`]: ../../../docs/protocol/tool-reference.md

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::jwt::ConsumerClass;
use crate::types::{Principal, WikiId, WikiIdParseError, WikiSlug, WikiSlugParseError};
use crate::wiki::{META_FILENAME, WikiError, WikiHandle, WikiMeta, WikiTree, atomic_write};
/// Who is performing the write — drives the gate matrix and the
/// `actor_kind` column of `wiki_admin_op_log`.
///
/// The op-log is no longer smart-wiki-only, so the
/// dashboard textual editor and future system-generated compensation
/// rows can be discriminated from a `wiki_admin_push` issued by a
/// smart consumer over MCP.
///
/// Gate matrix:
///
/// | `actor_kind`      | `consumer_class=smart` required? | smart-family wiki required? | owner-match required? |
/// |-------------------|----------------------------------|----------------------------------------|-----------------------|
/// | `SmartConsumer`   | yes (`AdminError::RequiresSmart`) | yes (`AdminError::WikiTypeNotAdminWritable`) | yes                   |
/// | `Dashboard`       | no                               | **relaxed** — any wiki     | yes                   |
/// | `System`          | no                               | no — reserved for the revert handler | n/a (handler-driven)  |
///
/// `System` is threaded through the API but its write logic lives in
/// `wiki_admin::op_revert`; see the module docstring for the
/// distribution of responsibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActorKind {
    /// MCP-side write from a `consumer_class=smart` token. The original
    /// behaviour, default for every existing call site.
    SmartConsumer,
    /// Dashboard-side write from the textual page editor. The
    /// smart-family gate is relaxed: any wiki the
    /// operator owns can be edited from the dashboard.
    Dashboard,
    /// System-generated compensation row produced by the revert
    /// handler. Threaded through `record_op_log` only — no
    /// `push` call ever passes this variant today; reserved for
    /// future use.
    System,
}

impl ActorKind {
    /// Canonical wire string stored in `wiki_admin_op_log.actor_kind`.
    /// Matches the CHECK constraint in migration 0027.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::SmartConsumer => "smart_consumer",
            Self::Dashboard => "dashboard",
            Self::System => "system",
        }
    }
}

/// Errors surfaced by the public [`push`] / [`pull`] entry points.
///
/// Mapped 1-to-1 onto the `ToolErrorClass` variants at the MCP
/// boundary so the wire codes match `tool-reference.md §H` verbatim.
#[derive(Debug, Error)]
pub enum AdminError {
    /// Caller's JWT does not carry `consumer_class=smart`.
    #[error("requires consumer_class=smart (see protocollo.md §2)")]
    RequiresSmart,
    /// Target wiki's owner is not the caller's user.
    #[error("wiki {wiki_id} is owned by user:{owner}, but token belongs to user:{caller_owner}")]
    WikiOwnedByOtherUser {
        /// Target wiki id.
        wiki_id: WikiId,
        /// The wiki's owning user id — the scope principal derived from its
        /// path to the root identity wiki.
        owner: String,
        /// User id derived from `token.sender_id`.
        caller_owner: String,
    },
    /// Target wiki's `wiki_type` is not in the smart family.
    #[error(
        "wiki_type {wiki_type:?} is not in the smart family (use wiki_ingest_message for standard wikis)"
    )]
    WikiTypeNotAdminWritable {
        /// The non-smart `wiki_type` that was targeted.
        wiki_type: String,
    },
    /// Target wiki's derived scope principal is something other than a
    /// single user (Global or Group). MVP smart-wikis require a user owner.
    #[error("wiki {wiki_id} has owner {acl_default:?}; smart-wikis require a user owner")]
    AmbiguousOwner {
        /// Target wiki id.
        wiki_id: WikiId,
        /// The derived scope principal that wasn't a single user.
        acl_default: String,
    },
    /// A child-only `wiki_type` was asked to be created with no
    /// `parent_wiki_id`. The smart family is the user of this
    /// gate: a top-level smart wiki would lose the
    /// ACL-inheritance it relies
    /// on. Wire form: `400 wiki_type_requires_parent`.
    ///
    /// The message names the value to pass. A first-connect agent hit
    /// this live and could not act on it: the old text said what was
    /// wrong and not what to do, and the answer — "your own root wiki,
    /// which is your `sender_id`" — is something the server knows and
    /// the caller has to guess.
    #[error(
        "a smart wiki must be created under your own root wiki: pass parent_wiki_id={expected_parent:?} \
         (wiki_type {wiki_type:?} cannot be created as top-level)"
    )]
    WikiTypeRequiresParent {
        /// The `wiki_type` that refused a parent-less create.
        wiki_type: String,
        /// The `parent_wiki_id` the caller should have passed — its own
        /// root wiki, derived from the authenticated `sender_id`.
        expected_parent: String,
    },
    /// Target wiki was not found (pull / upsert path).
    #[error("wiki {0} not found")]
    NotFound(WikiId),
    /// Generic input validation (bad slug, missing required field,
    /// duplicate page path, …).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Another smart consumer holds a `wiki_admin_lease` on this
    /// wiki — see `wiki_admin_lease_acquire`.
    #[error(
        "wiki {wiki_id} is locked by lease held by consumer {held_by_consumer_id:?} until {expires_at}"
    )]
    WikiLockedByLease {
        /// Target wiki id.
        wiki_id: WikiId,
        /// `consumer_id` of the holding device, when set.
        held_by_consumer_id: Option<String>,
        /// `sender_id` of the holding user (always set).
        held_by_sender_id: String,
        /// ISO-8601 UTC `expires_at` of the holding lease.
        expires_at: String,
    },
    /// `mark_processed` carried a briefing-item id that does not exist
    /// or does not belong to the wiki this push targets. Validated
    /// server-side so the smart consumer cannot mark
    /// arbitrary rows as recepiti.
    #[error("unknown briefing item id {bi_id:?} for wiki {wiki_id}")]
    UnknownBriefingItemId {
        /// Canonical `bi_<N>` string carried by the failing entry.
        bi_id: String,
        /// Target wiki id of the push.
        wiki_id: WikiId,
    },
    /// `mark_processed` list exceeded
    /// [`MARK_PROCESSED_CAP_PER_PUSH`] entries. The per-push
    /// fan-out is capped at the same value as the briefing rate-limit cap so a
    /// single push cannot drain an entire hour of notify traffic at once.
    #[error("too many briefing item ids in mark_processed (got {received}, cap {cap})")]
    TooManyBriefingItems {
        /// Number of entries the caller passed.
        received: usize,
        /// Server cap (currently [`MARK_PROCESSED_CAP_PER_PUSH`]).
        cap: usize,
    },
    /// `expected_op_log_head` was set on an `Upsert` but a newer write
    /// op landed on the wiki since the caller synced — a concurrent
    /// device pushed in the same window. The caller should
    /// `wiki_admin_pull`, re-diff, and re-push. Wire form:
    /// `409 conflicting_op_log_head`.
    #[error(
        "wiki {wiki_id} moved on: expected op_log head {expected}, but the latest write op is {actual} (pull, re-diff, re-push)"
    )]
    ConflictingOpLogHead {
        /// Target wiki id of the push.
        wiki_id: WikiId,
        /// `op_id` the caller believed was current (`expected_op_log_head`).
        expected: i64,
        /// `op_id` of the newest write op the server actually holds.
        actual: i64,
    },
    /// Bubbling from the wiki module.
    #[error("wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Bubbling from `WikiId` parser.
    #[error("wiki_id: {0}")]
    WikiIdParse(#[from] WikiIdParseError),
    /// Bubbling from `WikiSlug` parser.
    #[error("slug: {0}")]
    SlugParse(#[from] WikiSlugParseError),
    /// Database failure (`op_log` insert or registry lookup).
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    /// Raw filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Maximum number of briefing items a single `wiki_admin_push` may
/// mark as processed via [`PushRequest::mark_processed`].
///
/// Matches the per-wiki `notify` rate-limit cap (`50/wiki/h`) so a
/// single push cannot drain an entire hour of inbound notify traffic
/// in one shot — a defensive bound against a runaway smart consumer.
/// Over the cap → [`AdminError::TooManyBriefingItems`] before the
/// transaction is opened.
pub const MARK_PROCESSED_CAP_PER_PUSH: usize = 50;

/// Push mode requested by the consumer.
///
/// `SnapshotReplace` from the spec is intentionally deferred — see
/// the module docstring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    /// Create a new smart-wiki under `parent_wiki_id`. Fails if
    /// the derived `wiki_id` already exists.
    Create,
    /// Upsert pages into an existing smart-wiki. `wiki_id` is
    /// required; missing pages stay untouched (use `deletes` to
    /// remove).
    Upsert,
}

impl PushMode {
    /// Canonical wire string used in `wiki_admin_op_log.op_kind`.
    #[must_use]
    pub const fn op_log_kind(self) -> &'static str {
        match self {
            Self::Create => "push_create",
            Self::Upsert => "push_upsert",
        }
    }

    /// Wire mode string (`"create"` / `"upsert"`) for the audit row.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Upsert => "upsert",
        }
    }
}

/// One page in a [`PushRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPage {
    /// Relative path inside the wiki directory (e.g.
    /// `"modules/auth.md"`). Validated against
    /// [`crate::wiki::is_safe_page_path`].
    pub path: String,
    /// Full markdown body, including any frontmatter. Written
    /// verbatim — mwe-mcp does not re-render.
    pub content: String,
}

/// All inputs for a [`push`] call. Fields populated depend on
/// [`PushMode`]:
///
/// - `Create` requires `parent_wiki_id`, `slug`, `title`, `wiki_type`.
///   Optionally `project_id` (stamped into `_meta.md.extra`).
/// - `Upsert` requires `wiki_id`.
#[derive(Debug, Clone)]
pub struct PushRequest {
    /// Mode (`create` or `upsert`).
    pub mode: PushMode,
    /// Required on `Upsert`. Forbidden on `Create` (the id is derived).
    pub wiki_id: Option<WikiId>,
    /// Required on `Create`. Parent wiki id; the new wiki lands as a
    /// child of it.
    pub parent_wiki_id: Option<WikiId>,
    /// Required on `Create`. Directory slug (validated).
    pub slug: Option<String>,
    /// Required on `Create`. Human display title.
    pub title: Option<String>,
    /// Required on create. A free-form tone/label (no registry); does
    /// NOT determine smart-ness — pass `smart: true` for that.
    pub wiki_type: Option<String>,
    /// Set `true` on create to forge a **smart wiki** (markerless,
    /// content-indexed, owner-administered via `wiki_admin_*`). Stamped
    /// into `WikiMeta.smart`; ignored on upsert. Default `false`.
    pub smart: bool,
    /// Optional. Stable opaque id of the project the consumer is
    /// tracking. Stamped into `_meta.md.extra.project_id` for future
    /// `wiki_search` filtering (the filter lands later, when a real
    /// multi-project consumer needs it).
    pub project_id: Option<String>,
    /// Pages to write. On `Create`, any pages here land alongside the
    /// auto-generated `_meta.md`. On `Upsert`, each page is created
    /// or overwritten.
    pub pages: Vec<PushPage>,
    /// Paths to delete. Honoured on `Upsert`. Each path must already
    /// exist (silent-skip would mask a bug in the consumer's diff
    /// logic). `_meta.md` is never deletable through this surface.
    pub deletes: Vec<String>,
    /// Opaque briefing-item ids (`bi_<N>` or bare `<N>`) the
    /// caller wants to mark as `processed_at = NOW()` atomically with
    /// this push. Empty / absent means "do not touch
    /// `wiki_briefing_items`", preserving the prior behaviour.
    ///
    /// Each id is validated server-side against the push's target
    /// `wiki_id`: a missing row or a row that belongs to a different
    /// wiki yields [`AdminError::UnknownBriefingItemId`] and the whole
    /// push is rolled back (transaction-scoped). The list size is
    /// capped at [`MARK_PROCESSED_CAP_PER_PUSH`] entries — over the
    /// cap → [`AdminError::TooManyBriefingItems`].
    pub mark_processed: Vec<String>,
    /// Optimistic-concurrency guard, honoured on `Upsert` only. When
    /// set, the push is rejected with [`AdminError::ConflictingOpLogHead`]
    /// if any *write* op (`push_create` / `push_upsert`, including a
    /// system revert compensation) newer than this `op_id` landed on the
    /// wiki since the caller last synced — i.e. a concurrent device wrote
    /// in the meantime. `None` (the default) skips the check and keeps the
    /// prior last-writer-wins behaviour. Read ops (`pull` / `notify`) never
    /// trip the gate, so the value the caller stamps from a push response's
    /// `op_log_id` (or a pull's `op_log_head`) stays stable across its own
    /// pulls. Ignored on `Create` (the derived id cannot pre-exist).
    pub expected_op_log_head: Option<i64>,
}

/// Summary counts of what the push actually applied.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushOpsApplied {
    /// Pages that did not exist before the push.
    pub created: usize,
    /// Pages that existed and were overwritten.
    pub updated: usize,
    /// Pages that were deleted.
    pub deleted: usize,
}

/// Return value of [`push`]. Maps 1-to-1 to the wire shape of
/// `wiki_admin_push` documented in `tool-reference.md §H.1`.
#[derive(Debug, Clone)]
pub struct PushResponse {
    /// The wiki the push landed in. Useful on `Create` where the
    /// caller did not know the derived id.
    pub wiki_id: WikiId,
    /// Per-operation counts.
    pub ops_applied: PushOpsApplied,
    /// Auto-increment id of the row inserted into
    /// `wiki_admin_op_log`. Future consumers reference it via
    /// `expected_op_log_head` for optimistic concurrency.
    pub op_log_id: i64,
    /// Plain-language warnings about what was just written, for the
    /// consumer to relay. Today: one line per pushed page whose blocks
    /// are too dense to section cleanly ([`shape_warnings`]), capped at
    /// [`MAX_SHAPE_WARNINGS_PER_PUSH`]. Empty on a standard wiki and on
    /// a healthy push.
    pub warnings: Vec<String>,
    /// Canonical `bi_<N>` ids the push successfully marked
    /// `processed_at = NOW()`. Sorted ascending, no duplicates, in the
    /// same order returned by the validating `SELECT` (so the smart
    /// consumer can echo the list back to the user without re-sorting).
    /// Empty when the caller did not request any marks.
    pub marked_processed: Vec<String>,
    /// Provenance breadcrumbs for the pages this push authored, as plain
    /// `[[wiki_id/page]]` wikilinks (one per written page, `_meta.md` and
    /// deletes excluded). A smart consumer carries these into the next
    /// `wiki_ingest_message` (`metadata.authored_refs`) so the standard
    /// personal-memory pipeline records a **reference** to the project
    /// page instead of re-storing its body — the "link, don't duplicate"
    /// provenance tube (roadmap group 17). The form matches what
    /// [`crate::capture::wiki_link`] emits and recall-as-navigation
    /// follows.
    pub authored_refs: Vec<String>,
}

/// Format the `[[wiki_id/page]]` provenance breadcrumbs for the pages a
/// push wrote — the upstream half of the group-17 provenance tube.
///
/// One link per authored page (`_meta.md` and deletes are not authorship),
/// with the trailing `.md` stripped and `\` normalised to `/` so the link
/// reads naturally in Obsidian and parses with
/// [`crate::recall::extract_wikilink_wiki_ids`].
fn authored_refs_for(wiki_id: &WikiId, pages: &[PushPage]) -> Vec<String> {
    pages
        .iter()
        .map(|p| {
            let path = p.path.replace('\\', "/");
            let trimmed = path.strip_suffix(".md").unwrap_or(&path);
            format!("[[{wiki_id}/{trimmed}]]")
        })
        .collect()
}

/// One page in a [`PullResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullPage {
    /// Relative path inside the wiki directory.
    pub path: String,
    /// Full markdown body. `None` in shape mode — the whole point of
    /// that mode is to answer "how will these pages retrieve?" without
    /// moving the wiki through the caller's context.
    pub content: Option<String>,
    /// What the section index will make of this page. `Some` only in
    /// shape mode ([`PullRequest::shape`]).
    pub shape: Option<crate::document::PageShape>,
}

/// Input to [`pull`].
#[derive(Debug, Clone)]
pub struct PullRequest {
    /// Wiki to read.
    pub wiki_id: WikiId,
    /// Narrow the pull to these wiki-relative page paths (forward
    /// slashes). Empty = the whole wiki. A path that does not exist is
    /// simply absent from the response — a pull is a read, and asking
    /// for a page that was deleted elsewhere is the normal way to find
    /// out that it was.
    pub paths: Vec<String>,
    /// Report each page's [`crate::document::PageShape`] instead of its
    /// bytes. Re-derived from disk, so it answers correctly even though
    /// section indexing is queued rather than synchronous.
    pub shape: bool,
}

/// Return value of [`pull`].
#[derive(Debug, Clone)]
pub struct PullResponse {
    /// Every `.md` page in the wiki directory excluding `_meta.md`
    /// and sub-wikis (matches `WikiHandle::list_pages` semantics),
    /// narrowed by [`PullRequest::paths`] when set.
    pub pages: Vec<PullPage>,
    /// `op_id` of the most recent `wiki_admin_op_log` row for this
    /// wiki, or `None` if no admin op was ever recorded. The smart
    /// consumer stamps it into `.mwe/state.json` and forwards it as
    /// `expected_op_log_head` on the next push.
    pub op_log_head: Option<i64>,
}

/// Authenticated caller for [`push`] / [`pull`]. Constructed by the
/// MCP layer from `IdentityProfile`; explicit fields make the unit
/// tests independent from the server crate.
#[derive(Debug, Clone)]
pub struct AdminCaller {
    /// `token.sender_id` (the user id, bare slug — `"alice"`, not
    /// `"user:alice"`).
    pub sender_id: String,
    /// `token.consumer_id` (the device id — `"cc-laptop"`,
    /// `"cowork-aws"`). Stamped into the `op_log` row for audit.
    pub consumer_id: Option<String>,
    /// `token.consumer_class`.
    pub consumer_class: ConsumerClass,
}

/// Run a `wiki_admin_push` against the workdir. See module docstring
/// for invariants and deferred features.
///
/// `actor_kind` discriminates the writer (smart consumer over MCP,
/// dashboard textual editor, or system compensation row from the
/// revert handler) and drives the gate matrix — see [`ActorKind`] for
/// the per-variant rules. The `actor_kind` value is also stamped into
/// the resulting `wiki_admin_op_log` row.
///
/// # Errors
///
/// One of [`AdminError`] mapped to the wire codes in `tool-reference.md §H`.
pub async fn push(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &AdminCaller,
    actor_kind: ActorKind,
    req: PushRequest,
) -> Result<PushResponse, AdminError> {
    // The `consumer_class=smart` gate only fires on the smart-consumer
    // path. Dashboard writes carry an admin session, not an MCP token,
    // so the smartness check does not apply.
    if actor_kind == ActorKind::SmartConsumer && !caller.consumer_class.is_smart() {
        return Err(AdminError::RequiresSmart);
    }
    // Fail-fast on the size cap before doing any other work.
    // Per-id parsing happens inside each branch once the effective
    // `wiki_id` is known (create derives it from parent + slug; upsert
    // takes it from the request) so the diagnostic carries the right
    // wiki id even for ids that turn out to belong to a sibling wiki.
    if req.mark_processed.len() > MARK_PROCESSED_CAP_PER_PUSH {
        return Err(AdminError::TooManyBriefingItems {
            received: req.mark_processed.len(),
            cap: MARK_PROCESSED_CAP_PER_PUSH,
        });
    }
    // Optional cooperative lease. Only `Upsert` is gated:
    // `Create` derives a brand-new wiki id that cannot be the target
    // of an existing lease, so there is nothing to coordinate.
    if req.mode == PushMode::Upsert
        && let Some(target_id) = req.wiki_id.as_ref()
    {
        let active =
            crate::wiki_admin_leases::active_lease_for(pool, target_id, chrono::Utc::now()).await?;
        if let Some(lease) = active
            && !lease.held_by(caller)
        {
            return Err(AdminError::WikiLockedByLease {
                wiki_id: target_id.clone(),
                held_by_consumer_id: lease.consumer_id,
                held_by_sender_id: lease.sender_id,
                expires_at: lease.expires_at,
            });
        }
    }
    match req.mode {
        PushMode::Create => push_create(pool, tree, caller, actor_kind, req).await,
        PushMode::Upsert => push_upsert(pool, tree, caller, actor_kind, req).await,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "create flow reads top-to-bottom; splitting hides the linear validation→write→op_log pipeline"
)]
async fn push_create(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &AdminCaller,
    actor_kind: ActorKind,
    req: PushRequest,
) -> Result<PushResponse, AdminError> {
    let slug_str = req.slug.as_deref().ok_or_else(|| {
        AdminError::InvalidInput(
            "create makes a NEW WIKI and requires `slug` (its id under the parent); \
                 to add or edit a PAGE in an existing wiki use mode=upsert with that wiki's wiki_id"
                .into(),
        )
    })?;
    let title = req
        .title
        .as_deref()
        .ok_or_else(|| {
            AdminError::InvalidInput(
                "create (new wiki) requires `title` — the wiki's display name, e.g. \"LNPrint — \
                 engineering wiki\""
                    .into(),
            )
        })?
        .trim();
    if title.is_empty() {
        return Err(AdminError::InvalidInput("title must not be blank".into()));
    }
    let wiki_type = req.wiki_type.as_deref().ok_or_else(|| {
        AdminError::InvalidInput(
            "create (new wiki) requires `wiki_type` — a free-form label (\"project\" is the usual \
             one); smart-ness comes from the separate `smart: true` flag, not from this string"
                .into(),
        )
    })?;
    if req.wiki_id.is_some() {
        return Err(AdminError::InvalidInput(
            "create must not pass wiki_id (it is derived from parent + slug)".into(),
        ));
    }

    // Smart-family gate. The `wiki_type` axis was already dissolved (the
    // registry describe + the magic-string family sniff are both gone):
    // smart-ness is now an **explicit request flag** the smart consumer
    // sends — it decides to create (importing a previously-local wiki, or
    // a new project wiki on user request). `wiki_type` survives only as a
    // free-form tone/label (feeding `compiler::resolve_tone`) and no
    // longer steers any gate. The flag is stamped into `WikiMeta.smart`
    // below. The gate only fires on the smart-consumer path — dashboard
    // creates are a power-user shortcut and the dashboard UI itself
    // steers toward sensible templates.
    let is_smart_family = req.smart;
    if actor_kind == ActorKind::SmartConsumer && !is_smart_family {
        return Err(AdminError::WikiTypeNotAdminWritable {
            wiki_type: wiki_type.to_owned(),
        });
    }

    // Child-only gate, now per-kind: only the smart family
    // inherits a parent's ACL scope and must be created beneath one — the
    // `requires_parent` template flag is gone with the registry describe.
    // Surfaced ahead of the generic "create requires parent_wiki_id" so the
    // caller sees the wire-stable `WikiTypeRequiresParent`.
    if is_smart_family && req.parent_wiki_id.is_none() {
        return Err(AdminError::WikiTypeRequiresParent {
            wiki_type: wiki_type.to_owned(),
            expected_parent: caller.sender_id.clone(),
        });
    }
    let parent_id = req.parent_wiki_id.clone().ok_or_else(|| {
        AdminError::InvalidInput(format!(
            "create (new wiki) requires parent_wiki_id — your own root wiki, `{}`",
            caller.sender_id
        ))
    })?;

    // Locate the parent on disk (or refuse if it's missing — we
    // need its abs_dir to land the child).
    let parent_handle = tree
        .locate(&parent_id)
        .map_err(|_| AdminError::NotFound(parent_id.clone()))?;

    let slug = WikiSlug::parse(slug_str)?;
    let new_wiki_id = WikiId::child_of(&parent_id, &slug);

    // Refuse if the new wiki already exists — `create` is
    // strictly additive.
    if tree.locate(&new_wiki_id).is_ok() {
        return Err(AdminError::InvalidInput(format!(
            "wiki {new_wiki_id} already exists; use upsert"
        )));
    }

    // The smart-wiki is owned by the caller. The owning principal is no
    // longer stamped into `_meta.md`: it is **derived** from topology —
    // this wiki is created under `parent_id`, whose chain roots at the
    // caller's own `wiki-user` identity wiki, so `resolve_scope_principal`
    // (and `resolve_owner_user`) yield `user:<caller>` unchanged.
    let now_iso = chrono::Utc::now().to_rfc3339();
    let mut extra = serde_yaml::Mapping::new();
    if let Some(pid) = req.project_id.as_deref() {
        extra.insert(
            serde_yaml::Value::String("project_id".to_owned()),
            serde_yaml::Value::String(pid.to_owned()),
        );
    }
    let meta = WikiMeta {
        wiki_id: new_wiki_id.clone(),
        wiki_type: wiki_type.to_owned(),
        parent_wiki_id: Some(parent_id.clone()),
        slug,
        title: title.to_owned(),
        scope: None,
        shared_with: Vec::new(),
        style_overrides: serde_yaml::Mapping::new(),
        keywords: serde_yaml::Mapping::new(),
        children: Vec::new(),
        promoted_from: None,
        no_archive: false,
        // Stamp the per-wiki smart flag into `_meta.md` from the
        // explicit `smart` request flag. This is the authoritative marker
        // the smart/standard family gates read now they no longer query
        // `wiki_types_registry` nor sniff the `wiki_type` id. A dashboard
        // power-user create lands `false` (the default); a smart-consumer
        // smart-wiki create passes `smart: true`.
        smart: is_smart_family,
        // A smart-consumer wiki create is never an agent's own identity wiki.
        is_agent: false,
        created: Some(now_iso.clone()),
        updated: Some(now_iso),
        extra,
    };

    if !req.deletes.is_empty() {
        return Err(AdminError::InvalidInput(
            "create mode does not accept `deletes` (the wiki is new)".into(),
        ));
    }

    // Vet every page path BEFORE the wiki directory is forged: a bad
    // path surfacing later (inside `write_pages`) would leave a
    // half-made wiki on disk that the transaction rollback cannot
    // remove.
    validate_push_pages(&req.pages)?;

    // Parse + dedupe `mark_processed` ids *before* any write.
    // On `Create` the new wiki was empty an instant ago, so a non-empty
    // list always points at briefing items that don't belong here —
    // the per-id SELECT inside the transaction will catch it. We still
    // shape-check up front to surface malformed strings before the
    // filesystem is touched.
    let mark_ids = parse_mark_processed(&new_wiki_id, &req.mark_processed)?;

    let dir = parent_handle.abs_dir().join(slug_str);
    let meta_path = dir.join(META_FILENAME);
    let meta_doc = meta.render("").map_err(|e| {
        AdminError::Wiki(WikiError::InvalidFrontmatter {
            path: meta_path.clone(),
            detail: format!("rendering canonical meta: {e}"),
        })
    })?;

    // Open the transaction *before* any file write so a failed
    // briefing-item validation rolls back without leaving the
    // wiki directory on disk. The transaction wraps validation + page
    // writes + processed_at flips + op_log insert: dropping it without
    // commit (the error paths below) restores both the DB and the
    // filesystem (file writes happen inside, but they are not
    // transactional — see write_pages docstring).
    let mut tx = pool.begin().await?;
    let now_for_marks = chrono::Utc::now().to_rfc3339();
    let marked_processed =
        validate_and_mark_processed(&mut tx, &new_wiki_id, &mark_ids, &now_for_marks).await?;

    atomic_write(&meta_path, meta_doc.as_bytes())?;
    let mut ops = PushOpsApplied::default();
    write_pages(&dir, &req.pages, &mut ops, /* allow_overwrite */ false)?;

    let op_log_id = record_op_log(
        &mut *tx,
        OpLogInsert {
            wiki_id: &new_wiki_id,
            sender_id: &caller.sender_id,
            consumer_id: caller.consumer_id.as_deref(),
            actor_kind,
            op_kind: PushMode::Create.op_log_kind(),
            op_mode: Some(PushMode::Create.wire()),
            payload_hash: &payload_hash(&req),
            pages_affected: ops.created + ops.updated + ops.deleted,
            // Create has no pre-image: the wiki did not exist before
            // this row. NULL is the canonical "no revert possible"
            // marker — the dashboard surfaces it as a disabled
            // Revert button.
            pre_image_json: None,
        },
    )
    .await?;
    tx.commit().await?;

    let authored_refs = authored_refs_for(&new_wiki_id, &req.pages);
    let warnings = shape_warnings(&req.pages, is_smart_family);
    Ok(PushResponse {
        wiki_id: new_wiki_id,
        ops_applied: ops,
        op_log_id,
        warnings,
        marked_processed,
        authored_refs,
    })
}

async fn push_upsert(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &AdminCaller,
    actor_kind: ActorKind,
    req: PushRequest,
) -> Result<PushResponse, AdminError> {
    let wiki_id = req
        .wiki_id
        .clone()
        .ok_or_else(|| AdminError::InvalidInput("upsert requires wiki_id".into()))?;
    if req.parent_wiki_id.is_some()
        || req.slug.is_some()
        || req.title.is_some()
        || req.wiki_type.is_some()
    {
        return Err(AdminError::InvalidInput(
            "upsert must not pass create-only fields (parent_wiki_id, slug, title, wiki_type)"
                .into(),
        ));
    }

    let handle = tree
        .locate(&wiki_id)
        .map_err(|_| AdminError::NotFound(wiki_id.clone()))?;

    enforce_admin_auth(pool, tree, &handle, caller, actor_kind).await?;

    // Optimistic-concurrency gate. When the caller stamps
    // `expected_op_log_head` (the write-op id it last synced to), reject
    // before touching disk if a *newer write op* landed in the gap —
    // another device of the same user pushed, or a dashboard revert
    // rewrote pages. Read ops (`pull` / `notify`) are excluded via the
    // `push_%` prefix (mirror of [`is_write_op_kind`]) so the caller's
    // own pulls never look like a conflict. `None` keeps the legacy
    // last-writer-wins path.
    if let Some(expected) = req.expected_op_log_head {
        let latest_write_head: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(op_id) FROM wiki_admin_op_log WHERE wiki_id = ? AND op_kind LIKE 'push\\_%' ESCAPE '\\'",
        )
        .bind(wiki_id.as_str())
        .fetch_one(pool)
        .await?;
        if let Some(actual) = latest_write_head
            && actual > expected
        {
            return Err(AdminError::ConflictingOpLogHead {
                wiki_id: wiki_id.clone(),
                expected,
                actual,
            });
        }
    }

    // Vet every page path before the pre-image snapshot joins them to
    // the wiki directory (and before any write).
    validate_push_pages(&req.pages)?;

    // Parse + dedupe `mark_processed` ids up front so a
    // malformed string fails before the on-disk wiki is touched.
    let mark_ids = parse_mark_processed(&wiki_id, &req.mark_processed)?;

    // Snapshot pre-image bodies *before* anything is written so the
    // revert handler can roll back to the exact state the
    // upsert overwrote. Captures every page that is about to be
    // upserted or deleted; `content: None` distinguishes "page did
    // not exist" from "page existed and was empty" per the migration
    // 0027 column comment.
    let dir = handle.abs_dir().to_path_buf();
    let pre_image_json = snapshot_pre_image(&dir, &req)?;

    // Open the transaction *before* the file writes so a failed
    // briefing-item validation rolls back the op_log row + the
    // `processed_at` flips without leaving a half-applied marker. The
    // page writes happen inside the transaction window so a validation
    // failure in `validate_and_mark_processed` reaches the early-return
    // before any disk mutation.
    let mut tx = pool.begin().await?;
    let now_for_marks = chrono::Utc::now().to_rfc3339();
    let marked_processed =
        validate_and_mark_processed(&mut tx, &wiki_id, &mark_ids, &now_for_marks).await?;

    // Apply page upserts (overwrites allowed; deletes happen after).
    let mut ops = PushOpsApplied::default();
    write_pages(&dir, &req.pages, &mut ops, /* allow_overwrite */ true)?;

    // Apply deletes — refuse `_meta.md`, refuse escapes, error if
    // the page doesn't exist (caller's diff is wrong if so).
    for rel_path in &req.deletes {
        let pb = PathBuf::from(rel_path);
        if !crate::wiki::is_safe_page_path(&pb) {
            return Err(AdminError::InvalidInput(format!(
                "unsafe delete path: {rel_path}"
            )));
        }
        if rel_path == META_FILENAME {
            return Err(AdminError::InvalidInput(format!(
                "{META_FILENAME} cannot be deleted via wiki_admin_push"
            )));
        }
        let abs = dir.join(&pb);
        if !abs.exists() {
            return Err(AdminError::InvalidInput(format!(
                "delete target {rel_path} does not exist"
            )));
        }
        std::fs::remove_file(&abs)?;
        ops.deleted += 1;
    }

    let op_log_id = record_op_log(
        &mut *tx,
        OpLogInsert {
            wiki_id: &wiki_id,
            sender_id: &caller.sender_id,
            consumer_id: caller.consumer_id.as_deref(),
            actor_kind,
            op_kind: PushMode::Upsert.op_log_kind(),
            op_mode: Some(PushMode::Upsert.wire()),
            payload_hash: &payload_hash(&req),
            pages_affected: ops.created + ops.updated + ops.deleted,
            pre_image_json: Some(pre_image_json.as_str()),
        },
    )
    .await?;
    tx.commit().await?;

    let authored_refs = authored_refs_for(&wiki_id, &req.pages);
    let warnings = shape_warnings(&req.pages, handle.meta().smart);
    Ok(PushResponse {
        wiki_id,
        ops_applied: ops,
        op_log_id,
        warnings,
        marked_processed,
        authored_refs,
    })
}

/// Cap on density warnings a single push may carry. A bulk import of a
/// badly-shaped wiki would otherwise answer with one line per page; the
/// whole-wiki picture is what [`pull`]'s shape mode is for.
const MAX_SHAPE_WARNINGS_PER_PUSH: usize = 5;

/// Roadmap 51f — the write half of "page shape is measured and reported,
/// never asked".
///
/// A page whose blocks are too long to index as one retrieves badly, and
/// nobody finds out: the push succeeds, the sections are cut mid-sentence
/// in the background, and the damage surfaces months later as an answer
/// that misses. The moment a page is written is the moment its author is
/// still here — the same reasoning that puts `signpost_hint` on this
/// response.
///
/// Smart wikis only: a standard wiki's content is fact-indexed, not
/// sectioned, so markdown shape has no bearing on how it is retrieved.
/// Computed from the bytes in hand — no DB, no queue, no embedder.
fn shape_warnings(pages: &[PushPage], smart: bool) -> Vec<String> {
    if !smart {
        return Vec::new();
    }
    let policy = crate::document::DocumentPolicy::for_sections();
    let mut warnings: Vec<String> = Vec::new();
    let mut suppressed = 0usize;
    for page in pages {
        let Some(line) = crate::document::page_shape(&page.content, &policy).warning(&page.path)
        else {
            continue;
        };
        if warnings.len() < MAX_SHAPE_WARNINGS_PER_PUSH {
            warnings.push(line);
        } else {
            suppressed += 1;
        }
    }
    if suppressed > 0 {
        warnings.push(format!(
            "…and {suppressed} more page(s) in this push have the same problem. Pull the wiki with \
             `shape: true` for the full picture."
        ));
    }
    warnings
}

/// Run a `wiki_admin_pull`. See module docstring for what is and
/// isn't enforced.
///
/// Three shapes, one call: the whole wiki (the default), a named subset
/// ([`PullRequest::paths`] — the narrowing the smart-consumer skill has
/// always documented), and the per-page section shape instead of the
/// bytes ([`PullRequest::shape`], roadmap 51f).
///
/// # Errors
///
/// One of [`AdminError`] mapped to the wire codes in `tool-reference.md §H.2`.
pub async fn pull(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &AdminCaller,
    req: &PullRequest,
) -> Result<PullResponse, AdminError> {
    if !caller.consumer_class.is_smart() {
        return Err(AdminError::RequiresSmart);
    }
    let wiki_id = &req.wiki_id;
    let handle = tree
        .locate(wiki_id)
        .map_err(|_| AdminError::NotFound(wiki_id.clone()))?;
    enforce_admin_auth(pool, tree, &handle, caller, ActorKind::SmartConsumer).await?;

    let wanted: Option<std::collections::HashSet<&str>> = if req.paths.is_empty() {
        None
    } else {
        Some(req.paths.iter().map(String::as_str).collect())
    };
    let policy = crate::document::DocumentPolicy::for_sections();
    let mut pages: Vec<PullPage> = Vec::new();
    for info in handle.list_pages()? {
        let rel = info
            .rel_path
            .to_str()
            .ok_or_else(|| AdminError::InvalidInput("non-utf8 page path".into()))?
            .replace('\\', "/");
        if wanted.as_ref().is_some_and(|w| !w.contains(rel.as_str())) {
            continue;
        }
        let body = std::fs::read_to_string(&info.abs_path)?;
        if req.shape {
            pages.push(PullPage {
                path: rel,
                shape: Some(crate::document::page_shape(&body, &policy)),
                content: None,
            });
        } else {
            pages.push(PullPage {
                path: rel,
                content: Some(body),
                shape: None,
            });
        }
    }

    let op_log_head: Option<i64> =
        sqlx::query_scalar("SELECT MAX(op_id) FROM wiki_admin_op_log WHERE wiki_id = ?")
            .bind(wiki_id.as_str())
            .fetch_one(pool)
            .await?;

    record_op_log(
        pool,
        OpLogInsert {
            wiki_id,
            sender_id: &caller.sender_id,
            consumer_id: caller.consumer_id.as_deref(),
            actor_kind: ActorKind::SmartConsumer,
            op_kind: "pull",
            op_mode: None,
            payload_hash: &empty_payload_hash(),
            pages_affected: pages.len(),
            // Pull is a read — no pre-image to capture, and the
            // revert handler is not interested in pull rows anyway.
            pre_image_json: None,
        },
    )
    .await?;

    Ok(PullResponse { pages, op_log_head })
}

// ---------- revert handler ----------

/// Errors raised by [`op_revert`].
///
/// Mapped onto the dashboard wire codes documented in
/// [`tool-reference.md revert extension`] and
/// [`protocollo.md §10.2`]:
///
/// | Variant            | Status | Wire code                       |
/// |--------------------|--------|---------------------------------|
/// | `NotFound`         | 404    | `op_not_found`                  |
/// | `NoPreImage`       | 400    | `op_not_revertable`             |
/// | `NotRevertable`    | 400    | `op_not_revertable`             |
/// | `TargetChanged`    | 409    | `op_log_target_changed_since`   |
/// | `Db` / `Io`        | 500    | `internal_error`                |
///
/// The strict conflict policy is the
/// load-bearing decision: when a newer row touched any of the same
/// pages, the handler refuses with [`RevertError::TargetChanged`] and
/// the dashboard banner steers the operator through the manual
/// fall-back. No force, no "revert as new push annotated".
///
/// [`tool-reference.md revert extension`]: ../../../docs/protocol/tool-reference.md
/// [`protocollo.md §10.2`]: ../../../docs/protocol/tool-reference.md
#[derive(Debug, Error)]
pub enum RevertError {
    /// `op_id` does not match any row in `wiki_admin_op_log`.
    #[error("op {0} not found in wiki_admin_op_log")]
    NotFound(i64),
    /// The target row has `pre_image_json IS NULL` — either a legacy
    /// row (no pre-image was captured at the time), a `pull` / `notify`
    /// row (reads + side-channel notifications never carry pre-images),
    /// or a previous `system` compensation row (which we explicitly
    /// refuse to revert: chained revert-of-revert is performed by
    /// clicking the original target again, not by reverting the
    /// compensation).
    #[error("op {op_id} cannot be reverted: {reason}")]
    NoPreImage {
        /// Target `op_id`.
        op_id: i64,
        /// Human-readable reason for the disabled state.
        reason: String,
    },
    /// The row is not a write (`pull`, `notify`, …) — nothing to
    /// roll back.
    #[error("op {op_id} is not revertable: {reason}")]
    NotRevertable {
        /// Target `op_id`.
        op_id: i64,
        /// Human-readable reason ("non-write op", "_meta.md write
        /// snuck in", …).
        reason: String,
    },
    /// A newer write touched (at least) one of the same pages — strict
    /// conflict policy refuses to revert. The dashboard renders the
    /// payload as a guided banner ("page X was modified by op Y on
    /// date Z") with direct links to each conflicting op.
    #[error(
        "op cannot be reverted: pages {conflicting_pages:?} were touched by later ops {conflicting_ops:?}"
    )]
    TargetChanged {
        /// `op_id`s of every newer row whose `pages_affected` intersect
        /// the target's pages. Ordered by `op_id` ASC for stable UX.
        conflicting_ops: Vec<i64>,
        /// Page paths that the conflicting ops touched.
        conflicting_pages: Vec<String>,
    },
    /// `pre_image_json` failed to deserialise — the JSON is well-formed
    /// but does not match the
    /// `{pages: [{path, content: string|null}]}` shape. Surfaced as
    /// `op_not_revertable` on the wire because the row is structurally
    /// unsound.
    #[error("op {op_id} has malformed pre_image_json: {detail}")]
    MalformedPreImage {
        /// Target `op_id`.
        op_id: i64,
        /// Parser detail.
        detail: String,
    },
    /// Database failure during the conflict check / compensating
    /// insert.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    /// Filesystem failure during page restore / delete.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Catch-all for [`crate::wiki`] / serialisation glue surfacing
    /// during the inverse pass.
    #[error("internal: {0}")]
    Internal(String),
}

/// Outcome of a successful [`op_revert`].
#[derive(Debug, Clone)]
pub struct RevertOutcome {
    /// `op_id` of the compensating row written into
    /// `wiki_admin_op_log` (always carries `actor_kind = 'system'`).
    pub compensating_op_id: i64,
    /// Relative paths of every page the revert touched (restored or
    /// deleted) — surfaced verbatim in the dashboard success flash.
    pub restored_pages: Vec<String>,
    /// ISO-8601 timestamp stamped on the compensating row.
    pub ts: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OpLogTargetRow {
    wiki_id: String,
    op_kind: String,
    actor_kind: String,
    pre_image_json: Option<String>,
    ts: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OpLogNeighbourRow {
    op_id: i64,
    pre_image_json: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct PreImageDoc {
    pages: Vec<PreImagePage>,
}

#[derive(Debug, serde::Deserialize)]
struct PreImagePage {
    path: String,
    content: Option<String>,
}

/// Revert a single `wiki_admin_op_log` row by restoring its
/// `pre_image_json` to disk and inserting a `system`-tagged
/// compensating row.
///
/// See [`RevertError`] for the wire mapping and the strict conflict
/// policy. The compensating row carries its own `pre_image_json` =
/// the *post-state* of the target row (i.e. what was on disk just
/// before the revert wrote), so a future revert of the compensation
/// can restore the post-state. The chain stays shallow: the dashboard
/// surface hides the revert button on `actor_kind = 'system'` rows
/// (the inverse of an inverse is a no-op), and operators are steered
/// to click the original target again if they change their mind.
///
/// # Errors
///
/// One of [`RevertError`].
#[allow(
    clippy::too_many_lines,
    reason = "the revert flow reads top-to-bottom as a single transaction: load → policy check → conflict scan → snapshot post-state → restore pages → insert compensating row; splitting it would scatter the strict-conflict policy across helpers without clarifying the contract"
)]
pub async fn op_revert(
    pool: &SqlitePool,
    tree: &WikiTree,
    op_id: i64,
    reverted_by: &str,
) -> Result<RevertOutcome, RevertError> {
    // 1. Load the target row.
    let target: OpLogTargetRow = sqlx::query_as(
        "SELECT wiki_id, op_kind, actor_kind, pre_image_json, ts
           FROM wiki_admin_op_log WHERE op_id = ?",
    )
    .bind(op_id)
    .fetch_optional(pool)
    .await?
    .ok_or(RevertError::NotFound(op_id))?;

    // 2. Refuse non-write rows. Pulls + notifies do not touch pages,
    //    so there is nothing to roll back. Likewise, a `system` row is
    //    a compensation: chained revert-of-revert is performed by
    //    re-clicking the original, not by reverting the compensation.
    if !is_write_op_kind(&target.op_kind) {
        return Err(RevertError::NotRevertable {
            op_id,
            reason: format!("non-write op ({})", target.op_kind),
        });
    }
    if target.actor_kind == ActorKind::System.wire() {
        return Err(RevertError::NotRevertable {
            op_id,
            reason: "this is a system compensation row (revert-of-revert is performed by clicking the original op again)".into(),
        });
    }

    // 3. Require a pre-image — legacy rows and `Create` rows have
    //    `pre_image_json IS NULL` and cannot be rolled back.
    let pre_image_raw = target
        .pre_image_json
        .clone()
        .ok_or(RevertError::NoPreImage {
            op_id,
            reason: "missing pre-image (legacy row, create row, or non-upsert write)".into(),
        })?;
    let pre_image: PreImageDoc =
        serde_json::from_str(&pre_image_raw).map_err(|e| RevertError::MalformedPreImage {
            op_id,
            detail: e.to_string(),
        })?;
    let target_pages: Vec<String> = pre_image.pages.iter().map(|p| p.path.clone()).collect();

    // Defensive: refuse `_meta.md` even though `push` already blocks
    // it. A malformed pre-image with `_meta.md` would corrupt the
    // wiki's metadata on restore.
    if target_pages.iter().any(|p| is_meta_path(p)) {
        return Err(RevertError::NotRevertable {
            op_id,
            reason: format!("pre-image references {META_FILENAME} which is never revertable"),
        });
    }

    // 4. Conflict check: any newer write row whose touched pages
    //    intersect the target's is a hard refusal.
    let target_page_set: HashSet<String> = target_pages.iter().cloned().collect();
    let neighbours: Vec<OpLogNeighbourRow> = sqlx::query_as(
        "SELECT op_id, pre_image_json
           FROM wiki_admin_op_log
          WHERE wiki_id = ?
            AND ts > ?
            AND op_kind LIKE 'push_%'
          ORDER BY op_id ASC",
    )
    .bind(&target.wiki_id)
    .bind(&target.ts)
    .fetch_all(pool)
    .await?;

    let mut conflicting_ops: Vec<i64> = Vec::new();
    let mut conflicting_pages_set: HashSet<String> = HashSet::new();
    for row in &neighbours {
        let intersect = pages_intersect_target(row, &target_page_set);
        if !intersect.is_empty() {
            conflicting_ops.push(row.op_id);
            conflicting_pages_set.extend(intersect);
        }
    }
    if !conflicting_ops.is_empty() {
        let mut conflicting_pages: Vec<String> = conflicting_pages_set.into_iter().collect();
        conflicting_pages.sort();
        return Err(RevertError::TargetChanged {
            conflicting_ops,
            conflicting_pages,
        });
    }

    // 5. Locate the wiki and snapshot the *post-state* of every page
    //    referenced by the target (what is on disk right now, just
    //    before we overwrite it). This goes into the compensating
    //    row's `pre_image_json` so a future revert of the compensation
    //    can restore the post-state.
    let wiki_id = WikiId::parse(&target.wiki_id)
        .map_err(|e| RevertError::Internal(format!("bad wiki_id in op log row: {e}")))?;
    let handle = tree
        .locate(&wiki_id)
        .map_err(|e| RevertError::Internal(format!("locate wiki {wiki_id}: {e}")))?;
    let dir = handle.abs_dir().to_path_buf();
    let post_state_json = snapshot_post_state(&dir, &target_pages)?;

    // 6. Restore each page from the target's pre-image. `content: Some`
    //    means "the page existed before the target op — restore the
    //    body"; `content: None` means "the page did not exist — delete
    //    the current file" (idempotent if already gone).
    let mut restored_pages: Vec<String> = Vec::new();
    for page in &pre_image.pages {
        let pb = PathBuf::from(&page.path);
        if !crate::wiki::is_safe_page_path(&pb) {
            return Err(RevertError::NotRevertable {
                op_id,
                reason: format!("unsafe path in pre-image: {}", page.path),
            });
        }
        let abs = dir.join(&pb);
        match &page.content {
            Some(body) => {
                if let Some(parent) = abs.parent()
                    && !parent.exists()
                {
                    std::fs::create_dir_all(parent)?;
                }
                atomic_write(&abs, body.as_bytes())
                    .map_err(|e| RevertError::Internal(format!("restore {}: {e}", page.path)))?;
            },
            None => {
                if abs.exists() {
                    std::fs::remove_file(&abs)?;
                }
            },
        }
        restored_pages.push(page.path.clone());
    }

    // 7. Insert the compensating row. `actor_kind='system'`,
    //    `op_kind='push_upsert'` (everything we wrote was an upsert /
    //    delete of a body that existed at restore time), `consumer_id`
    //    is NULL (the operator at the dashboard is not behind an MCP
    //    device), and `pre_image_json` carries the post-state we just
    //    overwrote.
    let payload_hash = revert_payload_hash(op_id, &target_pages);
    let pages_affected = target_pages.len();
    let now = chrono::Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_admin_op_log
            (wiki_id, sender_id, consumer_id, actor_kind, op_kind, op_mode,
             payload_hash, pages_affected, pre_image_json, ts)
         VALUES (?, ?, NULL, ?, 'push_upsert', 'upsert', ?, ?, ?, ?)
         RETURNING op_id",
    )
    .bind(&target.wiki_id)
    .bind(reverted_by)
    .bind(ActorKind::System.wire())
    .bind(&payload_hash)
    .bind(i64::try_from(pages_affected).unwrap_or(i64::MAX))
    .bind(&post_state_json)
    .bind(&now)
    .fetch_one(pool)
    .await?;

    Ok(RevertOutcome {
        compensating_op_id: row.0,
        restored_pages,
        ts: now,
    })
}

/// `op_kind` values that semantically wrote to the wiki and therefore
/// have something to roll back. Mirror of the spec ([`protocollo.md
/// §10.2`]): `push_create` / `push_upsert` / `push_snapshot_replace`
/// are writes; `pull` / `notify` are not.
///
/// [`protocollo.md §10.2`]: ../../../docs/protocol/tool-reference.md
fn is_write_op_kind(op_kind: &str) -> bool {
    op_kind.starts_with("push_")
}

fn is_meta_path(rel: &str) -> bool {
    let normalised = rel.replace('\\', "/");
    normalised == META_FILENAME || normalised.ends_with(&format!("/{META_FILENAME}"))
}

/// Decide which pages of `target_page_set` a neighbour row in the op
/// log touched. The conservative branch (`pre_image_json IS NULL`)
/// only kicks in for legacy rows: any current `push_*` row
/// carries a `pre_image_json` populated by [`push`]. Returns the
/// intersection so the caller can build a precise
/// `conflicting_pages: Vec<String>` for the wire payload.
fn pages_intersect_target(
    neighbour: &OpLogNeighbourRow,
    target_page_set: &HashSet<String>,
) -> Vec<String> {
    let Some(raw) = neighbour.pre_image_json.as_deref() else {
        // Conservative: a legacy row with no pre-image could have
        // touched any page in this wiki. Treat the whole target page
        // set as in-conflict — the strict policy never restores under
        // ambiguity.
        return target_page_set.iter().cloned().collect();
    };
    let Ok(doc) = serde_json::from_str::<PreImageDoc>(raw) else {
        // Same conservative branch for malformed JSON.
        return target_page_set.iter().cloned().collect();
    };
    doc.pages
        .into_iter()
        .map(|p| p.path)
        .filter(|p| target_page_set.contains(p))
        .collect()
}

fn snapshot_post_state(dir: &Path, paths: &[String]) -> Result<String, RevertError> {
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(paths.len());
    for rel in paths {
        let abs = dir.join(rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(RevertError::Io(e)),
        };
        entries.push(serde_json::json!({ "path": rel, "content": content }));
    }
    let doc = serde_json::json!({ "pages": entries });
    serde_json::to_string(&doc)
        .map_err(|e| RevertError::Internal(format!("serialise post-state pre_image_json: {e}")))
}

/// SHA-256 of the canonical input to the revert: the target `op_id`
/// + sorted paths.
///
/// The compensating row's `payload_hash` is deterministic for a given
/// revert, which makes the row easy to fingerprint in audit replays.
fn revert_payload_hash(op_id: i64, paths: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"revert_of=");
    h.update(op_id.to_string().as_bytes());
    let mut sorted = paths.to_vec();
    sorted.sort();
    for p in &sorted {
        h.update(b"\n");
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

// ---------- Shared helpers ----------

async fn enforce_admin_auth(
    pool: &SqlitePool,
    tree: &WikiTree,
    handle: &WikiHandle,
    caller: &AdminCaller,
    actor_kind: ActorKind,
) -> Result<(), AdminError> {
    match resolve_owner_user(tree, handle) {
        // User-owned wiki: only its single owner may write.
        Ok(owner) => {
            if owner != caller.sender_id {
                return Err(AdminError::WikiOwnedByOtherUser {
                    wiki_id: handle.meta().wiki_id.clone(),
                    owner,
                    caller_owner: caller.sender_id.clone(),
                });
            }
        },
        // Group-owned wiki: a MEMBER of the owning group is owner-equivalent and
        // may write (the group-ownership model — docs/concepts/identity-and-acl.md:
        // members are owner-equivalent on the group's wiki). The public `global`
        // group has no individual owner, so it stays write-refused.
        Err(AdminError::AmbiguousOwner {
            wiki_id,
            acl_default,
        }) => {
            let group = acl_default
                .strip_prefix("group:")
                .filter(|g| *g != "global");
            let allowed = match group {
                Some(g) => crate::enrollment::groups_for(pool, &caller.sender_id)
                    .await?
                    .iter()
                    .any(|m| m == g),
                None => false,
            };
            if !allowed {
                return Err(AdminError::WikiOwnedByOtherUser {
                    wiki_id,
                    owner: acl_default,
                    caller_owner: caller.sender_id.clone(),
                });
            }
        },
        Err(e) => return Err(e),
    }
    // The smart-family gate keeps standard wikis (`wiki-user`,
    // `wiki-tech`, …) write-protected from smart consumers — they must
    // reach those wikis via the regular `wiki_ingest_message`
    // LLM-mediated path. Dashboard writes bypass the gate by design:
    // a human at the editor is the intended escape hatch. Read
    // per-wiki from `_meta.smart` (the `wiki_types_registry`
    // describe is retired; the marker was stamped at create time).
    if actor_kind == ActorKind::SmartConsumer && !handle.meta().smart {
        return Err(AdminError::WikiTypeNotAdminWritable {
            wiki_type: handle.meta().wiki_type.clone(),
        });
    }
    Ok(())
}

/// Resolve a smart-wiki's owner user id, following `inherit`
/// chains. Returns the bare user id (`"alice"`, not `"user:alice"`).
fn resolve_owner_user(tree: &WikiTree, handle: &WikiHandle) -> Result<String, AdminError> {
    let principal = tree
        .resolve_scope_principal(handle.meta())
        .map_err(AdminError::Wiki)?;
    match principal {
        Principal::User(id) => Ok(id),
        // A group acl_default (including the builtin global group) has no
        // single owning user.
        Principal::Group(_) => Err(AdminError::AmbiguousOwner {
            wiki_id: handle.meta().wiki_id.clone(),
            acl_default: principal.to_string(),
        }),
    }
}

// ---------- shared_with read+notify resolution ----------

/// Outcome of [`resolve_read_access`].
///
/// Encodes *why* the caller has (or doesn't have) read access to a
/// smart-wiki — the dashboard audit view renders this
/// verbatim, and the per-call tracing prefers a tagged enum over a
/// raw boolean so the access-via-group path stays distinguishable
/// from owner / direct-user / global.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadAccessOutcome {
    /// `caller.sender_id` matches the resolved `acl_default` owner.
    Owner,
    /// `caller.sender_id` is a member of the group that **owns** this
    /// wiki (`acl_default` resolves to `Principal::Group(_)`). A member of
    /// the owning group is **owner-equivalent** (read / comment / edit /
    /// push) per the group-ownership rules; the owning group id is kept
    /// for the audit log.
    OwnerGroupMember(String),
    /// `caller.sender_id` matches a `Principal::User(_)` entry inside
    /// `_meta.md.shared_with`.
    SharedUser,
    /// `caller.sender_id` is a member of the named group, which appears
    /// as a `Principal::Group(_)` entry inside `_meta.md.shared_with`.
    /// The group id is preserved for the audit log.
    SharedGroup(String),
    /// `_meta.md.shared_with` names the builtin `global` group — anyone
    /// authenticated can read.
    Global,
    /// Caller cannot read. `owner` is the resolved owner user id, kept
    /// for diagnostic messages on the 403 path.
    Denied {
        /// Resolved owner user id (bare form, no `user:` prefix).
        owner: String,
    },
}

impl ReadAccessOutcome {
    /// `true` for every variant except [`ReadAccessOutcome::Denied`].
    #[must_use]
    pub const fn is_granted(&self) -> bool {
        !matches!(self, Self::Denied { .. })
    }

    /// `true` when the caller has **owner-equivalent** authority — the
    /// wiki's single user owner, or a member of the owning group. This is
    /// the write / edit / push gate: sharing (`SharedUser` / `SharedGroup`
    /// / `Global`) grants reads, not writes.
    #[must_use]
    pub const fn is_owner_equivalent(&self) -> bool {
        matches!(self, Self::Owner | Self::OwnerGroupMember(_))
    }
}

/// Decide whether `caller_sender_id` can read this wiki, and *why*.
///
/// Resolution order (first match wins, so the dashboard audit always
/// shows the most-specific grant):
///
/// 1. The derived scope principal is `Principal::User` and matches the
///    caller's bare `sender_id` → [`ReadAccessOutcome::Owner`].
/// 2. The derived scope principal is `Principal::Group(g)` and the caller
///    is a **member** of `g` (via [`crate::enrollment::groups_for`]) → the
///    caller is **owner-equivalent**
///    ([`ReadAccessOutcome::OwnerGroupMember`]). The builtin `global`
///    group as owner is a *public* wiki: everyone reads
///    ([`ReadAccessOutcome::Global`]), nobody owns it.
/// 3. Caller appears as a `Principal::User(_)` entry inside
///    `_meta.md.shared_with` → [`ReadAccessOutcome::SharedUser`].
/// 4. Caller is a member of a `Principal::Group(_)` entry inside
///    `shared_with` → [`ReadAccessOutcome::SharedGroup`].
/// 5. `shared_with` names the builtin `global` group → every
///    authenticated token reads ([`ReadAccessOutcome::Global`]).
/// 6. Else [`ReadAccessOutcome::Denied`] with the resolved owner
///    principal (bare user id, or `group:<id>` for a group-owned wiki).
///
/// Group lookups run only when needed. The scope principal is derived
/// from the parent chain via [`WikiTree::resolve_scope_principal`].
///
/// # Errors
///
/// - [`AdminError::Wiki`] if the inherit chain cannot be resolved;
/// - [`AdminError::Db`] from the enrollment lookup.
pub async fn resolve_read_access(
    pool: &SqlitePool,
    tree: &WikiTree,
    handle: &WikiHandle,
    caller_sender_id: &str,
) -> Result<ReadAccessOutcome, AdminError> {
    let principal = tree
        .resolve_scope_principal(handle.meta())
        .map_err(AdminError::Wiki)?;
    match &principal {
        Principal::User(owner_id) => {
            if caller_sender_id == owner_id {
                return Ok(ReadAccessOutcome::Owner);
            }
        },
        // The builtin everyone-group as owner = a public wiki: anyone
        // authenticated reads, but nobody is its individual owner.
        Principal::Group(g) if g == "global" => {
            return Ok(ReadAccessOutcome::Global);
        },
        // A real group as owner: every member is owner-equivalent
        // (read / comment / edit / push). One SQL round-trip.
        Principal::Group(g) => {
            let memberships = crate::enrollment::groups_for(pool, caller_sender_id).await?;
            if memberships.iter().any(|m| m == g) {
                return Ok(ReadAccessOutcome::OwnerGroupMember(g.clone()));
            }
        },
    }
    // shared_with cheap-passes first: user-direct + global don't need
    // any DB round-trip.
    let mut group_entries: Vec<String> = Vec::new();
    let mut saw_global = false;
    for entry in &handle.meta().shared_with {
        match entry {
            Principal::User(id) if id == caller_sender_id => {
                return Ok(ReadAccessOutcome::SharedUser);
            },
            Principal::Group(g) if g == "global" => saw_global = true,
            Principal::Group(g) => group_entries.push(g.clone()),
            Principal::User(_) => {},
        }
    }
    if !group_entries.is_empty() {
        let memberships = crate::enrollment::groups_for(pool, caller_sender_id).await?;
        if let Some(group_id) = group_entries
            .iter()
            .find(|g| memberships.iter().any(|m| m == g.as_str()))
        {
            return Ok(ReadAccessOutcome::SharedGroup(group_id.clone()));
        }
    }
    if saw_global {
        return Ok(ReadAccessOutcome::Global);
    }
    let owner = match &principal {
        Principal::User(id) => id.clone(),
        Principal::Group(g) => format!("group:{g}"),
    };
    Ok(ReadAccessOutcome::Denied { owner })
}

/// Request-shape validation for the `pages` of a push, run BEFORE any
/// disk write — `push_create` in particular must reject a bad path
/// before forging the wiki directory, or the failed create leaves a
/// half-made wiki on disk that the transaction rollback cannot remove.
///
/// Per page: safe path, no `_meta.md`, no case variant of a reserved
/// filename or of the `.md` extension
/// ([`crate::wiki::page_path_case_hazard`]), and no two pages in the
/// same request whose paths differ only by ASCII case — they would be
/// one file on a smart consumer's case-insensitive local mirror.
fn validate_push_pages(pages: &[PushPage]) -> Result<(), AdminError> {
    let mut seen: HashMap<String, &str> = HashMap::with_capacity(pages.len());
    for page in pages {
        let pb = PathBuf::from(&page.path);
        if !crate::wiki::is_safe_page_path(&pb) {
            return Err(AdminError::InvalidInput(format!(
                "unsafe page path: {}",
                page.path
            )));
        }
        if page.path == META_FILENAME {
            return Err(AdminError::InvalidInput(format!(
                "{META_FILENAME} cannot be written via wiki_admin_push"
            )));
        }
        if let Some(hazard) = crate::wiki::page_path_case_hazard(&pb) {
            return Err(AdminError::InvalidInput(format!(
                "page {}: {hazard}",
                page.path
            )));
        }
        if let Some(prev) = seen.insert(page.path.to_ascii_lowercase(), &page.path) {
            let msg = if prev == page.path {
                format!("duplicate page path in push: {}", page.path)
            } else {
                format!(
                    "pages {prev} and {} differ only by case — a case-insensitive mirror treats them as the same file",
                    page.path
                )
            };
            return Err(AdminError::InvalidInput(msg));
        }
    }
    Ok(())
}

/// Write `pages` under `dir`. Request-shape validation (safe paths,
/// reserved names, intra-request duplicates) already ran in
/// [`validate_push_pages`]; this function only arbitrates against the
/// CURRENT disk state — the overwrite policy and case collisions with
/// existing entries ([`crate::wiki::page_case_conflict`]).
fn write_pages(
    dir: &Path,
    pages: &[PushPage],
    ops: &mut PushOpsApplied,
    allow_overwrite: bool,
) -> Result<(), AdminError> {
    for page in pages {
        let pb = PathBuf::from(&page.path);
        let abs = dir.join(&pb);
        let existed = abs.exists();
        if existed && !allow_overwrite {
            return Err(AdminError::InvalidInput(format!(
                "page {} already exists (create mode forbids overwrite)",
                page.path
            )));
        }
        if !existed && let Some(conflict) = crate::wiki::page_case_conflict(dir, &pb) {
            return Err(AdminError::InvalidInput(format!(
                "page {}: {conflict}",
                page.path
            )));
        }
        atomic_write(&abs, page.content.as_bytes())?;
        if existed {
            ops.updated += 1;
        } else {
            ops.created += 1;
        }
    }
    Ok(())
}

struct OpLogInsert<'a> {
    wiki_id: &'a WikiId,
    sender_id: &'a str,
    consumer_id: Option<&'a str>,
    actor_kind: ActorKind,
    op_kind: &'static str,
    op_mode: Option<&'static str>,
    payload_hash: &'a str,
    pages_affected: usize,
    pre_image_json: Option<&'a str>,
}

/// Parse + dedupe the `mark_processed` strings the caller
/// passed in. Returns the parsed primary keys (sorted ascending, no
/// duplicates) ready to be validated against the target wiki.
///
/// Performs the size cap check and the per-string shape check (via
/// [`crate::briefing::parse_bi_id`]) up-front — both of these are
/// cheap, so we surface them before any file write rather than after.
/// An unparseable id yields [`AdminError::UnknownBriefingItemId`]
/// because a malformed `bi_<garbage>` is wire-equivalent to "no such
/// briefing item" from the caller's perspective.
fn parse_mark_processed(wiki_id: &WikiId, raw: &[String]) -> Result<Vec<i64>, AdminError> {
    if raw.len() > MARK_PROCESSED_CAP_PER_PUSH {
        return Err(AdminError::TooManyBriefingItems {
            received: raw.len(),
            cap: MARK_PROCESSED_CAP_PER_PUSH,
        });
    }
    let mut seen: HashSet<i64> = HashSet::with_capacity(raw.len());
    let mut ids: Vec<i64> = Vec::with_capacity(raw.len());
    for s in raw {
        let id =
            crate::briefing::parse_bi_id(s).ok_or_else(|| AdminError::UnknownBriefingItemId {
                bi_id: s.clone(),
                wiki_id: wiki_id.clone(),
            })?;
        if seen.insert(id) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Validate that every `id` belongs to `wiki_id` and then
/// flip its `processed_at = now`. Both halves run on the same
/// transaction handle so a single rollback restores both the
/// briefing-items table and the `op_log` row inserted alongside.
///
/// Fail-fast on the first id that does not match the target wiki —
/// the whole transaction (page writes already done, `op_log` row
/// pending, `processed_at` flips pending) goes away together.
///
/// Returns the canonical `bi_<N>` strings for the marked rows in the
/// validation order (ascending by id) so the wire response is stable.
async fn validate_and_mark_processed(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    wiki_id: &WikiId,
    ids: &[i64],
    now: &str,
) -> Result<Vec<String>, AdminError> {
    let mut marked: Vec<String> = Vec::with_capacity(ids.len());
    for &id in ids {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT wiki_id FROM wiki_briefing_items WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        let Some((row_wiki_id,)) = row else {
            return Err(AdminError::UnknownBriefingItemId {
                bi_id: format!("bi_{id}"),
                wiki_id: wiki_id.clone(),
            });
        };
        if row_wiki_id != wiki_id.as_str() {
            return Err(AdminError::UnknownBriefingItemId {
                bi_id: format!("bi_{id}"),
                wiki_id: wiki_id.clone(),
            });
        }
        marked.push(format!("bi_{id}"));
    }
    if !ids.is_empty() {
        // Single batched UPDATE — the `wiki_id` clause is defence in
        // depth (already covered by the per-id validation above) so a
        // race that swapped a row's `wiki_id` between SELECT and UPDATE
        // would still refuse to mark it.
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE wiki_briefing_items SET processed_at = ? WHERE id IN ({placeholders}) AND wiki_id = ?",
        );
        let mut q = sqlx::query(&sql).bind(now);
        for &id in ids {
            q = q.bind(id);
        }
        q = q.bind(wiki_id.as_str());
        q.execute(&mut **tx).await?;
    }
    Ok(marked)
}

async fn record_op_log<'e, E>(executor: E, op: OpLogInsert<'_>) -> Result<i64, AdminError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let now = chrono::Utc::now().to_rfc3339();
    let pages_affected = i64::try_from(op.pages_affected).unwrap_or(i64::MAX);
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_admin_op_log
            (wiki_id, sender_id, consumer_id, actor_kind, op_kind, op_mode,
             payload_hash, pages_affected, pre_image_json, ts)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING op_id",
    )
    .bind(op.wiki_id.as_str())
    .bind(op.sender_id)
    .bind(op.consumer_id)
    .bind(op.actor_kind.wire())
    .bind(op.op_kind)
    .bind(op.op_mode)
    .bind(op.payload_hash)
    .bind(pages_affected)
    .bind(op.pre_image_json)
    .bind(&now)
    .fetch_one(executor)
    .await?;
    Ok(row.0)
}

/// Snapshot the on-disk content of every page about to be touched by
/// an upsert into a JSON document of shape:
///
/// ```json
/// { "pages": [ { "path": "<rel>", "content": "<body>" | null }, … ] }
/// ```
///
/// `content: null` distinguishes "page did not exist before" from
/// "page existed and was empty" — both legal pre-states the revert
/// handler must restore faithfully. Captures both `pages`
/// (overwrites) and `deletes` (removals) so a single column carries
/// everything the inverse pass needs without touching the filesystem
/// at revert time.
fn snapshot_pre_image(dir: &Path, req: &PushRequest) -> Result<String, AdminError> {
    let mut entries: Vec<serde_json::Value> =
        Vec::with_capacity(req.pages.len() + req.deletes.len());
    let mut seen: HashSet<String> = HashSet::new();
    for page in &req.pages {
        if !seen.insert(page.path.clone()) {
            continue;
        }
        entries.push(read_pre_image_entry(dir, &page.path)?);
    }
    for rel in &req.deletes {
        if !seen.insert(rel.clone()) {
            continue;
        }
        entries.push(read_pre_image_entry(dir, rel)?);
    }
    let doc = serde_json::json!({ "pages": entries });
    serde_json::to_string(&doc)
        .map_err(|e| AdminError::InvalidInput(format!("serialise pre_image_json: {e}")))
}

fn read_pre_image_entry(dir: &Path, rel_path: &str) -> Result<serde_json::Value, AdminError> {
    let abs = dir.join(rel_path);
    let content = match std::fs::read_to_string(&abs) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(AdminError::Io(e)),
    };
    Ok(serde_json::json!({ "path": rel_path, "content": content }))
}

/// SHA-256 of the canonical serialised input — paths + sorted page
/// checksums, no raw content (kept out of the audit row for
/// privacy). Always 64 hex chars.
fn payload_hash(req: &PushRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(req.mode.wire().as_bytes());
    if let Some(pid) = &req.project_id {
        h.update(b"project_id=");
        h.update(pid.as_bytes());
    }
    let mut page_summaries: Vec<(String, String)> = req
        .pages
        .iter()
        .map(|p| (p.path.clone(), content_digest(&p.content)))
        .collect();
    page_summaries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, sha) in &page_summaries {
        h.update(b"\n");
        h.update(path.as_bytes());
        h.update(b"=");
        h.update(sha.as_bytes());
    }
    let mut sorted_deletes = req.deletes.clone();
    sorted_deletes.sort();
    for d in &sorted_deletes {
        h.update(b"\nD:");
        h.update(d.as_bytes());
    }
    hex::encode(h.finalize())
}

fn content_digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// 64-char sha256 of the empty string. Used for `pull` `op_log` rows
/// where there is no payload to hash. Recomputed each call to keep
/// the call site self-contained.
fn empty_payload_hash() -> String {
    content_digest("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{IdentityKind, create_identity_wiki};
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::tempdir;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    async fn seeded_tree() -> (tempfile::TempDir, WikiTree, SqlitePool) {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        // Identity wiki for `alice` (parent of any smart wiki we
        // forge in tests).
        let alice = WikiId::parse("alice").unwrap();
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).unwrap();
        (dir, tree, pool)
    }

    fn alice_smart() -> AdminCaller {
        AdminCaller {
            sender_id: "alice".into(),
            consumer_id: Some("cc-laptop".into()),
            consumer_class: ConsumerClass::Smart,
        }
    }

    fn alice_standard() -> AdminCaller {
        AdminCaller {
            sender_id: "alice".into(),
            consumer_id: None,
            consumer_class: ConsumerClass::Standard,
        }
    }

    /// Whole-wiki content pull — the only shape that existed before the
    /// `paths` / `shape` modes.
    fn pull_all(wiki_id: &WikiId) -> PullRequest {
        PullRequest {
            wiki_id: wiki_id.clone(),
            paths: Vec::new(),
            shape: false,
        }
    }

    fn page(path: &str, content: &str) -> PushPage {
        PushPage {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn authored_refs_format_strips_md_and_normalises_separators() {
        let wiki = WikiId::parse("alice-proj").unwrap();
        let refs = authored_refs_for(
            &wiki,
            &[
                page("index.md", ""),
                page("modules/auth.md", ""),
                page("notes/raw", ""),      // no .md suffix → kept verbatim
                page("win\\sub\\p.md", ""), // backslashes normalised
            ],
        );
        assert_eq!(
            refs,
            vec![
                "[[alice-proj/index]]".to_owned(),
                "[[alice-proj/modules/auth]]".to_owned(),
                "[[alice-proj/notes/raw]]".to_owned(),
                "[[alice-proj/win/sub/p]]".to_owned(),
            ]
        );
    }

    fn create_smart_wiki_request(slug: &str) -> PushRequest {
        PushRequest {
            mode: PushMode::Create,
            wiki_id: None,
            parent_wiki_id: Some(WikiId::parse("alice").unwrap()),
            slug: Some(slug.into()),
            title: Some("Alice's lnprint companion".into()),
            wiki_type: Some("wiki-companion".into()),
            smart: true,
            project_id: Some("lnprint-abc123".into()),
            pages: vec![
                page("index.md", "# lnprint\n\nminimal landing\n"),
                page("modules/auth.md", "# auth\n\nmfa flow\n"),
            ],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        }
    }

    #[tokio::test]
    async fn standard_consumer_cannot_push() {
        let (_dir, tree, pool) = seeded_tree().await;
        let err = push(
            &pool,
            &tree,
            &alice_standard(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect_err("must reject standard");
        assert!(matches!(err, AdminError::RequiresSmart));
    }

    #[tokio::test]
    async fn create_smart_wiki_writes_meta_and_pages_and_op_log() {
        let (_dir, tree, pool) = seeded_tree().await;
        let resp = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create push");
        assert_eq!(resp.wiki_id.as_str(), "alice-lnprint");
        assert_eq!(resp.ops_applied.created, 2);
        assert_eq!(resp.ops_applied.updated, 0);
        assert_eq!(resp.ops_applied.deleted, 0);
        assert!(resp.warnings.is_empty());

        // Provenance breadcrumbs (group 17): one [[wiki_id/page]] per
        // written page, `.md` stripped, in request order. `_meta.md` is
        // not authorship and must not appear.
        assert_eq!(
            resp.authored_refs,
            vec![
                "[[alice-lnprint/index]]".to_owned(),
                "[[alice-lnprint/modules/auth]]".to_owned(),
            ]
        );

        // Filesystem assertions: _meta.md + 2 pages exist with the
        // expected content.
        let handle = tree.locate(&resp.wiki_id).expect("locate after create");
        assert_eq!(handle.meta().wiki_type, "wiki-companion");
        assert_eq!(handle.meta().title, "Alice's lnprint companion");
        let owner = resolve_owner_user(&tree, &handle).expect("owner");
        assert_eq!(owner, "alice");
        // project_id round-trips into extra.
        let pid = handle
            .meta()
            .extra
            .get(serde_yaml::Value::String("project_id".into()));
        assert_eq!(
            pid,
            Some(&serde_yaml::Value::String("lnprint-abc123".into()))
        );
        let pages = handle.list_pages().expect("list");
        assert_eq!(pages.len(), 2);

        // Op log row written with our wiki_id + consumer_id.
        let row: (String, String, Option<String>, String, i64) = sqlx::query_as(
            "SELECT wiki_id, sender_id, consumer_id, op_kind, pages_affected
               FROM wiki_admin_op_log WHERE op_id = ?",
        )
        .bind(resp.op_log_id)
        .fetch_one(&pool)
        .await
        .expect("op log row");
        assert_eq!(row.0, "alice-lnprint");
        assert_eq!(row.1, "alice");
        assert_eq!(row.2.as_deref(), Some("cc-laptop"));
        assert_eq!(row.3, "push_create");
        assert_eq!(row.4, 2);
    }

    #[tokio::test]
    async fn create_refuses_duplicate_wiki() {
        let (_dir, tree, pool) = seeded_tree().await;
        push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("first");
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect_err("second must reject");
        assert!(matches!(err, AdminError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_keeps_uppercase_page_paths_byte_faithful() {
        let (_dir, tree, pool) = seeded_tree().await;
        let mut req = create_smart_wiki_request("lnprint");
        req.pages = vec![
            page("Docs/Setup.md", "# Setup\n"),
            page("README.md", "# readme\n"),
        ];
        let resp = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect("uppercase pages create");
        let handle = tree.locate(&resp.wiki_id).expect("locate");
        assert!(handle.abs_dir().join("Docs/Setup.md").is_file());
        assert!(handle.abs_dir().join("README.md").is_file());
    }

    #[tokio::test]
    async fn create_rejects_bad_page_paths_before_forging_the_wiki() {
        // Atomicity: every page path is vetted BEFORE `_meta.md` lands on
        // disk, so a failed create leaves no half-made wiki directory.
        let (_dir, tree, pool) = seeded_tree().await;
        for bad in [
            "../escape.md", // traversal
            "_Meta.md",     // case variant of a reserved filename
            "notes.MD",     // .md extension the index would never pick up
        ] {
            let mut req = create_smart_wiki_request("lnprint");
            req.pages.push(page(bad, "x"));
            let err = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
                .await
                .expect_err("must reject");
            assert!(matches!(err, AdminError::InvalidInput(_)), "{bad}: {err:?}");
            let parent = tree.locate(&WikiId::parse("alice").unwrap()).unwrap();
            assert!(
                !parent.abs_dir().join("lnprint").exists(),
                "{bad}: failed create must not forge the wiki directory"
            );
        }
    }

    #[tokio::test]
    async fn create_rejects_pages_differing_only_by_case() {
        // `Setup.md` and `setup.md` are one file on a case-insensitive
        // mirror — the request is contradictory, refuse it whole.
        let (_dir, tree, pool) = seeded_tree().await;
        let mut req = create_smart_wiki_request("lnprint");
        req.pages = vec![page("Setup.md", "a"), page("setup.md", "b")];
        let err = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect_err("case twins must reject");
        match err {
            AdminError::InvalidInput(msg) => assert!(msg.contains("case"), "{msg}"),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upsert_rejects_case_collision_with_existing_entries() {
        let (_dir, tree, pool) = seeded_tree().await;
        let resp = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let upsert = |pages: Vec<PushPage>| PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(resp.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages,
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        // File-level collision: `Index.md` vs the existing `index.md`.
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert(vec![page("Index.md", "x")]),
        )
        .await
        .expect_err("file case collision must reject");
        assert!(matches!(err, AdminError::InvalidInput(_)), "{err:?}");
        // Directory-level collision: `Modules/` vs the existing `modules/`.
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert(vec![page("Modules/extra.md", "x")]),
        )
        .await
        .expect_err("dir case collision must reject");
        assert!(matches!(err, AdminError::InvalidInput(_)), "{err:?}");
        // Byte-exact overwrite and a fresh uppercase page both pass.
        let ok = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert(vec![page("index.md", "v2"), page("Changelog.md", "x")]),
        )
        .await
        .expect("byte-exact + fresh page upsert");
        assert_eq!(ok.ops_applied.updated, 1);
        assert_eq!(ok.ops_applied.created, 1);
    }

    #[tokio::test]
    async fn create_refuses_non_smart_request() {
        let (_dir, tree, pool) = seeded_tree().await;
        let mut req = create_smart_wiki_request("lnprint");
        // A smart consumer creating a NON-smart wiki (smart flag off) is
        // refused — smart-ness is now the explicit request flag, not the
        // wiki_type label.
        req.smart = false;
        let err = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect_err("must reject non-smart create");
        assert!(matches!(err, AdminError::WikiTypeNotAdminWritable { .. }));
    }

    #[tokio::test]
    async fn create_refuses_top_level_smart_wiki() {
        // Child-only gate, now per-kind: only the smart family
        // requires a parent (it inherits the parent's ACL scope), so a
        // smart-wiki create with `parent_wiki_id: None` must surface
        // `WikiTypeRequiresParent` (wire form `400 wiki_type_requires_parent`)
        // ahead of the generic "create requires parent_wiki_id".
        let (_dir, tree, pool) = seeded_tree().await;
        let req = PushRequest {
            mode: PushMode::Create,
            wiki_id: None,
            parent_wiki_id: None,
            slug: Some("lnprint".into()),
            title: Some("Top-level smart wiki".into()),
            wiki_type: Some("wiki-companion".into()),
            smart: true,
            project_id: None,
            pages: Vec::new(),
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let err = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect_err("top-level smart wiki must reject");
        match &err {
            AdminError::WikiTypeRequiresParent {
                wiki_type,
                expected_parent,
            } => {
                assert_eq!(wiki_type, "wiki-companion");
                // The message has to carry the value to pass: an agent
                // meeting this error on its first connect cannot guess it.
                assert_eq!(expected_parent, "alice");
            },
            other => panic!("expected WikiTypeRequiresParent, got {other:?}"),
        }
        assert!(
            err.to_string().contains("parent_wiki_id=\"alice\""),
            "{err}"
        );
    }

    #[tokio::test]
    async fn create_allows_child_smart_wiki_under_parent() {
        // Per-kind gate satisfied: a smart-wiki create with a
        // `parent_wiki_id` proceeds (slug derivation, parent locate, page
        // writes).
        let (_dir, tree, pool) = seeded_tree().await;
        let req = PushRequest {
            mode: PushMode::Create,
            wiki_id: None,
            parent_wiki_id: Some(WikiId::parse("alice").unwrap()),
            slug: Some("lnprint".into()),
            title: Some("Alice's lnprint companion".into()),
            wiki_type: Some("wiki-companion".into()),
            smart: true,
            project_id: None,
            pages: vec![page("index.md", "# lnprint\n")],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let resp = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect("child smart wiki with parent must succeed");
        assert_eq!(resp.wiki_id.as_str(), "alice-lnprint");
    }

    #[tokio::test]
    async fn upsert_overwrites_and_deletes() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let req = PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(create.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![
                page("index.md", "# lnprint v2\n"),          // overwrite
                page("modules/payments.md", "# payments\n"), // new
            ],
            deletes: vec!["modules/auth.md".into()],
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let resp = push(&pool, &tree, &alice_smart(), ActorKind::SmartConsumer, req)
            .await
            .expect("upsert");
        assert_eq!(resp.ops_applied.created, 1, "modules/payments.md is new");
        assert_eq!(resp.ops_applied.updated, 1, "index.md was overwritten");
        assert_eq!(resp.ops_applied.deleted, 1, "modules/auth.md was deleted");

        let handle = tree.locate(&create.wiki_id).unwrap();
        let index_body = handle.read_page(Path::new("index.md")).unwrap();
        assert!(index_body.contains("v2"));
        let pages: Vec<String> = handle
            .list_pages()
            .unwrap()
            .into_iter()
            .map(|p| p.rel_path.to_string_lossy().into_owned())
            .collect();
        assert!(pages.iter().any(|p| p.ends_with("payments.md")));
        assert!(!pages.iter().any(|p| p.ends_with("auth.md")));
    }

    #[tokio::test]
    async fn upsert_rejects_cross_user_write() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create as alice");
        let bob = AdminCaller {
            sender_id: "bob".into(),
            consumer_id: Some("cc-bob".into()),
            consumer_class: ConsumerClass::Smart,
        };
        let req = PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(create.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![page("index.md", "# intrusion\n")],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let err = push(&pool, &tree, &bob, ActorKind::SmartConsumer, req)
            .await
            .expect_err("bob must not write into alice's wiki");
        assert!(matches!(err, AdminError::WikiOwnedByOtherUser { .. }));
    }

    #[tokio::test]
    async fn upsert_refuses_meta_md_writes_and_deletes() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let bad_write = PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(create.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![page("_meta.md", "fake meta\n")],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            bad_write,
        )
        .await
        .expect_err("must reject _meta.md write");
        assert!(matches!(err, AdminError::InvalidInput(_)));

        let bad_delete = PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(create.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: Vec::new(),
            deletes: vec!["_meta.md".into()],
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            bad_delete,
        )
        .await
        .expect_err("must reject _meta.md delete");
        assert!(matches!(err, AdminError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn pull_returns_pages_and_op_log_head() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let resp = pull(&pool, &tree, &alice_smart(), &pull_all(&create.wiki_id))
            .await
            .expect("pull");
        assert_eq!(resp.pages.len(), 2);
        // Pull row is now the latest in the op log.
        assert!(resp.op_log_head.is_some());
        let head = resp.op_log_head.unwrap();
        // op_log_head returned by pull is the head BEFORE the pull
        // row was inserted (= the create row); the next pull will
        // surface this pull's own row.
        assert_eq!(head, create.op_log_id);

        // Re-pull observes the previous pull row as the new head.
        let resp2 = pull(&pool, &tree, &alice_smart(), &pull_all(&create.wiki_id))
            .await
            .expect("second pull");
        assert!(resp2.op_log_head.unwrap() > create.op_log_id);
    }

    /// A page whose blocks the section cap must cut mid-sentence.
    fn dense_page_body() -> String {
        format!(
            "# Decisioni\n\n{}",
            format!("{}\n\n", "x".repeat(3_000)).repeat(4)
        )
    }

    #[tokio::test]
    async fn pull_narrows_to_requested_paths() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let resp = pull(
            &pool,
            &tree,
            &alice_smart(),
            &PullRequest {
                wiki_id: create.wiki_id.clone(),
                paths: vec!["modules/auth.md".to_owned(), "does/not/exist.md".to_owned()],
                shape: false,
            },
        )
        .await
        .expect("narrowed pull");
        // The narrowing the smart-consumer skill has always documented,
        // now real; an unknown path is absent, not an error.
        assert_eq!(resp.pages.len(), 1);
        assert_eq!(resp.pages[0].path, "modules/auth.md");
        assert!(
            resp.pages[0]
                .content
                .as_ref()
                .is_some_and(|c| c.contains("mfa flow"))
        );
    }

    #[tokio::test]
    async fn pull_shape_mode_reports_without_the_bytes() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("decisions.md", &dense_page_body())],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert dense page");
        let resp = pull(
            &pool,
            &tree,
            &alice_smart(),
            &PullRequest {
                wiki_id: create.wiki_id.clone(),
                paths: Vec::new(),
                shape: true,
            },
        )
        .await
        .expect("shape pull");
        assert!(
            resp.pages.iter().all(|p| p.content.is_none()),
            "shape mode must not ship bytes"
        );
        let dense = resp
            .pages
            .iter()
            .find(|p| p.path == "decisions.md")
            .expect("dense page present");
        let shape = dense.shape.expect("shape computed");
        assert_eq!(shape.oversize_blocks, 4);
        assert!(shape.needs_repair());
        // The healthy pages of the same wiki stay quiet.
        let healthy = resp
            .pages
            .iter()
            .find(|p| p.path == "index.md")
            .expect("index present");
        assert!(!healthy.shape.expect("shape computed").needs_repair());
    }

    #[tokio::test]
    async fn push_warns_only_about_dense_smart_pages() {
        let (_dir, tree, pool) = seeded_tree().await;
        // A healthy create says nothing.
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        assert!(create.warnings.is_empty());

        let dense = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("decisions.md", &dense_page_body())],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert");
        assert_eq!(dense.warnings.len(), 1, "{:?}", dense.warnings);
        assert!(dense.warnings[0].contains("decisions.md"));

        // Same bytes on a standard wiki: no sections, so no warning.
        let alice_id = WikiId::parse("alice").unwrap();
        let standard = push(
            &pool,
            &tree,
            &alice_dashboard(),
            ActorKind::Dashboard,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(alice_id),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("decisions.md", &dense_page_body())],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("dashboard upsert");
        assert!(standard.warnings.is_empty());
    }

    #[tokio::test]
    async fn pull_requires_smart_consumer() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let err = pull(&pool, &tree, &alice_standard(), &pull_all(&create.wiki_id))
            .await
            .expect_err("standard must not pull");
        assert!(matches!(err, AdminError::RequiresSmart));
    }

    // ---------- shared_with / resolve_read_access ----------

    /// Rewrite a smart wiki's `_meta.md` to inject a
    /// `shared_with: [...]` roster. We touch the file directly because
    /// the dashboard "/sharing" route that owns this surface lands
    /// later — the test still needs to assert the resolver behaves
    /// correctly today.
    fn inject_shared_with(tree: &WikiTree, wiki_id: &WikiId, entries: &[&str]) {
        use std::fmt::Write as _;
        let handle = tree.locate(wiki_id).expect("locate smart wiki");
        let abs = handle.abs_dir().join(crate::wiki::META_FILENAME);
        let raw = std::fs::read_to_string(&abs).expect("read meta");
        // Insert `shared_with` right after the `title:` line — always
        // present, and the canonical order the serializer emits places
        // `shared_with` shortly after it (the `acl_default` line is retired).
        let mut out = String::with_capacity(raw.len() + 128);
        for line in raw.lines() {
            out.push_str(line);
            out.push('\n');
            if line.starts_with("title:") {
                out.push_str("shared_with:\n");
                for e in entries {
                    writeln!(out, "  - {e}").expect("write to string");
                }
            }
        }
        std::fs::write(&abs, out).expect("write meta");
    }

    /// Re-home a wiki onto a **group** owner so the resolver derives a
    /// `group:<id>` scope principal — the case the dashboard 500-ed on.
    ///
    /// Ownership is no longer declared in `_meta.md`; it is derived from the
    /// root identity wiki's type. So we (1) create the `group_id` group
    /// identity wiki (a `wiki-group` root) and (2) rewrite this wiki's
    /// `parent_wiki_id` to point at it. `resolve_scope_principal` resolves
    /// the parent by id (not physical nesting), walks up to the `wiki-group`
    /// root, and yields `group:<group_id>`.
    fn rehome_under_group(tree: &WikiTree, wiki_id: &WikiId, group_id: &str) {
        use std::fmt::Write as _;

        use crate::wiki::{IdentityKind, create_identity_wiki};
        let gid = WikiId::parse(group_id).expect("group id");
        create_identity_wiki(tree, &gid, group_id, IdentityKind::Group).expect("create group root");
        let handle = tree.locate(wiki_id).expect("locate");
        let abs = handle.abs_dir().join(crate::wiki::META_FILENAME);
        let raw = std::fs::read_to_string(&abs).expect("read meta");
        let mut out = String::with_capacity(raw.len());
        for line in raw.lines() {
            if line.starts_with("parent_wiki_id:") {
                writeln!(out, "parent_wiki_id: {group_id}").expect("write to string");
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        std::fs::write(&abs, out).expect("write meta");
    }

    #[tokio::test]
    async fn resolve_read_access_owner_path() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();
        let access = resolve_read_access(&pool, &tree, &handle, "alice")
            .await
            .expect("resolve");
        assert!(matches!(access, ReadAccessOutcome::Owner));
        assert!(access.is_granted());
    }

    #[tokio::test]
    async fn resolve_read_access_shared_user_path() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        inject_shared_with(&tree, &create.wiki_id, &["user:bob", "user:carol"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();

        let bob = resolve_read_access(&pool, &tree, &handle, "bob")
            .await
            .expect("resolve bob");
        assert!(matches!(bob, ReadAccessOutcome::SharedUser));

        let mallory = resolve_read_access(&pool, &tree, &handle, "mallory")
            .await
            .expect("resolve mallory");
        assert!(matches!(mallory, ReadAccessOutcome::Denied { ref owner } if owner == "alice"));
        assert!(!mallory.is_granted());
    }

    #[tokio::test]
    async fn resolve_read_access_shared_group_path() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        // Seed enrollment_groups membership so `groups_for("bob")`
        // returns `lnprint-devs`. `members` is a JSON array text column
        // per migration 0006 — `enrollment::mirror_to_db` would do the
        // same shape; we INSERT directly to avoid carrying the whole
        // enrollment.yaml flow into this test.
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("lnprint-devs")
            .bind(r#"["bob"]"#)
            .execute(&pool)
            .await
            .unwrap();
        inject_shared_with(&tree, &create.wiki_id, &["group:lnprint-devs"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();

        let bob = resolve_read_access(&pool, &tree, &handle, "bob")
            .await
            .expect("resolve bob");
        match bob {
            ReadAccessOutcome::SharedGroup(g) => assert_eq!(g, "lnprint-devs"),
            other => panic!("expected SharedGroup, got {other:?}"),
        }

        // Mallory is not in the group.
        let mallory = resolve_read_access(&pool, &tree, &handle, "mallory")
            .await
            .expect("resolve mallory");
        assert!(matches!(mallory, ReadAccessOutcome::Denied { .. }));
    }

    #[tokio::test]
    async fn resolve_read_access_global_path_grants_anyone() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        inject_shared_with(&tree, &create.wiki_id, &["global"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();

        let stranger = resolve_read_access(&pool, &tree, &handle, "anyone")
            .await
            .expect("resolve");
        assert!(matches!(stranger, ReadAccessOutcome::Global));
    }

    #[tokio::test]
    async fn resolve_read_access_group_owned_member_is_owner_equivalent() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        // Re-home the wiki onto a GROUP owner (the case the dashboard 500-ed on)
        // and seed the membership the resolver consults.
        rehome_under_group(&tree, &create.wiki_id, "famiglia");
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("famiglia")
            .bind(r#"["bob"]"#)
            .execute(&pool)
            .await
            .unwrap();
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();

        // A member of the owning group is owner-equivalent.
        let bob = resolve_read_access(&pool, &tree, &handle, "bob")
            .await
            .expect("resolve bob");
        match bob {
            ReadAccessOutcome::OwnerGroupMember(ref g) => assert_eq!(g, "famiglia"),
            other => panic!("expected OwnerGroupMember, got {other:?}"),
        }
        assert!(bob.is_owner_equivalent());
        assert!(bob.is_granted());

        // A non-member gets no wiki-level grant (per-fragment ACL still governs
        // content elsewhere).
        let mallory = resolve_read_access(&pool, &tree, &handle, "mallory")
            .await
            .expect("resolve mallory");
        assert!(matches!(mallory, ReadAccessOutcome::Denied { .. }));
        assert!(!mallory.is_owner_equivalent());
    }

    #[tokio::test]
    async fn enforce_admin_auth_admits_owning_group_member() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        rehome_under_group(&tree, &create.wiki_id, "famiglia");
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("famiglia")
            .bind(r#"["bob"]"#)
            .execute(&pool)
            .await
            .unwrap();
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let handle = tree.locate(&create.wiki_id).unwrap();

        // A member of the owning group is owner-equivalent → may write/push.
        let member = AdminCaller {
            sender_id: "bob".into(),
            consumer_id: Some("cc-laptop".into()),
            consumer_class: ConsumerClass::Smart,
        };
        enforce_admin_auth(&pool, &tree, &handle, &member, ActorKind::SmartConsumer)
            .await
            .expect("a member of the owning group may write");

        // A non-member is refused (sharing would grant reads, not writes).
        let stranger = AdminCaller {
            sender_id: "mallory".into(),
            consumer_id: Some("cc-laptop".into()),
            consumer_class: ConsumerClass::Smart,
        };
        let err = enforce_admin_auth(&pool, &tree, &handle, &stranger, ActorKind::SmartConsumer)
            .await
            .expect_err("a non-member is refused");
        assert!(matches!(err, AdminError::WikiOwnedByOtherUser { .. }));
    }

    #[tokio::test]
    async fn shared_with_does_not_grant_write_access() {
        // Sharing is read+notify only — the
        // `wiki.owner_user == token.owner_user` invariant is preserved.
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        inject_shared_with(&tree, &create.wiki_id, &["user:bob"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();

        // bob has a smart token but is NOT the owner.
        let bob_smart = AdminCaller {
            sender_id: "bob".into(),
            consumer_id: Some("cc-bob-laptop".into()),
            consumer_class: ConsumerClass::Smart,
        };
        let err = push(
            &pool,
            &tree,
            &bob_smart,
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# bob's edit\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect_err("shared_with does not grant write");
        assert!(
            matches!(err, AdminError::WikiOwnedByOtherUser { ref owner, .. } if owner == "alice"),
            "expected WikiOwnedByOtherUser, got {err:?}"
        );

        // pull is also write-restricted (smart-only) so bob can't pull
        // either. The MVP keeps both write tools owner-only.
        let pull_err = pull(&pool, &tree, &bob_smart, &pull_all(&create.wiki_id))
            .await
            .expect_err("pull also stays owner-only in MVP");
        assert!(matches!(pull_err, AdminError::WikiOwnedByOtherUser { .. }));
    }

    #[tokio::test]
    async fn upsert_blocked_by_active_lease_from_other_consumer() {
        // End-to-end: same owner, two different devices. The
        // desktop holds an active lease; the laptop's upsert must fail
        // with WikiLockedByLease carrying the desktop's coordinates.
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");

        // Desktop acquires a 60s lease.
        let desktop = AdminCaller {
            sender_id: "alice".into(),
            consumer_id: Some("cc-desktop".into()),
            consumer_class: ConsumerClass::Smart,
        };
        let lease = crate::wiki_admin_leases::acquire(&pool, &desktop, &create.wiki_id, Some(60))
            .await
            .expect("desktop acquires lease");

        // Laptop (alice_smart) tries to upsert while the lease is held
        // by the desktop — must be refused with WikiLockedByLease.
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# laptop edit\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect_err("laptop must be blocked by desktop's lease");
        let AdminError::WikiLockedByLease {
            held_by_consumer_id,
            held_by_sender_id,
            ..
        } = err
        else {
            panic!("expected WikiLockedByLease, got {err:?}");
        };
        assert_eq!(held_by_consumer_id.as_deref(), Some("cc-desktop"));
        assert_eq!(held_by_sender_id, "alice");

        // After the desktop releases, the laptop's upsert succeeds.
        crate::wiki_admin_leases::release(&pool, &desktop, &lease.lease_id)
            .await
            .expect("desktop releases lease");
        push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# laptop edit\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert succeeds once lease is released");
    }

    #[tokio::test]
    async fn upsert_passes_through_when_same_consumer_holds_lease() {
        // Self-held lease must not block the caller's own pushes.
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");

        crate::wiki_admin_leases::acquire(&pool, &alice_smart(), &create.wiki_id, Some(60))
            .await
            .expect("acquire");
        push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# self edit\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("self-held lease lets the same caller upsert");
    }

    // ---------- actor_kind discipline ----------

    /// Caller shape for a dashboard editor save: no smart class
    /// required, no `consumer_id` plumbed through (the human is at
    /// the dashboard, not behind an MCP token).
    fn alice_dashboard() -> AdminCaller {
        AdminCaller {
            sender_id: "alice".into(),
            consumer_id: None,
            // The `consumer_class` field is irrelevant on the
            // `Dashboard` path — the gate is bypassed. We pin
            // `Standard` here to assert the bypass is real (the
            // smart-consumer gate would reject this caller).
            consumer_class: ConsumerClass::Standard,
        }
    }

    #[tokio::test]
    async fn dashboard_actor_kind_bypasses_smart_family_gate() {
        // A dashboard write must succeed on a
        // non-smart wiki (here `wiki-user`, Alice's identity
        // wiki seeded by the fixture) and produce an op-log row
        // tagged `actor_kind = 'dashboard'`. Same wiki + same
        // request shape under `SmartConsumer` would be rejected by
        // the smart-family gate — covered by the sibling test
        // below.
        let (_dir, tree, pool) = seeded_tree().await;
        let alice_id = WikiId::parse("alice").unwrap();

        let resp = push(
            &pool,
            &tree,
            &alice_dashboard(),
            ActorKind::Dashboard,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(alice_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# dashboard-typed note\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("dashboard upsert on wiki-user must succeed");

        assert_eq!(resp.ops_applied.created, 1);

        let (actor_kind, sender_id, consumer_id): (String, String, Option<String>) =
            sqlx::query_as(
                "SELECT actor_kind, sender_id, consumer_id
                   FROM wiki_admin_op_log WHERE op_id = ?",
            )
            .bind(resp.op_log_id)
            .fetch_one(&pool)
            .await
            .expect("op log row");
        assert_eq!(actor_kind, "dashboard");
        assert_eq!(sender_id, "alice");
        assert!(
            consumer_id.is_none(),
            "dashboard writes carry no consumer_id (the operator is at the editor, not behind an MCP device)"
        );
    }

    #[tokio::test]
    async fn smart_consumer_actor_kind_still_enforces_smart_family_gate() {
        // Backward-compat: the same upsert request under
        // `SmartConsumer` must still trip `WikiTypeNotAdminWritable`
        // because `wiki-user` is not in the smart family.
        let (_dir, tree, pool) = seeded_tree().await;
        let alice_id = WikiId::parse("alice").unwrap();
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(alice_id),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("notes.md", "# smart attempt\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect_err("smart consumer must NOT be able to upsert into a non-smart wiki");
        assert!(
            matches!(err, AdminError::WikiTypeNotAdminWritable { ref wiki_type } if wiki_type == "wiki-user"),
            "expected WikiTypeNotAdminWritable on wiki-user, got {err:?}"
        );
    }

    #[tokio::test]
    async fn expected_op_log_head_gate_rejects_stale_upsert_but_ignores_pulls() {
        // Optimistic concurrency (20e): an upsert carrying the current
        // write head succeeds; a `pull` in between must NOT look like a
        // conflict (it writes a non-`push_` op-log row); a stale head is
        // rejected with `ConflictingOpLogHead` naming the real heads.
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let wiki_id = create.wiki_id.clone();

        let guarded_upsert = |expected: i64| PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![page("index.md", "# lnprint\n\nbumped\n")],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: Some(expected),
        };

        // Guarded by the current head → success, head advances.
        let up1 = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            guarded_upsert(create.op_log_id),
        )
        .await
        .expect("upsert guarded by the current head must pass");
        assert!(up1.op_log_id > create.op_log_id);

        // A pull writes a `pull` op-log row (raw MAX(op_id) advances) but
        // must not trip the gate: an upsert guarded by the last *write*
        // head still passes.
        pull(&pool, &tree, &alice_smart(), &pull_all(&wiki_id))
            .await
            .expect("pull");
        let up2 = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            guarded_upsert(up1.op_log_id),
        )
        .await
        .expect("a pull between writes must not look like a conflict");
        assert!(up2.op_log_id > up1.op_log_id);

        // Re-using the now-stale head is rejected, naming both heads.
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            guarded_upsert(up1.op_log_id),
        )
        .await
        .expect_err("a stale expected_op_log_head must conflict");
        assert!(
            matches!(
                err,
                AdminError::ConflictingOpLogHead { expected, actual, .. }
                    if expected == up1.op_log_id && actual == up2.op_log_id
            ),
            "expected ConflictingOpLogHead {{expected: {}, actual: {}}}, got {err:?}",
            up1.op_log_id,
            up2.op_log_id,
        );
    }

    #[tokio::test]
    async fn pre_image_json_is_populated_on_upsert_and_null_on_create() {
        // Round-trip: the `pre_image_json` column carries a
        // deterministic JSON snapshot of touched pages on upsert
        // (with `content: null` for newly created paths and
        // `content: "<body>"` for overwritten ones), and stays NULL
        // on create rows (the wiki did not exist before).
        let (_dir, tree, pool) = seeded_tree().await;

        // Create — pre_image_json must be NULL.
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let create_pre_image: Option<String> =
            sqlx::query_scalar("SELECT pre_image_json FROM wiki_admin_op_log WHERE op_id = ?")
                .bind(create.op_log_id)
                .fetch_one(&pool)
                .await
                .expect("query create row");
        assert!(
            create_pre_image.is_none(),
            "create rows must have pre_image_json IS NULL (the wiki did not exist before): {create_pre_image:?}"
        );

        // Upsert — pre_image_json must list both pages: index.md
        // already exists (content = current body), modules/new.md
        // does not (content = null).
        let upsert = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![
                    page("index.md", "# lnprint v2\n"),
                    page("modules/new.md", "# brand new\n"),
                ],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert");

        let raw: String =
            sqlx::query_scalar("SELECT pre_image_json FROM wiki_admin_op_log WHERE op_id = ?")
                .bind(upsert.op_log_id)
                .fetch_one(&pool)
                .await
                .expect("query upsert row");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("pre_image_json must be valid JSON");
        let pages = parsed["pages"]
            .as_array()
            .expect("pre_image_json.pages must be an array");
        assert_eq!(pages.len(), 2, "two pages touched: {parsed}");

        let index_entry = pages
            .iter()
            .find(|p| p["path"] == "index.md")
            .expect("index.md in pre-image");
        let index_body = index_entry["content"]
            .as_str()
            .expect("index.md existed before the upsert, content must be a string");
        assert!(
            index_body.contains("# lnprint"),
            "captured pre-write body must match what was on disk: {index_body}"
        );

        let new_entry = pages
            .iter()
            .find(|p| p["path"] == "modules/new.md")
            .expect("modules/new.md in pre-image");
        assert!(
            new_entry["content"].is_null(),
            "modules/new.md did not exist before — content must be JSON null: {new_entry}"
        );
    }

    // ---------- op_revert handler ----------

    /// Seed a smart-wiki + take a baseline upsert that touches
    /// `index.md` (already existed) and `modules/new.md` (did not). The
    /// upsert's `pre_image_json` is what every revert test below
    /// rolls back to.
    async fn seed_revertable_upsert() -> (
        tempfile::TempDir,
        WikiTree,
        SqlitePool,
        WikiId,
        i64, // create op_log_id
        i64, // upsert op_log_id (the revert target)
    ) {
        let (dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        // Sleep 1ms to make sure the ts strings are strictly ordered —
        // SQLite stores them as TEXT and the conflict check uses
        // `ts > target.ts`.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let upsert = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![
                    page("index.md", "# lnprint v2\n"), // overwrite of "# lnprint\n…"
                    page("modules/new.md", "# brand new\n"), // brand-new file
                ],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert");
        (
            dir,
            tree,
            pool,
            create.wiki_id,
            create.op_log_id,
            upsert.op_log_id,
        )
    }

    #[tokio::test]
    async fn op_revert_restores_pre_image_for_upsert() {
        let (_dir, tree, pool, wiki_id, _create_id, target_id) = seed_revertable_upsert().await;
        let outcome = op_revert(&pool, &tree, target_id, "alice")
            .await
            .expect("revert must succeed: no later op touches the same pages");
        // The compensating op_id is greater than the target.
        assert!(outcome.compensating_op_id > target_id);
        assert_eq!(outcome.restored_pages.len(), 2);

        // index.md is back to the pre-upsert body ("# lnprint\n\nminimal
        // landing\n" from `create_smart_wiki_request`).
        let handle = tree.locate(&wiki_id).expect("locate");
        let restored = std::fs::read_to_string(handle.abs_dir().join("index.md")).expect("read");
        assert!(
            restored.contains("minimal landing"),
            "restored index.md must carry the pre-upsert body, got: {restored:?}"
        );
        // modules/new.md is gone (didn't exist before the target).
        assert!(
            !handle.abs_dir().join("modules/new.md").exists(),
            "modules/new.md must have been deleted on revert (it did not exist pre-target)"
        );
    }

    #[tokio::test]
    async fn op_revert_deletes_page_when_pre_image_content_is_null() {
        // Build a stand-alone scenario where the *only* touched page
        // had `content: None` in the pre-image — proves the delete
        // branch fires independently of any restore branch.
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let upsert = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(create.wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("modules/payments.md", "# payments\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("upsert that creates a brand-new page");

        let handle = tree.locate(&create.wiki_id).unwrap();
        assert!(handle.abs_dir().join("modules/payments.md").exists());

        let outcome = op_revert(&pool, &tree, upsert.op_log_id, "alice")
            .await
            .expect("revert");
        assert_eq!(outcome.restored_pages, vec!["modules/payments.md"]);
        // The file is gone — pre-image had `content: null`.
        assert!(
            !handle.abs_dir().join("modules/payments.md").exists(),
            "page must be deleted when pre-image content was null"
        );
    }

    #[tokio::test]
    async fn op_revert_rejects_with_target_changed_when_later_op_touched_same_page() {
        let (_dir, tree, pool, wiki_id, _create_id, target_id) = seed_revertable_upsert().await;
        // A later upsert touches `index.md` again — the conflict gate
        // must trip.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let later = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("index.md", "# lnprint v3 (independent edit)\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("independent later edit");

        let err = op_revert(&pool, &tree, target_id, "alice")
            .await
            .expect_err("strict conflict policy must refuse");
        let RevertError::TargetChanged {
            conflicting_ops,
            conflicting_pages,
        } = err
        else {
            panic!("expected TargetChanged, got {err:?}");
        };
        assert_eq!(conflicting_ops, vec![later.op_log_id]);
        assert_eq!(conflicting_pages, vec!["index.md".to_owned()]);

        // The op log carries no compensating row — the revert did not
        // run.
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_admin_op_log
              WHERE wiki_id = ? AND actor_kind = 'system'",
        )
        .bind(wiki_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row_count, 0, "no compensating row on refusal");
    }

    #[tokio::test]
    async fn op_revert_rejects_when_pre_image_is_null() {
        // `Create` rows carry `pre_image_json = NULL` — there is no
        // pre-state to roll back to.
        let (_dir, tree, pool, _wiki_id, create_id, _target_id) = seed_revertable_upsert().await;
        let err = op_revert(&pool, &tree, create_id, "alice")
            .await
            .expect_err("create rows are not revertable");
        assert!(
            matches!(err, RevertError::NoPreImage { .. }),
            "expected NoPreImage, got {err:?}"
        );
    }

    #[tokio::test]
    async fn op_revert_rejects_when_op_is_pull_or_notify() {
        // `pull` writes a row to the op log too — but the row is a
        // read and has no pre-image. The revert handler must refuse.
        let (_dir, tree, pool, wiki_id, _create_id, _target_id) = seed_revertable_upsert().await;
        // Run a pull to produce a `pull` row.
        let _ = pull(&pool, &tree, &alice_smart(), &pull_all(&wiki_id))
            .await
            .unwrap();
        let pull_op_id: i64 = sqlx::query_scalar(
            "SELECT op_id FROM wiki_admin_op_log
              WHERE wiki_id = ? AND op_kind = 'pull'
              ORDER BY op_id DESC LIMIT 1",
        )
        .bind(wiki_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        let err = op_revert(&pool, &tree, pull_op_id, "alice")
            .await
            .expect_err("pull rows are not revertable (non-write)");
        let RevertError::NotRevertable { reason, .. } = err else {
            panic!("expected NotRevertable, got {err:?}");
        };
        assert!(
            reason.contains("non-write"),
            "reason must mention non-write: {reason}"
        );
    }

    #[tokio::test]
    async fn op_revert_writes_compensating_row_with_actor_kind_system_and_proper_pre_image() {
        // The compensating row must (a) carry `actor_kind='system'` +
        // `consumer_id IS NULL` + `sender_id=reverted_by`, and (b)
        // carry its own `pre_image_json` snapshotting the *post-state*
        // of the target row (= what was on disk just before the
        // revert wrote). So if a user clicks the compensation row,
        // they get back the state the original push achieved.
        let (_dir, tree, pool, wiki_id, _create_id, target_id) = seed_revertable_upsert().await;
        let outcome = op_revert(&pool, &tree, target_id, "alice")
            .await
            .expect("revert");

        let (actor_kind, sender_id, consumer_id, op_kind, op_mode, pre_image): (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT actor_kind, sender_id, consumer_id, op_kind, op_mode, pre_image_json
               FROM wiki_admin_op_log WHERE op_id = ?",
        )
        .bind(outcome.compensating_op_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(actor_kind, "system");
        assert_eq!(sender_id, "alice");
        assert!(
            consumer_id.is_none(),
            "compensating rows carry no consumer_id"
        );
        assert_eq!(op_kind, "push_upsert");
        assert_eq!(op_mode.as_deref(), Some("upsert"));

        // The compensating row's pre_image_json is the post-state of
        // the target op (i.e. what the target push wrote): `index.md`
        // = "# lnprint v2\n", `modules/new.md` = "# brand new\n".
        let parsed: serde_json::Value = serde_json::from_str(&pre_image).unwrap();
        let pages = parsed["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 2);

        let index_entry = pages
            .iter()
            .find(|p| p["path"] == "index.md")
            .expect("index.md in compensation pre-image");
        assert_eq!(index_entry["content"].as_str(), Some("# lnprint v2\n"));

        let new_entry = pages
            .iter()
            .find(|p| p["path"] == "modules/new.md")
            .expect("modules/new.md in compensation pre-image");
        assert_eq!(new_entry["content"].as_str(), Some("# brand new\n"));

        // System compensation rows are themselves non-revertable
        // (chained revert-of-revert is performed by re-clicking the
        // original target).
        let err = op_revert(&pool, &tree, outcome.compensating_op_id, "alice")
            .await
            .expect_err("system rows must refuse revert");
        assert!(matches!(err, RevertError::NotRevertable { .. }));

        // wiki_id used by sqlx assertion above came from a clone-free
        // borrow; silence "unused" lint for `wiki_id`.
        let _ = &wiki_id;
    }

    #[tokio::test]
    async fn op_revert_succeeds_when_later_op_touched_disjoint_pages() {
        // A newer push that touches a different page must NOT block
        // the revert. Only intersecting page sets are conflicts.
        let (_dir, tree, pool, wiki_id, _create_id, target_id) = seed_revertable_upsert().await;
        std::thread::sleep(std::time::Duration::from_millis(5));
        push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            PushRequest {
                mode: PushMode::Upsert,
                wiki_id: Some(wiki_id.clone()),
                parent_wiki_id: None,
                slug: None,
                title: None,
                wiki_type: None,
                smart: false,
                project_id: None,
                pages: vec![page("docs/unrelated.md", "# disjoint\n")],
                deletes: Vec::new(),
                mark_processed: Vec::new(),
                expected_op_log_head: None,
            },
        )
        .await
        .expect("disjoint later edit");

        let outcome = op_revert(&pool, &tree, target_id, "alice")
            .await
            .expect("revert must succeed because later ops touched disjoint pages");
        assert_eq!(outcome.restored_pages.len(), 2);
        // The disjoint page is untouched.
        let handle = tree.locate(&wiki_id).unwrap();
        let disjoint =
            std::fs::read_to_string(handle.abs_dir().join("docs/unrelated.md")).expect("read");
        assert!(disjoint.contains("disjoint"));
    }

    #[tokio::test]
    async fn op_revert_treats_pre_c11_rows_with_null_pre_image_in_history_as_conflict() {
        // A legacy row in the history would have `pre_image_json IS
        // NULL`. The conservative branch in the conflict scan must
        // treat such rows as touching every target page → strict
        // refusal. Simulate by manually inserting a later row whose
        // `pre_image_json` is NULL.
        let (_dir, tree, pool, wiki_id, _create_id, target_id) = seed_revertable_upsert().await;

        let future_ts = (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
        sqlx::query(
            "INSERT INTO wiki_admin_op_log
                (wiki_id, sender_id, consumer_id, actor_kind, op_kind, op_mode,
                 payload_hash, pages_affected, pre_image_json, ts)
             VALUES (?, 'alice', NULL, 'smart_consumer', 'push_upsert', 'upsert',
                     'deadbeef', 1, NULL, ?)",
        )
        .bind(wiki_id.as_str())
        .bind(&future_ts)
        .execute(&pool)
        .await
        .unwrap();

        let err = op_revert(&pool, &tree, target_id, "alice")
            .await
            .expect_err("legacy row with NULL pre-image must trigger conservative conflict");
        assert!(
            matches!(err, RevertError::TargetChanged { .. }),
            "expected TargetChanged, got {err:?}"
        );
    }

    #[tokio::test]
    async fn op_revert_returns_not_found_for_unknown_op_id() {
        let (_dir, tree, pool) = seeded_tree().await;
        let err = op_revert(&pool, &tree, 99_999, "alice")
            .await
            .expect_err("unknown op_id must be NotFound");
        assert!(matches!(err, RevertError::NotFound(99_999)));
    }

    // ---------- wiki_admin_push.mark_processed ----------

    /// Seed one row in `wiki_briefing_items` and return its primary
    /// key. `processed_at` defaults to NULL (= pending) so the tests
    /// can assert the post-push flip.
    async fn seed_briefing_item(pool: &SqlitePool, wiki_id: &WikiId, body: &str) -> i64 {
        let now = chrono::Utc::now().to_rfc3339();
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO wiki_briefing_items
                (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, author_sender_id, processed_at)
             VALUES (?, 'dashboard_comment', 'dashboard:alice', 'seed', ?, NULL, ?, NULL, 'alice', NULL)
             RETURNING id",
        )
        .bind(wiki_id.as_str())
        .bind(body)
        .bind(&now)
        .fetch_one(pool)
        .await
        .expect("seed briefing");
        row.0
    }

    async fn pending_count(pool: &SqlitePool, wiki_id: &WikiId) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id = ? AND processed_at IS NULL",
        )
        .bind(wiki_id.as_str())
        .fetch_one(pool)
        .await
        .expect("count pending")
    }

    async fn op_log_count(pool: &SqlitePool, wiki_id: &WikiId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM wiki_admin_op_log WHERE wiki_id = ?")
            .bind(wiki_id.as_str())
            .fetch_one(pool)
            .await
            .expect("count op_log")
    }

    fn upsert_with_marks(wiki_id: &WikiId, marks: Vec<String>) -> PushRequest {
        PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![page("index.md", "# updated\n")],
            deletes: Vec::new(),
            mark_processed: marks,
            expected_op_log_head: None,
        }
    }

    #[tokio::test]
    async fn push_marks_listed_briefing_items_processed_atomically() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let bi = seed_briefing_item(&pool, &create.wiki_id, "comment on index").await;
        let pre_oplog = op_log_count(&pool, &create.wiki_id).await;
        assert_eq!(pending_count(&pool, &create.wiki_id).await, 1);

        let resp = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(&create.wiki_id, vec![format!("bi_{bi}")]),
        )
        .await
        .expect("push with mark_processed");

        assert_eq!(resp.marked_processed, vec![format!("bi_{bi}")]);
        assert_eq!(
            pending_count(&pool, &create.wiki_id).await,
            0,
            "briefing item must be flipped to processed by the same push"
        );
        let processed_at: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(processed_at.is_some(), "processed_at must be set");
        // New op_log row recorded for the push itself (the validation
        // + mark + insert all committed together).
        assert_eq!(
            op_log_count(&pool, &create.wiki_id).await,
            pre_oplog + 1,
            "exactly one new op_log row per successful push"
        );
    }

    #[tokio::test]
    async fn push_rejects_unknown_briefing_item_id_with_400_and_rolls_back() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let pre_oplog = op_log_count(&pool, &create.wiki_id).await;

        // Snapshot the index.md body so we can assert it was not
        // overwritten by the failed push.
        let handle = tree.locate(&create.wiki_id).expect("locate");
        let pre_body = handle.read_page(Path::new("index.md")).expect("read");

        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(
                &create.wiki_id,
                vec!["bi_99999".into()], // not present
            ),
        )
        .await
        .expect_err("unknown bi_id must abort");

        match err {
            AdminError::UnknownBriefingItemId { bi_id, wiki_id } => {
                assert_eq!(bi_id, "bi_99999");
                assert_eq!(wiki_id, create.wiki_id);
            },
            other => panic!("expected UnknownBriefingItemId, got {other:?}"),
        }

        assert_eq!(
            op_log_count(&pool, &create.wiki_id).await,
            pre_oplog,
            "no new op_log row allowed when mark_processed validation failed"
        );
        let post_body = handle.read_page(Path::new("index.md")).expect("read");
        assert_eq!(
            pre_body, post_body,
            "index.md must NOT have been overwritten by the failed push"
        );
    }

    #[tokio::test]
    async fn push_rejects_briefing_item_id_belonging_to_other_wiki_with_400_and_rolls_back() {
        let (_dir, tree, pool) = seeded_tree().await;
        let lnprint = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create lnprint");
        let mut other_req = create_smart_wiki_request("voxhobbit");
        // Distinct page set so the wiki id differs but everything else
        // is structurally the same.
        other_req.pages = vec![page("index.md", "# voxhobbit\n")];
        let voxhobbit = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            other_req,
        )
        .await
        .expect("create voxhobbit");

        // Briefing item seeded against lnprint, then we try to mark it
        // from a push targeting voxhobbit. The cross-wiki check inside
        // `validate_and_mark_processed` must refuse.
        let bi = seed_briefing_item(&pool, &lnprint.wiki_id, "belongs to lnprint").await;
        let pre_oplog = op_log_count(&pool, &voxhobbit.wiki_id).await;

        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(&voxhobbit.wiki_id, vec![format!("bi_{bi}")]),
        )
        .await
        .expect_err("cross-wiki bi_id must abort");
        match err {
            AdminError::UnknownBriefingItemId { wiki_id, bi_id } => {
                assert_eq!(wiki_id, voxhobbit.wiki_id);
                assert_eq!(bi_id, format!("bi_{bi}"));
            },
            other => panic!("expected UnknownBriefingItemId, got {other:?}"),
        }
        assert_eq!(
            op_log_count(&pool, &voxhobbit.wiki_id).await,
            pre_oplog,
            "no op_log row on voxhobbit"
        );
        // And the briefing item on lnprint is still pending.
        let processed_at: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed_at.is_none(),
            "lnprint briefing item must stay pending"
        );
    }

    #[tokio::test]
    async fn push_with_empty_mark_processed_behaves_like_push_without_field() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");

        // Push with empty Vec.
        let resp_empty = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(&create.wiki_id, Vec::new()),
        )
        .await
        .expect("push with empty marks");
        assert!(resp_empty.marked_processed.is_empty());

        // The struct already defaults to an empty Vec for the no-field
        // path; the wire shape is "field absent or empty array". Both
        // must produce identical side effects.
        let req_no_field = PushRequest {
            mode: PushMode::Upsert,
            wiki_id: Some(create.wiki_id.clone()),
            parent_wiki_id: None,
            slug: None,
            title: None,
            wiki_type: None,
            smart: false,
            project_id: None,
            pages: vec![page("index.md", "# updated2\n")],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let resp_omitted = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            req_no_field,
        )
        .await
        .expect("push without field");
        assert!(resp_omitted.marked_processed.is_empty());
    }

    #[tokio::test]
    async fn push_with_mark_processed_accepts_bi_prefix_and_bare_integer() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let bi_a = seed_briefing_item(&pool, &create.wiki_id, "a").await;
        let bi_b = seed_briefing_item(&pool, &create.wiki_id, "b").await;

        // Mix one canonical form and one bare integer.
        let resp = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(
                &create.wiki_id,
                vec![format!("bi_{bi_a}"), format!("{bi_b}")],
            ),
        )
        .await
        .expect("push with mixed forms");
        // Result is sorted ascending by id and rendered in canonical
        // `bi_<N>` form regardless of input shape.
        let mut expected = vec![format!("bi_{bi_a}"), format!("bi_{bi_b}")];
        expected.sort();
        assert_eq!(resp.marked_processed, expected);
        assert_eq!(pending_count(&pool, &create.wiki_id).await, 0);
    }

    #[tokio::test]
    async fn push_rejects_more_than_cap_briefing_items() {
        let (_dir, tree, pool) = seeded_tree().await;
        let create = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            create_smart_wiki_request("lnprint"),
        )
        .await
        .expect("create");
        let pre_oplog = op_log_count(&pool, &create.wiki_id).await;

        let too_many: Vec<String> = (1..=(MARK_PROCESSED_CAP_PER_PUSH + 1))
            .map(|n| format!("bi_{n}"))
            .collect();
        let err = push(
            &pool,
            &tree,
            &alice_smart(),
            ActorKind::SmartConsumer,
            upsert_with_marks(&create.wiki_id, too_many),
        )
        .await
        .expect_err("over the cap must abort");
        match err {
            AdminError::TooManyBriefingItems { received, cap } => {
                assert_eq!(received, MARK_PROCESSED_CAP_PER_PUSH + 1);
                assert_eq!(cap, MARK_PROCESSED_CAP_PER_PUSH);
            },
            other => panic!("expected TooManyBriefingItems, got {other:?}"),
        }
        assert_eq!(
            op_log_count(&pool, &create.wiki_id).await,
            pre_oplog,
            "no op_log row on cap violation"
        );
    }
}
