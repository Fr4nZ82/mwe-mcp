// SPDX-License-Identifier: AGPL-3.0-or-later
//! Identity validation and DB mirror — invoked by the dashboard CRUD
//! handlers (enrollment loader).
//!
//! The DDL for the two mirror tables (`enrollment_users` and
//! `enrollment_groups`) lives in `migrations/0006_enrollment.sql`.
//! Those tables are the **source of truth** for identity,
//! not a mirror of an on-disk YAML — but the in-memory
//! [`EnrollmentFile`] / [`UserEntry`] / [`GroupEntry`] shapes are
//! reused as a transport for dashboard form submissions and for the
//! one-shot "import existing enrollment.yaml" admin action (if it
//! ever ships).
//!
//! ## Flow
//!
//! 1. The dashboard handler assembles an [`EnrollmentFile`] from form
//!    input (or from an uploaded YAML in the admin import path).
//! 2. [`validate`] applies the rules of `schemi.md §4.4` (still
//!    authoritative for id regex, duplicates, group↔user collision,
//!    dangling members, filesystem-safe slugs) and returns either a
//!    bag of soft warnings or a hard [`EnrollmentError`].
//! 3. [`mirror_to_db`] atomically replaces both tables in a single
//!    [`sqlx::Transaction`] so read-only consumers (REM, ACL) never
//!    see a half-applied state.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

/// Bundle of users + groups handed to [`validate`] / [`mirror_to_db`].
/// Layout mirrors `schemi.md §4.1` so the same shape can be reused
/// for the future admin "import existing enrollment.yaml" action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentFile {
    /// Schema version. Day-1 only `1` is supported.
    pub version: u32,
    /// All known users. Required, can be empty (rare but legal).
    pub users: Vec<UserEntry>,
    /// All known groups. Optional — defaults to empty.
    #[serde(default)]
    pub groups: Vec<GroupEntry>,
}

/// A single user entry
/// ([identity and ACL §1.6](../../../docs/concepts/identity-and-acl.md)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEntry {
    /// Stable identifier. Must match `^[a-z][a-z0-9°]*$`.
    pub id: String,
    /// Alternative names accepted by `users_resolve` fuzzy match.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// UI gating hint for the built-in dashboard; **does not** bypass
    /// ACL ([identity and ACL](../../../docs/concepts/identity-and-acl.md)).
    #[serde(rename = "isAdmin", default)]
    pub is_admin: bool,
    /// Optional BCP-47 locale tag (`it-IT`, `en-US`, ...) used as the
    /// per-user default for the prompt `LANGUAGE` directive. When
    /// unset, the orchestrator falls back to the legacy "mirror the
    /// user's message" rule. `None` is the day-1 state until the
    /// admin or the welcome wizard populates it.
    #[serde(default)]
    pub locale: Option<String>,
}

/// A single group entry
/// ([identity and ACL §1.6](../../../docs/concepts/identity-and-acl.md)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupEntry {
    /// Stable identifier. Must match `^[a-z][a-z0-9]*$` (no `°` allowed
    /// for groups — that marker is reserved for user collision suffixes).
    pub id: String,
    /// User ids that belong to the group. Every entry must resolve to
    /// a user in the same file.
    pub members: Vec<String>,
    /// Free-form prose describing the scope of the group.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Errors that *abort* enrollment processing.
///
/// Soft warnings (alias collisions) are reported via
/// [`ValidationReport::warnings`] and never raised as errors.
#[derive(Debug, Error)]
pub enum EnrollmentError {
    /// `version:` field references a schema version we do not support.
    #[error("unsupported enrollment version: {0} (only 1 is supported)")]
    UnsupportedVersion(u32),
    /// A `users[].id` does not match the canonical regex.
    #[error("invalid user id {id:?}: must match /^[a-z][a-z0-9°]*$/")]
    InvalidUserId {
        /// The offending id.
        id: String,
    },
    /// A `groups[].id` does not match the canonical regex.
    #[error("invalid group id {id:?}: must match /^[a-z][a-z0-9]*$/")]
    InvalidGroupId {
        /// The offending id.
        id: String,
    },
    /// Two users share the same id.
    #[error("duplicate user id: {0}")]
    DuplicateUserId(String),
    /// Two groups share the same id.
    #[error("duplicate group id: {0}")]
    DuplicateGroupId(String),
    /// A group id collides with a user id (same `wiki/<id>` namespace).
    #[error("group id {0} collides with a user of the same id")]
    GroupCollidesWithUser(String),
    /// A group lists a member that does not exist in `users[]`.
    #[error("group {group}: member {member} is not a known user")]
    DanglingMember {
        /// Group whose member list is offending.
        group: String,
        /// Id referenced by the group but missing from `users[]`.
        member: String,
    },
    /// An id contains characters that are not filesystem-safe.
    #[error("unsafe slug in id {0:?}: contains '/', '..', or whitespace")]
    UnsafeSlug(String),
    /// A user or group tried to claim a builtin reserved id (`guest`).
    #[error(
        "id {0:?} is reserved: `guest` is the builtin unidentified-human \
         pseudo-identity and can never be enrolled"
    )]
    ReservedId(String),
    /// Underlying `sqlx` error while mirroring to the DB.
    #[error("enrollment db error: {0}")]
    Db(#[from] sqlx::Error),
    /// Failed to encode `aliases` / `members` to JSON before the
    /// `INSERT`. Practically unreachable for in-memory `Vec<String>`
    /// but kept explicit so the failure mode does not get silently
    /// turned into a panic via `unwrap`.
    #[error("enrollment json encode error: {0}")]
    JsonEncode(#[from] serde_json::Error),
}

/// Result of [`validate`]. Hard errors short-circuit; soft warnings
/// accumulate so the caller can log them and still proceed.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    /// Non-fatal observations (e.g. alias collisions between users).
    pub warnings: Vec<String>,
}

