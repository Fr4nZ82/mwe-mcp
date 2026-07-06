// SPDX-License-Identifier: AGPL-3.0-or-later
//! LLM adapter — trait + Ollama backend.
//!
//! ## Why a trait
//!
//! The five canonical LLM functions — `hub_writer`, `ingest`,
//! `rem_promotions`, `rem_dedup_semantic`, `cronista` — all want the
//! same thing: feed a prompt (optionally with a system message) to a
//! model, get back the completion. The provider behind that contract
//! is a deployment choice (Ollama for local-only, Anthropic/OpenAI/
//! Google for cloud) that the operator pins per-function in
//! `mwe-mcp.config.yaml`.
//!
//! ## What ships today
//!
//! - The [`LlmBackend`] trait (async, `Send + Sync`, dyn-safe via
//!   `async_trait`).
//! - [`CompletionRequest`] + [`CompletionResponse`] + [`CompletionUsage`]
//!   value types so the trait stays narrow and additive (extending
//!   `CompletionRequest` doesn't break implementations as long as new
//!   fields default).
//! - Chat-with-tools shapes — [`ChatMessage`], [`Role`], [`Tool`],
//!   [`ToolCall`], [`ChatRequest`], [`ChatResponse`] — used by the
//!   dashboard's agentic loop (LLM functions) when
//!   it composes `_internal.*` operations through function calling.
//!   The `complete` single-prompt path stays untouched: callers that
//!   do not need tools (ingest, REM, dedup) keep using it.
//! - [`OllamaBackend`] — an `HTTP` client for the Ollama generate API
//!   (`POST /api/generate`) with `stream: false`, plus the chat API
//!   (`POST /api/chat`) for the tools-enabled path. Default model is
//!   the operator's choice; we don't bake in a name.
//! - [`FakeLlmBackend`] under `#[cfg(any(test, feature = "test-fakes"))]`
//!   for downstream tests that need a deterministic response without
//!   the network.
//!
//! Anthropic / Google (Gemini) providers also ship, plus `OpenRouter` (an
//! OpenAI-compatible aggregator — one key, hundreds of `vendor/model`
//! slugs); a direct first-party `OpenAI` adapter is deferred.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// Default Ollama base URL ([cf. `crate::embedder::DEFAULT_OLLAMA_URL`]).
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// HTTP timeout for a single completion. Generation can take tens of
/// seconds for long outputs on a local Ollama; 120 s is a safe ceiling
/// that still surfaces a hung backend.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a completion stopped — coarse mapping from per-provider
/// finish-reason strings, since callers rarely care about provider-
/// specific nuance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model emitted an end-of-turn token.
    EndOfTurn,
    /// The completion hit the request's `max_tokens` ceiling.
    MaxTokens,
    /// Provider returned a reason we did not categorise (logged at
    /// debug level by the backend).
    Other,
}

/// Central truncation tripwire (maintainer ruling 2026-07-02): output
/// caps are resource valves, but a reply that stops AT the ceiling is on
/// its way to a silent parse failure upstream — so every backend warns
/// here, loudly, unless the caller declared the tiny cap intentional
/// (`CompletionRequest::truncation_expected`, the health probes). The
/// reply still flows unchanged: a cap yields a warning, never an error.
fn warn_if_truncated(
    backend: &'static str,
    model: &str,
    request: &CompletionRequest,
    response: &CompletionResponse,
) {
    warn_if_truncated_parts(
        backend,
        model,
        request.max_tokens,
        request.truncation_expected,
        response,
    );
}

/// [`warn_if_truncated`] for backends that dismantle the request before
/// the response exists (Gemini moves `stop` into the wire body, the
/// `OpenRouter` path destructures the whole request) — same tripwire,
/// pre-captured fields.
fn warn_if_truncated_parts(
    backend: &'static str,
    model: &str,
    cap: Option<u32>,
    truncation_expected: bool,
    response: &CompletionResponse,
) {
    if response.finish_reason == FinishReason::MaxTokens && !truncation_expected {
        tracing::warn!(
            backend,
            model,
            cap = ?cap,
            emitted = ?response.usage.completion_tokens,
            "llm reply hit the max_tokens ceiling — output truncated; a structured caller may misparse. Raise this call's cap"
        );
    }
}

/// [`warn_if_truncated`] for the chat surface. Chat callers set no tight
/// caps, so hitting the ceiling is always anomalous — no opt-out flag.
fn warn_if_truncated_chat(backend: &'static str, model: &str, response: &ChatResponse) {
    if response.finish_reason == FinishReason::MaxTokens {
        tracing::warn!(
            backend,
            model,
            emitted = ?response.usage.completion_tokens,
            "llm chat reply hit the max_tokens ceiling — output truncated"
        );
    }
}

/// Token-accounting bag returned alongside every completion. All
/// fields are best-effort — providers vary in what they expose.
#[derive(Debug, Clone, Default)]
pub struct CompletionUsage {
    /// Tokens consumed by the prompt (system + user). `None` when the
    /// backend does not report it.
    pub prompt_tokens: Option<u32>,
    /// Tokens emitted by the model. `None` when the backend does not
    /// report it.
    pub completion_tokens: Option<u32>,
}

/// One image riding a completion request (the vision path of the
/// media pipeline).
///
/// Carried per-request, never as backend state: the ingest slot is
/// shared by several callers and only the main ingest classify call may
/// attach images.
#[derive(Debug, Clone)]
pub struct ImageInput {
    /// MIME type (`image/jpeg`, `image/png`, …) — Anthropic's
    /// `media_type` / Gemini's `mimeType`; Ollama ignores it.
    pub mime_type: String,
    /// Base64-encoded image bytes (no data-URL prefix).
    pub data_base64: String,
}

/// Request shape every [`LlmBackend`] accepts. Builder-style so new
/// fields can be added without breaking implementations.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// The user-facing prompt to send to the model.
    pub prompt: String,
    /// Optional system / instruction message prepended to the
    /// conversation.
    pub system: Option<String>,
    /// Hard ceiling on generated tokens. `None` lets the backend
    /// apply its own default.
    pub max_tokens: Option<u32>,
    /// Sampling temperature in `[0.0, 2.0]`. `None` lets the backend
    /// apply its own default.
    pub temperature: Option<f32>,
    /// Stop sequences — generation halts when any is emitted.
    pub stop: Vec<String>,
    /// Images riding the user turn (empty for the text-only common
    /// case — every existing wire shape is unchanged when empty). A
    /// non-vision model ignores or degrades on these; the caller owns
    /// that risk (soft-fail doctrine).
    pub images: Vec<ImageInput>,
    /// The caller capped `max_tokens` deliberately tight and hitting
    /// the ceiling is part of the contract (health probes). Suppresses
    /// the central truncation warning — see [`warn_if_truncated`].
    pub truncation_expected: bool,
}

impl CompletionRequest {
    /// Construct a minimal request: only the user prompt is set,
    /// everything else takes the backend default.
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            system: None,
            max_tokens: None,
            temperature: None,
            stop: Vec::new(),
            images: Vec::new(),
            truncation_expected: false,
        }
    }

    /// Builder: declare that hitting the `max_tokens` ceiling is
    /// intentional for this call (a deliberately tiny probe cap), so
    /// the central truncation warning stays silent.
    #[must_use]
    pub const fn with_truncation_expected(mut self) -> Self {
        self.truncation_expected = true;
        self
    }

    /// Builder: set the system message.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Builder: cap the completion length.
    #[must_use]
    pub const fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Builder: pin the sampling temperature.
    #[must_use]
    pub const fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Builder: attach images to the user turn.
    #[must_use]
    pub fn with_images(mut self, images: Vec<ImageInput>) -> Self {
        self.images = images;
        self
    }
}

/// Response shape every [`LlmBackend`] returns.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// Generated text — already concatenated, never streamed.
    pub text: String,
    /// Why generation stopped.
    pub finish_reason: FinishReason,
    /// Token-accounting bag (best-effort).
    pub usage: CompletionUsage,
}

// ---------------------------------------------------------------------------
// Chat-with-tools API
//
// Separate from the single-prompt `complete` path so the existing
// structured callers (ingest, REM dedup) are untouched. Used by the
// dashboard agentic loop (LLM functions) to
// compose `mwe-core`'s `_internal.*` operations: the model receives a
// turn-by-turn message history plus a list of callable tools, decides
// whether to emit one or more `tool_calls`, and the dashboard
// executes them and feeds the results back as `Role::Tool` messages.
// ---------------------------------------------------------------------------

/// Role of a chat turn.
///
/// Mirrors the Ollama / `OpenAI` vocabulary so the Ollama transport
/// is a one-to-one mapping; `Tool` is the role for messages that
/// carry the *result* of a previous tool call (the `tool_call_id`
/// field links the result back to the request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Initial framing / instructions from the dashboard / caller.
    System,
    /// The end-user's turn.
    User,
    /// The model's turn — either a plain text reply or a request to
    /// invoke one or more tools (`tool_calls` populated).
    Assistant,
    /// Carries the result of executing a previous assistant tool
    /// call. `tool_call_id` ties the result back to the request.
    Tool,
}

impl Role {
    /// Lowercase serialisation token, matching the Ollama / `OpenAI`
    /// vocabulary expected on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A single turn in a chat exchange.
///
/// Three flavours of message, distinguished by `role`:
///
/// - `System` / `User`: `content` carries the text, `tool_calls` is
///   empty, `tool_call_id` is `None`.
/// - `Assistant`: either a plain text reply (`tool_calls` empty,
///   `content` populated) or a request to call one or more tools
///   (`tool_calls` non-empty; `content` may be empty or carry a
///   "thinking-aloud" prelude, depending on the model).
/// - `Tool`: the result of executing an assistant's tool call.
///   `tool_call_id` links the result back to the call, `content` is
///   the JSON-stringified result (or an error message), `tool_calls`
///   is empty.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Speaker of this turn.
    pub role: Role,
    /// Text content of the message. May be empty when the model
    /// produced only tool calls and no surrounding prose.
    pub content: String,
    /// Populated when `role == Assistant` and the model decided to
    /// dispatch one or more tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Populated when `role == Tool`. Identifies the assistant's
    /// `ToolCall.id` whose result this message carries.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Build a `System` message — the initial framing the dashboard
    /// hands to the model (instructions, available tools, persona).
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Build a `User` message — the verbatim turn typed by the
    /// dashboard user.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Build a plain-text `Assistant` message — the model's reply
    /// without tool calls. Use the struct literal directly when the
    /// turn also carries `tool_calls`.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Build a `Tool` message carrying the result of executing the
    /// assistant's `tool_call_id` with the JSON-encoded `result`.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: result.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Descriptor of a function-callable tool the model is allowed to invoke.
///
/// The `parameters` field is a JSON Schema object describing the
/// tool's arguments. The dashboard's `AgenticTool` registry produces
/// these descriptors from the whitelisted `_internal.*` operations
/// (LLM functions).
#[derive(Debug, Clone)]
pub struct Tool {
    /// Tool name. Must be unique per `ChatRequest`. Matched against
    /// `ToolCall.name` when the model decides to invoke.
    pub name: String,
    /// Human-readable description of what the tool does. The model
    /// uses this to pick between tools; write it carefully.
    pub description: String,
    /// JSON Schema for the tool's arguments. The model is expected to
    /// produce a JSON value matching this schema in
    /// `ToolCall.arguments`.
    pub parameters: serde_json::Value,
}

/// Request from the model to execute a tool. The dashboard runs the
/// matching `AgenticTool` against `arguments` and feeds the result
/// back as a `ChatMessage::tool_result(id, ...)` on the next turn.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Opaque identifier minted by the model. The dashboard echoes
    /// this back in the corresponding `ChatMessage.tool_call_id` so
    /// the model can correlate the result with the request.
    pub id: String,
    /// Name of the tool to invoke. Must match a `Tool.name` from the
    /// request's `tools` list.
    pub name: String,
    /// Arguments for the tool, as a JSON value matching the tool's
    /// `parameters` schema.
    pub arguments: serde_json::Value,
    /// Opaque provider "thought signature" (Gemini 3 with thinking)
    /// attached to a function-call part; MUST be echoed back verbatim
    /// when this call is replayed in a later request (omitting it is a
    /// hard 400 on Gemini). Captured on the inbound response, re-emitted
    /// by the backend. None for providers that don't use it.
    pub thought_signature: Option<String>,
}

/// Multi-turn chat request with optional tool descriptors.
///
/// The dashboard's agentic loop builds this incrementally: it starts
/// with `[System, User]`, calls `chat`, appends the resulting
/// `Assistant` message, optionally appends `Tool` results, and loops.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Ordered conversation history. The last turn is typically a
    /// `User` (initial call) or `Tool` (subsequent iterations of the
    /// agentic loop, carrying a tool result).
    pub messages: Vec<ChatMessage>,
    /// Tools the model is allowed to call. Empty = plain chat, no
    /// function calling.
    pub tools: Vec<Tool>,
    /// Hard ceiling on generated tokens (per turn). `None` lets the
    /// backend apply its own default.
    pub max_tokens: Option<u32>,
    /// Sampling temperature in `[0.0, 2.0]`. `None` lets the backend
    /// apply its own default.
    pub temperature: Option<f32>,
}

impl ChatRequest {
    /// Construct a request with the given message history and no
    /// tools. Useful for plain chat probes; the agentic loop populates
    /// `tools` directly.
    #[must_use]
    pub const fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
        }
    }

    /// Builder: attach the list of tools the model is allowed to
    /// invoke during this turn.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// Builder: cap the per-turn completion length.
    #[must_use]
    pub const fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Builder: pin the sampling temperature for the turn.
    #[must_use]
    pub const fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }
}

/// Response from a chat call.
///
/// The agentic loop branches on `message.tool_calls`: empty means the
/// model produced a final answer (`message.content`); non-empty means
/// the model wants to invoke tools before continuing — the loop
/// executes them and re-calls `chat` with the appended results.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The assistant turn the backend just produced.
    pub message: ChatMessage,
    /// Why generation stopped (end of turn, max-tokens hit, other).
    pub finish_reason: FinishReason,
    /// Token-accounting bag (best-effort, provider-dependent).
    pub usage: CompletionUsage,
}

/// Errors raised by an [`LlmBackend`].
#[derive(Debug, Error)]
pub enum LlmError {
    /// Backend rejected the input (unknown model, prompt too long,
    /// bad params). Non-retriable without changing the request.
    #[error("llm invalid request: {0}")]
    Invalid(String),
    /// Transport-level failure (DNS, TCP, TLS, timeout). Retriable.
    #[error("llm transport error: {0}")]
    Transport(String),
    /// Backend returned a 5xx or otherwise malformed response.
    #[error("llm backend error: {0}")]
    Backend(String),
    /// Protocol mismatch (unparseable response).
    #[error("llm protocol error: {0}")]
    Protocol(String),
    /// Cloud provider returned 429 Too Many Requests. Retriable after
    /// the documented back-off window. Surfaced as a distinct variant
    /// so the REM scheduler's fatal-error path can decide to
    /// abort the cycle on auth/protocol errors but keep retrying on
    /// rate limits.
    #[error("llm rate-limited: {0}")]
    RateLimit(String),
    /// Cloud provider returned 401/403 — the API key is missing,
    /// malformed, or revoked. Non-retriable; the operator must fix
    /// `mwe-mcp.env`. The error message names the offending env-var
    /// so the operator can find it without digging into config.
    #[error("llm auth error: {0}")]
    Auth(String),
}

/// Result alias for LLM operations.
pub type Result<T> = std::result::Result<T, LlmError>;

/// Map a transport-layer `reqwest` failure to [`LlmError::Transport`],
/// dropping the request URL from the message: a provider URL can carry
/// the API key in its query string (Gemini's `?key=...`), and transport
/// errors are logged verbatim by every caller.
fn transport_error(e: reqwest::Error) -> LlmError {
    LlmError::Transport(e.without_url().to_string())
}

/// Contract every LLM backend honours.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Stable identifier of the underlying model. Used as part of
    /// audit logs + cost tracking so a deployment can correlate
    /// completions with model versions.
    fn model_id(&self) -> &str;

    /// Run a single non-streaming completion.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;

    /// Run a multi-turn chat exchange, optionally with function-callable
    /// tools. Used by the dashboard agentic loop (LLM functions)
    /// where the model alternates between text replies and tool calls.
    ///
    /// Backends that do not support function calling — currently every
    /// non-Ollama provider in this codebase — return
    /// [`LlmError::Backend`] with a descriptive message. Callers that
    /// depend on tools must check at startup that the configured
    /// `hub_writer` slot supports `chat`; the dashboard does this via
    /// the boot-time health check path.
    ///
    /// # Errors
    ///
    /// Returns whatever the underlying transport / API produces. The
    /// dashboard agentic loop surfaces this to the user as a chat
    /// panel error bubble (no silent fallback).
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Err(LlmError::Backend(format!(
            "backend `{}` does not implement the chat / function-calling API",
            self.model_id()
        )))
    }

    /// Sanity check that this backend is reachable and the configured
    /// model responds. Called at `mwe-mcp serve` boot
    /// (LLM functions) for every
    /// configured slot so the server refuses to bind the listener
    /// when an LLM is misconfigured or unreachable; called on-demand
    /// by `mwe-mcp doctor` for the same check post-deploy.
    ///
    /// Default implementation: issue a `complete` with a 1-token cap.
    /// Backends that have a cheaper liveness probe (e.g. an
    /// `/api/version` endpoint that does not warm a model) override
    /// this method.
    ///
    /// # Errors
    ///
    /// Returns whatever the underlying transport / API surface
    /// produces. The caller (boot path, doctor) renders the error to
    /// the operator.
    async fn health_check(&self) -> Result<()> {
        // No pinned `temperature` (some models reject sampling params with
        // a 400) and a small-but-non-trivial `max_tokens` (a `max_tokens: 1`
        // probe can return zero content blocks). Backends with stricter
        // needs override this (see `AnthropicBackend::health_check`).
        let probe = CompletionRequest::new("ping")
            .with_max_tokens(16)
            .with_truncation_expected();
        let _ = self.complete(probe).await?;
        Ok(())
    }
}

/// HTTP client for the Ollama generate API.
///
/// Posts to `{base_url}/api/generate` with `stream: false` and the
/// fields Ollama supports (model, prompt, system, options). The body
/// shape is documented at <https://github.com/ollama/ollama/blob/main/docs/api.md>.
pub struct OllamaBackend {
    client: Client,
    base_url: String,
    model: String,
    /// Optional Bearer token for a remote / cloud / proxied Ollama
    /// (`Authorization: Bearer …`). `None` for a local daemon, which
    /// needs no auth.
    auth_token: Option<String>,
}

impl OllamaBackend {
    /// Build a fresh backend pointing at `base_url` with `model` as the
    /// default model. `base_url` should not include a path suffix.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            model: model.into(),
            auth_token: None,
        })
    }

    /// Attach an optional Bearer token — used for Ollama Cloud or a
    /// self-hosted daemon behind an authenticating proxy. A `None` or
    /// blank value leaves the backend unauthenticated (the local case).
    #[must_use]
    pub fn with_bearer(mut self, token: Option<String>) -> Self {
        self.auth_token = token.filter(|t| !t.trim().is_empty());
        self
    }

    /// Shortcut for "Ollama on localhost with `model`".
    pub fn local(model: impl Into<String>) -> Result<Self> {
        Self::new(DEFAULT_OLLAMA_URL, model)
    }

    /// Apply the configured Bearer token to a request, if any.
    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(token) => rb.bearer_auth(token),
            None => rb,
        }
    }

    /// Names + sizes of the models installed on the daemon
    /// (`GET /api/tags`), for the dashboard model picker. Best-effort: a
    /// caller treats any error as "no suggestions" and falls back to
    /// free-text entry.
    ///
    /// # Errors
    ///
    /// [`LlmError::Transport`] if the daemon is unreachable,
    /// [`LlmError::Backend`] on a non-2xx status, [`LlmError::Protocol`]
    /// on a malformed body.
    pub async fn list_models(&self) -> Result<Vec<OllamaModelInfo>> {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let response = self
            .with_auth(self.client.get(url))
            .send()
            .await
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(LlmError::Backend(format!(
                "ollama /api/tags returned HTTP {}",
                response.status()
            )));
        }
        let parsed: OllamaTagsResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding /api/tags: {e}")))?;
        Ok(parsed
            .models
            .into_iter()
            .map(|m| OllamaModelInfo {
                name: m.name,
                size: m.size,
            })
            .collect())
    }
}

