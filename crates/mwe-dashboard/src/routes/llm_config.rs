// SPDX-License-Identifier: AGPL-3.0-or-later
//! Admin-only LLM configuration page: the per-role model assignment in
//! `<workdir>/mwe-mcp.config.yaml > llm`, the API-key env-vars it
//! references, the Claude Code login store, and the models.dev catalog.
//!
//! The page is **credentials-first**: a "Provider e credenziali" section
//! (one card per backend) where keys are set and Anthropic's auth mode is
//! chosen, then a "Ruoli" section that assigns each canonical LLM function
//! a provider + model with plain-language guidance. The `api_key_env` is
//! **derived** from the chosen provider on save ([`derive_api_key_env`]),
//! never an operator-edited column.
//!
//! Routes (all behind [`AdminUser`]):
//!
//! - GET  `/admin/llm-config`                — render the page.
//! - POST `/admin/llm-config`                — atomic save of the `llm`
//!   YAML section (backup `.bak`, serialise, atomic-write) + hot-reload.
//!   Operator comments in the YAML are flattened (`serde_yaml` keeps none).
//! - POST `/admin/llm-config/profile/:name`  — apply a preset
//!   [`mwe_core::config::LlmProfile`] to every role.
//! - POST `/admin/api-keys/:name`            — upsert a key env-var in
//!   `<workdir>/mwe-mcp.env` via [`mwe_core::env_file::write_key`].
//! - POST `/admin/llm-catalog/refresh`       — re-fetch the models.dev
//!   catalog into the workdir cache.
//!
//! Credentials never render the key in cleartext: each card shows a 4-char
//! fingerprint of the value's last characters, the origin (`live override`
//! / `shell` / `env file`), and a one-input form to overwrite it. The
//! well-known list ([`WELL_KNOWN_KEYS`]) covers one key per cloud backend
//! in [`ALLOWED_BACKENDS`]. Both the YAML save and the key set are
//! **hot-reloaded** into the running process (in-memory swap + override
//! map) — no `mwe-mcp serve` restart needed; the env-file copy on disk is
//! what survives a restart.
//!
//! See the admin LLM config
//! wiki page.

#![allow(clippy::too_many_lines, reason = "single-page admin editor")]

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use mwe_core::config::{CONFIG_FILENAME, Config, LlmConfig, LlmFunction, LlmFunctionConfig};
use mwe_core::env_file;
use mwe_core::model_catalog::{self, Catalog};
use mwe_core::wiki::atomic_write;

use crate::auth::AdminUser;
use crate::error::{DashboardError, Result};
use crate::form::HtmlForm;
use crate::state::{DashboardState, MemoryHandles};
use crate::ui::{components, layout};

/// Filename of the workdir env file [`env_file::write_key`] writes into.
/// Pinned here (instead of pulled from `mwe-mcp-server`) so the
/// dashboard crate does not depend on the binary crate.
const ENV_FILENAME: &str = "mwe-mcp.env";

/// Env vars the credentials section always renders, even when no role
/// references them — so the admin can pre-load a key before assigning the
/// provider to a role. One per selectable cloud backend in
/// [`ALLOWED_BACKENDS`].
const WELL_KNOWN_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    // Optional Bearer for a remote / cloud Ollama; a local daemon needs none.
    "OLLAMA_API_KEY",
];

/// Env-var holding the provider-level Ollama endpoint (per-role `base_url`
/// under «Advanced» still overrides it; the runtime falls back to
/// [`mwe_core::llm::DEFAULT_OLLAMA_URL`]). Mirrors the cloud keys: set from
/// the dashboard, hot-reloaded via the override map, persisted to the env
/// file.
const OLLAMA_BASE_URL_ENV: &str = "OLLAMA_BASE_URL";

/// Backends accepted by the per-role provider dropdown. Pinned to the five
/// the runtime can actually materialise
/// ([`LlmFunctionConfig::build_backend`]); other strings parsed from YAML
/// are tolerated by `Config::load` but the editor refuses to emit them.
const ALLOWED_BACKENDS: &[&str] = &["ollama", "anthropic", "gemini", "openai", "openrouter"];

/// A provider surfaced in the credentials section and the per-role
/// provider dropdown.
struct ProviderInfo {
    /// Backend tag stored in `LlmFunctionConfig::backend`.
    tag: &'static str,
    /// Human label shown in the UI.
    label: &'static str,
}

/// Providers in display order: the local backend first, then the cloud
/// provider/aggregators. Drives the per-role provider `<select>`.
const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        tag: "ollama",
        label: "Ollama (local)",
    },
    ProviderInfo {
        tag: "anthropic",
        label: "Anthropic",
    },
    ProviderInfo {
        tag: "gemini",
        label: "Google Gemini",
    },
    ProviderInfo {
        tag: "openai",
        label: "OpenAI",
    },
    ProviderInfo {
        tag: "openrouter",
        label: "OpenRouter",
    },
];

/// Plain-language guidance for each role — the point of the redesign:
/// telling the operator, in plain English, what strength each function
/// needs (a local 9B, a mid-tier model, or a strong one).
struct RoleGuide {
    /// The canonical slot this guide describes.
    slot: LlmFunction,
    /// Short human title.
    title: &'static str,
    /// One-line description of what the role does.
    blurb: &'static str,
    /// Capability-tier hint + a concrete model suggestion.
    tier: &'static str,
}

/// Roles in display order — conversational first, then the nightly /
/// per-turn functions. All six [`LlmFunction`] slots, ordered for the UX.
#[allow(
    deprecated,
    reason = "Cronista is surfaced as a role card despite its deprecated marker"
)]
const ROLE_GUIDES: &[RoleGuide] = &[
    RoleGuide {
        slot: LlmFunction::Ingest,
        title: "Conversation & capture",
        blurb: "Classifies intent and extracts facts on every message.",
        tier: "workhorse — a local 9B is fine; for structural judgements a mid-tier model is better (Sonnet / Gemini Flash)",
    },
    RoleGuide {
        slot: LlmFunction::HubWriter,
        title: "Index summaries",
        blurb: "Regenerates the index.md summaries on every write.",
        tier: "workhorse — a local 9B is fine",
    },
    RoleGuide {
        slot: LlmFunction::OperatorChat,
        title: "Operator chat",
        blurb: "Powers this dashboard's operational chat — multi-step tool calls (recall, forget, move, supersede) that must handle fact ids faithfully. Leave disabled to reuse «Index summaries» (hub_writer).",
        tier: "strong, with reliable function-calling (Sonnet / Gemini Pro / a 32B+ local); the 9B struggles with multi-tool reasoning and ids",
    },
    RoleGuide {
        slot: LlmFunction::RemPromotions,
        title: "Nightly optimisation",
        blurb: "Promotes paragraphs, files and wikis at night. Quality-critical, cost acceptable.",
        tier: "strong — not the 9B (Opus 4.8 / Gemini Pro)",
    },
    RoleGuide {
        slot: LlmFunction::Cronista,
        title: "Prose compilation",
        blurb: "Compiles captured facts into the readable wiki pages — without it the memory still recalls, but the pages stay blank.",
        tier: "strong — not the 9B (Opus 4.8 / Gemini Pro)",
    },
    RoleGuide {
        slot: LlmFunction::Navigator,
        title: "Recall navigation",
        blurb: "Decides which wikis and pages to open on every turn.",
        tier: "strong but cheap (Haiku / Gemini Flash)",
    },
    RoleGuide {
        slot: LlmFunction::RemDedupSemantic,
        title: "Dedup",
        blurb: "Yes/no classifier after the jaccard pre-pass.",
        tier: "small — a 9B / mini is enough",
    },
];

/// `reasoning_effort` choices the editor surfaces in the dropdown.
/// Free-form per the YAML schema (`LlmFunctionConfig` is `Option<String>`),
/// but the editor pins a known set so an admin picking from the UI
/// always sends a value the Anthropic adapter understands. Ignored
/// by the Gemini adapter (which pins its own thinking level
/// internally — see `mwe_core::llm::GEMINI_THINKING_LEVEL`).
const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "extra-high"];

