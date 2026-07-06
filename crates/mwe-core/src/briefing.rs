// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smart-wiki briefing inbox (`wiki_admin_notify` core).
//!
//! `_briefing.md` is a markdown file at the root of every
//! smart-wiki. REM (Briefing dispatcher + Backlink reciprocity
//! detector sub-jobs), other consumers (openclaw, hermes,
//! etc. via this tool), and the dashboard append items to it; the
//! smart consumer reads them at `smart_bootstrap` time and rotates
//! the file to `_briefing.archive.md` after triage. See
//! [`modello-memoria.md §9`] for the lifecycle.
//!
//! ## Authorisation
//!
//! Open to **any token with read access to the target wiki** — not
//! restricted to `consumer_class=smart`. Rationale (per
//! [`protocollo.md §H.3`]): an openclaw standard consumer must be
//! able to notify when the user, talking on Telegram, leaves an
//! observation that the smart consumer should pick up next session.
//!
//! The MVP defines "read access" as `caller.sender_id ==
//! resolved_owner_user`. Cross-user notify (`shared_with` members)
//! lands later alongside the share UI.
//!
//! ## Rate limit
//!
//! `50 notify/wiki/hour` per the spec. Enforced via a single
//! `SELECT COUNT(*)` over [`wiki_briefing_items`] in the trailing
//! hour. Counts toward the cap even if the resulting markdown
//! append fails (so a buggy caller cannot bypass the limit with
//! retries against transient filesystem errors).
//!
//! ## `_briefing.md` file shape
//!
//! On first notify the file is created with a tiny YAML frontmatter
//! plus a banner; subsequent calls append a `## <id> — <topic>`
//! section. The exact byte format is captured in [`render_section`],
//! kept stable so the smart consumer's parser at
//! `smart_bootstrap` time can rely on the heading shape.
//!
//! [`modello-memoria.md §9`]: ../../../docs/concepts/memory-model.md
//! [`protocollo.md §H.3`]: ../../../docs/protocol/tool-reference.md

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::types::WikiId;
use crate::wiki::{WikiError, WikiHandle, WikiTree, atomic_write};

/// Filename used for the briefing inbox at the wiki root.
pub const BRIEFING_FILENAME: &str = "_briefing.md";

/// Rate-limit cap per `tool-reference.md §H.3` (50 notify per wiki
/// per hour). Exposed as a const so the dashboard rate-card can
/// surface the same number without re-encoding it.
pub const NOTIFY_RATE_PER_HOUR: i64 = 50;

/// Errors surfaced by the public [`notify`] entry point. Mapped 1-to-1
/// onto the `ToolErrorClass` variants at the MCP boundary.
#[derive(Debug, Error)]
pub enum BriefingError {
    /// Target wiki not found.
    #[error("wiki {0} not found")]
    NotFound(WikiId),
    /// Target wiki's `wiki_type` is not in the smart family.
    /// Maps to `400 wiki_type_not_briefing_capable` — `_briefing.md`
    /// only exists for smart-wikis (see
    /// [`tool-reference.md §H.3`]).
    ///
    /// Preserved for the REM-internal path (`notify_as_rem`). The
    /// public MCP `wiki_admin_notify` surface uses the
    /// matrix-aware variants below instead.
    #[error(
        "wiki_type {wiki_type:?} is not in the smart family (only smart-wikis have a _briefing.md)"
    )]
    WikiTypeNotBriefingCapable {
        /// The non-smart `wiki_type` that was targeted.
        wiki_type: String,
    },
    /// A smart consumer tried to notify a smart-wiki it administers
    /// itself. Matrix cell `smart consumer × smart wiki`: the smart
    /// consumer should administer the briefing directly via
    /// `wiki_admin_push`, not notify itself. Maps to `403
    /// smart_does_not_notify_own_wiki`.
    #[error(
        "smart consumer cannot notify its own smart wiki ({wiki_type:?}): use wiki_admin_push instead"
    )]
    SmartDoesNotNotifyOwnWiki {
        /// The smart-family `wiki_type` that was targeted.
        wiki_type: String,
    },
    /// A standard consumer tried to notify a standard (non-smart)
    /// wiki. Matrix cell `standard consumer × standard wiki`: the canonical
    /// channel for standard consumers on standard wikis is
    /// `wiki_ingest_message` (workhorse 4-intent classifier). Maps to
    /// `403 standard_uses_ingest_for_memory`.
    #[error(
        "standard consumer must use wiki_ingest_message for standard wikis ({wiki_type:?}), not wiki_admin_notify"
    )]
    StandardUsesIngestForMemory {
        /// The standard-family `wiki_type` that was targeted.
        wiki_type: String,
    },
    /// Generic superset error (`400
    /// consumer_class_wiki_family_mismatch`) raised by the
    /// gate when an unexpected combination is detected — only used as
    /// fallback for branches that callers should never hit (forward-
    /// compatibility, in case a future `ConsumerClass` variant lands
    /// without an explicit matrix row).
    #[error("consumer class / wiki family mismatch ({detail})")]
    ConsumerClassWikiFamilyMismatch {
        /// Human-readable description of which combination was rejected.
        detail: String,
    },
    /// Caller does not have read access to the target wiki. The
    /// MVP enforces "caller is the `owner_user`"; cross-user notify
    /// via `shared_with` lands later.
    #[error("wiki {wiki_id} is owned by user:{owner}; cross-user notify is deferred")]
    ReadAccessDenied {
        /// Target wiki id.
        wiki_id: WikiId,
        /// The wiki's owning user id — the scope principal derived from its
        /// path to the root identity wiki.
        owner: String,
        /// User id derived from `token.sender_id`.
        caller_owner: String,
    },
    /// Wiki's derived scope principal is something other than a single
    /// user (Global or Group). MVP smart-wikis require a user owner.
    #[error("wiki {wiki_id} has owner {acl_default:?}; smart-wikis require a user owner")]
    AmbiguousOwner {
        /// Target wiki id.
        wiki_id: WikiId,
        /// The derived scope principal that wasn't a single user.
        acl_default: String,
    },
    /// `50 notify/wiki/h` cap exceeded.
    #[error("rate limit exceeded: {0} notify/wiki/h cap")]
    RateLimited(i64),
    /// Generic input validation (topic / body too long, unknown
    /// `source.kind`, etc.).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Bubbling from the wiki module.
    #[error("wiki: {0}")]
    Wiki(#[from] WikiError),
    /// Database failure.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    /// Raw filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Where the briefing item came from. Mirrors the spec wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingSourceKind {
    /// The end user, typically forwarded by a standard consumer that
    /// transcribed a Telegram / voice / email message.
    User,
    /// REM nightly cycle (Briefing dispatcher / Backlink reciprocity
    /// detector sub-jobs).
    Rem,
    /// Another consumer of the same user (smart or standard) that
    /// observed something worth flagging.
    Consumer,
    /// The dashboard UI — operator typed the item by hand.
    Dashboard,
}

impl BriefingSourceKind {
    /// Wire string used in the DB row and the on-disk markdown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Rem => "rem",
            Self::Consumer => "consumer",
            Self::Dashboard => "dashboard",
        }
    }
}

/// Authenticated caller for [`notify`]. Built by the MCP layer from
/// `IdentityProfile`; explicit fields keep the unit tests independent
/// from the server crate.
#[derive(Debug, Clone)]
pub struct NotifyCaller {
    /// `token.sender_id` (bare user id — `"alice"`, not `"user:alice"`).
    pub sender_id: String,
    /// `token.consumer_class` (smart vs standard). The
    /// `wiki_admin_notify` gate matrix routes the call to either the
    /// full smart-wiki path (DB + `_briefing.md`) or the standard-wiki
    /// DB-only path, and refuses two diagonal combinations (smart on
    /// own smart wiki, standard on standard wiki). The previous smart-wiki-
    /// only HARD gate is superseded by the per-cell matrix.
    pub consumer_class: crate::jwt::ConsumerClass,
}

/// Inputs of one [`notify`] call. Mirrors `wiki_admin_notify` (see
/// `tool-reference.md §H.3`).
#[derive(Debug, Clone)]
pub struct NotifyRequest {
    /// Target smart-wiki.
    pub wiki_id: WikiId,
    /// Short topic line (≤ 200 chars). Whitespace-trimmed on entry.
    pub topic: String,
    /// Long markdown body (≤ 4 KB). Whitespace-trimmed on entry.
    pub body: String,
    /// Origin of the notify (`source.kind`).
    pub source_kind: BriefingSourceKind,
    /// Free-form reference (`source.ref`) — `user:frodo`,
    /// `rem:briefing_dispatcher`, `cc-laptop`, etc.
    pub source_ref: String,
    /// Optional semantic tag (`observation` | `reasoning` |
    /// `external`, per the three-layer classification).
    /// `None` defaults to `"observation"` in the response and the
    /// markdown — kept distinct from `Some("observation")` so the
    /// future filter can tell apart explicit annotations from
    /// defaults.
    pub kind: Option<String>,
    /// Optional stable handle to a wiki section the briefing item is
    /// talking about. Format:
    /// `wiki://<wiki_id>/<page_path>(#<heading-slug>)?`. Validated
    /// server-side via [`parse_cite`] — a malformed string surfaces
    /// as [`BriefingError::InvalidInput`] so an off-by-one client
    /// gets a loud error instead of a silently-broken link. When
    /// `Some(_)`, the rendered `_briefing.md` section gains an inline
    /// `→ <cite>` line so the smart consumer at next bootstrap can
    /// open the exact wiki region the item points at.
    pub target_cite: Option<String>,
    /// Optional caller-provided timestamp (ISO 8601). `None` = now.
    pub ts: Option<String>,
}

