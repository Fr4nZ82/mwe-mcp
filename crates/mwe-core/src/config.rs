// SPDX-License-Identifier: AGPL-3.0-or-later
//! `mwe-mcp.config.yaml` loader.
//!
//! Incremental — every sub-section becomes a Rust struct
//! the moment another module needs it. The sections with a Rust home
//! today:
//!
//! - `logging`.
//! - `llm` — six canonical functions (`hub_writer`, `ingest`,
//!   `rem_promotions`, `rem_dedup_semantic`, `cronista`, `navigator`);
//!   needed by the ingest orchestrator that consumes `llm.ingest`.
//!
//! Everything else is captured verbatim in [`Config::extra`].
//!
//! The canonical schema lives in
//! [the config schema reference](../../../docs/protocol/config-schema.md);
//! this module follows it.
//!
//! ## Lookup order
//!
//! - `<workdir>/mwe-mcp.config.yaml` if present.
//! - Otherwise: a [`Config::default`] is returned and the absence is
//!   logged at `info` level (the absence is normal for a fresh
//!   workdir; it stops being normal once an operator deliberately
//!   placed a file and we silently ignored it because of a typo).
//!
//! ## Hot-reload
//!
//! Out of scope for now. The spec calls out `rate_limits`, `budget`,
//! and `rem.schedule` as future hot-reload candidates; `logging.level`
//! could join them but for now a restart is required.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default filename inside the workdir.
pub const CONFIG_FILENAME: &str = "mwe-mcp.config.yaml";

/// Errors raised by the config layer.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Underlying filesystem error reading the file.
    #[error("config io: {0}")]
    Io(#[from] std::io::Error),

    /// YAML parse failure on the file body.
    #[error("config parse error at {path}: {detail}")]
    Parse {
        /// Path of the file that failed to parse, relative to workdir
        /// where available.
        path: PathBuf,
        /// Human-readable detail (English, no trailing period).
        detail: String,
    },

    /// `logging.level` value was not one of the accepted choices.
    #[error("config logging.level {value:?}: expected `info` or `debug`")]
    InvalidLogLevel {
        /// The offending value.
        value: String,
    },

    /// `logging.file_rotation` value was not one of the accepted choices.
    #[error(
        "config logging.file_rotation {value:?}: expected `daily`, `hourly`, `never`, or `disabled`"
    )]
    InvalidLogFileRotation {
        /// The offending value.
        value: String,
    },

    /// `rem.schedule.mode` value was not one of the accepted choices.
    #[error("config rem.schedule.mode {value:?}: expected `interval` or `disabled`")]
    InvalidRemScheduleMode {
        /// The offending value.
        value: String,
    },

    /// An `llm.<function>` sub-section names an `backend` that this
    /// build cannot construct.
    ///
    /// Today the build supports `ollama`, `anthropic`, and `gemini`.
    /// `openai` lands with a follow-up adapter milestone; until then a
    /// config that names it is loaded without error but
    /// [`LlmFunctionConfig::build_backend`] refuses to materialise the
    /// backend so the operator's mistake surfaces at startup, not at
    /// the first request.
    #[error(
        "config llm.{function}.backend {backend:?}: only `ollama`, `anthropic`, and `gemini` are supported in this build"
    )]
    UnsupportedLlmBackend {
        /// Which `llm.*` slot referenced the backend.
        function: String,
        /// The backend name from YAML.
        backend: String,
    },

    /// An `llm.<function>` sub-section names a cloud backend (Anthropic,
    /// `OpenAI`, …) but is missing or has an empty `api_key_env`,
    /// or the env-var it names is unset / empty in the process
    /// environment.
    ///
    /// The error message names the offending env-var (when known) so
    /// the operator can find it without diffing the config against
    /// `mwe-mcp.env`. The check runs at boot via
    /// `health_check_llm_slots`, so a missing key never silently
    /// corrupts the first request.
    #[error("config llm.{function}: {detail}")]
    MissingApiKeyEnv {
        /// Which `llm.*` slot is missing its key.
        function: String,
        /// Human-readable diagnostic: which env-var was tried, and
        /// what specifically was missing.
        detail: String,
    },

    /// `embedding.backend` names a backend this build cannot construct.
    ///
    /// Today the build supports `ollama` (the default) and — when
    /// compiled with the `local-embedder` feature — `bundled`
    /// (Candle / bge-m3). `openai` is reserved for a follow-up adapter;
    /// until then a config that names it loads without error but
    /// [`EmbeddingConfig::build_embedder`] refuses to materialise it so
    /// the operator's mistake surfaces at startup, not at the first
    /// recall.
    #[error(
        "config embedding.backend {backend:?}: only `ollama` and `bundled` (with the `local-embedder` build feature) are supported in this build"
    )]
    UnsupportedEmbeddingBackend {
        /// The backend name from YAML.
        backend: String,
    },

    /// `embedding` selected a supported backend that cannot be built in
    /// this configuration — `bundled` on a binary compiled without the
    /// `local-embedder` feature, `device: gpu` without a CUDA build
    /// (roadmap 18f), a missing `model_dir` before the
    /// weight-distribution work (roadmap 18c), or the backend
    /// constructor failing.
    #[error("config embedding: {detail}")]
    EmbeddingUnavailable {
        /// Human-readable diagnostic (English, no trailing period).
        detail: String,
    },
}

/// Result alias for this module.
pub type Result<T> = std::result::Result<T, ConfigError>;

// ---------- Log level ----------

/// Two-level log filter selectable from `mwe-mcp.config.yaml`.
///
/// The choice is deliberately narrow: an operator
/// either wants normal operational visibility (`Info`) or full
/// internal debugging (`Debug`). `warn` / `error` always pass through;
/// `trace` is not exposed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Boundary events: capture / supersede / forget done, REM cycle
    /// start/end, startup/shutdown, identity bootstrap, ACL denies on
    /// the request boundary. The "I can see what is happening" tier.
    #[default]
    Info,
    /// Boundary events plus internal step detail: jaccard dedup
    /// scores, embedding dim + text size, parser warnings, file
    /// watcher raw events, `atomic_write` path/size, SQL slow queries.
    /// The "something is wrong, follow every step" tier.
    Debug,
}

impl LogLevel {
    /// `tracing_subscriber::EnvFilter` directive corresponding to this
    /// level — applied to the mwe-mcp crates only so a chatty
    /// dependency (e.g. `sqlx`, `notify`) does not flood debug output.
    #[must_use]
    pub fn as_env_filter(self) -> String {
        match self {
            // Bring the mwe-mcp crates in at the chosen level. Everything
            // else stays at `warn` so the operator's terminal does not get
            // buried.
            Self::Info => "warn,mwe_core=info,mwe_dashboard=info,mwe_mcp=info".to_owned(),
            Self::Debug => "warn,mwe_core=debug,mwe_dashboard=debug,mwe_mcp=debug".to_owned(),
        }
    }
}

// ---------- LLM ----------

/// One of the canonical LLM functions (see [the config schema reference](../../../docs/protocol/config-schema.md)).
///
/// Used both as a config sub-section name and as the suffix for the
/// env-var override convention: `MWE_LLM_<UPPER>_MODEL` /
/// `MWE_LLM_<UPPER>_BACKEND`.
///
/// **Variants today**: all six are dashboard-configurable role cards —
/// `HubWriter`, `Ingest`, `OperatorChat`, `RemPromotions`, `Cronista`,
/// `RemDedupSemantic`, `Navigator`. `Cronista` drives the narrative
/// compiler ([`crate::compiler::compile_leaf_page`], via the dream
/// compile pass) that rewrites each dirty standard-wiki leaf from its
/// facts into prose; with the slot unconfigured the compile step is
/// skipped and the pages stay blank, so it is surfaced like every other
/// slot. It keeps the `#[deprecated]` marker only because the compiler
/// has not yet graduated to a full REM sub-job (`structure_proposal`
/// output + budget enforcement) — the marker lifts when it does and does
/// **not** mean the slot is unused. The value loads from an
/// `llm.cronista:` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFunction {
    /// `hub_writer` — regenerates `index.md` summaries on writes.
    HubWriter,
    /// `ingest` — backs `wiki_ingest_message` and the dashboard's
    /// non-agentic consumer-style turn (the welcome-wizard primer).
    Ingest,
    /// `operator_chat` — the dashboard's operational agentic chat loop
    /// (the maintainer's tool on their own memory). Wants a **strong**
    /// model with reliable function-calling: it reasons over multiple
    /// tool calls and must handle fact ids faithfully. Optional — when
    /// the slot is unconfigured the chat falls back to `hub_writer`
    /// (`MemoryHandles::backend_for_chat`), so existing deployments keep
    /// working without a new YAML key.
    OperatorChat,
    /// `rem_promotions` — promotes paragraphs / files / wikis nightly.
    RemPromotions,
    /// `rem_dedup_semantic` — semantic dedup after the jaccard pre-pass.
    RemDedupSemantic,
    /// `cronista` — the narrative prose compiler.
    /// `crate::compiler::compile_leaf_page` invokes this slot to
    /// rewrite each dirty standard-wiki leaf from its facts into prose. Wants a
    /// **strong** model (faithful fact→prose without invention or leak).
    Cronista,
    /// `navigator` — the recall navigator: per-turn, reads the root
    /// index + destination cards and decides which wikis/pages to open
    /// next (the recall pipeline).
    /// Wants a **strong-but-cheap** model: it runs on every turn
    /// (latency + cost bound) but its link choices are the recall
    /// quality bar.
    Navigator,
}

#[allow(deprecated, reason = "Cronista arm kept for YAML backward compat")]
impl LlmFunction {
    /// YAML key for this function under `llm:`.
    #[must_use]
    pub const fn yaml_key(self) -> &'static str {
        match self {
            Self::HubWriter => "hub_writer",
            Self::Ingest => "ingest",
            Self::OperatorChat => "operator_chat",
            Self::RemPromotions => "rem_promotions",
            Self::RemDedupSemantic => "rem_dedup_semantic",
            Self::Cronista => "cronista",
            Self::Navigator => "navigator",
        }
    }

    /// Env-var prefix for the override of this function's keys.
    /// E.g. `MWE_LLM_INGEST` for [`Self::Ingest`].
    #[must_use]
    pub fn env_prefix(self) -> String {
        format!("MWE_LLM_{}", self.yaml_key().to_uppercase())
    }
}