/// The canonical LLM slots surfaced as role cards in the admin UI, in
/// display order — **every** [`LlmFunction`] variant. `save()` rebuilds
/// the config from exactly this list, so a slot left off here is dropped
/// on every save: omitting `Cronista` meant the dashboard silently wiped
/// the prose compiler, so a fresh setup captured facts but never rendered
/// readable pages (see
/// admin LLM config).
/// `Cronista` keeps its `#[deprecated]` marker (it has not yet graduated
/// to a full REM sub-job) but is configured like every other slot.
#[allow(
    deprecated,
    reason = "Cronista is surfaced as a configurable slot despite its deprecated marker"
)]
const SLOTS: &[LlmFunction] = &[
    LlmFunction::HubWriter,
    LlmFunction::Ingest,
    LlmFunction::OperatorChat,
    LlmFunction::RemPromotions,
    LlmFunction::Cronista,
    LlmFunction::RemDedupSemantic,
    LlmFunction::Navigator,
];

/// Sub-router for `/admin/llm-config` and `/admin/api-keys/:name`.
/// Mounted inside the authenticated tree.
pub fn router() -> Router<DashboardState> {
    Router::new()
        .route("/admin/llm-config", get(page).post(save))
        .route("/admin/llm-config/profile/:name", post(apply_profile))
        .route("/admin/api-keys/:name", post(set_api_key))
        .route("/admin/llm-catalog/refresh", post(refresh_catalog))
        .route("/admin/ollama-endpoint", post(set_ollama_endpoint))
        .route("/admin/ollama-models", get(ollama_models))
}

// ---------- helpers shared across routes ----------

/// Pull `MemoryHandles` out of state or fail with a 500. Reachable from
/// the sibling Claude Code login routes ([`super::claude_login`]) so they
/// can land the operator back on this page after the OAuth exchange (the
/// module is private, so `pub` here stays crate-internal).
pub fn require_memory(state: &DashboardState) -> Result<&MemoryHandles> {
    state.memory.as_ref().ok_or_else(|| {
        DashboardError::Internal(
            "memory handles missing — start the server with `mwe-mcp serve`".to_owned(),
        )
    })
}

/// First-run gate: true while the signed-in admin has not yet completed
/// (or skipped) the profile primer. Drives the step-1 onboarding banner,
/// and pairs with the [`super::welcome`] redirect that sends the admin
/// here before that primer.
async fn in_onboarding(state: &DashboardState, admin: &AdminUser) -> Result<bool> {
    crate::routes::welcome::user_already_initialized(state, &admin.session().sender_id)
        .await
        .map(|done| !done)
}

fn config_path_in(memory: &MemoryHandles) -> PathBuf {
    memory.workdir.join(CONFIG_FILENAME)
}

fn env_path_in(memory: &MemoryHandles) -> PathBuf {
    memory.workdir.join(ENV_FILENAME)
}

fn backup_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Return the last `n` chars of `s` (chars, not bytes) to surface a
/// 4-char fingerprint of an API key without ever exposing the secret.
fn fingerprint(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_owned()
    } else {
        s.chars().skip(count - n).collect()
    }
}

/// True when `name` is a syntactically valid env-var identifier
/// (`[A-Z_][A-Z0-9_]*`). The route refuses to write arbitrary names
/// — an operator typo in the URL would otherwise become a random
/// assignment in `mwe-mcp.env`.
fn is_valid_env_key_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_uppercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Parse `<workdir>/mwe-mcp.env` into a map of `KEY -> value`. We do
/// our own tiny parser instead of pulling `dotenvy` because the
/// dashboard only needs the values, not the side-effect of pushing
/// them into the process env (which would require `unsafe` we cannot
/// use). Format handled: `KEY=value` and `KEY="value"` with `\\` /
/// `\"` escapes — symmetric to [`env_file::write_key`].
fn read_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(body) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for raw_line in body.lines() {
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim().to_owned();
        if key.is_empty() {
            continue;
        }
        let value_raw = line[eq + 1..].trim();
        let value = value_raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map_or_else(|| value_raw.to_owned(), unescape);
        out.insert(key, value);
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped @ ('\\' | '"')) => out.push(escaped),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                },
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------- API key panel data ----------

/// Source the API key value was sighted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOrigin {
    /// In-memory override populated by the dashboard set-API-key
    /// handler. Takes precedence over shell + env file: the
    /// override is what [`MemoryHandles::backend_for`] will pass to
    /// the LLM constructor on the next request.
    Override,
    /// `std::env::var(name)` returned a non-empty value. Shell-wins
    /// in `env_loader` precedence — the operator exported it before
    /// `mwe-mcp serve`, or systemd set it via `EnvironmentFile=`.
    Shell,
    /// Value found only in the workdir env file. The server already
    /// holds this value (`env_loader` ran at boot); rendering the
    /// fingerprint here is the source of truth for what is wired.
    EnvFile,
    /// No value anywhere.
    NotSet,
}

impl KeyOrigin {
    const fn label(self) -> &'static str {
        match self {
            Self::Override => "live override",
            Self::Shell => "shell",
            Self::EnvFile => "env file",
            Self::NotSet => "—",
        }
    }
}

#[derive(Debug)]
struct ApiKeyRow {
    name: String,
    origin: KeyOrigin,
    fingerprint: Option<String>,
}

/// Compute the API key panel rows. Combines the well-known names with
/// every `api_key_env` referenced by the live `LlmConfig`. Reads both
/// the process env (shell-wins) and the workdir env file (so a value
/// just saved here surfaces even if the running process holds a
/// stale shell copy).
fn api_key_rows(memory: &MemoryHandles) -> Vec<ApiKeyRow> {
    let file_values = read_env_file(&env_path_in(memory));
    let overrides = memory.api_key_overrides_snapshot();
    let llm = memory.llm_config_snapshot();

    let mut names: BTreeSet<String> = WELL_KNOWN_KEYS.iter().map(|s| (*s).to_owned()).collect();
    for slot in SLOTS {
        if let Some(cfg) = llm.slot(*slot)
            && let Some(key) = cfg.api_key_env.as_deref()
            && !key.is_empty()
            // The `claude-code` sentinel is not an env var — it routes a
            // slot to the Claude Code login store, surfaced by its own
            // panel below, not the API-key table.
            && key != mwe_core::oauth::CLAUDE_CODE_LOGIN
        {
            names.insert(key.to_owned());
        }
    }
    // An override set for an env-var that is no longer referenced by
    // any slot still deserves to be shown, otherwise the operator
    // who sets a key before wiring the slot would see "not set".
    for key in overrides.keys() {
        names.insert(key.clone());
    }

    names
        .into_iter()
        .map(|name| {
            // Precedence mirrors `MemoryHandles::backend_for`:
            // override > shell > env file. The renderer surfaces the
            // origin so the admin can tell which layer is winning.
            let override_value = overrides.get(&name).cloned();
            let shell_value = std::env::var(&name).ok();
            let file_value = file_values.get(&name).cloned();
            let candidates = [
                (KeyOrigin::Override, override_value.as_deref()),
                (KeyOrigin::Shell, shell_value.as_deref()),
                (KeyOrigin::EnvFile, file_value.as_deref()),
            ];
            let (origin, fingerprint) = candidates
                .into_iter()
                .find_map(|(o, v)| {
                    v.filter(|raw| !raw.trim().is_empty())
                        .map(|raw| (o, Some(fingerprint(raw, 4))))
                })
                .unwrap_or((KeyOrigin::NotSet, None));
            ApiKeyRow {
                name,
                origin,
                fingerprint,
            }
        })
        .collect()
}

// ---------- GET ----------

#[derive(Debug, Default, Clone, Copy)]
struct Flash<'a> {
    kind: &'a str,
    msg: &'a str,
}

async fn page(State(state): State<DashboardState>, admin: AdminUser) -> Result<Html<String>> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let onboarding = in_onboarding(&state, &admin).await?;
    let body = render(chrome, admin.session(), memory, None, onboarding);
    Ok(Html(body))
}