/// Maximum bytes accepted on `topic`.
pub const TOPIC_MAX_BYTES: usize = 200;
/// Maximum bytes accepted on `body`.
pub const BODY_MAX_BYTES: usize = 4096;
/// Maximum bytes accepted on `target_cite`.
///
/// The wire format (`wiki://<wiki_id>/<page_path>#<heading-slug>`)
/// has hard upper bounds on each segment but together they can
/// grow — the cap shields the DB column + the markdown render from
/// a runaway value.
pub const CITE_MAX_BYTES: usize = 512;

/// Wire prefix of citation handles.
///
/// Exposed as a `pub const` so the dashboard `/cite/` endpoint
/// and the smart consumer's bootstrap can compose URLs
/// without re-encoding the magic string.
pub const CITE_SCHEME_PREFIX: &str = "wiki://";

/// Parsed shape of a [`CITE_SCHEME_PREFIX`] handle, returned by
/// [`parse_cite`].
///
/// Used by the dashboard `/cite/` endpoint to route to the
/// page + anchor, and exposed so other callers can avoid re-parsing
/// the string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCite {
    /// `<wiki_id>` segment — already a `WikiId` because we validated
    /// against the same charset rules `_meta.md` enforces.
    pub wiki_id: WikiId,
    /// `<page_path>` segment, e.g. `modules/auth.md` or `_briefing.md`.
    /// Always non-empty when [`parse_cite`] returns `Ok`.
    pub path: String,
    /// `#<heading-slug>` fragment, if present.
    pub anchor: Option<String>,
}

/// A heading + slug + line number found in a markdown body by
/// [`extract_anchors_from_markdown`].
///
/// Used by the future `wiki_admin_push` integration that
/// will persist a `wiki_anchors` table for the dashboard `/cite/`
/// endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingAnchor {
    /// 1-based line number of the heading in the source body.
    pub line_number: usize,
    /// Heading depth (`#` count): 1..=6.
    pub level: u8,
    /// Raw heading text after the leading `#`s and one space, before
    /// any trailing `#` decoration or whitespace.
    pub heading_text: String,
    /// Deterministic slug derived via [`slug_from_heading`].
    pub anchor: String,
}

/// Render a markdown heading text as the slug used inside citation
/// handles (`#<heading-slug>`). Pure function — same input always
/// returns the same output.
///
/// Parse the canonical `bi_<N>` briefing-item identifier (the form
/// returned by [`notify`] and by [`list_items`]). The `bi_` prefix is
/// optional so a hand-typed integer still resolves — both
/// `wiki_admin_push.mark_processed` and the
/// `/cite/:bi_id` resolver accept either shape.
///
/// Returns `None` for any other input: empty string, negative or
/// zero values, trailing whitespace, suffix garbage. Primary keys are
/// always positive integers, so `0` and `-N` are refused as malformed
/// rather than treated as not-found by an `id = 0` query that could
/// silently match nothing on most schemas.
///
/// This helper lives in [`crate::briefing`] (and not next to the
/// dashboard's `/cite/` route where it originally landed) so
/// every caller speaking the public briefing-item identifier — the
/// MCP `wiki_admin_push` handler, the dashboard cite resolver, future
/// hook bundles — shares the exact same shape rules.
#[must_use]
pub fn parse_bi_id(raw: &str) -> Option<i64> {
    let body = raw.strip_prefix("bi_").unwrap_or(raw);
    let n: i64 = body.parse().ok()?;
    if n < 1 { None } else { Some(n) }
}

/// Rules (mirror [the tool reference](../../../docs/protocol/tool-reference.md)):
///
/// - lowercase ASCII;
/// - any contiguous run of non-`[a-z0-9]` characters collapses to a
///   single `-`;
/// - leading + trailing `-` stripped.
///
/// Empty result is possible when the heading is whitespace / emoji /
/// purely non-ASCII; callers should treat `""` as "no anchor here".
#[must_use]
pub fn slug_from_heading(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_dash = true; // synthesise a leading dash, trimmed below
    for ch in text.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Walk `body` and return one [`HeadingAnchor`] per markdown
/// `# … ###### ` line.
///
/// Inline-only scan — fenced code blocks are **not** filtered out
/// yet (acceptable for MVP; the future `wiki_admin_push` integration
/// will harden this).
///
/// Duplicate slugs are returned verbatim; the dashboard / persistence
/// layer is responsible for disambiguating with `-2`, `-3` suffixes
/// when it writes the `wiki_anchors` table.
#[must_use]
pub fn extract_anchors_from_markdown(body: &str) -> Vec<HeadingAnchor> {
    let mut out = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let hash_count = trimmed.chars().take_while(|c| *c == '#').count();
        if hash_count == 0 || hash_count > 6 {
            continue;
        }
        let rest = &trimmed[hash_count..];
        if !rest.starts_with(' ') {
            continue;
        }
        let heading = rest
            .trim_start()
            .trim_end_matches(|c: char| c == '#' || c.is_whitespace())
            .trim()
            .to_owned();
        if heading.is_empty() {
            continue;
        }
        let anchor = slug_from_heading(&heading);
        if anchor.is_empty() {
            continue;
        }
        out.push(HeadingAnchor {
            line_number: idx + 1,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "hash_count is bounded to 6 above"
            )]
            level: hash_count as u8,
            heading_text: heading,
            anchor,
        });
    }
    out
}

/// Compose a citation handle from its parts. Returns
/// [`BriefingError::InvalidInput`] when any segment fails the same
/// validation [`parse_cite`] applies on the reverse path.
///
/// # Errors
///
/// - empty `path`;
/// - `path` containing a literal `#` (it would collide with the
///   fragment separator);
/// - non-empty `anchor` that fails the slug charset (`[a-z0-9-]+`,
///   no leading / trailing dash).
pub fn compose_cite(
    wiki_id: &WikiId,
    path: &str,
    anchor: Option<&str>,
) -> Result<String, BriefingError> {
    let trimmed_path = path.trim();
    if trimmed_path.is_empty() {
        return Err(BriefingError::InvalidInput(
            "target_cite path must not be blank".into(),
        ));
    }
    if trimmed_path.contains('#') {
        return Err(BriefingError::InvalidInput(format!(
            "target_cite path {trimmed_path:?} must not contain `#` (use the anchor argument)"
        )));
    }
    let mut out = String::with_capacity(
        CITE_SCHEME_PREFIX.len() + wiki_id.as_str().len() + trimmed_path.len() + 1,
    );
    out.push_str(CITE_SCHEME_PREFIX);
    out.push_str(wiki_id.as_str());
    if !trimmed_path.starts_with('/') {
        out.push('/');
    }
    out.push_str(trimmed_path);
    if let Some(a) = anchor {
        let trimmed_anchor = a.trim();
        if !trimmed_anchor.is_empty() {
            validate_anchor(trimmed_anchor)?;
            out.push('#');
            out.push_str(trimmed_anchor);
        }
    }
    if out.len() > CITE_MAX_BYTES {
        return Err(BriefingError::InvalidInput(format!(
            "composed target_cite exceeds {CITE_MAX_BYTES} bytes"
        )));
    }
    Ok(out)
}

/// Parse a citation handle into [`ParsedCite`]. Used by the dashboard
/// `/cite/` endpoint and by [`notify_append`] for the
/// server-side validation of [`NotifyRequest::target_cite`].
///
/// # Errors
///
/// - missing [`CITE_SCHEME_PREFIX`];
/// - missing / empty `wiki_id` segment;
/// - missing / empty `path` segment;
/// - `wiki_id` that fails [`WikiId::parse`];
/// - `anchor` that fails the slug charset.
pub fn parse_cite(s: &str) -> Result<ParsedCite, BriefingError> {
    if s.len() > CITE_MAX_BYTES {
        return Err(BriefingError::InvalidInput(format!(
            "target_cite exceeds {CITE_MAX_BYTES} bytes"
        )));
    }
    let Some(rest) = s.strip_prefix(CITE_SCHEME_PREFIX) else {
        return Err(BriefingError::InvalidInput(format!(
            "target_cite {s:?} must start with {CITE_SCHEME_PREFIX:?}"
        )));
    };
    let (wiki_segment, path_with_anchor) = rest.split_once('/').ok_or_else(|| {
        BriefingError::InvalidInput(format!(
            "target_cite {s:?} must include a `/` between wiki_id and path"
        ))
    })?;
    if wiki_segment.is_empty() {
        return Err(BriefingError::InvalidInput(
            "target_cite wiki_id must not be blank".into(),
        ));
    }
    let wiki_id = WikiId::parse(wiki_segment).map_err(|e| {
        BriefingError::InvalidInput(format!("target_cite wiki_id {wiki_segment:?} invalid: {e}"))
    })?;
    let (path, anchor) = match path_with_anchor.split_once('#') {
        Some((p, a)) => (p, Some(a)),
        None => (path_with_anchor, None),
    };
    if path.is_empty() {
        return Err(BriefingError::InvalidInput(
            "target_cite path must not be blank".into(),
        ));
    }
    let anchor = match anchor {
        Some("") | None => None,
        Some(a) => {
            validate_anchor(a)?;
            Some(a.to_owned())
        },
    };
    Ok(ParsedCite {
        wiki_id,
        path: path.to_owned(),
        anchor,
    })
}