/// One model installed on an Ollama daemon, surfaced to the dashboard
/// model picker by [`OllamaBackend::list_models`].
#[derive(Debug, Clone)]
pub struct OllamaModelInfo {
    /// The exact tag to drop into a role's `model` field (e.g.
    /// `qwen3.5:9b-q8_0`).
    pub name: String,
    /// On-disk size in bytes when the daemon reports it — rendered as a
    /// human hint next to the name.
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagModel {
    name: String,
    #[serde(default)]
    size: Option<u64>,
}

// ---------------------------------------------------------------------------
// Ollama chat-API wire types
//
// Ollama 0.4+ exposes function calling on `POST /api/chat`. The body
// shape we serialise here matches that endpoint; `OllamaGenerateRequest`
// below is still used by the single-prompt `complete` path.
// See <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion>.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    /// See `OllamaGenerateRequest.think` — same reasoning, same hard
    /// `false` until a UI surface for the reasoning lands.
    think: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OllamaToolDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaChatMessage {
    role: &'static str,
    content: String,
    /// Populated only for assistant messages that requested tool calls.
    /// Sent back to the model when re-issuing the conversation so it
    /// can correlate previous reasoning with the tool results that
    /// follow.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OllamaToolCallOut>,
}

#[derive(Debug, Serialize)]
struct OllamaToolDescriptor {
    /// Always `"function"` for the Ollama tools API.
    #[serde(rename = "type")]
    kind: &'static str,
    function: OllamaToolFunction,
}

#[derive(Debug, Serialize)]
struct OllamaToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OllamaToolCallOut {
    function: OllamaToolCallFunctionOut,
}

#[derive(Debug, Serialize)]
struct OllamaToolCallFunctionOut {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    #[serde(default)]
    message: Option<OllamaChatResponseMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCallIn>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCallIn {
    function: OllamaToolCallFunctionIn,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCallFunctionIn {
    name: String,
    /// Ollama serialises `arguments` as a JSON object directly when the
    /// model uses the `parameters` schema correctly; older or stricter
    /// servers stringify it. Accept both — we normalise downstream.
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    stream: bool,
    /// Whether the model is allowed to emit `<think>` blocks.
    /// Forced `false` system-wide: thinking-capable models
    /// (Qwen 3.x, etc.) otherwise consume their `num_predict` budget
    /// inside the reasoning block and return an empty `response`,
    /// which breaks every structured caller (ingest, REM dedup) that
    /// expects parseable output. The dashboard UI for surfacing the
    /// reasoning to the user is deferred — when it lands this field
    /// becomes per-call configurable per LLM functions.
    think: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    stop: &'a [String],
    /// Bare base64 images for vision models (the Ollama generate API's
    /// multimodal field). Absent on the wire when empty so every
    /// text-only body — and every wiremock literal matcher — is
    /// byte-identical to before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

fn options_for(max_tokens: Option<u32>, temperature: Option<f32>) -> Option<OllamaOptions> {
    if max_tokens.is_some() || temperature.is_some() {
        Some(OllamaOptions {
            num_predict: max_tokens.and_then(|n| i32::try_from(n).ok()),
            temperature,
        })
    } else {
        None
    }
}

fn serialise_chat_messages(messages: &[ChatMessage]) -> Vec<OllamaChatMessage> {
    messages
        .iter()
        .map(|m| OllamaChatMessage {
            role: m.role.as_str(),
            content: m.content.clone(),
            tool_calls: m
                .tool_calls
                .iter()
                .map(|tc| OllamaToolCallOut {
                    function: OllamaToolCallFunctionOut {
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    },
                })
                .collect(),
        })
        .collect()
}

fn serialise_tool_descriptors(tools: &[Tool]) -> Vec<OllamaToolDescriptor> {
    tools
        .iter()
        .map(|t| OllamaToolDescriptor {
            kind: "function",
            function: OllamaToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

fn parse_chat_response(parsed: OllamaChatResponse) -> Result<ChatResponse> {
    let raw_message = parsed
        .message
        .ok_or_else(|| LlmError::Protocol("ollama chat response is missing `message`".into()))?;
    let content = raw_message.content.unwrap_or_default();
    let tool_calls: Vec<ToolCall> = raw_message
        .tool_calls
        .into_iter()
        .enumerate()
        .map(|(idx, raw)| ToolCall {
            id: format!("call_{idx}"),
            name: raw.function.name,
            arguments: raw.function.arguments,
            // Ollama has no thought-signature concept.
            thought_signature: None,
        })
        .collect();
    Ok(ChatResponse {
        message: ChatMessage {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
        },
        finish_reason: classify_finish_reason(parsed.done_reason.as_deref()),
        usage: CompletionUsage {
            prompt_tokens: parsed.prompt_eval_count,
            completion_tokens: parsed.eval_count,
        },
    })
}

const fn classify_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some(s) if matches!(s.as_bytes(), b"stop" | b"end_of_turn") => FinishReason::EndOfTurn,
        Some(s) if matches!(s.as_bytes(), b"length" | b"max_tokens") => FinishReason::MaxTokens,
        _ => FinishReason::Other,
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        if request.prompt.is_empty() {
            return Err(LlmError::Invalid("empty prompt".into()));
        }

        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let options = if request.max_tokens.is_some() || request.temperature.is_some() {
            Some(OllamaOptions {
                num_predict: request.max_tokens.and_then(|n| i32::try_from(n).ok()),
                temperature: request.temperature,
            })
        } else {
            None
        };
        let body = OllamaGenerateRequest {
            model: &self.model,
            prompt: &request.prompt,
            system: request.system.as_deref(),
            stream: false,
            think: false,
            options,
            stop: &request.stop,
            images: request
                .images
                .iter()
                .map(|i| i.data_base64.clone())
                .collect(),
        };

        let response = self
            .with_auth(self.client.post(url).json(&body))
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Backend(format!(
                "HTTP {status}: {}",
                body.chars().take(500).collect::<String>()
            )));
        }
        let parsed: OllamaGenerateResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding response: {e}")))?;
        if !parsed.done {
            tracing::debug!("Ollama returned a non-final response — treating as done anyway");
        }

        let resp = CompletionResponse {
            text: parsed.response,
            finish_reason: classify_finish_reason(parsed.done_reason.as_deref()),
            usage: CompletionUsage {
                prompt_tokens: parsed.prompt_eval_count,
                completion_tokens: parsed.eval_count,
            },
        };
        warn_if_truncated("ollama", &self.model, &request, &resp);
        Ok(resp)
    }

    /// Multi-turn chat with optional function-callable tools, posted
    /// to `POST /api/chat` on the Ollama daemon. Used by the dashboard
    /// agentic loop (LLM functions) where the
    /// model alternates between text replies and tool invocations.
    ///
    /// Behaviour notes:
    ///
    /// - `stream: false` matches the `complete` path: one round-trip
    ///   per turn, no chunked streaming. The agentic loop runs many
    ///   turns; streaming would be observable to the user but it is
    ///   a later polish.
    /// - `think: false` is hardcoded, same reasoning as `complete`.
    /// - Tool-call IDs are minted client-side from a monotonic per-call
    ///   counter — Ollama does not generate them. The dashboard agentic
    ///   loop echoes the same id back in the matching `Role::Tool`
    ///   message so it can correlate results.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.messages.is_empty() {
            return Err(LlmError::Invalid("empty messages".into()));
        }

        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let body = OllamaChatRequest {
            model: &self.model,
            messages: serialise_chat_messages(&request.messages),
            stream: false,
            think: false,
            tools: serialise_tool_descriptors(&request.tools),
            options: options_for(request.max_tokens, request.temperature),
        };

        let response = self
            .with_auth(self.client.post(url).json(&body))
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Backend(format!(
                "HTTP {status}: {}",
                body.chars().take(500).collect::<String>()
            )));
        }
        let parsed: OllamaChatResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding chat response: {e}")))?;
        if !parsed.done {
            tracing::debug!("Ollama chat returned a non-final response — treating as done anyway");
        }
        let resp = parse_chat_response(parsed)?;
        warn_if_truncated_chat("ollama", &self.model, &resp);
        Ok(resp)
    }

    /// Cheaper liveness probe for Ollama: hits `/api/version` instead
    /// of running an actual completion (which would warm the model
    /// into RAM). Used at `mwe-mcp serve` boot per
    /// LLM functions.
    ///
    /// Only confirms the daemon is reachable; the configured model is
    /// not exercised here. A follow-up `complete` against an
    /// unloaded model will still error out — we rely on REM's fatal
    /// LLM-error path to surface that. The version probe
    /// is the cheap pre-boot sanity check, not a full smoke test.
    async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/version", self.base_url.trim_end_matches('/'));
        let response = self
            .with_auth(self.client.get(url))
            .send()
            .await
            .map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(LlmError::Backend(format!(
                "ollama /api/version returned HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AnthropicBackend — HTTP client for the Anthropic Messages API
// <https://docs.anthropic.com/en/api/messages>. Authenticates via the
// `x-api-key` header; pins the wire version with `anthropic-version`.
//
// Why a separate struct rather than a config knob on `OllamaBackend`:
// the wire shapes diverge enough (single `content` blocks list, no
// per-message `tool_calls`, mandatory `max_tokens`, tool calling via
// typed content blocks instead of a sibling `tool_calls` field) that
// flattening them behind a shared transport would obscure both.
//
// `keep_alive` is not applicable — Anthropic has no per-client model
// warm-up concept. `format: "json"` is not applicable — Anthropic
// has no native grammar constraint; structured callers (ingest, REM)
// use robust parsers downstream. `think` is not applicable — that
// is a Qwen-only Ollama knob.
// ---------------------------------------------------------------------------

/// Default base URL for the Anthropic Messages API. Override via
/// [`AnthropicBackend::with_base_url`] in tests or for self-hosted
/// gateways (e.g. Bedrock proxy, regional endpoints).
pub const DEFAULT_ANTHROPIC_URL: &str = "https://api.anthropic.com";

/// Wire version pinned by every outgoing request. The Anthropic
/// Messages API requires this header; `2023-06-01` is the stable
/// long-lived version that covers the body shape we send.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// API key for an [`AnthropicBackend`], `Debug`-redacted.
///
/// Same pattern as [`crate::jwt::TokenSecret`]. Holding the key in a
/// newtype keeps it from leaking through `tracing` field-debug or
/// `?` formatting on the backend struct.
#[derive(Clone)]
pub struct AnthropicApiKey(String);

impl AnthropicApiKey {
    /// Wrap a raw API key string. Empty input is rejected so a
    /// misread env-var surfaces at construction time instead of as
    /// an opaque 401 on first request.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Invalid`] when `raw` is empty after
    /// trimming surrounding whitespace.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let s = raw.into();
        if s.trim().is_empty() {
            return Err(LlmError::Invalid("anthropic api key is empty".into()));
        }
        Ok(Self(s))
    }

    /// Expose the raw key bytes — only the HTTP layer needs this.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AnthropicApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicApiKey")
            .field("len", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

/// System-prompt identity required on Anthropic OAuth (Claude Code
/// subscription) requests: the Messages API rejects an OAuth credential
/// unless the first `system` block declares the caller is Claude Code.
/// Our own (REM / ingest) system prompt follows it. **Test/personal use
/// only** — see [`is_anthropic_oauth_token`].
const CLAUDE_CODE_SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `anthropic-beta` header sent on OAuth requests — the set the Claude
/// Code CLI sends, so Anthropic routes the subscription traffic.
const ANTHROPIC_OAUTH_BETAS: &str = "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14,claude-code-20250219,oauth-2025-04-20";

/// `claude-cli/<version>` reported when the local `claude` binary cannot
/// be probed. Anthropic rejects OAuth requests whose spoofed user-agent
/// version is too far behind the live release, so [`claude_code_user_agent`]
/// prefers the installed CLI's own version.
const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";

/// Classify an Anthropic credential by prefix: `true` ⇒ an OAuth / Claude
/// Code subscription token (Bearer auth + Claude Code identity), `false` ⇒
/// a Console API key (`x-api-key`). Mirrors the Claude Code CLI's own
/// classification — `sk-ant-api…` is always a Console key; `sk-ant-…`
/// (setup tokens `sk-ant-oat-…`), `eyJ…` (OAuth JWTs) and `cc-…` (Claude
/// Code access tokens) are OAuth; anything else is treated as a key.
///
/// How an operator opts into subscription auth: drop a Claude Code token
/// into the slot's `api_key_env` (e.g. point it at `CLAUDE_CODE_OAUTH_TOKEN`)
/// and the backend routes by value — no config-schema change. Intended for
/// **local/personal testing on your own subscription**, never a deployed
/// product (it presents as the CLI; buyers bring their own Console keys).
fn is_anthropic_oauth_token(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() || token.starts_with("sk-ant-api") {
        return false;
    }
    token.starts_with("sk-ant-") || token.starts_with("eyJ") || token.starts_with("cc-")
}

/// Probe the installed Claude Code version (`claude --version`, then
/// `claude-code --version`), falling back to [`CLAUDE_CODE_VERSION_FALLBACK`].
fn detect_claude_code_version() -> String {
    for cmd in ["claude", "claude-code"] {
        let Ok(output) = std::process::Command::new(cmd).arg("--version").output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(stdout) = String::from_utf8(output.stdout) else {
            continue;
        };
        // Output is like "2.1.74 (Claude Code)" or just "2.1.74".
        if let Some(version) = stdout.split_whitespace().next()
            && version.starts_with(|c: char| c.is_ascii_digit())
        {
            return version.to_owned();
        }
    }
    CLAUDE_CODE_VERSION_FALLBACK.to_owned()
}

/// Cached `user-agent` for OAuth requests, e.g.
/// `claude-cli/2.1.74 (external, cli)`. Probed once, process-wide.
/// Shared with [`crate::oauth`] for the token-exchange / refresh calls.
pub(crate) fn claude_code_user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| {
        format!(
            "claude-cli/{} (external, cli)",
            detect_claude_code_version()
        )
    })
    .as_str()
}

/// Apply the credential-specific auth + identity headers to an Anthropic
/// request: `x-api-key` on the Console-key path, or `Authorization: Bearer`
/// plus the Claude Code fingerprint (betas, user-agent, `x-app`) on the OAuth
/// path. `token` is the already-resolved Console key or access token.
fn apply_anthropic_auth(
    req: reqwest::RequestBuilder,
    token: &str,
    oauth: bool,
) -> reqwest::RequestBuilder {
    if oauth {
        req.header("authorization", format!("Bearer {token}"))
            .header("anthropic-beta", ANTHROPIC_OAUTH_BETAS)
            .header("user-agent", claude_code_user_agent())
            .header("x-app", "cli")
    } else {
        req.header("x-api-key", token)
    }
}

/// HTTP client for the Anthropic Messages API.
///
/// Two paths today:
///
/// - `complete` packages [`CompletionRequest::system`] as the
///   top-level `system` field and [`CompletionRequest::prompt`] as a
///   single `user` message.
/// - `chat` issues the same `POST /v1/messages` but with the full
///   message history converted to Anthropic shape (system stripped,
///   tool calls/results encoded as typed content blocks). Tool calls
///   round-trip via the Anthropic-minted `tool_use.id`, mirrored back
///   into [`ToolCall::id`].
///
/// Operator-relevant behaviour:
///
/// - `max_tokens` is **required** by the Anthropic API. When the
///   caller leaves it unset, the backend uses
///   [`Self::DEFAULT_MAX_TOKENS`] so the request never fails before
///   it leaves the process. Structured callers (ingest, REM) cap it
///   explicitly upstream; the default is a safety net for ad-hoc use.
/// - 401/403 are surfaced as [`LlmError::Auth`] with the env-var name
///   in the message, so an operator with a missing `ANTHROPIC_API_KEY`
///   sees what to fix without grepping logs.
/// - 429 is surfaced as [`LlmError::RateLimit`] (retriable distinct
///   from generic [`LlmError::Backend`]).
pub struct AnthropicBackend {
    client: Client,
    base_url: String,
    model: String,
    /// How requests authenticate: a static env-resolved key/token, or the
    /// Claude Code login store (resolved + refreshed per request).
    credential: AnthropicCredential,
    /// Name of the env-var that originally carried the key. Used only
    /// to enrich the auth-error message — never compared against the
    /// process environment at runtime (the config layer already
    /// resolved it to a string).
    api_key_env: String,
    /// Extended-thinking budget in tokens, mapped from the slot's
    /// `reasoning_effort` by [`Self::with_reasoning_effort`]. `None` (the
    /// default) sends no `thinking` field — the model answers without an
    /// extended reasoning pass, exactly as before this knob existed.
    /// `Some(b)` enables `thinking: { type: "enabled", budget_tokens: b }`
    /// on the single-shot `complete` path only; `chat` is deliberately
    /// excluded (see [`LlmBackend::chat`] for the round-trip reason).
    thinking_budget: Option<u32>,
}

/// How an [`AnthropicBackend`] authenticates.
enum AnthropicCredential {
    /// A static credential resolved once from an env var: a Console API key
    /// (`x-api-key`) or a long-lived OAuth / setup token (`Bearer`). `oauth`
    /// is the [`is_anthropic_oauth_token`] classification of the value.
    Static { token: AnthropicApiKey, oauth: bool },
    /// The Claude Code login store: the access token is read (and refreshed
    /// when near expiry) from the workdir on every request. Always OAuth.
    /// **Test/personal use only.**
    Login(std::sync::Arc<crate::oauth::OauthStore>),
}

impl AnthropicCredential {
    /// Whether requests use the OAuth (`Bearer` + Claude Code identity) path.
    const fn is_oauth(&self) -> bool {
        match self {
            Self::Static { oauth, .. } => *oauth,
            Self::Login(_) => true,
        }
    }

    /// Resolve the current bearer / api-key token, refreshing a login token
    /// transparently when it is near expiry.
    async fn resolve(&self) -> Result<String> {
        match self {
            Self::Static { token, .. } => Ok(token.as_str().to_owned()),
            Self::Login(store) => store
                .resolve_access_token()
                .await
                .map_err(|e| LlmError::Auth(format!("claude code login: {e}"))),
        }
    }
}

impl AnthropicBackend {
    /// Floor on `max_tokens` applied when the caller does not pin one
    /// explicitly. Picked so a short structured response (intent JSON,
    /// dedup yes/no) fits comfortably; long-form generation (REM hub
    /// rewrites, cronista) caps higher upstream.
    pub const DEFAULT_MAX_TOKENS: u32 = 1024;

    /// Build a backend pointing at the canonical Anthropic endpoint.
    /// Pass the env-var name you read `api_key` from so auth errors
    /// can name it back to the operator.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Transport`] when the underlying `reqwest`
    /// client cannot be constructed (TLS init, runtime issues).
    pub fn new(
        api_key: AnthropicApiKey,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(transport_error)?;
        let oauth = is_anthropic_oauth_token(api_key.as_str());
        Ok(Self {
            client,
            base_url: DEFAULT_ANTHROPIC_URL.to_owned(),
            model: model.into(),
            credential: AnthropicCredential::Static {
                token: api_key,
                oauth,
            },
            api_key_env: api_key_env.into(),
            thinking_budget: None,
        })
    }

    /// Build a backend that authenticates via the Claude Code **login store**
    /// (`<workdir>/anthropic_oauth.json`): the access token is resolved — and
    /// refreshed when near expiry — from the store on every request, rather
    /// than read once from an env var. `api_key_env` is only a label for
    /// auth-error messages here.
    ///
    /// **Test / personal use only.** See [`crate::oauth`].
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Transport`] when the `reqwest` client cannot be built.
    pub fn with_login_store(
        store: std::sync::Arc<crate::oauth::OauthStore>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            base_url: DEFAULT_ANTHROPIC_URL.to_owned(),
            model: model.into(),
            credential: AnthropicCredential::Login(store),
            api_key_env: api_key_env.into(),
            thinking_budget: None,
        })
    }

    /// Builder: replace the default base URL — used by tests against
    /// wiremock and by self-hosted gateways. The string must not have
    /// a trailing slash; the backend joins paths verbatim.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Map the slot's optional `reasoning_effort` hint onto an Anthropic
    /// extended-thinking `budget_tokens` value, applied on the `complete`
    /// path (see `anthropic_thinking_budget` for the table). Anthropic
    /// has no `minimal` tier — its budget floor is 1024 — so unset /
    /// `minimal` disable thinking entirely, preserving the pre-knob
    /// behaviour for slots that do not ask for it.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        self.thinking_budget = anthropic_thinking_budget(effort);
        self
    }

    /// Build the `system` field for a request. On the OAuth path the Claude
    /// Code identity must be the first system block (the API rejects the
    /// credential otherwise) and the caller's own prompt follows it. On the
    /// API-key path the prompt is sent verbatim as a plain string — the
    /// legacy wire format, no identity prefix (it buys nothing for our
    /// structured task prompts and is not required there).
    fn build_system<'a>(&self, system: Option<&'a str>) -> Option<AnthropicSystem<'a>> {
        if !self.credential.is_oauth() {
            return system.map(AnthropicSystem::Text);
        }
        let mut blocks = vec![AnthropicSystemBlock {
            kind: "text",
            text: CLAUDE_CODE_SYSTEM_PREFIX,
        }];
        if let Some(text) = system.filter(|s| !s.is_empty()) {
            blocks.push(AnthropicSystemBlock { kind: "text", text });
        }
        Some(AnthropicSystem::Blocks(blocks))
    }

    /// Shared body of [`LlmBackend::complete`], parameterised on the
    /// extended-thinking `budget` so [`LlmBackend::health_check`] can
    /// force it off (`None`): the liveness probe must not engage the
    /// ≥1024-token thinking floor on a slot that configured an effort.
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive request lifecycle: build body, auth, send, parse"
    )]
    async fn complete_inner(
        &self,
        request: CompletionRequest,
        budget: Option<u32>,
    ) -> Result<CompletionResponse> {
        if request.prompt.is_empty() {
            return Err(LlmError::Invalid("empty prompt".into()));
        }

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        // Text-only requests keep the plain-string content ("the simple
        // path stays simple on the wire"); images switch the user turn to
        // content blocks — image blocks first, then the prompt text.
        let content = if request.images.is_empty() {
            AnthropicMessageContent::Text(&request.prompt)
        } else {
            let mut blocks: Vec<AnthropicContentBlockOut<'_>> = request
                .images
                .iter()
                .map(|img| AnthropicContentBlockOut::Image {
                    source: AnthropicImageSource {
                        kind: "base64",
                        media_type: &img.mime_type,
                        data: &img.data_base64,
                    },
                })
                .collect();
            blocks.push(AnthropicContentBlockOut::Text {
                text: &request.prompt,
            });
            AnthropicMessageContent::Blocks(blocks)
        };
        // Forward the caller's temperature, except to models that reject
        // sampling params outright (Opus 4.7+, Fable / Mythos: a 400). A
        // thinking budget stacks on the caller's output ceiling (keeping
        // `budget_tokens < max_tokens`) and forces temperature off too —
        // the API rejects a custom temperature alongside thinking.
        let caller_max = request.max_tokens.unwrap_or(Self::DEFAULT_MAX_TOKENS);
        let plain_temperature = if anthropic_rejects_sampling_params(&self.model) {
            None
        } else {
            request.temperature
        };
        let (max_tokens, thinking, temperature) =
            budget.map_or((caller_max, None, plain_temperature), |budget_tokens| {
                (
                    caller_max.saturating_add(budget_tokens),
                    Some(AnthropicThinking {
                        kind: "enabled",
                        budget_tokens,
                    }),
                    None,
                )
            });
        let body = AnthropicMessagesRequest {
            model: &self.model,
            max_tokens,
            messages: vec![AnthropicMessage {
                role: "user",
                content,
            }],
            system: self.build_system(request.system.as_deref()),
            temperature,
            thinking,
            stop_sequences: &request.stop,
        };

        let token = self.credential.resolve().await?;
        let response =
            apply_anthropic_auth(self.client.post(url), &token, self.credential.is_oauth())
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let trimmed = body_text.chars().take(500).collect::<String>();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::Auth(format!(
                    "HTTP {status} from Anthropic — check `{}` in mwe-mcp.env: {trimmed}",
                    self.api_key_env,
                )),
                429 => LlmError::RateLimit(format!("HTTP {status}: {trimmed}")),
                400 => LlmError::Invalid(format!("HTTP {status}: {trimmed}")),
                _ => LlmError::Backend(format!("HTTP {status}: {trimmed}")),
            });
        }

        let parsed: AnthropicMessagesResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding anthropic response: {e}")))?;

        let mut text = String::new();
        let mut saw_text_block = false;
        for block in &parsed.content {
            if let AnthropicContentBlockIn::Text { text: Some(t) } = block {
                text.push_str(t);
                saw_text_block = true;
            } else if matches!(block, AnthropicContentBlockIn::Text { text: None }) {
                saw_text_block = true;
            }
        }

        if !saw_text_block {
            return Err(LlmError::Protocol(
                "anthropic response has no `text` content block".into(),
            ));
        }

        let resp = CompletionResponse {
            text,
            finish_reason: classify_anthropic_stop_reason(parsed.stop_reason.as_deref()),
            usage: CompletionUsage {
                prompt_tokens: parsed.usage.as_ref().and_then(|u| u.input_tokens),
                completion_tokens: parsed.usage.as_ref().and_then(|u| u.output_tokens),
            },
        };
        warn_if_truncated("anthropic", &self.model, &request, &resp);
        Ok(resp)
    }
}

/// Translate a slot's `reasoning_effort` string into an Anthropic
/// extended-thinking budget, used by
/// [`AnthropicBackend::with_reasoning_effort`].
///
/// | `reasoning_effort` | `budget_tokens` |
/// |---|---|
/// | unset / `""` / `minimal` | none (thinking off) |
/// | `medium` | 4096 |
/// | `high` | 8192 |
/// | `extra-high` | 16384 |
/// | `low` **and any other value (incl. typos)** | 2048 |
///
/// Anthropic has no sub-1024 "minimal" thinking, so unset / `minimal`
/// turn thinking off rather than mapping to a tiny budget. The
/// unknown-value floor is `low` (2048), mirroring the Gemini adapter's
/// stance that a misspelt effort still buys *some* reasoning rather than
/// silently none.
fn anthropic_thinking_budget(effort: Option<&str>) -> Option<u32> {
    match effort.map(str::trim) {
        None | Some("" | "minimal") => None,
        Some("medium") => Some(4_096),
        Some("high") => Some(8_192),
        Some("extra-high") => Some(16_384),
        Some(_) => Some(2_048),
    }
}

/// `true` for Anthropic models that reject the sampling parameters
/// (`temperature`, `top_p`, `top_k`) outright — sending any returns
/// HTTP 400 `invalid_request_error`. Opus 4.7+ and the Fable / Mythos
/// family removed them in favour of adaptive thinking + effort; Sonnet
/// 4.6, Opus 4.6, Haiku and the 3.x line still accept `temperature`.
///
/// Matched on the family prefix so point releases are covered. A newer
/// family that drops sampling params must be added here — but a miss is
/// not fatal: the boot health-check pins no temperature, so an unlisted
/// model only risks a 400 on a *deliberately configured* `temperature`,
/// never on boot.
fn anthropic_rejects_sampling_params(model: &str) -> bool {
    let m = model.trim();
    m.starts_with("claude-opus-4-7")
        || m.starts_with("claude-opus-4-8")
        || m.starts_with("claude-fable")
        || m.starts_with("claude-mythos")
}

#[derive(Debug, Serialize)]
struct AnthropicMessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<AnthropicSystem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    stop_sequences: &'a [String],
}

/// Extended-thinking control block, emitted only when the slot's
/// `reasoning_effort` maps to a budget (see [`anthropic_thinking_budget`]).
/// `budget_tokens` must be `< max_tokens`; the `complete` path guarantees
/// this by stacking the budget on top of the caller's output ceiling.
#[derive(Debug, Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

/// The `system` field: a plain string on the API-key path, or a list of
/// text blocks on the OAuth path (so the Claude Code identity can lead).
/// Untagged so `Text` serializes exactly as the legacy bare string — the
/// API-key wire format is byte-for-byte unchanged.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicSystem<'a> {
    Text(&'a str),
    Blocks(Vec<AnthropicSystemBlock<'a>>),
}

#[derive(Debug, Serialize)]
struct AnthropicSystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage<'a> {
    role: &'static str,
    content: AnthropicMessageContent<'a>,
}

/// Anthropic accepts the `content` of a message either as a plain
/// string (legacy) or as a typed list of content blocks (required
/// once tool use enters the picture). We always send blocks for
/// `chat` (so `tool_use` / `tool_result` round-trip cleanly), and a plain
/// string for `complete` (so the simple path stays simple on the
/// wire).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicMessageContent<'a> {
    Text(&'a str),
    Blocks(Vec<AnthropicContentBlockOut<'a>>),
}

/// Outbound content block. `text` blocks carry the prose; `tool_use`
/// blocks carry an assistant's prior tool-call request (echoed back
/// to preserve conversation symmetry); `tool_result` blocks carry the
/// dashboard's reply to that call; `image` blocks carry base64 media
/// on the vision path of the completion call.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockOut<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
    },
    Image {
        source: AnthropicImageSource<'a>,
    },
}

/// The `source` payload of an `image` content block.
#[derive(Debug, Serialize)]
struct AnthropicImageSource<'a> {
    /// Always `"base64"` — the only source kind the completion path uses.
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'a str,
    data: &'a str,
}

/// Inbound `tools` descriptor — Anthropic's shape mirrors `OpenAI`'s
/// function tools but the JSON Schema lives at `input_schema` (not
/// `parameters`).
#[derive(Debug, Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<AnthropicSystem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessagesResponse {
    #[serde(default)]
    content: Vec<AnthropicContentBlockIn>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlockIn {
    Text {
        #[serde(default)]
        text: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Future-proofing: any new block type Anthropic introduces
    /// (e.g. `thinking`) decodes into this catch-all and is ignored.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

const fn classify_anthropic_stop_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some(s) if matches!(s.as_bytes(), b"end_turn" | b"stop_sequence" | b"tool_use") => {
            FinishReason::EndOfTurn
        },
        Some(s) if matches!(s.as_bytes(), b"max_tokens") => FinishReason::MaxTokens,
        _ => FinishReason::Other,
    }
}

/// Translate a `ChatMessage` history into Anthropic's body shape.
///
/// Anthropic carries the system prompt as a top-level field instead
/// of a message, so any `Role::System` entry is stripped here and
/// surfaced to the caller via the returned `Option<String>`. Multiple
/// `System` messages are concatenated with a blank line between them
/// to preserve the document order; the agentic loop only sends one
/// today, but the contract should not silently drop the rest.
///
/// `Role::Tool` messages become `user`-role messages with a single
/// `tool_result` content block, mirroring the API's expectation.
/// `Role::Assistant` messages with `tool_calls` populated become a
/// content list mixing the prose `text` block with the per-call
/// `tool_use` blocks (Anthropic correlates by `id`).
fn split_anthropic_messages(
    messages: &[ChatMessage],
) -> (Option<String>, Vec<AnthropicMessage<'_>>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut converted: Vec<AnthropicMessage<'_>> = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => {
                if !m.content.is_empty() {
                    system_parts.push(m.content.as_str());
                }
            },
            Role::User => {
                converted.push(AnthropicMessage {
                    role: "user",
                    content: AnthropicMessageContent::Text(m.content.as_str()),
                });
            },
            Role::Assistant => {
                if m.tool_calls.is_empty() {
                    converted.push(AnthropicMessage {
                        role: "assistant",
                        content: AnthropicMessageContent::Text(m.content.as_str()),
                    });
                } else {
                    let mut blocks: Vec<AnthropicContentBlockOut<'_>> = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(AnthropicContentBlockOut::Text {
                            text: m.content.as_str(),
                        });
                    }
                    for call in &m.tool_calls {
                        blocks.push(AnthropicContentBlockOut::ToolUse {
                            id: call.id.as_str(),
                            name: call.name.as_str(),
                            input: &call.arguments,
                        });
                    }
                    converted.push(AnthropicMessage {
                        role: "assistant",
                        content: AnthropicMessageContent::Blocks(blocks),
                    });
                }
            },
            Role::Tool => {
                // tool_call_id is mandatory on the Anthropic wire — without
                // it the API has nothing to correlate the result to. Fall
                // back to an empty string so the bug surfaces on the
                // server-side response rather than as an opaque encode error
                // on our side.
                let tool_use_id = m.tool_call_id.as_deref().unwrap_or("");
                converted.push(AnthropicMessage {
                    role: "user",
                    content: AnthropicMessageContent::Blocks(vec![
                        AnthropicContentBlockOut::ToolResult {
                            tool_use_id,
                            content: m.content.as_str(),
                        },
                    ]),
                });
            },
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, converted)
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        // Delegate to the shared body with the slot's configured thinking
        // budget (`None` unless `reasoning_effort` was mapped). The probe
        // in `health_check` calls `complete_inner` directly with `None`.
        self.complete_inner(request, self.thinking_budget).await
    }

    /// Multi-turn chat with optional function-callable tools, posted
    /// to `POST /v1/messages` with the message history converted to
    /// Anthropic's typed content-block shape. Used by the dashboard
    /// agentic loop (LLM functions) when the
    /// operator pins a cloud profile (`hybrid` / `all-api`).
    ///
    /// Behaviour notes:
    ///
    /// - System messages are pulled out of the history and surfaced as
    ///   the top-level `system` field; mid-conversation system entries
    ///   (rare) are concatenated to the same string with a blank line.
    /// - `Role::Tool` messages become `user` messages with a single
    ///   `tool_result` block, keyed on the original `tool_call_id`
    ///   that Anthropic minted in its earlier `tool_use` response.
    /// - Tool-call IDs round-trip verbatim — unlike the Ollama path,
    ///   we do NOT mint them client-side. The dashboard agentic loop
    ///   echoes the Anthropic id back in `ChatMessage::tool_call_id`
    ///   on the matching `Role::Tool` entry, and Anthropic correlates.
    /// - `stop_reason: "tool_use"` collapses to `FinishReason::EndOfTurn`
    ///   in the response — the agentic loop branches on
    ///   `message.tool_calls.is_empty()`, not on the finish reason.
    /// - **Extended thinking is intentionally not applied here**, even on
    ///   a slot that set `reasoning_effort`. Anthropic requires the
    ///   `thinking` blocks of an assistant turn that also emitted
    ///   `tool_use` to be echoed back verbatim (signature included) on
    ///   the next turn; this loop does not round-trip them, so enabling
    ///   thinking would break multi-turn tool use. The reasoning budget
    ///   binds only on the single-shot `complete` path (the strong slots
    ///   that use it — `rem_promotions`, `cronista` — are completion
    ///   callers).
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.messages.is_empty() {
            return Err(LlmError::Invalid("empty messages".into()));
        }

        let (system, messages) = split_anthropic_messages(&request.messages);

        let tools: Vec<AnthropicTool<'_>> = request
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.as_str(),
                description: t.description.as_str(),
                input_schema: &t.parameters,
            })
            .collect();

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = AnthropicChatRequest {
            model: &self.model,
            max_tokens: request.max_tokens.unwrap_or(Self::DEFAULT_MAX_TOKENS),
            messages,
            system: self.build_system(system.as_deref()),
            temperature: if anthropic_rejects_sampling_params(&self.model) {
                None
            } else {
                request.temperature
            },
            tools,
        };

        let token = self.credential.resolve().await?;
        let response =
            apply_anthropic_auth(self.client.post(url), &token, self.credential.is_oauth())
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let trimmed = body_text.chars().take(500).collect::<String>();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::Auth(format!(
                    "HTTP {status} from Anthropic — check `{}` in mwe-mcp.env: {trimmed}",
                    self.api_key_env,
                )),
                429 => LlmError::RateLimit(format!("HTTP {status}: {trimmed}")),
                400 => LlmError::Invalid(format!("HTTP {status}: {trimmed}")),
                _ => LlmError::Backend(format!("HTTP {status}: {trimmed}")),
            });
        }

        let parsed: AnthropicMessagesResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding anthropic chat response: {e}")))?;

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for block in parsed.content {
            match block {
                AnthropicContentBlockIn::Text { text: Some(t) } => text.push_str(&t),
                AnthropicContentBlockIn::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input,
                        // Anthropic has no thought-signature concept.
                        thought_signature: None,
                    });
                },
                // `Text { text: None }` is degenerate (Anthropic does not
                // emit it in practice but the deserialiser tolerates it);
                // `Other` is the forward-compatibility catch-all for any
                // new block type. Both are silently ignored.
                AnthropicContentBlockIn::Text { text: None } | AnthropicContentBlockIn::Other => {},
            }
        }

        let resp = ChatResponse {
            message: ChatMessage {
                role: Role::Assistant,
                content: text,
                tool_calls,
                tool_call_id: None,
            },
            finish_reason: classify_anthropic_stop_reason(parsed.stop_reason.as_deref()),
            usage: CompletionUsage {
                prompt_tokens: parsed.usage.as_ref().and_then(|u| u.input_tokens),
                completion_tokens: parsed.usage.as_ref().and_then(|u| u.output_tokens),
            },
        };
        warn_if_truncated_chat("anthropic", &self.model, &resp);
        Ok(resp)
    }

    /// Cheap liveness probe for Anthropic: a short completion against the
    /// configured model. The Messages API has no dedicated "list models"
    /// endpoint that does not also exercise auth, so we use the same path
    /// as `complete` with a minimal payload. Refuses to bind the listener
    /// when the API key is missing or the model is misspelled.
    ///
    /// Calls `complete_inner` with `None` so the probe never engages
    /// extended thinking even on a slot that configured it. Two details
    /// keep it compatible with the current model line-up: it pins **no**
    /// `temperature` (Opus 4.7+ reject sampling params outright, a 400),
    /// and it asks for a small-but-non-trivial `max_tokens` — a
    /// `max_tokens: 1` request can come back with **zero** content blocks
    /// on some models (the ceiling is hit before any text is emitted),
    /// which the response parser rejects as "no `text` content block".
    async fn health_check(&self) -> Result<()> {
        let probe = CompletionRequest::new("ping")
            .with_max_tokens(16)
            .with_truncation_expected();
        let _ = self.complete_inner(probe, None).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GeminiBackend — HTTP client for the Google Gemini `generateContent`
// REST API (<https://ai.google.dev/gemini-api/docs/text-generation>).
// Authenticates via the `?key=` query parameter (Gemini does not
// accept the key as a header on the public endpoint).
//
// Why a separate struct rather than a config knob on the existing
// backends: the body shape is unique to Gemini (a `contents[].parts[]`
// tree with `role: "user"` | `"model"`, plus a `generationConfig`
// nested object that pins thinking + sampling + structured output),
// the finish-reason vocabulary is different, and the operational
// constraints below force a different default policy than Ollama or
// Anthropic. Flattening them into a shared adapter would obscure
// every one of those gotchas.
//
// ## Operational notes (carried over from a pre-1.0 dogfood post-
// mortem 2026-05-07; mirrored in `docs/development/conventions.md`
// + `docs/architecture/overview.md`)
//
// - **Combined thinking + output budget.** Gemini 3's `maxOutputTokens`
//   is a *combined* budget for the model's internal reasoning AND the
//   externally-visible output. With the model default
//   `thinkingConfig.thinkingLevel = "high"` it routinely burns the
//   whole budget reasoning and returns ~1K of truncated text with
//   `finishReason: "MAX_TOKENS"`. We pin `thinkingLevel: "minimal"`
//   and `maxOutputTokens: 65536` on every request — same hard policy
//   as `think: false` on Ollama Qwen 3.x — so the structured callers
//   (ingest, REM dedup, hub_writer JSON) get parseable output instead
//   of a truncated tail. A future surface that wants visible reasoning
//   (dashboard ChatPanel "show thinking" toggle) will make this
//   per-call configurable, mirroring LLM functions.
// - **Temperature 1.0 mandatory.** Gemini 3 documentation is explicit
//   that values below 1.0 cause loops and degraded performance on
//   reasoning / math tasks. We clamp the caller's requested
//   temperature to `1.0` at the boundary (logging a debug-level note
//   when overridden) and never pass `temperature: 0.0` even from the
//   default `health_check` — the override below uses 1.0 + 1 token
//   for the probe.
// - **`MAX_TOKENS` is a retriable backend error, not a soft truncation.**
//   On Anthropic / Ollama, `finish_reason == MaxTokens` is a legitimate
//   end-of-generation that the caller can choose to use; on Gemini it
//   nearly always means "thinking ate the budget, the output is a
//   truncated string nobody can parse". We surface it as
//   `LlmError::Backend` so the caller throws and the upstream retry
//   layer (REM scheduler, dashboard agentic loop) gets a chance
//   to back off and reissue rather than serialise a half-JSON.
// - **Structured JSON via `responseMimeType: "application/json"`.**
//   When the caller asks for a JSON response (today via
//   [`CompletionRequest::stop`] being empty and the structured caller
//   convention; a richer `response_format` field is deferred until
//   we have a second provider that supports it), Gemini guarantees
//   well-formed JSON output. Belt-and-suspenders against the
//   `parse_plan` failure rate on ingest.
// - **Function calling (`tools`) and `thoughtSignature`.** Gemini 3
//   (with thinking) attaches an opaque `thoughtSignature` to each
//   `functionCall` part of an assistant turn and requires it echoed
//   back verbatim when that turn is replayed in a later request
//   (strict validation: a missing signature is a hard 400,
//   `INVALID_ARGUMENT` — "Function call is missing a thought_signature
//   in functionCall parts"). The signature sits *next to* `functionCall`
//   in the part object, not inside it. We capture it on the inbound
//   response onto [`ToolCall::thought_signature`] and re-emit it on the
//   outbound `functionCall` part in `split_gemini_messages`, so it
//   survives the dashboard's agentic loop (LLM functions),
//   which holds `Vec<ChatMessage>` and replays the assistant turn.
//   The signature is carried opaquely on the assistant message's
//   `ToolCall`; providers without the concept leave it `None`.
// - **Empty `contents` and orphaned function-call truncation.** Gemini
//   rejects an empty `contents[]` with HTTP 400 (we check
//   `request.messages.is_empty()` at the boundary). If a dashboard
//   client truncates chat history in the middle of a
//   `functionCall`/`functionResponse` pair, Gemini returns
//   `400 INVALID_ARGUMENT`; the agentic-loop truncator walks backward
//   to a clean `user` turn before slicing. Not enforced here —
//   responsibility of the caller — but documented so the next
//   integration does not relearn it.
// ---------------------------------------------------------------------------

/// Default base URL for the Gemini `generateContent` REST API.
/// Override via [`GeminiBackend::with_base_url`] in tests or for
/// regional / proxy gateways.
pub const DEFAULT_GEMINI_URL: &str = "https://generativelanguage.googleapis.com";

/// API-version segment of the Gemini REST path.
///
/// Pinned to `v1beta` because that's where the Gemini 3 models
/// (Flash and Pro) live as of 2026-05; `v1` lags behind on
/// thinking-config support. When Gemini 3 graduates to `v1` we'll
/// bump and drop a `logs.md` entry.
pub const GEMINI_API_VERSION: &str = "v1beta";

/// Forced `maxOutputTokens` for every Gemini request.
///
/// The model max for Gemini 3 Flash / Pro. We always send the max
/// because the budget is shared with the internal thinking trace;
/// passing a smaller value leaves no room for either side.
pub const GEMINI_MAX_OUTPUT_TOKENS: u32 = 65_536;

/// **Default** `thinkingConfig.thinkingLevel` for a Gemini request.
///
/// `"minimal"` keeps the reasoning trace below ~256 tokens so the
/// combined budget is dominated by the actual output — see the module
/// docs for why this matters. It is the default for every backend; a
/// per-slot `reasoning_effort` hint overrides it via
/// [`GeminiBackend::with_reasoning_effort`] (required for Gemini 3.x
/// Pro, which rejects `"minimal"`).
pub const GEMINI_THINKING_LEVEL: &str = "minimal";

/// Forced sampling temperature. Gemini 3 requires `1.0`; values below
/// cause loops and degraded performance. Caller-supplied temperatures
/// are clamped to this value at the boundary.
pub const GEMINI_TEMPERATURE: f32 = 1.0;

/// API key for a [`GeminiBackend`], `Debug`-redacted.
///
/// Same pattern as [`AnthropicApiKey`] / `crate::jwt::TokenSecret`.
/// Holding the key in a newtype keeps it from leaking through
/// `tracing` field-debug or `?` formatting on the backend struct.
#[derive(Clone)]
pub struct GeminiApiKey(String);

impl GeminiApiKey {
    /// Wrap a raw API key string. Empty input is rejected so a
    /// misread env-var surfaces at construction time instead of as
    /// an opaque 400 on first request.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Invalid`] when `raw` is empty after
    /// trimming surrounding whitespace.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let s = raw.into();
        if s.trim().is_empty() {
            return Err(LlmError::Invalid("gemini api key is empty".into()));
        }
        Ok(Self(s))
    }

    /// Expose the raw key bytes — only the HTTP layer needs this to
    /// stamp the `?key=` query parameter.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for GeminiApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiApiKey")
            .field("len", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

/// HTTP client for Google's Gemini `generateContent` REST API.
///
/// Two paths:
///
/// - `complete` packages [`CompletionRequest::system`] as the
///   top-level `systemInstruction` and [`CompletionRequest::prompt`]
///   as a single `user`-role `contents` entry.
/// - `chat` issues the same `POST` but with the full history converted
///   to `contents[]` (system stripped to `systemInstruction`, tool
///   calls/results encoded as `functionCall`/`functionResponse` parts).
///   Tool-call ids round-trip via Gemini-minted `functionCall.name`
///   plus our synthetic per-position id (Gemini does not mint ids
///   itself — it correlates by `name`; we mint stable indices so the
///   `ChatMessage::tool_call_id` contract stays uniform across
///   providers).
///
/// Operator-relevant behaviour pinned by the module docs above:
///
/// - `maxOutputTokens` is always [`GEMINI_MAX_OUTPUT_TOKENS`]
///   regardless of [`CompletionRequest::max_tokens`].
/// - `thinkingLevel` defaults to [`GEMINI_THINKING_LEVEL`]
///   (`"minimal"`) and is overridden per slot by
///   [`GeminiBackend::with_reasoning_effort`] — Gemini 3.x Pro rejects
///   `"minimal"` and needs `low`/`medium`/`high`.
/// - `temperature` is always [`GEMINI_TEMPERATURE`] regardless of
///   [`CompletionRequest::temperature`] (clamped at the boundary).
/// - `finishReason == "MAX_TOKENS"` is surfaced as
///   [`LlmError::Backend`] rather than [`FinishReason::MaxTokens`],
///   because on Gemini it almost always means "thinking ate the
///   budget, the output is unparseable" rather than a legitimate
///   end-of-generation.
/// - 401 / 403 → [`LlmError::Auth`] with the env-var name; 429 →
///   [`LlmError::RateLimit`]; 400 → [`LlmError::Invalid`].
pub struct GeminiBackend {
    client: Client,
    base_url: String,
    model: String,
    api_key: GeminiApiKey,
    /// Name of the env-var that originally carried the key. Used only
    /// to enrich the auth-error message, same convention as
    /// [`AnthropicBackend`].
    api_key_env: String,
    /// `thinkingConfig.thinkingLevel` sent on every request. Defaults
    /// to [`GEMINI_THINKING_LEVEL`] (`"minimal"`), overridden per slot
    /// via [`Self::with_reasoning_effort`]. Gemini 3 Flash accepts
    /// `"minimal"`; Gemini 3.x Pro rejects it, so a Pro slot must carry
    /// a non-minimal `reasoning_effort` in its config.
    thinking_level: String,
}

impl GeminiBackend {
    /// Build a backend pointing at the canonical Gemini endpoint.
    /// Pass the env-var name you read `api_key` from so auth errors
    /// can name it back to the operator.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Transport`] when the underlying `reqwest`
    /// client cannot be constructed (TLS init, runtime issues).
    pub fn new(
        api_key: GeminiApiKey,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            base_url: DEFAULT_GEMINI_URL.to_owned(),
            model: model.into(),
            api_key,
            api_key_env: api_key_env.into(),
            thinking_level: GEMINI_THINKING_LEVEL.to_owned(),
        })
    }

    /// Map the slot's optional `reasoning_effort` config hint onto
    /// Gemini's `thinkingConfig.thinkingLevel`.
    ///
    /// Gemini 3 Flash accepts `"minimal"` (the default, which keeps the
    /// thinking trace tiny so it does not eat the shared output budget —
    /// see the module docs); Gemini 3.x **Pro rejects `"minimal"`** with
    /// a `400` and requires `low`/`medium`/`high`. The mapping:
    ///
    /// | `reasoning_effort` | `thinkingLevel` |
    /// |---|---|
    /// | unset / `"minimal"` | `minimal` |
    /// | `"low"` | `low` |
    /// | `"medium"` | `medium` |
    /// | `"high"` / `"extra-high"` | `high` |
    /// | anything else | `low` |
    ///
    /// The unknown-value floor is `low` (a safe non-minimal level) so a
    /// typo in the config never locks out a Pro model at boot.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        let level = match effort.map(str::trim) {
            None | Some("" | "minimal") => "minimal",
            Some("medium") => "medium",
            Some("high" | "extra-high") => "high",
            // `"low"` and any unrecognised value collapse to a safe
            // non-minimal floor, so a typo never locks a Pro model
            // (which rejects `minimal`) out of boot.
            Some(_) => "low",
        };
        level.clone_into(&mut self.thinking_level);
        self
    }

    /// Builder: replace the default base URL — used by tests against
    /// wiremock and by self-hosted / regional gateways. The string
    /// must not have a trailing slash; the backend joins paths
    /// verbatim.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// URL for the model's `generateContent` endpoint, with the API
    /// key as a query parameter. Never logged — `reqwest` redacts it
    /// at debug level, and our own `tracing` call sites only emit the
    /// model name + status code.
    fn generate_content_url(&self) -> String {
        format!(
            "{}/{}/models/{}:generateContent?key={}",
            self.base_url.trim_end_matches('/'),
            GEMINI_API_VERSION,
            self.model,
            self.api_key.as_str(),
        )
    }
}

