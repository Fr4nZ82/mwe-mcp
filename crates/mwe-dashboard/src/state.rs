// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared dashboard state — the bag of handles every handler needs.
//!
//! Cheap to clone (everything inside is `Arc` or `Pool`-like), so we
//! keep a single instance in `Router::with_state` and let Axum hand it
//! out by value to each request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use mwe_core::config::{LlmConfig, LlmFunction, LlmFunctionConfig, RecallConfig};
use mwe_core::delegations::DelegationCache;
use mwe_core::embedder::Embedder;
use mwe_core::jwt::{BlacklistCache, TokenSecret};
use mwe_core::llm::LlmBackend;
use mwe_core::wiki::WikiTree;
use sqlx::SqlitePool;

/// Tunable knobs for the dashboard runtime.
///
/// Defaults are baked in so identity-console code paths can build a
/// [`DashboardState`] without reaching for `mwe-mcp.config.yaml`;
/// the loader will later wire these to override from disk.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Minimum password length accepted by the setup wizard,
    /// `/dashboard/accept-invite`, and the self-service password
    /// change. Pinned to 12 by the [memory model](../../../docs/concepts/memory-model.md).
    pub min_password_len: usize,

    /// TTL for fresh `user_invitations` rows. Per the
    /// engine DB and migrations
    /// the default is 24h; the dashboard form does not expose this knob
    /// (only the CLI `mwe-mcp admin-reset` does), so it lives here.
    pub invitation_ttl_hours: i64,

    /// TTL for self-service password-reset links (`password_resets`,
    /// roadmap 28). Much shorter than an invitation — a recovery link is
    /// acted on immediately. Default 30 minutes.
    pub reset_ttl_minutes: i64,

    /// Sliding TTL of the dashboard session cookie. The canonical
    /// value is 60 minutes; the middleware re-issues the cookie on
    /// every authenticated request with a fresh `exp`, and a small
    /// keepalive ping (`/dashboard/session/keepalive`) refreshes it on
    /// user activity so a long form (the welcome primer) never lapses
    /// mid-fill. See the
    /// JWT & session model.
    pub session_ttl_minutes: i64,

    /// When set, the session cookie is emitted with the `Secure`
    /// attribute. Default is `false` so `mwe-mcp serve` on `127.0.0.1`
    /// works out of the box; production deployments behind a TLS proxy
    /// flip this on via the operator-facing config.
    pub cookie_secure: bool,

    /// The admin ACL-reveal switch is taken away from the dashboard
    /// admin (`mwe-mcp.config.yaml > instance.admin_reveal_locked`).
    ///
    /// Enforced in one place — [`crate::reveal::active`] — so every
    /// reveal-aware surface inherits it, and the toggle route refuses
    /// rather than silently setting a cookie nothing honours. There is
    /// no dashboard editor for it on purpose: this is the machine
    /// operator's switch, not the panel admin's.
    pub admin_reveal_locked: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            min_password_len: 12,
            invitation_ttl_hours: 24,
            reset_ttl_minutes: 30,
            session_ttl_minutes: 60,
            cookie_secure: false,
            admin_reveal_locked: false,
        }
    }
}

/// Long-lived handles the memory-explorer + chat routes need on
/// top of the identity-console floor.
///
/// Wrapped in [`DashboardState::memory`] so the identity-console handlers
/// (which only touch `pool` / `secret` / `blacklist`) keep building without
/// requiring the operator to wire an embedder; the new routes return
/// `503 Service Unavailable` when this bundle is missing.
#[derive(Clone)]
pub struct MemoryHandles {
    /// Filesystem-side handle on `<workdir>/wikis/` — read-only access
    /// for the wiki view + chat snippet rendering.
    pub tree: WikiTree,
    /// Embedder shared with the MCP transport so `wiki_ingest_message`
    /// from chat uses the same vector space as the rest of the system.
    pub embedder: Arc<dyn Embedder>,
    /// LLM function configuration — shared behind an `Arc<RwLock>`
    /// so the admin LLM-config editor can swap the slots
    /// in place and every cloned [`MemoryHandles`] in the running
    /// process sees the new values on the next read.
    ///
    /// Each request that calls [`Self::backend_for`] / [`Self::defaults_for`]
    /// takes a short read-lock; the dashboard save handler takes a
    /// write-lock for the swap. Lock contention is a non-issue at
    /// this scale (writes are operator-initiated, reads cost
    /// microseconds).
    pub llm_config: Arc<RwLock<LlmConfig>>,