/// Validate the fragment after `#`: lowercase alphanum + `-`, no
/// leading / trailing dash. The same rules [`slug_from_heading`]
/// produces, so a slug round-trips cleanly.
fn validate_anchor(anchor: &str) -> Result<(), BriefingError> {
    if anchor.is_empty() {
        return Err(BriefingError::InvalidInput(
            "target_cite anchor must not be blank".into(),
        ));
    }
    if anchor.starts_with('-') || anchor.ends_with('-') {
        return Err(BriefingError::InvalidInput(format!(
            "target_cite anchor {anchor:?} cannot start or end with `-`"
        )));
    }
    for c in anchor.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !ok {
            return Err(BriefingError::InvalidInput(format!(
                "target_cite anchor {anchor:?} contains invalid character {c:?} (expected [a-z0-9-])"
            )));
        }
    }
    Ok(())
}

/// Return value of [`notify`].
#[derive(Debug, Clone)]
pub struct NotifyResponse {
    /// `bi_<id>` opaque identifier matching the spec.
    pub briefing_item_id: String,
    /// Effective timestamp written (caller-provided or stamped now).
    pub ts: String,
}

/// Three-layer semantic classification per [the memory model](../../../docs/concepts/memory-model.md).
///
/// `wiki_admin_notify` defaults to `Observation` when the caller omits
/// the field; REM sub-jobs stamp it explicitly. Used by
/// [`list_items`] and [`counts_by_kind`] to project the inbox
/// per layer for the smart consumer's triage flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingKind {
    /// A fact observed by REM or another agent during a session —
    /// "this is what happened / this is what the data says". The
    /// implicit default when the caller omits `kind`.
    Observation,
    /// A decision the briefing item is asking the smart consumer to
    /// make, with the relevant context attached — "we should decide
    /// whether to X".
    Reasoning,
    /// An appointment left by the end user or by a non-owner consumer
    /// reading the wiki via `shared_with` — "frodo said on Telegram:
    /// fix MFA recovery codes". Includes manual references the user
    /// dropped in passing (commit ids, URLs, tickets).
    External,
}

