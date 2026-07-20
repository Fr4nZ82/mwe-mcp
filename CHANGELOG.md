# Changelog

All notable changes to **mwe-mcp** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

From 1.0, the public interface (the MCP tool surface, by family — see
[`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md)) is a stable,
semver-governed surface — breaking changes are called out explicitly.

## 1.4.6 — 2026-07-20

### Added

- **Per-user timezone — two users, two places, both right.** One
  deployment-wide `recall.ingest_timezone` is wrong the moment two
  users live in different zones (London and Sydney hear "tomorrow
  at 9" eleven hours apart), so the reference-time zone now resolves
  **per sender**: `enrollment_users.timezone` (new migration 0061) wins
  over the deployment default, which stays as the fallback; unset both,
  spoken times read as UTC as before. The admin sets it on the users
  page (create + edit); the welcome wizard's existing timezone question
  now also lands in the column (it previously became only a memory
  fact — the stamping plumbing reads the column, never the memory).
  A per-turn zone from the consumer (device time, covers travel) is a
  tracked protocol extension.

## 1.4.5 — 2026-07-20

### Added

- **Server-settings sections on the Settings page — no more YAML-only
  config.** Every typed config section without a dashboard surface is
  now an admin-only section of `/dashboard/settings/me`, closing the
  gap survey: **Ingest timezone** (`recall.ingest_timezone`, hot —
  swapped into the shared recall handle so the next ingest turn stamps
  wall-clock times in the deployment's zone), **Dream cadence**
  (`rem.schedule:` — mode, full/light intervals and initial delays,
  the light-dream backlog trigger), **Logging** (`logging:` — level,
  file rotation, file path), and **Document pipeline** (`document:` —
  segmenting, extraction caps, worker cadence, merge threshold). Same
  atomic `.bak`-guarded `load_raw` round-trip as the other editors;
  the boot-read sections say so and point at the Backup console's
  Restart button. The REM-settings and recall-settings panels now
  cross-link instead of declaring those keys YAML-only.
- **Recovery surfaces — automatic snapshots, dashboard restore, safe
  memory reset (roadmap 4d).** A new `backup:` config section (on by
  default: daily, retention 7) drives an automatic-snapshot scheduler:
  a due-check loop that hot-snapshots the whole workdir into a
  snapshots home (default: the `<workdir-name>-snapshots` sibling),
  prunes the oldest `auto-*` snapshots beyond the retention, persists
  its last-run stamp in `engine_meta` (a restart never re-fires inside
  the interval), and reports its outcome to the dashboard. The Backup
  console grows into the full recovery surface: settings editor
  (hot-swapped, `.bak`-guarded), the snapshots-on-disk listing with
  provenance badges, per-snapshot **Restore…** / **Delete**, and a
  type-`RESET`-to-confirm **Memory reset**. Restore and reset are
  **staged**: a one-shot `recovery-pending.json` marker (excluded from
  snapshots) that the next boot applies under the lockfile — automatic
  safety snapshot first, refusal-leaves-untouched, outcome persisted
  for the console — because a live server cannot safely replace its
  own open workdir. Reset wipes the memory tables and the
  `wikis/`/`media/`/`training-spool/` trees while preserving accounts,
  enrollment, consumers, tokens, 2FA/OAuth state, custom skills,
  config, env and prompt overrides; identity wikis are re-scaffolded
  empty and `profile_initialized` is cleared so the welcome wizard
  re-seeds each profile. A "Restart now" button applies a pending
  recovery from the dashboard: graceful shutdown, then exit code 75
  (`EX_TEMPFAIL`) so a `Restart=on-failure` systemd unit relaunches.
- **Verbatim source promotion — pasted text becomes a cited document
  (roadmap 46).** The server now backstops the media-first routing:
  document-shaped inline text is promoted to the media rail — the text
  is materialised verbatim as a content-addressed blob +
  `media_catalog` row (kind `doc`, `text/plain`) and the document
  pipeline runs against it, so extracted facts cite `source_ref =
  catalog_id` and the dashboard serves the preserved original, exactly
  like an uploaded file. Two doors, one deterministic shape heuristic
  (email headers, forwarded banners, quote/markup density,
  greeting/sign-off, size): `wiki_ingest_external source.type=inline`
  (response: `promoted_catalog_id`; `dry_run` reports `would_promote`)
  and an oversized `wiki_ingest_message` turn — the paste-into-chat
  case — which is archived + enqueued as a document job while the
  conversational ingest sees a bounded excerpt plus the promoted
  document as a linked attachment (response: `document_promoted`).
  A new `promote: always | never` dial on both tools forces the
  decision either way, disposition-style; guests, `dashboard_command`
  and assistant-authored turns never promote.
- **Behaviour-rule scopes — the user's rule for every assistant
  (roadmap 42).** The `behaviour_scope` axis grows a third value,
  `user-global`: a directive the user explicitly addresses to every
  assistant they talk to ("voglio che TUTTI gli assistenti mi parlino
  in italiano") now files in the **sender's own identity-wiki
  `rules.md`** (owner = the sender, no admin gate — it binds only
  their conversations) and the `rules` channel of every consumer
  serving that user surfaces it, the bindingless smart consumer
  included. Order pinned in `YOUR RULES`, most specific last:
  agent-wide → user-global → per-user. The classifier sees the union
  with fact ids and scope tags, so a revision supersedes across all
  three sources — but only the admin may supersede an agent-wide rule
  (a non-admin's revision files additively at its own scope). This
  retires the old salience-`high` workaround for cross-assistant
  directives, and the governance read (`sender_rules`) now strips fact
  regions from the user's `rules.md` so a user-global rule never leaks
  into the classifier's policy section.

### Fixed

- **Dashboard saves no longer persist `MWE_LLM_*` env overrides.** The
  config gains a `load_raw` round-trip primitive (file contents
  verbatim, no env overlay); every dashboard section editor now loads
  through it, so saving an unrelated panel can never bake a
  runtime-only override into the YAML. The LLM-config editor remains
  deliberately what-you-see-is-what-you-save.
- **hermes `mwe-watchdog` 0.2.0 — two verification gaps closed.** The
  watchdog now hashes the whitespace-trimmed turn text (the memory
  provider's canonical form), so a padded gateway message can no longer
  silently skip its handshake entry; and it requires the
  `<memory-context>` fence on the request's **last user message** —
  host injection is API-call-time only, so a fence on an earlier
  message is a stale injection index landing on the wrong turn and now
  counts as a miss.

## 1.4.1 — 2026-07-19

### Added

- **Training spool — teacher traces for local-slot distillation.** When
  `training_spool.enabled` is on (default off), every internal-LLM
  exchange — any slot, any backend, every transport (MCP ingest, REM
  cycle, dashboard chat) — is recorded verbatim as one JSON line (slot,
  backend, model, full request, full response, finish reason, token
  usage) into per-day files under `<workdir>/training-spool/`. The
  strong API slots act as teachers; their traces become the dataset for
  fine-tuning the local workhorse on mwe-mcp's own structured tasks.
  Recording is a decorator inside `build_backend` (no call-site
  changes), best-effort (an I/O failure never fails the turn); health
  probes and failed calls are excluded, images ride as MIME-only. New
  admin dashboard panel `/admin/training-spool` ("Spool" in the nav):
  checkbox with the atomic-YAML + `.bak` save idiom, hot-flip of the
  running recorder (no restart), on-disk inventory, and the privacy
  stance (the spool embeds recalled memory content — it stays on the
  host; scrub before sharing a dataset). See
  [`llm-functions.md` §6](docs/design-notes/llm-functions.md).
- **hermes bridge: `mwe-watchdog` verification plugin (trio → quartet).**
  Out-of-tree hermes plugin that verifies the mwe recall block actually
  reaches the model each turn — born from a silent memory-blackout
  incident where the host's stale injection index dropped the
  `<memory-context>` block after transcript repair. See
  [`agents-bridges.md`](docs/development/agents-bridges.md).

## 1.4.0 — 2026-07-15

### Added

- **Fresh-session resume: a blank-context requester is served its own
  surface.** A `wiki_ingest_message` turn that carries no `recent_messages`
  has no local context a served thread could duplicate — a reborn/blank
  session (e.g. a hermes gateway session silently reset by idle-expiry,
  upstream hermes-agent#43008) or a consumer that keeps no window at all.
  Such a turn now receives the cross-consumer recent window **including its
  own surface**: the thread the user is continuing, minus the message being
  spoken (the window fetch runs before the turn's own buffer write). Turns
  that bring their window keep the self-echo exclusion unchanged. A consumer
  on this contract never wakes up amnesiac — session resume with no
  host-side support.

## 1.3.0 — 2026-07-15

### Added

- **Cross-consumer recent window — the thread of discourse follows the user.**
  The server now retains a bounded serving buffer of the exchanges the
  per-turn ingest already receives (per user, hard cap `recent_window_entries`
  = 32 AND TTL `recent_window_ttl_hours` = 4, enforced in the write path;
  never indexed, never embedded, never REM-processed; deleted with the user)
  and every `wiki_ingest_message` response serves it back as the
  self-labelled `recent_window` field: the user's live thread from their
  OTHER surfaces, entries tagged with relative age and origin
  (`[2 min ago · via <consumer>/<channel>] user: …`), oldest first, newest
  winning the `recent_window_chars` (1200) budget, headed by an explicit
  do-not-re-answer framing. Consumers declare their surface with the new
  optional `metadata.channel` label; self-echo is excluded by
  (consumer, channel) — whole consumer when no label is sent. Windows never
  cross users. This restates the no-transcript invariant as *no unbounded
  transcript*: the buffer serves the live thread (minutes-to-hours), while
  long-range continuity stays with recalled facts.
- **hermes bridge, memory plugin 0.2.0** — sends the gateway key as
  `metadata.channel` and injects `recent_window` verbatim between the rules
  and the recalled facts.

### Fixed

- **Dashboard Facts pager: real ACL-projected totals and an editable page
  box.** Prev/next now derive from the real filtered total instead of a
  page-size heuristic, the page number is directly editable (jumps preserve
  the active filters), the disabled state reads "of M", and totals beyond the
  scan ceiling render as an "M+" estimate.
- **hermes bridge: `compression.in_place: true` is withdrawn — rotation mode
  (the vanilla default) is required.** hermes-agent's in-place compaction
  path re-appends the whole compacted window into the same active transcript
  after a preflight cut (its flush bookkeeping resets and the turn's history
  reference is nulled), doubling the conversation; the model then re-answers
  the replayed tail — observed live as a Telegram bot answering yesterday's
  messages. The bridge no longer recommends in-place anywhere; with rotation
  the same re-append lands in the freshly rotated session, where it is
  correct behaviour.
- **mwe-truncate 0.3.0: oversized tool results are snipped on a cut**
  (`snip_tool_chars`, default 4000). The window bounds *turns*, not *weight* —
  browser-tool spam kept the bounded window permanently above the compression
  trigger (fire-abort on every call, one session crash-looping at ~328k
  tokens). Snipping is copy-on-write (the rotated-out archive keeps full
  contents) and never touches the tail from the last user message onward; a
  snip-only pass must save ≥8% or it reports a no-op through the abort
  protocol.

## 1.2.0 — 2026-07-14

### Added

- **Deployment timezone for the ingest classifier** — `recall.ingest_timezone`
  (or the `MWE_INGEST_TIMEZONE` env var) names the users' IANA timezone (e.g.
  `Europe/Rome`). When set, a bare wall-clock time a user speaks ("alle 16") is
  read in that zone and converted to UTC for a fact's validity interval,
  instead of being stamped verbatim as UTC — which drifted every dated
  commitment by the local offset, so deadlines expired late and stale plans
  resurfaced as if still current. Unset keeps the prior UTC-only anchor. The
  DST-aware conversion is delegated to the classifier; no timezone database is
  compiled in.

### Changed

- **Relationships between people now reach the always-on identity core.** A
  statement of who someone is to someone else ("X is Y's partner / parent /
  child") is classified as identity core (`bio` / `high`), extracted
  reciprocally when both are enrolled, and shielded from dedup and
  contradiction retirement — so an agent stops confusing who is who across a
  family or a team.

### Fixed

- **hermes bridge: the per-turn recall block no longer injects
  `suggested_seed`.** A consumer that brings its own model was handed a
  pre-drafted reply inside the user turn, which a weaker model could adopt or
  continue — laundering the ingest classifier's guesses into the agent's
  replies. The bridge now surfaces only the recalled facts.

## 1.1.2 — 2026-07-14

### Fixed

- **An orphaned identity wiki can now be deleted (admin).** Deleting a
  user keeps their wikis (the sender-scrub invariant), but the
  identity-wiki guard refused deletion unconditionally ("remove the
  user/group instead") — a dead end once the user was already gone. The
  refusal is now scoped to *living* principals: an identity wiki whose
  user/group is no longer enrolled is deletable from the dashboard like
  any other wiki, with the same typed-id confirmation and move/tombstone
  dispositions. (#4)

## 1.1.1 — 2026-07-14

### Fixed

- **The binary self-reports its release version again** — the v1.1.0
  artifacts printed `1.0.0` because the workspace version was not bumped
  at release time.
- **Deleting an enrolled user no longer 500s when the identity is bound
  to a consumer.** `consumers.system_user_id` is a plain FK with no `ON
  DELETE` action, so the dashboard's bare `DELETE FROM enrollment_users`
  was rejected by SQLite for any identity registered as a consumer's
  system user. Deletion now goes through `enrollment::remove_user`, one
  transaction that dismantles everything hanging off the identity:
  consumers system-bound to it (registration row, delegation grant,
  web-agent OAuth rows), the user's own OAuth codes/refresh rows (a live
  refresh row would keep minting tokens for a vanished sender), then the
  enrollment row (CASCADE clears credentials, invitations, 2FA, votes).
  The delegation cache is refreshed post-commit so act-as dies on the
  next call. The deleted user's *memory* outlives the identity: their
  wikis stay, and facts they authored are re-pointed at the containing
  wiki's scope principal (the sender-scrub invariant).
- **hermes bridge — the `mwe-truncate` context engine now actually bounds
  the conversation window** (plugin 0.2.0). Its only trigger was hermes's
  `threshold_percent` (0.75 of the model context — a summarization
  default), so on a million-token model the first cut sat at ~786k prompt
  tokens: far beyond per-minute provider token quotas, which a long-lived
  session exhausted first. The window is now counted in recent **user
  turns** (`protect_last_users`, default 5 — cut at a user-message
  boundary, so tool-call pairing holds by construction) with a slack
  (`slack_users`, default 3) that keeps the prompt prefix cache-stable
  between cuts, and the trigger is capped in absolute tokens
  (`threshold_tokens_cap`, default 30k). A no-op fire reports through the
  host's abort protocol instead of rotating the session; pair with
  hermes's `compression.in_place: true` (see the bridge README). The
  `protect_last_n` config key is retired (logged and ignored).

## 1.1.0 — 2026-07-06

*(Section reconstructed after the fact — 1.1.0 shipped without a
changelog entry; the content below is from the release commit.)*

### Added

- **Proactive smart-consumer wiki onboarding** — onboarding is offered
  at connect.
- **Bulk-copy bootstrap** — bulk-copy moves bytes without going through
  the LLM.
- **Bounded log pages** — append-only log pages get a rotate-by-period
  discipline.

## 1.0.0 — 2026-07-06

First public release.

### Changed

- **License: AGPL-3.0-or-later** (was `MIT OR Apache-2.0`), with a
  commercial dual-license available — see [LICENSING.md](LICENSING.md).
  SPDX headers on all first-party sources; contributions now require a
  DCO sign-off plus a relicensing grant ([CONTRIBUTING.md](CONTRIBUTING.md)).
- **Public repository with a fresh history.** The engineering wiki stays
  in the maintainer's private archive; the user/integrator documentation
  ships in [`docs/`](docs/).
- The MCP tool families are declared **stable under semver** from this
  release.

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
documentation set (now `docs/`) is the single source
of truth; the pointers below link the relevant page.

### Added

- **Filesystem-SSOT memory model.** Memory lives as Obsidian-native
  markdown on disk; the `engine.db` sqlite index is fully
  reconstructible by re-walking the filesystem, so deleting it is a
  recoverable operation rather than data loss
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **`wiki_type` registry** with bundled templates and an on-demand
  forge that invents a new template (frontmatter schema + lifecycle
  rules) at apply time
  ([`docs/concepts/memory-model.md`](docs/concepts/memory-model.md)).
- **Block-level ACL** via inline `{{owner=… allow=… sender=…}}…{{/}}`
  markers, with per-sender redaction applied region-by-region at render
  time ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Multi-user identity.** Users and groups with a single-admin model,
  managed through the dashboard CRUD; one unified JWT shape shared by
  the MCP and dashboard surfaces
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
- **Write-side flow:** `wiki_capture` / `wiki_supersede` / `wiki_forget`
  / `wiki_link`, with jaccard 6-gram dedup against active facts on
  capture ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **Hybrid recall:** lexical + semantic (embedding cosine) + wikilink
  multi-hop traversal, ACL-filtered
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **`wiki_ingest_message` LLM router:** a single LLM call classifies a
  consumer message into capture / supersede / recall / structural-hint /
  skip and routes it to the write-side flow
  ([`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md)).