    /// In-memory override of API key env-var values, populated by the
    /// dashboard admin LLM-config editor. When a key is
    /// present here, [`Self::backend_for`] reads it instead of the
    /// process env — closing the hot-reload gap that the
    /// `forbid_unsafe_code` crate policy would otherwise force
    /// (`std::env::set_var` is unsafe on unix).
    ///
    /// Keys absent from this map fall through to `std::env::var`,
    /// preserving the [`env_loader::load_workdir_env`] precedence
    /// for the boot-time secrets. Always empty at process start; the
    /// dashboard set-API-key handler is the only writer.
    pub api_key_overrides: Arc<RwLock<HashMap<String, String>>>,
    /// Optional per-slot LLM backend overrides. When a slot is set
    /// here, [`Self::backend_for`] returns it directly instead of
    /// constructing a fresh backend from [`Self::llm_config`]; this
    /// is the seam that lets dashboard integration tests plant a
    /// [`mwe_core::llm::FakeLlmBackend`] without spinning up Ollama.
    /// Production code never populates this field — `bootstrap_state`
    /// leaves it empty and the live `LlmConfig` does the work.
    ///
    /// This is the seam that enables the e2e ingest test with a fake
    /// LLM backend (see the dashboard).
    pub llm_overrides: LlmBackendOverrides,
    /// Workdir root, exposed for handlers that need to construct
    /// `<workdir>/...` paths (today only used by the chat handler's
    /// audit log).
    pub workdir: PathBuf,
}

/// An in-flight "Log in with Claude Code" attempt.
///
/// Held in a single slot on [`DashboardState`] between the `start`
/// redirect and the `callback`/`paste` completion (one operator login at
/// a time; a new attempt overwrites any abandoned one).
///
/// The token endpoint requires the same `redirect_uri` at code exchange
/// that built the authorize URL, so it is recorded here alongside the
/// PKCE `verifier` and the CSRF `state`. Test/personal use only — see
/// [`mwe_core::oauth`].
#[derive(Debug, Clone)]
pub struct PendingClaudeLogin {
    /// PKCE secret replayed at code exchange.
    pub verifier: String,
    /// CSRF guard echoed back by the provider and re-checked on completion.
    pub state: String,
    /// The exact `redirect_uri` used for the authorize URL (loopback
    /// callback for the seamless path, out-of-band for the paste path).
    pub redirect_uri: String,
}

/// Per-slot overrides for the canonical LLM functions. Empty in
/// production; tests populate the relevant slots with a fake backend.
///
/// Each field is `Option<Arc<dyn LlmBackend>>` so cloning a
/// [`MemoryHandles`] is cheap and the override outlives a single
/// request (essential for an agentic loop that re-invokes the same
/// backend over many iterations).
#[derive(Clone, Default)]
pub struct LlmBackendOverrides {
    pub hub_writer: Option<Arc<dyn LlmBackend>>,
    pub ingest: Option<Arc<dyn LlmBackend>>,
    pub operator_chat: Option<Arc<dyn LlmBackend>>,
    pub rem_promotions: Option<Arc<dyn LlmBackend>>,
    pub rem_dedup_semantic: Option<Arc<dyn LlmBackend>>,
    pub cronista: Option<Arc<dyn LlmBackend>>,
    pub navigator: Option<Arc<dyn LlmBackend>>,
}

