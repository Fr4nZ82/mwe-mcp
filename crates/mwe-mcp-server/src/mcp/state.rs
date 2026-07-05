// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared state behind every MCP transport.
//!
//! `McpState` is the single bag of handles cloned into the HTTP
//! `StreamableHttpService` factory and into the stdio `ServerHandler`.
//! Cheap to clone — every member is either `Arc`-backed (pool, secret,
//! cache, embedder, tree path) or `Copy`.
//!
//! `IdentityProfile` is the authenticated caller projected into the
//! shape the per-tool handlers need. It is derived per request from the
//! `Authorization: Bearer …` JWT in
//! [`super::auth::jwt_auth_middleware`] (mwe-mcp is HTTP-only).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use mwe_core::config::{LlmConfig, RecallConfig};
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::Embedder;
use mwe_core::jwt::{BlacklistCache, ConsumerClass, ConsumerProfile, TokenSecret};
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;

/// Shape of an authenticated MCP caller.
///
/// Every field is filled from the per-call JWT (mwe-mcp is HTTP-only).
/// The per-tool handlers downstream see one canonical shape.
#[derive(Debug, Clone)]
pub struct IdentityProfile {
    /// `sender_id` claim — the user the call acts on behalf of.
    pub sender_id: String,
    /// `device_label` claim — for audit (`tool_executions.device_label`).
    pub device_label: String,
    /// `rate_limit_id` claim — referenced from `mwe-mcp.config.yaml`.
    pub rate_limit_id: String,
    /// `consumer_id` claim — set on bot / orchestrator tokens; required
    /// for `events_poll` / `events_ack` calls.
    pub consumer_id: Option<String>,
    /// UI gating hint — `tool_log_search` surfaces every row when this
    /// is true; otherwise it scopes to `sender_id = caller`.
    pub is_admin: bool,
    /// Consumer class — `Smart` authorises the smart-wiki
    /// tool families (`wiki_admin_*`, `wiki_type_register`). Defaults
    /// to [`ConsumerClass::Standard`] when the JWT omits the claim, so
    /// older tokens behave exactly as before.
    pub consumer_class: ConsumerClass,
    /// Connection profile — `Web` (claude.ai over `webagentoauth`) trims the
    /// `tools/list` catalog to what a bridge-less, no-local-FS client can use.
    /// Defaults to [`ConsumerProfile::Local`] (full catalog) for every other
    /// consumer.
    pub profile: ConsumerProfile,
}

impl IdentityProfile {
    /// Construct a profile from a verified [`mwe_core::jwt::TokenClaims`].
    #[must_use]
    pub fn from_claims(claims: mwe_core::jwt::TokenClaims) -> Self {
        Self {
            sender_id: claims.sender_id,
            device_label: claims.device_label,
            rate_limit_id: claims.rate_limit_id,
            consumer_id: claims.consumer_id,
            is_admin: claims.is_admin,
            consumer_class: claims.consumer_class,
            profile: claims.profile,
        }
    }
}

/// All the long-lived handles the dispatcher needs.
///
/// Cloning is `O(arc bump count)` — every field is share-friendly so a
/// per-request clone in `service_factory` does not allocate.
#[derive(Clone)]
pub struct McpState {
    /// Connection pool to `engine.db`. Shared with the dashboard.
    pub pool: SqlitePool,
    /// Memory-wiki tree handle. Cheap to clone (`PathBuf` only).
    pub tree: WikiTree,
    /// Embedding model used by `wiki_ingest_message` + `wiki_search`.
    pub embedder: Arc<dyn Embedder>,
    /// JWT secret — shared with the dashboard so a dashboard cookie and
    /// an MCP token verify against the same key.
    pub secret: TokenSecret,
    /// Shared revocation cache — same instance as the dashboard.
    pub blacklist: Arc<BlacklistCache>,
    /// Shared mirror of `consumer_delegations` — same instance as the
    /// dashboard so a `X-MWE-Act-As` resolution sees the admin's most
    /// recent edits via the `tokens.rs` write paths (which call
    /// [`mwe_core::delegations::DelegationCache::refresh`]).
    pub delegations: Arc<DelegationCache>,
    /// LLM function configuration. The dispatcher reads it on each
    /// `wiki_ingest_message` call to materialise the `ingest` backend
    /// fresh — picks up config edits without restarting the server.
    pub llm_config: LlmConfig,
    /// Operator recall settings (`recall:` config section). Shared
    /// behind `Arc<RwLock>` with the dashboard's recall-settings editor
    /// so a save there reaches the next ingest turn without a restart.
    pub recall: Arc<RwLock<RecallConfig>>,
    /// Workdir — exposed for tools that need filesystem paths relative
    /// to the workdir root (blob reads, prompt overrides).
    pub workdir: PathBuf,
    /// Document-ingest resource knobs (`document:` config section),
    /// resolved once at boot — shared by the `wiki_ingest_external`
    /// handler and the document worker.
    pub document_policy: mwe_core::document::DocumentPolicy,
}

impl std::fmt::Debug for McpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpState")
            .field("workdir", &self.workdir)
            .field("secret", &"<redacted>")
            .field("embedder", &self.embedder.model_id())
            .finish_non_exhaustive()
    }
}