// Inline-style snippets (design tokens from `tokens.css`) for the new
// card-based layout — the same approach the Dream / Help modals use, so no
// Tailwind recompile is needed for the redesigned surface.
const CARD_STYLE: &str =
    "border:1px solid var(--border);border-radius:.5rem;padding:.7rem .9rem;background:var(--bg-2)";
const CARD_HEAD_STYLE: &str =
    "display:flex;align-items:center;gap:.6rem;justify-content:space-between";
const FIELD_LABEL_STYLE: &str = "font-size:.72rem;text-transform:uppercase;letter-spacing:.05em";
const INPUT_STYLE: &str = "width:100%;background:var(--bg);border:1px solid var(--border);border-radius:.3rem;padding:.35rem .5rem;color:var(--text);font-family:var(--font-mono);font-size:.85rem";

/// Which providers currently have usable authentication — drives the
/// per-role "needs a key" warning and the JS gating map.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per cloud provider — the set is fixed and known at compile time; a map would only hide that"
)]
struct ProviderAuth {
    anthropic: bool,
    gemini: bool,
    openai: bool,
    openrouter: bool,
}

/// Compute usable-auth per cloud provider: a key is set, or (Anthropic
/// only) the Claude Code login store holds credentials.
fn provider_auth(rows: &[ApiKeyRow]) -> ProviderAuth {
    let has = |name: &str| {
        rows.iter()
            .any(|r| r.name == name && r.fingerprint.is_some())
    };
    let claude_logged = mwe_core::oauth::global_store()
        .and_then(|s| s.load().ok().flatten())
        .is_some();
    ProviderAuth {
        anthropic: has("ANTHROPIC_API_KEY") || claude_logged,
        gemini: has("GEMINI_API_KEY"),
        openai: has("OPENAI_API_KEY"),
        openrouter: has("OPENROUTER_API_KEY"),
    }
}

/// True when any Anthropic role is wired to the Claude Code login store —
/// the current "auth mode" the credential card's toggle reflects.
fn anthropic_uses_login(llm: &LlmConfig) -> bool {
    SLOTS.iter().any(|s| {
        llm.slot(*s).and_then(|c| c.api_key_env.as_deref())
            == Some(mwe_core::oauth::CLAUDE_CODE_LOGIN)
    })
}

/// Can the runtime materialise this backend tag with the credentials
/// present right now? Mirrors the per-role "needs a key" gating: the
/// local backend needs none, each cloud backend needs its provider auth.
fn backend_usable(backend: &str, auth: &ProviderAuth) -> bool {
    match backend {
        "ollama" => true,
        "anthropic" => auth.anthropic,
        "gemini" => auth.gemini,
        "openai" => auth.openai,
        "openrouter" => auth.openrouter,
        // empty (disabled), or a tag the editor never emits.
        _ => false,
    }
}

/// True when the **ingest** role is wired to a usable backend. This is the
/// gate for reaching the first-login profile primer, which calls
/// `wiki_ingest_message` and degrades to an empty `skip` turn without a
/// usable ingest model. Config-level only — no network probe (the redesign
/// carries no active connection check), so a selected Ollama counts as
/// usable; an unreachable daemon surfaces as a graceful skip, not here.
fn ingest_ready(llm: &LlmConfig, auth: &ProviderAuth) -> bool {
    llm.slot(LlmFunction::Ingest)
        .is_some_and(|cfg| backend_usable(&cfg.backend, auth))
}

/// First-run onboarding banner pinned to the top of the page while the
/// admin has not yet completed the profile primer: it frames the page as
/// step 1 of 2 and offers the path forward. The "continue" link is live
/// only once the ingest role is usable — otherwise the primer would bounce
/// straight back here (the [`super::welcome`] guard), so we show a hint
/// instead of a dead-ending link.
fn onboarding_banner(ingest_ready: bool) -> Markup {
    html! {
        div.flash.flash-info style="display:flex;align-items:center;justify-content:space-between;gap:1rem;flex-wrap:wrap" {
            div {
                strong { "First-run setup · step 1 of 2 — wire the model." }
                " The profile setup that follows uses the " code { "ingest" }
                " role, so configure it here first."
            }
            @if ingest_ready {
                a href="/dashboard/welcome"
                  style="white-space:nowrap;display:inline-block;background:var(--p);color:var(--bg);font-weight:600;padding:.4rem .7rem;border-radius:.3rem;text-decoration:none" {
                    "Continue to profile setup →"
                }
            } @else {
                span.muted style="white-space:nowrap;font-size:.85rem" {
                    "Give the ingest role a usable provider to continue."
                }
            }
        }
    }
}

fn render(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    memory: &MemoryHandles,
    flash: Option<Flash<'_>>,
    onboarding: bool,
) -> String {
    let rows = api_key_rows(memory);
    let llm = memory.llm_config_snapshot();
    let catalog = model_catalog::load(&memory.workdir);
    let auth = provider_auth(&rows);
    let anthropic_login = anthropic_uses_login(&llm);
    let ingest_ok = ingest_ready(&llm, &auth);
    let body = html! {
        @if onboarding {
            (onboarding_banner(ingest_ok))
        }
        @if let Some(f) = flash {
            (components::flash(f.kind, f.msg))
        }

        p.flash.flash-info {
            strong { "Changes take effect on the next request." }
            " Roles and keys are applied to the running process as well as "
            "written to disk — no " code { "mwe-mcp serve" } " restart. "
            "Keys set here live in memory; the on-disk copy ("
            code { (ENV_FILENAME) } ") is what survives a restart."
        }

        // ── Section 1 — providers & credentials ───────────────────────────
        h2 { "1 · Providers & credentials" }
        p.muted {
            "Set up credentials here, before the roles. A role can only use a "
            "provider with valid authentication. The cleartext key never leaves "
            "the server — only its last 4 characters render."
        }
        (providers_section(memory, &rows, &llm, anthropic_login))

        // ── Section 2 — roles ─────────────────────────────────────────────
        h2 { "2 · Roles" }
        p.muted {
            "Give each function a provider and a model. Start from a quick "
            "profile, then fine-tune. Leave a provider on «— disabled —» only "
            "for the optional functions; the others are needed to run."
        }
        (profile_quickset())
        (catalog_refresh(&catalog))
        form #llm-roles action="/dashboard/admin/llm-config" method="post" {
            // Role cards flow in an auto-fit grid (≈2–3 columns on a wide
            // screen, one column on mobile) so they use the page width instead
            // of stacking in a tall narrow strip. `.card-grid` also opts this
            // form out of the single-column form width cap (see app.css).
            div.card-grid {
                @for guide in ROLE_GUIDES {
                    (role_row(guide, llm.slot(guide.slot)))
                }
            }
            p style="margin-top:.9rem" { button type="submit" { "Save configuration" } }
        }

        p.muted {
            "Saving rewrites " code { (CONFIG_FILENAME) } " atomically "
            "(backup " code { (format!("{CONFIG_FILENAME}.bak")) } ") and hot-reloads "
            "the config. " strong { "Comments in the YAML are not preserved." }
        }

        (model_datalists(&catalog))
        // Filled client-side from GET /admin/ollama-models — Ollama's
        // installed tags aren't in the models.dev catalog.
        datalist #models-ollama {}
        (embed_config_data(&catalog, &auth))
        script src="/dashboard/static/llm-config.js" defer {}
    };
    layout::authenticated_page(chrome, "LLM config", session, &body)
}

/// The provider/credentials cards: the local backend, then the cloud
/// providers (Anthropic carries the Claude Code login auth-mode toggle).
fn providers_section(
    memory: &MemoryHandles,
    rows: &[ApiKeyRow],
    llm: &LlmConfig,
    anthropic_login: bool,
) -> Markup {
    html! {
        // Provider cards flow in the same auto-fit grid as the role cards
        // below (≈2–3 columns on a wide screen, one column on mobile).
        div.card-grid style="margin:.3rem 0 1rem" {
            (provider_card_ollama(memory, rows))
            (provider_card_anthropic(rows, llm, anthropic_login))
            (provider_card_key("Google Gemini", "GEMINI_API_KEY", rows, None))
            (provider_card_key(
                "OpenAI",
                "OPENAI_API_KEY",
                rows,
                Some("Your own OpenAI key, used directly: no aggregator account and \
                      nobody else in the path. Reasoning models (the gpt-5 line, the \
                      o-series) are handled from their documented behaviour."),
            ))
            (provider_card_key(
                "OpenRouter",
                "OPENROUTER_API_KEY",
                rows,
                Some("OpenAI-compatible aggregator: one key, models as vendor/model \
                      slugs. The catalogue is public — models show up even without a \
                      key, which is only needed to use them."),
            ))
        }
    }
}