/// One LLM function configuration: which backend to drive and what
/// model id to ask for.
///
/// Backends supported in this build: `ollama`. Naming a different
/// backend in YAML is accepted by [`Config::load`] (so an operator's
/// existing config does not break upgrades) but
/// [`Self::build_backend`] refuses to materialise it.
///
/// `PartialEq` only (no `Eq`) because [`Self::temperature`] is an
/// `Option<f32>` and `f32` does not implement `Eq` (NaN). The rest of
/// the type is `Eq`-compatible; equality comparisons treat
/// `Some(NaN) != Some(NaN)`, which is the standard IEEE 754 behavior
/// and what every config-equality call site here actually wants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmFunctionConfig {
    /// Backend tag (`ollama` | `anthropic` | `gemini` | `openrouter`).
    pub backend: String,
    /// Model id passed to the backend.
    pub model: String,
    /// Env-var name to read the API key from (cloud backends only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Override of the backend's base URL (e.g. self-hosted Ollama on
    /// a custom port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Backend-specific reasoning-effort hint. For Anthropic models
    /// with extended thinking the adapter translates this to a
    /// `budget_tokens` value; for `OpenAI` o-series it forwards as
    /// `reasoning_effort`; for Gemini it maps onto
    /// `thinkingConfig.thinkingLevel` (`low`/`medium`/`high`; unset →
    /// `minimal`) — **required non-minimal for Gemini 3.x Pro, which
    /// rejects `minimal`**; for Ollama it is ignored. Accepted values
    /// are not pinned here — adapters validate at build time. Common
    /// strings: `"low"`, `"medium"`, `"high"`, `"extra-high"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Default sampling temperature applied by
    /// [`Self::apply_defaults_to_completion`] /
    /// [`Self::apply_defaults_to_chat`] when the caller leaves the
    /// request field unset. Operator-overridable knob for the
    /// generative slots (`hub_writer`, `cronista`, etc.); call sites
    /// that pin temperature explicitly (e.g. `ingest` parsing or REM
    /// revisor at 0.1 for determinism) are unaffected because the
    /// helpers only fill `None` fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Default `max_tokens` ceiling applied by
    /// [`Self::apply_defaults_to_completion`] /
    /// [`Self::apply_defaults_to_chat`] when the caller leaves the
    /// request field unset. Same fill-only-if-unset semantics as
    /// [`Self::temperature`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl LlmFunctionConfig {
    /// Build a live [`crate::llm::LlmBackend`] from this config.
    ///
    /// Supported backends: `ollama` (local) plus the `anthropic`,
    /// `gemini`, and `openrouter` cloud providers; any other tag returns
    /// [`ConfigError::UnsupportedLlmBackend`], with the function tag
    /// carried into the error so the operator can find the offending
    /// YAML key. The cloud backends also require `api_key_env` pointing
    /// at an env-var that is set and non-empty in the process
    /// environment — otherwise [`ConfigError::MissingApiKeyEnv`].
    ///
    /// # Errors
    ///
    /// - [`ConfigError::UnsupportedLlmBackend`] when `backend` is none
    ///   of `"ollama"`, `"anthropic"`, `"gemini"`, `"openrouter"`.
    /// - [`ConfigError::MissingApiKeyEnv`] when `backend` is a cloud
    ///   provider (`anthropic`, `gemini`, or `openrouter`) but
    ///   `api_key_env` is
    ///   missing, names an unset env-var, or resolves to an empty
    ///   string.
    /// - Propagates the transport / parse errors raised by the chosen
    ///   backend's constructor as [`ConfigError::Parse`] (so the
    ///   caller sees one error type).
    pub fn build_backend(&self, function: LlmFunction) -> Result<Box<dyn crate::llm::LlmBackend>> {
        self.build_backend_with_env(function, |k| std::env::var(k).ok())
    }

    /// Lookup-injectable variant of [`Self::build_backend`] used by
    /// the test suite to drive the cloud constructors without
    /// touching the process environment.
    ///
    /// The closure receives an env-var name and returns its value, or
    /// `None` if unset. Production callers use the `std::env::var`
    /// variant via [`Self::build_backend`].
    ///
    /// Every built backend is passed through
    /// [`crate::training_spool::maybe_wrap`]: when the server has
    /// installed a process-wide training spool, the backend records
    /// its prompt/completion pairs (per-call enabled check — the
    /// dashboard toggle is honoured without a rebuild); without an
    /// installed spool this is a passthrough.
    ///
    /// # Errors
    ///
    /// Same as [`Self::build_backend`].
    pub fn build_backend_with_env<F>(
        &self,
        function: LlmFunction,
        env: F,
    ) -> Result<Box<dyn crate::llm::LlmBackend>>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let inner = self.build_backend_raw_with_env(function, env)?;
        Ok(crate::training_spool::maybe_wrap(
            inner,
            function,
            &self.backend,
        ))
    }

    /// The dispatch itself — one arm per backend tag, spool-free.
    #[allow(
        clippy::too_many_lines,
        reason = "flat dispatch match — one arm per backend; splitting hurts readability"
    )]
    fn build_backend_raw_with_env<F>(
        &self,
        function: LlmFunction,
        mut env: F,
    ) -> Result<Box<dyn crate::llm::LlmBackend>>
    where
        F: FnMut(&str) -> Option<String>,
    {
        match self.backend.as_str() {
            "ollama" => {
                // Endpoint precedence: per-role override (`base_url` under
                // «Advanced») > the provider-level `OLLAMA_BASE_URL` set from the
                // dashboard Ollama card > the localhost default. `OLLAMA_API_KEY`
                // carries an optional Bearer token for a remote / cloud / proxied
                // daemon (blank = local, no auth).
                let base_url = self
                    .base_url
                    .clone()
                    .or_else(|| env("OLLAMA_BASE_URL"))
                    .unwrap_or_else(|| crate::llm::DEFAULT_OLLAMA_URL.to_owned());
                let backend = crate::llm::OllamaBackend::new(base_url, self.model.clone())
                    .map_err(|e| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: format!("ollama: {e}"),
                    })?
                    .with_bearer(env("OLLAMA_API_KEY"));
                Ok(Box::new(backend))
            },
            "anthropic" => {
                // The reserved `claude-code` sentinel routes the slot to the
                // Claude Code login store (resolved + refreshed per request)
                // instead of an env var. Test/personal use only; see
                // `crate::oauth` and `docs/protocol/config-schema.md`.
                if self.api_key_env.as_deref() == Some(crate::oauth::CLAUDE_CODE_LOGIN) {
                    let store = crate::oauth::global_store().ok_or_else(|| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: "anthropic `api_key_env: claude-code` selects the Claude Code \
                                 login, but no login store is available — log in from the dashboard"
                            .to_owned(),
                    })?;
                    let mut backend = crate::llm::AnthropicBackend::with_login_store(
                        store,
                        self.model.clone(),
                        crate::oauth::CLAUDE_CODE_LOGIN,
                    )
                    .map_err(|e| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: format!("anthropic: {e}"),
                    })?
                    .with_reasoning_effort(self.reasoning_effort.as_deref());
                    if let Some(base_url) = self.base_url.as_deref() {
                        backend = backend.with_base_url(base_url);
                    }
                    return Ok(Box::new(backend));
                }
                let (env_name, raw) = resolve_cloud_api_key(
                    "anthropic",
                    function,
                    self.api_key_env.as_deref(),
                    &mut env,
                )?;
                let key =
                    crate::llm::AnthropicApiKey::new(raw).map_err(|e| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: format!("anthropic: {e}"),
                    })?;
                let mut backend =
                    crate::llm::AnthropicBackend::new(key, self.model.clone(), env_name)
                        .map_err(|e| ConfigError::Parse {
                            path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                            detail: format!("anthropic: {e}"),
                        })?
                        .with_reasoning_effort(self.reasoning_effort.as_deref());
                if let Some(base_url) = self.base_url.as_deref() {
                    backend = backend.with_base_url(base_url);
                }
                Ok(Box::new(backend))
            },
            "gemini" => {
                let (env_name, raw) = resolve_cloud_api_key(
                    "gemini",
                    function,
                    self.api_key_env.as_deref(),
                    &mut env,
                )?;
                let key = crate::llm::GeminiApiKey::new(raw).map_err(|e| ConfigError::Parse {
                    path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                    detail: format!("gemini: {e}"),
                })?;
                let mut backend = crate::llm::GeminiBackend::new(key, self.model.clone(), env_name)
                    .map_err(|e| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: format!("gemini: {e}"),
                    })?
                    .with_reasoning_effort(self.reasoning_effort.as_deref());
                if let Some(base_url) = self.base_url.as_deref() {
                    backend = backend.with_base_url(base_url);
                }
                Ok(Box::new(backend))
            },
            "openrouter" => {
                let (env_name, raw) = resolve_cloud_api_key(
                    "openrouter",
                    function,
                    self.api_key_env.as_deref(),
                    &mut env,
                )?;
                let key =
                    crate::llm::OpenRouterApiKey::new(raw).map_err(|e| ConfigError::Parse {
                        path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                        detail: format!("openrouter: {e}"),
                    })?;
                let mut backend =
                    crate::llm::OpenRouterBackend::new(key, self.model.clone(), env_name)
                        .map_err(|e| ConfigError::Parse {
                            path: PathBuf::from(format!("llm.{}", function.yaml_key())),
                            detail: format!("openrouter: {e}"),
                        })?
                        .with_reasoning_effort(self.reasoning_effort.as_deref());
                if let Some(base_url) = self.base_url.as_deref() {
                    backend = backend.with_base_url(base_url);
                }
                Ok(Box::new(backend))
            },
            #[cfg(any(test, feature = "test-fakes"))]
            "fake" => {
                // Test-only backend: `model` is reinterpreted as the
                // canned response string the [`FakeLlmBackend`] returns
                // from every `complete` / `chat` call. Lets dispatcher
                // integration tests exercise `wiki_ingest_message`
                // end-to-end without standing up an Ollama instance.
                // Built only with the `test-fakes` Cargo feature
                // (auto-enabled by `cargo test`), so production
                // configs cannot accidentally select it.
                let response = self.model.clone();
                Ok(Box::new(crate::llm::FakeLlmBackend::new("fake", response)))
            },
            other => Err(ConfigError::UnsupportedLlmBackend {
                function: function.yaml_key().to_owned(),
                backend: other.to_owned(),
            }),
        }
    }
}

/// Resolve a cloud backend's API key against `api_key_env` (the YAML
/// field naming the env-var) and the injected `env` lookup. Returns
/// `(env_var_name, raw_key)` ready for the backend constructor, which
/// echoes the env-var name back to the operator on auth errors.
///
/// Centralised here because the `anthropic` and `gemini` branches
/// share the same three-step contract: the YAML must name an env-var,
/// the env-var must be set, and the value must be non-empty after
/// trimming. The error variants (`MissingApiKeyEnv`) round-trip the
/// failing slot's YAML key so the operator can find the offending
/// section without diffing `mwe-mcp.config.yaml` against
/// `mwe-mcp.env`.
fn resolve_cloud_api_key<F>(
    provider: &'static str,
    function: LlmFunction,
    api_key_env: Option<&str>,
    env: &mut F,
) -> Result<(String, String)>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(env_name) = api_key_env else {
        return Err(ConfigError::MissingApiKeyEnv {
            function: function.yaml_key().to_owned(),
            detail: format!(
                "{provider} backend requires `api_key_env` to name the env-var holding the API key"
            ),
        });
    };
    let raw = env(env_name)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingApiKeyEnv {
            function: function.yaml_key().to_owned(),
            detail: format!("env-var `{env_name}` is unset or empty — set it in mwe-mcp.env"),
        })?;
    Ok((env_name.to_owned(), raw))
}

/// `llm:` section of `mwe-mcp.config.yaml`.
///
/// Each canonical function is optional in YAML — missing keys mean
/// "this function is not wired in this deployment". The operator can
/// reach into the runtime config by `Config::llm.ingest` (etc.) and
/// build the backend lazily.
///
/// Env-var overrides (see [the config schema reference](../../../docs/protocol/config-schema.md))
/// are applied by [`Self::apply_env_overrides`] after YAML parse:
/// `MWE_LLM_INGEST_MODEL` overrides `llm.ingest.model`,
/// `MWE_LLM_INGEST_BACKEND` overrides `llm.ingest.backend`, and so on
/// for the other slots. An override that names a function
/// not present in YAML creates the entry (with `model` falling back to
/// the empty string if the env-var only set `BACKEND`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    /// `all-local` | `hybrid` | `all-api` | `custom` — informational,
    /// not enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// `hub_writer` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_writer: Option<LlmFunctionConfig>,
    /// `ingest` slot — required when `wiki_ingest_message` is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<LlmFunctionConfig>,
    /// `operator_chat` slot — the dashboard's operational agentic chat.
    /// Optional: unset falls back to `hub_writer`
    /// ([`crate::config::LlmFunction::OperatorChat`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_chat: Option<LlmFunctionConfig>,
    /// `rem_promotions` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rem_promotions: Option<LlmFunctionConfig>,
    /// `rem_dedup_semantic` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rem_dedup_semantic: Option<LlmFunctionConfig>,
    /// `cronista` slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cronista: Option<LlmFunctionConfig>,
    /// `navigator` slot — the per-turn recall navigator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigator: Option<LlmFunctionConfig>,
}

/// Profile presets seeded by `mwe-mcp init`.
///
/// The three canned profiles in [the config schema reference](../../../docs/protocol/config-schema.md)
/// plus the catch-all `custom` (empty skeleton — operator fills in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProfile {
    /// Every slot points to a local Ollama instance. Optimised for
    /// privacy-first deployments; matches the workhorse runtime
    /// baseline in the `project-workhorse-runtime-baseline` memory.
    AllLocal,
    /// `ingest` and `hub_writer` stay local; the strong nightly slots
    /// (`rem_promotions`, `cronista`) and the navigator go to an API
    /// (Anthropic by default). Local chat latency, API-quality
    /// maintenance.
    Hybrid,
    /// Every slot goes to an API. Anthropic by default for the
    /// quality-sensitive slots, `OpenAI` for the cheap dedup pass.
    AllApi,
    /// Empty skeleton — operator wires every slot manually.
    Custom,
}

impl LlmProfile {
    /// Parse the YAML-side string. Case-insensitive.
    ///
    /// # Errors
    ///
    /// Returns the unknown value as `Err(value)` so the caller can
    /// surface it verbatim.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "all-local" | "all_local" | "alllocal" => Ok(Self::AllLocal),
            "hybrid" => Ok(Self::Hybrid),
            "all-api" | "all_api" | "allapi" => Ok(Self::AllApi),
            "custom" => Ok(Self::Custom),
            other => Err(other.to_owned()),
        }
    }

    /// Profile name as it lives in YAML.
    #[must_use]
    pub const fn yaml_name(self) -> &'static str {
        match self {
            Self::AllLocal => "all-local",
            Self::Hybrid => "hybrid",
            Self::AllApi => "all-api",
            Self::Custom => "custom",
        }
    }

    /// Build the canonical [`LlmConfig`] for this profile.
    ///
    /// Picks per slot follow the tier table in
    /// the ingest pipeline notes:
    ///
    /// - `hub_writer`, `ingest` — workhorse tier. Local where possible
    ///   (latency matters for chat).
    /// - `rem_promotions`, `cronista` — strong tier. API where
    ///   possible (decisions worth the cost).
    /// - `rem_dedup_semantic` — yes/no classifier. Reuses the already-
    ///   loaded workhorse on local deploys (no point spinning up a
    ///   separate small model for ~30 calls/night when the workhorse
    ///   is already in VRAM); uses a cheap API endpoint on api-only
    ///   deploys.
    /// - `navigator` — strong-but-cheap tier: per-turn recall
    ///   navigation, latency + cost bound but quality-bearing. Haiku
    ///   on the API profiles; the local workhorse on all-local (a
    ///   dedicated local navigator tune is a tracked extension).
    ///
    /// NOTE: `hybrid` and `all-api` reference the `anthropic` backend.
    /// [`LlmFunctionConfig::build_backend`] now materialises it
    /// provided the env-var named in `api_key_env` (default
    /// `ANTHROPIC_API_KEY`) is set in the process environment. The
    /// `gemini` backend is also materialisable (with `GEMINI_API_KEY`
    /// by convention) but no canned profile pins it yet — operators
    /// who want a Gemini-based deployment wire it slot-by-slot via
    /// the dashboard editor or YAML. The `openai` backend remains
    /// gated by `ConfigError::UnsupportedLlmBackend` until its
    /// adapter milestone lands.
    #[must_use]
    pub fn build(self) -> LlmConfig {
        match self {
            // Uniform Qwen 3.5 9B Q8 — fits ~10 GB VRAM alongside the
            // bge-m3 embedder (~5 GB), works on a 16 GB GPU. Operators
            // with more VRAM may swap `qwen3:32b` into `rem_promotions`
            // / `cronista` for higher-quality nightly decisions; the
            // cap_promote default (5) stays the same.
            Self::AllLocal => LlmConfig {
                profile: Some("all-local".into()),
                hub_writer: Some(ollama("qwen3.5:9b-q8_0")),
                ingest: Some(ollama("qwen3.5:9b-q8_0")),
                // Falls back to `hub_writer` (the local workhorse) unless
                // the operator wires a stronger tool-calling model.
                operator_chat: None,
                rem_promotions: Some(ollama("qwen3.5:9b-q8_0").with_reasoning_effort("extra-high")),
                rem_dedup_semantic: Some(ollama("qwen3.5:9b-q8_0")),
                cronista: Some(ollama("qwen3.5:9b-q8_0")),
                navigator: Some(ollama("qwen3.5:9b-q8_0")),
            },
            // Recommended default. Conversational + frequent slots on
            // the local workhorse (zero latency, no API cost); nightly
            // structural decisions on Anthropic Opus 4.7 with
            // extra-high effort (quality bar where it matters);
            // `rem_dedup_semantic` reuses the local workhorse so we
            // don't open a second VRAM tenant just for yes/no.
            Self::Hybrid => LlmConfig {
                profile: Some("hybrid".into()),
                hub_writer: Some(ollama("qwen3.5:9b-q8_0")),
                ingest: Some(ollama("qwen3.5:9b-q8_0")),
                // Falls back to `hub_writer` (the local workhorse) unless
                // the operator wires a stronger tool-calling model.
                operator_chat: None,
                rem_promotions: Some(
                    anthropic("claude-opus-4-7", "ANTHROPIC_API_KEY")
                        .with_reasoning_effort("extra-high"),
                ),
                rem_dedup_semantic: Some(ollama("qwen3.5:9b-q8_0")),
                cronista: Some(anthropic("claude-opus-4-7", "ANTHROPIC_API_KEY")),
                navigator: Some(anthropic("claude-haiku-4-5-20251001", "ANTHROPIC_API_KEY")),
            },
            // API-only. Haiku for the bandwidth-heavy `hub_writer`,
            // Sonnet for `ingest` (intent classification benefits from
            // the bigger model), Opus 4.7 for the strong slots. Dedup
            // stays on Haiku — single-provider deploys are simpler.
            Self::AllApi => LlmConfig {
                profile: Some("all-api".into()),
                hub_writer: Some(anthropic("claude-haiku-4-5-20251001", "ANTHROPIC_API_KEY")),
                ingest: Some(anthropic("claude-sonnet-4-6", "ANTHROPIC_API_KEY")),
                // Falls back to `hub_writer` unless wired; a strong
                // tool-calling model is the right pick when set.
                operator_chat: None,
                rem_promotions: Some(
                    anthropic("claude-opus-4-7", "ANTHROPIC_API_KEY")
                        .with_reasoning_effort("extra-high"),
                ),
                rem_dedup_semantic: Some(anthropic(
                    "claude-haiku-4-5-20251001",
                    "ANTHROPIC_API_KEY",
                )),
                cronista: Some(anthropic("claude-opus-4-7", "ANTHROPIC_API_KEY")),
                navigator: Some(anthropic("claude-haiku-4-5-20251001", "ANTHROPIC_API_KEY")),
            },
            Self::Custom => LlmConfig {
                profile: Some("custom".into()),
                ..LlmConfig::default()
            },
        }
    }
}