#[allow(deprecated, reason = "Cronista arm kept for YAML backward compat")]
impl LlmBackendOverrides {
    /// Look up the override for `slot`, or `None` if production
    /// should fall back to [`LlmConfig`].
    #[must_use]
    pub fn get(&self, slot: LlmFunction) -> Option<Arc<dyn LlmBackend>> {
        match slot {
            LlmFunction::HubWriter => self.hub_writer.clone(),
            LlmFunction::Ingest => self.ingest.clone(),
            LlmFunction::OperatorChat => self.operator_chat.clone(),
            LlmFunction::RemPromotions => self.rem_promotions.clone(),
            LlmFunction::RemDedupSemantic => self.rem_dedup_semantic.clone(),
            LlmFunction::Cronista => self.cronista.clone(),
            LlmFunction::Navigator => self.navigator.clone(),
        }
    }

    /// Builder: plant a backend for `slot`. Used by test fixtures.
    #[must_use]
    pub fn with(mut self, slot: LlmFunction, backend: Arc<dyn LlmBackend>) -> Self {
        match slot {
            LlmFunction::HubWriter => self.hub_writer = Some(backend),
            LlmFunction::Ingest => self.ingest = Some(backend),
            LlmFunction::OperatorChat => self.operator_chat = Some(backend),
            LlmFunction::RemPromotions => self.rem_promotions = Some(backend),
            LlmFunction::RemDedupSemantic => self.rem_dedup_semantic = Some(backend),
            LlmFunction::Cronista => self.cronista = Some(backend),
            LlmFunction::Navigator => self.navigator = Some(backend),
        }
        self
    }
}