#[derive(Debug, Serialize)]
struct GeminiGenerateRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    system_instruction: Option<GeminiSystemInstruction<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTool<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    role: &'static str,
    parts: Vec<GeminiPart<'a>>,
}

/// Gemini's `parts[]` entry is a tagged union: text, function call,
/// or function response. We serialise with `#[serde(untagged)]` so the
/// wire shape matches Google's documentation exactly (text part is
/// `{ "text": "..." }` with no envelope, function call is
/// `{ "functionCall": { "name": ..., "args": ... } }`, etc.).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum GeminiPart<'a> {
    Text {
        text: &'a str,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCallOut<'a>,
        /// Gemini 3 thinking attaches an opaque `thoughtSignature` as a
        /// sibling of `functionCall` inside the part object (not nested
        /// inside it); it MUST be echoed back verbatim or Gemini rejects
        /// the replayed turn with a hard 400.
        #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
        thought_signature: Option<&'a str>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponseOut<'a>,
    },
    /// Inline base64 media — the vision path of the completion call.
    /// The field is mandatory so the untagged union stays structurally
    /// unambiguous.
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineDataOut<'a>,
    },
}

/// The `inlineData` payload of a media part.
#[derive(Debug, Serialize)]
struct GeminiInlineDataOut<'a> {
    #[serde(rename = "mimeType")]
    mime_type: &'a str,
    data: &'a str,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionCallOut<'a> {
    name: &'a str,
    args: &'a serde_json::Value,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionResponseOut<'a> {
    name: &'a str,
    response: GeminiFunctionResponseBody<'a>,
}

/// Gemini wants the function result wrapped in an object with a single
/// key the model can index ("content" by convention). We pass the
/// caller's textual result through this envelope verbatim.
#[derive(Debug, Serialize)]
struct GeminiFunctionResponseBody<'a> {
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    #[serde(rename = "thinkingConfig")]
    thinking_config: GeminiThinkingConfig,
    #[serde(skip_serializing_if = "Option::is_none", rename = "responseMimeType")]
    response_mime_type: Option<&'static str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty", rename = "stopSequences")]
    stop_sequences: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingLevel")]
    thinking_level: String,
}