/// Validate an [`EnrollmentFile`] against the rules of
/// `schemi.md §4.4`. Returns the warning bag on success; the first
/// hard rule violation surfaces as an [`EnrollmentError`].
pub fn validate(file: &EnrollmentFile) -> Result<ValidationReport, EnrollmentError> {
    if file.version != 1 {
        return Err(EnrollmentError::UnsupportedVersion(file.version));
    }

    let mut report = ValidationReport::default();
    let mut user_ids = std::collections::HashSet::new();
    let mut alias_seen: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for user in &file.users {
        if !is_valid_user_id(&user.id) {
            return Err(EnrollmentError::InvalidUserId {
                id: user.id.clone(),
            });
        }
        if is_guest(&user.id) {
            return Err(EnrollmentError::ReservedId(user.id.clone()));
        }
        if !is_filesystem_safe(&user.id) {
            return Err(EnrollmentError::UnsafeSlug(user.id.clone()));
        }
        if !user_ids.insert(user.id.clone()) {
            return Err(EnrollmentError::DuplicateUserId(user.id.clone()));
        }
        // Alias collisions are soft — the alias still works, but the
        // operator should know the resolution may be ambiguous.
        for alias in &user.aliases {
            if let Some(prev_owner) = alias_seen.insert(alias.clone(), user.id.clone()) {
                report.warnings.push(format!(
                    "alias {alias:?} is shared by users {prev_owner} and {}",
                    user.id
                ));
            }
            if user_ids.contains(alias) && alias != &user.id {
                report.warnings.push(format!(
                    "alias {alias:?} of {} matches an existing user id",
                    user.id
                ));
            }
            if is_guest(alias) {
                report.warnings.push(format!(
                    "alias \"guest\" of {} shadows the builtin guest pseudo-identity in \
                     classifier attribution — consider a different alias",
                    user.id
                ));
            }
        }
    }

    let mut group_ids = std::collections::HashSet::new();
    for group in &file.groups {
        if !is_valid_group_id(&group.id) {
            return Err(EnrollmentError::InvalidGroupId {
                id: group.id.clone(),
            });
        }
        if is_guest(&group.id) {
            return Err(EnrollmentError::ReservedId(group.id.clone()));
        }
        if !is_filesystem_safe(&group.id) {
            return Err(EnrollmentError::UnsafeSlug(group.id.clone()));
        }
        if !group_ids.insert(group.id.clone()) {
            return Err(EnrollmentError::DuplicateGroupId(group.id.clone()));
        }
        if user_ids.contains(&group.id) {
            return Err(EnrollmentError::GroupCollidesWithUser(group.id.clone()));
        }
        for member in &group.members {
            if !user_ids.contains(member) {
                return Err(EnrollmentError::DanglingMember {
                    group: group.id.clone(),
                    member: member.clone(),
                });
            }
        }
    }

    Ok(report)
}

/// Canonical id of the builtin universal group.
///
/// Everyone is implicitly a member, so it is the public/world principal
/// (see [`crate::types::Principal`]). It is a normal `enrollment_groups`
/// row so the admin can edit its `scope`, but it is special-cased
/// throughout: never hand-membered or deleted, excluded from
/// [`list_groups`] (it seeds no `group_theme` hub), and always surfaced by
/// [`groups_with_scope_for`] regardless of membership.
pub const GLOBAL_GROUP_ID: &str = "global";

/// Default `scope` shipped for the builtin [`GLOBAL_GROUP_ID`] group.
///
/// The admin can override it from the dashboard. Frames `global` ownership
/// as WORLD facts, explicitly NOT as "a public personal fact" (that is the
/// `allow` visibility axis), so the classifier does not collapse public
/// profile facts onto `owner=global`.
pub const DEFAULT_GLOBAL_SCOPE: &str = "General, public-domain facts that are true for everyone and \
belong to no single user or group (e.g. common knowledge, weather, public events). NOT for making a \
personal fact public — that is visibility (put `global` in a fact's allow-list), not ownership.";

/// Whether `id` is the builtin universal group.
#[must_use]
pub fn is_global_group(id: &str) -> bool {
    id == GLOBAL_GROUP_ID
}

/// Canonical id of the builtin `guest` pseudo-identity — the
/// unidentified-human sender.
///
/// A consumer that meets a human it cannot identify (an unrecognized
/// voice on a satellite, an unknown sender in a group chat, a walk-up
/// kiosk user) acts as `guest` instead of falling back to a wrong real
/// identity. Unlike [`GLOBAL_GROUP_ID`], guest is **not** an enrollment
/// row: it has no wiki, no login, no aliases, and can never hold a
/// token ([`validate_token_identity`]). It exists only as an
/// **effective sender** reached via `X-MWE-Act-As`, gated by the same
/// per-consumer delegation roster as any real user — granting `guest`
/// to a consumer from the dashboard IS the enable switch, so the
/// feature is off until the admin delegates it.
///
/// Semantics downstream: guest turns are **ephemeral** (the ingest
/// orchestrator files nothing, [`crate::ingest`]), guest reads only the
/// public slice (the `global` arm of [`crate::acl::can_read`] is the
/// only principal it can ever match), and permanent-write tools reject
/// it. The id is reserved here in [`validate`] so no real user or group
/// can ever claim it.
pub const GUEST_USER_ID: &str = "guest";

/// Whether `id` is the builtin guest pseudo-identity.
#[must_use]
pub fn is_guest(id: &str) -> bool {
    id == GUEST_USER_ID
}

/// Re-seed the builtin [`GLOBAL_GROUP_ID`] row if absent.
///
/// Uses [`DEFAULT_GLOBAL_SCOPE`]. Idempotent (`INSERT OR IGNORE`), so it
/// never clobbers an admin-edited scope. Call it after any operation that
/// may have wiped the table (e.g. [`mirror_to_db`]); the initial seed also
/// ships as a migration.
///
/// # Errors
///
/// Propagates the underlying `sqlx` error.
pub async fn ensure_global_group(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO enrollment_groups (group_id, members, scope) VALUES (?, '[]', ?)",
    )
    .bind(GLOBAL_GROUP_ID)
    .bind(DEFAULT_GLOBAL_SCOPE)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return the ids of every group the given user is a member of,