/// Why [`MemoryHandles::backend_for`] could not produce a backend.
///
/// Surfaced as `DashboardError::Validation` (slot missing — operator
/// fix) or `DashboardError::Internal` (build failure — programmer fix)
/// by the chat handlers; the variants stay decoupled here so this
/// crate's `state` module does not depend on `error`.
#[derive(Debug, thiserror::Error)]
pub enum BackendForError {
    /// The override is absent AND `LlmConfig` has no entry for `slot`.
    /// Operator must wire the slot in `mwe-mcp.config.yaml`.
    #[error("LLM slot `{0}` is not configured")]
    SlotMissing(&'static str),
    /// `LlmConfig::build_backend` returned an error. Surface to the
    /// operator as an internal failure (the YAML named a backend we
    /// cannot actually construct).
    #[error("building LLM backend for slot `{slot}`: {detail}")]
    BuildFailed { slot: &'static str, detail: String },
}

impl MemoryHandles {
    /// Resolve a per-slot LLM backend, preferring [`Self::llm_overrides`]
    /// when populated, falling back to a fresh build from
    /// [`Self::llm_config`].
    ///
    /// The override path is what dashboard integration tests use; the
    /// build path is what `mwe-mcp serve` exercises in production.
    /// Both arms return the same `Arc<dyn LlmBackend>` so callers do
    /// not branch on the source.
    ///
    /// # Errors
    /// See [`BackendForError`].
    pub fn backend_for(&self, slot: LlmFunction) -> Result<Arc<dyn LlmBackend>, BackendForError> {
        if let Some(injected) = self.llm_overrides.get(slot) {
            return Ok(injected);
        }
        // Hold the read-lock only long enough to clone the slot
        // config; the backend constructor itself does not need the
        // lock (and would deadlock on a future write attempt by the
        // same task if it did).
        let slot_cfg = {
            let guard = self.llm_config.read().expect("llm_config rwlock poisoned");
            guard
                .slot(slot)
                .cloned()
                .ok_or_else(|| BackendForError::SlotMissing(slot.yaml_key()))?
        };
        let overrides = Arc::clone(&self.api_key_overrides);
        slot_cfg
            .build_backend_with_env(slot, move |name| {
                // Dashboard-side hot-reload precedence: an API key
                // set from /admin/api-keys/:name takes effect on the
                // next request even though the process env is left
                // untouched (the crate cannot `std::env::set_var`
                // under `forbid_unsafe_code`). When absent, fall
                // back to the regular shell-wins lookup so the boot
                // env_loader values keep working.
                overrides
                    .read()
                    .expect("api_key_overrides rwlock poisoned")
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
            })
            .map(Arc::from)
            .map_err(|e| BackendForError::BuildFailed {
                slot: slot.yaml_key(),
                detail: e.to_string(),
            })
    }

    /// Per-slot default knobs (`temperature`, `max_tokens`, …) pulled
    /// from `mwe-mcp.config.yaml`. The agentic chat loop pipes these
    /// through [`LlmFunctionConfig::apply_defaults_to_chat`] on every
    /// `ChatRequest` so an operator editing the slot from the admin
    /// page changes the chat's sampling parameters without code
    /// changes.
    ///
    /// Returns `None` when the slot is not present in `llm_config`
    /// (the `LlmConfig::default()` path that integration tests use
    /// with planted `llm_overrides`). The caller must treat `None` as
    /// "no defaults" — the request fields stay whatever the caller
    /// pinned, falling back to the backend's own defaults.
    #[must_use]
    pub fn defaults_for(&self, slot: LlmFunction) -> Option<LlmFunctionConfig> {
        self.llm_config
            .read()
            .expect("llm_config rwlock poisoned")
            .slot(slot)
            .cloned()
    }

    /// Resolve the backend for the dashboard's operational agentic chat:
    /// prefer the dedicated [`LlmFunction::OperatorChat`] slot, falling
    /// back to [`LlmFunction::HubWriter`] when it is unconfigured. The
    /// chat is a distinct workload from the `regenerate_index` sub-job
    /// that also rides `hub_writer` (interactive, multi-step
    /// function-calling, faithful fact-id handling), so an operator can
    /// point it at a stronger model without inflating the index-regen
    /// cost — but the fallback keeps deployments that never set
    /// `operator_chat` working exactly as before, with no new YAML key.
    ///
    /// Only [`BackendForError::SlotMissing`] on the dedicated slot falls
    /// through: a `BuildFailed` is a real misconfiguration and is
    /// surfaced, not masked by silently reaching for `hub_writer`.
    ///
    /// # Errors
    /// See [`BackendForError`]. `SlotMissing` here means **both** slots
    /// are unconfigured.
    pub fn backend_for_chat(&self) -> Result<Arc<dyn LlmBackend>, BackendForError> {
        match self.backend_for(LlmFunction::OperatorChat) {
            Err(BackendForError::SlotMissing(_)) => self.backend_for(LlmFunction::HubWriter),
            other => other,
        }
    }

    /// Per-slot default knobs for the operational chat, mirroring
    /// [`Self::backend_for_chat`]'s fallback: the `operator_chat` slot's
    /// defaults when configured, otherwise `hub_writer`'s.
    #[must_use]
    pub fn chat_defaults(&self) -> Option<LlmFunctionConfig> {
        self.defaults_for(LlmFunction::OperatorChat)
            .or_else(|| self.defaults_for(LlmFunction::HubWriter))
    }

    /// Replace the live `LlmConfig` in place. Used by the admin
    /// LLM-config editor right after the YAML save: the
    /// previous handle was cloned out into per-request `Arc`s, those
    /// continue to work with their captured config (no live LLM call
    /// rips out the rug mid-iteration), and every subsequent
    /// [`Self::backend_for`] / [`Self::defaults_for`] picks up the
    /// new slots.
    pub fn replace_llm_config(&self, fresh: LlmConfig) {
        *self.llm_config.write().expect("llm_config rwlock poisoned") = fresh;
    }

    /// Upsert an API key value into the in-memory override map. The
    /// next [`Self::backend_for`] for any slot whose
    /// `api_key_env` matches `name` will read the new value
    /// (overriding `std::env::var`).
    pub fn set_api_key_override(&self, name: impl Into<String>, value: impl Into<String>) {
        self.api_key_overrides
            .write()
            .expect("api_key_overrides rwlock poisoned")
            .insert(name.into(), value.into());
    }

    /// Snapshot the API key override map. Returned as an owned
    /// `HashMap<String, String>` so the caller (today the dashboard
    /// fingerprint renderer) does not hold the lock while iterating.
    #[must_use]
    pub fn api_key_overrides_snapshot(&self) -> HashMap<String, String> {
        self.api_key_overrides
            .read()
            .expect("api_key_overrides rwlock poisoned")
            .clone()
    }

    /// Snapshot the full `LlmConfig`. Returned as an owned value so
    /// the caller does not hold the read-lock across an `await` (the
    /// `RwLockReadGuard` from `std::sync` is not `Send`).
    #[must_use]
    pub fn llm_config_snapshot(&self) -> LlmConfig {
        self.llm_config
            .read()
            .expect("llm_config rwlock poisoned")
            .clone()
    }
}

impl std::fmt::Debug for MemoryHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryHandles")
            .field("workdir", &self.workdir)
            .field("embedder", &self.embedder.model_id())
            .finish_non_exhaustive()
    }
}