/// Raw current value of an env-var as the running config sees it
/// (override > shell > workdir env file). Unlike [`api_key_rows`] this
/// returns the value in cleartext — used for the non-secret Ollama
/// endpoint, which the card shows in full.
fn current_env_value(memory: &MemoryHandles, name: &str) -> Option<String> {
    memory
        .api_key_overrides_snapshot()
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
        .or_else(|| read_env_file(&env_path_in(memory)).get(name).cloned())
        .filter(|v| !v.trim().is_empty())
}

/// The Ollama card. Unlike the cloud cards it carries an **endpoint** (a
/// local daemon needs no key) plus an *optional* Bearer token for a remote
/// / cloud / proxied daemon. Both are provider-level and hot-reloaded; the
/// per-role `base_url` under «Advanced» still wins.
fn provider_card_ollama(memory: &MemoryHandles, rows: &[ApiKeyRow]) -> Markup {
    let endpoint = current_env_value(memory, OLLAMA_BASE_URL_ENV)
        .unwrap_or_else(|| mwe_core::llm::DEFAULT_OLLAMA_URL.to_owned());
    html! {
        div style=(CARD_STYLE) {
            div style=(CARD_HEAD_STYLE) {
                strong { "Ollama " span.muted style="font-weight:400" { "(local or remote)" } }
                span.badge.badge-default { "no key for local" }
            }
            p.muted style="margin:.3rem 0 .5rem;font-size:.8rem" {
                "A local daemon needs no key. For a remote or cloud daemon set the "
                "endpoint and, if it is authenticated, a Bearer token. The model field "
                "then lists what is installed there; a per-role endpoint override stays "
                "under «Advanced»."
            }
            form action="/dashboard/admin/ollama-endpoint" method="post"
                 style="display:flex;gap:.5rem;align-items:flex-end;flex-wrap:wrap" {
                div style="flex:1;min-width:15rem" {
                    label.muted style=(FIELD_LABEL_STYLE) { "Endpoint" }
                    input type="text" name="endpoint" value=(endpoint)
                        placeholder=(mwe_core::llm::DEFAULT_OLLAMA_URL)
                        autocomplete="off" spellcheck="false" style=(INPUT_STYLE);
                }
                button type="submit" { "Save endpoint" }
            }
            div style="margin-top:.6rem" {
                p.muted style="margin:0 0 .25rem;font-size:.74rem" {
                    "API key — remote / cloud only, optional (Bearer) "
                    code style="font-size:.72rem" { "OLLAMA_API_KEY" }
                }
                (api_key_form("OLLAMA_API_KEY", rows))
            }
        }
    }
}

/// A cloud provider card with a single API-key credential.
fn provider_card_key(
    label: &str,
    env_name: &str,
    rows: &[ApiKeyRow],
    note: Option<&str>,
) -> Markup {
    html! {
        div style=(CARD_STYLE) {
            div style=(CARD_HEAD_STYLE) {
                strong { (label) }
                code.muted style="font-size:.72rem" { (env_name) }
            }
            @if let Some(note) = note {
                p.muted style="margin:.25rem 0 .4rem;font-size:.78rem" { (note) }
            }
            div style="margin-top:.4rem" { (api_key_form(env_name, rows)) }
        }
    }
}

/// The Anthropic card: an auth-mode toggle (API key vs Claude Code login)
/// whose radios are associated with the roles form (`form="llm-roles"`), so
/// the save derives each Anthropic role's `api_key_env` from the choice.
fn provider_card_anthropic(rows: &[ApiKeyRow], llm: &LlmConfig, anthropic_login: bool) -> Markup {
    html! {
        div style=(CARD_STYLE) {
            div style=(CARD_HEAD_STYLE) {
                strong { "Anthropic" }
                code.muted style="font-size:.72rem" { "ANTHROPIC_API_KEY" }
            }
            div style="display:flex;gap:1.4rem;margin:.5rem 0 .4rem;font-size:.85rem;flex-wrap:wrap" {
                label style="display:inline-flex;align-items:center;gap:.35rem;cursor:pointer" {
                    input type="radio" name="anthropic_auth_mode" value="key"
                        form="llm-roles" checked[!anthropic_login];
                    "API key"
                }
                label style="display:inline-flex;align-items:center;gap:.35rem;cursor:pointer" {
                    input type="radio" name="anthropic_auth_mode" value="login"
                        form="llm-roles" checked[anthropic_login];
                    "Claude Code login " span.badge.badge-bundled { "personal" }
                }
            }
            div id="anthropic-key-block" style=(if anthropic_login { "display:none" } else { "" }) {
                (api_key_form("ANTHROPIC_API_KEY", rows))
            }
            div id="anthropic-login-block" style=(if anthropic_login { "" } else { "display:none" }) {
                (claude_login_block(llm))
            }
        }
    }
}

/// Status badge + one-input form to set/rotate a single API-key env-var.
/// Shared by every cloud provider card. The cleartext value never leaves
/// the server; only the last-4 fingerprint renders.
fn api_key_form(env_name: &str, rows: &[ApiKeyRow]) -> Markup {
    let row = rows.iter().find(|r| r.name == env_name);
    let fingerprint = row.and_then(|r| r.fingerprint.clone());
    let origin = row.map_or("—", |r| r.origin.label());
    html! {
        p style="margin:0 0 .35rem" {
            @if let Some(fp) = &fingerprint {
                span.badge.badge-default { "set" } " …" code { (fp) }
                " " span.muted style="font-size:.74rem" { "(" (origin) ")" }
            } @else {
                span.badge.badge-bundled { "no key" }
            }
        }
        form action=(format!("/dashboard/admin/api-keys/{env_name}"))
             method="post" class="inline-form" {
            input type="password" name="value" autocomplete="off"
                placeholder=(if fingerprint.is_some() { "new value…" } else { "paste the key…" })
                required;
            button type="submit" { "Save" }
        }
    }
}

/// Quick-set buttons that apply a whole preset profile to every role.
fn profile_quickset() -> Markup {
    html! {
        div style="display:flex;align-items:center;gap:.5rem;flex-wrap:wrap;margin:.2rem 0 .7rem" {
            span.muted style="font-size:.8rem" { "Quick profile:" }
            (profile_button("all-local", "All local"))
            (profile_button("hybrid", "Hybrid"))
            (profile_button("all-api", "All API"))
            span.muted style="font-size:.72rem" { "fills every role — then fine-tune" }
        }
    }
}

fn profile_button(name: &str, label: &str) -> Markup {
    html! {
        form action=(format!("/dashboard/admin/llm-config/profile/{name}"))
             method="post" class="inline-form" {
            button type="submit" { (label) }
        }
    }
}

/// One `<datalist>` per cloud backend, populated from the models.dev
/// catalog — the suggestions behind the free-text model combobox.
fn model_datalists(catalog: &Catalog) -> Markup {
    html! {
        @for backend in ["anthropic", "gemini", "openai", "openrouter"] {
            datalist id=(format!("models-{backend}")) {
                @for m in catalog.models_for(backend) {
                    option value=(m.id) { (m.name) }
                }
            }
        }
    }
}

/// Embed the catalog + per-provider auth status as inline JSON for
/// `llm-config.js` (metadata strip + provider gating).
fn embed_config_data(catalog: &Catalog, auth: &ProviderAuth) -> Markup {
    let data = serde_json::json!({
        "catalog": {
            "anthropic": catalog.models_for("anthropic"),
            "gemini": catalog.models_for("gemini"),
            "openai": catalog.models_for("openai"),
            "openrouter": catalog.models_for("openrouter"),
        },
        "auth": {
            "ollama": true,
            "anthropic": auth.anthropic,
            "gemini": auth.gemini,
            "openai": auth.openai,
            "openrouter": auth.openrouter,
        },
    });
    let json = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_owned());
    html! {
        script type="application/json" id="llm-config-data" { (maud::PreEscaped(json)) }
    }
}

