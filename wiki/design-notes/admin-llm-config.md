---
title: Admin LLM config + API key editor
area: design-notes
status: implemented
last_review: "2026-06-24"
---

# Admin LLM config + API key editor

The dashboard ships an admin-only page that configures the `llm:` section of
`<workdir>/mwe-mcp.config.yaml`, the API key env-vars in
`<workdir>/mwe-mcp.env`, and the Claude Code login store — all from the
browser, hot-reloaded into the running server. Without it the operator would
SSH in, hand-edit YAML, and restart every time they swapped a model or
rotated a key.

The page is **credentials-first**: section 1 (*Providers & credentials*)
holds the keys and login; section 2 (*Roles*) assigns each LLM function a
provider + model with plain-language guidance. A role can only sensibly use
a provider whose credentials are in place — that ordering is the redesign's
core idea.

## Routes

The page's routes, all behind [`AdminUser`]:

| Method | Path | Purpose |
|---|---|---|
| GET  | `/dashboard/admin/llm-config`               | Render the page. |
| POST | `/dashboard/admin/llm-config`               | Atomic save of the `llm:` section, `.bak` backup, hot-reload. |
| POST | `/dashboard/admin/llm-config/profile/:name` | Apply a preset `LlmProfile` (`all-local` / `hybrid` / `all-api`) to every role. |
| POST | `/dashboard/admin/api-keys/:name`           | Upsert a key env-var in `mwe-mcp.env` via `mwe_core::env_file::write_key`. |
| POST | `/dashboard/admin/llm-catalog/refresh`      | Re-fetch the models.dev catalog into the workdir cache. |
| POST | `/dashboard/admin/ollama-endpoint`          | Set the provider-level Ollama endpoint (`OLLAMA_BASE_URL`) — same persist + hot-reload path as a key. |
| GET  | `/dashboard/admin/ollama-models`            | Proxy the daemon's installed tags (`/api/tags`) as JSON for the model picker; best-effort, honours the endpoint + Bearer. |

The router lives in [`crates/mwe-dashboard/src/routes/llm_config.rs`](../../crates/mwe-dashboard/src/routes/llm_config.rs);
the Claude Code OAuth routes live in
[`routes::claude_login`](../../crates/mwe-dashboard/src/routes/claude_login.rs)
and are merged alongside.

## Onboarding step 1

This page doubles as **step 1 of first-run onboarding**. The `/dashboard/setup`
POST lands the freshly-created admin here (not on the profile primer at
`/dashboard/welcome`), because that primer calls `wiki_ingest_message` and needs
a usable `ingest` model first. While the admin's `profile_initialized` flag is
still `0`, the GET page prepends a step-1 banner with a *Continue to profile
setup →* link that goes live only once the `ingest` role resolves to a usable
backend — the local backend, or a cloud provider whose key/login is present (a
config-level check, no network probe; a selected Ollama counts). Once the
profile is initialized the banner disappears and the page is the normal admin
editor. The flow is documented in
[setup-and-identity.md](setup-and-identity.md#first-login-profile-wizard).

## Section 1 — providers & credentials

One card per backend, in display order: **Ollama** (local or remote),
**Anthropic**, **Google Gemini**, **OpenRouter**. Each cloud card shows the
key status — a `set` / `no key` badge, a 4-char fingerprint of
the value's last characters, and the origin (`live override` / `shell` /
`env file`) — and a one-input `type=password` form that POSTs to
`/admin/api-keys/:name`. The well-known list (`WELL_KNOWN_KEYS`) renders one
key per cloud backend even when no role references it, so the operator can
pre-load a key before assigning the provider to a role.

The cleartext key **never leaves the server**: no read endpoint returns it,
and the render only ever embeds the fingerprint. Rotating means re-entering
the whole new value. The env-file writer preserves chmod `0o600` on unix
even if the file existed with a more permissive mode beforehand (on non-unix
platforms the mode call is a no-op).