/// Handles passed to every dashboard handler. Cloning is cheap:
/// `SqlitePool` is itself an `Arc`, `TokenSecret` already wraps an
/// `Arc<Vec<u8>>`, and the blacklist cache is shared behind an `Arc`.
#[derive(Clone)]
pub struct DashboardState {
    /// Connection pool to `engine.db`. Same pool the MCP transport
    /// uses — no separate connection per subsystem.
    pub pool: SqlitePool,

    /// HMAC secret backing every JWT this deployment signs. The
    /// dashboard re-uses it as-is for the session cookie.
    pub secret: TokenSecret,

    /// Shared in-memory mirror of `token_blacklist`. Critical that the
    /// dashboard and the MCP transport share this single instance: a
    /// revoke from `/dashboard/tokens/revoke` must take effect at the
    /// MCP middleware on the next call without re-reading the DB.
    pub blacklist: Arc<BlacklistCache>,

    /// Shared in-memory mirror of `consumer_delegations`. Same
    /// invariant as [`Self::blacklist`]: the dashboard write paths
    /// under `/dashboard/tokens/...` call
    /// [`mwe_core::delegations::DelegationCache::refresh`] right after
    /// every upsert/delete so the MCP middleware resolves
    /// `X-MWE-Act-As` against the fresh row on the next tool call.
    pub delegations: Arc<DelegationCache>,

    /// Tunable knobs (password length, session TTL, ...). Cloned by
    /// value because the struct is tiny.
    pub config: DashboardConfig,

    /// Memory-explorer + chat handles. `None` for the
    /// identity-console build; populated by the wiring in
    /// `mwe-mcp serve`.
    pub memory: Option<MemoryHandles>,

    /// Serialization gate for the admin-triggered Dream console
    /// (`POST /dashboard/dream/{light,compile,full}`). `try_lock`
    /// ensures two concurrent manual cycles can never overlap. Sharing this gate
    /// with the interval REM scheduler — so a manual trigger also
    /// excludes a *scheduled* cycle — is a tracked follow-up
    /// (`rem-manual-trigger-scheduler-gate`); during
    /// a test session the scheduler is dormant (24h interval), so the
    /// manual-only gate is sufficient today.
    pub rem_gate: Arc<tokio::sync::Mutex<()>>,

    /// REM full-cycle policy (including the auto-promote thresholds),
    /// resolved from `mwe-mcp.config.yaml > rem.policy:` at startup.
    /// Shared behind `Arc<RwLock>` with the server's REM scheduler
    /// (which reads it fresh at each cycle start) so a save from the
    /// REM settings editor reaches the next scheduled cycle **and** the
    /// Dream console's `run_full` without a restart. Defaults to
    /// `RemPolicy::default()` on the identity-only / test builds.
    pub rem_policy: Arc<RwLock<mwe_core::rem::RemPolicy>>,

    /// Operator recall settings (`recall:` config section). Shared
    /// behind `Arc<RwLock>` with the MCP transport's `McpState` so a
    /// save from the recall-settings editor reaches the next ingest
    /// turn on **both** transports without a restart. Defaults to
    /// `RecallConfig::default()` on the identity-only / test builds.
    pub recall: Arc<RwLock<RecallConfig>>,