#[derive(Debug, Serialize)]
struct GeminiTool<'a> {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDeclaration<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GeminiGenerateResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiCandidateContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<GeminiCandidatePart>,
}

/// Inbound `parts[]` entry. We accept any of the three shapes Gemini
/// emits and ignore the rest (e.g. `executableCode`, `codeExecutionResult`)
/// so a forward-compatible response with unfamiliar parts does not
/// crash the deserialiser.
#[derive(Debug, Deserialize)]
struct GeminiCandidatePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiFunctionCallIn>,
    /// Opaque `thoughtSignature` Gemini 3 attaches to a `functionCall`
    /// part. Captured here and carried on the resulting [`ToolCall`] so
    /// the agentic loop can echo it back verbatim on replay (omitting it
    /// is a hard 400). Sibling of `functionCall`, not nested in it.
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiFunctionCallIn {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
}

/// Map Gemini's `finishReason` vocabulary onto [`FinishReason`].
///
/// `MAX_TOKENS` is **not** mapped here — it is intercepted in the
/// response handler and surfaced as [`LlmError::Backend`] because on
/// Gemini it almost always indicates a truncated, unparseable output
/// caused by thinking consuming the combined budget. See the module
/// docs for why.
const fn classify_gemini_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some(s) if matches!(s.as_bytes(), b"STOP" | b"end_of_turn") => FinishReason::EndOfTurn,
        _ => FinishReason::Other,
    }
}

/// Map an unsuccessful Gemini HTTP response (status + truncated body)
/// onto the appropriate [`LlmError`] variant. Same mapping for
/// `complete` and `chat` — extracted so the two paths cannot drift
/// (the REM scheduler fatal-error logic and the dashboard
/// agentic loop both branch on the variant, not the message).
fn map_gemini_http_error(
    status: reqwest::StatusCode,
    body_text: &str,
    api_key_env: &str,
) -> LlmError {
    let trimmed = body_text.chars().take(500).collect::<String>();
    match status.as_u16() {
        401 | 403 => LlmError::Auth(format!(
            "HTTP {status} from Gemini — check `{api_key_env}` in mwe-mcp.env: {trimmed}",
        )),
        429 => LlmError::RateLimit(format!("HTTP {status}: {trimmed}")),
        400 => LlmError::Invalid(format!("HTTP {status}: {trimmed}")),
        _ => LlmError::Backend(format!("HTTP {status}: {trimmed}")),
    }
}

fn build_generation_config(
    stop: Vec<String>,
    want_json: bool,
    thinking_level: &str,
) -> GeminiGenerationConfig {
    GeminiGenerationConfig {
        temperature: GEMINI_TEMPERATURE,
        max_output_tokens: GEMINI_MAX_OUTPUT_TOKENS,
        thinking_config: GeminiThinkingConfig {
            thinking_level: thinking_level.to_owned(),
        },
        response_mime_type: if want_json {
            Some("application/json")
        } else {
            None
        },
        stop_sequences: stop,
    }
}

/// Translate a `ChatMessage` history into Gemini's `contents[]` shape.
///
/// Gemini carries the system prompt as a top-level `systemInstruction`
/// instead of a message, so any `Role::System` entry is stripped and
/// surfaced via the returned `Option<String>` (multiple concatenated
/// with a blank line, same convention as the Anthropic adapter).
///
/// `Role::Assistant` maps to `role: "model"`; an assistant turn with
/// `tool_calls` becomes a `contents[]` entry whose `parts[]` mixes
/// optional `text` with one `functionCall` per call.
///
/// `Role::Tool` maps to `role: "user"` with a single `functionResponse`
/// part (Gemini correlates by `name`, not by id; we use the tool name
/// the caller stored on the original `ToolCall`).
fn split_gemini_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<GeminiContent<'_>>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut converted: Vec<GeminiContent<'_>> = Vec::with_capacity(messages.len());
    let mut last_tool_call_name_by_id: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for m in messages {
        match m.role {
            Role::System => {
                if !m.content.is_empty() {
                    system_parts.push(m.content.as_str());
                }
            },
            Role::User => {
                converted.push(GeminiContent {
                    role: "user",
                    parts: vec![GeminiPart::Text {
                        text: m.content.as_str(),
                    }],
                });
            },
            Role::Assistant => {
                let mut parts: Vec<GeminiPart<'_>> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(GeminiPart::Text {
                        text: m.content.as_str(),
                    });
                }
                for call in &m.tool_calls {
                    last_tool_call_name_by_id.insert(call.id.as_str(), call.name.as_str());
                    parts.push(GeminiPart::FunctionCall {
                        function_call: GeminiFunctionCallOut {
                            name: call.name.as_str(),
                            args: &call.arguments,
                        },
                        thought_signature: call.thought_signature.as_deref(),
                    });
                }
                converted.push(GeminiContent {
                    role: "model",
                    parts,
                });
            },
            Role::Tool => {
                // Resolve the function name from the prior assistant turn
                // by tool_call_id (Gemini correlates by name, not by id).
                // Fall back to the tool_call_id verbatim — it won't match
                // any declared tool, which surfaces as a Gemini-side error
                // on the response rather than as a silent miscorrelation.
                let id_ref = m.tool_call_id.as_deref().unwrap_or("");
                let name = last_tool_call_name_by_id
                    .get(id_ref)
                    .copied()
                    .unwrap_or(id_ref);
                converted.push(GeminiContent {
                    role: "user",
                    parts: vec![GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponseOut {
                            name,
                            response: GeminiFunctionResponseBody {
                                content: m.content.as_str(),
                            },
                        },
                    }],
                });
            },
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, converted)
}