impl BriefingKind {
    /// Wire string used in the DB column and the on-disk `[<kind>]`
    /// section label. Mirrors the JSON enum on the MCP boundary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Reasoning => "reasoning",
            Self::External => "external",
        }
    }

    /// Parse the wire string. Returns `None` on unknown values so the
    /// loader can keep going on a single corrupt row. Not implementing
    /// `std::str::FromStr` deliberately — that trait's `Err` type
    /// makes "unknown string → silently `None`" awkward for callers.
    #[must_use]
    pub fn parse_wire(s: &str) -> Option<Self> {
        match s {
            "observation" => Some(Self::Observation),
            "reasoning" => Some(Self::Reasoning),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// One row of [`wiki_briefing_items`] projected for callers.
///
/// Used by the dashboard list, `smart_bootstrap` triage, and REM
/// sweep checks. Mirrors the DB schema 1-to-1 with the typed `kind`
/// resolved to [`BriefingKind`] via [`BriefingKind::parse_wire`]; an
/// unknown wire value lands as `None`.
#[derive(Debug, Clone)]
pub struct BriefingItem {
    /// `bi_<id>` opaque identifier (same shape as [`NotifyResponse`]).
    pub briefing_item_id: String,
    /// Target smart-wiki id.
    pub wiki_id: WikiId,
    /// Origin of the row.
    pub source_kind: BriefingSourceKind,
    /// Free-form attribution reference (`user:frodo`,
    /// `rem:briefing_dispatcher`, …).
    pub source_ref: String,
    /// Trimmed topic line.
    pub topic: String,
    /// Trimmed markdown body.
    pub body: String,
    /// Three-layer classification. `None` when the DB row stored NULL
    /// (legacy rows from before the three-layer classification landed)
    /// or when the column value is not recognised.
    pub kind: Option<BriefingKind>,
    /// Stable handle to a specific section of the smart wiki's
    /// wiki this item refers to, or `None` for items that comment on
    /// the wiki as a whole. Already validated against
    /// [`CITE_SCHEME_PREFIX`] when persisted.
    pub target_cite: Option<String>,
    /// ISO-8601 timestamp the row was written with.
    pub ts: String,
    /// ISO-8601 timestamp the smart consumer drained the row at, or
    /// `None` when the item is still pending triage.
    pub processed_at: Option<String>,
}

/// Per-kind counts returned by [`counts_by_kind`].
///
/// `pending_*` cells count items where `processed_at IS NULL` — what
/// the smart consumer's `smart_bootstrap` surfaces at session start;
/// `total_*` cells include the already-drained items the dashboard
/// shows on the per-wiki briefing tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BriefingKindCounts {
    /// Pending [`BriefingKind::Observation`] items.
    pub pending_observation: i64,
    /// Pending [`BriefingKind::Reasoning`] items.
    pub pending_reasoning: i64,
    /// Pending [`BriefingKind::External`] items.
    pub pending_external: i64,
    /// Pending items with NULL / unrecognised `kind` (legacy rows).
    pub pending_unclassified: i64,
    /// All rows, including drained ones — useful for the dashboard
    /// "X items archived" badge.
    pub total: i64,
}

/// Append a briefing item to `_briefing.md` and mirror it in
/// [`wiki_briefing_items`]. See the module docstring for the auth
/// model + rate limit.
///
/// # Errors
///
/// One of [`BriefingError`].
pub async fn notify(
    pool: &SqlitePool,
    tree: &WikiTree,
    caller: &NotifyCaller,
    req: NotifyRequest,
) -> Result<NotifyResponse, BriefingError> {
    let (handle, outcome) = gate_notify_target_matrix(tree, &req.wiki_id, caller.consumer_class)?;
    // Owner always passes; otherwise resolve_read_access
    // checks the `shared_with` roster (direct user → SharedUser,
    // group via enrollment::groups_for → SharedGroup, Global →
    // Global). Denial surfaces the canonical 403 with the resolved
    // owner for diagnostics.
    let access = crate::wiki_admin::resolve_read_access(pool, tree, &handle, &caller.sender_id)
        .await
        .map_err(map_admin_error)?;
    if let crate::wiki_admin::ReadAccessOutcome::Denied { owner } = access {
        return Err(BriefingError::ReadAccessDenied {
            wiki_id: req.wiki_id.clone(),
            owner,
            caller_owner: caller.sender_id.clone(),
        });
    }
    let write_file = matches!(outcome, NotifyOutcome::FullCompanion);
    notify_append(pool, &handle, req, write_file).await
}

/// Bridge a [`crate::wiki_admin::AdminError`] from the shared ACL
/// resolver into the briefing module's own error space, keeping every
/// notify-side caller speaking [`BriefingError`].
fn map_admin_error(err: crate::wiki_admin::AdminError) -> BriefingError {
    use crate::wiki_admin::AdminError;
    match err {
        AdminError::AmbiguousOwner {
            wiki_id,
            acl_default,
        } => BriefingError::AmbiguousOwner {
            wiki_id,
            acl_default,
        },
        AdminError::Wiki(w) => BriefingError::Wiki(w),
        AdminError::Db(db) => BriefingError::Db(db),
        other => BriefingError::InvalidInput(format!("read access resolution: {other}")),
    }
}

/// REM-internal entry point used by the Briefing dispatcher and
/// Backlink reciprocity detector sub-jobs.
///
/// Same validation, smart-family gate, rate limit, DB row, and
/// `_briefing.md` append as [`notify`] — but **bypasses the caller ACL
/// check**: REM is a server-internal actor with no user identity, and
/// the notify lands in the wiki owner's own briefing inbox.
/// `source_kind` is forced to [`BriefingSourceKind::Rem`] regardless of
/// the value carried by `req`.
///
/// # Errors
///
/// One of [`BriefingError`], minus [`BriefingError::ReadAccessDenied`]
/// (which cannot fire on this path).
pub async fn notify_as_rem(
    pool: &SqlitePool,
    tree: &WikiTree,
    mut req: NotifyRequest,
) -> Result<NotifyResponse, BriefingError> {
    req.source_kind = BriefingSourceKind::Rem;
    let handle = locate_smart_wiki_target(tree, &req.wiki_id)?;
    notify_append(pool, &handle, req, true).await
}

/// Filter applied by [`list_items`] when projecting
/// [`wiki_briefing_items`] for callers.
#[derive(Debug, Clone, Default)]
pub struct ListItemsFilter {
    /// When `Some(k)`, only items whose stored `kind` matches `k.as_str()`
    /// are returned. `Some(BriefingKind::Observation)` does **not**
    /// surface NULL rows — the smart consumer that wants legacy
    /// rows must pass `None` and inspect `item.kind` itself.
    pub kind: Option<BriefingKind>,
    /// `Some(true)` returns only pending items (`processed_at IS NULL`).
    /// `Some(false)` returns only drained ones. `None` returns both,
    /// useful for the dashboard "history" tab.
    pub pending_only: Option<bool>,
    /// Maximum rows returned. `None` falls back to
    /// [`LIST_ITEMS_DEFAULT_LIMIT`] so a chatty consumer cannot pull
    /// the whole inbox by accident. Hard-capped to
    /// [`LIST_ITEMS_MAX_LIMIT`] regardless of caller input.
    pub limit: Option<i64>,
}

/// Default `limit` for [`list_items`] when the caller leaves
/// [`ListItemsFilter::limit`] unset. Matches the size of one
/// "session briefing" surfaced at `smart_bootstrap` time.
pub const LIST_ITEMS_DEFAULT_LIMIT: i64 = 50;
/// Hard ceiling on [`ListItemsFilter::limit`]. Beyond this the
/// dashboard / triage UI should paginate.
pub const LIST_ITEMS_MAX_LIMIT: i64 = 200;

/// List briefing items for a smart-wiki, projected as
/// [`BriefingItem`] and filtered by [`ListItemsFilter`].
///
/// Ordered by `ts DESC` so the freshest item is first — matches the
/// triage flow the smart consumer follows at `smart_bootstrap`. Rows
/// with an unknown `kind` value land with `item.kind == None` rather
/// than being dropped silently.
///
/// # Errors
///
/// Underlying `sqlx` failure when the cache table is unreachable.
pub async fn list_items(
    pool: &SqlitePool,
    wiki_id: &WikiId,
    filter: &ListItemsFilter,
) -> Result<Vec<BriefingItem>, BriefingError> {
    let limit = filter
        .limit
        .unwrap_or(LIST_ITEMS_DEFAULT_LIMIT)
        .clamp(1, LIST_ITEMS_MAX_LIMIT);

    // We compose the SQL imperatively so the JIT can still plan a
    // covering index scan when both `kind` and `pending_only` are set —
    // there is no `?`-free way to do the optional `kind = ?` filter
    // without a dynamic fragment.
    let mut sql = String::from(
        "SELECT id, wiki_id, source_kind, source_ref, topic, body, kind, target_cite, ts, processed_at \
         FROM wiki_briefing_items WHERE wiki_id = ?",
    );
    if filter.kind.is_some() {
        sql.push_str(" AND kind = ?");
    }
    match filter.pending_only {
        Some(true) => sql.push_str(" AND processed_at IS NULL"),
        Some(false) => sql.push_str(" AND processed_at IS NOT NULL"),
        None => {},
    }
    sql.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");

    let mut q = sqlx::query_as::<_, BriefingItemRow>(&sql).bind(wiki_id.as_str());
    if let Some(k) = filter.kind {
        q = q.bind(k.as_str());
    }
    q = q.bind(limit);
    let rows = q.fetch_all(pool).await?;
    Ok(rows.into_iter().map(BriefingItem::from_row).collect())
}

/// Bucket the wiki's briefing items by [`BriefingKind`] in a single
/// SQL round-trip.
///
/// Used by the dashboard's per-wiki briefing tab to surface
/// the four "X to triage" badges without fetching the bodies, and by
/// `smart_bootstrap` to know up-front whether the session has anything
/// to surface.
///
/// # Errors
///
/// Underlying `sqlx` failure when the cache table is unreachable.
pub async fn counts_by_kind(
    pool: &SqlitePool,
    wiki_id: &WikiId,
) -> Result<BriefingKindCounts, BriefingError> {
    let row: KindCountsRow = sqlx::query_as(
        "SELECT \
            SUM(CASE WHEN processed_at IS NULL AND kind = 'observation' THEN 1 ELSE 0 END) \
                AS pending_observation, \
            SUM(CASE WHEN processed_at IS NULL AND kind = 'reasoning'   THEN 1 ELSE 0 END) \
                AS pending_reasoning, \
            SUM(CASE WHEN processed_at IS NULL AND kind = 'external'    THEN 1 ELSE 0 END) \
                AS pending_external, \
            SUM(CASE WHEN processed_at IS NULL AND (kind IS NULL OR kind NOT IN ('observation','reasoning','external')) \
                     THEN 1 ELSE 0 END) AS pending_unclassified, \
            COUNT(*) AS total \
         FROM wiki_briefing_items WHERE wiki_id = ?",
    )
    .bind(wiki_id.as_str())
    .fetch_one(pool)
    .await?;
    Ok(BriefingKindCounts {
        pending_observation: row.pending_observation.unwrap_or(0),
        pending_reasoning: row.pending_reasoning.unwrap_or(0),
        pending_external: row.pending_external.unwrap_or(0),
        pending_unclassified: row.pending_unclassified.unwrap_or(0),
        total: row.total,
    })
}

/// Raw row shape used by [`list_items`] before we resolve the typed
/// [`BriefingKind`] and re-shape it as [`BriefingItem`].
#[derive(sqlx::FromRow)]
struct BriefingItemRow {
    id: i64,
    wiki_id: String,
    source_kind: String,
    source_ref: String,
    topic: String,
    body: String,
    kind: Option<String>,
    target_cite: Option<String>,
    ts: String,
    processed_at: Option<String>,
}

impl BriefingItem {
    fn from_row(row: BriefingItemRow) -> Self {
        // Every row in `wiki_briefing_items` was written through
        // `notify_append`, which serialises a validated `WikiId` —
        // the parse cannot fail except on DB corruption. We expect on
        // failure so the caller sees a loud error rather than silent
        // truncation.
        let wiki_id = WikiId::parse(&row.wiki_id)
            .expect("wiki_briefing_items.wiki_id is always a validated WikiId");
        Self {
            briefing_item_id: format!("bi_{}", row.id),
            wiki_id,
            source_kind: match row.source_kind.as_str() {
                "rem" => BriefingSourceKind::Rem,
                "consumer" => BriefingSourceKind::Consumer,
                "dashboard" => BriefingSourceKind::Dashboard,
                _ => BriefingSourceKind::User,
            },
            source_ref: row.source_ref,
            topic: row.topic,
            body: row.body,
            kind: row.kind.as_deref().and_then(BriefingKind::parse_wire),
            target_cite: row.target_cite,
            ts: row.ts,
            processed_at: row.processed_at,
        }
    }
}

/// Aggregate row for [`counts_by_kind`].
#[derive(sqlx::FromRow)]
struct KindCountsRow {
    pending_observation: Option<i64>,
    pending_reasoning: Option<i64>,
    pending_external: Option<i64>,
    pending_unclassified: Option<i64>,
    total: i64,
}

/// Resolve the target wiki handle and enforce the smart-wiki-only gate.
///
/// Used by the REM-internal path ([`notify_as_rem`]): REM only writes
/// to smart-wikis (per the Briefing dispatcher + Backlink
/// reciprocity sub-jobs), so the simple smart-wiki-only check stays.
///
/// The public MCP path goes through [`gate_notify_target_matrix`]
/// instead — see [`tool-reference.md §H.3`] for the consumer-class ×
/// wiki-family matrix.
fn locate_smart_wiki_target(
    tree: &WikiTree,
    wiki_id: &WikiId,
) -> Result<WikiHandle, BriefingError> {
    let handle = tree
        .locate(wiki_id)
        .map_err(|_| BriefingError::NotFound(wiki_id.clone()))?;
    // Smart-wiki-only gate, read per-wiki from the `_meta` smart flag (the
    // `wiki_types_registry` describe is retired).
    if !handle.meta().smart {
        return Err(BriefingError::WikiTypeNotBriefingCapable {
            wiki_type: handle.meta().wiki_type.clone(),
        });
    }
    Ok(handle)
}

/// Outcome of the `consumer_class × wiki_family` matrix gate
/// applied to the public MCP `wiki_admin_notify` path. Determines
/// whether [`notify_append`] writes to both the DB and `_briefing.md`
/// or to the DB only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyOutcome {
    /// `standard consumer × smart wiki` (status quo): append a row to
    /// [`wiki_briefing_items`] **and** the rendered section to
    /// `_briefing.md` (created on demand). The smart consumer rotates
    /// the file at `smart_bootstrap`.
    FullCompanion,
    /// `smart consumer × standard wiki`: append a row to [`wiki_briefing_items`]
    /// only. `_briefing.md` is **not** written — standard wikis don't
    /// own one, and the REM Briefing-processor sub-job 10
    /// drains the row in the next cycle.
    NarrativeDbOnly,
}

