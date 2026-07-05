# Changelog

All notable changes to **mwe-mcp** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the project is pre-1.0, the public interface (the MCP tool
surface — see [`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md))
may break between minor versions — breaking changes are called out
explicitly.

## [Unreleased]

### Added

- **Bridge distribution from the running server** (roadmap 3i). A running
  mwe-mcp is now the distribution point for its own bridges — no repo
  clone, no manual symlinks. A public, anonymous root surface serves a
  slim **front page** (`GET /`: an agent line → the catalog, a human
  sign-in link), the **bridge catalog** (`GET /bridges`,
  `GET /bridges/<consumer>` — each entry with an *agent instructions*
  link to its `install.md`; the install command tailored to the request
  `Host`), and a **self-contained installer**
  (`GET /bridges/<consumer>/install.{sh,ps1,md}`): the bridge's plugin
  files are embedded in the binary and inlined into the script, so one
  `curl … | sh` from inside the hermes checkout lays everything down. The
  **same** catalog + guide are also a dashboard **Bridges** tab
  (`/dashboard/bridges`), and the dashboard home gained a *Connect a
  consumer* card (MCP URL + issue-token + wire-a-consumer). The **token**
  is issued from that card — never from the bridge pages or the
  installer, which only instruct the operator to set it, disable the
  host's built-in memory, and restart. The standalone admin-only
  `/connect` page was **retired** (its onboarding role moved to the home
  + Bridges tab; the `/connect/hooks/*` bundle endpoints remain).

- **The media pipeline** (roadmap group 12). Photos, video, audio and
  documents become memory without betraying the pillars: a media item
  enters as an ordinary described fact whose body carries a bare
  `{{embed=<catalog_id>}}` key, while everything behind the key — kind,
  MIME, size and the **per-media ACL** — is authoritative in the new
  `media_catalog` table (migration 0039), the twin of `fact_index`;
  bytes live once in a global content-addressed store under
  `<workdir>/media/` (sha256, blob-before-row write order). Entry is
  two-phase: `POST /media` (multipart, the same bearer JWT +
  `X-MWE-Act-As` as `/mcp`, idempotent per-owner dedup) mints the
  `c-YYYY-MM-DD-<kind>-NNN.<ext>` id with the closed English kind
  vocabulary, then the new optional `attachments` array on
  `wiki_ingest_message` links it to the turn. Undescribed photos ride
  the existing ingest LLM call as inline image parts (Gemini
  `inlineData`, Anthropic `image` blocks, Ollama `images`; prompt
  v2.27); a consumer-supplied `description` is trusted instead. The
  classifier claims attachments per extraction; markers are rendered by
  code, claimed media widen their catalog ACL to the fact's read set
  (monotone union), and a deterministic fallback files whatever no plan
  claimed — catalogued media is never dead memory. Exit:
  `GET /media/<catalog_id>` (per-media ACL, strong sha256 ETag,
  inline-safe MIME policy) plus the dashboard's cookie-authenticated
  alias with inline `<img>`/`<video>`/`<audio>` rendering of embeds;
  the export archive bundles referenced blobs under `_media/` with a
  catalog manifest; `wiki_lint` ships the `embed_missing` check. The
  marker grammar legalizes embeds inside region bodies (collected on
  the Region event, no more `NestedRegion` warning) so media travel
  with their facts through page reorganizations. The hermes bridge
  grows a standalone `mwe-media` gateway-hook plugin (opt-in):
  Telegram media → fail-closed sender gate → upload → spool →
  `attachments` on the turn's ingest, closing the host's
  native-image-mode memory bypass.

- **The agent-bridge home (`agents-bridges/`) and the hermes bridge.** Host
  adapters are now in-repo deliverables: an authoring guide, a per-bridge
  `bridge.toml` compat manifest (pinned upstream + per-turn-contract
  version, schema-checked), a two-tier smoke harness (offline against a
  recording stub endpoint; live against a real server), and a separate
  non-blocking CI workflow with a weekly upstream-HEAD canary. The
  per-turn contract in `INTEGRATING.md` is stamped **v1**. The first
  bridge ships with it: a zero-fork **hermes-agent plugin pair**
  (`mwe` memory provider — one mechanical ingest per turn, consumer-owned
  window, per-sender act-as pool, one-way mirror of the built-in memory;
  `mwe-truncate` context engine — bounded window, no summarization pass),
  validated live end-to-end.
- **Per-user (addressed) structure proposals** (migration 0032). A
  `recipient_id` column records who a proposal concerns; REM derives it
  from the triggering fact and carries it on the `StructureProposed` /
  `DedupProposed` event payloads. The dashboard tray,
  `structure_proposal_list`, and `pending_attention` scope to "addressed
  to me or unaddressed" for a non-admin (admins see all); apply / confirm
  / revert are gated to the addressee or an admin.
- **Single-use dashboard magic-link.** `GET /dashboard/auth/link` redeems
  a `dashboard_link` token exactly once (compare-and-set on the `jti` in
  `token_blacklist`), sets the sliding session cookie, and redirects to
  the deep-link. `dashboard_link` URLs now target this endpoint and are
  no longer replayable. Together these wire the per-user proposal
  notification flow: REM event → consumer agent → Telegram → single-use
  dashboard link.

### Changed

- **`mwe-mcp serve` provisions the dedicated-user service for you.** The
  dedicated-user gate (roadmap 14b) used to refuse to boot under a login
  account or root and only *print* the `useradd`/`chown`/`chmod` steps.
  Now, on an interactive terminal, it **offers to set the whole thing
  up**: on confirmation it creates the `mwe-mcp` account, installs the
  binary to `/usr/local/bin/mwe-mcp`, relocates (preserving data) or
  creates and locks the workdir at `/home/mwe-mcp/workdir`, installs the
  `mwe-mcp.service` unit (`User=mwe-mcp`, `Restart=on-failure`,
  `ProtectSystem=strict`, boot-enabled), and `enable --now`s it — then
  hands the port to the service and exits. Each privileged step is shown
  and runs under `sudo`. Declining, or a non-interactive host (systemd,
  CI, container, piped stdin), keeps the printed manual steps.
  And on an interactive **`--bypassdedicateduser`** run under a login
  account — the shape for a box dedicated to mwe-mcp (no co-located
  consumer to wall off) — it likewise offers a restart-on-boot service,
  this one `User=<your login user>` with the bypass baked into `ExecStart`
  and no workdir relocation. Both generated units pin `XDG_CACHE_HOME`
  inside the workdir so the bge-m3 weights (~2.2 GB) download succeeds
  under `ProtectSystem=strict`. Net effect: `mwe-mcp serve` takes a fresh
  operator from a login-account refusal to a running, boot-enabled,
  auto-restarting service in one prompt — co-located *or* standalone.
- **`serve` asks where to listen.** `--bind` / `--port` are now optional;
  on a bare interactive `serve` it asks whether to expose the server to
  other machines (`0.0.0.0`, LAN / port-forwardable) or keep it local
  (`127.0.0.1`, the default) and on which port — mwe-mcp is a server
  multiple consumers reach over HTTP, often from other hosts. The choice
  bakes into the systemd unit when the gate provisions one. Passing either
  flag, or a non-interactive host, skips the prompt and uses the loopback
  defaults. When you do expose it, the endpoint is JWT-gated but plain
  HTTP — put TLS in front (reverse proxy / tunnel) and mint `exposed`
  tokens.

### Removed / Breaking

- **The `wiki_type` registry tools and the runtime type-forge were removed.**
  Concretely: the three MCP tools `wiki_type_register` / `wiki_type_list` /
  `wiki_type_describe` (the tool roster drops 23 → 20); the dashboard **Types** page
  and the chat **forge / schema-evolve** verbs; and the emergent *vertical-genre*
  layer. The bundled templates, the registry table, and the structured routing remain;
  the core `_internal.wiki_type_*` functions still exist server-side.

## [0.2.0] — 2026-05-30

First real release. It back-fills the whole feature set built across
Phase B (memory engine + MVP dashboard) and Phase C (REM, structure
proposals, smart wikis) — the surface that turns the Phase A
scaffold into a working product. This is a **documentation-consolidation
milestone**, not a frozen-API 1.0: the public release with stability
guarantees is the Phase E target. The repo is now at Phase D
(first-consumer cutover). For what each capability *is and does*, the
engineering wiki ([`wiki/index.md`](wiki/index.md)) is the single source
of truth; the pointers below link the relevant page.

### Added

- **Filesystem-SSOT memory model.** Memory lives as Obsidian-native
  markdown on disk; the `engine.db` sqlite index is fully
  reconstructible by re-walking the filesystem, so deleting it is a
  recoverable operation rather than data loss
  ([`wiki/concepts/memory-model.md`](wiki/concepts/memory-model.md)).
- **`wiki_type` registry** with bundled templates and an on-demand
  forge that invents a new template (frontmatter schema + lifecycle
  rules) at apply time
  ([`wiki/concepts/memory-model.md`](wiki/concepts/memory-model.md)).
- **Block-level ACL** via inline `{{owner=… allow=… sender=…}}…{{/}}`
  markers, with per-sender redaction applied region-by-region at render
  time ([`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md)).
- **Multi-user identity.** Users and groups with a single-admin model,
  managed through the dashboard CRUD; one unified JWT shape shared by
  the MCP and dashboard surfaces
  ([`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md)).
- **Write-side flow:** `wiki_capture` / `wiki_supersede` / `wiki_forget`
  / `wiki_link`, with jaccard 6-gram dedup against active facts on
  capture ([`wiki/protocol/tool-reference.md`](wiki/protocol/tool-reference.md)).
- **Hybrid recall:** lexical + semantic (embedding cosine) + wikilink
  multi-hop traversal, ACL-filtered
  ([`wiki/protocol/tool-reference.md`](wiki/protocol/tool-reference.md)).
- **`wiki_ingest_message` LLM router:** a single LLM call classifies a
  consumer message into capture / supersede / recall / structural-hint /
  skip and routes it to the write-side flow
  ([`wiki/protocol/tool-reference.md`](wiki/protocol/tool-reference.md)).
- **REM self-reorganization.** A nightly cycle runs lifecycle rules,
  settles overdue structure proposals, and emits dedup / promotion /
  type-forge / archive proposals plus hub regeneration
  ([`wiki/architecture/overview.md`](wiki/architecture/overview.md)).
- **MCP tool surface over HTTP** — families A–K (identity, capture,
  recall, ingest, structure proposals, audit, smart-wiki admin,
  skills, smart-consumer bootstrap, …). The exact roster lives in
  [`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md); the
  proposal-write actions (apply / confirm / revert) are dashboard-only,
  not on the MCP surface.
- **Smart-wikis + smart-consumer surface:** `wiki_admin_*`
  authoritative writes, the `_briefing.md` channel, cooperative leases,
  an append-only op-log with revert, the `/cite` resolver, and inline
  dashboard comments
  ([`wiki/protocol/mcp-tools.md`](wiki/protocol/mcp-tools.md)).
- **Built-in dashboard:** identity console, memory MVP (wiki / fact
  browser), agentic chat panel, admin LLM-config editor, and the
  operational-prompt editor
  ([`wiki/architecture/overview.md`](wiki/architecture/overview.md)).
- **Configurable internal LLM** with all-local / hybrid / all-api
  profiles across Ollama, Anthropic, and Gemini backends, wired per
  function and per backend through config + the dashboard editor
  ([`wiki/architecture/runtime-topology.md`](wiki/architecture/runtime-topology.md),
  [`wiki/protocol/config-schema.md`](wiki/protocol/config-schema.md)).

### Changed

- **Documentation consolidated.** The engineering wiki
  ([`wiki/index.md`](wiki/index.md)) is now the single source of truth
  for what the system is and does; the planning corpus
  ([`wiki/roadmap.md`](wiki/roadmap.md))
  is forward-only (roadmap + open questions).
- Rust toolchain pinned to **1.88** (was 1.85).

### Removed / Breaking

- **stdio MCP transport removed** — the server is HTTP-only now
  ([`wiki/architecture/runtime-topology.md`](wiki/architecture/runtime-topology.md)).
- **Legacy `enrollment.yaml` loader removed** — identity is created and
  managed through the dashboard first-run wizard + CRUD, not a seed file
  ([`wiki/concepts/identity-and-acl.md`](wiki/concepts/identity-and-acl.md)).
- Internally, the `wiki_type` "family" column was refactored to a
  `companion: bool` marker (the live registry reads `companion` via
  `is_companion()`; the old `family TEXT` column is retired).

## [0.0.1] — 2026-05-17

### Added
- Initial scaffold for Phase A.
- Cargo workspace with three crates:
  - `mwe-core` — headless memory engine (module skeleton).
  - `mwe-mcp-server` — CLI binary `mwe-mcp` with `init`, `serve`, `token-issue`,
    `token-revoke`, `token-list`, `doctor` subcommands (stubs).
  - `mwe-dashboard` — built-in PWA library (router stub).
- Pinned core dependencies: `rmcp` 1.7, `axum` 0.7, `maud` 0.26, `sqlx` 0.8,
  `tokio` 1, `jsonwebtoken` 9, `uuid` 1 (v7), `notify` 6, `clap` 4,
  `reqwest` 0.12 (rustls), `fs2` 0.4, `rust-embed` 8.
- Rust toolchain pinned to 1.85 (edition 2024).
- `rustfmt.toml`, `clippy.toml`, `.cargo/config.toml`, `deny.toml`.
- Dual license `MIT OR Apache-2.0`.
- Full planning corpus copied into `docs/design/` (read-only reference).
- Placeholder `AGENT_INSTRUCTIONS.md` with cardinal rule + decision tree.