#[async_trait]
impl LlmBackend for GeminiBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        if request.prompt.is_empty() {
            return Err(LlmError::Invalid("empty prompt".into()));
        }
        if request.temperature.is_some() && request.temperature != Some(GEMINI_TEMPERATURE) {
            tracing::debug!(
                requested = ?request.temperature,
                forced = GEMINI_TEMPERATURE,
                "gemini: clamping caller temperature to mandated 1.0 (sub-1 values cause loops)"
            );
        }

        let system_instruction = request.system.as_deref().map(|s| GeminiSystemInstruction {
            parts: vec![GeminiPart::Text { text: s }],
        });
        // Image parts precede the text part (the conventional order);
        // a text-only request keeps the single-part body unchanged.
        let mut parts: Vec<GeminiPart<'_>> = request
            .images
            .iter()
            .map(|img| GeminiPart::InlineData {
                inline_data: GeminiInlineDataOut {
                    mime_type: &img.mime_type,
                    data: &img.data_base64,
                },
            })
            .collect();
        parts.push(GeminiPart::Text {
            text: request.prompt.as_str(),
        });
        // Captured before `request.stop` moves into the wire body: the
        // truncation warning below still needs the caller's cap + flag.
        let cap = request.max_tokens;
        let truncation_expected = request.truncation_expected;
        let body = GeminiGenerateRequest {
            contents: vec![GeminiContent {
                role: "user",
                parts,
            }],
            system_instruction,
            generation_config: build_generation_config(request.stop, false, &self.thinking_level),
            tools: Vec::new(),
        };

        let response = self
            .client
            .post(self.generate_content_url())
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(map_gemini_http_error(status, &body_text, &self.api_key_env));
        }

        let parsed: GeminiGenerateResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding gemini response: {e}")))?;

        let candidate = parsed
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Protocol("gemini response has no `candidates`".into()))?;
        let finish_raw = candidate.finish_reason.as_deref();
        if matches!(finish_raw, Some("MAX_TOKENS")) {
            return Err(LlmError::Backend(
                "gemini output truncated (finishReason=MAX_TOKENS) — combined thinking+output budget exhausted; the response is unparseable, treat as retriable".to_owned(),
            ));
        }

        let mut text = String::new();
        let mut saw_text = false;
        if let Some(content) = candidate.content {
            for part in content.parts {
                if let Some(t) = part.text {
                    text.push_str(&t);
                    saw_text = true;
                }
            }
        }
        if !saw_text {
            return Err(LlmError::Protocol(
                "gemini response has no `text` part in the first candidate".into(),
            ));
        }

        let resp = CompletionResponse {
            text,
            finish_reason: classify_gemini_finish_reason(finish_raw),
            usage: CompletionUsage {
                prompt_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .and_then(|u| u.prompt_token_count),
                completion_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .and_then(|u| u.candidates_token_count),
            },
        };
        warn_if_truncated_parts("gemini", &self.model, cap, truncation_expected, &resp);
        Ok(resp)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.messages.is_empty() {
            return Err(LlmError::Invalid("empty messages".into()));
        }
        if request.temperature.is_some() && request.temperature != Some(GEMINI_TEMPERATURE) {
            tracing::debug!(
                requested = ?request.temperature,
                forced = GEMINI_TEMPERATURE,
                "gemini: clamping caller temperature to mandated 1.0 (sub-1 values cause loops)"
            );
        }

        let (system, contents) = split_gemini_messages(&request.messages);
        let system_instruction = system.as_deref().map(|s| GeminiSystemInstruction {
            parts: vec![GeminiPart::Text { text: s }],
        });

        let tools: Vec<GeminiTool<'_>> = if request.tools.is_empty() {
            Vec::new()
        } else {
            vec![GeminiTool {
                function_declarations: request
                    .tools
                    .iter()
                    .map(|t| GeminiFunctionDeclaration {
                        name: t.name.as_str(),
                        description: t.description.as_str(),
                        parameters: &t.parameters,
                    })
                    .collect(),
            }]
        };

        let body = GeminiGenerateRequest {
            contents,
            system_instruction,
            generation_config: build_generation_config(Vec::new(), false, &self.thinking_level),
            tools,
        };

        let response = self
            .client
            .post(self.generate_content_url())
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(map_gemini_http_error(status, &body_text, &self.api_key_env));
        }

        let parsed: GeminiGenerateResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding gemini chat response: {e}")))?;

        let candidate =
            parsed.candidates.into_iter().next().ok_or_else(|| {
                LlmError::Protocol("gemini chat response has no `candidates`".into())
            })?;
        let finish_raw = candidate.finish_reason.as_deref();
        if matches!(finish_raw, Some("MAX_TOKENS")) {
            return Err(LlmError::Backend(
                "gemini chat truncated (finishReason=MAX_TOKENS) — combined thinking+output budget exhausted; treat as retriable".to_owned(),
            ));
        }

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(content) = candidate.content {
            for (idx, part) in content.parts.into_iter().enumerate() {
                if let Some(t) = part.text {
                    text.push_str(&t);
                }
                if let Some(call) = part.function_call {
                    tool_calls.push(ToolCall {
                        id: format!("call_{idx}"),
                        name: call.name,
                        arguments: call.args,
                        thought_signature: part.thought_signature,
                    });
                }
            }
        }

        let resp = ChatResponse {
            message: ChatMessage {
                role: Role::Assistant,
                content: text,
                tool_calls,
                tool_call_id: None,
            },
            finish_reason: classify_gemini_finish_reason(finish_raw),
            usage: CompletionUsage {
                prompt_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .and_then(|u| u.prompt_token_count),
                completion_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .and_then(|u| u.candidates_token_count),
            },
        };
        warn_if_truncated_chat("gemini", &self.model, &resp);
        Ok(resp)
    }

    /// Cheap liveness probe for Gemini: a minimal `generateContent`
    /// against the configured model. Gemini has no auth-only endpoint,
    /// so we use the same path as `complete` with a 1-char prompt.
    /// Pinned at the mandated [`GEMINI_TEMPERATURE`] (= 1.0) so the
    /// default trait probe's `temperature: 0.0` is never sent.
    async fn health_check(&self) -> Result<()> {
        let probe = CompletionRequest::new(".").with_temperature(GEMINI_TEMPERATURE);
        let _ = self.complete(probe).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OpenRouter backend
//
// OpenRouter (<https://openrouter.ai>) is an OpenAI-compatible aggregator:
// one API key + one base URL routes to hundreds of upstream models, each
// addressed by a `vendor/model` slug (`anthropic/claude-sonnet-4-6`,
// `google/gemini-3-pro`, …). A single backend that collapses the
// multi-provider / multi-key problem — see the admin LLM config wiki page.
//
// The wire format is the OpenAI Chat Completions API
// (`POST {base_url}/chat/completions`, `Authorization: Bearer`), so this
// backend implements both the single-prompt `complete` path and the
// tools-enabled `chat` path. The optional `reasoning_effort` slot hint
// maps onto OpenRouter's `reasoning: { effort }` knob (`low`/`medium`/
// `high`); `minimal`/unset omits it.
// ---------------------------------------------------------------------------

/// Default `OpenRouter` API base URL. Path-suffix-free; the backend appends
/// `/chat/completions`.
pub const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1";

/// API key for `OpenRouter`. Same redaction discipline as
/// [`GeminiApiKey`] / [`AnthropicApiKey`]: held in a newtype so it cannot
/// leak through `tracing` field-debug or `?` on the backend struct.
#[derive(Clone)]
pub struct OpenRouterApiKey(String);

impl OpenRouterApiKey {
    /// Wrap a raw key. Empty input is rejected at construction so a
    /// misread env-var surfaces here instead of as an opaque 401 on the
    /// first request.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Invalid`] when `raw` is empty after trimming.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let s = raw.into();
        if s.trim().is_empty() {
            return Err(LlmError::Invalid("openrouter api key is empty".into()));
        }
        Ok(Self(s))
    }

    /// Expose the raw key — only the HTTP layer needs it to stamp the
    /// `Authorization: Bearer` header.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for OpenRouterApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterApiKey")
            .field("len", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

/// HTTP client for `OpenRouter`'s OpenAI-compatible Chat Completions API.
///
/// - `complete` sends `[system?, user]` and reads `choices[0].message.content`.
/// - `chat` converts the full [`ChatMessage`] history to `OpenAI` message
///   shape (assistant `tool_calls`, `tool` results by `tool_call_id`) and
///   parses tool calls back out of the response.
///
/// `temperature` and `max_tokens` pass through unset → provider default
/// (no clamping, unlike Gemini). `base_url` is overridable for tests and
/// self-hosted gateways.
pub struct OpenRouterBackend {
    client: Client,
    base_url: String,
    model: String,
    api_key: OpenRouterApiKey,
    /// Env-var the key came from — echoed back on auth errors, same
    /// convention as [`AnthropicBackend`] / [`GeminiBackend`].
    api_key_env: String,
    /// Optional `reasoning.effort` (`low`/`medium`/`high`). `None` omits
    /// the `reasoning` block entirely.
    reasoning_effort: Option<String>,
}

impl OpenRouterBackend {
    /// Build a backend pointing at the canonical `OpenRouter` endpoint.
    /// Pass the env-var name you read `api_key` from so auth errors can
    /// name it back to the operator.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Transport`] when the `reqwest` client cannot
    /// be constructed (TLS init, runtime issues).
    pub fn new(
        api_key: OpenRouterApiKey,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            base_url: DEFAULT_OPENROUTER_URL.to_owned(),
            model: model.into(),
            api_key,
            api_key_env: api_key_env.into(),
            reasoning_effort: None,
        })
    }

    /// Map the slot's `reasoning_effort` hint onto `OpenRouter`'s
    /// `reasoning.effort`. `OpenRouter` accepts `low`/`medium`/`high`;
    /// `extra-high` folds to `high`, `minimal`/unset omits the block,
    /// and an unrecognised value floors to `medium` so a typo never
    /// sends an invalid effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        self.reasoning_effort = match effort.map(str::trim) {
            None | Some("" | "minimal") => None,
            Some("low") => Some("low".to_owned()),
            Some("high" | "extra-high") => Some("high".to_owned()),
            // `"medium"` and any unrecognised value collapse to the safe
            // middle so a config typo never sends an invalid effort.
            Some(_) => Some("medium".to_owned()),
        };
        self
    }

    /// Builder: override the base URL — used by tests against wiremock
    /// and by self-hosted / proxy gateways. Must not carry a trailing
    /// slash; the backend appends paths verbatim.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn reasoning(&self) -> Option<OpenRouterReasoning<'_>> {
        self.reasoning_effort
            .as_deref()
            .map(|effort| OpenRouterReasoning { effort })
    }
}

// ---- OpenRouter wire types (request) ----

#[derive(Debug, Serialize)]
struct OpenRouterChatBody<'a> {
    model: &'a str,
    messages: Vec<OpenRouterMessageOut<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenRouterToolDef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenRouterReasoning<'a>>,
}

#[derive(Debug, Serialize)]
struct OpenRouterReasoning<'a> {
    effort: &'a str,
}

#[derive(Debug, Serialize)]
struct OpenRouterMessageOut<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenRouterContentOut<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenRouterToolCallOut<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// `OpenAI` message `content`: a bare string, or an array of typed parts
/// (the vision path). Serialise-only, so `untagged` just emits whichever
/// variant we built.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenRouterContentOut<'a> {
    Text(&'a str),
    Parts(Vec<OpenRouterContentPart<'a>>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OpenRouterContentPart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OpenRouterImageUrl },
}

#[derive(Debug, Serialize)]
struct OpenRouterImageUrl {
    /// `data:{mime};base64,{data}` URL.
    url: String,
}

#[derive(Debug, Serialize)]
struct OpenRouterToolDef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenRouterFunctionDef<'a>,
}

#[derive(Debug, Serialize)]
struct OpenRouterFunctionDef<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OpenRouterToolCallOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenRouterFunctionCallOut<'a>,
}

#[derive(Debug, Serialize)]
struct OpenRouterFunctionCallOut<'a> {
    name: &'a str,
    /// `OpenAI` passes tool-call arguments as a JSON-encoded **string**.
    arguments: String,
}

// ---- OpenRouter wire types (response) ----

#[derive(Debug, Deserialize)]
struct OpenRouterChatResponse {
    #[serde(default)]
    choices: Vec<OpenRouterChoice>,
    #[serde(default)]
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    #[serde(default)]
    message: Option<OpenRouterMessageIn>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterMessageIn {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenRouterToolCallIn>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterToolCallIn {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenRouterFunctionCallIn>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterFunctionCallIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenRouterUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
}

/// Map OpenAI/OpenRouter `finish_reason` onto [`FinishReason`].
const fn classify_openrouter_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some(s) if matches!(s.as_bytes(), b"stop" | b"end_turn") => FinishReason::EndOfTurn,
        Some(s) if matches!(s.as_bytes(), b"length") => FinishReason::MaxTokens,
        _ => FinishReason::Other,
    }
}

/// Map an unsuccessful `OpenRouter` HTTP response onto an [`LlmError`].
/// Same variant discipline as the other cloud backends so the REM
/// scheduler / agentic loop branch on the variant, not the message.
fn map_openrouter_http_error(
    status: reqwest::StatusCode,
    body_text: &str,
    api_key_env: &str,
) -> LlmError {
    let trimmed = body_text.chars().take(500).collect::<String>();
    match status.as_u16() {
        401 | 403 => LlmError::Auth(format!(
            "openrouter rejected the key from `{api_key_env}` (HTTP {status}): {trimmed}"
        )),
        429 => LlmError::RateLimit(format!(
            "openrouter rate-limited (HTTP {status}): {trimmed}"
        )),
        400 => LlmError::Invalid(format!(
            "openrouter rejected the request (HTTP {status}): {trimmed}"
        )),
        _ => LlmError::Backend(format!("openrouter HTTP {status}: {trimmed}")),
    }
}

/// Parse `OpenAI`'s JSON-string tool-call arguments into a [`serde_json::Value`],
/// degrading an empty / malformed payload to an empty object so the
/// agentic loop always gets a structurally valid argument bag.
fn parse_openrouter_tool_args(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({}))
}

/// Build the `[system?, user]` message list for a single completion,
/// using image content parts (the media-pipeline vision path) when the
/// request carries images.
fn openrouter_completion_messages<'a>(
    prompt: &'a str,
    system: Option<&'a str>,
    images: &'a [ImageInput],
) -> Vec<OpenRouterMessageOut<'a>> {
    let mut messages: Vec<OpenRouterMessageOut<'a>> = Vec::with_capacity(2);
    if let Some(system) = system {
        messages.push(OpenRouterMessageOut {
            role: "system",
            content: Some(OpenRouterContentOut::Text(system)),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    // User turn: a plain text string, or text + image parts (vision path).
    let user_content = if images.is_empty() {
        OpenRouterContentOut::Text(prompt)
    } else {
        let mut parts: Vec<OpenRouterContentPart<'a>> = images
            .iter()
            .map(|img| OpenRouterContentPart::ImageUrl {
                image_url: OpenRouterImageUrl {
                    url: format!("data:{};base64,{}", img.mime_type, img.data_base64),
                },
            })
            .collect();
        parts.push(OpenRouterContentPart::Text { text: prompt });
        OpenRouterContentOut::Parts(parts)
    };
    messages.push(OpenRouterMessageOut {
        role: "user",
        content: Some(user_content),
        tool_calls: Vec::new(),
        tool_call_id: None,
    });
    messages
}

/// Convert a [`ChatMessage`] history to chat-completions message shape:
/// assistant `tool_calls` serialised to JSON-string arguments, `tool`
/// results keyed by `tool_call_id`, and pure-tool-call turns with an
/// omitted `content`.
fn openrouter_messages(history: &[ChatMessage]) -> Vec<OpenRouterMessageOut<'_>> {
    history
        .iter()
        .map(|msg| {
            let tool_calls: Vec<OpenRouterToolCallOut<'_>> = msg
                .tool_calls
                .iter()
                .map(|tc| OpenRouterToolCallOut {
                    id: tc.id.as_str(),
                    kind: "function",
                    function: OpenRouterFunctionCallOut {
                        name: tc.name.as_str(),
                        arguments: serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_owned()),
                    },
                })
                .collect();
            let content = if msg.content.is_empty() && !tool_calls.is_empty() {
                None
            } else {
                Some(OpenRouterContentOut::Text(msg.content.as_str()))
            };
            OpenRouterMessageOut {
                role: msg.role.as_str(),
                content,
                tool_calls,
                tool_call_id: msg.tool_call_id.as_deref(),
            }
        })
        .collect()
}

/// Convert [`Tool`] descriptors to chat-completions `tools[]` entries.
fn openrouter_tools(tools: &[Tool]) -> Vec<OpenRouterToolDef<'_>> {
    tools
        .iter()
        .map(|t| OpenRouterToolDef {
            kind: "function",
            function: OpenRouterFunctionDef {
                name: t.name.as_str(),
                description: t.description.as_str(),
                parameters: &t.parameters,
            },
        })
        .collect()
}

#[async_trait]
impl LlmBackend for OpenRouterBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let CompletionRequest {
            prompt,
            system,
            max_tokens,
            temperature,
            stop,
            images,
            truncation_expected,
        } = request;
        if prompt.is_empty() {
            return Err(LlmError::Invalid("empty prompt".into()));
        }

        let body = OpenRouterChatBody {
            model: &self.model,
            messages: openrouter_completion_messages(&prompt, system.as_deref(), &images),
            max_tokens,
            temperature,
            stop,
            tools: Vec::new(),
            reasoning: self.reasoning(),
        };

        let response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(self.api_key.as_str())
            .header("content-type", "application/json")
            .header("x-title", "mwe-mcp")
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(map_openrouter_http_error(
                status,
                &body_text,
                &self.api_key_env,
            ));
        }

        let parsed: OpenRouterChatResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding openrouter response: {e}")))?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Protocol("openrouter response has no `choices`".into()))?;
        let finish_raw = choice.finish_reason.as_deref();
        let text = choice
            .message
            .and_then(|m| m.content)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                LlmError::Protocol("openrouter response has no message content".into())
            })?;

        let resp = CompletionResponse {
            text,
            finish_reason: classify_openrouter_finish_reason(finish_raw),
            usage: CompletionUsage {
                prompt_tokens: parsed.usage.as_ref().and_then(|u| u.prompt_tokens),
                completion_tokens: parsed.usage.as_ref().and_then(|u| u.completion_tokens),
            },
        };
        warn_if_truncated_parts(
            "openrouter",
            &self.model,
            max_tokens,
            truncation_expected,
            &resp,
        );
        Ok(resp)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let ChatRequest {
            messages: history,
            tools: tool_descs,
            max_tokens,
            temperature,
        } = request;
        if history.is_empty() {
            return Err(LlmError::Invalid("empty messages".into()));
        }

        let body = OpenRouterChatBody {
            model: &self.model,
            messages: openrouter_messages(&history),
            max_tokens,
            temperature,
            stop: Vec::new(),
            tools: openrouter_tools(&tool_descs),
            reasoning: self.reasoning(),
        };

        let response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(self.api_key.as_str())
            .header("content-type", "application/json")
            .header("x-title", "mwe-mcp")
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(map_openrouter_http_error(
                status,
                &body_text,
                &self.api_key_env,
            ));
        }

        let parsed: OpenRouterChatResponse = response
            .json()
            .await
            .map_err(|e| LlmError::Protocol(format!("decoding openrouter chat response: {e}")))?;

        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            LlmError::Protocol("openrouter chat response has no `choices`".into())
        })?;
        let finish_raw = choice.finish_reason.as_deref();
        let (content_opt, raw_tool_calls) = choice
            .message
            .map_or((None, Vec::new()), |m| (m.content, m.tool_calls));

        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (idx, tc) in raw_tool_calls.into_iter().enumerate() {
            let Some(func) = tc.function else { continue };
            tool_calls.push(ToolCall {
                id: tc.id.unwrap_or_else(|| format!("call_{idx}")),
                name: func.name,
                arguments: parse_openrouter_tool_args(&func.arguments),
                thought_signature: None,
            });
        }

        let resp = ChatResponse {
            message: ChatMessage {
                role: Role::Assistant,
                content: content_opt.unwrap_or_default(),
                tool_calls,
                tool_call_id: None,
            },
            finish_reason: classify_openrouter_finish_reason(finish_raw),
            usage: CompletionUsage {
                prompt_tokens: parsed.usage.as_ref().and_then(|u| u.prompt_tokens),
                completion_tokens: parsed.usage.as_ref().and_then(|u| u.completion_tokens),
            },
        };
        warn_if_truncated_chat("openrouter", &self.model, &resp);
        Ok(resp)
    }
}

/// Deterministic in-process backend used by tests.
///
/// Two-axis behaviour:
///
/// - `complete` returns the configured `response` string on every
///   call, with a `prompt_tokens` count equal to the prompt's
///   whitespace-separated word count. Behaviour predates the chat
///   API and is unchanged.
/// - `chat` consumes responses in order from `chat_script`, a FIFO
///   queue populated by the test via [`FakeLlmBackend::with_chat_script`].
///   When the queue is empty, falls back to an assistant turn whose
///   `content` is the fake's static `response` string and no tool
///   calls — same shape `complete` would produce, suitable for tests
///   that don't care about multi-turn dynamics.
#[cfg(any(test, feature = "test-fakes"))]
pub struct FakeLlmBackend {
    model: String,
    response: String,
    finish_reason: FinishReason,
    chat_script: std::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
    /// `system` prompt of the most recent `complete` call. Tests that
    /// want to assert the orchestrator rendered placeholders into the
    /// system prompt (locale directive, future
    /// metadata.timezone, ...) read this back after the call.
    last_system_prompt: std::sync::Mutex<Option<String>>,
    /// User `prompt` of the most recent `complete` call. Tests that
    /// want to assert on the orchestrator's context bundle (the
    /// `current_time:` anchor, the roster sections, ...) read this
    /// back after the call.
    last_prompt: std::sync::Mutex<Option<String>>,
    /// Images of the most recent `complete` call — lets ingest tests
    /// assert the vision bytes actually reached the backend.
    last_images: std::sync::Mutex<Vec<ImageInput>>,
}

#[cfg(any(test, feature = "test-fakes"))]
impl FakeLlmBackend {
    /// Build a fake that always returns `response` with
    /// [`FinishReason::EndOfTurn`] from `complete`, and an empty
    /// `chat_script`.
    pub fn new(model: impl Into<String>, response: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            response: response.into(),
            finish_reason: FinishReason::EndOfTurn,
            chat_script: std::sync::Mutex::new(std::collections::VecDeque::new()),
            last_system_prompt: std::sync::Mutex::new(None),
            last_prompt: std::sync::Mutex::new(None),
            last_images: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Snapshot of the `system` prompt the last `complete` call
    /// received. `None` when no `complete` has run yet.
    #[must_use]
    pub fn last_system_prompt(&self) -> Option<String> {
        self.last_system_prompt
            .lock()
            .expect("last_system_prompt mutex poisoned")
            .clone()
    }

    /// Snapshot of the user `prompt` the last `complete` call
    /// received. `None` when no `complete` has run yet.
    #[must_use]
    pub fn last_prompt(&self) -> Option<String> {
        self.last_prompt
            .lock()
            .expect("last_prompt mutex poisoned")
            .clone()
    }

    /// Images the last `complete` call carried (empty when none).
    #[must_use]
    pub fn last_images(&self) -> Vec<ImageInput> {
        self.last_images
            .lock()
            .expect("last_images mutex poisoned")
            .clone()
    }

    /// Override the finish reason returned on every `complete` call.
    #[must_use]
    pub const fn with_finish_reason(mut self, reason: FinishReason) -> Self {
        self.finish_reason = reason;
        self
    }

    /// Queue a sequence of `chat` responses to be consumed in order
    /// across subsequent `chat` calls. Useful for tests that exercise
    /// the dashboard agentic loop: queue (`tool_call_turn`,
    /// `final_text_turn`) and assert the loop dispatched the tool,
    /// fed the result back, and rendered the final text.
    #[must_use]
    pub fn with_chat_script(mut self, responses: Vec<ChatResponse>) -> Self {
        self.chat_script = std::sync::Mutex::new(responses.into_iter().collect());
        self
    }
}

#[cfg(any(test, feature = "test-fakes"))]
#[async_trait]
impl LlmBackend for FakeLlmBackend {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        if request.prompt.is_empty() {
            return Err(LlmError::Invalid("empty prompt".into()));
        }
        self.last_system_prompt
            .lock()
            .expect("last_system_prompt mutex poisoned")
            .clone_from(&request.system);
        *self.last_prompt.lock().expect("last_prompt mutex poisoned") =
            Some(request.prompt.clone());
        self.last_images
            .lock()
            .expect("last_images mutex poisoned")
            .clone_from(&request.images);
        let prompt_words = u32::try_from(request.prompt.split_whitespace().count()).unwrap_or(0);
        Ok(CompletionResponse {
            text: self.response.clone(),
            finish_reason: self.finish_reason,
            usage: CompletionUsage {
                prompt_tokens: Some(prompt_words),
                completion_tokens: None,
            },
        })
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.messages.is_empty() {
            return Err(LlmError::Invalid("empty messages".into()));
        }
        let next = self
            .chat_script
            .lock()
            .expect("chat_script mutex poisoned")
            .pop_front();
        if let Some(resp) = next {
            return Ok(resp);
        }
        // Fallback: behave like `complete` would, just dressed as a
        // chat turn so callers that did not configure a script still
        // see a sensible round-trip.
        Ok(ChatResponse {
            message: ChatMessage::assistant(self.response.clone()),
            finish_reason: self.finish_reason,
            usage: CompletionUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fake_backend_returns_configured_response() {
        let llm = FakeLlmBackend::new("fake-1", "hello world");
        let resp = llm
            .complete(CompletionRequest::new("ciao"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "hello world");
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
        assert_eq!(resp.usage.prompt_tokens, Some(1));
    }

    #[tokio::test]
    async fn fake_backend_rejects_empty_prompt() {
        let llm = FakeLlmBackend::new("fake-1", "x");
        let err = llm
            .complete(CompletionRequest::new(""))
            .await
            .expect_err("must reject");
        assert!(matches!(err, LlmError::Invalid(_)));
    }

    #[tokio::test]
    async fn ollama_backend_posts_and_decodes_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(serde_json::json!({
                "model": "llama3",
                "prompt": "Hello",
                "stream": false,
                "think": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "Hi there!",
                "done": true,
                "done_reason": "stop",
                "prompt_eval_count": 5,
                "eval_count": 3
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "llama3").expect("new");
        let resp = llm
            .complete(CompletionRequest::new("Hello"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "Hi there!");
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
        assert_eq!(resp.usage.prompt_tokens, Some(5));
        assert_eq!(resp.usage.completion_tokens, Some(3));
    }

    #[tokio::test]
    async fn ollama_list_models_parses_tags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "qwen3.5:9b-q8_0", "size": 10_000_000_000_u64 },
                    { "name": "bge-m3:latest" }
                ]
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "").expect("new");
        let models = llm.list_models().await.expect("list");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen3.5:9b-q8_0");
        assert_eq!(models[0].size, Some(10_000_000_000));
        assert_eq!(models[1].name, "bge-m3:latest");
        assert_eq!(models[1].size, None);
    }

    #[tokio::test]
    async fn ollama_with_bearer_sends_authorization_header() {
        let server = MockServer::start().await;
        // The mock only matches when the Bearer header is present, so a
        // successful list proves the header was attached.
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer secret-token",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [ { "name": "m" } ]
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "")
            .expect("new")
            .with_bearer(Some("secret-token".to_owned()));
        let models = llm.list_models().await.expect("list with bearer");
        assert_eq!(models.len(), 1);
    }

