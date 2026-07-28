// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stable error-class mapping for the MCP dispatcher.
//!
//! Goal: every per-tool handler returns one of a small, audited set of
//! error classes ([tool reference](../../../../docs/protocol/tool-reference.md))
//! so the consumer can branch on the wire string instead of reading
//! free-form messages. The dispatcher writes the same string into
//! `tool_executions.error` and into the JSON-RPC error response.
//!
//! `into_mcp_error` is the single conversion: it composes the `code` /
//! `message` pair the rmcp layer needs while preserving the class
//! string for the audit row.

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::json;

/// One of the canonical wire error classes the dispatcher emits.
/// Order matches the table in `tool-reference.md §0.errors`.
#[derive(Debug, Clone, Copy)]
pub enum ToolErrorClass {
    /// `400 invalid_input` — malformed or missing parameter.
    InvalidInput,
    /// `403 sender_unauthorized` — sender not authorised for this op.
    SenderUnauthorized,
    /// `403 sender_token_mismatch` — `sender_id` arg ≠ token sender.
    SenderTokenMismatch,
    /// `403 consumer_not_registered` — bot/orch not in `consumers`.
    ConsumerNotRegistered,
    /// `404 not_found` — entity missing.
    NotFound,
    /// `500 internal_error` — unexpected bug, surface generic message.
    InternalError,
    /// `503 service_unavailable` — backing infra down or feature not
    /// yet fully implemented.
    ServiceUnavailable,
    /// `501 not_implemented_phase_c` — the tool exists but its full
    /// implementation lands later. Used by `structure_proposal_apply` /
    /// `_revert` and `wiki_ingest_external` non-inline sources.
    NotImplementedPhaseC,
    /// `403 requires_consumer_class_smart` — token has
    /// `consumer_class != smart`; tool is in the smart-wiki
    /// authoritative-write families H + I.
    RequiresConsumerClassSmart,
    /// `403 wiki_owned_by_other_user` — `wiki.owner_user` does not
    /// match `token.owner_user`. Invariant: a smart consumer is
    /// custodian of writes only for wikis its owner owns.
    WikiOwnedByOtherUser,
    /// `400 wiki_type_not_admin_writable` — `wiki_admin_push` /
    /// `_pull` targeted a wiki whose smart flag is `false`
    /// (refactor of the previous `family != "companion"` check).
    /// Standard wikis are written via `wiki_ingest_message`.
    WikiTypeNotAdminWritable,
    /// `400 wiki_type_not_briefing_capable` — `wiki_admin_notify`
    /// targeted a wiki whose smart flag is `false`.
    /// `_briefing.md` only exists in smart-wikis.
    ///
    /// Preserved for REM-internal callers (`notify_as_rem`); the
    /// public MCP path uses the matrix variants below instead.
    WikiTypeNotBriefingCapable,
    /// `403 smart_does_not_notify_own_wiki` — matrix
    /// cell `smart consumer × smart wiki`: the smart consumer administers the
    /// smart-wiki directly via `wiki_admin_push`, so notifying
    /// itself is a no-op-by-design.
    SmartDoesNotNotifyOwnWiki,
    /// `403 standard_uses_ingest_for_memory` — matrix
    /// cell `standard consumer × standard wiki`: the canonical channel for a
    /// standard consumer on a standard wiki is `wiki_ingest_message`.
    StandardUsesIngestForMemory,
    /// `400 consumer_class_wiki_family_mismatch` — generic
    /// superset returned only for branches without an explicit cell
    /// in the matrix (forward-compatibility fallback when a future
    /// `ConsumerClass` variant lands without a documented row).
    ConsumerClassWikiFamilyMismatch,
    /// `409 conflicting_op_log_head` — the `expected_op_log_head`
    /// passed to `wiki_admin_push` does not match the current head;
    /// the consumer must `wiki_admin_pull`, re-diff, and retry.
    ConflictingOpLogHead,
    /// `429 rate_limited` — generic rate-limit cap exceeded.
    /// Currently surfaced only by `wiki_admin_notify` (`50/wiki/h`).
    RateLimited,
    /// `423 wiki_locked_by_lease` — another smart consumer holds an
    /// active `wiki_admin_lease` on the target wiki. The
    /// caller must wait for the lease to expire / be released, or
    /// re-acquire it themselves if the holder is the same logical
    /// consumer.
    WikiLockedByLease,
    /// `400 unknown_briefing_item_id` — `wiki_admin_push.mark_processed`
    /// carried a `bi_<N>` id that either does not exist or
    /// does not belong to the wiki the push targets. The whole push
    /// is rolled back; the caller may retry without the offending id.
    UnknownBriefingItemId,
    /// `400 too_many_briefing_items` — `wiki_admin_push.mark_processed`
    /// list exceeded `MARK_PROCESSED_CAP_PER_PUSH` entries (currently
    /// 50, matching the per-wiki notify rate-limit cap). The caller
    /// should split the marks across multiple pushes.
    TooManyBriefingItems,
    /// `403 instance_read_only` — the deployment runs with
    /// `instance.read_only`, and the tool changes memory or
    /// configuration. Reading, searching and navigating are unaffected;
    /// the refusal is the whole write half of the surface at once, so
    /// the class names the *instance*, not the caller: no token, role or
    /// consumer class lifts it.
    InstanceReadOnly,
    /// `400 wiki_type_requires_parent` — the requested `wiki_type`
    /// declares `requires_parent: true` and the create
    /// call did not pass a `parent_wiki_id`. Day-one user is
    /// `wiki-cron`, which inherits its ACL scope from the parent; the
    /// gate is generic so any future child-only type can opt in via
    /// the template field.
    WikiTypeRequiresParent,
}