    /// Single-slot holder for an in-flight "Log in with Claude Code"
    /// OAuth attempt (shared across the `start` → `callback`/`paste`
    /// requests). Empty until the operator starts a login from the
    /// LLM-config page. Test/personal use only — see [`PendingClaudeLogin`].
    pub claude_login: Arc<Mutex<Option<PendingClaudeLogin>>>,

    /// Live status of the admin Dream console's **background** runs. The
    /// JSON path of `POST /dashboard/dream/{compile,full}` spawns the dream
    /// on a task (guarded by [`Self::rem_gate`]) and returns immediately;
    /// this slot lets `GET /dashboard/dream/status` report "still running"
    /// and the last outcome so the topnav indicator can animate and then
    /// show the result. The synchronous no-JS path does not touch it.
    pub dream_status: Arc<Mutex<DreamStatus>>,

    /// Automatic-snapshot schedule (`backup:` config section, resolved).
    /// Shared behind `Arc<RwLock>` with the server's backup scheduler
    /// (which reads it fresh at each due-check) so a save from the
    /// Backup console applies without a restart. `None` on the
    /// identity-only / test builds (the scheduler idles).
    pub backup_schedule: Arc<RwLock<Option<mwe_core::backup::BackupSchedule>>>,

    /// Handle to request a process restart from the Backup console
    /// (staged recovery needs a boot to apply). `None` outside the full
    /// `mwe-mcp serve` build — the restart button is then hidden.
    pub restart: Option<RestartHandle>,
}

/// Restart plumbing `mwe-mcp serve` wires into the dashboard.
///
/// The graceful-shutdown broadcast plus a flag the main loop checks
/// after the joins to exit with the deliberate non-zero restart code
/// (the provisioned systemd unit is `Restart=on-failure`, so a clean
/// exit would stay down).
#[derive(Clone)]
pub struct RestartHandle {
    /// The serve loop's shutdown fan-out — sending starts the same
    /// graceful teardown as ctrl-c.
    pub shutdown: tokio::sync::broadcast::Sender<()>,
    /// Set before sending so the main loop knows to exit non-zero.
    pub requested: Arc<std::sync::atomic::AtomicBool>,
}

/// Live status of the most recent background Dream run, surfaced to the
/// topnav indicator via `GET /dashboard/dream/status`. Serialized straight
/// to JSON, so the field names are the client contract.
#[derive(Clone, Default, serde::Serialize)]
pub struct DreamStatus {
    /// Human label of the dream currently running (`"Compile"`, `"Full REM"`),
    /// or `None` when idle.
    pub running: Option<String>,
    /// The outcome of the last finished run, kept until the next one starts.
    pub last: Option<DreamLast>,
}

/// Outcome of one finished background Dream run.
#[derive(Clone, serde::Serialize)]
pub struct DreamLast {
    /// Human label of the dream that ran.
    pub kind: String,
    /// `true` when the dream completed; `false` when it errored.
    pub ok: bool,
    /// One-line summary on success, or the error message on failure.
    pub summary: String,
    /// RFC-3339 completion stamp.
    pub finished_at: String,
}

impl DashboardState {
    /// Convenience constructor — most callers want defaults for
    /// [`DashboardConfig`] and only pass the live handles.
    #[must_use]
    pub fn new(
        pool: SqlitePool,
        secret: TokenSecret,
        blacklist: Arc<BlacklistCache>,
        delegations: Arc<DelegationCache>,
    ) -> Self {
        Self {
            pool,
            secret,
            blacklist,
            delegations,
            config: DashboardConfig::default(),
            memory: None,
            rem_gate: Arc::new(tokio::sync::Mutex::new(())),
            rem_policy: Arc::new(RwLock::new(mwe_core::rem::RemPolicy::default())),
            recall: Arc::new(RwLock::new(RecallConfig::default())),
            claude_login: Arc::new(Mutex::new(None)),
            dream_status: Arc::new(Mutex::new(DreamStatus::default())),
            backup_schedule: Arc::new(RwLock::new(None)),
            restart: None,
        }
    }