impl LlmFunctionConfig {
    /// Builder-style setter for [`Self::reasoning_effort`]. Used by the
    /// preset constructors and by tests; production operators edit the
    /// YAML directly.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Builder-style setter for [`Self::temperature`]. Mirrors
    /// [`Self::with_reasoning_effort`] for the per-slot default
    /// sampling temperature applied by [`Self::apply_defaults_to_completion`].
    #[must_use]
    pub const fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Builder-style setter for [`Self::max_tokens`]. Mirrors
    /// [`Self::with_reasoning_effort`] for the per-slot default
    /// `max_tokens` ceiling applied by [`Self::apply_defaults_to_completion`].
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Fill the request's `temperature` / `max_tokens` from this
    /// slot's defaults **only when the caller left them unset**.
    ///
    /// Call sites that pin a value explicitly (e.g. `ingest` at
    /// `temperature=0.1` for parser determinism, REM revisor at
    /// `max_tokens=60` for terse yes/no) are unaffected — the helper
    /// touches only `None` fields. This is the seam that lets the
    /// dashboard admin config page influence generative slots
    /// (agentic chat, cronista) without breaking the
    /// determinism contracts of the classifier slots.
    pub const fn apply_defaults_to_completion(&self, req: &mut crate::llm::CompletionRequest) {
        if req.temperature.is_none() {
            req.temperature = self.temperature;
        }
        if req.max_tokens.is_none() {
            req.max_tokens = self.max_tokens;
        }
    }

    /// Chat-request counterpart of [`Self::apply_defaults_to_completion`].
    /// Same fill-only-if-unset semantics, same fields.
    pub const fn apply_defaults_to_chat(&self, req: &mut crate::llm::ChatRequest) {
        if req.temperature.is_none() {
            req.temperature = self.temperature;
        }
        if req.max_tokens.is_none() {
            req.max_tokens = self.max_tokens;
        }
    }
}

fn ollama(model: &str) -> LlmFunctionConfig {
    LlmFunctionConfig {
        backend: "ollama".into(),
        model: model.into(),
        api_key_env: None,
        base_url: None,
        reasoning_effort: None,
        temperature: None,
        max_tokens: None,
    }
}

fn anthropic(model: &str, key_env: &str) -> LlmFunctionConfig {
    LlmFunctionConfig {
        backend: "anthropic".into(),
        model: model.into(),
        api_key_env: Some(key_env.into()),
        base_url: None,
        reasoning_effort: None,
        temperature: None,
        max_tokens: None,
    }
}

#[allow(
    dead_code,
    reason = "kept for the all-api preset's future cross-provider variants"
)]
fn openai(model: &str, key_env: &str) -> LlmFunctionConfig {
    LlmFunctionConfig {
        backend: "openai".into(),
        model: model.into(),
        api_key_env: Some(key_env.into()),
        base_url: None,
        reasoning_effort: None,
        temperature: None,
        max_tokens: None,
    }
}

#[allow(
    dead_code,
    reason = "kept for the all-api preset's future cross-provider variants"
)]
fn gemini(model: &str, key_env: &str) -> LlmFunctionConfig {
    LlmFunctionConfig {
        backend: "gemini".into(),
        model: model.into(),
        api_key_env: Some(key_env.into()),
        base_url: None,
        reasoning_effort: None,
        temperature: None,
        max_tokens: None,
    }
}

// LlmFunction::Cronista is intentionally referenced from the match
// arms below + the apply_env_overrides FUNCTIONS list so the
// `cronista: Option<LlmFunctionConfig>` field on `LlmConfig` keeps
// parsing from existing YAML files. The variant is marked
// `#[deprecated]` to discourage NEW consumers; the allow here scopes
// the warning suppression to the symmetry-maintaining code paths.
#[allow(deprecated, reason = "Cronista variant kept for YAML backward compat")]
impl LlmConfig {
    /// Lookup helper.
    #[must_use]
    pub const fn slot(&self, function: LlmFunction) -> Option<&LlmFunctionConfig> {
        match function {
            LlmFunction::HubWriter => self.hub_writer.as_ref(),
            LlmFunction::Ingest => self.ingest.as_ref(),
            LlmFunction::OperatorChat => self.operator_chat.as_ref(),
            LlmFunction::RemPromotions => self.rem_promotions.as_ref(),
            LlmFunction::RemDedupSemantic => self.rem_dedup_semantic.as_ref(),
            LlmFunction::Cronista => self.cronista.as_ref(),
            LlmFunction::Navigator => self.navigator.as_ref(),
        }
    }

    const fn slot_mut(&mut self, function: LlmFunction) -> &mut Option<LlmFunctionConfig> {
        match function {
            LlmFunction::HubWriter => &mut self.hub_writer,
            LlmFunction::Ingest => &mut self.ingest,
            LlmFunction::OperatorChat => &mut self.operator_chat,
            LlmFunction::RemPromotions => &mut self.rem_promotions,
            LlmFunction::RemDedupSemantic => &mut self.rem_dedup_semantic,
            LlmFunction::Cronista => &mut self.cronista,
            LlmFunction::Navigator => &mut self.navigator,
        }
    }

    /// Apply env-var overrides. The provided lookup function
    /// is injected so tests can drive overrides without touching the
    /// process environment.
    ///
    /// Returns the count of overrides applied — useful for logging at
    /// startup and for asserting in tests.
    pub fn apply_env_overrides<F>(&mut self, mut env: F) -> usize
    where
        F: FnMut(&str) -> Option<String>,
    {
        const FUNCTIONS: [LlmFunction; 7] = [
            LlmFunction::HubWriter,
            LlmFunction::Ingest,
            LlmFunction::OperatorChat,
            LlmFunction::RemPromotions,
            LlmFunction::RemDedupSemantic,
            LlmFunction::Cronista,
            LlmFunction::Navigator,
        ];
        let mut applied = 0usize;
        for func in FUNCTIONS {
            let prefix = func.env_prefix();
            let model_var = format!("{prefix}_MODEL");
            let backend_var = format!("{prefix}_BACKEND");
            let api_key_env_var = format!("{prefix}_API_KEY_ENV");
            let base_url_var = format!("{prefix}_BASE_URL");
            let temperature_var = format!("{prefix}_TEMPERATURE");
            let max_tokens_var = format!("{prefix}_MAX_TOKENS");

            let model = env(&model_var);
            let backend = env(&backend_var);
            let api_key_env = env(&api_key_env_var);
            let base_url = env(&base_url_var);
            let temperature_raw = env(&temperature_var);
            let max_tokens_raw = env(&max_tokens_var);

            if model.is_none()
                && backend.is_none()
                && api_key_env.is_none()
                && base_url.is_none()
                && temperature_raw.is_none()
                && max_tokens_raw.is_none()
            {
                continue;
            }

            let slot = self.slot_mut(func);
            let cfg = slot.get_or_insert_with(|| LlmFunctionConfig {
                backend: String::new(),
                model: String::new(),
                api_key_env: None,
                base_url: None,
                reasoning_effort: None,
                temperature: None,
                max_tokens: None,
            });
            if let Some(v) = model {
                cfg.model = v;
                applied += 1;
            }
            if let Some(v) = backend {
                cfg.backend = v;
                applied += 1;
            }
            if let Some(v) = api_key_env {
                cfg.api_key_env = Some(v);
                applied += 1;
            }
            if let Some(v) = base_url {
                cfg.base_url = Some(v);
                applied += 1;
            }
            // Numeric overrides parse leniently: a malformed value
            // logs a warning and is ignored rather than tearing down
            // the whole config load. Operator typo on a per-slot env
            // override should not crash a server that is otherwise
            // healthy — the YAML default carries on.
            if let Some(raw) = temperature_raw {
                match raw.trim().parse::<f32>() {
                    Ok(v) => {
                        cfg.temperature = Some(v);
                        applied += 1;
                    },
                    Err(e) => tracing::warn!(
                        var = %temperature_var,
                        value = %raw,
                        error = %e,
                        "config: ignoring malformed LLM temperature env override"
                    ),
                }
            }
            if let Some(raw) = max_tokens_raw {
                match raw.trim().parse::<u32>() {
                    Ok(v) => {
                        cfg.max_tokens = Some(v);
                        applied += 1;
                    },
                    Err(e) => tracing::warn!(
                        var = %max_tokens_var,
                        value = %raw,
                        error = %e,
                        "config: ignoring malformed LLM max_tokens env override"
                    ),
                }
            }
        }
        applied
    }
}

// ---------- Logging ----------

/// File-rotation cadence for the optional file sink.
///
/// `tracing-appender::rolling` exposes time-based rotation only (daily
/// / hourly / minutely / never); size-based rotation would need a
/// different dependency and is deferred until
/// a chattier load profile makes it necessary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFileRotation {
    /// One file per UTC day, rotated at midnight. The friendly default
    /// — an `info`-level mwe-mcp produces only a few MB/day, so a
    /// daily roll keeps the log directory small without losing detail.
    #[default]
    Daily,
    /// One file per UTC hour. Useful in load testing or when a bug
    /// only reproduces inside a narrow window.
    Hourly,
    /// No rotation: a single file grows for the lifetime of the
    /// deployment. The escape hatch for read-only mounts where the
    /// operator has wired external log shipping.
    Never,
    /// File sink disabled: only the `stderr` writer is installed
    /// (recovers the original stderr-only floor).
    Disabled,
}

impl LogFileRotation {
    /// YAML-side name for this variant.
    #[must_use]
    pub const fn yaml_name(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Hourly => "hourly",
            Self::Never => "never",
            Self::Disabled => "disabled",
        }
    }

    /// True when the file sink should be installed.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// Default `logging.file_path` value relative to the workdir.
///
/// Resolved against `<workdir>` at runtime in
/// [`LoggingConfig::resolved_file_path`]; that helper is the single
/// callsite the server uses when wiring `tracing-appender::rolling`.
pub const DEFAULT_LOG_FILE_PATH: &str = "logs/mwe-mcp.log";

/// `logging:` section of `mwe-mcp.config.yaml`.
///
/// Free-form keys other than `level` / `file_rotation` / `file_path`
/// are ignored — they round-trip through `extra` so a future config
/// that adds e.g. a `log_format` key does not need a forced restart of
/// the schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log filter level. See [`LogLevel`].
    #[serde(default)]
    pub level: LogLevel,
    /// File-sink rotation cadence (see [`LogFileRotation`]). Defaults
    /// to [`LogFileRotation::Daily`] so a fresh workdir comes with a
    /// rotating log file out-of-the-box; the operator can flip to
    /// `disabled` to recover the stderr-only original behaviour.
    #[serde(default)]
    pub file_rotation: LogFileRotation,
    /// File-sink target path. When relative, resolved against the
    /// workdir at startup; when absolute, used verbatim. Defaults to
    /// `<workdir>/logs/mwe-mcp.log` (the relative form
    /// [`DEFAULT_LOG_FILE_PATH`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
}

impl LoggingConfig {
    /// Resolve the configured `file_path` against `workdir`.
    ///
    /// Returns `None` when [`Self::file_rotation`] is
    /// [`LogFileRotation::Disabled`] (the operator opted out of the
    /// file sink entirely); otherwise returns the absolute path the
    /// rotating appender should write to. Relative paths are joined
    /// onto `workdir`; absolute paths are used verbatim so an operator
    /// can point at an external mount.
    #[must_use]
    pub fn resolved_file_path(&self, workdir: &Path) -> Option<PathBuf> {
        if !self.file_rotation.is_enabled() {
            return None;
        }
        let raw = self
            .file_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_FILE_PATH));
        if raw.is_absolute() {
            Some(raw)
        } else {
            Some(workdir.join(raw))
        }
    }
}

// ---------- REM ----------

/// `rem:` section of `mwe-mcp.config.yaml`.
///
/// Drives the scheduler that runs [`crate::rem::run_cycle`] inside the
/// long-lived HTTP server. The default profile is **enabled** with a
/// 24-hour cadence so a fresh deployment auto-organises memory without
/// the operator having to flip a switch (closes
/// `open-questions.md §13 rem-cycle-not-scheduled`). Operators who run
/// `rem::run_cycle` from an external scheduler (systemd timer, cron,
/// cloud scheduler) should set `schedule.mode: disabled` and invoke
/// `mwe-mcp rem run-cycle` on their own cadence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemConfig {
    /// Scheduling subsection. See [`RemScheduleConfig`].
    #[serde(default)]
    pub schedule: RemScheduleConfig,
    /// Auto-promote threshold overrides. See [`RemPolicyConfig`].
    #[serde(default)]
    pub policy: RemPolicyConfig,
}

