---
title: Dashboard — architecture and auth model
area: design-notes
status: implemented
last_review: "2026-06-26"
---

# Dashboard — architecture and auth model

The identity, JWT, transport, and consumer-delegation model that this
page builds on lives in its own deep dive,
[`jwt-and-session-model.md`](jwt-and-session-model.md) — single admin
per deployment, the one-JWT-shape model, the bearer middleware, and the
`X-MWE-Act-As` resolution. The visibility / redaction policy is in
[`redaction-policy.md`](redaction-policy.md). The stack rationale
(Rust + Axum + Maud + sqlx + tokio) lives in
[`why-rust.md`](why-rust.md).

This page is the **canonical reference** for the dashboard runtime —
the design that ships in
[`crates/mwe-dashboard/`](../../crates/mwe-dashboard/). For day-to-day
collaboration with the code, this is the page to read first; the
overview at [`architecture/overview.md`](../architecture/overview.md)
indexes it, and the memory-side surface has its own deep
dive in [`dashboard-memory-mvp.md`](dashboard-memory-mvp.md).

## What ships today

The dashboard covers the **identity console + memory MVP + agentic
operational chat + first-login welcome wizard**.

- **Identity console** — first-run admin bootstrap, login/logout,
  users CRUD (with invitation flow), groups CRUD, tokens CRUD (with
  consumer delegation editor), self-service password change. See
  [`setup-and-identity.md`](setup-and-identity.md).
