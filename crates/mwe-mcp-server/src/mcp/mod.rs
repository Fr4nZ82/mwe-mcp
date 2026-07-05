// SPDX-License-Identifier: AGPL-3.0-or-later
//! MCP dispatcher — implements [`rmcp::ServerHandler`] for the
//! tool surface and plumbs the per-call audit trail.
//!
//! ## Architecture
//!
//! - [`state::McpState`] is the single shared bag of handles (pool,
//!   tree, embedder, secret, blacklist, LLM config). Cloned into the
//!   HTTP `StreamableHttpService` factory.
//! - [`auth::jwt_auth_middleware`] runs in front of `/mcp` on the HTTP
//!   transport, extracts the bearer JWT, verifies it, attaches an
//!   [`state::IdentityProfile`] to the request extensions.
//! - [`McpHandler`] (this module) reads the profile from request
//!   extensions (the HTTP JWT), calls
//!   the per-tool handler in [`tools`], and writes one
//!   `tool_executions` row via [`mwe_core::audit::record`].
//! - [`schemas::all_tools`] lists every tool with its JSON Schema,
//!   shared between `list_tools` and HTTP introspection.
//!
//! ## Error mapping
//!
//! Per-tool handlers return [`error::ToolError`] (class string +
//! message). The dispatcher folds the class into both the
//! `tool_executions.error` column **and** the wire `McpError` via
//! [`error::into_mcp_error`], so a consumer can branch on the same
//! string the audit log records.

use std::sync::Arc;
use std::time::Instant;

use mwe_core::audit::{self, ToolExecutionInput};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub mod auth;
pub mod error;
pub mod schemas;
pub mod state;
pub mod tools;

use error::{ToolError, ToolErrorClass, into_mcp_error};
use state::{IdentityProfile, McpState};

/// Server identity returned in the `initialize` handshake.
const SERVER_NAME: &str = "mwe-mcp";

/// Dispatcher type, one per connection. Cloning is cheap (every member
/// is `Arc`-shared via [`McpState`]).
#[derive(Clone)]
pub struct McpHandler {
    state: McpState,
}