/// `rem.policy:` subsection — overrides for the REM cycle policy knobs
/// in [`crate::rem::RemPolicy`] (auto-promote thresholds, per-cycle
/// sweep caps, the briefing-processor grace).
///
/// Each field is optional; an omitted field keeps the
/// `RemPolicy::default()` value. Exposed because the defaults (8 facts on
/// one page **and** 5 recalls) are calibrated for production, so a small
/// deployment that wants to exercise the auto-promotion → `wiki_promote`
/// path can lower the bar. Editable from the dashboard REM settings
/// panel (`/dashboard/admin/rem-settings`), which rewrites this section
/// and hot-swaps the running policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemPolicyConfig {
    /// Override `auto_promote_min_page_facts` (default 8).
    #[serde(default)]
    pub auto_promote_min_page_facts: Option<usize>,
    /// Override `auto_promote_subwiki_min_page_facts` (default 20) — the
    /// page→sub-wiki emergence mass bar.
    #[serde(default)]
    pub auto_promote_subwiki_min_page_facts: Option<usize>,
    /// Override `auto_promote_cap` (default 5).
    #[serde(default)]
    pub auto_promote_cap: Option<usize>,
    /// Override `page_merge_cap` (default 3) — confirmation calls the
    /// page-merge sub-job may spend per cycle; `0` disables the sub-job.
    #[serde(default)]
    pub page_merge_cap: Option<usize>,
    /// Override `completion_sweep_cap` (default 8) — evidence facts the
    /// completion sweep may send to the LLM per cycle; `0` disables.
    #[serde(default)]
    pub completion_sweep_cap: Option<usize>,
    /// Override `contradiction_sweep_cap` (default 8) — freshly
    /// contradicted seeds the cluster sweep may send per cycle; `0`
    /// disables.
    #[serde(default)]
    pub contradiction_sweep_cap: Option<usize>,
    /// Override `date_normalize_cap` (default 16) — flagged facts the
    /// date normalizer may send to the LLM per cycle; `0` disables.
    #[serde(default)]
    pub date_normalize_cap: Option<usize>,
    /// Override `provenance_hygiene_cap` (default 32) — trailing
    /// source-pointer facts the provenance-hygiene sweep repairs per
    /// cycle (deterministic, embedder spend only); `0` disables.
    #[serde(default)]
    pub provenance_hygiene_cap: Option<usize>,
    /// Override `briefing_processor_grace` in **seconds** (default
    /// 15 min = 900) — how long a fresh comment is left alone before
    /// the briefing processor interprets it into fact ops (the
    /// operator might still be editing it in the dashboard).
    #[serde(default)]
    pub briefing_processor_grace_secs: Option<u64>,
    /// Override `husk_gc_cap` (default 4) — plan-absent husk page files
    /// (all rows tombstoned or superseded past the revert window) the
    /// GC sweep removes per full cycle; `0` disables.
    #[serde(default)]
    pub husk_gc_cap: Option<usize>,
    /// Override `recall_repair_cap` (default 3) — pending recall misses
    /// the recall-repair sub-job judges per cycle (each costs a proposal
    /// completion + a gold-set gate replay); `0` disables.
    #[serde(default)]
    pub recall_repair_cap: Option<usize>,
    /// Override `recall_tuning_recurrence` (default 3) — miss count on
    /// the same fact at which an unrepaired miss queues the
    /// `recall_tuning_proposed` operator notice.
    #[serde(default)]
    pub recall_tuning_recurrence: Option<i64>,
}

impl RemConfig {
    /// Build the [`crate::rem::RemPolicy`] for this deployment: start from
    /// the defaults and apply the `policy:` overrides.
    #[must_use]
    pub fn resolved_policy(&self) -> crate::rem::RemPolicy {
        let mut p = crate::rem::RemPolicy::default();
        if let Some(m) = self.policy.auto_promote_min_page_facts {
            p.auto_promote_min_page_facts = m;
        }
        if let Some(m) = self.policy.auto_promote_subwiki_min_page_facts {
            p.auto_promote_subwiki_min_page_facts = m;
        }
        if let Some(c) = self.policy.auto_promote_cap {
            p.auto_promote_cap = c;
        }
        if let Some(c) = self.policy.page_merge_cap {
            p.page_merge_cap = c;
        }
        if let Some(c) = self.policy.completion_sweep_cap {
            p.completion_sweep_cap = c;
        }
        if let Some(c) = self.policy.contradiction_sweep_cap {
            p.contradiction_sweep_cap = c;
        }
        if let Some(c) = self.policy.date_normalize_cap {
            p.date_normalize_cap = c;
        }
        if let Some(c) = self.policy.provenance_hygiene_cap {
            p.provenance_hygiene_cap = c;
        }
        if let Some(s) = self.policy.briefing_processor_grace_secs {
            p.briefing_processor_grace =
                chrono::Duration::seconds(i64::try_from(s).unwrap_or(i64::MAX));
        }
        if let Some(c) = self.policy.husk_gc_cap {
            p.husk_gc_cap = c;
        }
        if let Some(c) = self.policy.recall_repair_cap {
            p.recall_repair_cap = c;
        }
        if let Some(r) = self.policy.recall_tuning_recurrence {
            p.recall_tuning_recurrence = r;
        }
        p
    }
}

/// `document:` section — resource knobs of the document-ingest pipeline
/// (document ingest).
///
/// Every knob is a resource cap (segment sizing, job cadence, merge
/// threshold), never a semantic gate — the disposition and the extraction
/// stay LLM judgments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentConfig {
    /// Override `poll_secs` (default 10) — worker poll cadence.
    #[serde(default)]
    pub poll_secs: Option<u64>,
    /// Override `segment_target_chars` (default 3000).
    #[serde(default)]
    pub segment_target_chars: Option<usize>,
    /// Override `segment_max_chars` (default 4500).
    #[serde(default)]
    pub segment_max_chars: Option<usize>,
    /// Override `max_segments` (default 400) — a larger document is
    /// refused at enqueue (no silent truncation).
    #[serde(default)]
    pub max_segments: Option<usize>,
    /// Override `max_facts_per_segment` (default 12).
    #[serde(default)]
    pub max_facts_per_segment: Option<usize>,
    /// Override `classify_sample_chars` (default 6000) — the document
    /// prefix the disposition classifier sees.
    #[serde(default)]
    pub classify_sample_chars: Option<usize>,
    /// Override `merge_threshold` (default 0.90) — embedding cosine above
    /// which two candidate facts cluster for the reduce merge.
    #[serde(default)]
    pub merge_threshold: Option<f32>,
    /// Override `max_document_chars` (default 1,500,000) — hard input cap
    /// at enqueue.
    #[serde(default)]
    pub max_document_chars: Option<usize>,
}

impl DocumentConfig {
    /// Build the [`crate::document::DocumentPolicy`] for this deployment:
    /// defaults + the `document:` overrides.
    #[must_use]
    pub fn resolved_policy(&self) -> crate::document::DocumentPolicy {
        let mut p = crate::document::DocumentPolicy::default();
        if let Some(v) = self.poll_secs {
            p.poll_secs = v;
        }
        if let Some(v) = self.segment_target_chars {
            p.segment_target_chars = v;
        }
        if let Some(v) = self.segment_max_chars {
            p.segment_max_chars = v;
        }
        if let Some(v) = self.max_segments {
            p.max_segments = v;
        }
        if let Some(v) = self.max_facts_per_segment {
            p.max_facts_per_segment = v;
        }
        if let Some(v) = self.classify_sample_chars {
            p.classify_sample_chars = v;
        }
        if let Some(v) = self.merge_threshold {
            p.merge_threshold = v;
        }
        if let Some(v) = self.max_document_chars {
            p.max_document_chars = v;
        }
        p
    }
}

/// `rem.schedule:` subsection.
///
/// `mode: interval` runs `run_cycle` on a tokio interval ticker; the
/// first tick fires after `initial_delay_secs` to let the server warm
/// up and then every `interval_secs` thereafter. `mode: disabled`
/// keeps the binary inert and is appropriate when an external
/// scheduler invokes the CLI escape hatch.
///
/// `cron` (wall-clock "nightly at 03:00") is a future enhancement —
/// the cap on engineering complexity for this milestone is the
/// interval ticker, which covers the standard PWA-as-permanent-daemon
/// deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemScheduleConfig {
    /// Scheduler mode (`interval` or `disabled`).
    #[serde(default)]
    pub mode: RemScheduleMode,
    /// Distance between consecutive cycle runs in `interval` mode.
    /// Default: `86_400` seconds (24 hours).
    #[serde(default = "default_rem_interval_secs")]
    pub interval_secs: u64,
    /// Delay before the **first** cycle fires after startup. Lets the
    /// server warm up (LLM health-checks, embedder load, dashboard
    /// boot) before the first batch of work kicks in. Default: 300
    /// seconds (5 minutes).
    #[serde(default = "default_rem_initial_delay_secs")]
    pub initial_delay_secs: u64,
    /// Light dream — distance between consecutive light-dream runs
    /// (captures→facts promotion). Far more frequent than the full cycle so a
    /// buffered standard-wiki capture becomes recallable quickly. Default: `3_600`
    /// seconds (1 hour). The light dream shares `mode`: `disabled` turns both
    /// the full cycle and the light dream off.
    #[serde(default = "default_light_interval_secs")]
    pub light_interval_secs: u64,
    /// Delay before the **first** light-dream run after startup. Default: 60s.
    #[serde(default = "default_light_initial_delay_secs")]
    pub light_initial_delay_secs: u64,
    /// Backlog of buffered captures that triggers a light-dream run ahead of the
    /// timer (the "soglia"). Default: `20`. `0` disables the early trigger
    /// (timer only).
    #[serde(default = "default_light_backlog_threshold")]
    pub light_backlog_threshold: i64,
}

impl Default for RemScheduleConfig {
    fn default() -> Self {
        Self {
            mode: RemScheduleMode::Interval,
            interval_secs: default_rem_interval_secs(),
            initial_delay_secs: default_rem_initial_delay_secs(),
            light_interval_secs: default_light_interval_secs(),
            light_initial_delay_secs: default_light_initial_delay_secs(),
            light_backlog_threshold: default_light_backlog_threshold(),
        }
    }
}

/// `mode` enum for [`RemScheduleConfig`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemScheduleMode {
    /// Tokio interval ticker. First fire after
    /// `initial_delay_secs`, then every `interval_secs`.
    #[default]
    Interval,
    /// Scheduler is off; the operator drives REM through the CLI.
    Disabled,
}

const fn default_rem_interval_secs() -> u64 {
    86_400
}

const fn default_light_interval_secs() -> u64 {
    3_600
}

const fn default_light_initial_delay_secs() -> u64 {
    60
}

const fn default_light_backlog_threshold() -> i64 {
    20
}

const fn default_rem_initial_delay_secs() -> u64 {
    300
}

// ---------- Recall ----------

/// `recall:` section of `mwe-mcp.config.yaml` — the operator's recall
/// settings: the resource knobs of the per-turn recall block (flat slot,
/// navigator funnel, due-soon slot).
///
/// Every field is optional and mirrors its Rust name; an omitted field
/// keeps the default from [`crate::ingest::IngestPolicy`] /
/// [`crate::recall_nav::NavigatorPolicy`]. Only resources are
/// configured here — semantic judgment (link choice, stopping) lives in
/// the `navigator` prompt, never in a knob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallConfig {
    /// Override `IngestPolicy::recall_top_k` (flat-slot size, default 5).
    #[serde(default)]
    pub recall_top_k: Option<usize>,
    /// Override `IngestPolicy::recall_fresh_top_k` (fresh-slot size,
    /// default 3; `0` disables the slot).
    #[serde(default)]
    pub recall_fresh_top_k: Option<usize>,
    /// Override `NavigatorPolicy::max_hops` — the depth dial (default 2;
    /// the funnel clamps it to its hard hop cap).
    #[serde(default)]
    pub max_hops: Option<usize>,
    /// Override `NavigatorPolicy::pages_per_hop` (default 3).
    #[serde(default)]
    pub pages_per_hop: Option<usize>,
    /// Override `NavigatorPolicy::char_budget` — the total prose budget
    /// of a navigation (default 8000).
    #[serde(default)]
    pub char_budget: Option<usize>,
    /// Override `NavigatorPolicy::max_candidates` per hop (default 16).
    #[serde(default)]
    pub max_candidates: Option<usize>,
    /// Override `NavigatorPolicy::decision_max_tokens` (default 600).
    #[serde(default)]
    pub decision_max_tokens: Option<u32>,
    /// Override `IngestPolicy::due_soon_top_k` (default 3; `0` disables
    /// the slot).
    #[serde(default)]
    pub due_soon_top_k: Option<usize>,
    /// Override `IngestPolicy::due_soon_horizon_hours` (default 168 =
    /// 7 days).
    #[serde(default)]
    pub due_soon_horizon_hours: Option<u32>,
    /// Override `IngestPolicy::max_agent_identity_chars` — the recall
    /// block's `WHO YOU ARE` section budget (default 900).
    #[serde(default)]
    pub max_agent_identity_chars: Option<usize>,
    /// Override `IngestPolicy::max_agent_history_chars` — the recall
    /// block's `YOUR RECENT HISTORY WITH THIS USER` section budget
    /// (default 1400).
    #[serde(default)]
    pub max_agent_history_chars: Option<usize>,
    /// Deployment-wide IANA timezone of the users (e.g. `Europe/Rome`) →
    /// sets `IngestPolicy::ingest_timezone`. Also settable via the
    /// `MWE_INGEST_TIMEZONE` env var; this YAML field wins when both are set.
    /// Unset (and env absent) → the classifier sees only the UTC
    /// `current_time` anchor, and a wall-clock time the user speaks is stamped
    /// as UTC (the historical behaviour). Set it for any single-timezone
    /// deployment so dated commitments land at the right instant.
    #[serde(default)]
    pub ingest_timezone: Option<String>,
    /// Override `IngestPolicy::recent_window_entries` — per-user cap of the
    /// cross-consumer recent window's buffer (default 32; `0` disables the
    /// window entirely).
    #[serde(default)]
    pub recent_window_entries: Option<usize>,
    /// Override `IngestPolicy::recent_window_ttl_hours` — how long an
    /// exchange stays servable (default 4; short by design — the window
    /// serves the thread of discourse, not history).
    #[serde(default)]
    pub recent_window_ttl_hours: Option<u32>,
    /// Override `IngestPolicy::recent_window_chars` — char budget of the
    /// rendered `recent_window` section (default 1200; `0` stops serving
    /// while buffering continues).
    #[serde(default)]
    pub recent_window_chars: Option<usize>,
}