/// Resolve the target wiki handle and apply the
/// `consumer_class × wiki_family` matrix. Used by the public
/// [`notify`] entry point (MCP `wiki_admin_notify`).
fn gate_notify_target_matrix(
    tree: &WikiTree,
    wiki_id: &WikiId,
    consumer_class: crate::jwt::ConsumerClass,
) -> Result<(WikiHandle, NotifyOutcome), BriefingError> {
    let handle = tree
        .locate(wiki_id)
        .map_err(|_| BriefingError::NotFound(wiki_id.clone()))?;
    let wiki_type_label = handle.meta().wiki_type.clone();
    // The family read per-wiki from the `_meta` smart flag (the
    // `wiki_types_registry` describe is retired).
    let outcome = match (consumer_class, handle.meta().smart) {
        // smart consumer × smart wiki: should administer via
        // `wiki_admin_push`, not notify itself.
        (crate::jwt::ConsumerClass::Smart, true) => {
            return Err(BriefingError::SmartDoesNotNotifyOwnWiki {
                wiki_type: wiki_type_label,
            });
        },
        // smart consumer × standard wiki: append to the DB queue; REM sub-job 10
        // drains it at the next cycle.
        (crate::jwt::ConsumerClass::Smart, false) => NotifyOutcome::NarrativeDbOnly,
        // standard consumer × smart wiki: classic openclaw-style relay onto the
        // smart consumer's briefing inbox.
        (crate::jwt::ConsumerClass::Standard, true) => NotifyOutcome::FullCompanion,
        // standard consumer × standard wiki: canonical channel is `wiki_ingest_message`.
        (crate::jwt::ConsumerClass::Standard, false) => {
            return Err(BriefingError::StandardUsesIngestForMemory {
                wiki_type: wiki_type_label,
            });
        },
    };
    Ok((handle, outcome))
}

/// Validate input, enforce the per-hour rate limit, insert the DB row,
/// and (when `write_briefing_file` is `true`) append the rendered
/// section to `_briefing.md`. The shared core for [`notify`] and
/// [`notify_as_rem`] — both surfaces share the same DB shape and the
/// same `50/wiki/h` cap (per [`tool-reference.md §H.3`]).
///
/// `write_briefing_file = false` is the `smart consumer × standard wiki`
/// path (no `_briefing.md` exists for standard wikis; REM sub-job 10
/// drains the row). `write_briefing_file = true` is the historical
/// `standard consumer × smart wiki` shape (and the REM-internal smart-wiki path).
#[allow(
    clippy::too_many_lines,
    reason = "validation + rate-limit + DB insert + optional file append are all preconditions for one logical append; splitting hides the per-step order"
)]
async fn notify_append(
    pool: &SqlitePool,
    handle: &WikiHandle,
    req: NotifyRequest,
    write_briefing_file: bool,
) -> Result<NotifyResponse, BriefingError> {
    let topic = req.topic.trim().to_owned();
    if topic.is_empty() {
        return Err(BriefingError::InvalidInput(
            "topic must not be blank".into(),
        ));
    }
    if topic.len() > TOPIC_MAX_BYTES {
        return Err(BriefingError::InvalidInput(format!(
            "topic exceeds {TOPIC_MAX_BYTES} bytes"
        )));
    }
    let body = req.body.trim().to_owned();
    if body.is_empty() {
        return Err(BriefingError::InvalidInput("body must not be blank".into()));
    }
    if body.len() > BODY_MAX_BYTES {
        return Err(BriefingError::InvalidInput(format!(
            "body exceeds {BODY_MAX_BYTES} bytes"
        )));
    }
    if req.source_ref.trim().is_empty() {
        return Err(BriefingError::InvalidInput(
            "source.ref must not be blank".into(),
        ));
    }
    if let Some(k) = req.kind.as_deref()
        && !matches!(k, "observation" | "reasoning" | "external")
    {
        return Err(BriefingError::InvalidInput(format!(
            "unknown kind {k:?} (allowed: observation | reasoning | external)"
        )));
    }
    // target_cite must round-trip through `parse_cite` if
    // present — a malformed value gets rejected up-front instead of
    // landing in the DB to break the dashboard `/cite/` resolver
    // later. The trimmed string (or None) is what we persist.
    let target_cite: Option<String> = match req.target_cite.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let trimmed = raw.trim();
            parse_cite(trimmed)?;
            Some(trimmed.to_owned())
        },
        _ => None,
    };

    let wiki_id = &req.wiki_id;
    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_briefing_items
           WHERE wiki_id = ?
             AND ts > datetime('now', '-1 hour')",
    )
    .bind(wiki_id.as_str())
    .fetch_one(pool)
    .await?;
    if recent >= NOTIFY_RATE_PER_HOUR {
        return Err(BriefingError::RateLimited(NOTIFY_RATE_PER_HOUR));
    }

    let ts = req
        .ts
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| chrono::Utc::now().to_rfc3339(), str::to_owned);

    let row: (i64,) = sqlx::query_as(
        "INSERT INTO wiki_briefing_items
            (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, processed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)
         RETURNING id",
    )
    .bind(wiki_id.as_str())
    .bind(req.source_kind.as_str())
    .bind(req.source_ref.trim())
    .bind(&topic)
    .bind(&body)
    .bind(req.kind.as_deref())
    .bind(&ts)
    .bind(target_cite.as_deref())
    .fetch_one(pool)
    .await?;
    let briefing_item_id = format!("bi_{}", row.0);

    if write_briefing_file {
        let abs = briefing_file_path(handle);
        let existing = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => initial_briefing_doc(wiki_id),
            Err(e) => return Err(BriefingError::Io(e)),
        };
        let section = render_section(
            &briefing_item_id,
            &topic,
            &body,
            req.source_kind,
            req.source_ref.trim(),
            req.kind.as_deref(),
            target_cite.as_deref(),
            &ts,
        );
        let updated = touch_last_updated(&existing, &ts);
        let next = format!("{updated}{section}");
        atomic_write(&abs, next.as_bytes())?;
    }

    Ok(NotifyResponse {
        briefing_item_id,
        ts,
    })
}

/// Absolute path of `_briefing.md` inside the wiki directory.
fn briefing_file_path(handle: &WikiHandle) -> PathBuf {
    handle.abs_dir().join(BRIEFING_FILENAME)
}

/// Build the document body written when `_briefing.md` does not
/// exist yet. The frontmatter shape mirrors the spec sketch in
/// [`modello-memoria.md §9`].
fn initial_briefing_doc(wiki_id: &WikiId) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        "---\n\
         type: session_briefing\n\
         wiki_id: {wiki}\n\
         last_updated: {now}\n\
         ---\n\
         \n\
         # Session briefing for `{wiki}`\n\
         \n\
         REM (Briefing dispatcher + Backlink reciprocity detector), other\n\
         consumers, and the dashboard append items below. The smart\n\
         consumer reads them at `smart_bootstrap` and rotates this file\n\
         to `_briefing.archive.md` after triage.\n",
        wiki = wiki_id.as_str(),
        now = now,
    )
}

/// Update the `last_updated:` field in the frontmatter (if any) so the
/// dashboard "X items pending" badge stays in sync without a full
/// re-read of the file. Tolerates the corner case where an operator
/// edited the frontmatter by hand and stripped the field — in that
/// case we leave the doc as-is.
fn touch_last_updated(existing: &str, ts: &str) -> String {
    use std::fmt::Write as _;
    let Some((fm_start, fm_end)) = locate_frontmatter(existing) else {
        return existing.to_owned();
    };
    let fm = &existing[fm_start..fm_end];
    let mut new_fm = String::with_capacity(fm.len() + ts.len());
    let mut wrote_line = false;
    for line in fm.lines() {
        if line.starts_with("last_updated:") && !wrote_line {
            let _ = write!(new_fm, "last_updated: {ts}");
            new_fm.push('\n');
            wrote_line = true;
        } else {
            new_fm.push_str(line);
            new_fm.push('\n');
        }
    }
    let mut out = String::with_capacity(existing.len() + ts.len());
    out.push_str(&existing[..fm_start]);
    out.push_str(&new_fm);
    out.push_str(&existing[fm_end..]);
    out
}

/// Locate the byte range of the frontmatter (between the two `---`
/// fences, the inner content). Returns `None` when there is no
/// frontmatter at all.
fn locate_frontmatter(s: &str) -> Option<(usize, usize)> {
    let rest = s.strip_prefix("---\n")?;
    let body_offset = s.len() - rest.len();
    let close = rest.find("\n---")?;
    Some((body_offset, body_offset + close + 1))
}

#[allow(clippy::too_many_arguments)]
fn render_section(
    item_id: &str,
    topic: &str,
    body: &str,
    source_kind: BriefingSourceKind,
    source_ref: &str,
    kind: Option<&str>,
    target_cite: Option<&str>,
    ts: &str,
) -> String {
    let kind_label = kind.unwrap_or("observation");
    let mut out = String::with_capacity(topic.len() + body.len() + 256);
    out.push_str("\n## ");
    out.push_str(item_id);
    out.push_str(" — ");
    out.push_str(topic);
    out.push_str(" [");
    out.push_str(kind_label);
    out.push_str("]\n");
    out.push_str("\n*from ");
    out.push_str(source_kind.as_str());
    out.push_str(" (ref: `");
    out.push_str(source_ref);
    out.push_str("`, at ");
    out.push_str(ts);
    out.push_str(")*\n");
    if let Some(cite) = target_cite {
        // Stable handle inline so the smart consumer at next
        // bootstrap can open the exact wiki region. Rendered as a
        // markdown autolink so Obsidian preserves the clickable form
        // and a plain-text reader still sees the URL.
        out.push_str("\n*→ <");
        out.push_str(cite);
        out.push_str(">*\n");
    }
    out.push('\n');
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n---\n");
    out
}