- **Memory MVP** — wiki list (`/wiki`), single-wiki view
  (`/wiki/:id`), chat page (`/chat`) + omnipresent chat widget on
  every authenticated page, 4 KPI tiles on the home. The wiki-side
  handlers depend on the
  `MemoryHandles` bundle in `DashboardState`. Every dashboard
  edit lands in `wiki_admin_op_log` via the per-page
  textual editor at `/dashboard/wiki/:id/edit/*path` calling
  `mwe_core::wiki_admin::push` with `ActorKind::Dashboard`; the
  op-log page (`/dashboard/wiki/:id/op-log`) has
  a Revert button per row routing through
  `mwe_core::wiki_admin::op_revert` with strict conflict policy
  `409 op_log_target_changed_since` (no force, no merge); a
  read-only viewer `/dashboard/wiki/:id/view/*path` renders
  the page body and interpolates `wiki_briefing_items` with
  `target_cite` inline below the heading they point at, with an
  Orphaned comments footer for items whose anchor disappeared; the
  same viewer in `?mode=comment` surfaces "+ Comment"
  affordances per heading that POST to
  `/dashboard/wiki/:id/comment/*path?anchor=<slug>` to persist the
  comment as a `dashboard_comment` row attributed to the
  signed-in user; and the public top-level resolver
  `/cite/<bi_id>` (alias `/dashboard/cite/<bi_id>`)
  redirects to the deep-link of the cited page+anchor so smart
  consumers can embed clickable citations in their replies.
  `_meta.md` writes are refused from the editor to keep the
  sharing surface on `/dashboard/wiki/:id/sharing`. The single-wiki
  view also carries the admin-only **export** link
  (`GET /dashboard/wiki/:id/export`) streaming the wiki subtree as a
  portable full-marker tar via
  [`mwe_core::export`](../../crates/mwe-core/src/export.rs) — the
  dashboard's first download surface (`Content-Disposition:
  attachment`).
- **Agentic operational chat** — `POST /dashboard/chat/agentic`
  endpoint, agentic loop with `MAX_AGENTIC_ITERATIONS = 8`,
  whitelisted subset of `_internal.*` tools
  (`mwe_dashboard::agentic::tool_descriptors()` is the roster),
  transparent tool-call trace in the panel, `window.__mweChatPrimer`
  primer mechanism, and the paginated `/dashboard/facts` browser. See
  [`agentic-chat.md`](agentic-chat.md) for the agentic tool surface and
  the read/write tool split.
- **Proposals are operated from the chat, not a form** — there is no
  proposals questionnaire / tray FORM surface and no nav entry: the GET
  `/dashboard/proposals` list and the GET
  `/dashboard/proposals/pending-confirms` tray do not exist. Proposals are
  reviewed, applied, confirmed, and reverted by talking to the chat, which
  drives the same `mwe_core::proposals` chassis through its agentic tools
  (`structure_proposal_list` / `_get` / `_apply` / `_revert` /
  `_confirm` — the roster SSOT is `mwe_dashboard::agentic::AgenticTool`;
  there is no chat-side emit: proposals are REM-emitted, the chat only
  operates on them). `routes::proposals` mounts the **action routes** + the
  **open-in-chat bridge** as endpoints the chat / consumer links target:
  POST `:id/apply`, POST `:id/confirm`, POST `:id/revert` — each
  performs its chassis action and **303-redirects to `/dashboard/chat`**
  (the single operational surface) on both success and classified error,
  rather than re-rendering a page — and GET `:id/open-in-chat`, the
  primer landing the operator inside the chat with the proposal already
  summarised — a *review/apply* primer for the pending questionnaire.
- **In-flight badge + Help button** — two affordances keep the free-form
  chat discoverable and the pending work visible. All operator
  chrome here is English (matching the rest of the dashboard shell — the
  chat *replies* still follow the user's locale):
  - The **in-flight badge** (`#in-flight-badge`, in the topnav
    `user-block`) is hidden until `ui.js`, on load, fetches
    `GET /dashboard/proposals/in-flight-count` — an authenticated JSON
    endpoint returning the per-class counts (`pending`,
    `applied_pending_confirm`, `revertable_applied`, `total`) from
    [`mwe_core::proposals::count_in_flight`](../../crates/mwe-core/src/proposals.rs),
    ACL-scoped to the signed-in user: everyone — admins included — counts
    rows addressed to them plus the unaddressed/admin-fallback ones, and
    the admin ACL-reveal cookie ([`crate::reveal`](redaction-policy.md#dashboard-admin-reveal))
    lifts an admin to the deployment-wide count (a proposal's `context`
    carries per-fragment-ACL'd fact text, so an unconditional admin-wide
    count would leak it).
    `count_in_flight` is the count SSOT — the three classes it sums are
    defined there, not duplicated as a load-bearing list here. When
    `total > 0` the badge reveals with "N pending". `chat.js` intercepts
    the click: it opens the chat panel and `fetch`es the overview turn from
    `GET /dashboard/proposals/in-flight/chat-turn` — a JSON endpoint that
    runs the read-only "show me everything pending" primer through
    `chat::agentic_submission` and returns the [`AgenticTurn`] — rendering
    it inline as a normal turn, with a spinner while it loads. No page
    navigation (the badge is JS-revealed, so there is no no-JS landing to
    keep; the `href` is a plain fallback to `/dashboard/chat`). The
    single-proposal born-applied receipt keeps its full-page
    `GET /dashboard/proposals/:id/open-in-chat` landing
    (`land_turn_in_chat`), since that one is a real 303 navigation target.
  - The **Help button** (`#help-open`) lives **in the chat panel header**,
    between the "Chat" title and the close (×) — Help is about the chat,
    so it sits with it rather than in the topnav. It is a real anchor to
    `GET /dashboard/help` (the no-JS fallback page) that `ui.js` intercepts
    to reveal a modal (the same modal mechanism as the admin Dream
    affordance). Both surfaces render one shared `layout::help_body` — a
    skimmable map of spoken phrasings ("move X under Y", "keep items two
    days", "add a field", "undo that", "what's pending?") to what they do —
    so they cannot drift. Shown to every authenticated user, not just
    admins.
- **Form-to-chat bridge** — the
  deterministic `GET /dashboard/facts/:fact_id/edit` form (owner /
  allow / topics / fact_type / body, pre-populated from `fact_index`)
  routes its `POST /edit/submit` through a `match` mapper
  (`compose_edit_message` with three macro-cases — metadata-only /
  body-only / mixed) into `chat::agentic_submission`, then lands the
  user on `/dashboard/chat` with the resulting `AgenticTurn`
  hydrated via `window.__mweChatPrimer`. Net result: every write to
  a fact still goes through the chat's HARD-RULE explicit
  confirmation, the UI side just sources the *delta* in a
  shape that does not require typing prose. There is no
  `GET /dashboard/facts/:fact_id/open-in-chat` page; the
  per-row "Modifica via chat" deep-link from `/dashboard/facts`
  targets the `/edit` form.
- **Welcome wizard** — `/dashboard/welcome` first-login
  16-field form that composes an italian-language primer and feeds it
  to `chat::process_submission` via `window.__mweChatPrimer`. Gated
  by `enrollment_users.profile_initialized` (migration `0018`); flips
  to `true` once the wizard submission applies. See
  [`setup-and-identity.md`](setup-and-identity.md) §First-login profile
  wizard.
- **Admin LLM config + API key editor** —
  `/dashboard/admin/llm-config` admin-only editor for
  the `llm:` section of `mwe-mcp.config.yaml` (5 slots, backend +
  model + temperature + max_tokens + reasoning_effort + base_url)
  and for the API key env-vars in `mwe-mcp.env`. Set-only API key
  field with 4-char fingerprint display; the value never leaves the
  server in cleartext. Restart-required banner because `LlmConfig`
  and the env file are loaded once at boot; hot-reload is not yet
  supported. See [`admin-llm-config.md`](admin-llm-config.md) for the
  full design.
- **Recall settings** — `/dashboard/admin/recall-settings` admin-only
  editor for the [`recall:` config section](../protocol/config-schema.md#recall):
  the resource knobs of the per-turn recall block (flat/fresh slot
  sizes, navigator depth/budget/caps, due-soon horizon), empty field =
  Rust default. The save rewrites the YAML atomically (`.bak` backup,
  same mechanics as the LLM editor) and **hot-swaps** the shared
  `Arc<RwLock<RecallConfig>>` that both transports (MCP dispatcher and
  dashboard chat) read per turn — no restart caveat. Semantics stay in
  the `navigator` prompt; the panel only bounds resources.
- **Recall traces** — `/dashboard/admin/recall-traces` admin-only journal
  of the last few recall runs (the [`recall_trace`
  journal](recall-pipeline.md#recall-traces--the-last-10-journal): what a
  user action pulled out of memory and what was injected back). The list
  links each trace to a per-trace viewer that mounts the **animated 3D
  replay** of the route (WebGL, [`recall-trace.js`](dashboard-frontend.md#js-architecture))
  above the full textual trace — hits with scores and page regions, the
  entry-point fan, every navigator hop with its candidates / one-line
  note / vetting outcome, and the injected block verbatim; the text is
  also the no-JS / no-WebGL fallback. `/:id/data` serves the viewer's
  JSON feed. Admin-only because a trace journals what a specific sender
  was served, across wiki and ACL lines — operator telemetry, same tier
  as the engine DB.
- **REM settings** — `/dashboard/admin/rem-settings` admin-only editor
  for the [`rem.policy:` config subsection](../protocol/config-schema.md#rem):
  the REM cycle's behaviour knobs (auto-promote mass bars, per-cycle
  sweep caps, the briefing-processor grace — every `RemPolicyConfig`
  field), empty field = Rust default shown as the placeholder. Same
  atomic-YAML save mechanics as the recall editor, and the same
  **hot-swap**: the save replaces the shared `Arc<RwLock<RemPolicy>>`
  that the interval REM scheduler snapshots at each cycle start and the
  Dream console reads at each trigger — no restart caveat. The dream
  *cadence* (`rem.schedule:`) stays YAML-only; semantic judgment stays
  with the LLM sub-jobs.
- **Embedding settings** — `/dashboard/admin/embedding` admin-only editor
  for the [`embedding:` config section](../protocol/config-schema.md#embedding):
  backend selector (`ollama` / `bundled` / `openai`), `model`, Ollama
  `base_url`, the bundled CPU/GPU device toggle, `dimensions`, and the offline
  `model_dir`. Same atomic-write mechanics as the other editors, but
  **restart-required** (the embedder is built once at startup — no hot-swap)
  with a reindex warning when the backend or model changes. `bundled` is
  offered only on a `local-embedder` build and `gpu` only on a CUDA build
  (roadmap 18f); otherwise each is shown disabled with the reason.
- **Email settings** — admin-only **section of the Settings page**
  (`/dashboard/settings/me`; no page of its own — saves POST to
  `/dashboard/settings/email`, the test send to
  `/dashboard/settings/email/test`) editing the
  [`email:` config section](../protocol/config-schema.md#email): the SMTP
  backend (host, port, TLS mode, From address, `password_env`) that powers
  self-service **password recovery** (roadmap 28) and **delivery of
  user-invitation links** (create-user / reinvite email the one-shot
  accept-invite URL when the backend is configured; the dashboard still
  shows that URL as a backup and a failed send only logs), plus a "send
  test email" button. Same atomic-YAML save as the admin editors; off by
  default; hidden when the server runs without memory handles. Full flow in
  [JWT & session model §recovery and 2FA](jwt-and-session-model.md#password-recovery-and-two-factor-authentication).
- **Two-factor (TOTP)** — `/dashboard/settings/2fa` is the per-user
  enrollment surface (any signed-in user); the Settings page carries the
  admin **require-2FA-for-all** toggle and the edit-user page the per-user
  require flag + a Reset-2FA break-glass. The public login challenge is
  `/dashboard/2fa`. Mechanism: same §recovery-and-2FA reference.
- **Health** — `/dashboard/admin/health` admin-only **live diagnostics**
  page: the lockfile-free subset of `mwe-mcp doctor` run against the
  *running* server. Shows the engine-DB counts (app tables, migrations
  applied), the WAL recovery backlog (stale proposal/REM ops), the token
  blacklist size, the workdir permission audit, and per-slot LLM
  reachability. It calls the shared
  [`mwe_core::diagnostics`](../../crates/mwe-core/src/diagnostics.rs)
  collector (`collect_db` + `probe_llm_slots`) — the same code the CLI
  `doctor` uses — and probes the **live** LLM handles so dashboard-set API
  keys are honoured. The page paints immediately with the fast DB/workdir
  diagnostics and a spinner where the slots go; the slow per-slot probe
  (one network round-trip per slot, which can hang on an unreachable
  backend) is served from `/admin/health/llm-slots` and fetched
  client-side by `ui.js`, which swaps the table in when it arrives
  (`?fragment=1` returns the bare table; a direct hit — the no-JS
  `<noscript>` fallback — returns a full page). Read-only: no lockfile, so
  it never contends with `serve` (which is why the boot-failure-triage
  checks `doctor` keeps — lockfile, secret-from-env, JWT self-test — stay
  CLI-only).
- **Backup** — `/dashboard/admin/backup` admin-only "Backup now" trigger:
  a hot workdir snapshot via `mwe_core::backup::snapshot_workdir` (the
  same point-in-time copy the CLI `mwe-mcp backup --out` produces, safe
  next to the live server — no lockfile). The form prefills a timestamped
  destination outside the workdir and reports what was written; it warns
  that the snapshot contains `mwe-mcp.env` (the secret + API keys) and the
  cleartext memory wiki, so it must land in an owner-only location. The
  `backup` CLI stays for cron / server-off operation. ("Run REM now" — the
  other maintenance trigger — is the admin **Dream** console.)

Not yet shipped: audit / costs dashboard pages, full
HTMX swap-on-form, PWA service worker + manifest + installable icon,
Tailwind real build, push notification opt-in (planned — see the
roadmap).

### Route map

The router is assembled in
[`routes/mod.rs`](../../crates/mwe-dashboard/src/routes/mod.rs) as two
trees merged into one and mounted under `/dashboard`:

- **Public tree** (no session requirement): the root redirect (`/`),
  the first-run wizard (`/setup`), `/login`, the invitation-accept flow,
  the citation-handle resolver (`cite`, also mounted at the root as the
  canonical short `/cite/:bi_id`), and the embedded static assets. A
  verify failure on these is fine — auth fires on the destination page.
- **Authenticated tree** (behind the
  [session middleware](#auth-model--sliding-ttl-session) that redirects
  to `/dashboard/login` on any verify failure): `home`, `logout`, and
  the per-concern routers merged in — identity (`users`, `groups`,
  `tokens`, `settings`), the welcome wizard (`welcome`), memory
  (`proposals`, `wiki_view`, `smart_view`, `facts`, `briefing`),
  the media alias (`media` — `GET /dashboard/media/:catalog_id`, the
  cookie-authenticated byte serving behind inline embeds; per-media
  ACL, no admin bypass — see
  [media pipeline](media-pipeline.md)),
  the prompt editor (`prompts`), the
  admin LLM config editor (`llm_config`), the admin recall-settings
  editor (`recall_settings`), the admin recall-traces journal + 3D
  replay viewer (`recall_traces`), the admin REM-settings editor
  (`rem_settings`), the admin embedding-settings editor
  (`embedding_settings`), the admin live-diagnostics page (`health`),
  the admin "Backup now" trigger (`backup`), the admin **Dream** console
  (`dream` — on-demand `mwe_core::dream` triggers; the slow `compile` /
  `full` run as background tasks that a topnav indicator follows via
  `GET /dashboard/dream/status`, while a no-JS submit still gets the
  synchronous report — the handler branches on `Accept`), and the chat
  (`chat`). (The `/connect` hook-bundle helper is a root-level router
  mounted by `mwe-mcp-server`, not part of this tree.)

Each concern's router lives in its own module under
[`routes/`](../../crates/mwe-dashboard/src/routes/) so the merge list in
`build()` is the single source of truth for what the dashboard mounts —
read it there rather than trusting a hand-maintained count here. The
architecture overview indexes the same map at a higher altitude.

## Auth model — sliding-TTL session

The session cookie is a JWT signed with the deployment's
`MWE_TOKEN_SECRET` — **the same secret and the same payload shape**
the MCP-side tokens use (the one-JWT-shape model is documented in
[`jwt-and-session-model.md`](jwt-and-session-model.md)).
The only differences are:

| Aspect | Session cookie | MCP token |
|---|---|---|
| TTL | 60 min (sliding) | 1 year internal / 30 days exposed |
| `device_label` | hardcoded `"dashboard-session"` | operator-chosen |
| `rate_limit_id` | `"dashboard"` | operator-chosen |
| `consumer_id` | never set | optional (consumer bots) |
| Transport | `Cookie:` header, `Path=/dashboard` | `Authorization: Bearer` |

Sliding TTL is implemented as **re-issuance on every authenticated
request**, not as cookie expiry extension on the wire. The middleware
([`auth::session::refresh_session_layer`](../../crates/mwe-dashboard/src/auth/session.rs)):

1. Reads the cookie, runs `mwe_core::jwt::verify` (signature +
   `exp` + blacklist).
2. On success, attaches a typed `SessionUser` to the request
   extensions so the route handler reads it via a `FromRequestParts`
   extractor with no DB hit.
3. Runs the inner handler.
4. Mints a fresh JWT with the same `sender_id` + `is_admin` but a
   new `jti` and `exp = now + 60min`, attaches it to the response via
   `axum_extra::extract::cookie::CookieJar`.
5. On any verify failure, short-circuits with a 303 to
   `/dashboard/login`.

Because a sliding TTL only advances when a request reaches the
middleware, an entirely client-side form would let it lapse — the
welcome primer's stepper advances with no request until the final
submit, so a slow filler would have that submit bounced to `/login`
with their answers lost. The shell layout therefore pings
`GET /dashboard/session/keepalive` (a 204 endpoint under the same layer)
on user interaction, throttled to once every few minutes while the tab
is visible. The ping is an authenticated request like any other, so it
re-issues the cookie and keeps an active tab signed in.

The cookie attributes are `HttpOnly`, `SameSite=Lax`,
`Path=/dashboard`, and `Secure` only when the operator opts in via
`DashboardConfig.cookie_secure` (default `false` so the standard
`mwe-mcp serve --bind 127.0.0.1` flow works without TLS). The narrow
`Path` keeps the cookie out of `/mcp/*` so MCP clients never see it
even when the dashboard and MCP coexist behind the same host.

`is_admin` is trusted from the JWT for the cookie's 60-minute
lifetime. The only way to invalidate an admin role before the cookie
naturally expires is to revoke its `jti` via `/dashboard/tokens` or
the `mwe-mcp token-revoke` CLI; the shared `BlacklistCache` propagates
the change within its 60-second TTL.

Two extractors live in `auth::session`:

- `SessionUser` — succeeds whenever the middleware injected a session.
  Returns 401 (`DashboardError::Unauthenticated`) otherwise.
- `AdminUser` — wraps `SessionUser` + a `require_admin()` check;
  failure renders the 403 error page. Used by every admin-gated
  handler so the role check is visible at the function signature.

## Single admin per deployment

The deployment is restricted to **at most one row with `is_admin = 1`**
in `enrollment_users`, enforced by a partial unique index
`idx_single_admin` (see
[`jwt-and-session-model.md`](jwt-and-session-model.md) for the rationale
and the migration history). The dashboard collaborates with this
invariant in four places:

- The **first-run wizard** at `/setup` is the only producer of an
  admin row; it inserts with `is_admin = 1`.
- The **users CRUD form** never exposes an `is_admin` toggle: every
  user created via `/users/new` or `/users/:id/reinvite` lands with
  `is_admin = 0`.
- The **token issue form** **derives** the JWT `is_admin` claim, never
  toggles it: a **smart** token inherits the chosen owner's
  `enrollment_users.is_admin`; a **standard** token is always non-admin
  (its sender is a credential-less bot identity). No "issue as admin"
  knob exists, because there is no second admin to issue for.
- The **delete handler** refuses to drop the admin row from the
  dashboard (422 with an explanatory message); the only path is the
  CLI `mwe-mcp admin-reset --user <admin_id>` to re-invite the same
  admin, or direct DB tampering.

## UI choices

The dashboard ships **server-rendered Maud** (no client-side JS
framework, no HTMX yet, plain `<form method="POST">` everywhere)
on a **phosphor-terminal Tailwind v4 surface** built from
[`tailwind/`](../../tailwind/) by the standalone tailwindcss CLI
into [`assets/tailwind.css`](../../crates/mwe-dashboard/assets/tailwind.css),
embedded via `rust-embed`. Mobile-first responsive (hamburger nav
below 768 px, off-canvas chat drawer below 1280 px), JetBrains
Mono self-hosted, SVG mark from the design pack. Every CSS rule lives
in `tailwind/app.css` under `@layer components`, with colour references
routed through the phosphor design tokens.

The full page-anatomy / CSS-architecture / JS-architecture / build-
pipeline detail lives in the sibling page
[`dashboard-frontend.md`](dashboard-frontend.md). The two pages
split along the same line as their filenames: this one is the
*server-side* dashboard reference (routes, auth, single-admin
invariant); the frontend page is the *client-side* surface.

The PWA pieces — Web App Manifest, service worker, installable icon —
are *not* shipped yet.

## HTML form parsing — why `HtmlForm` instead of `axum::Form`

`axum::Form` uses `serde_urlencoded`, which *rejects* repeated keys.
The dashboard's group and delegation forms post `members=alice&members=bob`
checkbox lists where the natural Rust type is `Vec<String>`. We ship
a tiny [`form::HtmlForm<T>`](../../crates/mwe-dashboard/src/form.rs)
extractor backed by `serde_html_form` that does support repeated keys
and otherwise behaves like `axum::Form`. Routes that take only scalar
fields (login, setup, settings) keep `axum::Form`.

## Test strategy

Integration tests live in
[`crates/mwe-dashboard/tests/`](../../crates/mwe-dashboard/tests/),
one file per concern (`bootstrap.rs`, `users.rs`, `groups.rs`,
`tokens.rs`, `settings.rs`) plus a `common/` helper module. They use
`tower::ServiceExt::oneshot` against the live `mwe_dashboard::router`
backed by a `tempfile::TempDir` workdir + sqlite — no real TCP
listener, no port assignment, fully deterministic. The suite
sits at **528+ tests** across the workspace, with the dashboard's own
`tests/wiki_explorer.rs` providing 32 of them dedicated to the
chat-driven flows (see [`dashboard-memory-mvp.md`](dashboard-memory-mvp.md)).

What is intentionally **not** tested today: TLS, real listener
binding, browser-side service worker, accessibility (a11y), Lighthouse
PWA score.

## MCP handoff status

`/mcp` serves the rmcp Streamable HTTP dispatcher, wired through
[`mwe-mcp-server::mcp`](../../crates/mwe-mcp-server/src/mcp/) with
the bearer-JWT middleware
([`mwe-mcp-server::mcp::auth::jwt_auth_middleware`](../../crates/mwe-mcp-server/src/mcp/auth.rs))
that:

  - verifies the `Authorization: Bearer` JWT with the same
    `mwe_core::jwt::verify` the dashboard session uses,
  - resolves the optional `X-MWE-Act-As` header and attaches the
    resulting effective-sender `IdentityProfile` to the request
    extensions for the rmcp tools to read.

The `X-MWE-Act-As` resolution is **shipped**:
[`resolve_act_as`](../../crates/mwe-mcp-server/src/mcp/auth.rs) inspects
the header and the middleware checks it against the
[`consumer_delegations`](../../migrations/0014_consumer_delegations.sql)
table via a `DelegationCache` that mirrors the existing
`BlacklistCache`. A **smart**-class token that sets the header is
rejected `403 act_as_requires_standard` (delegation is a standard-only
feature — smart consumers are mono-user by design); a token without a
`consumer_id` claim is rejected `403 act_as_requires_consumer`; an
undelegated target is `403 act_as_not_delegated`; a malformed value is
`403 act_as_malformed`; otherwise the effective sender is rewritten to
the delegated user. The dashboard delegation editor at
`/dashboard/tokens/delegation/:consumer_id` writes the rows the cache
reads. Identity / transport / delegation semantics live in
[`jwt-and-session-model.md`](jwt-and-session-model.md); the per-tool
read-back is in [`mcp-dispatcher.md`](mcp-dispatcher.md).

The **token issue form** turns the diagonal model into a single
either/or — a *consumer class* radio — so the page only ever asks for
the fields that class needs:

- **Smart** binds the token to a chosen human **owner** (the sender) and
  takes a free *device id* (the `consumer_id`, for op-log attribution);
  no delegation.
- **Standard** takes a **bot id** that *is* the sender: a credential-less
  system user the form mints on the spot (plus its identity wiki) if it
  does not exist yet, refusing to reuse a human login account. The bot id
  must be `WikiId`-safe, so it doubles as the `consumer_id`. The form
  collects the **act-as** list and writes the `consumer_delegations` row.

Both act-as checkbox lists (issue form + delegation editor) offer, after
the enrolled users, the builtin **`guest`** pseudo-identity — ticking it
is the enable switch for serving unidentified humans on that consumer
(the validators accept `guest` without enrollment; every other unknown
id is still refused). See
[identity-and-acl.md §1](../concepts/identity-and-acl.md).

`device_label` is a free audit label that sits under the id field and
defaults to the `consumer_id` (the small `assets/tokens.js` mirrors it
live; the server applies the same fallback when it is left blank, so the
form degrades cleanly without JS). The shared
`enrollment::validate_token_identity` guard — the same one the
`mwe-mcp token-issue` CLI calls — backstops the form so the two paths
cannot drift.

There is no `/mcp/token-refresh` endpoint: no handler exists for it
today. Its absence is not load-bearing because the
dashboard already shares the `BlacklistCache` instance via
`DashboardState`, so revokes triggered from `/dashboard/tokens`
propagate to MCP verify within the cache TTL — a client whose token was
revoked re-issues from the dashboard rather than refreshing in place.

The omnipresent chat reuses the existing layout (`ui::layout`) and the
session extractors; only the SSE handler is new.