impl RecallConfig {
    /// Build the per-turn [`crate::ingest::IngestPolicy`] for this
    /// deployment: start from the defaults and apply this section's
    /// overrides. The prompt-budget knobs of `IngestPolicy` (recent
    /// messages, wiki/group/user caps) keep their defaults — they are
    /// classifier plumbing, not recall settings.
    #[must_use]
    pub fn resolved_ingest_policy(&self) -> crate::ingest::IngestPolicy {
        let mut p = crate::ingest::IngestPolicy::default();
        if let Some(v) = self.recall_top_k {
            p.recall_top_k = v;
        }
        if let Some(v) = self.recall_fresh_top_k {
            p.recall_fresh_top_k = v;
        }
        if let Some(v) = self.max_hops {
            p.nav.max_hops = v;
        }
        if let Some(v) = self.pages_per_hop {
            p.nav.pages_per_hop = v;
        }
        if let Some(v) = self.char_budget {
            p.nav.char_budget = v;
        }
        if let Some(v) = self.max_candidates {
            p.nav.max_candidates = v;
        }
        if let Some(v) = self.decision_max_tokens {
            p.nav.decision_max_tokens = v;
        }
        if let Some(v) = self.due_soon_top_k {
            p.due_soon_top_k = v;
        }
        if let Some(v) = self.due_soon_horizon_hours {
            p.due_soon_horizon_hours = v;
        }
        if let Some(v) = self.max_agent_identity_chars {
            p.max_agent_identity_chars = v;
        }
        if let Some(v) = self.max_agent_history_chars {
            p.max_agent_history_chars = v;
        }
        if let Some(v) = self.recent_window_entries {
            p.recent_window_entries = v;
        }
        if let Some(v) = self.recent_window_ttl_hours {
            p.recent_window_ttl_hours = v;
        }
        if let Some(v) = self.recent_window_chars {
            p.recent_window_chars = v;
        }
        // Timezone: YAML field wins; otherwise fall back to the
        // `MWE_INGEST_TIMEZONE` env var so a deployment can enable it with a
        // single env line. Blank values normalise to `None` (UTC-only anchor).
        p.ingest_timezone = self
            .ingest_timezone
            .clone()
            .or_else(|| std::env::var("MWE_INGEST_TIMEZONE").ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        p
    }
}

// ---------- Embedding ----------

/// `embedding:` config section — which [`Embedder`](crate::embedder::Embedder)
/// backend drives recall / capture / dedup.
///
/// Until this section existed the embedder was hardcoded to
/// `OllamaEmbedder::local_bge_m3()` at the server construction sites
/// (roadmap group 18). The section is honoured by
/// [`EmbeddingConfig::build_embedder`], the single factory those sites now
/// call. An absent section deserializes to [`EmbeddingConfig::default`]:
/// `bge-m3` / 1024-dim, with the backend chosen by the build — `bundled`
/// on a release build (compiled with `local-embedder`), `ollama` otherwise.
///
/// `backend` choices:
/// - `bundled` — the in-binary Candle embedder ([`crate::local_embedder`]):
///   the default on a release build, needs no external service. Available
///   only when compiled with the `local-embedder` feature.
/// - `ollama` — HTTP to a local/remote Ollama (`base_url`): the default on
///   a build without the bundled feature, and the opt-in for operators who
///   already run Ollama and prefer not to keep a second embedder.
/// - `openai` — reserved; not yet wired (→ `UnsupportedEmbeddingBackend`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Backend name: `ollama` | `bundled` | `openai`.
    #[serde(default = "default_embedding_backend")]
    pub backend: String,
    /// Model id. For `ollama` it is the model name sent on the wire; for
    /// `bundled` it is the stable [`Embedder::model_id`](crate::embedder::Embedder::model_id)
    /// used in cache keys + reindex checks. Defaults to `bge-m3`.
    #[serde(default = "default_embedding_model")]
    pub model: String,
    /// Endpoint override for the `ollama` backend (e.g. a remote host).
    /// `None` → `http://localhost:11434`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Compute device for the `bundled` backend: `cpu` (default) or
    /// `gpu`. `gpu` requires a CUDA build (roadmap 18f); on a CPU-only
    /// binary it is refused.
    #[serde(default = "default_embedding_device")]
    pub device: String,
    /// Vector dimension the chosen model produces (sanity-checked on
    /// every embed). `bge-m3` is 1024. Used by the `ollama` backend; the
    /// `bundled` backend reads its own dimension from the model config.
    #[serde(default = "default_embedding_dimensions")]
    pub dimensions: usize,
    /// Directory holding the `bundled` backend's weights (`config.json`,
    /// `tokenizer.json`, `pytorch_model.bin`) — the offline / air-gapped
    /// path. Download-on-first-run into a default cache lands with roadmap
    /// 18c. Ignored by other backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
}

/// Whether this binary was compiled with the bundled (Candle) embedder.
///
/// `true` iff the `local-embedder` Cargo feature is on. The dashboard
/// embedding editor uses it to mark the `bundled` backend available (or
/// not) in this build.
#[must_use]
pub const fn bundled_embedder_available() -> bool {
    cfg!(feature = "local-embedder")
}

fn default_embedding_backend() -> String {
    // The published release is built with `local-embedder`, so a fresh
    // deployment defaults to the zero-dependency bundled embedder
    // (maintainer 2026-06-22: default bundled, Ollama optional — so an
    // operator who already runs Ollama with an embedder is not forced to
    // keep a second model). A build WITHOUT the feature (dev / CI / source
    // build) cannot construct the bundled backend, so it falls back to
    // `ollama` rather than failing to boot.
    if bundled_embedder_available() {
        "bundled".to_owned()
    } else {
        "ollama".to_owned()
    }
}
fn default_embedding_model() -> String {
    crate::embedder::DEFAULT_EMBED_MODEL.to_owned()
}
fn default_embedding_device() -> String {
    "cpu".to_owned()
}
const fn default_embedding_dimensions() -> usize {
    1024
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            backend: default_embedding_backend(),
            model: default_embedding_model(),
            base_url: None,
            device: default_embedding_device(),
            dimensions: default_embedding_dimensions(),
            model_dir: None,
        }
    }
}

impl EmbeddingConfig {
    /// Build the configured [`Embedder`](crate::embedder::Embedder), ready
    /// to share behind an `Arc`. The single chokepoint the server
    /// construction sites call instead of hardcoding a backend
    /// (roadmap group 18).
    ///
    /// # Errors
    ///
    /// - [`ConfigError::UnsupportedEmbeddingBackend`] when `backend` is
    ///   `openai` or an unknown string.
    /// - [`ConfigError::EmbeddingUnavailable`] when `bundled` is selected
    ///   on a binary built without the `local-embedder` feature, when
    ///   `device: gpu` is requested without a CUDA build, when `bundled`
    ///   has no `model_dir`, or when a backend constructor fails.
    pub async fn build_embedder(&self) -> Result<std::sync::Arc<dyn crate::embedder::Embedder>> {
        use std::sync::Arc;
        match self.backend.as_str() {
            "ollama" => {
                let base_url = self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::embedder::DEFAULT_OLLAMA_URL.to_owned());
                let e = crate::embedder::OllamaEmbedder::new(
                    base_url,
                    self.model.clone(),
                    self.dimensions,
                )
                .map_err(|e| ConfigError::EmbeddingUnavailable {
                    detail: format!("building ollama embedder: {e}"),
                })?;
                Ok(Arc::new(e))
            },
            "bundled" => self.build_bundled().await,
            other => Err(ConfigError::UnsupportedEmbeddingBackend {
                backend: other.to_owned(),
            }),
        }
    }

    /// `bundled` arm of [`Self::build_embedder`] when the `local-embedder`
    /// feature is compiled in. Resolves the weights — an explicit
    /// `model_dir` (offline / air-gapped) wins; otherwise the default cache,
    /// auto-downloaded on first use (bge-m3 only, roadmap 18c) — then loads
    /// the model onto the chosen device.
    #[cfg(feature = "local-embedder")]
    async fn build_bundled(&self) -> Result<std::sync::Arc<dyn crate::embedder::Embedder>> {
        use std::sync::Arc;
        let device = match self.device.as_str() {
            "cpu" => candle_core::Device::Cpu,
            "gpu" => {
                return Err(ConfigError::EmbeddingUnavailable {
                    detail:
                        "device `gpu` needs a CUDA build (roadmap 18f); this binary is CPU-only"
                            .to_owned(),
                });
            },
            other => {
                return Err(ConfigError::EmbeddingUnavailable {
                    detail: format!("unknown embedding.device {other:?}: expected `cpu` or `gpu`"),
                });
            },
        };

        // An operator-provided directory is trusted as-is (the offline
        // path); otherwise resolve the default cache and fetch on first use.
        let model_dir = if let Some(dir) = self.model_dir.as_ref() {
            std::path::PathBuf::from(dir)
        } else {
            if self.model != crate::embedder::DEFAULT_EMBED_MODEL {
                return Err(ConfigError::EmbeddingUnavailable {
                    detail: format!(
                        "auto-download supports only `{}`; set embedding.model_dir for model {:?}",
                        crate::embedder::DEFAULT_EMBED_MODEL,
                        self.model
                    ),
                });
            }
            let cache = crate::local_embedder::default_cache_dir(&self.model);
            crate::local_embedder::ensure_bge_m3_weights(&cache)
                .await
                .map_err(|e| ConfigError::EmbeddingUnavailable {
                    detail: format!("fetching bge-m3 weights into {}: {e}", cache.display()),
                })?;
            cache
        };

        let e = crate::local_embedder::LocalEmbedder::load(&model_dir, device, self.model.clone())
            .map_err(|e| ConfigError::EmbeddingUnavailable {
                detail: format!("loading bundled embedder from {}: {e}", model_dir.display()),
            })?;
        Ok(Arc::new(e))
    }

    /// `bundled` arm when the `local-embedder` feature is absent: the
    /// backend is simply not in this binary.
    #[cfg(not(feature = "local-embedder"))]
    #[allow(clippy::unused_async, clippy::unused_self)]
    async fn build_bundled(&self) -> Result<std::sync::Arc<dyn crate::embedder::Embedder>> {
        Err(ConfigError::EmbeddingUnavailable {
            detail: "backend `bundled` requires a build with the `local-embedder` feature"
                .to_owned(),
        })
    }
}

// ---------- Email (SMTP password recovery, roadmap 28) ----------

/// `email:` section — the SMTP backend that powers self-service
/// password recovery (roadmap 28).
///
/// Off by default: with `enabled: false` (or any required field unset)
/// the dashboard hides the "forgot password" affordance and the request
/// route is inert, so a fresh deployment behaves exactly as before. The
/// SMTP password is **never** stored in the YAML — `password_env` names
/// an env-var (default `MWE_SMTP_PASSWORD`) read from the process
/// environment at send time, mirroring how the cloud LLM keys are
/// handled (`api_key_env`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailConfig {
    /// Master switch. Recovery mail is sent only when this is `true`
    /// **and** the SMTP fields below resolve. Default `false`.
    #[serde(default)]
    pub enabled: bool,
    /// SMTP relay host (e.g. `smtp.fastmail.com`). Required when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    /// SMTP relay port. `587` = STARTTLS (the default), `465` = implicit
    /// TLS, `25` = plaintext (dev / localhost only).
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// TLS mode: `starttls` (default) | `implicit` | `none`.
    #[serde(default = "default_smtp_tls")]
    pub tls: String,
    /// `From:` address the recovery mail is sent from. Required when
    /// enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    /// Optional display name on the `From:` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    /// SMTP AUTH username. `None` → no authentication (an open relay or
    /// localhost MTA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Env-var holding the SMTP password (never the YAML). Default
    /// `MWE_SMTP_PASSWORD`; read from the process environment at send
    /// time. Ignored when `username` is `None`.
    #[serde(default = "default_smtp_password_env")]
    pub password_env: String,
    /// Public origin used to build the absolute reset link in the email
    /// (e.g. `https://mwe.contea.casa`). `None` → derived from the
    /// request `Host` + forwarded scheme at send time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
}

const fn default_smtp_port() -> u16 {
    587
}
fn default_smtp_tls() -> String {
    "starttls".to_owned()
}
fn default_smtp_password_env() -> String {
    "MWE_SMTP_PASSWORD".to_owned()
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: None,
            smtp_port: default_smtp_port(),
            tls: default_smtp_tls(),
            from_address: None,
            from_name: None,
            username: None,
            password_env: default_smtp_password_env(),
            public_base_url: None,
        }
    }
}

impl EmailConfig {
    /// Whether the section carries enough to send mail: enabled, with a
    /// host and a `From:` address. The actual SMTP password (when a
    /// username is set) is checked at send time against the env-var, so a
    /// misconfigured secret surfaces on the "send test email" path rather
    /// than here.
    #[must_use]
    pub const fn is_sendable(&self) -> bool {
        self.enabled && self.smtp_host.is_some() && self.from_address.is_some()
    }
}

// ---------- Training spool ----------

/// `training_spool:` section — the prompt/completion training-pair
/// spool (see [`crate::training_spool`]).
///
/// Off by default: the spool holds raw prompts, which embed recalled
/// memory content, so turning it on is an explicit operator choice
/// (dashboard panel or YAML).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingSpoolConfig {
    /// Record internal-LLM prompt/completion pairs to
    /// `<workdir>/training-spool/`. Hot-toggleable from the dashboard.
    #[serde(default)]
    pub enabled: bool,
}

// ---------- Config ----------

/// Top-level config object.
///
/// Sub-sections appear here as more modules need them; for now
/// `logging`, `llm`, and `rem` are materialised, and the rest of the
/// YAML is captured opaquely in [`Config::extra`] so it does not fail
/// validation just because it predates the Rust struct.
///
/// `PartialEq` only (no `Eq`): inherits from [`LlmConfig`], which is
/// in turn `PartialEq`-only because of the `f32`-typed temperature
/// knob in [`LlmFunctionConfig`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `logging:` section.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// `llm:` section — five canonical functions.
    #[serde(default)]
    pub llm: LlmConfig,
    /// `embedding:` section — which embedder backend drives recall /
    /// capture / dedup. See [`EmbeddingConfig`].
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    /// `email:` section — SMTP backend for self-service password
    /// recovery (roadmap 28). Off by default. See [`EmailConfig`].
    #[serde(default)]
    pub email: EmailConfig,
    /// `rem:` section — scheduler for [`crate::rem::run_cycle`].
    #[serde(default)]
    pub rem: RemConfig,
    /// `recall:` section — the per-turn recall block's resource knobs.
    #[serde(default)]
    pub recall: RecallConfig,
    /// `document:` section — the document-ingest pipeline's resource knobs.
    #[serde(default)]
    pub document: DocumentConfig,
    /// `training_spool:` section — the prompt/completion training-pair
    /// spool. See [`TrainingSpoolConfig`].
    #[serde(default)]
    pub training_spool: TrainingSpoolConfig,
    /// Every other key in the YAML, preserved verbatim so we never
    /// strip an operator's settings during a round-trip.
    #[serde(flatten)]
    pub extra: serde_yaml::Mapping,
}