    #[tokio::test]
    async fn ollama_blank_bearer_sends_no_auth_header() {
        let server = MockServer::start().await;
        // Require the header: a blank token is filtered to `None`, so no
        // header is sent, no mock matches, and the default 404 surfaces.
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .and(wiremock::matchers::header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "models": [] })),
            )
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "")
            .expect("new")
            .with_bearer(Some("   ".to_owned()));
        assert!(llm.list_models().await.is_err());
    }

    #[tokio::test]
    async fn ollama_backend_passes_system_and_options() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(serde_json::json!({
                "model": "llama3",
                "prompt": "Q?",
                "system": "You are terse.",
                "stream": false,
                "think": false,
                "options": { "num_predict": 64, "temperature": 0.2 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "A.",
                "done": true,
                "done_reason": "length"
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "llama3").expect("new");
        let req = CompletionRequest::new("Q?")
            .with_system("You are terse.")
            .with_max_tokens(64)
            .with_temperature(0.2);
        let resp = llm.complete(req).await.expect("complete");
        assert_eq!(resp.text, "A.");
        assert_eq!(resp.finish_reason, FinishReason::MaxTokens);
    }

    /// Thinking is disabled system-wide on every Ollama request
    /// per LLM functions. The flag must reach
    /// `/api/generate`'s body so thinking-capable models (Qwen 3.x, etc.)
    /// produce a non-empty `response` instead of consuming `num_predict`
    /// inside an unobserved reasoning block. Wiremock matches the body
    /// shape literally, so an accidental removal of the field shows up
    /// here as a missed-mock 404.
    #[tokio::test]
    async fn ollama_backend_pins_think_false_in_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(serde_json::json!({
                "model": "qwen3.5:9b-q8_0",
                "prompt": "extract intent",
                "stream": false,
                "think": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "{\"intent\":\"capture\"}",
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "qwen3.5:9b-q8_0").expect("new");
        let resp = llm
            .complete(CompletionRequest::new("extract intent"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "{\"intent\":\"capture\"}");
    }

    #[tokio::test]
    async fn ollama_backend_surfaces_http_errors_as_backend() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .mount(&server)
            .await;
        let llm = OllamaBackend::new(server.uri(), "x").expect("new");
        let err = llm
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Backend(_)), "{err:?}");
    }

    #[test]
    fn classify_finish_reason_covers_common_cases() {
        assert_eq!(
            classify_finish_reason(Some("stop")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_finish_reason(Some("end_of_turn")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_finish_reason(Some("length")),
            FinishReason::MaxTokens
        );
        assert_eq!(
            classify_finish_reason(Some("max_tokens")),
            FinishReason::MaxTokens
        );
        assert_eq!(classify_finish_reason(Some("weird")), FinishReason::Other);
        assert_eq!(classify_finish_reason(None), FinishReason::Other);
    }

    // ---- chat-with-tools API ----

    /// Plain chat round-trip: no tools, just a system + user message
    /// in, an assistant message out. Validates the body shape we send
    /// to `/api/chat` and the unwrapping of `message.content` into
    /// `ChatResponse.message.content`.
    #[tokio::test]
    async fn ollama_chat_round_trip_without_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(serde_json::json!({
                "model": "qwen3.5:9b-q8_0",
                "messages": [
                    { "role": "system", "content": "Be brief." },
                    { "role": "user", "content": "ciao" }
                ],
                "stream": false,
                "think": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "role": "assistant", "content": "ehi." },
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "qwen3.5:9b-q8_0").expect("new");
        let resp = llm
            .chat(ChatRequest::new(vec![
                ChatMessage::system("Be brief."),
                ChatMessage::user("ciao"),
            ]))
            .await
            .expect("chat");
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "ehi.");
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
    }

    /// Chat with one tool descriptor in the request and one
    /// `tool_calls` entry in the response — the contract the dashboard
    /// agentic loop expects. Tool-call ids are minted client-side
    /// (`call_0`, `call_1`, …) because Ollama does not generate them.
    #[tokio::test]
    async fn ollama_chat_round_trip_with_tool_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(serde_json::json!({
                "model": "qwen3.5:9b-q8_0",
                "messages": [
                    { "role": "user", "content": "trovami i libri" }
                ],
                "stream": false,
                "think": false,
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "wiki_recall",
                            "description": "Semantic recall.",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string" }
                                },
                                "required": ["query"]
                            }
                        }
                    }
                ],
                "options": { "temperature": 0.1 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "function": {
                                "name": "wiki_recall",
                                "arguments": { "query": "libri" }
                            }
                        }
                    ]
                },
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "qwen3.5:9b-q8_0").expect("new");
        let tool = Tool {
            name: "wiki_recall".into(),
            description: "Semantic recall.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            }),
        };
        let req = ChatRequest::new(vec![ChatMessage::user("trovami i libri")])
            .with_tools(vec![tool])
            .with_temperature(0.1);
        let resp = llm.chat(req).await.expect("chat");
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.tool_calls.len(), 1);
        let call = &resp.message.tool_calls[0];
        assert_eq!(call.name, "wiki_recall");
        assert_eq!(call.id, "call_0");
        assert_eq!(call.arguments, serde_json::json!({ "query": "libri" }));
    }

    /// Tool-result messages sent back to the model must serialise
    /// `role: "tool"` plus the tool's text content. The dashboard
    /// agentic loop re-issues the conversation with the assistant's
    /// previous turn (carrying the original `tool_calls`) plus a new
    /// `Tool` message bearing the result.
    #[tokio::test]
    async fn ollama_chat_round_trip_serializes_tool_result_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(serde_json::json!({
                "model": "qwen3.5:9b-q8_0",
                "messages": [
                    { "role": "user", "content": "trovami i libri" },
                    {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [
                            {
                                "function": {
                                    "name": "wiki_recall",
                                    "arguments": { "query": "libri" }
                                }
                            }
                        ]
                    },
                    {
                        "role": "tool",
                        "content": "[{\"fact_id\":\"f-1\",\"body\":\"Il Pendolo di Foucault\"}]"
                    }
                ],
                "stream": false,
                "think": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": "Ho trovato 1 fatto: \"Il Pendolo di Foucault\"."
                },
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let llm = OllamaBackend::new(server.uri(), "qwen3.5:9b-q8_0").expect("new");
        let assistant_turn = ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_0".into(),
                name: "wiki_recall".into(),
                arguments: serde_json::json!({ "query": "libri" }),
                thought_signature: None,
            }],
            tool_call_id: None,
        };
        let result_turn = ChatMessage::tool_result(
            "call_0",
            "[{\"fact_id\":\"f-1\",\"body\":\"Il Pendolo di Foucault\"}]",
        );
        let resp = llm
            .chat(ChatRequest::new(vec![
                ChatMessage::user("trovami i libri"),
                assistant_turn,
                result_turn,
            ]))
            .await
            .expect("chat");
        assert!(resp.message.tool_calls.is_empty());
        assert!(resp.message.content.contains("Ho trovato 1 fatto"));
    }

    /// `FakeLlmBackend::chat` consumes the scripted responses in
    /// order. Test that two queued responses are returned in FIFO,
    /// then the fallback (assistant text from `new()`) kicks in.
    #[tokio::test]
    async fn fake_backend_chat_consumes_scripted_responses_then_falls_back() {
        let first = ChatResponse {
            message: ChatMessage::assistant("first"),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        };
        let second = ChatResponse {
            message: ChatMessage::assistant("second"),
            finish_reason: FinishReason::EndOfTurn,
            usage: CompletionUsage::default(),
        };
        let llm = FakeLlmBackend::new("fake-1", "fallback").with_chat_script(vec![first, second]);

        let r1 = llm
            .chat(ChatRequest::new(vec![ChatMessage::user("ping")]))
            .await
            .expect("chat 1");
        let r2 = llm
            .chat(ChatRequest::new(vec![ChatMessage::user("ping")]))
            .await
            .expect("chat 2");
        let r3 = llm
            .chat(ChatRequest::new(vec![ChatMessage::user("ping")]))
            .await
            .expect("chat 3");
        assert_eq!(r1.message.content, "first");
        assert_eq!(r2.message.content, "second");
        assert_eq!(r3.message.content, "fallback");
    }

    /// Backends that do not override `chat` (every non-Ollama provider
    /// today) surface a clear `LlmError::Backend` so the dashboard
    /// agentic loop can refuse to start instead of silently degrading.
    #[tokio::test]
    async fn default_chat_impl_rejects_when_not_overridden() {
        // FakeLlmBackend *does* override chat now, so we cannot use it
        // to test the default. Build a trivial test backend that
        // implements `complete` but leaves `chat` as the default.
        struct StubBackend;
        #[async_trait]
        impl LlmBackend for StubBackend {
            fn model_id(&self) -> &'static str {
                "stub"
            }
            async fn complete(&self, _r: CompletionRequest) -> Result<CompletionResponse> {
                Ok(CompletionResponse {
                    text: String::new(),
                    finish_reason: FinishReason::EndOfTurn,
                    usage: CompletionUsage::default(),
                })
            }
        }
        let err = StubBackend
            .chat(ChatRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .expect_err("default must error");
        assert!(matches!(err, LlmError::Backend(_)), "{err:?}");
    }

    // ---- Anthropic backend ----

    fn fake_key() -> AnthropicApiKey {
        AnthropicApiKey::new("sk-ant-api03-fake-key").expect("non-empty key")
    }

    /// `AnthropicApiKey` rejects empty / whitespace-only input at
    /// construction so a misread env-var surfaces immediately rather
    /// than as an opaque 401 on first request.
    #[test]
    fn anthropic_api_key_rejects_empty() {
        let err = AnthropicApiKey::new("").expect_err("must reject");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
        let err = AnthropicApiKey::new("   ").expect_err("must reject whitespace");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `Debug` on the wrapper key never prints the raw bytes — same
    /// guarantee as `jwt::TokenSecret`.
    #[test]
    fn anthropic_api_key_debug_is_redacted() {
        let key = AnthropicApiKey::new("sk-ant-very-secret-1234567890").unwrap();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("sk-ant-very-secret-1234567890"), "{dbg}");
        assert!(dbg.contains("redacted"), "{dbg}");
    }

    /// Happy path: POST to `/v1/messages` with the headers and body
    /// shape the Anthropic API expects, and decode the `content[0].text`
    /// + `usage` block into a `CompletionResponse`.
    #[tokio::test]
    async fn anthropic_backend_posts_and_decodes_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::header(
                "anthropic-version",
                ANTHROPIC_API_VERSION,
            ))
            .and(wiremock::matchers::header(
                "x-api-key",
                "sk-ant-api03-fake-key",
            ))
            .and(body_json(serde_json::json!({
                "model": "claude-haiku-4-5",
                "max_tokens": 64,
                "messages": [
                    { "role": "user", "content": "Hello" }
                ],
                "system": "You are terse.",
                "temperature": 0.2,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "model": "claude-haiku-4-5",
                "content": [
                    { "type": "text", "text": "Hi." }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 7, "output_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-haiku-4-5", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let req = CompletionRequest::new("Hello")
            .with_system("You are terse.")
            .with_max_tokens(64)
            .with_temperature(0.2);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "Hi.");
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
        assert_eq!(resp.usage.prompt_tokens, Some(7));
        assert_eq!(resp.usage.completion_tokens, Some(2));
        assert_eq!(backend.model_id(), "claude-haiku-4-5");
    }

    /// Pin the image wire shapes of the three providers: Anthropic
    /// switches the user turn to content blocks (image first, text
    /// after); the literal body matcher fails the test if the shape
    /// drifts.
    #[tokio::test]
    async fn anthropic_backend_sends_image_content_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_json(serde_json::json!({
                "model": "claude-haiku-4-5",
                "max_tokens": 64,
                "messages": [
                    { "role": "user", "content": [
                        { "type": "image",
                          "source": { "type": "base64", "media_type": "image/jpeg", "data": "QUJD" } },
                        { "type": "text", "text": "describe this" }
                    ]}
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [ { "type": "text", "text": "a photo" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 7, "output_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-haiku-4-5", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let req = CompletionRequest::new("describe this")
            .with_max_tokens(64)
            .with_images(vec![ImageInput {
                mime_type: "image/jpeg".into(),
                data_base64: "QUJD".into(),
            }]);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "a photo");
    }

    #[tokio::test]
    async fn ollama_backend_sends_bare_base64_images() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(serde_json::json!({
                "model": "qwen3-vl",
                "prompt": "describe this",
                "stream": false,
                "think": false,
                "images": ["QUJD"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "a photo",
                "done": true,
            })))
            .mount(&server)
            .await;

        let backend = OllamaBackend::new(server.uri(), "qwen3-vl").expect("new");
        let req = CompletionRequest::new("describe this").with_images(vec![ImageInput {
            mime_type: "image/jpeg".into(),
            data_base64: "QUJD".into(),
        }]);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "a photo");
    }

    /// Concatenate multiple `text` content blocks into a single
    /// completion string — Anthropic may chunk longer answers across
    /// blocks even on non-streaming responses.
    #[tokio::test]
    async fn anthropic_backend_concatenates_multiple_text_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Hello," },
                    { "type": "text", "text": " world." }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 3, "output_tokens": 4 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "Hello, world.");
    }

    /// `max_tokens` is required by the Anthropic API; when the caller
    /// leaves it unset the backend falls back to
    /// `DEFAULT_MAX_TOKENS` so the request never hits the wire
    /// without one.
    #[tokio::test]
    async fn anthropic_backend_pins_default_max_tokens_when_caller_omits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_json(serde_json::json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": AnthropicBackend::DEFAULT_MAX_TOKENS,
                "messages": [
                    { "role": "user", "content": "ping" }
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "pong" }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-sonnet-4-6", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("ping"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "pong");
    }

    #[test]
    fn anthropic_thinking_budget_maps_reasoning_effort() {
        // Anthropic has no sub-1024 tier, so unset / minimal mean OFF.
        assert_eq!(anthropic_thinking_budget(None), None);
        assert_eq!(anthropic_thinking_budget(Some("")), None);
        assert_eq!(anthropic_thinking_budget(Some("minimal")), None);
        // The enabled ladder.
        assert_eq!(anthropic_thinking_budget(Some("low")), Some(2_048));
        assert_eq!(anthropic_thinking_budget(Some("medium")), Some(4_096));
        assert_eq!(anthropic_thinking_budget(Some("high")), Some(8_192));
        assert_eq!(anthropic_thinking_budget(Some("extra-high")), Some(16_384));
        // A typo floors to the lowest enabled budget — never silently off.
        assert_eq!(anthropic_thinking_budget(Some("hgih")), Some(2_048));
        // Surrounding whitespace is trimmed before matching.
        assert_eq!(anthropic_thinking_budget(Some("  high  ")), Some(8_192));
    }

    /// A slot with `reasoning_effort` set sends a `thinking` block whose
    /// budget is stacked on top of the caller's output ceiling, drops the
    /// caller's temperature (Anthropic rejects a custom one with thinking),
    /// and the response's `thinking` blocks are ignored on the way back.
    #[tokio::test]
    async fn anthropic_backend_enables_thinking_and_drops_temperature() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_json(serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": AnthropicBackend::DEFAULT_MAX_TOKENS + 16_384,
                "messages": [ { "role": "user", "content": "ping" } ],
                "thinking": { "type": "enabled", "budget_tokens": 16_384 },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "…", "signature": "sig" },
                    { "type": "text", "text": "pong" }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-8", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri())
            .with_reasoning_effort(Some("extra-high"));
        let resp = backend
            .complete(CompletionRequest::new("ping").with_temperature(0.5))
            .await
            .expect("complete");
        assert_eq!(resp.text, "pong");
    }

    /// The liveness probe never engages thinking even on a slot that set
    /// `reasoning_effort`: the body keeps `max_tokens: 1` + `temperature:
    /// 0.0` and carries no `thinking` field (`body_json` is exact-match).
    #[tokio::test]
    async fn anthropic_health_check_probe_never_thinks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // The probe pins no `temperature` (Opus 4.7+ reject sampling
            // params with a 400) and asks for a non-trivial `max_tokens`
            // (a `max_tokens: 1` reply can carry zero content blocks).
            // `with_reasoning_effort` below is ignored — the probe never
            // thinks, so there is no `thinking` field.
            .and(body_json(serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 16,
                "messages": [ { "role": "user", "content": "ping" } ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [ { "type": "text", "text": "pong" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-8", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri())
            .with_reasoning_effort(Some("extra-high"));
        backend.health_check().await.expect("health check");
    }

    #[test]
    fn anthropic_rejects_sampling_params_matches_the_no_temperature_family() {
        // Opus 4.7+ and the Fable / Mythos family reject `temperature`
        // (a 400); dated snapshots share the family prefix.
        for m in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-4-8-20260101",
        ] {
            assert!(anthropic_rejects_sampling_params(m), "{m} should reject");
        }
        // Sonnet 4.6, Opus 4.6, Haiku and the 3.x line still accept it.
        for m in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
            "claude-3-5-sonnet-20241022",
        ] {
            assert!(!anthropic_rejects_sampling_params(m), "{m} should accept");
        }
    }

    #[test]
    fn anthropic_oauth_token_detection_by_prefix() {
        // Console API keys (and non-Anthropic keys) → x-api-key path.
        assert!(!is_anthropic_oauth_token(""));
        assert!(!is_anthropic_oauth_token("sk-ant-api03-xxxx"));
        assert!(!is_anthropic_oauth_token("  sk-ant-api03-xxxx  "));
        assert!(!is_anthropic_oauth_token("AIzaSyGeminiKey"));
        assert!(!is_anthropic_oauth_token("anything-else"));
        // OAuth / Claude Code tokens → Bearer path.
        assert!(is_anthropic_oauth_token("sk-ant-oat-xxxx")); // setup token
        assert!(is_anthropic_oauth_token("sk-ant-xxxx")); // managed / oauth
        assert!(is_anthropic_oauth_token("eyJhbGciOiJ")); // OAuth JWT
        assert!(is_anthropic_oauth_token("cc-xxxx")); // Claude Code access token
    }

    /// An OAuth / Claude Code token routes to `Authorization: Bearer` + the
    /// Claude Code identity: the `system` array leads with the Claude Code
    /// prefix block (our prompt second), and the `x-app` fingerprint is set.
    #[tokio::test]
    async fn anthropic_oauth_token_uses_bearer_and_claude_code_identity() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sk-ant-oat-faketoken",
            ))
            .and(wiremock::matchers::header("x-app", "cli"))
            .and(body_json(serde_json::json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": AnthropicBackend::DEFAULT_MAX_TOKENS,
                "messages": [ { "role": "user", "content": "ping" } ],
                "system": [
                    { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
                    { "type": "text", "text": "be terse" }
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC", "type": "message", "role": "assistant",
                "content": [ { "type": "text", "text": "pong" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(
            AnthropicApiKey::new("sk-ant-oat-faketoken").expect("key"),
            "claude-sonnet-4-6",
            "CLAUDE_CODE_OAUTH_TOKEN",
        )
        .expect("new")
        .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("ping").with_system("be terse"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "pong");
    }

    /// A login-store backend resolves the access token from the workdir store
    /// (no env key) and sends it as `Authorization: Bearer` with the Claude
    /// Code identity — the storage seam behind "log in with Claude Code".
    #[tokio::test]
    async fn anthropic_login_store_backend_sends_resolved_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer stored-access",
            ))
            .and(wiremock::matchers::header("x-app", "cli"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "m", "type": "message", "role": "assistant",
                "content": [ { "type": "text", "text": "pong" } ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let dir = std::env::temp_dir().join(format!(
            "mwe-llm-oauth-{}",
            crate::oauth::generate_state().expect("rng")
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store =
            crate::oauth::OauthStore::new(&dir, crate::oauth::OauthClient::new(Client::new()));
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 3600;
        store
            .save(&crate::oauth::StoredOauth {
                access_token: "stored-access".into(),
                refresh_token: "r".into(),
                expires_at,
            })
            .expect("save");

        let backend = AnthropicBackend::with_login_store(
            std::sync::Arc::new(store),
            "claude-sonnet-4-6",
            "CLAUDE_CODE_LOGIN",
        )
        .expect("new")
        .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("ping"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "pong");
    }

    /// `max_tokens` finish reason is mapped to `FinishReason::MaxTokens`
    /// so REM / ingest can detect a truncated reply and warn / retry.
    #[tokio::test]
    async fn anthropic_backend_maps_max_tokens_stop_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "truncated…" }
                ],
                "stop_reason": "max_tokens",
                "usage": { "input_tokens": 4, "output_tokens": 50 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");
        assert_eq!(resp.finish_reason, FinishReason::MaxTokens);
    }

    /// 401 is surfaced as `LlmError::Auth` with the env-var name in
    /// the message so the operator knows which key to fix in
    /// `mwe-mcp.env`.
    #[tokio::test]
    async fn anthropic_backend_maps_401_to_auth_error_with_env_var_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"type":"authentication_error"}}"#),
            )
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "MWE_ANTHROPIC_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Auth(msg) => {
                assert!(msg.contains("MWE_ANTHROPIC_KEY"), "{msg}");
                assert!(msg.contains("mwe-mcp.env"), "{msg}");
            },
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    /// 403 also maps to `LlmError::Auth` — same diagnostic message
    /// shape as 401.
    #[tokio::test]
    async fn anthropic_backend_maps_403_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    }

    /// 429 is surfaced as `LlmError::RateLimit` (distinct from generic
    /// `Backend`) so the REM scheduler can decide to back off rather
    /// than abort the cycle as if the deploy were broken.
    #[tokio::test]
    async fn anthropic_backend_maps_429_to_rate_limit_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::RateLimit(_)), "{err:?}");
    }

    /// 5xx surfaces as the generic `LlmError::Backend` — neither auth
    /// nor rate-limit; the REM scheduler's fatal-error path will
    /// abort the cycle.
    #[tokio::test]
    async fn anthropic_backend_maps_500_to_generic_backend_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Backend(_)), "{err:?}");
    }

    /// 400 is invalid-request, not infra: surface as
    /// `LlmError::Invalid` so the caller knows the request shape is
    /// wrong (model id, prompt too long, etc.) and can refuse to
    /// retry.
    #[tokio::test]
    async fn anthropic_backend_maps_400_to_invalid_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"type":"invalid_request"}}"#),
            )
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// A `complete` response with no `text` content block (e.g. an
    /// accidental tool-use-only turn) is surfaced as `LlmError::Protocol`
    /// so the caller knows the contract was broken — never silently
    /// degrade to an empty string. The `chat` path, by contrast,
    /// expects `tool_use` blocks as a normal outcome.
    #[tokio::test]
    async fn anthropic_backend_protocol_error_on_no_text_block() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01ABC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "toolu_01", "name": "x", "input": {} }
                ],
                "stop_reason": "tool_use"
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
    }

    /// Empty prompts are rejected at the boundary — never sent to
    /// Anthropic, never charged.
    #[tokio::test]
    async fn anthropic_backend_rejects_empty_prompt() {
        let backend =
            AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY").expect("new");
        let err = backend
            .complete(CompletionRequest::new(""))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `health_check` issues a 1-token completion — the wiremock path
    /// matches the same `/v1/messages` endpoint and a successful 200
    /// response makes the boot-time probe succeed.
    #[tokio::test]
    async fn anthropic_backend_health_check_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01HC",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "." }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        backend.health_check().await.expect("probe must succeed");
    }

    /// `health_check` propagates auth failures from `complete` — the
    /// boot-time guard refuses to bind when the API key is missing.
    #[tokio::test]
    async fn anthropic_backend_health_check_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("auth"))
            .mount(&server)
            .await;
        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend.health_check().await.expect_err("must fail");
        assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    }

    #[test]
    fn classify_anthropic_stop_reason_covers_documented_cases() {
        assert_eq!(
            classify_anthropic_stop_reason(Some("end_turn")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_anthropic_stop_reason(Some("stop_sequence")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_anthropic_stop_reason(Some("tool_use")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_anthropic_stop_reason(Some("max_tokens")),
            FinishReason::MaxTokens
        );
        assert_eq!(
            classify_anthropic_stop_reason(Some("unknown")),
            FinishReason::Other
        );
        assert_eq!(classify_anthropic_stop_reason(None), FinishReason::Other);
    }

    // ---- Anthropic chat (tool-calling) ----

    /// `chat` happy path: a System + User history posts the System as
    /// the top-level `system` field, encodes User as a `messages` entry
    /// with plain-text content, and decodes a pure-text response into
    /// `ChatMessage::assistant(...)`.
    #[tokio::test]
    async fn anthropic_chat_splits_system_and_returns_plain_text_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // opus-4-7 rejects sampling params, so the adapter drops the
            // caller's `temperature: 0.1` from the wire body (see
            // `anthropic_rejects_sampling_params`).
            .and(body_json(serde_json::json!({
                "model": "claude-opus-4-7",
                "max_tokens": 256,
                "messages": [
                    { "role": "user", "content": "Hi." }
                ],
                "system": "Be terse.",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_chat_1",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Hello." }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 5, "output_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let req = ChatRequest::new(vec![
            ChatMessage::system("Be terse."),
            ChatMessage::user("Hi."),
        ])
        .with_max_tokens(256)
        .with_temperature(0.1);
        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "Hello.");
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
    }

    /// `chat` decodes Anthropic's typed content blocks into
    /// `ToolCall` entries when the model decides to invoke a tool.
    /// The Anthropic-minted `tool_use.id` is preserved verbatim — the
    /// agentic loop echoes it back as `ChatMessage::tool_call_id` on
    /// the matching `Role::Tool` reply.
    #[tokio::test]
    async fn anthropic_chat_decodes_tool_use_blocks_with_preserved_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_chat_tu",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Looking up." },
                    {
                        "type": "tool_use",
                        "id": "toolu_01ABC",
                        "name": "wiki_recall",
                        "input": { "query": "alice coffee", "top_k": 5 }
                    }
                ],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 50, "output_tokens": 30 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let req = ChatRequest::new(vec![
            ChatMessage::system("Use tools."),
            ChatMessage::user("What do you know about alice's coffee?"),
        ])
        .with_tools(vec![Tool {
            name: "wiki_recall".into(),
            description: "Search the wiki for facts".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "top_k": { "type": "integer" }
                },
                "required": ["query"]
            }),
        }]);
        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.content, "Looking up.");
        assert_eq!(resp.message.tool_calls.len(), 1);
        let call = &resp.message.tool_calls[0];
        assert_eq!(call.id, "toolu_01ABC");
        assert_eq!(call.name, "wiki_recall");
        assert_eq!(call.arguments["query"], "alice coffee");
        assert_eq!(call.arguments["top_k"], 5);
        // tool_use stop_reason still maps to EndOfTurn — the agentic
        // loop branches on tool_calls.is_empty(), not on the finish
        // reason.
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
    }

    /// Outbound wire shape: tool descriptors are serialised with
    /// `input_schema` (not `parameters`), and an Assistant message
    /// that carried `tool_calls` is re-encoded as a content list mixing
    /// `text` and `tool_use` blocks. A subsequent `Role::Tool`
    /// message becomes a `user` role with a `tool_result` block keyed
    /// on `tool_use_id` — that's how Anthropic correlates the result
    /// with the prior call.
    #[tokio::test]
    async fn anthropic_chat_round_trips_assistant_tool_call_and_tool_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_json(serde_json::json!({
                "model": "claude-opus-4-7",
                "max_tokens": AnthropicBackend::DEFAULT_MAX_TOKENS,
                "messages": [
                    { "role": "user", "content": "find it" },
                    {
                        "role": "assistant",
                        "content": [
                            { "type": "text", "text": "checking" },
                            {
                                "type": "tool_use",
                                "id": "toolu_01ABC",
                                "name": "wiki_recall",
                                "input": { "query": "alice" }
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "toolu_01ABC",
                                "content": "no hits"
                            }
                        ]
                    }
                ],
                "system": "Be terse.",
                "tools": [
                    {
                        "name": "wiki_recall",
                        "description": "Search the wiki",
                        "input_schema": {
                            "type": "object",
                            "properties": { "query": { "type": "string" } }
                        }
                    }
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_chat_rt",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Nothing on file." }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 80, "output_tokens": 4 }
            })))
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());

        let assistant_with_tool = ChatMessage {
            role: Role::Assistant,
            content: "checking".into(),
            tool_calls: vec![ToolCall {
                id: "toolu_01ABC".into(),
                name: "wiki_recall".into(),
                arguments: serde_json::json!({ "query": "alice" }),
                thought_signature: None,
            }],
            tool_call_id: None,
        };

        let req = ChatRequest::new(vec![
            ChatMessage::system("Be terse."),
            ChatMessage::user("find it"),
            assistant_with_tool,
            ChatMessage::tool_result("toolu_01ABC", "no hits"),
        ])
        .with_tools(vec![Tool {
            name: "wiki_recall".into(),
            description: "Search the wiki".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }),
        }]);

        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.content, "Nothing on file.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// `chat` refuses an empty history at the boundary so callers
    /// surface their bug instead of paying for a zero-message call.
    #[tokio::test]
    async fn anthropic_chat_rejects_empty_messages() {
        let backend =
            AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY").expect("new");
        let err = backend
            .chat(ChatRequest::new(Vec::new()))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `chat` shares the same error mapping as `complete`: 401 →
    /// `LlmError::Auth` with the env-var name in the message.
    #[tokio::test]
    async fn anthropic_chat_maps_401_to_auth_error_with_env_var_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "MWE_ANTHROPIC_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .chat(ChatRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Auth(msg) => {
                assert!(msg.contains("MWE_ANTHROPIC_KEY"), "{msg}");
                assert!(msg.contains("mwe-mcp.env"), "{msg}");
            },
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    /// 429 on `chat` maps to `LlmError::RateLimit` — same as `complete`.
    /// REM scheduler back-off path depends on this.
    #[tokio::test]
    async fn anthropic_chat_maps_429_to_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;
        let backend = AnthropicBackend::new(fake_key(), "claude-opus-4-7", "ANTHROPIC_API_KEY")
            .expect("new")
            .with_base_url(server.uri());
        let err = backend
            .chat(ChatRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::RateLimit(_)), "{err:?}");
    }

    /// `split_anthropic_messages` is the wire-shape converter the
    /// `chat` body builder runs every call. The unit test covers the
    /// three non-trivial branches in isolation: System aggregation,
    /// Assistant + `tool_calls` flattening, and `Role::Tool` recoding
    /// as a `user` message with a `tool_result` block.
    #[test]
    fn split_anthropic_messages_strips_system_and_encodes_tool_blocks() {
        let assistant_with_tool = ChatMessage {
            role: Role::Assistant,
            content: "thinking".into(),
            tool_calls: vec![ToolCall {
                id: "toolu_42".into(),
                name: "lookup".into(),
                arguments: serde_json::json!({ "q": "x" }),
                thought_signature: None,
            }],
            tool_call_id: None,
        };
        let history = vec![
            ChatMessage::system("first"),
            ChatMessage::system("second"),
            ChatMessage::user("hi"),
            assistant_with_tool,
            ChatMessage::tool_result("toolu_42", "result"),
        ];
        let (system, messages) = split_anthropic_messages(&history);
        assert_eq!(system.as_deref(), Some("first\n\nsecond"));
        assert_eq!(messages.len(), 3);
        // Round-trip through serde so we can assert on the wire shape
        // without exposing the private DTOs.
        let wire = serde_json::to_value(&messages).expect("serialise");
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "hi");
        assert_eq!(wire[1]["role"], "assistant");
        assert_eq!(wire[1]["content"][0]["type"], "text");
        assert_eq!(wire[1]["content"][0]["text"], "thinking");
        assert_eq!(wire[1]["content"][1]["type"], "tool_use");
        assert_eq!(wire[1]["content"][1]["id"], "toolu_42");
        assert_eq!(wire[1]["content"][1]["name"], "lookup");
        assert_eq!(wire[1]["content"][1]["input"]["q"], "x");
        assert_eq!(wire[2]["role"], "user");
        assert_eq!(wire[2]["content"][0]["type"], "tool_result");
        assert_eq!(wire[2]["content"][0]["tool_use_id"], "toolu_42");
        assert_eq!(wire[2]["content"][0]["content"], "result");
    }

    // ---- Gemini backend ----

    fn fake_gemini_key() -> GeminiApiKey {
        GeminiApiKey::new("AIza-fake-gemini-key").expect("non-empty key")
    }

    /// A Gemini backend wired to a mock server. Folds the
    /// `new().expect().with_base_url()` boilerplate the chat tests
    /// repeat so the test bodies stay under the clippy line cap.
    fn gemini_test_backend(base_url: String) -> GeminiBackend {
        GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(base_url)
    }

    /// `GeminiApiKey` rejects empty / whitespace-only input at
    /// construction — symmetric with [`AnthropicApiKey`].
    #[test]
    fn gemini_api_key_rejects_empty() {
        let err = GeminiApiKey::new("").expect_err("must reject");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
        let err = GeminiApiKey::new("   ").expect_err("must reject whitespace");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `Debug` on the wrapper key never prints the raw bytes — same
    /// guarantee as the Anthropic newtype.
    #[test]
    fn gemini_api_key_debug_is_redacted() {
        let key = GeminiApiKey::new("AIza-very-secret-1234567890").unwrap();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("AIza-very-secret-1234567890"), "{dbg}");
        assert!(dbg.contains("redacted"), "{dbg}");
    }

    /// `classify_gemini_finish_reason` exposes the public-facing
    /// `EndOfTurn` mapping for `STOP`. `MAX_TOKENS` is *intentionally*
    /// not mapped here — it is intercepted before classification and
    /// surfaced as `LlmError::Backend`; this test guards the absence
    /// of an accidental `MaxTokens` mapping that would let unparseable
    /// truncated output through.
    #[test]
    fn classify_gemini_finish_reason_maps_stop_only() {
        assert_eq!(
            classify_gemini_finish_reason(Some("STOP")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_gemini_finish_reason(Some("MAX_TOKENS")),
            FinishReason::Other,
            "MAX_TOKENS must NOT map to MaxTokens — it is surfaced as a backend error upstream"
        );
        assert_eq!(
            classify_gemini_finish_reason(Some("OTHER")),
            FinishReason::Other
        );
        assert_eq!(classify_gemini_finish_reason(None), FinishReason::Other);
    }

    /// `reasoning_effort` maps onto `thinkingLevel`, defaulting to
    /// `minimal` (Flash-safe). Gemini 3.x Pro rejects `minimal`, so a
    /// Pro slot must carry a non-minimal effort — this is the boundary
    /// that translates the config hint into a level Pro accepts.
    #[test]
    fn gemini_reasoning_effort_maps_to_thinking_level() {
        let key = GeminiApiKey::new("AIza-fake").expect("key");
        let level = |e: Option<&str>| {
            GeminiBackend::new(key.clone(), "m", "ENV")
                .expect("backend")
                .with_reasoning_effort(e)
                .thinking_level
        };
        assert_eq!(level(None), "minimal");
        assert_eq!(level(Some("")), "minimal");
        assert_eq!(level(Some("minimal")), "minimal");
        assert_eq!(level(Some("low")), "low");
        assert_eq!(level(Some(" medium ")), "medium");
        assert_eq!(level(Some("high")), "high");
        assert_eq!(level(Some("extra-high")), "high");
        // Unknown values fall to a safe non-minimal floor so a typo
        // never locks a Pro model out of boot.
        assert_eq!(level(Some("bogus")), "low");
    }

    /// Happy path: POST to `/v1beta/models/{model}:generateContent?key=...`
    /// with the mandated `generationConfig` (temperature 1.0, max tokens
    /// 65536, thinkingLevel minimal) regardless of what the caller
    /// passed for `temperature` / `max_tokens`. Decodes
    /// `candidates[0].content.parts[0].text` into the completion text.
    #[tokio::test]
    async fn gemini_backend_posts_with_mandated_generation_config() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .and(wiremock::matchers::query_param(
                "key",
                "AIza-fake-gemini-key",
            ))
            .and(body_json(serde_json::json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "Hello" }] }
                ],
                "systemInstruction": {
                    "parts": [{ "text": "Be terse." }]
                },
                "generationConfig": {
                    "temperature": GEMINI_TEMPERATURE,
                    "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
                    "thinkingConfig": { "thinkingLevel": GEMINI_THINKING_LEVEL }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [{ "text": "Hi." }],
                            "role": "model"
                        },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": {
                    "promptTokenCount": 7,
                    "candidatesTokenCount": 2
                }
            })))
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        // Caller asks for a sub-1.0 temperature and a tiny max_tokens; both
        // are intentionally ignored — Gemini gets the mandated values.
        let req = CompletionRequest::new("Hello")
            .with_system("Be terse.")
            .with_max_tokens(32)
            .with_temperature(0.2);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "Hi.");
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
        assert_eq!(resp.usage.prompt_tokens, Some(7));
        assert_eq!(resp.usage.completion_tokens, Some(2));
        assert_eq!(backend.model_id(), "gemini-3-flash-preview");
    }

    /// Pin the `inlineData` wire shape of the vision path: image parts
    /// precede the text part inside the same user content.
    #[tokio::test]
    async fn gemini_backend_sends_inline_data_parts_before_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .and(body_json(serde_json::json!({
                "contents": [
                    { "role": "user", "parts": [
                        { "inlineData": { "mimeType": "image/jpeg", "data": "QUJD" } },
                        { "text": "describe this" }
                    ]}
                ],
                "generationConfig": {
                    "temperature": GEMINI_TEMPERATURE,
                    "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
                    "thinkingConfig": { "thinkingLevel": GEMINI_THINKING_LEVEL }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    { "content": { "parts": [{ "text": "a photo" }], "role": "model" },
                      "finishReason": "STOP" }
                ],
            })))
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let req = CompletionRequest::new("describe this").with_images(vec![ImageInput {
            mime_type: "image/jpeg".into(),
            data_base64: "QUJD".into(),
        }]);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "a photo");
    }

    /// `finishReason: MAX_TOKENS` is intercepted and surfaced as
    /// `LlmError::Backend` — never as `FinishReason::MaxTokens` —
    /// because on Gemini it almost always indicates a truncated
    /// unparseable output caused by thinking consuming the combined
    /// budget. Caller is expected to retry.
    #[tokio::test]
    async fn gemini_backend_max_tokens_surfaces_as_backend_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [{ "text": "trunca…" }],
                            "role": "model"
                        },
                        "finishReason": "MAX_TOKENS"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("MAX_TOKENS"), "{msg}");
                assert!(msg.contains("retriable"), "{msg}");
            },
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    /// Concatenate multiple `text` parts in the first candidate into a
    /// single completion string — Gemini occasionally splits longer
    /// answers across parts.
    #[tokio::test]
    async fn gemini_backend_concatenates_multiple_text_parts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [
                                { "text": "Hello," },
                                { "text": " world." }
                            ],
                            "role": "model"
                        },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 4 }
            })))
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let resp = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "Hello, world.");
    }

    /// 401 is surfaced as `LlmError::Auth` with the env-var name in
    /// the message so the operator knows which key to fix in
    /// `mwe-mcp.env`.
    #[tokio::test]
    async fn gemini_backend_maps_401_to_auth_error_with_env_var_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"code":401,"message":"invalid api key"}}"#),
            )
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "MWE_GEMINI_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Auth(msg) => {
                assert!(msg.contains("MWE_GEMINI_KEY"), "{msg}");
                assert!(msg.contains("mwe-mcp.env"), "{msg}");
            },
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    /// 429 is surfaced as `LlmError::RateLimit` (distinct from generic
    /// `Backend`) so the REM scheduler can back off rather than abort
    /// the cycle as if the deploy were broken.
    #[tokio::test]
    async fn gemini_backend_maps_429_to_rate_limit_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::RateLimit(_)), "{err:?}");
    }

    /// 400 is invalid-request: surface as `LlmError::Invalid` so the
    /// caller knows the request shape is wrong (model id, too long,
    /// orphaned function-response part, etc.) and does not retry blindly.
    #[tokio::test]
    async fn gemini_backend_maps_400_to_invalid_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"code":400,"message":"invalid argument"}}"#),
            )
            .mount(&server)
            .await;

        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// 5xx surfaces as the generic `LlmError::Backend`.
    #[tokio::test]
    async fn gemini_backend_maps_500_to_generic_backend_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Backend(_)), "{err:?}");
    }

    /// A 200 response with no `text` part in the first candidate
    /// surfaces as `LlmError::Protocol` — never silently return empty.
    #[tokio::test]
    async fn gemini_backend_protocol_error_on_no_text_part() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [
                                { "functionCall": { "name": "x", "args": {} } }
                            ],
                            "role": "model"
                        },
                        "finishReason": "STOP"
                    }
                ]
            })))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
    }

    /// Empty prompts are rejected at the boundary — never sent to
    /// Gemini, never charged.
    #[tokio::test]
    async fn gemini_backend_rejects_empty_prompt() {
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new");
        let err = backend
            .complete(CompletionRequest::new(""))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `health_check` overrides the default trait probe so it never
    /// sends `temperature: 0.0` (which Gemini rejects with degraded
    /// behaviour). Wiremock matches the mandated body shape; a 200
    /// makes the boot-time probe succeed.
    #[tokio::test]
    async fn gemini_backend_health_check_succeeds_on_200_with_mandated_temperature() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .and(body_json(serde_json::json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "." }] }
                ],
                "generationConfig": {
                    "temperature": GEMINI_TEMPERATURE,
                    "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
                    "thinkingConfig": { "thinkingLevel": GEMINI_THINKING_LEVEL }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": { "parts": [{ "text": "." }], "role": "model" },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1 }
            })))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        backend.health_check().await.expect("probe must succeed");
    }

    /// `health_check` propagates auth failures from `complete` — the
    /// boot-time guard refuses to bind when the API key is missing.
    #[tokio::test]
    async fn gemini_backend_health_check_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(401).set_body_string("auth"))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend.health_check().await.expect_err("must fail");
        assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    }

    // ---- Gemini chat (tool-calling) ----

    /// `chat` happy path: System + User splits into `systemInstruction`
    /// + a single `user` content; the model's text reply decodes into
    /// `ChatMessage::assistant(...)`.
    #[tokio::test]
    async fn gemini_chat_splits_system_and_returns_plain_text_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .and(body_json(serde_json::json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "Hi." }] }
                ],
                "systemInstruction": {
                    "parts": [{ "text": "Be terse." }]
                },
                "generationConfig": {
                    "temperature": GEMINI_TEMPERATURE,
                    "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
                    "thinkingConfig": { "thinkingLevel": GEMINI_THINKING_LEVEL }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": { "parts": [{ "text": "Hello." }], "role": "model" },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 2 }
            })))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let req = ChatRequest::new(vec![
            ChatMessage::system("Be terse."),
            ChatMessage::user("Hi."),
        ]);
        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "Hello.");
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
    }

    /// `chat` decodes Gemini's `functionCall` parts into `ToolCall`
    /// entries. Tool-call ids are minted client-side as `call_<idx>`
    /// (Gemini does not mint ids; it correlates by `name`).
    #[tokio::test]
    async fn gemini_chat_decodes_function_call_parts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [
                                { "text": "Looking up." },
                                {
                                    "functionCall": {
                                        "name": "wiki_recall",
                                        "args": { "query": "alice coffee", "top_k": 5 }
                                    }
                                }
                            ],
                            "role": "model"
                        },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 50, "candidatesTokenCount": 30 }
            })))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let req =
            ChatRequest::new(vec![ChatMessage::user("alice coffee?")]).with_tools(vec![Tool {
                name: "wiki_recall".into(),
                description: "Search the wiki for facts".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "top_k": { "type": "integer" }
                    },
                    "required": ["query"]
                }),
            }]);
        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.content, "Looking up.");
        assert_eq!(resp.message.tool_calls.len(), 1);
        let call = &resp.message.tool_calls[0];
        // idx 0 is the text part, idx 1 is the functionCall part.
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "wiki_recall");
        assert_eq!(call.arguments["query"], "alice coffee");
        assert_eq!(call.arguments["top_k"], 5);
    }

    /// `chat` round-trips an Assistant turn that carried a tool call
    /// plus the follow-up `Role::Tool` reply. The tool descriptor is
    /// serialised under `tools[].functionDeclarations[]` with `parameters`
    /// as the JSON Schema; the assistant turn becomes a `model`-role
    /// `contents[]` entry mixing `text` and `functionCall` parts; the
    /// `Tool` reply becomes a `user`-role entry with a single
    /// `functionResponse` part keyed on the function name (Gemini
    /// correlates by `name`, not by id).
    #[tokio::test]
    async fn gemini_chat_round_trips_function_call_and_function_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .and(body_json(serde_json::json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "find it" }] },
                    {
                        "role": "model",
                        "parts": [
                            { "text": "checking" },
                            {
                                "functionCall": {
                                    "name": "wiki_recall",
                                    "args": { "query": "alice" }
                                }
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "parts": [
                            {
                                "functionResponse": {
                                    "name": "wiki_recall",
                                    "response": { "content": "no hits" }
                                }
                            }
                        ]
                    }
                ],
                "systemInstruction": {
                    "parts": [{ "text": "Be terse." }]
                },
                "generationConfig": {
                    "temperature": GEMINI_TEMPERATURE,
                    "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
                    "thinkingConfig": { "thinkingLevel": GEMINI_THINKING_LEVEL }
                },
                "tools": [
                    {
                        "functionDeclarations": [
                            {
                                "name": "wiki_recall",
                                "description": "Search the wiki",
                                "parameters": {
                                    "type": "object",
                                    "properties": { "query": { "type": "string" } }
                                }
                            }
                        ]
                    }
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": { "parts": [{ "text": "Nothing on file." }], "role": "model" },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 80, "candidatesTokenCount": 4 }
            })))
            .mount(&server)
            .await;

        let backend = gemini_test_backend(server.uri());

        let assistant_with_tool = ChatMessage {
            role: Role::Assistant,
            content: "checking".into(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "wiki_recall".into(),
                arguments: serde_json::json!({ "query": "alice" }),
                thought_signature: None,
            }],
            tool_call_id: None,
        };
        let req = ChatRequest::new(vec![
            ChatMessage::system("Be terse."),
            ChatMessage::user("find it"),
            assistant_with_tool,
            ChatMessage::tool_result("call_1", "no hits"),
        ])
        .with_tools(vec![Tool {
            name: "wiki_recall".into(),
            description: "Search the wiki".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }),
        }]);

        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.content, "Nothing on file.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// `chat` refuses an empty history at the boundary so callers
    /// surface their bug instead of paying for a zero-message call
    /// (Gemini would 400 with `INVALID_ARGUMENT` on `contents: []`).
    #[tokio::test]
    async fn gemini_chat_rejects_empty_messages() {
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "GEMINI_API_KEY",
        )
        .expect("new");
        let err = backend
            .chat(ChatRequest::new(Vec::new()))
            .await
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }

    /// `chat` shares the same auth-error mapping as `complete`: 401
    /// surfaces as `LlmError::Auth` with the env-var name in the message.
    #[tokio::test]
    async fn gemini_chat_maps_401_to_auth_error_with_env_var_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let backend = GeminiBackend::new(
            fake_gemini_key(),
            "gemini-3-flash-preview",
            "MWE_GEMINI_KEY",
        )
        .expect("new")
        .with_base_url(server.uri());
        let err = backend
            .chat(ChatRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Auth(msg) => {
                assert!(msg.contains("MWE_GEMINI_KEY"), "{msg}");
                assert!(msg.contains("mwe-mcp.env"), "{msg}");
            },
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    /// `split_gemini_messages` is the wire-shape converter for the
    /// `chat` body builder. Covers the three non-trivial branches in
    /// isolation: System aggregation (stripped + concatenated),
    /// Assistant + `tool_calls` flattening (text + functionCall parts
    /// under `role: "model"`), and `Role::Tool` recoding as a `user`
    /// message with a `functionResponse` part whose `name` resolves
    /// from the prior assistant turn's `tool_call_id → name` map.
    #[test]
    fn split_gemini_messages_strips_system_and_encodes_function_parts() {
        let assistant_with_tool = ChatMessage {
            role: Role::Assistant,
            content: "thinking".into(),
            tool_calls: vec![ToolCall {
                id: "id_42".into(),
                name: "lookup".into(),
                arguments: serde_json::json!({ "q": "x" }),
                thought_signature: None,
            }],
            tool_call_id: None,
        };
        let history = vec![
            ChatMessage::system("first"),
            ChatMessage::system("second"),
            ChatMessage::user("hi"),
            assistant_with_tool,
            ChatMessage::tool_result("id_42", "result"),
        ];
        let (system, contents) = split_gemini_messages(&history);
        assert_eq!(system.as_deref(), Some("first\n\nsecond"));
        assert_eq!(contents.len(), 3);
        // Round-trip through serde so we can assert on wire shape
        // without exposing the private DTOs.
        let wire = serde_json::to_value(&contents).expect("serialise");
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["parts"][0]["text"], "hi");
        assert_eq!(wire[1]["role"], "model");
        assert_eq!(wire[1]["parts"][0]["text"], "thinking");
        assert_eq!(wire[1]["parts"][1]["functionCall"]["name"], "lookup");
        assert_eq!(wire[1]["parts"][1]["functionCall"]["args"]["q"], "x");
        assert_eq!(wire[2]["role"], "user");
        assert_eq!(
            wire[2]["parts"][0]["functionResponse"]["name"], "lookup",
            "tool result must resolve to the function name via tool_call_id lookup",
        );
        assert_eq!(
            wire[2]["parts"][0]["functionResponse"]["response"]["content"],
            "result"
        );
    }

    /// Gemini 3 (with thinking) attaches an opaque `thoughtSignature` to
    /// each `functionCall` part and rejects the replayed turn with a
    /// hard 400 if it is not echoed back verbatim. This test proves both
    /// halves of the round-trip without hitting Google:
    ///
    /// - **capture**: a `chat` response whose `functionCall` part carries
    ///   a sibling `thoughtSignature` decodes into a `ToolCall` whose
    ///   `thought_signature` holds that value;
    /// - **echo**: an assistant turn whose `ToolCall` carries a
    ///   `thought_signature` re-serialises with the signature as a
    ///   sibling of `functionCall` in the outbound part.
    #[tokio::test]
    async fn gemini_chat_round_trips_thought_signature_on_function_call() {
        // ---- capture: inbound functionCall part → ToolCall.thought_signature
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-3-flash-preview:generateContent",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [
                    {
                        "content": {
                            "parts": [
                                {
                                    "functionCall": { "name": "foo", "args": {} },
                                    "thoughtSignature": "SIG123"
                                }
                            ],
                            "role": "model"
                        },
                        "finishReason": "STOP"
                    }
                ],
                "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5 }
            })))
            .mount(&server)
            .await;
        let backend = gemini_test_backend(server.uri());
        let resp = backend
            .chat(
                ChatRequest::new(vec![ChatMessage::user("go")]).with_tools(vec![Tool {
                    name: "foo".into(),
                    description: "does foo".into(),
                    parameters: serde_json::json!({ "type": "object", "properties": {} }),
                }]),
            )
            .await
            .expect("chat");
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(
            resp.message.tool_calls[0].thought_signature.as_deref(),
            Some("SIG123"),
            "the inbound functionCall part's thoughtSignature must be captured onto the ToolCall",
        );

        // ---- echo: ToolCall.thought_signature → outbound functionCall sibling
        let assistant_turn = ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_0".into(),
                name: "foo".into(),
                arguments: serde_json::json!({}),
                thought_signature: Some("SIG123".into()),
            }],
            tool_call_id: None,
        };
        let echo_history = [ChatMessage::user("go"), assistant_turn];
        let (_system, contents) = split_gemini_messages(&echo_history);
        let wire = serde_json::to_value(&contents).expect("serialise");
        let part = &wire[1]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "foo");
        assert_eq!(
            part["thoughtSignature"], "SIG123",
            "the ToolCall's thought_signature must re-emit as a sibling of functionCall, \
             not nested inside it",
        );
        // Belt-and-suspenders: the signature must NOT leak inside the
        // functionCall object (Gemini wants it as a sibling).
        assert!(
            part["functionCall"]["thoughtSignature"].is_null(),
            "thoughtSignature must not be nested inside functionCall",
        );

        // A ToolCall without a signature must omit the key entirely
        // (skip_serializing_if), not emit `null`.
        let plain = ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_0".into(),
                name: "foo".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }],
            tool_call_id: None,
        };
        let plain_history = [plain];
        let (_s, plain_contents) = split_gemini_messages(&plain_history);
        let plain_wire = serde_json::to_value(&plain_contents).expect("serialise");
        assert!(
            plain_wire[0]["parts"][0].get("thoughtSignature").is_none(),
            "absent signature must omit the key, not serialise null",
        );
    }

    // ---- OpenRouter backend ----

    /// An `OpenRouter` backend wired to a mock server, folding the
    /// `new().expect().with_base_url()` boilerplate the round-trip tests
    /// repeat.
    fn openrouter_test_backend(base_url: String) -> OpenRouterBackend {
        OpenRouterBackend::new(
            OpenRouterApiKey::new("sk-or-fake-key").expect("non-empty key"),
            "anthropic/claude-sonnet-4-6",
            "OPENROUTER_API_KEY",
        )
        .expect("new")
        .with_base_url(base_url)
    }

    /// The key newtype rejects empty / whitespace-only input — symmetric
    /// with the Anthropic / Gemini newtypes.
    #[test]
    fn openrouter_api_key_rejects_empty() {
        assert!(matches!(
            OpenRouterApiKey::new(""),
            Err(LlmError::Invalid(_))
        ));
        assert!(matches!(
            OpenRouterApiKey::new("   "),
            Err(LlmError::Invalid(_))
        ));
    }

    /// `Debug` never prints the raw key bytes.
    #[test]
    fn openrouter_api_key_debug_is_redacted() {
        let key = OpenRouterApiKey::new("sk-or-super-secret-987654321").unwrap();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("sk-or-super-secret-987654321"), "{dbg}");
        assert!(dbg.contains("redacted"), "{dbg}");
    }

    /// `stop` → `EndOfTurn`, `length` → `MaxTokens`, everything else
    /// (incl. `tool_calls`) → `Other`.
    #[test]
    fn classify_openrouter_finish_reason_maps_stop_and_length() {
        assert_eq!(
            classify_openrouter_finish_reason(Some("stop")),
            FinishReason::EndOfTurn
        );
        assert_eq!(
            classify_openrouter_finish_reason(Some("length")),
            FinishReason::MaxTokens
        );
        assert_eq!(
            classify_openrouter_finish_reason(Some("tool_calls")),
            FinishReason::Other
        );
        assert_eq!(classify_openrouter_finish_reason(None), FinishReason::Other);
    }

    /// `reasoning_effort` maps onto `reasoning.effort`; `minimal`/unset
    /// omits the block, `extra-high` folds to `high`, an unknown value
    /// floors to `medium`.
    #[test]
    fn openrouter_reasoning_effort_maps_to_effort() {
        let effort = |e: Option<&str>| {
            OpenRouterBackend::new(OpenRouterApiKey::new("k").unwrap(), "m", "ENV")
                .expect("backend")
                .with_reasoning_effort(e)
                .reasoning_effort
        };
        assert_eq!(effort(None), None);
        assert_eq!(effort(Some("")), None);
        assert_eq!(effort(Some("minimal")), None);
        assert_eq!(effort(Some("low")).as_deref(), Some("low"));
        assert_eq!(effort(Some("high")).as_deref(), Some("high"));
        assert_eq!(effort(Some("extra-high")).as_deref(), Some("high"));
        assert_eq!(effort(Some("bogus")).as_deref(), Some("medium"));
    }

    /// Tool-call arguments arrive as a JSON string; empty / malformed
    /// payloads degrade to an empty object so the agentic loop always
    /// gets a structurally valid bag.
    #[test]
    fn parse_openrouter_tool_args_degrades_empty_and_malformed() {
        assert_eq!(
            parse_openrouter_tool_args("{\"q\":1}"),
            serde_json::json!({ "q": 1 })
        );
        assert_eq!(parse_openrouter_tool_args(""), serde_json::json!({}));
        assert_eq!(parse_openrouter_tool_args("   "), serde_json::json!({}));
        assert_eq!(
            parse_openrouter_tool_args("not json"),
            serde_json::json!({})
        );
    }

    /// Happy path: `POST /chat/completions` with a `Bearer` token and the
    /// `OpenAI` body shape; decode `choices[0].message.content` + usage.
    #[tokio::test]
    async fn openrouter_backend_posts_chat_completions_and_decodes_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sk-or-fake-key",
            ))
            .and(body_json(serde_json::json!({
                "model": "anthropic/claude-sonnet-4-6",
                "messages": [
                    { "role": "system", "content": "Be terse." },
                    { "role": "user", "content": "Hello" }
                ],
                "max_tokens": 64,
                "temperature": 0.5
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [
                    { "message": { "content": "Hi." }, "finish_reason": "stop" }
                ],
                "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let backend = openrouter_test_backend(server.uri());
        let req = CompletionRequest::new("Hello")
            .with_system("Be terse.")
            .with_max_tokens(64)
            .with_temperature(0.5);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "Hi.");
        assert_eq!(resp.finish_reason, FinishReason::EndOfTurn);
        assert_eq!(resp.usage.prompt_tokens, Some(5));
        assert_eq!(resp.usage.completion_tokens, Some(2));
        assert_eq!(backend.model_id(), "anthropic/claude-sonnet-4-6");
    }

    /// Vision path: image parts precede the text part inside the same
    /// user `content` array.
    #[tokio::test]
    async fn openrouter_backend_sends_image_parts_then_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_json(serde_json::json!({
                "model": "anthropic/claude-sonnet-4-6",
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                        { "type": "text", "text": "what is this" }
                    ]
                }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "a logo" }, "finish_reason": "stop" }]
            })))
            .mount(&server)
            .await;

        let backend = openrouter_test_backend(server.uri());
        let req = CompletionRequest::new("what is this").with_images(vec![ImageInput {
            mime_type: "image/png".into(),
            data_base64: "AAAA".into(),
        }]);
        let resp = backend.complete(req).await.expect("complete");
        assert_eq!(resp.text, "a logo");
    }

    /// `reasoning.effort` rides the body only when the slot configures it.
    #[tokio::test]
    async fn openrouter_backend_includes_reasoning_effort_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "reasoning": { "effort": "high" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }]
            })))
            .mount(&server)
            .await;

        let backend = OpenRouterBackend::new(
            OpenRouterApiKey::new("sk-or-fake-key").unwrap(),
            "anthropic/claude-opus-4-8",
            "OPENROUTER_API_KEY",
        )
        .expect("new")
        .with_base_url(server.uri())
        .with_reasoning_effort(Some("extra-high"));
        let resp = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("complete");
        assert_eq!(resp.text, "ok");
    }

    /// `chat` decodes assistant `tool_calls`, parsing the JSON-string
    /// arguments into a structured value and minting an id fallback only
    /// when the provider omits one.
    #[tokio::test]
    async fn openrouter_chat_decodes_tool_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": { "name": "lookup", "arguments": "{\"q\":\"x\"}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })))
            .mount(&server)
            .await;

        let backend = openrouter_test_backend(server.uri());
        let tools = vec![Tool {
            name: "lookup".into(),
            description: "look it up".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let req = ChatRequest::new(vec![ChatMessage::user("find x")]).with_tools(tools);
        let resp = backend.chat(req).await.expect("chat");
        assert_eq!(resp.message.tool_calls.len(), 1);
        let call = &resp.message.tool_calls[0];
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "lookup");
        assert_eq!(call.arguments, serde_json::json!({ "q": "x" }));
        assert!(resp.message.content.is_empty());
    }

    /// 401 → [`LlmError::Auth`] naming the offending env-var.
    #[tokio::test]
    async fn openrouter_backend_maps_401_to_auth_error_with_env_var_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let backend = openrouter_test_backend(server.uri());
        let err = backend
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("must fail");
        match err {
            LlmError::Auth(msg) => assert!(msg.contains("OPENROUTER_API_KEY"), "{msg}"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    /// An empty prompt is rejected before any network call.
    #[tokio::test]
    async fn openrouter_backend_rejects_empty_prompt() {
        let backend = openrouter_test_backend("http://127.0.0.1:1".to_owned());
        let err = backend
            .complete(CompletionRequest::new(""))
            .await
            .expect_err("must reject");
        assert!(matches!(err, LlmError::Invalid(_)), "{err:?}");
    }
}