// Tests share the rate-limit / write path with the canonical
// `_briefing.md` shape and the inherited auth gates.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{IdentityKind, create_identity_wiki};
    use crate::wiki_admin::{ActorKind, AdminCaller, PushMode, PushPage, PushRequest, push};
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

    async fn seeded_tree_with_smart_wiki() -> (tempfile::TempDir, WikiTree, SqlitePool, WikiId) {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let alice = WikiId::parse("alice").unwrap();
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).unwrap();
        let smart = AdminCaller {
            sender_id: "alice".into(),
            consumer_id: Some("cc-laptop".into()),
            consumer_class: crate::jwt::ConsumerClass::Smart,
        };
        let req = PushRequest {
            mode: PushMode::Create,
            wiki_id: None,
            parent_wiki_id: Some(alice.clone()),
            slug: Some("lnprint".into()),
            title: Some("lnprint companion".into()),
            wiki_type: Some("wiki-companion".into()),
            smart: true,
            project_id: None,
            pages: vec![PushPage {
                path: "index.md".into(),
                content: "# lnprint\n".into(),
            }],
            deletes: Vec::new(),
            mark_processed: Vec::new(),
            expected_op_log_head: None,
        };
        let resp = push(&pool, &tree, &smart, ActorKind::SmartConsumer, req)
            .await
            .expect("create");
        (dir, tree, pool, resp.wiki_id)
    }

    fn alice_caller() -> NotifyCaller {
        // Default helper: standard consumer relaying onto a smart-wiki-
        // wiki (the canonical use case for this surface — openclaw
        // notifying the user's smart consumer, per the module docstring).
        NotifyCaller {
            sender_id: "alice".into(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        }
    }

    fn sample_request(wiki_id: &WikiId) -> NotifyRequest {
        NotifyRequest {
            wiki_id: wiki_id.clone(),
            topic: "Recovery codes for MFA".into(),
            body: "Document recovery codes in `modules/auth.md`.".into(),
            source_kind: BriefingSourceKind::User,
            source_ref: "user:alice".into(),
            kind: None,
            target_cite: None,
            ts: None,
        }
    }

    #[tokio::test]
    async fn notify_creates_briefing_file_and_inserts_row() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let resp = notify(&pool, &tree, &alice_caller(), sample_request(&wiki_id))
            .await
            .expect("notify");
        assert!(resp.briefing_item_id.starts_with("bi_"));
        assert!(!resp.ts.is_empty());

        // DB row reflects the input.
        let row: (String, String, String, String, Option<String>) = sqlx::query_as(
            "SELECT source_kind, source_ref, topic, body, kind
               FROM wiki_briefing_items WHERE wiki_id = ?",
        )
        .bind(wiki_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("row");
        assert_eq!(row.0, "user");
        assert_eq!(row.1, "user:alice");
        assert_eq!(row.2, "Recovery codes for MFA");
        assert!(row.3.contains("modules/auth.md"));
        assert!(row.4.is_none(), "kind defaults to NULL when not provided");

        // _briefing.md materialised on disk.
        let handle = tree.locate(&wiki_id).unwrap();
        let body = std::fs::read_to_string(handle.abs_dir().join(BRIEFING_FILENAME))
            .expect("briefing file present");
        assert!(body.contains("type: session_briefing"));
        assert!(body.contains("Recovery codes for MFA"));
        assert!(body.contains("[observation]"), "default kind label");
        assert!(body.contains("`user:alice`"));
        assert!(body.contains(&resp.briefing_item_id));
    }

    #[tokio::test]
    async fn notify_appends_subsequent_items_under_same_frontmatter() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let _first = notify(&pool, &tree, &alice_caller(), sample_request(&wiki_id))
            .await
            .expect("first");
        let mut second = sample_request(&wiki_id);
        second.topic = "Second item".into();
        second.body = "follow-up".into();
        second.kind = Some("reasoning".into());
        let _second = notify(&pool, &tree, &alice_caller(), second)
            .await
            .expect("second");

        let handle = tree.locate(&wiki_id).unwrap();
        let body = std::fs::read_to_string(handle.abs_dir().join(BRIEFING_FILENAME))
            .expect("briefing file");
        // Single frontmatter at the top, both items below.
        let fence_count = body.match_indices("---\n").count();
        // 1 opening + 1 closing fence for frontmatter + 2 trailing "---" separators after items
        // (the items use `---\n` as their section separator).
        assert!(
            fence_count >= 3,
            "expected the frontmatter + at least 2 item separators, got {fence_count}"
        );
        assert!(body.contains("Recovery codes for MFA"));
        assert!(body.contains("Second item"));
        assert!(body.contains("[reasoning]"));
    }

    #[tokio::test]
    async fn notify_smart_on_own_smart_wiki_rejects() {
        // Matrix cell `smart consumer × smart wiki`: a smart consumer
        // administers its own smart wiki via `wiki_admin_push`, so
        // notifying itself is rejected with the dedicated 403.
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let smart_alice = NotifyCaller {
            sender_id: "alice".into(),
            consumer_class: crate::jwt::ConsumerClass::Smart,
        };
        let err = notify(&pool, &tree, &smart_alice, sample_request(&wiki_id))
            .await
            .expect_err("smart consumer × smart wiki must reject");
        assert!(matches!(
            err,
            BriefingError::SmartDoesNotNotifyOwnWiki { .. }
        ));
        // No row was inserted and no `_briefing.md` was created.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wiki_briefing_items")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn notify_smart_on_standard_appends_db_only_no_briefing_file() {
        // Matrix cell `smart consumer × standard wiki`: append a row to
        // `wiki_briefing_items` so REM sub-job 10 can drain it; do
        // **not** create `_briefing.md` (it does not belong on a
        // standard wiki).
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let alice = WikiId::parse("alice").unwrap();
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let smart_alice = NotifyCaller {
            sender_id: "alice".into(),
            consumer_class: crate::jwt::ConsumerClass::Smart,
        };
        let resp = notify(&pool, &tree, &smart_alice, sample_request(&alice))
            .await
            .expect("smart consumer × standard wiki must succeed db-only");
        assert!(resp.briefing_item_id.starts_with("bi_"));

        // DB row is there.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wiki_briefing_items")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1);

        // `_briefing.md` was NOT written.
        let handle = tree.locate(&alice).expect("locate");
        let briefing_file = handle.abs_dir().join(BRIEFING_FILENAME);
        assert!(
            !briefing_file.exists(),
            "smart consumer × standard wiki must not create _briefing.md"
        );
    }

    #[tokio::test]
    async fn notify_standard_on_standard_wiki_rejects_with_ingest_hint() {
        // Matrix cell `standard consumer × standard wiki`: a standard
        // consumer must use `wiki_ingest_message` for standard wikis;
        // the canonical reject is `403 standard_uses_ingest_for_memory`.
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let alice = WikiId::parse("alice").unwrap();
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).unwrap();
        // Target the identity wiki (wiki-user, not smart-family) directly.
        let req = NotifyRequest {
            wiki_id: alice,
            topic: "hi".into(),
            body: "x".into(),
            source_kind: BriefingSourceKind::User,
            source_ref: "user:alice".into(),
            kind: None,
            target_cite: None,
            ts: None,
        };
        let err = notify(&pool, &tree, &alice_caller(), req)
            .await
            .expect_err("standard consumer × standard wiki must reject");
        assert!(matches!(
            err,
            BriefingError::StandardUsesIngestForMemory { .. }
        ));
    }

    #[tokio::test]
    async fn notify_rejects_cross_user_access() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let bob = NotifyCaller {
            sender_id: "bob".into(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        };
        let err = notify(&pool, &tree, &bob, sample_request(&wiki_id))
            .await
            .expect_err("cross-user must reject without shared_with");
        assert!(matches!(err, BriefingError::ReadAccessDenied { .. }));
    }

    /// Inject `shared_with: [...]` into the smart wiki's
    /// `_meta.md` by rewriting it on disk. Mirrors the helper in
    /// `wiki_admin::tests`.
    fn inject_shared_with_in_briefing_tests(tree: &WikiTree, wiki_id: &WikiId, entries: &[&str]) {
        use std::fmt::Write as _;
        let handle = tree.locate(wiki_id).expect("locate smart wiki");
        let abs = handle.abs_dir().join(crate::wiki::META_FILENAME);
        let raw = std::fs::read_to_string(&abs).expect("read meta");
        let mut out = String::with_capacity(raw.len() + 128);
        for line in raw.lines() {
            out.push_str(line);
            out.push('\n');
            // Anchor on `title:` (always present); the `acl_default` line is
            // retired and the canonical order emits `shared_with` after title.
            if line.starts_with("title:") {
                out.push_str("shared_with:\n");
                for e in entries {
                    writeln!(out, "  - {e}").expect("write to string");
                }
            }
        }
        std::fs::write(&abs, out).expect("write meta");
    }

    #[tokio::test]
    async fn notify_allows_shared_with_user_to_post_cross_user_briefing_item() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        inject_shared_with_in_briefing_tests(&tree, &wiki_id, &["user:bob"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();

        let bob = NotifyCaller {
            sender_id: "bob".into(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        };
        let mut req = sample_request(&wiki_id);
        req.source_kind = BriefingSourceKind::Consumer;
        req.source_ref = "cc-bob-laptop".into();
        req.topic = "Heads-up from bob".into();
        let resp = notify(&pool, &tree, &bob, req)
            .await
            .expect("shared_with user must be allowed");
        assert!(resp.briefing_item_id.starts_with("bi_"));

        let row: (String, String) = sqlx::query_as(
            "SELECT source_kind, source_ref FROM wiki_briefing_items WHERE wiki_id = ?",
        )
        .bind(wiki_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "consumer");
        assert_eq!(row.1, "cc-bob-laptop");
    }

    #[tokio::test]
    async fn notify_allows_shared_with_group_member_via_enrollment() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        sqlx::query("INSERT INTO enrollment_groups (group_id, members) VALUES (?, ?)")
            .bind("lnprint-devs")
            .bind(r#"["bob"]"#)
            .execute(&pool)
            .await
            .unwrap();
        inject_shared_with_in_briefing_tests(&tree, &wiki_id, &["group:lnprint-devs"]);
        let tree = WikiTree::open(tree.workdir()).unwrap();
        let bob = NotifyCaller {
            sender_id: "bob".into(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        };
        let resp = notify(&pool, &tree, &bob, sample_request(&wiki_id))
            .await
            .expect("group member must be allowed");
        assert!(resp.briefing_item_id.starts_with("bi_"));

        // Mallory is not in the group → still rejected.
        let mallory = NotifyCaller {
            sender_id: "mallory".into(),
            consumer_class: crate::jwt::ConsumerClass::Standard,
        };
        let err = notify(&pool, &tree, &mallory, sample_request(&wiki_id))
            .await
            .expect_err("non-member rejected");
        assert!(matches!(err, BriefingError::ReadAccessDenied { .. }));
    }

    #[tokio::test]
    async fn notify_rejects_blank_inputs() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let mut req = sample_request(&wiki_id);
        req.topic = "  ".into();
        let err = notify(&pool, &tree, &alice_caller(), req)
            .await
            .expect_err("blank topic must reject");
        assert!(matches!(err, BriefingError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn notify_as_rem_bypasses_acl_and_forces_source_kind() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        // Even with `source_kind: User` in the request, REM forces it.
        let mut req = sample_request(&wiki_id);
        req.source_kind = BriefingSourceKind::User;
        req.source_ref = "rem:briefing_dispatcher:stale_draft:fact_42".into();
        let resp = notify_as_rem(&pool, &tree, req)
            .await
            .expect("rem notify must bypass owner ACL");
        assert!(resp.briefing_item_id.starts_with("bi_"));

        let row: (String, String) = sqlx::query_as(
            "SELECT source_kind, source_ref FROM wiki_briefing_items WHERE wiki_id = ?",
        )
        .bind(wiki_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("row");
        assert_eq!(row.0, "rem", "source_kind is forced to `rem`");
        assert_eq!(row.1, "rem:briefing_dispatcher:stale_draft:fact_42");
    }

    #[tokio::test]
    async fn notify_as_rem_still_rejects_non_smart() {
        let dir = tempdir().unwrap();
        let tree = WikiTree::open(dir.path()).unwrap();
        let pool = make_pool().await;
        let alice = WikiId::parse("alice").unwrap();
        create_identity_wiki(&tree, &alice, "Alice", IdentityKind::User).unwrap();
        let req = NotifyRequest {
            wiki_id: alice,
            topic: "hi".into(),
            body: "x".into(),
            source_kind: BriefingSourceKind::User, // will be overridden to Rem
            source_ref: "rem:backlink_reciprocity:wiki:source".into(),
            kind: None,
            target_cite: None,
            ts: None,
        };
        let err = notify_as_rem(&pool, &tree, req)
            .await
            .expect_err("rem path still gates on the smart family");
        assert!(matches!(
            err,
            BriefingError::WikiTypeNotBriefingCapable { .. }
        ));
    }

    #[tokio::test]
    async fn list_items_returns_freshest_first_with_typed_kind() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let mut req1 = sample_request(&wiki_id);
        req1.topic = "First obs".into();
        req1.kind = Some("observation".into());
        let _ = notify(&pool, &tree, &alice_caller(), req1).await.unwrap();
        let mut req2 = sample_request(&wiki_id);
        req2.topic = "Second decision".into();
        req2.kind = Some("reasoning".into());
        // Pin both timestamps so the ordering assertion is
        // independent of wall-clock — `req1` keeps its auto-generated
        // `Utc::now()`, but a future call to this test on a different
        // day would otherwise drift req1 past req2's hard-coded value
        // and flip the ordering. Use a "far future" timestamp instead.
        req2.ts = Some("2099-01-01T00:00:00Z".into());
        let _ = notify(&pool, &tree, &alice_caller(), req2).await.unwrap();

        let items = list_items(&pool, &wiki_id, &ListItemsFilter::default())
            .await
            .expect("list");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].topic, "Second decision");
        assert_eq!(items[0].kind, Some(BriefingKind::Reasoning));
        assert_eq!(items[1].topic, "First obs");
        assert_eq!(items[1].kind, Some(BriefingKind::Observation));
    }

    #[tokio::test]
    async fn list_items_filter_by_kind_only_returns_matching_rows() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        for kind in ["observation", "reasoning", "external"] {
            let mut req = sample_request(&wiki_id);
            req.topic = format!("topic-{kind}");
            req.kind = Some(kind.into());
            let _ = notify(&pool, &tree, &alice_caller(), req).await.unwrap();
        }
        let filter = ListItemsFilter {
            kind: Some(BriefingKind::Reasoning),
            ..ListItemsFilter::default()
        };
        let items = list_items(&pool, &wiki_id, &filter).await.expect("list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].topic, "topic-reasoning");
        assert_eq!(items[0].kind, Some(BriefingKind::Reasoning));
    }

    #[tokio::test]
    async fn list_items_filter_pending_only_skips_drained_rows() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let r1 = notify(&pool, &tree, &alice_caller(), sample_request(&wiki_id))
            .await
            .unwrap();
        let mut req2 = sample_request(&wiki_id);
        req2.topic = "kept pending".into();
        let _ = notify(&pool, &tree, &alice_caller(), req2).await.unwrap();
        // Manually flip the first row's processed_at — mirrors the
        // smart_bootstrap drain path.
        let id_num: i64 = r1
            .briefing_item_id
            .trim_start_matches("bi_")
            .parse()
            .unwrap();
        sqlx::query("UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ?")
            .bind("2026-05-25T20:30:00Z")
            .bind(id_num)
            .execute(&pool)
            .await
            .unwrap();

        let pending = list_items(
            &pool,
            &wiki_id,
            &ListItemsFilter {
                pending_only: Some(true),
                ..ListItemsFilter::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].topic, "kept pending");
        assert!(pending[0].processed_at.is_none());

        let drained = list_items(
            &pool,
            &wiki_id,
            &ListItemsFilter {
                pending_only: Some(false),
                ..ListItemsFilter::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].briefing_item_id, r1.briefing_item_id);
        assert!(drained[0].processed_at.is_some());
    }

    #[tokio::test]
    async fn list_items_respects_limit_default_and_cap() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        // 3 rows + an explicit limit of 2 → exactly 2 newest items.
        for i in 0..3 {
            let mut req = sample_request(&wiki_id);
            req.topic = format!("row {i}");
            req.ts = Some(format!("2026-05-25T20:{:02}:00Z", 10 + i));
            let _ = notify(&pool, &tree, &alice_caller(), req).await.unwrap();
        }
        let items = list_items(
            &pool,
            &wiki_id,
            &ListItemsFilter {
                limit: Some(2),
                ..ListItemsFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].topic, "row 2");
        assert_eq!(items[1].topic, "row 1");

        // Limit beyond LIST_ITEMS_MAX_LIMIT is silently clamped.
        let bounded = list_items(
            &pool,
            &wiki_id,
            &ListItemsFilter {
                limit: Some(LIST_ITEMS_MAX_LIMIT + 50),
                ..ListItemsFilter::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(bounded.len(), 3);
    }

    #[tokio::test]
    async fn counts_by_kind_buckets_pending_and_total_separately() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        // 2 observation (1 drained, 1 pending), 1 reasoning (pending),
        // 1 external (pending), 1 NULL-kind via direct INSERT (legacy).
        let obs1 = notify(&pool, &tree, &alice_caller(), sample_request(&wiki_id))
            .await
            .unwrap();
        let mut req_obs2 = sample_request(&wiki_id);
        req_obs2.topic = "obs 2".into();
        req_obs2.kind = Some("observation".into());
        let _ = notify(&pool, &tree, &alice_caller(), req_obs2)
            .await
            .unwrap();
        let mut req_rs = sample_request(&wiki_id);
        req_rs.topic = "rs".into();
        req_rs.kind = Some("reasoning".into());
        let _ = notify(&pool, &tree, &alice_caller(), req_rs).await.unwrap();
        let mut req_ext = sample_request(&wiki_id);
        req_ext.topic = "ext".into();
        req_ext.kind = Some("external".into());
        let _ = notify(&pool, &tree, &alice_caller(), req_ext)
            .await
            .unwrap();
        // Legacy row with NULL kind, written directly.
        sqlx::query(
            "INSERT INTO wiki_briefing_items \
                (wiki_id, source_kind, source_ref, topic, body, kind, ts, processed_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, NULL)",
        )
        .bind(wiki_id.as_str())
        .bind("user")
        .bind("user:alice")
        .bind("legacy")
        .bind("legacy body")
        .bind("2026-05-25T19:00:00Z")
        .execute(&pool)
        .await
        .unwrap();

        // Drain obs1 to test pending != total.
        let obs1_id: i64 = obs1
            .briefing_item_id
            .trim_start_matches("bi_")
            .parse()
            .unwrap();
        sqlx::query("UPDATE wiki_briefing_items SET processed_at = ? WHERE id = ?")
            .bind("2026-05-25T20:00:00Z")
            .bind(obs1_id)
            .execute(&pool)
            .await
            .unwrap();

        let counts = counts_by_kind(&pool, &wiki_id).await.expect("counts");
        assert_eq!(counts.pending_observation, 1);
        assert_eq!(counts.pending_reasoning, 1);
        assert_eq!(counts.pending_external, 1);
        assert_eq!(counts.pending_unclassified, 1);
        assert_eq!(counts.total, 5);
    }

    // ---------- citation IDs ----------

    #[test]
    fn parse_bi_id_accepts_canonical_and_bare_forms() {
        assert_eq!(parse_bi_id("bi_42"), Some(42));
        assert_eq!(parse_bi_id("42"), Some(42));
        assert_eq!(parse_bi_id("bi_1"), Some(1));
        assert_eq!(parse_bi_id("9999999"), Some(9_999_999));
    }

    #[test]
    fn parse_bi_id_rejects_malformed_input() {
        assert!(parse_bi_id("").is_none());
        assert!(parse_bi_id("bi_").is_none());
        assert!(parse_bi_id("bi_abc").is_none());
        assert!(parse_bi_id("abc").is_none());
        assert!(parse_bi_id("bi_-1").is_none());
        assert!(parse_bi_id("-1").is_none());
        assert!(parse_bi_id("0").is_none());
        assert!(parse_bi_id("bi_0").is_none());
        assert!(parse_bi_id("bi_42a").is_none());
        assert!(parse_bi_id("42 ").is_none());
        assert!(parse_bi_id(" 42").is_none());
    }

    #[test]
    fn slug_from_heading_round_trips_typical_headings() {
        assert_eq!(slug_from_heading("MFA flow"), "mfa-flow");
        assert_eq!(
            slug_from_heading("# Recovery codes (Q3 2026)"),
            "recovery-codes-q3-2026"
        );
        assert_eq!(slug_from_heading("  whitespace   "), "whitespace");
        assert_eq!(slug_from_heading("---"), "");
        // Adjacent punctuation collapses to a single dash.
        assert_eq!(slug_from_heading("Foo  ::  Bar"), "foo-bar");
        // Non-ASCII drops out — the slug remains ASCII-only.
        assert_eq!(slug_from_heading("café résumé"), "caf-r-sum");
    }

    #[test]
    fn extract_anchors_walks_each_heading() {
        let md = "# Top heading\n\nintro\n\n## Sub one\n\nbody\n\n### Sub-sub (Q3)\n";
        let anchors = extract_anchors_from_markdown(md);
        assert_eq!(anchors.len(), 3);
        assert_eq!(anchors[0].level, 1);
        assert_eq!(anchors[0].anchor, "top-heading");
        assert_eq!(anchors[0].line_number, 1);
        assert_eq!(anchors[1].level, 2);
        assert_eq!(anchors[1].anchor, "sub-one");
        assert_eq!(anchors[2].level, 3);
        assert_eq!(anchors[2].anchor, "sub-sub-q3");
    }

    #[test]
    fn extract_anchors_ignores_non_headings_and_trailing_decoration() {
        let md = "Not a heading\n#NoSpaceAfter\n# Real heading #\n#######  too-deep\n";
        let anchors = extract_anchors_from_markdown(md);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].heading_text, "Real heading");
        assert_eq!(anchors[0].anchor, "real-heading");
    }

    #[test]
    fn parse_cite_round_trips_with_and_without_anchor() {
        let ok = parse_cite("wiki://alice-lnprint/modules/auth.md#mfa-flow").unwrap();
        assert_eq!(ok.wiki_id.as_str(), "alice-lnprint");
        assert_eq!(ok.path, "modules/auth.md");
        assert_eq!(ok.anchor.as_deref(), Some("mfa-flow"));

        let no_anchor = parse_cite("wiki://alice-lnprint/decisions/recovery.md").unwrap();
        assert!(no_anchor.anchor.is_none());
        assert_eq!(no_anchor.path, "decisions/recovery.md");
    }

    #[test]
    fn parse_cite_rejects_malformed_handles() {
        assert!(parse_cite("https://example.com").is_err(), "wrong scheme");
        assert!(parse_cite("wiki://").is_err(), "missing wiki_id + path");
        assert!(parse_cite("wiki://alice").is_err(), "missing `/`");
        assert!(parse_cite("wiki://alice/").is_err(), "empty path");
        assert!(parse_cite("wiki:///page.md").is_err(), "empty wiki_id");
        assert!(
            parse_cite("wiki://alice/page.md#BAD-Slug").is_err(),
            "anchor uppercase rejected"
        );
        assert!(
            parse_cite("wiki://alice/page.md#-bad").is_err(),
            "leading dash on anchor rejected"
        );
    }

    #[test]
    fn compose_cite_assembles_handle_consistently() {
        let wiki = WikiId::parse("alice-lnprint").unwrap();
        let cite = compose_cite(&wiki, "modules/auth.md", Some("mfa-flow")).unwrap();
        assert_eq!(cite, "wiki://alice-lnprint/modules/auth.md#mfa-flow");
        // Round-trips through parse_cite cleanly.
        let parsed = parse_cite(&cite).unwrap();
        assert_eq!(parsed.wiki_id, wiki);

        // Leading `/` on path is tolerated (we keep the canonical
        // single-slash form).
        let cite2 = compose_cite(&wiki, "/page.md", None).unwrap();
        assert_eq!(cite2, "wiki://alice-lnprint/page.md");

        assert!(compose_cite(&wiki, "", None).is_err());
        assert!(compose_cite(&wiki, "modules#auth.md", None).is_err());
        assert!(compose_cite(&wiki, "page.md", Some("BAD")).is_err());
    }

    #[tokio::test]
    async fn notify_persists_and_renders_target_cite_when_present() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let cite = format!("wiki://{}/modules/auth.md#mfa-flow", wiki_id.as_str());
        let mut req = sample_request(&wiki_id);
        req.target_cite = Some(cite.clone());
        let _ = notify(&pool, &tree, &alice_caller(), req).await.unwrap();

        // DB column reflects the trimmed value.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT target_cite FROM wiki_briefing_items WHERE wiki_id = ?")
                .bind(wiki_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some(cite.as_str()));

        // Markdown contains the inline link.
        let handle = tree.locate(&wiki_id).unwrap();
        let body = std::fs::read_to_string(handle.abs_dir().join(BRIEFING_FILENAME)).unwrap();
        assert!(body.contains(&format!("→ <{cite}>")));

        // BriefingItem projection exposes it typed.
        let items = list_items(&pool, &wiki_id, &ListItemsFilter::default())
            .await
            .unwrap();
        assert_eq!(items[0].target_cite.as_deref(), Some(cite.as_str()));
    }

    #[tokio::test]
    async fn notify_rejects_malformed_target_cite() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let mut req = sample_request(&wiki_id);
        req.target_cite = Some("not-a-cite".into());
        let err = notify(&pool, &tree, &alice_caller(), req)
            .await
            .expect_err("malformed cite must reject");
        assert!(matches!(err, BriefingError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn notify_accepts_blank_target_cite_as_absent() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        let mut req = sample_request(&wiki_id);
        req.target_cite = Some("   ".into());
        let _ = notify(&pool, &tree, &alice_caller(), req)
            .await
            .expect("blank cite treated as absent");
        let stored: Option<String> =
            sqlx::query_scalar("SELECT target_cite FROM wiki_briefing_items WHERE wiki_id = ?")
                .bind(wiki_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stored.is_none());
    }

    #[tokio::test]
    async fn notify_rate_limited_after_cap() {
        let (_dir, tree, pool, wiki_id) = seeded_tree_with_smart_wiki().await;
        // Direct INSERTs seed the cap exactly — calling notify 50
        // times would be slower and exercises the same SQL path.
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..NOTIFY_RATE_PER_HOUR {
            sqlx::query(
                "INSERT INTO wiki_briefing_items
                    (wiki_id, source_kind, source_ref, topic, body, kind, ts, processed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
            )
            .bind(wiki_id.as_str())
            .bind("rem")
            .bind("rem:test_seed")
            .bind(format!("seed {i}"))
            .bind("seed body")
            .bind(None::<&str>)
            .bind(&now)
            .execute(&pool)
            .await
            .expect("seed");
        }
        let err = notify(&pool, &tree, &alice_caller(), sample_request(&wiki_id))
            .await
            .expect_err("the 51st notify must be rate-limited");
        assert!(matches!(err, BriefingError::RateLimited(_)));
    }
}