impl Config {
    /// Absolute path of the on-disk config file in `workdir`.
    #[must_use]
    pub fn path_in(workdir: &Path) -> PathBuf {
        workdir.join(CONFIG_FILENAME)
    }

    /// Load the config from `<workdir>/mwe-mcp.config.yaml`.
    ///
    /// Returns [`Config::default`] when the file is absent. Returns a
    /// [`ConfigError`] when the file exists but is malformed — silent
    /// fallback would mask operator-side typos.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Io`] for any IO failure other than not-found.
    /// - [`ConfigError::Parse`] for YAML parse failures.
    /// - [`ConfigError::InvalidLogLevel`] when `logging.level` is
    ///   neither `info` nor `debug`.
    pub fn load(workdir: &Path) -> Result<Self> {
        let path = Self::path_in(workdir);
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    config = %path.display(),
                    "config: file absent, falling back to defaults"
                );
                let mut cfg = Self::default();
                let n = cfg.llm.apply_env_overrides(|k| std::env::var(k).ok());
                if n > 0 {
                    tracing::info!(overrides = n, "config: applied LLM env overrides");
                }
                return Ok(cfg);
            },
            Err(e) => return Err(e.into()),
        };
        let mut cfg = Self::parse(&path, &raw)?;
        let n = cfg.llm.apply_env_overrides(|k| std::env::var(k).ok());
        if n > 0 {
            tracing::info!(overrides = n, "config: applied LLM env overrides");
        }
        Ok(cfg)
    }

    /// Parse from a raw YAML string. `path` is informational.
    ///
    /// # Errors
    ///
    /// See [`Config::load`].
    pub fn parse(path: &Path, raw: &str) -> Result<Self> {
        // Two-step parse so we can map the deserializer's error onto
        // our richer `ConfigError::Parse` and validate `logging.level`
        // explicitly (avoiding a generic enum-variant deserializer
        // message the operator would have to decode).
        let value: serde_yaml::Value =
            serde_yaml::from_str(raw).map_err(|e| ConfigError::Parse {
                path: path.to_path_buf(),
                detail: format!("yaml: {e}"),
            })?;
        Self::validate_log_level(&value)?;
        Self::validate_log_file_rotation(&value)?;
        Self::validate_rem_schedule_mode(&value)?;
        let cfg: Self = serde_yaml::from_value(value).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            detail: format!("schema: {e}"),
        })?;
        Ok(cfg)
    }

    fn validate_log_level(value: &serde_yaml::Value) -> Result<()> {
        let Some(level) = value
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("logging".into())))
            .and_then(serde_yaml::Value::as_mapping)
            .and_then(|m| m.get(serde_yaml::Value::String("level".into())))
        else {
            return Ok(());
        };
        let Some(s) = level.as_str() else {
            return Err(ConfigError::InvalidLogLevel {
                value: format!("{level:?}"),
            });
        };
        if matches!(s, "info" | "debug") {
            Ok(())
        } else {
            Err(ConfigError::InvalidLogLevel {
                value: s.to_owned(),
            })
        }
    }

    fn validate_log_file_rotation(value: &serde_yaml::Value) -> Result<()> {
        let Some(rot) = value
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("logging".into())))
            .and_then(serde_yaml::Value::as_mapping)
            .and_then(|m| m.get(serde_yaml::Value::String("file_rotation".into())))
        else {
            return Ok(());
        };
        let Some(s) = rot.as_str() else {
            return Err(ConfigError::InvalidLogFileRotation {
                value: format!("{rot:?}"),
            });
        };
        if matches!(s, "daily" | "hourly" | "never" | "disabled") {
            Ok(())
        } else {
            Err(ConfigError::InvalidLogFileRotation {
                value: s.to_owned(),
            })
        }
    }

    fn validate_rem_schedule_mode(value: &serde_yaml::Value) -> Result<()> {
        let Some(mode) = value
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("rem".into())))
            .and_then(serde_yaml::Value::as_mapping)
            .and_then(|m| m.get(serde_yaml::Value::String("schedule".into())))
            .and_then(serde_yaml::Value::as_mapping)
            .and_then(|m| m.get(serde_yaml::Value::String("mode".into())))
        else {
            return Ok(());
        };
        let Some(s) = mode.as_str() else {
            return Err(ConfigError::InvalidRemScheduleMode {
                value: format!("{mode:?}"),
            });
        };
        if matches!(s, "interval" | "disabled") {
            Ok(())
        } else {
            Err(ConfigError::InvalidRemScheduleMode {
                value: s.to_owned(),
            })
        }
    }
}