/// in deterministic alphabetical order.
///
/// This is the bridge between the `enrollment_groups` table (members
/// stored as a JSON array of user ids) and `SenderContext::sender_groups`
/// (the bare list the ACL evaluator matches `Principal::Group(_)`
/// against). Every production construction site of `SenderContext` must
/// populate `sender_groups` via this helper; the convenience
/// constructor `SenderContext::user` leaves it empty and is therefore
/// only safe for tests and the bootstrap anonymous path.
///
/// An empty `user_id` short-circuits to an empty result so the
/// anonymous path stays a single, predictable round-trip.
pub async fn groups_for(pool: &SqlitePool, user_id: &str) -> Result<Vec<String>, sqlx::Error> {
    if user_id.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT group_id FROM enrollment_groups
         WHERE EXISTS (SELECT 1 FROM json_each(members) WHERE json_each.value = ?)
         ORDER BY group_id ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Return the member user-ids of `group_id`, in deterministic
/// alphabetical order.
///
/// The inverse of [`groups_for`] (member → groups): this resolves
/// group → members by reading the `enrollment_groups.members` JSON array.
/// The fact-forget audience ([`crate::acl::audience`]) derives the
/// **eligible voter set** from it — a group `owner`/`allow` expands to its
/// members. The builtin `global` group stores
/// an empty `members` array (everyone is implicitly in it, never enumerated),
/// so this returns empty for it; an unknown / empty `group_id`
/// short-circuits to empty, matching [`groups_for`].
///
/// # Errors
///
/// Propagates the underlying `sqlx` error.
pub async fn members_for(pool: &SqlitePool, group_id: &str) -> Result<Vec<String>, sqlx::Error> {
    if group_id.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT json_each.value FROM enrollment_groups, json_each(enrollment_groups.members)
         WHERE enrollment_groups.group_id = ?
         ORDER BY json_each.value ASC",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await
}

/// Return every group the given user belongs to together with the
/// operator-configured `scope` prose, ordered alphabetically by
/// `group_id`.
///
/// Sibling of [`groups_for`]: that helper returns only the bare ids the
/// ACL evaluator matches `Principal::Group(_)` against; this one also
/// carries the `scope` column so the `ingest` classifier can route a
/// capture into a group's shared memory when the fact falls inside that
/// group's declared domain (the classifier determines `owner_id` by
/// consulting the scope of the sender's groups). The `scope` is
/// operator-written free prose, `None` when the admin never configured one.
///
/// The `ingest` orchestrator calls this once and derives the bare-id
/// [`recall::SenderContext::sender_groups`] from each pair's `.0`, so the
/// hot ACL paths keep using the leaner [`groups_for`] scan while only the
/// prompt path pays for the extra column. An empty `user_id`
/// short-circuits to an empty result, matching [`groups_for`].
pub async fn groups_with_scope_for(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    if user_id.is_empty() {
        return Ok(Vec::new());
    }
    // The builtin `global` group is surfaced to EVERY sender regardless of
    // membership, so the classifier always sees its (operator-editable)
    // scope and routes genuine world facts to `owner=global`.
    sqlx::query_as(
        "SELECT group_id, scope FROM enrollment_groups
         WHERE group_id = 'global'
            OR EXISTS (SELECT 1 FROM json_each(members) WHERE json_each.value = ?)
         ORDER BY group_id ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// A known user reduced to what the ingest classifier needs.
///
/// Just the canonical `user_id` and the operator-declared `aliases` (other
/// names the person is referred to by) for cross-user attribution. See
/// [`list_users`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledUserLite {
    /// Canonical user id (the `Principal::User` payload).
    pub user_id: String,
    /// Alternate names/spellings the operator declared for this person.
    pub aliases: Vec<String>,
}

/// Enumerate every enrolled user (id + aliases), ordered by `user_id`.
///
/// The "known users" roster the classifier injects into its
/// prompt so it can attribute a fact to the right person by canonical name —
/// e.g. a message from Alice that says "Bob prefers tea" resolves `owner_id`
/// to `user:bob` rather than filing it under Alice. Sibling of
/// [`groups_with_scope_for`] (groups + scope) on the identity-injection side.
/// `aliases` is parsed from the
/// `enrollment_users.aliases` JSON column (a malformed value degrades to an
/// empty alias list rather than failing the lookup).
///
/// # Errors
///
/// Propagates the underlying `sqlx` error.
pub async fn list_users(pool: &SqlitePool) -> Result<Vec<EnrolledUserLite>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id, aliases FROM enrollment_users ORDER BY user_id ASC")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, aliases_json)| EnrolledUserLite {
            user_id,
            aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
        })
        .collect())
}

/// A group reduced to what the planner's Fonditore needs.
///
/// The `group_id` and the `scope` prose — enough to seed a `group_theme`
/// hub page for the group. See [`list_groups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledGroupLite {
    /// Canonical group id (the `Principal::Group` payload).
    pub group_id: String,
    /// Operator-written scope prose (what the group's shared memory is for).
    pub scope: Option<String>,
}

/// Enumerate every enrolled group (id + scope), ordered by id.
///
/// The planner's Fonditore seeds one `group_theme` hub page per
/// group from this. Sibling of [`list_users`] on the foundation side.
///
/// # Errors
///
/// Propagates the underlying `sqlx` error.
pub async fn list_groups(pool: &SqlitePool) -> Result<Vec<EnrolledGroupLite>, sqlx::Error> {
    // Exclude the builtin `global` group: it is the universal ACL
    // principal, not a collaborative group, so it seeds no `group_theme`
    // hub and is not a navigation entry point.
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT group_id, scope FROM enrollment_groups
          WHERE group_id != 'global'
          ORDER BY group_id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(group_id, scope)| EnrolledGroupLite { group_id, scope })
        .collect())
}

/// Look up the BCP-47 locale the admin configured for `user_id`.
///
/// Returns `Ok(None)` for an unknown user, an empty `user_id`, or a
/// row whose `locale` column is NULL — all three collapse to "no
/// explicit locale" semantics so the orchestrator falls back to the
/// legacy "mirror the user's message" behaviour. The lookup is
/// a single-statement scalar; callers do not need to cache the result
/// because the cost is dominated by the surrounding LLM call.
pub async fn locale_for(pool: &SqlitePool, user_id: &str) -> Result<Option<String>, sqlx::Error> {
    if user_id.is_empty() {
        return Ok(None);
    }
    let row: Option<Option<String>> =
        sqlx::query_scalar("SELECT locale FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    // `Option<Option<T>>` collapses to `Option<T>`: outer None when the
    // user row is absent, inner None when the row exists but `locale`
    // is NULL. Both surface as `None` to the caller.
    Ok(row.flatten().filter(|s| !s.trim().is_empty()))
}

/// True when `user_id` is a **system user**.
///
/// A system user is enrolled but has no `user_credentials` account (so it
/// cannot log into the dashboard) — the credential-less bot identity a
/// *standard* consumer authenticates as in the diagonal identity model.
///
/// # Errors
/// - [`sqlx::Error`] for any SQL failure.
pub async fn is_system_user(pool: &SqlitePool, user_id: &str) -> Result<bool, sqlx::Error> {
    let has_credentials: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_credentials WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(has_credentials == 0)
}

/// Enforce the **diagonal identity model** at token issuance.
///
/// One policy shared by the `mwe-mcp token-issue` CLI and the dashboard
/// token form, so the two can never drift. The connection pattern is a
/// function of `consumer_class`, not a free choice (see
/// `docs/concepts/identity-and-acl.md` §1):
///
/// - **`standard`** ⇒ Pattern B. `sender` must be a credential-less
///   **system user** (the bot's own identity) and a `consumer_id` must be
///   present. This is the security-critical guard: issuing a standard token
///   for a human *with a login account* would let a multi-user bot
///   read/write as that one human → a cross-user recall/ACL leak.
/// - **`smart`** ⇒ Pattern A. The sender is the human owner; this side is a
///   convention, not a hard gate. A smart token is mono-user (no act-as), so
///   it is harmless regardless of sender, and "is this a bot or a
///   not-yet-onboarded human" is not reliably distinguishable by credentials
///   alone (both lack a `user_credentials` row). A future explicit
///   `is_system` marker would let us gate the smart side too.
///
/// A "system user" is an `enrollment_users` row with no `user_credentials`
/// account. `sender` is assumed to already be a known enrollment user.
///
/// # Errors
/// - `Err(String)` describing the policy violation (suitable for a CLI
///   `bail!` or a dashboard error banner), or surfacing a SQL failure.
pub async fn validate_token_identity(
    pool: &SqlitePool,
    sender: &str,
    consumer_class: crate::jwt::ConsumerClass,
    has_consumer_id: bool,
) -> Result<(), String> {
    // The builtin guest pseudo-identity never holds a token of any
    // class: it is only ever an *effective* sender reached via
    // `X-MWE-Act-As`, gated by the per-consumer delegation roster.
    if is_guest(sender) {
        return Err(
            "`guest` is the builtin unidentified-human pseudo-identity and cannot hold a \
             token; a consumer reaches it per turn via X-MWE-Act-As once the admin delegates \
             `guest` to it"
                .to_owned(),
        );
    }
    // Only the standard (multi-user bot) side is security-critical and gated.
    if consumer_class.is_standard() {
        if !is_system_user(pool, sender)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err(format!(
                "standard consumer token must bind a system user (a credential-less bot \
                 identity), but {sender:?} is a human account with a login. Create a dedicated \
                 bot identity and issue the token for it; the bot reaches real users via \
                 X-MWE-Act-As (diagonal identity model — docs/concepts/identity-and-acl.md §1)"
            ));
        }
        if !has_consumer_id {
            return Err(
                "standard consumer token requires a consumer_id (the bot's registered \
                 deployment id, e.g. samvise-prod): a standard consumer is always a registered \
                 system user, never an anonymous Pattern-A token"
                    .to_owned(),
            );
        }
        // Mutual-exclusion, agent side: a credential-less identity that is
        // already marked `is_agent` is fine (it IS a bot). The credential side
        // is barred separately in [`reject_if_agent`]. Nothing more to check
        // here — the system-user check above is the standard-side guard.
    }
    Ok(())
}