/// Render the LLM-config page with a flash banner. Reachable from the
/// sibling Claude Code login routes ([`super::claude_login`]) so they can
/// land the operator back here with a success/error message after the
/// OAuth exchange — the same direct-render pattern the `save` handler
/// uses (the module is private, so `pub` here stays crate-internal).
pub fn render_status_page(
    chrome: layout::Chrome,
    session: &crate::auth::SessionUser,
    memory: &MemoryHandles,
    kind: &'static str,
    msg: &str,
) -> String {
    // The Claude-login round-trip lands here with a flash but without the
    // step-1 onboarding banner; it reappears on the next page GET, which
    // recomputes the gate. Not worth threading the async profile lookup
    // through every claude_login caller.
    render(chrome, session, memory, Some(Flash { kind, msg }), false)
}

/// The Claude Code login controls inside the Anthropic credential card —
/// status + login / logout buttons for the workdir OAuth store. Shown when
/// the card's auth mode is "login". Test / personal use only; the `_llm`
/// param keeps the call site's `llm` threaded for future use. See
/// [`mwe_core::oauth`] and `docs/protocol/config-schema.md`.
fn claude_login_block(_llm: &LlmConfig) -> Markup {
    let creds = mwe_core::oauth::global_store().and_then(|s| s.load().ok().flatten());
    html! {
        p.muted style="margin:.2rem 0 .45rem;font-size:.78rem" {
            "Sign Anthropic requests with your own Claude subscription instead of "
            "an API key. " strong { "Personal / test use only" }
            " — a deployment brings its own keys."
        }
        @match creds {
            Some(creds) => {
                @let mins = remaining_minutes(creds.expires_at);
                p style="margin:0 0 .35rem" {
                    span.badge.badge-default { "connected" }
                    @if mins > 0 {
                        " — token valid ~" (mins) " min (auto-refreshes)."
                    } @else {
                        " — token expired (refreshes on next use)."
                    }
                }
                form action="/dashboard/admin/claude-login/logout" method="post" class="inline-form" {
                    button type="submit" { "Log out of Claude Code" }
                }
            },
            None => {
                p style="margin:0 0 .35rem" { span.badge.badge-bundled { "not connected" } }
                p.muted style="margin:0 0 .4rem;font-size:.76rem" {
                    "Approve on Claude's page, then paste the code it shows — a "
                    "couple of clicks."
                }
                // Always the out-of-band paste flow (`manual=1`): Claude Code's
                // OAuth client does not accept this server's loopback callback as
                // a redirect_uri, so the seamless path stays dormant. See
                // claude_login.rs.
                form action="/dashboard/admin/claude-login/start" method="post" class="inline-form" {
                    input type="hidden" name="manual" value="1";
                    button type="submit" { "Log in with Claude Code" }
                }
            },
        }
    }
}

/// Whole minutes until `expires_at` (unix seconds), saturating at 0 when
/// already past — for the login panel's "valid ~N min" hint.
fn remaining_minutes(expires_at: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    expires_at.saturating_sub(now) / 60
}

/// One role card: title + guidance, a provider `<select>` (gated client-
/// side), a free-text model combobox (datalist suggestions from the
/// catalog), a JS-filled metadata strip + auth warning, and an Advanced
/// `<details>` for the temperature / max-tokens / reasoning / base-URL
/// knobs. The `api_key_env` is no longer an operator-edited column — it is
/// derived from the chosen provider on save ([`derive_api_key_env`]).
fn role_row(guide: &RoleGuide, cfg: Option<&LlmFunctionConfig>) -> Markup {
    let key = guide.slot.yaml_key();
    let backend = cfg.map_or("", |c| c.backend.as_str());
    let model = cfg.map_or("", |c| c.model.as_str());
    let temperature = cfg
        .and_then(|c| c.temperature)
        .map(|v| format!("{v}"))
        .unwrap_or_default();
    let max_tokens = cfg
        .and_then(|c| c.max_tokens)
        .map(|v| format!("{v}"))
        .unwrap_or_default();
    let reasoning_effort = cfg
        .and_then(|c| c.reasoning_effort.as_deref())
        .unwrap_or("");
    let base_url = cfg.and_then(|c| c.base_url.as_deref()).unwrap_or("");
    html! {
        div data-role-row style=(CARD_STYLE) {
            div style="display:flex;align-items:baseline;justify-content:space-between;gap:.6rem;flex-wrap:wrap" {
                strong { (guide.title) }
                code.muted style="font-size:.72rem" { (key) }
            }
            p.muted style="margin:.15rem 0 .55rem;font-size:.8rem" {
                (guide.blurb) " " span style="color:var(--amber)" { (guide.tier) }
            }
            div style="display:flex;gap:.8rem;flex-wrap:wrap;align-items:flex-end" {
                label style="flex:1 1 11rem;display:flex;flex-direction:column;gap:.2rem" {
                    span.muted style=(FIELD_LABEL_STYLE) { "Provider" }
                    select name=(format!("{key}__backend")) data-role-provider style=(INPUT_STYLE) {
                        option value="" selected[backend.is_empty()] { "— disabled —" }
                        @for p in PROVIDERS {
                            option value=(p.tag) selected[p.tag == backend] { (p.label) }
                        }
                    }
                }
                label style="flex:2 1 17rem;display:flex;flex-direction:column;gap:.2rem" {
                    span.muted style=(FIELD_LABEL_STYLE) { "Model" }
                    input type="text" name=(format!("{key}__model")) value=(model)
                        data-role-model list=(model_list_for(backend))
                        placeholder="e.g. claude-sonnet-4-6 · gemini-3-pro-preview · qwen3.5:9b-q8_0"
                        style=(INPUT_STYLE);
                }
            }
            p data-role-meta class="muted" style="margin:.35rem 0 0;font-size:.75rem;min-height:1.1em" {}
            p data-role-warn style="margin:.2rem 0 0;font-size:.76rem;color:var(--rose);display:none" {
                "⚠ This provider has no valid key — set one in the «Providers & credentials» section above."
            }
            details style="margin-top:.5rem" {
                summary style="cursor:pointer;font-size:.78rem;color:var(--p-dim)" { "Advanced" }
                div style="display:flex;gap:.7rem;flex-wrap:wrap;margin-top:.5rem;align-items:flex-end" {
                    (adv_field(&format!("{key}__temperature"), "Temperature", &temperature, "default", "5.5rem"))
                    (adv_field(&format!("{key}__max_tokens"), "Max tokens", &max_tokens, "default", "6.5rem"))
                    (adv_select_effort(&format!("{key}__reasoning_effort"), reasoning_effort))
                    (adv_field(&format!("{key}__base_url"), "Base URL", base_url, "(default)", "14rem"))
                }
                p.muted style="margin:.4rem 0 0;font-size:.72rem" {
                    "Empty = the model's own default. Gemini ignores temperature; max tokens varies by model/role. "
                    "Base URL matters mostly for a remote Ollama."
                }
            }
        }
    }
}

/// One Advanced text field (temperature / max-tokens / base-URL).
fn adv_field(name: &str, label: &str, value: &str, placeholder: &str, width: &str) -> Markup {
    html! {
        label style=(format!("display:flex;flex-direction:column;gap:.18rem;flex:0 0 {width}")) {
            span.muted style=(FIELD_LABEL_STYLE) { (label) }
            input type="text" name=(name) value=(value) placeholder=(placeholder) style=(INPUT_STYLE);
        }
    }
}

/// The Advanced reasoning-effort `<select>`.
fn adv_select_effort(name: &str, current: &str) -> Markup {
    html! {
        label style="display:flex;flex-direction:column;gap:.18rem;flex:0 0 8rem" {
            span.muted style=(FIELD_LABEL_STYLE) { "Reasoning" }
            select name=(name) style=(INPUT_STYLE) {
                option value="" selected[current.is_empty()] { "— none —" }
                @for e in REASONING_EFFORTS {
                    option value=(e) selected[*e == current] { (e) }
                }
            }
        }
    }
}