#[cfg(test)]
#[allow(
    deprecated,
    reason = "tests cover the YAML-backward-compat surface that still references Cronista"
)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------- LlmProfile ----------

    #[test]
    fn llm_profile_parse_accepts_canonical_and_underscore_variants() {
        assert_eq!(
            LlmProfile::parse("all-local").unwrap(),
            LlmProfile::AllLocal
        );
        assert_eq!(
            LlmProfile::parse("All_Local").unwrap(),
            LlmProfile::AllLocal
        );
        assert_eq!(LlmProfile::parse(" hybrid ").unwrap(), LlmProfile::Hybrid);
        assert_eq!(LlmProfile::parse("all-api").unwrap(), LlmProfile::AllApi);
        assert_eq!(LlmProfile::parse("custom").unwrap(), LlmProfile::Custom);
        assert!(LlmProfile::parse("nope").is_err());
    }

    #[test]
    fn llm_profile_all_local_seeds_every_slot_on_ollama() {
        let cfg = LlmProfile::AllLocal.build();
        assert_eq!(cfg.profile.as_deref(), Some("all-local"));
        for func in [
            LlmFunction::HubWriter,
            LlmFunction::Ingest,
            LlmFunction::RemPromotions,
            LlmFunction::RemDedupSemantic,
            LlmFunction::Cronista,
            LlmFunction::Navigator,
        ] {
            let slot = cfg.slot(func).unwrap_or_else(|| panic!("{func:?} missing"));
            assert_eq!(slot.backend, "ollama");
            assert!(slot.api_key_env.is_none());
        }
    }

    #[test]
    fn llm_profile_hybrid_puts_conversational_slots_local_and_nightly_slots_on_anthropic() {
        // Updated post-realignment with the user:
        // hub_writer + ingest + rem_dedup_semantic all reuse the local
        // workhorse; rem_promotions + cronista go to Opus 4.7 with
        // extra-high reasoning effort on the structural decisions.
        let cfg = LlmProfile::Hybrid.build();
        assert_eq!(cfg.ingest.as_ref().unwrap().backend, "ollama");
        assert_eq!(cfg.hub_writer.as_ref().unwrap().backend, "ollama");
        assert_eq!(cfg.rem_dedup_semantic.as_ref().unwrap().backend, "ollama");
        let rem = cfg.rem_promotions.as_ref().unwrap();
        assert_eq!(rem.backend, "anthropic");
        assert_eq!(rem.model, "claude-opus-4-7");
        assert_eq!(rem.reasoning_effort.as_deref(), Some("extra-high"));
        let cronista = cfg.cronista.as_ref().unwrap();
        assert_eq!(cronista.backend, "anthropic");
        assert_eq!(cronista.model, "claude-opus-4-7");
        // The per-turn navigator goes strong-but-cheap, not local: link
        // choice is the recall quality bar, and the workhorse is busy
        // with ingest on every turn anyway.
        let navigator = cfg.navigator.as_ref().unwrap();
        assert_eq!(navigator.backend, "anthropic");
        assert_eq!(navigator.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn llm_profile_all_local_reuses_workhorse_for_dedup() {
        // Post-realignment: dedup runs on the same Qwen 9B that's
        // already in VRAM for hub_writer + ingest. Zero VRAM extra,
        // no model swap during the REM cycle.
        let cfg = LlmProfile::AllLocal.build();
        assert_eq!(
            cfg.rem_dedup_semantic.as_ref().unwrap().model,
            "qwen3.5:9b-q8_0"
        );
        let rem = cfg.rem_promotions.as_ref().unwrap();
        assert_eq!(rem.reasoning_effort.as_deref(), Some("extra-high"));
    }

    #[test]
    fn llm_profile_all_api_uses_opus_for_strong_slots() {
        let cfg = LlmProfile::AllApi.build();
        for slot in [cfg.rem_promotions.as_ref(), cfg.cronista.as_ref()] {
            let s = slot.expect("strong slot present");
            assert_eq!(s.backend, "anthropic");
            assert_eq!(s.model, "claude-opus-4-7");
        }
        assert_eq!(
            cfg.rem_promotions
                .as_ref()
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("extra-high"),
        );
    }

    #[test]
    fn llm_profile_custom_returns_empty_slots() {
        let cfg = LlmProfile::Custom.build();
        assert!(cfg.hub_writer.is_none());
        assert!(cfg.ingest.is_none());
        assert!(cfg.rem_promotions.is_none());
        assert!(cfg.rem_dedup_semantic.is_none());
        assert!(cfg.cronista.is_none());
        assert!(cfg.navigator.is_none());
        assert_eq!(cfg.profile.as_deref(), Some("custom"));
    }

    #[test]
    fn llm_profile_yaml_round_trips_through_serde() {
        let cfg = LlmProfile::AllLocal.build();
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let parsed: LlmConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, cfg);
    }

    // ---------- LogLevel ----------

    #[test]
    fn log_level_default_is_info() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn log_level_env_filter_strings() {
        assert!(LogLevel::Info.as_env_filter().contains("mwe_core=info"));
        assert!(LogLevel::Debug.as_env_filter().contains("mwe_core=debug"));
        // External crates pinned to warn — operator should not see
        // sqlx / notify chatter.
        assert!(LogLevel::Info.as_env_filter().starts_with("warn"));
    }

    // ---------- Config::load ----------

    #[test]
    fn load_returns_default_when_file_absent() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.logging.level, LogLevel::Info);
    }

    #[test]
    fn load_parses_logging_section_only() {
        let dir = tempdir().unwrap();
        let body = "logging:\n  level: debug\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.level, LogLevel::Debug);
    }

    #[test]
    fn load_preserves_unknown_top_level_keys_in_extra() {
        let dir = tempdir().unwrap();
        // `embedding` is now a typed section; `budget` is still unknown
        // and must round-trip through `extra` without breaking parse.
        let body = "logging:\n  level: info\nembedding:\n  backend: ollama\n  model: bge-m3\nbudget:\n  monthly_eur_cap: 20\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.level, LogLevel::Info);
        // `embedding` parsed into the typed field, not `extra`.
        assert_eq!(cfg.embedding.backend, "ollama");
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert!(
            !cfg.extra
                .contains_key(serde_yaml::Value::String("embedding".into())),
            "typed embedding section must not leak into extra"
        );
        // `budget` is still unknown → preserved in extra.
        assert!(
            cfg.extra
                .contains_key(serde_yaml::Value::String("budget".into()))
        );
    }

    #[test]
    fn embedding_default_backend_follows_build() {
        let cfg = Config::default();
        // The default backend tracks the build: `bundled` when the binary
        // carries the feature (the release), `ollama` otherwise.
        #[cfg(feature = "local-embedder")]
        assert_eq!(cfg.embedding.backend, "bundled");
        #[cfg(not(feature = "local-embedder"))]
        assert_eq!(cfg.embedding.backend, "ollama");
        assert_eq!(cfg.embedding.model, "bge-m3");
        assert_eq!(cfg.embedding.dimensions, 1024);
        assert_eq!(cfg.embedding.device, "cpu");
        assert!(cfg.embedding.base_url.is_none());
        assert!(cfg.embedding.model_dir.is_none());
    }

    #[cfg(not(feature = "local-embedder"))]
    #[tokio::test]
    async fn embedding_absent_section_builds_default_ollama_embedder() {
        // Without the bundled feature the default backend is Ollama bge-m3
        // (1024-dim); the constructor does no network I/O.
        let e = EmbeddingConfig::default()
            .build_embedder()
            .await
            .expect("default ollama embedder builds");
        assert_eq!(e.model_id(), "bge-m3");
        assert_eq!(e.dimensions(), 1024);
    }

    #[cfg(feature = "local-embedder")]
    #[tokio::test]
    async fn embedding_explicit_ollama_builds_when_bundled_is_default() {
        // On a release build the default backend is `bundled` (which needs
        // weights), so the "an ollama config still builds an embedder"
        // check uses an explicit backend — the constructor does no network
        // I/O, unlike building the bundled default.
        let cfg = EmbeddingConfig {
            backend: "ollama".to_owned(),
            ..EmbeddingConfig::default()
        };
        let e = cfg
            .build_embedder()
            .await
            .expect("explicit ollama embedder builds");
        assert_eq!(e.model_id(), "bge-m3");
        assert_eq!(e.dimensions(), 1024);
    }

    #[tokio::test]
    async fn embedding_openai_backend_is_unsupported() {
        let cfg = EmbeddingConfig {
            backend: "openai".to_owned(),
            ..EmbeddingConfig::default()
        };
        match cfg.build_embedder().await {
            Err(ConfigError::UnsupportedEmbeddingBackend { backend }) => {
                assert_eq!(backend, "openai");
            },
            Err(other) => panic!("expected UnsupportedEmbeddingBackend, got {other:?}"),
            Ok(_) => panic!("expected UnsupportedEmbeddingBackend, got Ok(embedder)"),
        }
    }

    #[tokio::test]
    async fn embedding_unknown_backend_is_unsupported() {
        let cfg = EmbeddingConfig {
            backend: "weaviate".to_owned(),
            ..EmbeddingConfig::default()
        };
        assert!(matches!(
            cfg.build_embedder().await,
            Err(ConfigError::UnsupportedEmbeddingBackend { .. })
        ));
    }

    #[cfg(not(feature = "local-embedder"))]
    #[tokio::test]
    async fn embedding_bundled_without_feature_is_unavailable() {
        let cfg = EmbeddingConfig {
            backend: "bundled".to_owned(),
            ..EmbeddingConfig::default()
        };
        assert!(matches!(
            cfg.build_embedder().await,
            Err(ConfigError::EmbeddingUnavailable { .. })
        ));
    }

    #[test]
    fn embedding_section_parses_typed_fields() {
        let dir = tempdir().unwrap();
        let body = "embedding:\n  backend: bundled\n  model: bge-m3\n  device: cpu\n  dimensions: 1024\n  model_dir: /opt/models/bge-m3\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.embedding.backend, "bundled");
        assert_eq!(cfg.embedding.device, "cpu");
        assert_eq!(
            cfg.embedding.model_dir.as_deref(),
            Some("/opt/models/bge-m3")
        );
    }

    #[test]
    fn load_rejects_invalid_log_level_explicitly() {
        let dir = tempdir().unwrap();
        let body = "logging:\n  level: chatty\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let err = Config::load(dir.path()).expect_err("must reject");
        match err {
            ConfigError::InvalidLogLevel { value } => assert_eq!(value, "chatty"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn load_rejects_malformed_yaml() {
        let dir = tempdir().unwrap();
        let body = "logging:\n  level: info\n  - bad nesting\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let err = Config::load(dir.path()).expect_err("must reject");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn log_file_rotation_default_is_daily() {
        assert_eq!(LogFileRotation::default(), LogFileRotation::Daily);
        assert!(LogFileRotation::default().is_enabled());
        assert!(!LogFileRotation::Disabled.is_enabled());
    }

    #[test]
    fn load_default_seeds_daily_file_rotation_under_logs_directory() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.file_rotation, LogFileRotation::Daily);
        assert!(cfg.logging.file_path.is_none());
        let resolved = cfg
            .logging
            .resolved_file_path(dir.path())
            .expect("default rotation enables the file sink");
        assert_eq!(resolved, dir.path().join("logs/mwe-mcp.log"));
    }

    #[test]
    fn load_parses_hourly_file_rotation_and_custom_path() {
        let dir = tempdir().unwrap();
        let body =
            "logging:\n  level: info\n  file_rotation: hourly\n  file_path: var/log/custom.log\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.file_rotation, LogFileRotation::Hourly);
        let resolved = cfg.logging.resolved_file_path(dir.path()).unwrap();
        assert_eq!(resolved, dir.path().join("var/log/custom.log"));
    }

    // Unix-only: the assertion fixes a literal `/tmp/...` absolute
    // path, which on Windows is re-rooted to `C:/tmp/...` by the path
    // normaliser — the production behaviour (do-not-re-root under the
    // workdir) is correct on both platforms; only the literal compared
    // here is Unix-flavoured. A future cross-platform variant would
    // parameterise the path via `std::env::temp_dir()` and assert
    // round-trip equality instead.
    #[cfg(unix)]
    #[test]
    fn load_absolute_file_path_is_used_verbatim() {
        let dir = tempdir().unwrap();
        // Absolute targets (an external mount, a shared `/var/log/`
        // sink) must not be re-rooted under the workdir.
        let body =
            "logging:\n  level: info\n  file_rotation: never\n  file_path: /tmp/mwe-mcp-test.log\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.file_rotation, LogFileRotation::Never);
        let resolved = cfg.logging.resolved_file_path(dir.path()).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/mwe-mcp-test.log"));
    }

    #[test]
    fn load_disabled_file_rotation_skips_the_file_sink() {
        let dir = tempdir().unwrap();
        let body = "logging:\n  level: info\n  file_rotation: disabled\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.file_rotation, LogFileRotation::Disabled);
        assert!(cfg.logging.resolved_file_path(dir.path()).is_none());
    }

    #[test]
    fn load_rejects_invalid_file_rotation_explicitly() {
        let dir = tempdir().unwrap();
        let body = "logging:\n  level: info\n  file_rotation: weekly\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let err = Config::load(dir.path()).expect_err("must reject");
        match err {
            ConfigError::InvalidLogFileRotation { value } => assert_eq!(value, "weekly"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn load_accepts_missing_logging_section_as_info() {
        let dir = tempdir().unwrap();
        let body = "embedding:\n  backend: ollama\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.logging.level, LogLevel::Info);
    }

    // ---------- REM scheduling ----------

    #[test]
    fn rem_defaults_to_enabled_interval_daily() {
        let cfg = Config::default();
        assert_eq!(cfg.rem.schedule.mode, RemScheduleMode::Interval);
        assert_eq!(cfg.rem.schedule.interval_secs, 86_400);
        assert_eq!(cfg.rem.schedule.initial_delay_secs, 300);
    }

    #[test]
    fn rem_load_disabled_mode_round_trips() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  schedule:\n    mode: disabled\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.rem.schedule.mode, RemScheduleMode::Disabled);
    }

    #[test]
    fn rem_load_custom_interval_preserves_seconds() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  schedule:\n    mode: interval\n    interval_secs: 3600\n    initial_delay_secs: 10\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.rem.schedule.mode, RemScheduleMode::Interval);
        assert_eq!(cfg.rem.schedule.interval_secs, 3_600);
        assert_eq!(cfg.rem.schedule.initial_delay_secs, 10);
    }

    #[test]
    fn rem_policy_overrides_resolve_else_default() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  policy:\n    auto_promote_min_page_facts: 3\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        let p = cfg.rem.resolved_policy();
        let def = crate::rem::RemPolicy::default();
        assert_eq!(p.auto_promote_min_page_facts, 3);
        // An override left unset keeps the default.
        assert_eq!(p.auto_promote_cap, def.auto_promote_cap);
    }

    #[test]
    fn rem_policy_grace_secs_override_resolves_to_duration() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  policy:\n    briefing_processor_grace_secs: 900\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        let p = cfg.rem.resolved_policy();
        assert_eq!(p.briefing_processor_grace, chrono::Duration::minutes(15));
    }

    #[test]
    fn rem_policy_husk_gc_cap_overrides_else_defaults() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  policy:\n    husk_gc_cap: 0\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.rem.resolved_policy().husk_gc_cap, 0, "0 disables");
        assert_eq!(crate::rem::RemPolicy::default().husk_gc_cap, 4);
    }

    #[test]
    fn rem_policy_grace_defaults_to_fifteen_minutes() {
        let p = crate::rem::RemPolicy::default();
        assert_eq!(p.briefing_processor_grace, chrono::Duration::minutes(15));
        // The YAML override is expressed in seconds; the resolved default
        // must therefore read as 900 s on the wire (the panel placeholder).
        assert_eq!(p.briefing_processor_grace.num_seconds(), 900);
    }

    #[test]
    fn recall_section_overlays_ingest_policy_else_default() {
        let dir = tempdir().unwrap();
        let body = "recall:\n  max_hops: 4\n  due_soon_horizon_hours: 24\n  recall_top_k: 8\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        let p = cfg.recall.resolved_ingest_policy();
        let def = crate::ingest::IngestPolicy::default();
        assert_eq!(p.nav.max_hops, 4);
        assert_eq!(p.due_soon_horizon_hours, 24);
        assert_eq!(p.recall_top_k, 8);
        // Overrides left unset keep the defaults.
        assert_eq!(p.nav.pages_per_hop, def.nav.pages_per_hop);
        assert_eq!(p.nav.char_budget, def.nav.char_budget);
        assert_eq!(p.due_soon_top_k, def.due_soon_top_k);
        assert_eq!(p.recall_fresh_top_k, def.recall_fresh_top_k);
        // The classifier prompt-budget knobs are not recall settings.
        assert_eq!(p.max_recent_messages, def.max_recent_messages);
    }

    #[test]
    fn recall_section_maps_ingest_timezone() {
        // The YAML field flows into IngestPolicy. It wins over any env var, so
        // this assertion is deterministic regardless of MWE_INGEST_TIMEZONE.
        let dir = tempdir().unwrap();
        let body = "recall:\n  ingest_timezone: Europe/Rome\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        let p = cfg.recall.resolved_ingest_policy();
        assert_eq!(p.ingest_timezone.as_deref(), Some("Europe/Rome"));
    }

    #[test]
    fn recall_section_absent_resolves_pure_defaults() {
        let dir = tempdir().unwrap();
        fs::write(Config::path_in(dir.path()), "logging:\n  level: info\n").unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.recall, RecallConfig::default());
        let p = cfg.recall.resolved_ingest_policy();
        let def = crate::ingest::IngestPolicy::default();
        assert_eq!(p.nav.max_hops, def.nav.max_hops);
        assert_eq!(p.due_soon_horizon_hours, def.due_soon_horizon_hours);
    }

    #[test]
    fn rem_load_rejects_unknown_mode_explicitly() {
        let dir = tempdir().unwrap();
        let body = "rem:\n  schedule:\n    mode: cron\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let err = Config::load(dir.path()).expect_err("must reject");
        match err {
            ConfigError::InvalidRemScheduleMode { value } => assert_eq!(value, "cron"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ---------- LLM ----------

    #[test]
    fn llm_function_env_prefix_uses_upper_snake() {
        assert_eq!(LlmFunction::Ingest.env_prefix(), "MWE_LLM_INGEST");
        assert_eq!(
            LlmFunction::RemDedupSemantic.env_prefix(),
            "MWE_LLM_REM_DEDUP_SEMANTIC"
        );
        assert_eq!(LlmFunction::HubWriter.env_prefix(), "MWE_LLM_HUB_WRITER");
    }

    #[test]
    fn parse_hybrid_profile_populates_canonical_slots() {
        // The "hybrid" profile, lifted as-is.
        let dir = tempdir().unwrap();
        let body = "llm:\n  profile: hybrid\n  hub_writer:\n    backend: ollama\n    model: qwen3.5-9b\n  ingest:\n    backend: ollama\n    model: qwen3.5-9b\n  rem_promotions:\n    backend: anthropic\n    model: claude-sonnet-4-6\n    api_key_env: MWE_ANTHROPIC_KEY\n  rem_dedup_semantic:\n    backend: ollama\n    model: phi-3-mini\n  cronista:\n    backend: anthropic\n    model: claude-sonnet-4-6\n    api_key_env: MWE_ANTHROPIC_KEY\n";
        fs::write(Config::path_in(dir.path()), body).unwrap();
        let cfg = Config::load(dir.path()).expect("load");
        assert_eq!(cfg.llm.profile.as_deref(), Some("hybrid"));
        let ingest = cfg.llm.ingest.as_ref().expect("ingest present");
        assert_eq!(ingest.backend, "ollama");
        assert_eq!(ingest.model, "qwen3.5-9b");
        let rem = cfg
            .llm
            .rem_promotions
            .as_ref()
            .expect("rem_promotions present");
        assert_eq!(rem.api_key_env.as_deref(), Some("MWE_ANTHROPIC_KEY"));
    }

    #[test]
    fn llm_slot_lookup_resolves_all_five() {
        let llm = LlmConfig {
            ingest: Some(LlmFunctionConfig {
                backend: "ollama".into(),
                model: "qwen3.5-9b".into(),
                api_key_env: None,
                base_url: None,
                reasoning_effort: None,
                temperature: None,
                max_tokens: None,
            }),
            ..LlmConfig::default()
        };
        assert!(llm.slot(LlmFunction::Ingest).is_some());
        assert!(llm.slot(LlmFunction::HubWriter).is_none());
    }

    #[test]
    fn apply_env_overrides_creates_slot_when_absent_and_replaces_model() {
        let mut llm = LlmConfig::default();
        // Inject overrides via a closure (no env mutation — keeps the
        // crate's #![forbid(unsafe_code)] invariant).
        let envs: std::collections::HashMap<&str, &str> = [
            ("MWE_LLM_INGEST_MODEL", "qwen3.5-9b"),
            ("MWE_LLM_INGEST_BACKEND", "ollama"),
        ]
        .into_iter()
        .collect();
        let n = llm.apply_env_overrides(|k| envs.get(k).map(|s| (*s).to_owned()));
        assert_eq!(n, 2);
        let ingest = llm.ingest.as_ref().expect("created by override");
        assert_eq!(ingest.model, "qwen3.5-9b");
        assert_eq!(ingest.backend, "ollama");
    }

    #[test]
    fn apply_env_overrides_replaces_model_on_existing_slot() {
        let mut llm = LlmConfig {
            ingest: Some(LlmFunctionConfig {
                backend: "ollama".into(),
                model: "yamls-original".into(),
                api_key_env: None,
                base_url: None,
                reasoning_effort: None,
                temperature: None,
                max_tokens: None,
            }),
            ..LlmConfig::default()
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("MWE_LLM_INGEST_MODEL", "env-override")).collect();
        let n = llm.apply_env_overrides(|k| envs.get(k).map(|s| (*s).to_owned()));
        assert_eq!(n, 1);
        assert_eq!(llm.ingest.as_ref().unwrap().model, "env-override");
        // backend untouched.
        assert_eq!(llm.ingest.as_ref().unwrap().backend, "ollama");
    }

    #[test]
    fn apply_env_overrides_returns_zero_when_no_vars_present() {
        let mut llm = LlmConfig::default();
        let n = llm.apply_env_overrides(|_| None);
        assert_eq!(n, 0);
        assert!(llm.ingest.is_none());
    }

    #[test]
    fn build_backend_rejects_unknown_backend() {
        // `openai` is the one cloud backend that does not have an
        // adapter yet — the perfect "loaded but unsupported" probe.
        let cfg = LlmFunctionConfig {
            backend: "openai".into(),
            model: "gpt-4o".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let result = cfg.build_backend(LlmFunction::Ingest);
        match result {
            Err(ConfigError::UnsupportedLlmBackend { function, backend }) => {
                assert_eq!(function, "ingest");
                assert_eq!(backend, "openai");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected unsupported-backend error"),
        }
    }

    #[test]
    fn build_backend_constructs_ollama_with_custom_base_url() {
        let cfg = LlmFunctionConfig {
            backend: "ollama".into(),
            model: "qwen3.5-9b".into(),
            api_key_env: None,
            base_url: Some("http://elsewhere:9999".into()),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let backend = cfg.build_backend(LlmFunction::Ingest).expect("build");
        assert_eq!(backend.model_id(), "qwen3.5-9b");
    }

    /// Anthropic slots materialise when `api_key_env` is set and the
    /// env-var resolves to a non-empty string. We drive the lookup
    /// via the injected `build_backend_with_env` closure so the test
    /// does not touch the process environment (keeps the
    /// `#![forbid(unsafe_code)]` invariant intact).
    #[test]
    fn build_backend_constructs_anthropic_when_api_key_env_resolves() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("ANTHROPIC_API_KEY", "sk-ant-fake")).collect();
        let backend = cfg
            .build_backend_with_env(LlmFunction::Cronista, |k| {
                envs.get(k).map(|s| (*s).to_owned())
            })
            .expect("build");
        assert_eq!(backend.model_id(), "claude-opus-4-7");
    }

    #[test]
    fn build_backend_anthropic_with_custom_base_url() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: Some("https://example.test/anthropic".into()),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("ANTHROPIC_API_KEY", "sk-ant-fake")).collect();
        let backend = cfg
            .build_backend_with_env(LlmFunction::RemPromotions, |k| {
                envs.get(k).map(|s| (*s).to_owned())
            })
            .expect("build");
        assert_eq!(backend.model_id(), "claude-sonnet-4-6");
    }

    /// An anthropic slot whose `api_key_env` is the `claude-code` sentinel
    /// takes the login-store path; with no login store installed it reports
    /// the not-logged-in condition (confirming the sentinel is recognized,
    /// not treated as a real env-var name).
    #[test]
    fn build_backend_anthropic_claude_code_login_without_store_errors() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            api_key_env: Some(crate::oauth::CLAUDE_CODE_LOGIN.into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        // `Box<dyn LlmBackend>` is not `Debug`, so `let...else` instead of `expect_err`.
        let Err(err) = cfg.build_backend_with_env(LlmFunction::Ingest, |_| None) else {
            panic!("expected an error with no login store installed");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("claude-code") || msg.contains("log in"),
            "unexpected error: {msg}"
        );
    }

    /// Missing `api_key_env` is a config-time error — not a runtime
    /// auth failure on the first request.
    #[test]
    fn build_backend_anthropic_rejects_when_api_key_env_missing() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let result = cfg.build_backend_with_env(LlmFunction::Cronista, |_| None);
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "cronista");
                assert!(detail.contains("api_key_env"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// `api_key_env` names an env-var that is unset in the process
    /// environment — surface the env-var name in the error so the
    /// operator can find it in `mwe-mcp.env`.
    #[test]
    fn build_backend_anthropic_rejects_when_env_var_unset() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let result = cfg.build_backend_with_env(LlmFunction::Cronista, |_| None);
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "cronista");
                assert!(detail.contains("ANTHROPIC_API_KEY"), "{detail}");
                assert!(detail.contains("mwe-mcp.env"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// Empty / whitespace-only env-var values are treated as unset —
    /// otherwise an operator who accidentally wrote `ANTHROPIC_API_KEY=`
    /// in `mwe-mcp.env` would see an opaque 401 instead of a
    /// config-time error.
    #[test]
    fn build_backend_anthropic_rejects_empty_env_var_value() {
        let cfg = LlmFunctionConfig {
            backend: "anthropic".into(),
            model: "claude-opus-4-7".into(),
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("ANTHROPIC_API_KEY", "  ")).collect();
        let result = cfg.build_backend_with_env(LlmFunction::Cronista, |k| {
            envs.get(k).map(|s| (*s).to_owned())
        });
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "cronista");
                assert!(detail.contains("ANTHROPIC_API_KEY"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// Smoke test the profile presets: every Anthropic slot in
    /// `hybrid` and `all-api` builds cleanly when the named env-var
    /// resolves. Regression guard against drift between
    /// `LlmProfile::build` (which names env-vars) and
    /// `build_backend_with_env` (which reads them).
    #[test]
    fn llm_profile_hybrid_materialises_anthropic_slots_when_env_set() {
        let cfg = LlmProfile::Hybrid.build();
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("ANTHROPIC_API_KEY", "sk-ant-fake")).collect();
        for func in [LlmFunction::RemPromotions, LlmFunction::Cronista] {
            let slot = cfg.slot(func).unwrap_or_else(|| panic!("{func:?} missing"));
            assert_eq!(slot.backend, "anthropic");
            slot.build_backend_with_env(func, |k| envs.get(k).map(|s| (*s).to_owned()))
                .unwrap_or_else(|e| panic!("{func:?} build failed: {e}"));
        }
        // The non-anthropic slots stay Ollama and still build without
        // any env injection.
        for func in [
            LlmFunction::HubWriter,
            LlmFunction::Ingest,
            LlmFunction::RemDedupSemantic,
        ] {
            let slot = cfg.slot(func).unwrap_or_else(|| panic!("{func:?} missing"));
            assert_eq!(slot.backend, "ollama");
            slot.build_backend_with_env(func, |_| None)
                .unwrap_or_else(|e| panic!("{func:?} build failed: {e}"));
        }
    }

    #[test]
    fn llm_profile_all_api_materialises_every_slot_on_anthropic() {
        let cfg = LlmProfile::AllApi.build();
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("ANTHROPIC_API_KEY", "sk-ant-fake")).collect();
        for func in [
            LlmFunction::HubWriter,
            LlmFunction::Ingest,
            LlmFunction::RemPromotions,
            LlmFunction::RemDedupSemantic,
            LlmFunction::Cronista,
        ] {
            let slot = cfg.slot(func).unwrap_or_else(|| panic!("{func:?} missing"));
            assert_eq!(slot.backend, "anthropic");
            slot.build_backend_with_env(func, |k| envs.get(k).map(|s| (*s).to_owned()))
                .unwrap_or_else(|e| panic!("{func:?} build failed: {e}"));
        }
    }

    // ---------- gemini backend construction ----------

    /// Gemini slots materialise via the same `resolve_cloud_api_key`
    /// pathway as Anthropic, parameterised by the YAML-named env-var.
    /// Drives the lookup via the injected closure so the test does not
    /// touch the process environment.
    #[test]
    fn build_backend_constructs_gemini_when_api_key_env_resolves() {
        let cfg = LlmFunctionConfig {
            backend: "gemini".into(),
            model: "gemini-3-flash-preview".into(),
            api_key_env: Some("GEMINI_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("GEMINI_API_KEY", "AIza-fake")).collect();
        let backend = cfg
            .build_backend_with_env(LlmFunction::Ingest, |k| {
                envs.get(k).map(|s| (*s).to_owned())
            })
            .expect("build");
        assert_eq!(backend.model_id(), "gemini-3-flash-preview");
    }

    #[test]
    fn build_backend_gemini_with_custom_base_url() {
        let cfg = LlmFunctionConfig {
            backend: "gemini".into(),
            model: "gemini-3.1-pro-preview".into(),
            api_key_env: Some("GEMINI_API_KEY".into()),
            base_url: Some("https://example.test/gemini".into()),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("GEMINI_API_KEY", "AIza-fake")).collect();
        let backend = cfg
            .build_backend_with_env(LlmFunction::Cronista, |k| {
                envs.get(k).map(|s| (*s).to_owned())
            })
            .expect("build");
        assert_eq!(backend.model_id(), "gemini-3.1-pro-preview");
    }

    /// Missing `api_key_env` for `gemini` follows the same
    /// config-time-error rule as `anthropic` — never deferred to the
    /// first request. The error message names the provider so the
    /// operator sees which adapter complained.
    #[test]
    fn build_backend_gemini_rejects_when_api_key_env_missing() {
        let cfg = LlmFunctionConfig {
            backend: "gemini".into(),
            model: "gemini-3-flash-preview".into(),
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let result = cfg.build_backend_with_env(LlmFunction::Ingest, |_| None);
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "ingest");
                assert!(detail.contains("api_key_env"), "{detail}");
                assert!(detail.contains("gemini"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// `api_key_env` names an env-var that is unset — surface the
    /// env-var name in the error so the operator can find it in
    /// `mwe-mcp.env`.
    #[test]
    fn build_backend_gemini_rejects_when_env_var_unset() {
        let cfg = LlmFunctionConfig {
            backend: "gemini".into(),
            model: "gemini-3-flash-preview".into(),
            api_key_env: Some("GEMINI_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let result = cfg.build_backend_with_env(LlmFunction::Ingest, |_| None);
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "ingest");
                assert!(detail.contains("GEMINI_API_KEY"), "{detail}");
                assert!(detail.contains("mwe-mcp.env"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// Whitespace-only env-var values are treated as unset — same
    /// invariant as Anthropic, prevents an opaque 401 from a
    /// `GEMINI_API_KEY=` line in `mwe-mcp.env`.
    #[test]
    fn build_backend_gemini_rejects_empty_env_var_value() {
        let cfg = LlmFunctionConfig {
            backend: "gemini".into(),
            model: "gemini-3-flash-preview".into(),
            api_key_env: Some("GEMINI_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let envs: std::collections::HashMap<&str, &str> =
            std::iter::once(("GEMINI_API_KEY", "   ")).collect();
        let result = cfg.build_backend_with_env(LlmFunction::Ingest, |k| {
            envs.get(k).map(|s| (*s).to_owned())
        });
        match result {
            Err(ConfigError::MissingApiKeyEnv { function, detail }) => {
                assert_eq!(function, "ingest");
                assert!(detail.contains("GEMINI_API_KEY"), "{detail}");
            },
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected MissingApiKeyEnv error"),
        }
    }

    /// YAML round-trip for a gemini slot — guards against accidental
    /// renaming of the `backend` discriminator.
    #[test]
    fn gemini_slot_round_trips_through_yaml() {
        let body = "llm:\n  ingest:\n    backend: gemini\n    model: gemini-3-flash-preview\n    api_key_env: GEMINI_API_KEY\n";
        let cfg = Config::parse(&PathBuf::from("test"), body).expect("parse");
        let ingest = cfg.llm.ingest.as_ref().expect("ingest slot");
        assert_eq!(ingest.backend, "gemini");
        assert_eq!(ingest.model, "gemini-3-flash-preview");
        assert_eq!(ingest.api_key_env.as_deref(), Some("GEMINI_API_KEY"));
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        assert!(yaml.contains("backend: gemini"), "{yaml}");
        assert!(yaml.contains("api_key_env: GEMINI_API_KEY"), "{yaml}");
    }

    // ---------- temperature / max_tokens ----------

    #[test]
    fn temperature_and_max_tokens_round_trip_through_yaml() {
        let body = "llm:\n  ingest:\n    backend: anthropic\n    model: claude-sonnet-4-6\n    api_key_env: ANTHROPIC_API_KEY\n    temperature: 0.4\n    max_tokens: 1200\n";
        let cfg = Config::parse(&PathBuf::from("test"), body).expect("parse");
        let ingest = cfg.llm.ingest.as_ref().expect("ingest slot");
        assert_eq!(ingest.temperature, Some(0.4));
        assert_eq!(ingest.max_tokens, Some(1200));
        // Round-trip: serializing back must preserve the keys (so a
        // dashboard save → load → save cycle is stable).
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        assert!(yaml.contains("temperature: 0.4"), "{yaml}");
        assert!(yaml.contains("max_tokens: 1200"), "{yaml}");
    }

    #[test]
    fn temperature_and_max_tokens_omitted_when_unset() {
        // The `skip_serializing_if = "Option::is_none"` keeps the YAML
        // small for operators who never set these knobs; otherwise
        // every slot would gain two noisy `~` lines on every save.
        let cfg = LlmFunctionConfig {
            backend: "ollama".into(),
            model: "qwen3.5-9b".into(),
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        assert!(!yaml.contains("temperature"), "{yaml}");
        assert!(!yaml.contains("max_tokens"), "{yaml}");
    }

    #[test]
    fn apply_env_overrides_parses_temperature_and_max_tokens() {
        let mut llm = LlmConfig::default();
        let envs: std::collections::HashMap<&str, &str> = [
            ("MWE_LLM_INGEST_BACKEND", "ollama"),
            ("MWE_LLM_INGEST_MODEL", "qwen3.5-9b"),
            ("MWE_LLM_INGEST_TEMPERATURE", "0.7"),
            ("MWE_LLM_INGEST_MAX_TOKENS", "2048"),
        ]
        .into_iter()
        .collect();
        let n = llm.apply_env_overrides(|k| envs.get(k).map(|s| (*s).to_owned()));
        assert_eq!(n, 4);
        let ingest = llm.ingest.as_ref().expect("created");
        assert_eq!(ingest.temperature, Some(0.7));
        assert_eq!(ingest.max_tokens, Some(2048));
    }

    #[test]
    fn apply_env_overrides_ignores_malformed_numeric_values() {
        // Operator typo on the env var should not torpedo the whole
        // config load — log + skip, the YAML default remains.
        let mut llm = LlmConfig {
            ingest: Some(
                ollama("qwen3.5-9b")
                    .with_temperature(0.2)
                    .with_max_tokens(800),
            ),
            ..LlmConfig::default()
        };
        let envs: std::collections::HashMap<&str, &str> = [
            ("MWE_LLM_INGEST_TEMPERATURE", "not-a-number"),
            ("MWE_LLM_INGEST_MAX_TOKENS", "fortytwo"),
        ]
        .into_iter()
        .collect();
        let n = llm.apply_env_overrides(|k| envs.get(k).map(|s| (*s).to_owned()));
        assert_eq!(n, 0, "malformed values must not count as applied");
        let ingest = llm.ingest.as_ref().expect("preserved");
        assert_eq!(ingest.temperature, Some(0.2));
        assert_eq!(ingest.max_tokens, Some(800));
    }

    #[test]
    fn apply_defaults_to_completion_fills_only_unset_fields() {
        let slot = ollama("qwen3.5-9b")
            .with_temperature(0.5)
            .with_max_tokens(400);
        // Caller pinned temperature explicitly — defaults must NOT
        // override (this is the contract that lets `ingest` keep its
        // hardcoded 0.1 even when the operator picks a chattier
        // default for the slot).
        let mut req = crate::llm::CompletionRequest::new("hello").with_temperature(0.1);
        slot.apply_defaults_to_completion(&mut req);
        assert_eq!(req.temperature, Some(0.1));
        assert_eq!(req.max_tokens, Some(400));
    }

    #[test]
    fn apply_defaults_to_completion_noop_when_slot_has_no_defaults() {
        let slot = ollama("qwen3.5-9b");
        let mut req = crate::llm::CompletionRequest::new("hello");
        slot.apply_defaults_to_completion(&mut req);
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
    }

    #[test]
    fn apply_defaults_to_chat_fills_only_unset_fields() {
        let slot = anthropic("claude-haiku-4-5-20251001", "ANTHROPIC_API_KEY")
            .with_temperature(0.6)
            .with_max_tokens(2048);
        let mut req = crate::llm::ChatRequest::new(vec![crate::llm::ChatMessage::user("hi")]);
        slot.apply_defaults_to_chat(&mut req);
        assert_eq!(req.temperature, Some(0.6));
        assert_eq!(req.max_tokens, Some(2048));
    }
}