/// True when `user_id` carries the explicit `is_agent` marker — a consumer
/// agent's own system-user identity (migration 0050).
///
/// The authoritative discriminator the diagonal-identity model deferred
/// (`docs/concepts/identity-and-acl.md` §1.5): it distinguishes a bot identity
/// from a not-yet-onboarded human (both lack `user_credentials`), and is mutually
/// exclusive with a login account. Set the moment a standard consumer token
/// establishes its binding ([`crate::consumers::ensure_agent_identity`]).
///
/// # Errors
/// - [`sqlx::Error`] for any SQL failure.
pub async fn is_agent(pool: &SqlitePool, user_id: &str) -> Result<bool, sqlx::Error> {
    let flag: Option<i64> =
        sqlx::query_scalar("SELECT is_agent FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(flag.unwrap_or(0) != 0)
}

/// True when `user_id` is the operator/admin — the single privileged identity
/// (`is_admin = 1`, guarded by a partial unique index, so there is at most one).
///
/// Bound to a conversational identity exactly like [`is_agent`], so the ingest
/// pipeline can authorise an **operational behaviour-rule** (a directive about
/// which tools/workflow the agent uses — agent-wide, admin-only) from the
/// sender alone. A non-admin's operational directive is refused; a *soul*
/// (conversational) directive stays open to everyone. See
/// the ingest-pipeline design note (behaviour-rule governance).
///
/// # Errors
/// - [`sqlx::Error`] for any SQL failure.
pub async fn is_admin(pool: &SqlitePool, user_id: &str) -> Result<bool, sqlx::Error> {
    let flag: Option<i64> =
        sqlx::query_scalar("SELECT is_admin FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(flag.unwrap_or(0) != 0)
}

/// Stamp the explicit `is_agent` marker on `user_id` (idempotent).
///
/// Called when a standard consumer token establishes its agent identity. A no-op
/// row-count when the user is absent (the bot identity is always a known
/// enrollment user by the time its token connects).
///
/// # Errors
/// - [`sqlx::Error`] for any SQL failure.
pub async fn mark_agent(pool: &SqlitePool, user_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE enrollment_users SET is_agent = 1 WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Gate the **credential** side of the agent⇎human mutual exclusion (migration
/// 0050).
///
/// Returns `Err(message)` when `user_id` is an agent identity, so the
/// credential-creation paths (invitation, password set/reset) refuse to give an
/// agent a dashboard login — the mirror of [`validate_token_identity`], which
/// refuses a standard token to a credentialed human. An identity is EITHER a
/// human with a login OR an agent, never both.
///
/// # Errors
/// - `Err(String)` when `user_id` is `is_agent`, or surfacing a SQL failure.
pub async fn reject_if_agent(pool: &SqlitePool, user_id: &str) -> Result<(), String> {
    if is_agent(pool, user_id).await.map_err(|e| e.to_string())? {
        return Err(format!(
            "{user_id:?} is a consumer-agent identity (is_agent) and cannot also hold a \
             dashboard login: an identity is either a human account with credentials or a \
             bot's credential-less system user, never both (diagonal identity model — \
             docs/concepts/identity-and-acl.md §1.5)"
        ));
    }
    Ok(())
}

/// Atomically replace the contents of `enrollment_users` and
/// `enrollment_groups` with the given file. Assumes the file has
/// already passed [`validate`].
pub async fn mirror_to_db(pool: &SqlitePool, file: &EnrollmentFile) -> Result<(), EnrollmentError> {
    let mut tx = pool.begin().await?;

    // Preserve the explicit `is_agent` markers (migration 0050) across this
    // full-replace: the EnrollmentFile (a dashboard form / yaml import) carries
    // no `is_agent` column, so re-inserting would silently demote every agent
    // identity to a plain user. Snapshot them now, re-stamp after the inserts.
    let preserved_agents: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM enrollment_users WHERE is_agent = 1")
            .fetch_all(&mut *tx)
            .await?;

    sqlx::query("DELETE FROM enrollment_users")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM enrollment_groups")
        .execute(&mut *tx)
        .await?;

    for user in &file.users {
        let aliases_json = serde_json::to_string(&user.aliases)?;
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin, locale)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&user.id)
        .bind(aliases_json)
        .bind(i64::from(user.is_admin))
        .bind(user.locale.as_deref())
        .execute(&mut *tx)
        .await?;
    }

    // Re-stamp the preserved agent markers onto the freshly-inserted rows so a
    // bulk sync never demotes a consumer-agent identity to a plain user.
    for agent in &preserved_agents {
        sqlx::query("UPDATE enrollment_users SET is_agent = 1 WHERE user_id = ?")
            .bind(agent)
            .execute(&mut *tx)
            .await?;
    }

    for group in &file.groups {
        let members_json = serde_json::to_string(&group.members)?;
        sqlx::query(
            "INSERT INTO enrollment_groups (group_id, members, scope)
             VALUES (?, ?, ?)",
        )
        .bind(&group.id)
        .bind(members_json)
        .bind(group.scope.as_deref())
        .execute(&mut *tx)
        .await?;
    }

    // Preserve the builtin universal `global` group across a file sync: it
    // is not part of the operator's enrollment file, but the public/world
    // ACL principal must always exist. No-op if the file declared it.
    sqlx::query(
        "INSERT OR IGNORE INTO enrollment_groups (group_id, members, scope) VALUES (?, '[]', ?)",
    )
    .bind(GLOBAL_GROUP_ID)
    .bind(DEFAULT_GLOBAL_SCOPE)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// What [`remove_user`] tore down besides the `enrollment_users` row —
/// for the caller's log line.
#[derive(Debug, Default)]
pub struct UserRemoval {
    /// Consumer registrations whose `system_user_id` was the deleted user
    /// (the diagonal identity model): each went down with its delegation
    /// grant and its web-agent OAuth rows. The caller should refresh its
    /// [`crate::delegations::DelegationCache`] when this is non-empty so
    /// act-as dies immediately instead of within the TTL window.
    pub consumers_dismantled: Vec<String>,
    /// Web-agent OAuth rows (codes + refresh) deleted because the user was
    /// their approving sender.
    pub oauth_rows_removed: u64,
}

/// Delete one enrolled user together with everything that hangs off the
/// identity, in a single transaction:
///
/// - **consumers bound via `consumers.system_user_id`** — a plain FK with
///   no `ON DELETE` action, so a naked user DELETE is rejected by `SQLite`;
///   a registration whose memory identity is gone means nothing, so the
///   consumer row, its `consumer_delegations` grant and its web-agent
///   OAuth rows are torn down with the user;
/// - **the user's own web-agent OAuth artefacts** (`sender_id = user`) — a
///   live refresh row would otherwise keep minting access tokens for a
///   sender that no longer exists;
/// - **the `enrollment_users` row** — `ON DELETE CASCADE` clears
///   credentials, invitations, 2FA and proposal votes.
///
/// Outstanding JWTs are stateless and cannot be enumerated; they expire at
/// their TTL. Group member lists and delegation allow-lists naming the
/// user are left as-is: a stale id in those JSON arrays never matches an
/// effective sender, which is the documented failure mode
/// (`crate::delegations`).
///
/// Returns `Ok(None)` when no such user exists.
///
/// # Errors
/// - [`sqlx::Error`] for any SQL failure (the transaction rolls back).
pub async fn remove_user(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Option<UserRemoval>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM enrollment_users WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Ok(None);
    }

    let consumers_dismantled: Vec<String> =
        sqlx::query_scalar("SELECT consumer_id FROM consumers WHERE system_user_id = ?")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
    for consumer_id in &consumers_dismantled {
        for sql in [
            "DELETE FROM consumer_delegations WHERE consumer_id = ?",
            "DELETE FROM webagentoauth_codes WHERE consumer_id = ?",
            "DELETE FROM webagentoauth_refresh WHERE consumer_id = ?",
            "DELETE FROM consumers WHERE consumer_id = ?",
        ] {
            sqlx::query(sql).bind(consumer_id).execute(&mut *tx).await?;
        }
    }

    let mut oauth_rows_removed = 0;
    for sql in [
        "DELETE FROM webagentoauth_codes WHERE sender_id = ?",
        "DELETE FROM webagentoauth_refresh WHERE sender_id = ?",
    ] {
        oauth_rows_removed += sqlx::query(sql)
            .bind(user_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    sqlx::query("DELETE FROM enrollment_users WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(UserRemoval {
        consumers_dismantled,
        oauth_rows_removed,
    }))
}

/// `user_id` regex: `^[a-z][a-z0-9°]*$`.
///
/// Same rule applied by the bulk mirror writer ([`validate`]) and by
/// the dashboard CRUD forms, so a single-user insert can never produce
/// an id the validator would later reject.
///
/// The charset is deliberately a **subset of the `WikiId` charset**
/// (`[a-z0-9°-]`): every enrollable id must be creatable as an identity
/// wiki. Underscores are rejected here for exactly that reason —
/// `WikiId::parse` refuses `_`, so an underscore id would enroll and
/// then silently fail to get its identity wiki (maintainer decision,
/// 2026-07-02).
#[must_use]
pub fn is_valid_user_id(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    for c in chars {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '°';
        if !ok {
            return false;
        }
    }
    true
}

/// `group_id` regex: `^[a-z][a-z0-9]*$`.
///
/// Groups deliberately disallow the degree sign so the collision-suffix
/// convention stays unambiguous (only user ids carry it); underscores
/// are out for the same identity-wiki reason as [`is_valid_user_id`].
#[must_use]
pub fn is_valid_group_id(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    for c in chars {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit();
        if !ok {
            return false;
        }
    }
    true
}

/// Reject ids that would land in unsafe filesystem paths (slashes,
/// dot-dot, whitespace). Applied on top of the id regex.
#[must_use]
pub fn is_filesystem_safe(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        && !s.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> UserEntry {
        UserEntry {
            id: id.into(),
            aliases: Vec::new(),
            is_admin: false,
            locale: None,
        }
    }

    fn user_with_aliases(id: &str, aliases: &[&str]) -> UserEntry {
        UserEntry {
            id: id.into(),
            aliases: aliases.iter().map(|s| (*s).to_owned()).collect(),
            is_admin: false,
            locale: None,
        }
    }

    fn group(id: &str, members: &[&str]) -> GroupEntry {
        GroupEntry {
            id: id.into(),
            members: members.iter().map(|s| (*s).to_owned()).collect(),
            scope: None,
        }
    }

    fn canonical_file() -> EnrollmentFile {
        EnrollmentFile {
            version: 1,
            users: vec![
                UserEntry {
                    id: "alice".into(),
                    aliases: vec!["Alice".into(), "ali".into()],
                    is_admin: false,
                    locale: Some("it-IT".into()),
                },
                UserEntry {
                    id: "bob".into(),
                    aliases: Vec::new(),
                    is_admin: true,
                    locale: None,
                },
            ],
            groups: vec![GroupEntry {
                id: "team".into(),
                members: vec!["alice".into(), "bob".into()],
                scope: Some("Project decisions and shared deadlines.".into()),
            }],
        }
    }

    #[test]
    fn validate_accepts_canonical_example() {
        let file = canonical_file();
        let report = validate(&file).expect("validate");
        assert!(
            report.warnings.is_empty(),
            "unexpected warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn version_other_than_one_is_rejected() {
        let file = EnrollmentFile {
            version: 2,
            users: Vec::new(),
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::UnsupportedVersion(2)));
    }

    #[test]
    fn user_id_must_start_with_lowercase_letter() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("1bad")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::InvalidUserId { .. }));
    }

    #[test]
    fn user_id_must_not_contain_uppercase() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("Alice")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::InvalidUserId { .. }));
    }

    #[test]
    fn user_id_allows_degree_sign_marker() {
        // `°` is the Samvise convention for collision suffix; it is
        // allowed inside ids.
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("bilbo°2")],
            groups: Vec::new(),
        };
        validate(&file).expect("must accept");
    }

    #[test]
    fn underscore_is_rejected_for_users_and_groups() {
        // `WikiId::parse` refuses `_`, so an underscore id would enroll
        // and then silently fail to create its identity wiki — forbidden
        // at the door instead (maintainer decision, 2026-07-02).
        assert!(!is_valid_user_id("hermes_bot"));
        assert!(!is_valid_group_id("my_group"));
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("hermes_bot")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::InvalidUserId { .. }));
    }

    #[test]
    fn every_valid_id_is_a_valid_wiki_id() {
        // The invariant behind the grammar: an enrollable id must always
        // be creatable as an identity wiki. Exercise the full id charset
        // in the second position plus representative whole ids.
        for c in ('a'..='z').chain('0'..='9').chain(['°']) {
            let id = format!("a{c}b");
            assert!(is_valid_user_id(&id), "user grammar must accept {id:?}");
            assert!(
                crate::types::WikiId::parse(&id).is_ok(),
                "WikiId must accept every valid user id, got {id:?}"
            );
        }
        for id in ["franz", "bilbo°2", "famiglia", "x9"] {
            assert!(is_valid_user_id(id));
            assert!(crate::types::WikiId::parse(id).is_ok());
        }
    }

    #[test]
    fn duplicate_user_id_is_rejected() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice"), user("alice")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::DuplicateUserId(_)));
    }

    #[test]
    fn group_id_must_not_contain_degree_sign() {
        // Groups deliberately disallow `°` so the collision-suffix
        // convention stays unambiguous (only user ids carry it).
        let file = EnrollmentFile {
            version: 1,
            users: Vec::new(),
            groups: vec![group("a°b", &[])],
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::InvalidGroupId { .. }));
    }

    #[test]
    fn guest_id_is_reserved_for_users_and_groups() {
        // The builtin unidentified-human pseudo-identity can never be
        // enrolled — as a user or as a group.
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("guest")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::ReservedId(_)));

        let file = EnrollmentFile {
            version: 1,
            users: Vec::new(),
            groups: vec![group("guest", &[])],
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::ReservedId(_)));
    }

    #[test]
    fn guest_alias_soft_warns() {
        // An alias "guest" still enrolls (soft), but the operator is told
        // it shadows the builtin pseudo-identity in classifier attribution.
        let file = EnrollmentFile {
            version: 1,
            users: vec![user_with_aliases("marco", &["guest"])],
            groups: Vec::new(),
        };
        let report = validate(&file).expect("soft warn, not an error");
        assert!(
            report.warnings.iter().any(|w| w.contains("guest")),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn group_colliding_with_user_id_is_rejected() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice")],
            groups: vec![group("alice", &["alice"])],
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(err, EnrollmentError::GroupCollidesWithUser(_)));
    }

    #[test]
    fn dangling_group_member_is_rejected() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice")],
            groups: vec![group("team", &["alice", "ghost"])],
        };
        let err = validate(&file).expect_err("must reject");
        match err {
            EnrollmentError::DanglingMember { group, member } => {
                assert_eq!(group, "team");
                assert_eq!(member, "ghost");
            },
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn alias_collision_is_a_warning_not_an_error() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![
                user_with_aliases("alice", &["Boss"]),
                user_with_aliases("bob", &["Boss"]),
            ],
            groups: Vec::new(),
        };
        let report = validate(&file).expect("must accept");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("Boss"));
    }

    #[test]
    fn unsafe_slug_is_rejected() {
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("al ice")],
            groups: Vec::new(),
        };
        let err = validate(&file).expect_err("must reject");
        assert!(matches!(
            err,
            EnrollmentError::InvalidUserId { .. } | EnrollmentError::UnsafeSlug(_)
        ));
    }

    #[tokio::test]
    async fn mirror_to_db_round_trips_users_and_groups() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file = canonical_file();
        validate(&file).expect("validate");
        mirror_to_db(&pool, &file).await.expect("mirror");

        let n_users: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollment_users")
            .fetch_one(&pool)
            .await
            .expect("count users");
        assert_eq!(n_users, 2);

        let (id, aliases, is_admin): (String, Option<String>, i64) = sqlx::query_as(
            "SELECT user_id, aliases, is_admin FROM enrollment_users WHERE user_id = 'alice'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch alice");
        assert_eq!(id, "alice");
        assert_eq!(is_admin, 0);
        let aliases: Vec<String> =
            serde_json::from_str(aliases.as_deref().unwrap_or("[]")).expect("aliases json");
        assert_eq!(aliases, vec!["Alice", "ali"]);

        let (gid, members): (String, String) = sqlx::query_as(
            "SELECT group_id, members FROM enrollment_groups WHERE group_id = 'team'",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch team");
        assert_eq!(gid, "team");
        let members: Vec<String> = serde_json::from_str(&members).expect("members json");
        assert_eq!(members, vec!["alice", "bob"]);
    }

    #[tokio::test]
    async fn remove_user_dismantles_bound_consumer() {
        // The exact shape that used to reject a naked user DELETE on the
        // `consumers.system_user_id` FK (dashboard 500, 2026-07-11): an
        // enrolled identity bound as the system user of a registered
        // consumer, with a delegation grant and a live OAuth refresh row.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('voicebox', 0)")
            .execute(&pool)
            .await
            .expect("user");
        sqlx::query(
            "INSERT INTO consumers (consumer_id, consumer_secret, registered_at, system_user_id)
             VALUES ('voicebox', 'aa', '2026-07-02T00:00:00Z', 'voicebox')",
        )
        .execute(&pool)
        .await
        .expect("consumer");
        sqlx::query(
            "INSERT INTO consumer_delegations
                (consumer_id, allowed_sender_ids, granted_at, granted_by)
             VALUES ('voicebox', '[\"alice\"]', '2026-07-02T00:00:00Z', 'alice')",
        )
        .execute(&pool)
        .await
        .expect("delegation");
        sqlx::query(
            "INSERT INTO webagentoauth_refresh
                (token_hash, client_id, sender_id, consumer_id, wiki_id, created_at, expires_at)
             VALUES ('h1', 'c1', 'voicebox', 'voicebox', 'w1',
                     '2026-07-02T00:00:00Z', '2036-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("refresh");

        let removal = remove_user(&pool, "voicebox")
            .await
            .expect("remove must succeed")
            .expect("user existed");
        assert_eq!(removal.consumers_dismantled, vec!["voicebox"]);

        for (table, n_expected) in [
            ("enrollment_users", 0i64),
            ("consumers", 0),
            ("consumer_delegations", 0),
            ("webagentoauth_refresh", 0),
        ] {
            let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .expect("count");
            assert_eq!(n, n_expected, "{table} not emptied");
        }
    }

    #[tokio::test]
    async fn remove_user_revokes_own_oauth_leaves_foreign_consumers() {
        // A human user with web-agent OAuth rows (their claude.ai
        // connection): deleting the user revokes those rows, but a
        // consumer the user does NOT system-own stays registered.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('alice', 0)")
            .execute(&pool)
            .await
            .expect("user");
        sqlx::query(
            "INSERT INTO consumers (consumer_id, consumer_secret, registered_at)
             VALUES ('webclient', 'bb', '2026-07-02T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("consumer");
        sqlx::query(
            "INSERT INTO webagentoauth_refresh
                (token_hash, client_id, sender_id, consumer_id, wiki_id, created_at, expires_at)
             VALUES ('h2', 'c1', 'alice', 'webclient', 'w1',
                     '2026-07-02T00:00:00Z', '2036-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("refresh");
        sqlx::query(
            "INSERT INTO webagentoauth_codes
                (code_hash, client_id, redirect_uri, code_challenge, sender_id, consumer_id,
                 wiki_id, created_at, expires_at)
             VALUES ('ch1', 'c1', 'https://x/cb', 'ch', 'alice', 'webclient', 'w1',
                     '2026-07-02T00:00:00Z', '2036-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("code");

        let removal = remove_user(&pool, "alice")
            .await
            .expect("remove must succeed")
            .expect("user existed");
        assert!(removal.consumers_dismantled.is_empty());
        assert_eq!(removal.oauth_rows_removed, 2, "code + refresh rows");

        let consumers: i64 = sqlx::query_scalar("SELECT count(*) FROM consumers")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(consumers, 1, "unbound consumer must survive");
        let oauth: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM webagentoauth_refresh)
                  + (SELECT count(*) FROM webagentoauth_codes)",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(oauth, 0, "the deleted user's OAuth rows must be gone");
    }

    #[tokio::test]
    async fn remove_user_missing_returns_none() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        assert!(
            remove_user(&pool, "nobody")
                .await
                .expect("no sql failure")
                .is_none()
        );
    }

    #[tokio::test]
    async fn token_identity_rejects_guest_for_any_class() {
        // The builtin guest pseudo-identity never holds a token — not as
        // a standard bot sender, not as a smart human owner. It is only
        // ever reached via X-MWE-Act-As under a delegation grant.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let err =
            validate_token_identity(&pool, "guest", crate::jwt::ConsumerClass::Standard, true)
                .await
                .expect_err("standard guest token must be refused");
        assert!(err.contains("guest"), "got: {err}");

        let err = validate_token_identity(&pool, "guest", crate::jwt::ConsumerClass::Smart, false)
            .await
            .expect_err("smart guest token must be refused");
        assert!(err.contains("guest"), "got: {err}");
    }

    #[tokio::test]
    async fn db_rejects_two_admins_via_partial_unique_index() {
        // At most one row with is_admin=1 per deployment, enforced
        // by `idx_single_admin` from migration 0015. The mirror writer
        // does not check this in application code on purpose — we want
        // the DB to be the last line of defense.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let first = EnrollmentFile {
            version: 1,
            users: vec![UserEntry {
                id: "admin".into(),
                aliases: Vec::new(),
                is_admin: true,
                locale: None,
            }],
            groups: Vec::new(),
        };
        mirror_to_db(&pool, &first).await.expect("first admin ok");

        // Mirror the same admin again — atomic replace, so it is still
        // exactly one row, still satisfying the index. This is the
        // ordinary "save and reload" path.
        mirror_to_db(&pool, &first)
            .await
            .expect("re-mirror same admin ok");

        // Now try to insert a second admin row directly via SQL,
        // bypassing the mirror writer. The partial unique index must
        // refuse this.
        let err = sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin)
             VALUES (?, NULL, 1)",
        )
        .bind("intruder")
        .execute(&pool)
        .await
        .expect_err("a second admin must be rejected by idx_single_admin");

        let msg = format!("{err}");
        assert!(
            msg.contains("idx_single_admin") || msg.contains("UNIQUE"),
            "expected unique-constraint failure, got {msg}"
        );
    }

    #[tokio::test]
    async fn is_admin_reads_the_marker() {
        // The ingest behaviour-rule gate authorises an operational directive on
        // this alone, so true/false/absent must all be correct.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('boss', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO enrollment_users (user_id, is_admin) VALUES ('peon', 0)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            is_admin(&pool, "boss").await.unwrap(),
            "the admin row reads true"
        );
        assert!(
            !is_admin(&pool, "peon").await.unwrap(),
            "a non-admin row reads false"
        );
        assert!(
            !is_admin(&pool, "ghost").await.unwrap(),
            "an absent user reads false, not an error"
        );
    }

    #[tokio::test]
    async fn mirror_to_db_is_atomic_replace() {
        // Apply once with two users, then apply again with one user.
        // The DB should reflect only the latter — proves the DELETE
        // happened inside the same transaction as the INSERTs.
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file_v1 = canonical_file();
        mirror_to_db(&pool, &file_v1).await.expect("mirror v1");

        let file_v2 = EnrollmentFile {
            version: 1,
            users: vec![user("alice")],
            groups: Vec::new(),
        };
        mirror_to_db(&pool, &file_v2).await.expect("mirror v2");

        let users: Vec<String> = sqlx::query_scalar("SELECT user_id FROM enrollment_users")
            .fetch_all(&pool)
            .await
            .expect("list");
        assert_eq!(users, vec!["alice"]);

        // The operator's groups are dropped, but the builtin `global` group
        // survives the wipe (re-seeded inside mirror_to_db).
        let groups: Vec<String> =
            sqlx::query_scalar("SELECT group_id FROM enrollment_groups ORDER BY group_id ASC")
                .fetch_all(&pool)
                .await
                .expect("list groups");
        assert_eq!(
            groups,
            vec![GLOBAL_GROUP_ID.to_owned()],
            "second mirror dropped the operator group but kept builtin global"
        );
    }

    #[tokio::test]
    async fn ensure_global_group_is_idempotent_and_preserves_edited_scope() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        // open_or_init already seeded global; an admin edits its scope.
        sqlx::query("UPDATE enrollment_groups SET scope = ? WHERE group_id = ?")
            .bind("custom world scope")
            .bind(GLOBAL_GROUP_ID)
            .execute(&pool)
            .await
            .expect("edit scope");

        // Re-ensuring must NOT clobber the edited scope (INSERT OR IGNORE).
        ensure_global_group(&pool).await.expect("ensure");

        let scope: Option<String> =
            sqlx::query_scalar("SELECT scope FROM enrollment_groups WHERE group_id = ?")
                .bind(GLOBAL_GROUP_ID)
                .fetch_one(&pool)
                .await
                .expect("scope");
        assert_eq!(scope.as_deref(), Some("custom world scope"));

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM enrollment_groups WHERE group_id = ?")
                .bind(GLOBAL_GROUP_ID)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(n, 1, "exactly one global row");

        assert!(is_global_group(GLOBAL_GROUP_ID));
        assert!(!is_global_group("famiglia"));
    }

    #[tokio::test]
    async fn groups_for_returns_member_groups_sorted() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice"), user("bob"), user("carol")],
            groups: vec![
                group("famiglia", &["alice", "bob"]),
                group("admins", &["alice"]),
                group("guests", &["carol"]),
            ],
        };
        mirror_to_db(&pool, &file).await.expect("mirror");

        let alice = groups_for(&pool, "alice").await.expect("groups_for alice");
        assert_eq!(alice, vec!["admins", "famiglia"]);

        let bob = groups_for(&pool, "bob").await.expect("groups_for bob");
        assert_eq!(bob, vec!["famiglia"]);

        let carol = groups_for(&pool, "carol").await.expect("groups_for carol");
        assert_eq!(carol, vec!["guests"]);

        let ghost = groups_for(&pool, "ghost").await.expect("groups_for ghost");
        assert!(ghost.is_empty(), "unknown user belongs to no groups");

        let anonymous = groups_for(&pool, "").await.expect("groups_for empty");
        assert!(
            anonymous.is_empty(),
            "empty user_id short-circuits to empty"
        );
    }

    #[tokio::test]
    async fn members_for_returns_group_roster_sorted() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice"), user("bob"), user("carol")],
            groups: vec![
                group("famiglia", &["bob", "alice"]),
                group("guests", &["carol"]),
            ],
        };
        mirror_to_db(&pool, &file).await.expect("mirror");

        // The roster comes back alphabetically regardless of insertion order.
        let famiglia = members_for(&pool, "famiglia").await.expect("members");
        assert_eq!(famiglia, vec!["alice", "bob"]);

        let guests = members_for(&pool, "guests").await.expect("members");
        assert_eq!(guests, vec!["carol"]);

        // The builtin global group enumerates nobody (empty members array).
        let global = members_for(&pool, GLOBAL_GROUP_ID).await.expect("global");
        assert!(global.is_empty(), "global group has no enumerated members");

        // Unknown and empty group_id both short-circuit to empty, not an error.
        assert!(
            members_for(&pool, "ghosts")
                .await
                .expect("unknown")
                .is_empty()
        );
        assert!(members_for(&pool, "").await.expect("empty").is_empty());
    }

    #[tokio::test]
    async fn groups_with_scope_for_returns_scope_pairs_sorted() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file = EnrollmentFile {
            version: 1,
            users: vec![user("alice"), user("bob")],
            groups: vec![
                GroupEntry {
                    id: "famiglia".into(),
                    members: vec!["alice".into(), "bob".into()],
                    scope: Some("Spesa, regole di casa, piani condivisi.".into()),
                },
                // No scope configured — must surface as None, not "".
                GroupEntry {
                    id: "admins".into(),
                    members: vec!["alice".into()],
                    scope: None,
                },
            ],
        };
        mirror_to_db(&pool, &file).await.expect("mirror");

        // The builtin global group is surfaced to every sender (sorted last).
        let global = (
            GLOBAL_GROUP_ID.to_owned(),
            Some(DEFAULT_GLOBAL_SCOPE.to_owned()),
        );

        // Sorted by group_id: admins before famiglia before global. The scope
        // column rides along, `None` when the operator left it unset.
        let alice = groups_with_scope_for(&pool, "alice")
            .await
            .expect("scope pairs alice");
        assert_eq!(
            alice,
            vec![
                ("admins".to_owned(), None),
                (
                    "famiglia".to_owned(),
                    Some("Spesa, regole di casa, piani condivisi.".to_owned())
                ),
                global.clone(),
            ]
        );

        // bob is in famiglia and (like everyone) the global group.
        let bob = groups_with_scope_for(&pool, "bob")
            .await
            .expect("scope pairs bob");
        assert_eq!(
            bob,
            vec![
                (
                    "famiglia".to_owned(),
                    Some("Spesa, regole di casa, piani condivisi.".to_owned())
                ),
                global.clone(),
            ]
        );

        // An enrolled-but-groupless / unknown sender still belongs to global.
        assert_eq!(
            groups_with_scope_for(&pool, "ghost").await.expect("ghost"),
            vec![global]
        );
        // The anonymous (empty) sender short-circuits to empty.
        assert!(
            groups_with_scope_for(&pool, "")
                .await
                .expect("empty")
                .is_empty()
        );
    }

    /// `locale_for` happy path: an admin-configured locale round-trips
    /// through `mirror_to_db` → lookup.
    #[tokio::test]
    async fn locale_for_returns_configured_locale() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let file = canonical_file();
        validate(&file).expect("validate");
        mirror_to_db(&pool, &file).await.expect("mirror");

        // `canonical_file` gives alice locale = it-IT, bob locale = None.
        let alice = locale_for(&pool, "alice").await.expect("lookup alice");
        assert_eq!(alice.as_deref(), Some("it-IT"));
        let bob = locale_for(&pool, "bob").await.expect("lookup bob");
        assert!(bob.is_none(), "bob has no locale configured");
    }

    /// Unknown `user_id` and empty `user_id` both surface as `None`
    /// without an error — the orchestrator falls back to the mirror
    /// clause.
    #[tokio::test]
    async fn locale_for_unknown_or_empty_user_returns_none() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        let ghost = locale_for(&pool, "ghost-user").await.expect("lookup ghost");
        assert!(ghost.is_none());
        let anon = locale_for(&pool, "").await.expect("lookup empty");
        assert!(anon.is_none());
    }

    /// A whitespace-only `locale` column collapses to `None` so the
    /// renderer falls back to the mirror clause instead of building a
    /// `"User locale:  . Respond in …"` malformed directive.
    #[tokio::test]
    async fn locale_for_treats_whitespace_only_as_none() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");

        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin, locale)
             VALUES ('ghost', '[]', 0, '   ')",
        )
        .execute(&pool)
        .await
        .expect("insert ghost");

        let v = locale_for(&pool, "ghost").await.expect("lookup ghost");
        assert!(v.is_none(), "whitespace-only locale must collapse to None");
    }

    /// The explicit `is_agent` marker (migration 0050) round-trips, and the
    /// credential-side gate `reject_if_agent` blocks a login for an agent while
    /// letting plain / unknown identities through.
    #[tokio::test]
    async fn is_agent_marker_round_trips_and_gate_blocks_agents() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        sqlx::query(
            "INSERT INTO enrollment_users (user_id, aliases, is_admin) VALUES ('bot', '[]', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Default false → the gate lets it through (it could still become a human).
        assert!(!is_agent(&pool, "bot").await.unwrap());
        assert!(reject_if_agent(&pool, "bot").await.is_ok());

        // Mark it → the gate now bars giving it a login.
        mark_agent(&pool, "bot").await.unwrap();
        assert!(is_agent(&pool, "bot").await.unwrap());
        assert!(
            reject_if_agent(&pool, "bot").await.is_err(),
            "an agent identity cannot also hold a dashboard login"
        );

        // An unknown user is not an agent and is not blocked.
        assert!(!is_agent(&pool, "ghost").await.unwrap());
        assert!(reject_if_agent(&pool, "ghost").await.is_ok());
    }

    /// A bulk `mirror_to_db` (whose `EnrollmentFile` carries no `is_agent`
    /// column) must NOT silently demote an agent identity to a plain user.
    #[tokio::test]
    async fn mirror_to_db_preserves_is_agent_marker() {
        let pool = crate::db::open_or_init(
            Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))).path(),
        )
        .await
        .expect("open db");
        let file = EnrollmentFile {
            version: 1,
            users: vec![user("hermesbot"), user("alice")],
            groups: Vec::new(),
        };
        mirror_to_db(&pool, &file).await.expect("mirror");
        mark_agent(&pool, "hermesbot").await.unwrap();
        assert!(is_agent(&pool, "hermesbot").await.unwrap());

        // A second sync from the (is_agent-less) file must keep the marker.
        mirror_to_db(&pool, &file).await.expect("re-mirror");
        assert!(
            is_agent(&pool, "hermesbot").await.unwrap(),
            "is_agent survives a bulk replace"
        );
        assert!(
            !is_agent(&pool, "alice").await.unwrap(),
            "a non-agent stays a non-agent"
        );
    }
}