    /// Builder-style override of the runtime config.
    #[must_use]
    pub fn with_config(self, config: DashboardConfig) -> Self {
        Self { config, ..self }
    }

    /// Builder-style attachment of the [`MemoryHandles`] needed by the
    /// memory-explorer + chat routes.
    #[must_use]
    pub fn with_memory(self, memory: MemoryHandles) -> Self {
        Self {
            memory: Some(memory),
            ..self
        }
    }

    /// Builder-style attachment of the **shared** REM policy handle
    /// (`rem.policy:` config section, resolved) — `mwe-mcp serve`
    /// passes the same `Arc` it hands the REM scheduler so the REM
    /// settings editor's hot-reload reaches the scheduled cycles too.
    #[must_use]
    pub fn with_rem_policy(self, rem_policy: Arc<RwLock<mwe_core::rem::RemPolicy>>) -> Self {
        Self { rem_policy, ..self }
    }

    /// Builder-style attachment of the **shared** recall-settings
    /// handle (`recall:` config section) — `mwe-mcp serve` passes the
    /// same `Arc` it puts in the MCP transport's state so the editor's
    /// hot-reload reaches both transports.
    #[must_use]
    pub fn with_recall(self, recall: Arc<RwLock<RecallConfig>>) -> Self {
        Self { recall, ..self }
    }

    /// Builder-style attachment of the **shared** backup-schedule
    /// handle (`backup:` config section, resolved) — `mwe-mcp serve`
    /// passes the same `Arc` it hands the backup scheduler so the
    /// Backup console's save applies at the next due-check.
    #[must_use]
    pub fn with_backup_schedule(
        self,
        backup_schedule: Arc<RwLock<Option<mwe_core::backup::BackupSchedule>>>,
    ) -> Self {
        Self {
            backup_schedule,
            ..self
        }
    }

    /// Builder-style attachment of the restart plumbing (serve build
    /// only).
    #[must_use]
    pub fn with_restart(self, restart: RestartHandle) -> Self {
        Self {
            restart: Some(restart),
            ..self
        }
    }

    /// Replace the live backup schedule in place — the Backup console
    /// calls this right after the YAML save (write disk first, then
    /// swap, same ordering as the other section editors).
    pub fn replace_backup_schedule(&self, fresh: mwe_core::backup::BackupSchedule) {
        *self
            .backup_schedule
            .write()
            .expect("backup schedule rwlock poisoned") = Some(fresh);
    }

    /// Snapshot the operator recall settings. Owned value so the caller
    /// never holds the read-lock across an `await`.
    #[must_use]
    pub fn recall_snapshot(&self) -> RecallConfig {
        self.recall
            .read()
            .expect("recall settings rwlock poisoned")
            .clone()
    }

    /// Replace the live recall settings in place — the recall-settings
    /// editor calls this right after the YAML save (write disk first,
    /// then swap, same ordering as the LLM-config editor).
    pub fn replace_recall(&self, fresh: RecallConfig) {
        *self
            .recall
            .write()
            .expect("recall settings rwlock poisoned") = fresh;
    }

    /// Snapshot the REM full-cycle policy. Owned value so the caller
    /// never holds the read-lock across an `await` (the Dream trigger
    /// runs a whole cycle with it).
    #[must_use]
    pub fn rem_policy_snapshot(&self) -> mwe_core::rem::RemPolicy {
        self.rem_policy
            .read()
            .expect("rem policy rwlock poisoned")
            .clone()
    }

    /// Replace the live REM policy in place — the REM settings editor
    /// calls this right after the YAML save (write disk first, then
    /// swap, same ordering as the recall-settings editor).
    pub fn replace_rem_policy(&self, fresh: mwe_core::rem::RemPolicy) {
        *self.rem_policy.write().expect("rem policy rwlock poisoned") = fresh;
    }
}