impl ToolErrorClass {
    /// Canonical wire string (lowercase `snake_case`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::SenderUnauthorized => "sender_unauthorized",
            Self::SenderTokenMismatch => "sender_token_mismatch",
            Self::ConsumerNotRegistered => "consumer_not_registered",
            Self::NotFound => "not_found",
            Self::InternalError => "internal_error",
            Self::ServiceUnavailable => "service_unavailable",
            Self::NotImplementedPhaseC => "not_implemented_phase_c",
            Self::RequiresConsumerClassSmart => "requires_consumer_class_smart",
            Self::WikiOwnedByOtherUser => "wiki_owned_by_other_user",
            Self::WikiTypeNotAdminWritable => "wiki_type_not_admin_writable",
            Self::WikiTypeNotBriefingCapable => "wiki_type_not_briefing_capable",
            Self::SmartDoesNotNotifyOwnWiki => "smart_does_not_notify_own_wiki",
            Self::StandardUsesIngestForMemory => "standard_uses_ingest_for_memory",
            Self::ConsumerClassWikiFamilyMismatch => "consumer_class_wiki_family_mismatch",
            Self::ConflictingOpLogHead => "conflicting_op_log_head",
            Self::RateLimited => "rate_limited",
            Self::WikiLockedByLease => "wiki_locked_by_lease",
            Self::UnknownBriefingItemId => "unknown_briefing_item_id",
            Self::TooManyBriefingItems => "too_many_briefing_items",
            Self::InstanceReadOnly => "instance_read_only",
            Self::WikiTypeRequiresParent => "wiki_type_requires_parent",
        }
    }

    /// JSON-RPC error code matching the class. Maps to the standard
    /// codes where applicable; uses `-32603` (`internal_error`) for
    /// classes without a closer-fitting JSON-RPC slot.
    #[must_use]
    pub const fn json_rpc_code(self) -> ErrorCode {
        match self {
            Self::InvalidInput => ErrorCode::INVALID_PARAMS,
            Self::NotFound => ErrorCode::METHOD_NOT_FOUND,
            _ => ErrorCode::INTERNAL_ERROR,
        }
    }
}

/// Public tool-side error.
///
/// Carries the wire class string + human-readable message; converted
/// to `McpError` at the dispatcher boundary.
#[derive(Debug, Clone)]
pub struct ToolError {
    /// Class string written to `tool_executions.error`.
    pub class: ToolErrorClass,
    /// Human message — short, English, no PII. Always suffixed with the
    /// class string so an operator reading raw logs has both.
    pub message: String,
}

impl ToolError {
    /// Build a [`ToolError`] from a class + message string.
    pub fn new(class: ToolErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.as_str(), self.message)
    }
}

impl std::error::Error for ToolError {}

/// Convert a [`ToolError`] into the rmcp wire shape. The wire class
/// string rides as `data.error_class` so the consumer can branch on it
/// regardless of the JSON-RPC code.
#[must_use]
pub fn into_mcp_error(err: ToolError) -> McpError {
    let class = err.class.as_str();
    McpError::new(
        err.class.json_rpc_code(),
        err.message,
        Some(json!({ "error_class": class })),
    )
}

/// Convenience builder for the common "deserialize args failed" path
/// — wraps `serde_json::Error` into an `invalid_input` [`ToolError`].
pub fn invalid_input(msg: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorClass::InvalidInput, msg)
}