**Anthropic auth mode.** The Anthropic card carries a radio toggle — *API
key* vs *Claude Code login (personal)* — whose radios are associated with
the roles form via `form="llm-roles"`. The choice decides how every Anthropic
role authenticates and is **derived** into `api_key_env` on save (see
[Section 2](#section-2--roles)), never edited per-role. Selecting *Claude
Code login* reveals the login controls (see
[Claude Code login](#claude-code-login)).

**Ollama endpoint + optional auth.** Ollama is the one card that is not
key-only: a local daemon needs no credential, but a remote or cloud one does.
The card carries an **Endpoint** field (`OLLAMA_BASE_URL`, default
`http://localhost:11434`, posted to `/admin/ollama-endpoint`) and an *optional*
Bearer token (`OLLAMA_API_KEY`) for Ollama Cloud or a daemon behind an
authenticating proxy. Both are provider-level; the runtime resolves the
endpoint as **per-role `base_url` («Advanced») > `OLLAMA_BASE_URL` > the
localhost default**, and the token, when set, rides every request as
`Authorization: Bearer …`. The endpoint persists + hot-reloads exactly like a
key (it just renders in full, not fingerprinted). The model field then suggests
the daemon's installed tags — see [Section 2](#section-2--roles).

## Section 2 — roles

One card per LLM slot — **every**
[`LlmFunction`](../../crates/mwe-core/src/config.rs) variant: `ingest`,
`hub_writer`, `operator_chat`, `rem_promotions`, `cronista`, `navigator`,
`rem_dedup_semantic`, in that UX order. `cronista` (the narrative prose
compiler, invoked by the dream compile pass) is surfaced like every other
slot: `save()` rebuilds the config from exactly this card set, so leaving
`cronista` off the list silently dropped the prose compiler on every save —
a fresh setup then captured facts but never rendered readable pages. It
keeps its `#[deprecated]` marker (pending graduation to a full REM sub-job)
but is configured normally. `operator_chat` is the dashboard's operational
agentic-chat slot; unlike the others, leaving it **disabled is benign** —
the chat falls back to `hub_writer` (`backend_for_chat`), so the card is
how an operator *opts in* to a stronger chat model, not a slot that breaks
when unset. Each card pairs a one-line description
with a **capability-tier hint** (the operator-facing guidance that is the
point of the redesign: "strong — not the 9B", "workhorse — a local 9B is
fine"), then the controls:

- **Provider** — `<select>` over the four
  [`ALLOWED_BACKENDS`](../../crates/mwe-dashboard/src/routes/llm_config.rs)
  (`ollama` / `anthropic` / `gemini` / `openrouter`), or "— disabled —"
  (empty) to drop the slot from config. A role on a provider without usable
  auth shows an inline warning (filled client-side from the embedded
  per-provider auth map).
- **Model** — a free-text combobox (`<input list=…>`) bound to a
  `<datalist>` of suggestions; any id can still be typed. For a cloud provider
  the suggestions come from the [models.dev catalog](#modelsdev-catalog), with a
  JS-filled metadata strip under it (context window, $/M in/out cost, vision /
  tools / reasoning badges). For **Ollama** the catalog has no entries, so the
  JS fetches the daemon's installed tags from `/admin/ollama-models` and fills a
  shared `#models-ollama` datalist (with size hints) — exact names, no guessing.
  Best-effort: an unreachable daemon just leaves the field free-text.
- **Advanced** (a native `<details>`) — `temperature`, `max_tokens`,
  `reasoning_effort`, `base_url`. Empty means the model's own default (no
  misleading placeholder values); see [advanced knobs](#advanced-knobs).

`api_key_env` is **not an editable column** — it is derived from the
chosen provider on save by `derive_api_key_env`: `ANTHROPIC_API_KEY` /
`GEMINI_API_KEY` / `OPENROUTER_API_KEY` for the cloud backends (or the
`claude-code` login sentinel for Anthropic when the card is in login mode),
`None` for Ollama.

A **profile quick-set** row above the roles applies a whole preset
([`LlmProfile::build`](../../crates/mwe-core/src/config.rs)) — *All local*
/ *Hybrid* / *All API* — filling every role in one POST, after which the
operator fine-tunes.

### Advanced knobs

- **temperature** (`Option<f32>`) / **max_tokens** (`Option<u32>`) — per-slot
  defaults applied by
  [`LlmFunctionConfig::apply_defaults_to_completion`](../../crates/mwe-core/src/config.rs)
  / `apply_defaults_to_chat` only when the call site leaves the request field
  unset. Call sites that pin a value (`ingest` at 0.1, the REM revisor at
  0.1) are unaffected; the agentic chat loop is the one that visibly picks
  the default up. Gemini ignores temperature (it pins 1.0 at the boundary).
- **reasoning_effort** — `low` / `medium` / `high` / `extra-high`. The
  per-backend mapping (Anthropic `budget_tokens`, Gemini `thinkingLevel`,
  OpenRouter `reasoning.effort`, Ollama ignores it) is documented as current
  state in [config-schema.md](../protocol/config-schema.md). Gemini 3.x
  **Pro** rejects `minimal`, so a Pro slot must carry a non-minimal effort or
  it fails the boot health-check.
- **base_url** — optional endpoint override; matters mostly for a remote /
  custom-port Ollama.

## models.dev catalog

The model combobox is backed by
[`mwe_core::model_catalog`](../../crates/mwe-core/src/model_catalog.rs), a
compact projection of [models.dev](https://models.dev) — per-model metadata
(context window, per-million-token cost, vision / tools / reasoning flags)
for the three cloud backends (`anthropic`, `google` → the `gemini` tag,
`openrouter`). A filtered snapshot is **vendored** at
`crates/mwe-core/assets/model-catalog.json` and embedded at compile time, so
the picker works fully offline. `POST /admin/llm-catalog/refresh` re-fetches
the live `api.json` into `<workdir>/model-catalog.json`, which `load` prefers
over the bundled copy — a model added upstream then shows up without a
rebuild. Ollama is absent by design (its installed models live on the running
server and the registry is not listable), so the Ollama model field stays
free-text.

The catalog + a per-provider usable-auth map are embedded in the page as an
inline JSON blob (`#llm-config-data`) that
[`llm-config.js`](../../crates/mwe-dashboard/assets/llm-config.js) reads to
drive the datalist swap, the metadata strip, and the auth warning. Without
JS the inputs are plain text and the credential forms still post — the page
degrades cleanly.

## Save discipline

`save` and `apply_profile` share `persist_llm_config`:

1. Load the existing `Config` so unrelated sections survive (`logging:`,
   `rem:`, the `serde(flatten) extra` mapping).
2. Replace only `cfg.llm` with the freshly parsed roles (or the profile).
3. Copy the prior `mwe-mcp.config.yaml` to `mwe-mcp.config.yaml.bak`
   (single backup, overwritten each save).
4. Serialize with `serde_yaml::to_string` and atomic-write via
   [`mwe_core::wiki::atomic_write`](../../crates/mwe-core/src/wiki.rs)
   (temp + fsync + rename + dir-fsync).
5. Hot-reload (swap the in-memory copy — see [Hot-reload](#hot-reload)).

**Operator comments are flattened on save** — `serde_yaml` has no comment
retention, so any `# this slot is the workhorse` narrative disappears the
first time the dashboard rewrites the file. The disclaimer is on the page;
the `.bak` is the recovery path.

## Claude Code login

The Anthropic card's *Login Claude Code* mode drives the test/personal path
that signs `anthropic` roles with the operator's own Claude subscription
instead of a Console API key — the UI half of
[`mwe_core::oauth`](../../crates/mwe-core/src/oauth.rs); the routes live in
[`routes::claude_login`](../../crates/mwe-dashboard/src/routes/claude_login.rs)
and the credential mechanics + token transport are documented as current
state in
[config-schema.md](../protocol/config-schema.md#anthropic-claude-code--oauth-auth).

- The login block shows the state from the process-wide login store (logged
  in + the access token's remaining minutes, or not logged in), with
  **Log in with Claude Code** / **Log out**.
- **Out-of-band paste flow.** **Log in with Claude Code** posts `manual=1`, so
  the page renders the authorize link plus a paste box; the operator approves
  on Claude, copies the `code#state` blob the out-of-band page shows, and
  pastes it back. The `redirect_uri` is `oauth::OOB_REDIRECT_URI`
  (`https://console.anthropic.com/oauth/code/callback`) — the one Claude Code's
  OAuth client accepts. A single-slot `PendingClaudeLogin` on `DashboardState`
  carries the PKCE verifier + CSRF `state` + that `redirect_uri` between the two
  requests.
- **Seamless loopback is dormant.** A second channel in `claude_login.rs` would
  302 the browser back to the server's own callback with no copy-paste, but
  Claude Code's OAuth client **rejects this server's custom loopback
  `redirect_uri`**, so the dashboard never selects it (the button always posts
  `manual=1`). The code is kept intact for a future attempt at a `redirect_uri`
  the client accepts.
- **Boot leniency.** A `claude-code` slot that is not yet authenticated does
  **not** fail the boot LLM health-check (the server warns and skips it) —
  otherwise the dashboard you log in from could never come up. The slot's
  feature stays unavailable until the login lands.

**Test / personal use only** — never a deployed product auth mode; a
deployment brings its own Console keys. Residual work (finding a loopback
`redirect_uri` Claude Code's client accepts so the dormant seamless channel can
wake, refresh-failure re-login UX) is tracked in
[roadmap group 16](../roadmap.md).

## Hot-reload

Both surfaces are hot-reloaded — the admin page banner reads "Le modifiche
vanno in vigore al prossimo messaggio." Mechanism:

- `MemoryHandles.llm_config` lives behind an `Arc<RwLock<LlmConfig>>`. The
  save handler atomic-writes the YAML to disk first, then calls
  [`MemoryHandles::replace_llm_config`](../../crates/mwe-dashboard/src/state.rs)
  to swap the in-memory copy. Subsequent
  [`MemoryHandles::backend_for`](../../crates/mwe-dashboard/src/state.rs) /
  [`defaults_for`](../../crates/mwe-dashboard/src/state.rs) calls take a short
  read-lock and see the new roles.
- API key env-vars are mirrored in `MemoryHandles.api_key_overrides`, an
  `Arc<RwLock<HashMap>>`. `backend_for` drives
  [`LlmFunctionConfig::build_backend_with_env`](../../crates/mwe-core/src/config.rs)
  with a closure that **prefers the override map, then falls back to
  `std::env::var`** — the path that closes the gap
  `#![forbid(unsafe_code)]` would otherwise force (`std::env::set_var` on
  unix is `unsafe`, and the dashboard crate's `forbid_unsafe_code` is not
  negotiable).
- Lookup precedence for a key value is **override → shell → env file**
  (matching `backend_for`'s closure). The credential card surfaces the
  winning layer with an origin label so the admin can tell which value is
  actually in use.

The override map lives in memory only — it does not survive `mwe-mcp serve`
restart. The env-file copy on disk (written by the same handler via
[`mwe_core::env_file::write_key`](../../crates/mwe-core/src/env_file.rs)) is
the restart-survival path: `env_loader::load_workdir_env` pushes those values
into `std::env` on next boot, and the override map is re-populated empty.

The MCP transport (`McpState.llm_config`) is **not** hot-reloaded — it keeps
its own cloned `LlmConfig` from boot. The dashboard chat, proposal apply, and
welcome-wizard call sites all read through `MemoryHandles` and so do get the
live values.

## Validation guards

The editor refuses to persist configurations the runtime would reject:

- **Unknown backend** → 422 with the allowed list. `ollama` / `anthropic` /
  `gemini` / `openrouter` ship; `openai` is parsed by `Config::load` (so an
  operator's existing YAML survives upgrades) but the editor will not commit
  it.
- **Model required** → 422 naming the role when a provider is set without a
  model.

`api_key_env` cannot be missing for a cloud role — it is derived from
the provider, so no "cloud backend without an API-key env-var" save
error exists. The API key endpoint validates the env-var name against
`[A-Z_][A-Z0-9_]*` so a typo in the URL cannot become an arbitrary
assignment.

## Test coverage

- Unit tests in
  [`routes/llm_config.rs`](../../crates/mwe-dashboard/src/routes/llm_config.rs)
  cover the form parser (provider → `api_key_env` derivation in key vs login
  mode, profile preservation, malformed-number rejection), the role-card
  render, the fingerprint helper, the env-key validator, and the env-file
  reader.
- Integration tests in
  [`tests/llm_config.rs`](../../crates/mwe-dashboard/tests/llm_config.rs)
  drive the full axum + sqlite + filesystem stack: anonymous redirect, render
  of the credential cards + role cards, atomic YAML save with `.bak`,
  derived-key save, key upsert + redirect, env-var name validation,
  fingerprint shown without leaking the value, and `0o600` preservation on
  unix.
- The catalog module has its own unit suite in
  [`model_catalog.rs`](../../crates/mwe-core/src/model_catalog.rs) (bundled
  parse, `gemini`→`google` mapping, alias stripping, cache-vs-bundled
  resolution), and the OpenRouter backend is covered by wiremock round-trips
  in [`llm.rs`](../../crates/mwe-core/src/llm.rs).

## Not yet supported

The following extensions are not implemented today (planned — see the
[roadmap](../roadmap.md)):

- Live Ollama model listing (`/api/tags`) in the picker — Ollama stays
  free-text for now.
- A "verify connection" live probe per provider — the page shows passive
  usable-auth status, not an active round-trip test.
- REM scheduler editor (`rem.mode` / `interval_secs` / `initial_delay_secs`).
- Read-only doctor health UI (mirror of `mwe-mcp doctor`).
- A banner that warns when YAML on disk diverges from the loaded `LlmConfig`
  (the hand-edit-between-saves case — hot-reload made it less acute but did
  not eliminate it).
- Extending hot-reload to `McpState.llm_config` on the MCP transport side.