/// The `<datalist>` id backing a backend's model suggestions, or `""` for
/// the local backend (free-text only).
fn model_list_for(backend: &str) -> &'static str {
    match backend {
        "anthropic" => "models-anthropic",
        "gemini" => "models-gemini",
        "openai" => "models-openai",
        "openrouter" => "models-openrouter",
        _ => "",
    }
}

// ---------- POST /admin/llm-config ----------

/// Persist a parsed [`LlmConfig`] and hot-reload it. Re-loads the on-disk
/// `Config` raw (preserving every non-`llm` section exactly as the file
/// has it — env overrides are runtime-only and must never ride a save to
/// disk), backs up the live YAML, atomic-writes, then swaps the
/// in-memory copy. Note the `llm` section itself is what-you-see-is-
/// what-you-save: the form renders the runtime snapshot, so values an
/// operator saw and saved are persisted deliberately. Shared by [`save`]
/// and [`apply_profile`]. Order matters — disk first, so a crash after
/// the swap still boots with the new YAML.
fn persist_llm_config(memory: &MemoryHandles, parsed: LlmConfig) -> Result<()> {
    let mut cfg = Config::load_raw(&memory.workdir)
        .map_err(|e| DashboardError::Internal(format!("config load: {e}")))?;
    cfg.llm = parsed.clone();

    let path = config_path_in(memory);
    let backup = backup_path_for(&path);
    match fs::read(&path) {
        Ok(bytes) => atomic_write(&backup, &bytes)
            .map_err(|e| DashboardError::Internal(format!("backup: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(DashboardError::Internal(format!("read for backup: {e}"))),
    }

    let yaml = serde_yaml::to_string(&cfg)
        .map_err(|e| DashboardError::Internal(format!("serialize config: {e}")))?;
    atomic_write(&path, yaml.as_bytes())
        .map_err(|e| DashboardError::Internal(format!("write config: {e}")))?;

    memory.replace_llm_config(parsed);
    Ok(())
}

async fn save(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<HashMap<String, String>>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let parsed = parse_form_into_llm_config(&form, &memory.llm_config_snapshot())?;
    persist_llm_config(memory, parsed)?;

    tracing::info!(
        admin = %admin.session().sender_id,
        "llm-config: saved llm roles from dashboard (hot-reloaded)"
    );

    let onboarding = in_onboarding(&state, &admin).await?;
    let body = render(
        chrome,
        admin.session(),
        memory,
        Some(Flash {
            kind: "success",
            msg: "LLM configuration saved and hot-reloaded — the next request uses the new values.",
        }),
        onboarding,
    );
    Ok(Html(body).into_response())
}

/// Decode the flat form into a fresh `LlmConfig`. An empty `backend`
/// for a slot means "remove the slot from config"; otherwise the
/// slot is materialised with whatever fields the form carries.
/// Numeric parses are lenient — empty string → `None`, malformed →
/// hard 422 with the field name, so a typo never silently sets the
/// wrong default.
fn parse_form_into_llm_config(
    form: &HashMap<String, String>,
    prior: &LlmConfig,
) -> Result<LlmConfig> {
    // Preserve `profile` from the previous config — the dashboard
    // does not surface it as a per-save field (a future revision could).
    let mut out = LlmConfig {
        profile: prior.profile.clone(),
        ..LlmConfig::default()
    };

    // The Anthropic auth mode is a page-level radio in the credential card
    // (associated with this form via `form="llm-roles"`): it decides whether
    // Anthropic roles authenticate with the API key or the Claude Code login
    // store. `api_key_env` is derived from it, never an operator-edited field.
    let anthropic_login = form.get("anthropic_auth_mode").map(String::as_str) == Some("login");

    for slot in SLOTS {
        let key = slot.yaml_key();
        let backend = trimmed_field(form, key, "backend");
        if backend.is_empty() {
            // Role disabled — leave the `Option<...>` as None.
            continue;
        }
        if !ALLOWED_BACKENDS.contains(&backend.as_str()) {
            return Err(DashboardError::Validation(format!(
                "role `{key}`: backend `{backend}` is not supported (allowed: {})",
                ALLOWED_BACKENDS.join(", "),
            )));
        }
        let model = trimmed_field(form, key, "model");
        if model.is_empty() {
            return Err(DashboardError::Validation(format!(
                "role `{key}`: a model is required when the provider is set"
            )));
        }
        // Derive the API-key env-var from the chosen provider (the old
        // per-role column is gone): the well-known key per cloud backend,
        // the Claude Code login sentinel for Anthropic in login mode, or
        // none for the local backend.
        let api_key_env = derive_api_key_env(&backend, anthropic_login);
        let base_url = optional_field(form, key, "base_url");
        let reasoning_effort = optional_field(form, key, "reasoning_effort");
        let temperature = parse_optional_f32(form, key, "temperature")?;
        let max_tokens = parse_optional_u32(form, key, "max_tokens")?;
        let cfg = LlmFunctionConfig {
            backend,
            model,
            api_key_env,
            base_url,
            reasoning_effort,
            temperature,
            max_tokens,
        };
        *slot_mut(&mut out, *slot) = Some(cfg);
    }

    Ok(out)
}

/// Map a chosen provider tag to the `api_key_env` to persist. Cloud
/// backends get their well-known env-var (Anthropic the Claude Code login
/// sentinel when `anthropic_login`); the local backend gets `None`.
fn derive_api_key_env(backend: &str, anthropic_login: bool) -> Option<String> {
    match backend {
        "anthropic" if anthropic_login => Some(mwe_core::oauth::CLAUDE_CODE_LOGIN.to_owned()),
        "anthropic" => Some("ANTHROPIC_API_KEY".to_owned()),
        "gemini" => Some("GEMINI_API_KEY".to_owned()),
        "openai" => Some("OPENAI_API_KEY".to_owned()),
        "openrouter" => Some("OPENROUTER_API_KEY".to_owned()),
        _ => None,
    }
}

#[allow(deprecated, reason = "Cronista arm kept for YAML backward compat")]
const fn slot_mut(llm: &mut LlmConfig, slot: LlmFunction) -> &mut Option<LlmFunctionConfig> {
    match slot {
        LlmFunction::HubWriter => &mut llm.hub_writer,
        LlmFunction::Ingest => &mut llm.ingest,
        LlmFunction::OperatorChat => &mut llm.operator_chat,
        LlmFunction::RemPromotions => &mut llm.rem_promotions,
        LlmFunction::RemDedupSemantic => &mut llm.rem_dedup_semantic,
        LlmFunction::Cronista => &mut llm.cronista,
        LlmFunction::Navigator => &mut llm.navigator,
    }
}

fn trimmed_field(form: &HashMap<String, String>, slot: &str, field: &str) -> String {
    form.get(&format!("{slot}__{field}"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn optional_field(form: &HashMap<String, String>, slot: &str, field: &str) -> Option<String> {
    let v = trimmed_field(form, slot, field);
    if v.is_empty() { None } else { Some(v) }
}

fn parse_optional_f32(
    form: &HashMap<String, String>,
    slot: &str,
    field: &str,
) -> Result<Option<f32>> {
    let raw = trimmed_field(form, slot, field);
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<f32>().map(Some).map_err(|_| {
        DashboardError::Validation(format!(
            "slot `{slot}`: field `{field}` must be a number (got `{raw}`)"
        ))
    })
}

fn parse_optional_u32(
    form: &HashMap<String, String>,
    slot: &str,
    field: &str,
) -> Result<Option<u32>> {
    let raw = trimmed_field(form, slot, field);
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u32>().map(Some).map_err(|_| {
        DashboardError::Validation(format!(
            "slot `{slot}`: field `{field}` must be a positive integer (got `{raw}`)"
        ))
    })
}

// ---------- POST /admin/api-keys/:name ----------

#[derive(Debug, serde::Deserialize)]
struct ApiKeySubmission {
    value: String,
}

async fn set_api_key(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
    HtmlForm(form): HtmlForm<ApiKeySubmission>,
) -> Result<Response> {
    let memory = require_memory(&state)?;

    if !is_valid_env_key_name(&name) {
        return Err(DashboardError::Validation(format!(
            "env-var name `{name}` is not a valid identifier ([A-Z_][A-Z0-9_]*)"
        )));
    }

    let trimmed = form.value.trim();
    if trimmed.is_empty() {
        return Err(DashboardError::Validation(
            "value cannot be empty — use the env file directly to remove a key".to_owned(),
        ));
    }

    env_file::write_key(&env_path_in(memory), &name, trimmed)
        .map_err(|e| DashboardError::Internal(format!("env_file::write_key: {e}")))?;

    // Hot-reload: the env file write above survives restart,
    // but the running process holds whatever the env_loader pushed
    // into std::env at boot — and the crate is forbid_unsafe_code,
    // so we cannot std::env::set_var. The in-memory override map
    // closes the gap: the next backend_for call sees the new value
    // via the closure injected in MemoryHandles::backend_for.
    memory.set_api_key_override(name.clone(), trimmed);

    tracing::info!(
        admin = %admin.session().sender_id,
        env_var = %name,
        "llm-config: api-key set from dashboard (value not logged, hot-reloaded)"
    );

    Ok(Redirect::to("/dashboard/admin/llm-config").into_response())
}

// ---------- POST /admin/ollama-endpoint ----------

/// Body of the Ollama endpoint form.
#[derive(Debug, serde::Deserialize)]
struct OllamaEndpointSubmission {
    endpoint: String,
}

/// Set the provider-level Ollama endpoint (`OLLAMA_BASE_URL`). Mirrors
/// [`set_api_key`]: persisted to the workdir env file and pushed into the
/// override map so the next request uses it without a restart. The
/// per-role `base_url` under «Advanced» still overrides it.
async fn set_ollama_endpoint(
    State(state): State<DashboardState>,
    admin: AdminUser,
    HtmlForm(form): HtmlForm<OllamaEndpointSubmission>,
) -> Result<Response> {
    let memory = require_memory(&state)?;
    let endpoint = form.endpoint.trim();
    if endpoint.is_empty() {
        return Err(DashboardError::Validation(
            "endpoint cannot be empty — keep the localhost default or enter a remote URL"
                .to_owned(),
        ));
    }
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return Err(DashboardError::Validation(
            "endpoint must start with http:// or https://".to_owned(),
        ));
    }
    env_file::write_key(&env_path_in(memory), OLLAMA_BASE_URL_ENV, endpoint)
        .map_err(|e| DashboardError::Internal(format!("env_file::write_key: {e}")))?;
    memory.set_api_key_override(OLLAMA_BASE_URL_ENV.to_owned(), endpoint);
    tracing::info!(
        admin = %admin.session().sender_id,
        endpoint,
        "llm-config: ollama endpoint set from dashboard (hot-reloaded)"
    );
    Ok(Redirect::to("/dashboard/admin/llm-config").into_response())
}

// ---------- GET /admin/ollama-models ----------

/// Proxy the installed-model list from the configured Ollama daemon
/// (`GET /api/tags`) so the page's model field can suggest exact tags.
/// Best-effort: an unreachable daemon yields `{"models":[],"error":…}` and
/// the field stays free-text. Honours the provider-level endpoint +
/// optional Bearer token.
async fn ollama_models(State(state): State<DashboardState>, _admin: AdminUser) -> Result<Response> {
    let memory = require_memory(&state)?;
    let endpoint = current_env_value(memory, OLLAMA_BASE_URL_ENV)
        .unwrap_or_else(|| mwe_core::llm::DEFAULT_OLLAMA_URL.to_owned());
    let token = current_env_value(memory, "OLLAMA_API_KEY");
    let listed = match mwe_core::llm::OllamaBackend::new(&endpoint, "") {
        Ok(backend) => backend.with_bearer(token).list_models().await,
        Err(e) => Err(e),
    };
    let payload = match listed {
        Ok(models) => serde_json::json!({
            "models": models
                .into_iter()
                .map(|m| serde_json::json!({ "name": m.name, "size": m.size }))
                .collect::<Vec<_>>(),
        }),
        Err(e) => {
            tracing::debug!(error = %e, endpoint, "ollama-models: list failed (returning empty)");
            serde_json::json!({ "models": [], "error": e.to_string() })
        },
    };
    Ok(Json(payload).into_response())
}

// ---------- POST /admin/llm-config/profile/:name ----------

/// Apply a preset profile (`all-local` / `hybrid` / `all-api`) to every
/// role in one click, then persist + hot-reload — the operator fine-tunes
/// from there.
async fn apply_profile(
    State(state): State<DashboardState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let profile = mwe_core::config::LlmProfile::parse(&name)
        .map_err(|bad| DashboardError::Validation(format!("unknown profile: `{bad}`")))?;
    if matches!(profile, mwe_core::config::LlmProfile::Custom) {
        return Err(DashboardError::Validation(
            "the `custom` profile does not fill the roles — configure them manually".to_owned(),
        ));
    }
    persist_llm_config(memory, profile.build())?;
    tracing::info!(
        admin = %admin.session().sender_id,
        profile = %name,
        "llm-config: applied preset profile (hot-reloaded)"
    );
    let onboarding = in_onboarding(&state, &admin).await?;
    let body = render(
        chrome,
        admin.session(),
        memory,
        Some(Flash {
            kind: "success",
            msg: "Profile applied to every role — fine-tune from here.",
        }),
        onboarding,
    );
    Ok(Html(body).into_response())
}

// ---------- POST /admin/llm-catalog/refresh ----------

/// Re-fetch the models.dev catalog into the workdir cache so the picker
/// reflects upstream additions. Best-effort — a failure keeps the bundled
/// snapshot serving.
async fn refresh_catalog(
    State(state): State<DashboardState>,
    admin: AdminUser,
) -> Result<Response> {
    let chrome = layout::Chrome::of(&state);
    let memory = require_memory(&state)?;
    let onboarding = in_onboarding(&state, &admin).await?;
    let body = match model_catalog::refresh(&memory.workdir).await {
        Ok(n) => render(
            chrome,
            admin.session(),
            memory,
            Some(Flash {
                kind: "success",
                msg: &format!("Model catalogue updated — {n} models."),
            }),
            onboarding,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "llm-catalog: refresh failed");
            render(
                chrome,
                admin.session(),
                memory,
                Some(Flash {
                    kind: "error",
                    msg: "Catalogue update failed — the bundled snapshot stays in use.",
                }),
                onboarding,
            )
        },
    };
    Ok(Html(body).into_response())
}

/// Catalog size + a refresh button, shown above the roles.
fn catalog_refresh(catalog: &Catalog) -> Markup {
    html! {
        div style="display:flex;align-items:center;gap:.5rem;flex-wrap:wrap;margin:0 0 .8rem" {
            span.muted style="font-size:.75rem" {
                "Model catalogue (models.dev): " (catalog.len()) " models."
            }
            form action="/dashboard/admin/llm-catalog/refresh" method="post" class="inline-form" {
                button type="submit" { "Refresh catalogue" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_returns_last_n_chars() {
        assert_eq!(fingerprint("sk-ant-abcdef", 4), "cdef");
        assert_eq!(fingerprint("xy", 4), "xy");
        assert_eq!(fingerprint("", 4), "");
    }

    #[test]
    fn is_valid_env_key_name_accepts_canonical_uppercase_with_digits() {
        assert!(is_valid_env_key_name("ANTHROPIC_API_KEY"));
        assert!(is_valid_env_key_name("_X"));
        assert!(is_valid_env_key_name("OPENAI_API_KEY_2"));
    }

    #[test]
    fn is_valid_env_key_name_rejects_lowercase_or_special() {
        assert!(!is_valid_env_key_name(""));
        assert!(!is_valid_env_key_name("anthropic"));
        assert!(!is_valid_env_key_name("KEY=2"));
        assert!(!is_valid_env_key_name("KEY.SUB"));
        assert!(!is_valid_env_key_name("3KEYS"));
    }

    #[test]
    fn read_env_file_parses_quoted_and_unquoted_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        std::fs::write(
            &path,
            "# comment\n\
             ANTHROPIC_API_KEY=\"sk-ant-fake\"\n\
             OPENAI_API_KEY=raw-value\n\
             # ANTHROPIC_API_KEY  hint line — must be ignored\n\
             \n",
        )
        .expect("seed");
        let values = read_env_file(&path);
        assert_eq!(
            values.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-ant-fake")
        );
        assert_eq!(
            values.get("OPENAI_API_KEY").map(String::as_str),
            Some("raw-value")
        );
        // Commented hint line not parsed as an assignment.
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn read_env_file_unescapes_backslash_and_quote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mwe-mcp.env");
        std::fs::write(&path, "WEIRD=\"has \\\"quote\\\" and \\\\back\"\n").expect("seed");
        let values = read_env_file(&path);
        assert_eq!(
            values.get("WEIRD").map(String::as_str),
            Some("has \"quote\" and \\back")
        );
    }

    #[test]
    fn parse_form_with_empty_backend_omits_slot() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), String::new());
        let prior = LlmConfig::default();
        let parsed = parse_form_into_llm_config(&form, &prior).expect("parse");
        assert!(parsed.ingest.is_none());
        assert!(parsed.hub_writer.is_none());
    }

    #[test]
    fn parse_form_rejects_unsupported_backend() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "google".into());
        form.insert("ingest__model".into(), "gemini".into());
        let prior = LlmConfig::default();
        let err = parse_form_into_llm_config(&form, &prior).expect_err("rejects");
        match err {
            DashboardError::Validation(msg) => {
                assert!(msg.contains("google"), "{msg}");
                assert!(msg.contains("ingest"), "{msg}");
            },
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Anthropic in the default (key) mode derives `ANTHROPIC_API_KEY` —
    /// the operator no longer picks the env-var per role.
    #[test]
    fn parse_form_derives_anthropic_api_key_in_key_mode() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "anthropic".into());
        form.insert("ingest__model".into(), "claude-sonnet-4-6".into());
        let parsed = parse_form_into_llm_config(&form, &LlmConfig::default()).expect("parse");
        assert_eq!(
            parsed
                .ingest
                .as_ref()
                .expect("ingest")
                .api_key_env
                .as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
    }

    /// With the credential card's auth mode set to "login", Anthropic roles
    /// derive the Claude Code login sentinel instead of the key env-var.
    #[test]
    fn parse_form_anthropic_login_mode_derives_claude_code() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("anthropic_auth_mode".into(), "login".into());
        form.insert("ingest__backend".into(), "anthropic".into());
        form.insert("ingest__model".into(), "claude-opus-4-8".into());
        let parsed = parse_form_into_llm_config(&form, &LlmConfig::default()).expect("parse");
        assert_eq!(
            parsed
                .ingest
                .as_ref()
                .expect("ingest")
                .api_key_env
                .as_deref(),
            Some(mwe_core::oauth::CLAUDE_CODE_LOGIN)
        );
    }

    /// Gemini and `OpenRouter` derive their well-known key env-vars.
    #[test]
    fn parse_form_derives_gemini_and_openrouter_keys() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "gemini".into());
        form.insert("ingest__model".into(), "gemini-3-flash-preview".into());
        form.insert("navigator__backend".into(), "openrouter".into());
        form.insert(
            "navigator__model".into(),
            "anthropic/claude-haiku-4-5".into(),
        );
        let parsed = parse_form_into_llm_config(&form, &LlmConfig::default()).expect("parse");
        assert_eq!(
            parsed
                .ingest
                .as_ref()
                .expect("ingest")
                .api_key_env
                .as_deref(),
            Some("GEMINI_API_KEY")
        );
        assert_eq!(
            parsed
                .navigator
                .as_ref()
                .expect("navigator")
                .api_key_env
                .as_deref(),
            Some("OPENROUTER_API_KEY")
        );
    }

    /// A gemini role round-trips through the parser, with `api_key_env`
    /// derived (the form no longer carries it).
    #[test]
    fn parse_form_round_trips_gemini_slot() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "gemini".into());
        form.insert("ingest__model".into(), "gemini-3-flash-preview".into());
        let prior = LlmConfig::default();
        let parsed = parse_form_into_llm_config(&form, &prior).expect("parse");
        let ingest = parsed.ingest.as_ref().expect("ingest");
        assert_eq!(ingest.backend, "gemini");
        assert_eq!(ingest.model, "gemini-3-flash-preview");
        assert_eq!(ingest.api_key_env.as_deref(), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn parse_form_rejects_anthropic_with_missing_model() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "anthropic".into());
        let prior = LlmConfig::default();
        let err = parse_form_into_llm_config(&form, &prior).expect_err("rejects");
        match err {
            DashboardError::Validation(msg) => {
                assert!(msg.contains("a model is required"), "{msg}");
            },
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_form_round_trips_full_slot() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("hub_writer__backend".into(), "anthropic".into());
        form.insert("hub_writer__model".into(), "claude-opus-4-7".into());
        form.insert("hub_writer__temperature".into(), "0.4".into());
        form.insert("hub_writer__max_tokens".into(), "2048".into());
        form.insert("hub_writer__reasoning_effort".into(), "extra-high".into());
        form.insert("hub_writer__base_url".into(), "https://example.test".into());
        let prior = LlmConfig::default();
        let parsed = parse_form_into_llm_config(&form, &prior).expect("parse");
        let hw = parsed.hub_writer.as_ref().expect("hub_writer");
        assert_eq!(hw.backend, "anthropic");
        assert_eq!(hw.model, "claude-opus-4-7");
        // `api_key_env` is derived from the provider, not the form.
        assert_eq!(hw.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(hw.temperature, Some(0.4));
        assert_eq!(hw.max_tokens, Some(2048));
        assert_eq!(hw.reasoning_effort.as_deref(), Some("extra-high"));
        assert_eq!(hw.base_url.as_deref(), Some("https://example.test"));
    }

    /// A role card renders the four provider options (the selected one
    /// marked), a free-text model combobox bound to the catalog datalist,
    /// and the JS hooks the page logic depends on.
    #[test]
    fn role_row_renders_providers_and_model_combobox() {
        let cfg = LlmFunctionConfig {
            backend: "openrouter".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            api_key_env: Some("OPENROUTER_API_KEY".into()),
            base_url: None,
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
        };
        let html = role_row(&ROLE_GUIDES[0], Some(&cfg)).into_string();
        assert!(html.contains(">OpenRouter<"), "{html}");
        assert!(html.contains(">Ollama (local)<"), "{html}");
        assert!(html.contains("value=\"openrouter\" selected"), "{html}");
        assert!(html.contains("list=\"models-openrouter\""), "{html}");
        assert!(html.contains("anthropic/claude-sonnet-4-6"), "{html}");
        assert!(html.contains("data-role-provider"), "{html}");
        assert!(html.contains("data-role-model"), "{html}");
    }

    #[test]
    fn parse_form_preserves_profile_from_prior() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "ollama".into());
        form.insert("ingest__model".into(), "qwen3.5:9b-q8_0".into());
        let prior = LlmConfig {
            profile: Some("hybrid".into()),
            ..LlmConfig::default()
        };
        let parsed = parse_form_into_llm_config(&form, &prior).expect("parse");
        assert_eq!(parsed.profile.as_deref(), Some("hybrid"));
    }

    #[test]
    fn parse_form_rejects_malformed_temperature() {
        let mut form: HashMap<String, String> = HashMap::new();
        form.insert("ingest__backend".into(), "ollama".into());
        form.insert("ingest__model".into(), "qwen3.5-9b".into());
        form.insert("ingest__temperature".into(), "not-a-number".into());
        let prior = LlmConfig::default();
        let err = parse_form_into_llm_config(&form, &prior).expect_err("rejects");
        match err {
            DashboardError::Validation(msg) => {
                assert!(msg.contains("temperature"), "{msg}");
            },
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