- **REM self-reorganization.** A nightly cycle runs lifecycle rules,
  settles overdue structure proposals, and emits dedup / promotion /
  type-forge / archive proposals plus hub regeneration
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **MCP tool surface over HTTP** — families A–K (identity, capture,
  recall, ingest, structure proposals, audit, smart-wiki admin,
  skills, smart-consumer bootstrap, …). The exact roster lives in
  [`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md); the
  proposal-write actions (apply / confirm / revert) are dashboard-only,
  not on the MCP surface.
- **Smart-wikis + smart-consumer surface:** `wiki_admin_*`
  authoritative writes, the `_briefing.md` channel, cooperative leases,
  an append-only op-log with revert, the `/cite` resolver, and inline
  dashboard comments
  ([`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md)).
- **Built-in dashboard:** identity console, memory MVP (wiki / fact
  browser), agentic chat panel, admin LLM-config editor, and the
  operational-prompt editor
  ([`docs/architecture/overview.md`](docs/architecture/overview.md)).
- **Configurable internal LLM** with all-local / hybrid / all-api
  profiles across Ollama, Anthropic, and Gemini backends, wired per
  function and per backend through config + the dashboard editor
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md),
  [`docs/protocol/config-schema.md`](docs/protocol/config-schema.md)).

### Changed

- **Documentation consolidated.** The engineering documentation is now
  the single source of truth for what the system is and does; the
  planning corpus is forward-only (roadmap + open questions).
- Rust toolchain pinned to **1.88** (was 1.85).

### Removed / Breaking

- **stdio MCP transport removed** — the server is HTTP-only now
  ([`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md)).
- **Legacy `enrollment.yaml` loader removed** — identity is created and
  managed through the dashboard first-run wizard + CRUD, not a seed file
  ([`docs/concepts/identity-and-acl.md`](docs/concepts/identity-and-acl.md)).
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