impl McpHandler {
    /// Build a fresh handler around shared state.
    #[must_use]
    pub const fn new(state: McpState) -> Self {
        Self { state }
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> InitializeResult {
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        let mut server = Implementation::from_build_env();
        server.name = SERVER_NAME.into();
        server.title = Some("Memory Wiki Engine (mwe-mcp)".into());
        server.version = env!("CARGO_PKG_VERSION").into();
        info.server_info = server;
        // No per-connection profile is available here (`get_info` is context-free),
        // so the text covers both paths and each client self-selects: a local CLI
        // (Claude Code, full `Local` catalog incl. `skill_fetch`) loads `core` and
        // follows its dispatcher; a bridge-less web client (claude.ai, trimmed `Web`
        // catalog without `skill_fetch`) uses the uploaded `web-smart-consumer` skill.
        info.instructions = Some(
            "mwe-mcp memory server — tools are organised into families A–L (see \
             tool-reference.md for wire shapes). Load the skill that matches how you \
             connected: a local CLI agent (e.g. Claude Code over OAuth on a loopback \
             callback) should `skill_fetch` the `core` skill and follow its dispatcher \
             — it routes to `smart-consumer`/`smart-codebase` inside a project, or \
             `core-globalmemory` for transversal recall; a bridge-less web client \
             (claude.ai, no local filesystem and no `skill_fetch`) follows the bundled \
             `web-smart-consumer` skill instead. Either way, recall before you answer \
             and keep the user's memory current as you go."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Shape the advertised catalog to the caller's connection profile: a
        // bridge-less web client (claude.ai over webagentoauth) gets the trimmed
        // web surface, every other consumer the full set. Call-time auth is
        // unchanged — this only cuts routing noise. On the (JWT-gated, so
        // practically impossible) no-identity path, fall back to the full set.
        let profile = resolve_identity(&ctx)
            .map(|p| p.profile)
            .unwrap_or_default();
        Ok(ListToolsResult {
            tools: schemas::tools_for(profile),
            next_cursor: None,
            meta: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        schemas::all_tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let identity = match resolve_identity(&ctx) {
            Ok(p) => p,
            Err(e) => {
                record_audit(
                    &self.state,
                    "<unauthenticated>",
                    &params.name,
                    None,
                    &started,
                    Some(e.class),
                    Some(&e.message),
                );
                return Err(into_mcp_error(e));
            },
        };

        let raw_args = params.arguments.clone().map_or(Value::Null, Value::Object);
        let args_hash = hash_args(&raw_args);
        let outcome = dispatch(&self.state, &identity, &params.name, raw_args).await;

        match outcome {
            Ok(value) => {
                let summary = short_result_summary(&value);
                record_audit(
                    &self.state,
                    &identity.sender_id,
                    &params.name,
                    Some(&args_hash),
                    &started,
                    None,
                    Some(&summary),
                );
                Ok(success_result(value))
            },
            Err(e) => {
                let cls = e.class;
                let msg = e.message.clone();
                record_audit(
                    &self.state,
                    &identity.sender_id,
                    &params.name,
                    Some(&args_hash),
                    &started,
                    Some(cls),
                    Some(&msg),
                );
                Err(into_mcp_error(e))
            },
        }
    }
}

fn resolve_identity(ctx: &RequestContext<RoleServer>) -> Result<IdentityProfile, ToolError> {
    if let Some(parts) = ctx.extensions.get::<http::request::Parts>()
        && let Some(profile) = parts.extensions.get::<IdentityProfile>().cloned()
    {
        return Ok(profile);
    }
    Err(ToolError::new(
        ToolErrorClass::SenderUnauthorized,
        "no authenticated identity attached to this MCP call",
    ))
}

/// Direct dispatcher entry point — public for integration tests.
///
/// Also used by callers that want to drive a single tool without
/// spinning up a transport. Soft contract identical to the
/// `ServerHandler::call_tool` path: the audit row is **not** written
/// here (the dispatcher writes it around this call).
pub async fn dispatch(
    state: &McpState,
    identity: &IdentityProfile,
    tool_name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match tool_name {
        "wiki_ingest_message" => tools::call_wiki_ingest_message(state, identity, args).await,
        "events_poll" => tools::call_events_poll(state, identity, args).await,
        "events_ack" => tools::call_events_ack(state, identity, args).await,
        // The whole `structure_proposal_*` family was removed from the
        // MCP surface — structural changes apply directly in REM and
        // reach the consumer as `structure_applied` notices over
        // `events_poll`; the dashboard is the undo surface and calls
        // `mwe-core::proposals` directly.
        "wiki_read" => tools::call_wiki_read(state, identity, args).await,
        "wiki_search" => tools::call_wiki_search(state, identity, args).await,
        "wiki_navigate" => tools::call_wiki_navigate(state, identity, args).await,
        "tool_log_search" => tools::call_tool_log_search(state, identity, args).await,
        "wiki_lint" => tools::call_wiki_lint(state, identity, args).await,
        "consumer_register" => tools::call_consumer_register(state, identity, args).await,
        "wiki_ingest_external" => tools::call_wiki_ingest_external(state, identity, args).await,
        "dashboard_link" => tools::call_dashboard_link(state, identity, args).await,
        // Smart-wiki authoritative writes (H family).
        "wiki_admin_push" => tools::call_wiki_admin_push(state, identity, args).await,
        "wiki_admin_pull" => tools::call_wiki_admin_pull(state, identity, args).await,
        // Open to any read-access token (NOT gated on smart),
        // so a standard openclaw can relay user observations into the
        // smart consumer's briefing.
        "wiki_admin_notify" => tools::call_wiki_admin_notify(state, identity, args).await,
        // Optional cooperative lease for `wiki_admin_push`
        // coordination across multiple smart consumers of the same
        // owner. Both gated on `consumer_class=smart`.
        "wiki_admin_lease_acquire" => {
            tools::call_wiki_admin_lease_acquire(state, identity, args).await
        },
        "wiki_admin_lease_release" => {
            tools::call_wiki_admin_lease_release(state, identity, args).await
        },
        // Skill catalog (I family). Open to every authenticated
        // token — bundled skills are public documentation. (Custom
        // skills were removed in the `wiki_type` teardown.)
        "skill_list" => tools::call_skill_list(state, identity, args).await,
        "skill_fetch" => tools::call_skill_fetch(state, identity, args).await,
        // K family — atomic primitives for the Claude Code hook
        // bundle (`SessionStart` + `UserPromptSubmit`). Both gated on
        // `consumer_class=smart` server-side.
        "smart_bootstrap" => tools::call_smart_bootstrap(state, identity, args).await,
        "recall_core_global" => tools::call_recall_core_global(state, identity, args).await,
        // L family — authority-routed forget. The caller deletes a fact
        // they authored, or opens an audience vote to forget one they own
        // but did not author (voting itself stays dashboard-only);
        // `wiki_forget_bulk` clears the caller's own facts in bulk.
        "wiki_forget" => tools::call_wiki_forget(state, identity, args).await,
        "wiki_forget_bulk" => tools::call_wiki_forget_bulk(state, identity, args).await,
        other => Err(ToolError::new(
            ToolErrorClass::NotFound,
            format!("unknown tool: {other}"),
        )),
    }
}

fn hash_args(args: &Value) -> String {
    let bytes = serde_json::to_vec(args).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

/// Per-tool result summary written to `tool_executions.result_summary`.
/// We bound the length so the audit row stays short.
fn short_result_summary(value: &Value) -> String {
    let serialised = value.to_string();
    if serialised.len() > 240 {
        let mut out = serialised.chars().take(237).collect::<String>();
        out.push_str("...");
        out
    } else {
        serialised
    }
}

fn success_result(value: Value) -> CallToolResult {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    let mut result = CallToolResult::structured(value);
    result.content = vec![Content::text(text)];
    result
}

fn record_audit(
    state: &McpState,
    sender_id: &str,
    tool_name: &str,
    args_hash: Option<&str>,
    started: &Instant,
    error_class: Option<ToolErrorClass>,
    summary_or_msg: Option<&str>,
) {
    let pool = state.pool.clone();
    let tool_name = tool_name.to_owned();
    let sender_id = sender_id.to_owned();
    // mwe-mcp is HTTP-only; the per-call device/rate-limit aren't
    // threaded into this audit helper, so they stay at the historical
    // HTTP defaults.
    let device_label = "mcp".to_owned();
    let rate_limit_id: Option<String> = None;
    let args_hash = args_hash.map(str::to_owned);
    let summary = summary_or_msg.map(str::to_owned);
    let elapsed_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let err_class = error_class.map(|c| c.as_str().to_owned());
    tokio::spawn(async move {
        let _ = audit::record(
            &pool,
            &ToolExecutionInput {
                tool_name: &tool_name,
                sender_id: &sender_id,
                device_label: &device_label,
                rate_limit_id: rate_limit_id.as_deref(),
                args_hash: args_hash.as_deref(),
                result_summary: summary.as_deref(),
                latency_ms: elapsed_ms,
                cost_estimate: None,
                error: err_class.as_deref(),
            },
        )
        .await;
    });
}

/// Borrow of the shared state for `service_factory` Fn closures —
/// rmcp's signature wants `Send + Sync` factories so the `Arc<McpState>`
/// must be cloned per call.
pub fn factory_for(
    state: Arc<McpState>,
) -> impl Fn() -> std::io::Result<McpHandler> + Send + Sync + 'static {
    move || Ok(McpHandler::new((*state).clone()))
}
