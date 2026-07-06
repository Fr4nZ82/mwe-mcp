---
title: JWT, sessions, transport, and consumer delegation
area: design-notes
status: implemented
last_review: "2026-06-26"
---

# JWT, sessions, transport, and consumer delegation

This page is the canonical reference for the JWT schema and lifecycle,
the blacklist and delegation caches, the HTTP-only transport, the JWT
bearer middleware, the `X-MWE-Act-As` semantics, and the token-refresh
endpoint. Adjacent wiki pages carry the related surfaces:
[identity and ACL](../concepts/identity-and-acl.md) (dashboard owns
identity, single admin, first-run wizard, no `enrollment.yaml`),
[engine DB and migrations](engine-db-and-migrations.md) (the DDL behind
the tables listed under [Durable storage](#durable-storage)), and the
[config schema](../protocol/config-schema.md) (bind, port, secrets).
This page documents the **runtime model** behind
[`mwe-core::jwt`](../../crates/mwe-core/src/jwt.rs),
[`mwe-core::delegations`](../../crates/mwe-core/src/delegations.rs),
and the
[`mwe-mcp-server::mcp::auth`](../../crates/mwe-mcp-server/src/mcp/auth.rs)
middleware. The chassis is shipped end-to-end: bearer-JWT verify,
blacklist cache, dashboard sliding-TTL session, identity console,
consumer delegation table, the `X-MWE-Act-As` resolution, **SMTP
self-service password recovery, and TOTP two-factor authentication**
(see [§Password recovery and 2FA](#password-recovery-and-two-factor-authentication)).
The only residual is the `/mcp/token-refresh` endpoint — see [§Token
refresh](#token-refresh--staying-alive-without-admin-intervention).

## Transport is HTTP-only

`mwe-mcp` is exclusively an HTTP server. There is no stdio
transport: even a single developer running everything on their laptop
talks to `http://localhost:8742/mcp`. The reasons:

- mwe-mcp is persistent memory — it must outlive any client session.
- It is multi-consumer (Claude Code + Cursor + bot Telegram +
  built-in dashboard all coexist on the same workdir).
- The dashboard is a web UI on the same process, which already needs
  HTTP.
- The lockfile allows one process per workdir, which is
  incompatible with N stdio subprocesses sharing the workdir.

One Axum process, one port (default 8742), two route trees:

- `/mcp` — Streamable HTTP MCP transport (rmcp).
- `/dashboard/*` — built-in web UI.

Bind defaults to `127.0.0.1`; explicit `--bind 0.0.0.0` for exposure
through Cloudflare Tunnel, reverse proxy, etc.

## One JWT shape, three usage profiles

There is **one** kind of token. A JWT signed with the workspace
`MWE_TOKEN_SECRET` (HS256), payload:

```json
{
  "sender_id":      "frodo",
  "device_label":   "claude-code-pclavoro",
  "rate_limit_id":  "frodo-exposed-default",
  "jti":            "018f1234-5678-7abc-9def-0123456789ab",
  "iat":            1779000000,
  "exp":            1810536000,
  "isAdmin":        false,
  "consumer_id":    null,
  "consumer_class": "smart"
}
```

`consumer_class` is an optional claim. Values are `standard`
(default) and `smart`. The field is `#[serde(default,
skip_serializing_if = "ConsumerClass::is_standard")]`, so tokens
without the claim parse as `Standard` and freshly-issued
`Standard` tokens still omit the claim on the wire — the JSON example
above is for a `Smart` token; a `Standard` token omits the claim
entirely. See §[Consumer class — the smart vs
standard gate](#consumer-class--the-smart-vs-standard-gate) for what
the claim actually unlocks.

Three profiles share that shape, distinguished **only by the TTL
passed at issuance time** and by the presence of `consumer_id`:

| Profile | TTL | `consumer_id`? | Issued by | Used by |
|---|---|---|---|---|
| MCP local | 1y (`DEFAULT_INTERNAL_TTL`) | no | `/dashboard/tokens` (or `mwe-mcp token-issue` CLI fallback) | Consumer agents on the operator's own machine (Claude Code, Cursor, Antigravity, browser dashboard for the operator) |
| MCP exposed | 30d (`DEFAULT_EXPOSED_TTL`) | no | same | Consumer agents reached over the public internet (e.g. dev laptop hitting a Tunnel) |
| MCP consumer (bot) | 1y or 30d depending on hosting | **yes** | `/dashboard/tokens`, **Standard** consumer class | Multi-user bots (Samvise daemon, Discord bot, Slack bot) |
| Dashboard session | 60min sliding | no | `/dashboard/login` (password), or a single-use `dashboard_link` redeemed at `/dashboard/auth/link` (agent-driven) | Human in a browser/PWA |

Notable: the dashboard session token and the MCP local token are
exactly the same shape; they differ only by TTL and by where they
travel (cookie vs `Authorization` header). The middleware that handles
the dashboard session refreshes it on every interaction by minting a
new JWT with the same `sender_id` and a fresh `exp`+`jti`, no token
state stored anywhere.

### Magic-link redemption (`/dashboard/auth/link`, 0032)

`dashboard_link` (the MCP tool) does **not** hand the user a session
cookie directly. It mints a short-lived (10min), session-shaped JWT
bound to the act-as human and returns a URL pointing at the public
redemption route `/dashboard/auth/link?token=…&next=<deep-link>`. The
consumer relays that URL (e.g. on Telegram). On the first open the route:

1. `jwt::verify`s the token (signature + expiry + blacklist);
2. rejects anything whose `device_label` is not `dashboard-session`
   (a stolen MCP bearer JWT cannot be swapped for a session here);
3. **burns the token once** with `jwt::revoke_once` — a plain `INSERT`
   of the `jti` into `token_blacklist`, so the DB primary key is the
   serialization point and only one of two concurrent opens wins — then
   forces a synchronous `BlacklistCache::refresh` so the revocation is
   visible to the very next `verify` (closing the 60s cache window);
4. mints the **real** sliding session cookie (a separate, longer-lived
   JWT) for the token's subject;
5. 303-redirects to the validated `next` (same-origin `/dashboard/…`
   only — an open-redirect guard), token stripped from the address bar.

Single-use lives on the *link* token, not the cookie: refresh and
back-button keep working via the cookie after the link is spent. A
replayed or shared link fails at step 3 and shows a "link already used"
page rather than bouncing to `/login`. One limitation: a link minted by
a standard bot via act-as carries the **bot's** `is_admin` flag, not the
human's — fine for the `answer_proposal` landing, but admin-only intents
minted through act-as would land non-admin.

## Consumer class — the smart vs standard gate

An optional `consumer_class` claim on the token marks which side of
the LLM-budget split a consumer sits on:

| Class | Who | What it unlocks |
|---|---|---|
| `standard` (default) | Conversational consumers that route every turn through `wiki_ingest_message` and the server-side `ingest` LLM slot (openclaw, hermes, nanoclaw, dashboard chat). | The base surface. Nothing else. |
| `smart` | Consumers that bring their own LLM budget and want to mint smart-wiki pages themselves (Claude Code, Cowork, custom agents). | The family H write tools (`wiki_admin_push` / `wiki_admin_pull`); `wiki_admin_notify` is open to any class but is most useful to smart consumers building a `_briefing.md` round-trip. *(There is no runtime type forge: smart consumers do not mint custom smart-family types.)* |

The dispatcher folds the claim into
[`IdentityProfile.consumer_class`](../../crates/mwe-mcp-server/src/mcp/state.rs)
on every call. Per-tool handlers refuse standard tokens on family H
write tools with `403 requires_consumer_class_smart`; the dashboard
issuance form's **consumer-class radio** (Smart) flips the bit and
demands a `consumer_id` (the smart-consumer device id like `cc-laptop` —
`sender_id`/`owner_id` always points at the human owner). The CLI gets
the same control via `mwe-mcp token-issue --class smart`.

See [smart-wikis.md](smart-wikis.md) for the full design
context of why smart-class consumers exist at all — short version:
they avoid the "double LLM bill" that would happen if a Claude Code
session ran its `ingest` through the server's LLM after it already
spent its own.

## Revocation wire codes — `token_revoked` vs `invalid_token`

The middleware in [`auth::jwt_auth_middleware`](../../crates/mwe-mcp-server/src/mcp/auth.rs)
distinguishes two flavours of `401`:

| Wire code | Triggered by | Consumer should |
|---|---|---|
| `invalid_token` | Signature mismatch, expired `exp`, unsupported alg, malformed JWT bytes — anything that says "this never could authenticate against this server". | Hard-fail: stop queuing writes, surface the configuration error. The token might never have been valid. |
| `token_revoked` | `jti` is in `token_blacklist` (operator hit `mwe-mcp token-revoke` or the dashboard equivalent). | Degrade gracefully: keep the local `.mwe/wiki/` cache, surface "issue a fresh token", queue local edits, replay on the next session via `wiki_admin_pull` + `wiki_admin_push mode: upsert`. |

The two paths differ because the failure modes are different. A
signature mismatch usually means the consumer is talking to the wrong
server (lost config, deployment swap) — discarding local state would
correct the configuration, not lose work. A revoke is a legitimate
rotation of an actively-used token; the smart consumer was working
with valid state up to the revoke instant, and that work must
survive. The dedicated `token_revoked` code is wired and the policy is
documented in the bundled [`smart-consumer`
skill](../../crates/mwe-core/skills/smart-consumer.md), whose body
codifies the local `.mwe/state.json` shape.

## Three concepts the runtime distinguishes

Confusing these is the source of most bugs in identity layers:

- **Token holder** = the `sender_id` claim of the JWT presented in
  `Authorization: Bearer`. Who possesses the token.
- **Effective sender** = who is logically performing the call.
  Equal to the token holder for single-user clients; different
  (and controlled by the `X-MWE-Act-As` HTTP header) for multi-user
  consumer bots.
- **Fact owner** = application-level `owner_id` parameter on
  capture-style tools. Defaults to the effective sender. When
  different, it expresses cross-user attribution: the
  marker shows `{{owner=user:X sender=user:Y ...}}`.

## How the middleware resolves the effective sender

The Axum HTTP middleware for `/mcp/*` runs this rule on every request:

```
jwt_claims := verify(Authorization Bearer)
if jwt_claims is None: reject 401

requested_act_as := request.header("X-MWE-Act-As")   # Optional

if requested_act_as is None:
    effective_sender := jwt_claims.sender_id

elif jwt_claims.consumer_class != standard:
    reject 403 "act_as_requires_standard"   # smart consumers are mono-user by design

elif jwt_claims.consumer_id is None:
    reject 403 "act_as_requires_consumer"

elif requested_act_as not in lookup_delegation(jwt_claims.consumer_id):
    reject 403 "consumer not delegated for that sender"

else:
    effective_sender := requested_act_as

attach (jwt_claims, effective_sender) to request context
dispatch to rmcp tool handler
```

The MCP tools themselves do not take a `sender_id` parameter — they
read `effective_sender` from the request context. Tools that perform
cross-user attribution take `owner_id` as a regular application
parameter (separate concern entirely).

The builtin **`guest`** pseudo-identity rides this exact rule with no
middleware special case: `X-MWE-Act-As: guest` passes iff the admin put
`guest` in the consumer's `allowed_sender_ids` — the delegation grant is
the guest feature's enable switch. Guest can never be the *token holder*
(`validate_token_identity` refuses it for both classes); what a
guest-effective-sender turn may then do is the tool surface's contract
(ephemeral ingest, public-slice reads, permanent writes refused — see
[identity-and-acl.md §1](../concepts/identity-and-acl.md)).

## What this buys us

- **One verify path** for every call: same `jwt::verify`, same
  `BlacklistCache`. Whether the call comes from Claude Code or from
  the Samvise daemon, the first few lines of middleware are identical.
- **One secret to rotate**: `MWE_TOKEN_SECRET` rotation is a hard
  cutover that invalidates everything. One operator action,
  no per-tier coordination.
- **Delegation modifications propagate immediately**: removing
  `bilbo` from `samvise-prod`'s `allowed_sender_ids` takes effect on
  the next tool call from the bot, because the delegation table is
  queried per-call (with the same TTL caching as the blacklist). No
  token re-issuance, no bot restart.
- **Dashboard sliding TTL is just re-issuance**: every dashboard
  interaction mints a fresh JWT with the same `sender_id` and a new
  `exp`+`jti` (`exp = now + 60min`). No special state machine. A tiny
  client-side keepalive (`GET /dashboard/session/keepalive`, fired on
  user interaction, throttled to once every few minutes while the tab is
  visible) counts as such an interaction — so a long, entirely
  client-side form (the welcome primer's multi-step stepper makes no
  request until final submit) keeps the session alive instead of
  silently lapsing and bouncing the submit to `/login`.
- **Single-user clients know nothing**: Claude Code, Cursor,
  Antigravity send a Bearer header and that's it. They don't pass a
  `sender_id` parameter, don't pass `X-MWE-Act-As`, don't know
  consumer tokens exist.

## What this gives up (and the cheap fix if it matters)

A long-lived MCP-local token *could* be presented at the dashboard
URL and accepted. This is the same identity in both places, so it is
not a security problem — but it means that revoking only "the MCP
half" of a user's access requires revoking their entire identity.

If that ever bites, the cheap fix is to add a standard JWT `aud`
(audience) claim — backwards-compatible (omitted-`aud` tokens stay
valid until they expire), no shape change. This separate-audience
hardening is not implemented today.

## Durable storage

- **`MWE_TOKEN_SECRET`** — environment variable, ≥32 bytes. Generated
  by `mwe-mcp init` if absent and printed to stdout for the operator
  to capture. No fallback file.
- **`enrollment_users`** + **`enrollment_groups`** — identity SSOT,
  written via dashboard CRUD. No more `enrollment.yaml`.
- **`user_credentials`** — Argon2id hashes for users who can log in
  to the dashboard (system users like `samvise-bot` have no row here).
- **`user_invitations`** — one-shot UUIDv7 tokens, 24h TTL, used by
  the invitation-link flow for adding new users without the admin
  ever touching their password.
- **`consumers`** — the consumer registry. Its `system_user_id` column
  (migration 0029) materialises the diagonal-model binding consumer ↔
  system-user: a *standard* consumer's `consumer_register` records its
  own `sender_id` (the bot's credential-less system user) here, tying
  the deployment id to the memory identity it *is*. NULL for smart
  consumers (they are their human owner, Pattern A).
- **`consumer_delegations`** — `consumer_id → allowed_sender_ids[]`
  for the multi-user (standard, Pattern B) consumer pattern.
- **`token_blacklist`** — `jti → revoked`. Keyed by `jti`,
  consulted at every verify via `BlacklistCache`.
- **`password_resets`** (migration 0048) — one-shot UUIDv7 token,
  short TTL (`reset_ttl_minutes`, default 30), `consumed_at` burn-once.
  Backs the self-service forgot-password flow.
- **`user_2fa`** (migration 0049) — per-user TOTP enrollment: the
  shared secret **encrypted at rest** (XChaCha20-Poly1305, key derived
  from `MWE_TOKEN_SECRET` via SHA-256 — so rotating the secret
  invalidates enrollments), plus an `enabled` flag (`0` = enrolled but
  unconfirmed, `1` = active).
- **`user_2fa_recovery_codes`** — single-use recovery codes
  (high-entropy, SHA-256-hashed for direct lookup), `used_at` marks a
  spent code.
- **`pending_2fa`** — the second-factor challenge between a verified
  password and the session mint. Keyed by an **opaque random
  `challenge_id`** carried in a short-lived cookie — deliberately *not*
  a JWT, so it can never be swapped into a session cookie.
- **`enrollment_users.require_2fa`** (migration 0049) — per-user
  enforcement flag; the deployment-wide toggle lives in `engine_meta`
  (`auth.require_2fa_all`).

## In-memory caches

- **`BlacklistCache`** — full snapshot of `token_blacklist.jti`,
  refreshed on first call and every 60s thereafter
  ([`BLACKLIST_REFRESH_INTERVAL`](../../crates/mwe-core/src/jwt.rs)).
  Explicit `refresh()` for immediate propagation right after a revoke.
- **`DelegationCache`** ([`mwe-core::delegations`](../../crates/mwe-core/src/delegations.rs))
  — full snapshot of `consumer_delegations` as `consumer_id →
  HashSet<allowed_sender>`, same 60s TTL pattern
  ([`DELEGATION_REFRESH_INTERVAL`](../../crates/mwe-core/src/delegations.rs)).
  Both the dashboard upsert path
  ([`tokens.rs::upsert_delegation`](../../crates/mwe-dashboard/src/routes/tokens.rs))
  and the edit path
  ([`tokens.rs::delegation_submit`](../../crates/mwe-dashboard/src/routes/tokens.rs))
  call `refresh()` right after every write so the MCP middleware
  resolves the next tool call against the fresh row instead of
  waiting up to the TTL window. Shared as `Arc<DelegationCache>`
  between `McpState` and `DashboardState`, the same way the
  blacklist cache is shared.

## Bootstrap and admin lifecycle

`mwe-mcp init` does only three things:

1. Create workdir, acquire lockfile, open DB and run all migrations.
2. Generate `MWE_TOKEN_SECRET` if absent and print the `export` line.
3. Print the URL of `/dashboard/setup` for the first-run admin wizard.

It does **not**:

- Read any enrollment YAML (none exists anymore).
- Create any wiki directories (wiki roots are emergent).
- Issue any tokens (the dashboard does that after setup).

The first-run wizard (`/dashboard/setup`, served only while
`enrollment_users` is empty) asks for `admin_id` + password,
inserts the admin into `enrollment_users` with `is_admin=true`,
writes the Argon2id hash to `user_credentials`, emits a session JWT,
sets a cookie, redirects to `/dashboard/home`.

After that:

- New users via `/dashboard/users` → admin enters id + metadata →
  system creates an invitation row → admin shares the link →
  invitee opens the link, picks their own password.
- New tokens via `/dashboard/tokens` → issuance form with a
  Smart/Standard **consumer-class** radio; the Standard branch mints
  the bot's system user and adds the delegation list.
- **Self-service password recovery** over SMTP (when the admin has
  configured the `email:` backend in the Email section of
  `/dashboard/settings/me`):
  `/dashboard/forgot-password` mints a one-shot `password_resets` token,
  emails the link, and `/dashboard/reset-password/:token` sets a new
  Argon2id hash and bounces to login. See
  [§Password recovery and 2FA](#password-recovery-and-two-factor-authentication).
- Break-glass via CLI `mwe-mcp admin-reset --user <id>` (mints a fresh
  invitation link) — with `--clear-2fa` to also drop a lost
  authenticator. The admin can do the same from the dashboard: the
  `reinvite` button on `/dashboard/users` and the per-user **Reset 2FA**
  button on the edit-user page.

## Token refresh — staying alive without admin intervention

`POST /mcp/token-refresh` is the bot's way to extend its
token before expiry. The bot calls it with its current valid token
in `Authorization`; the server validates and returns a new token
with the same claims but a fresh `jti` and `exp`.

Behavior:

- The old `jti` is **not** added to `token_blacklist`. It expires on
  its own, avoiding the race where the bot has tool calls in flight
  with the old token at the moment of refresh.
- The delegation list is **not** carried in the token at all
  (`consumer_delegations` is queried per-call), so refresh doesn't
  need to refresh it.
- TTL profile is preserved via a `ttl_profile` claim (or derived
  from `original.exp - original.iat`).
- Hard failures (`401 revoked` / `401 secret_rotated` / `401
  expired`) require manual operator action — hard cutover, no
  auto-recovery.

Bot pattern: a cron in the bot calls `/mcp/token-refresh` when
`exp - now < 7 days`. On 401, log fatal + stop; intervention
required.

## Password recovery and two-factor authentication

These harden the **only** password surface mwe-mcp has — the human
`/dashboard/login` form — for a public user base. The MCP transport is
bearer-JWT with no interactive login, so neither touches it; system/bot
users (no `user_credentials` row) are exempt by construction.

### Self-service password recovery (SMTP)

The admin enables it by configuring an SMTP backend in the Email section
of the Settings page, `/dashboard/settings/me` (the typed `email:` config
section — host, port, TLS mode, From address; the SMTP password is read
from the env-var named by `password_env`, default `MWE_SMTP_PASSWORD`,
never the YAML). The
client is `lettre` over rustls (no OpenSSL). With it off (the default)
the login page hides the *Forgot your password?* link and the route is
inert.

The flow:

1. `POST /dashboard/forgot-password` resolves the email to a credentialed
   user (same lookup as login), mints a one-shot `password_resets` row,
   and **fires the email off the request path** so the response time is
   constant. The response is **identical** whether or not the email
   exists (anti-enumeration), and the endpoint is rate-limited per-email
   and per-IP.
2. `GET/POST /dashboard/reset-password/:token` validates the token,
   writes a fresh Argon2id hash, and **burns the token in the same
   transaction** (the conditional `UPDATE … WHERE consumed_at IS NULL`
   is the serialization point). It sends the user to `/login` — no
   auto-sign-in — so the next login re-runs any 2FA gate.

### Two-factor authentication (TOTP)

Opt-in TOTP (RFC 6238) via an authenticator app, with single-use
recovery codes. The second factor is **TOTP only** — no email/SMS OTP
(NIST SP 800-63B discourages it as a primary second factor); SMTP is for
recovery, not a factor.

- **Enrollment** at `/dashboard/settings/2fa`: mint a secret (stored
  `enabled=0`), show the QR + manual key, confirm a live code → `enabled=1`
  and a one-time batch of recovery codes.
- **Login challenge.** When the user has 2FA on, the password is only the
  first factor: login (and a redeemed **magic link** — a magic link does
  *not* bypass 2FA) hands off to `/dashboard/2fa`, which holds the state
  in `pending_2fa` keyed by an opaque cookie and mints the session only
  after a valid TOTP **or** recovery code. Attempts are rate-limited per
  challenge.
- **Enforcement.** Opt-in by default; an admin can require it per-user
  (the edit-user page) or deployment-wide (`auth.require_2fa_all`, the
  Settings toggle). An obliged-but-unenrolled user is **trapped on the
  setup page** by the session middleware (every other route redirects
  there) until they enroll. Break-glass for a lost authenticator:
  `mwe-mcp admin-reset --clear-2fa` or the dashboard **Reset 2FA** button.

## Walkthrough: Frodo on Claude Code, single-user

1. Admin opens dashboard, creates Frodo (already exists as the
   admin themselves), then `/dashboard/tokens` → "Issue new token"
   → form: `sender_id=frodo`, `device_label=claude-code-pclavoro`,
   `rate_limit_id=frodo-internal-default`, `ttl=internal`, no
   consumer checkbox. Token shown once with copy button.
2. Frodo pastes the token into Claude Code's MCP config as the
   `Authorization: Bearer ...` value for the `http://localhost:8742/mcp`
   server entry.
3. Claude Code starts up, opens an HTTP connection to mwe-mcp,
   presents the Bearer token on every request. No `X-MWE-Act-As`
   header is ever set.
4. mwe-mcp middleware: token verifies, no act-as header → effective
   sender = frodo. Tool calls run as frodo.

Cursor, Antigravity, and the browser dashboard for Frodo follow the
identical pattern with their own per-device tokens.

## Walkthrough: Samvise bot, multi-user

1. Admin creates `samvise-bot` as a regular user in
   `enrollment_users` (no `user_credentials` row — the bot doesn't
   log in via password).
2. Admin opens `/dashboard/tokens` → "Issue new token" → form:
   `sender_id=samvise-bot`, `device_label=sam-orchestrator-pcnuovo`,
   `rate_limit_id=internal-unlimited`, `ttl=internal`, **consumer
   checkbox ON**, `consumer_id=samvise-prod`, delegation checkboxes
   `[✓] frodo [✓] galadriel [✓] gollum [✓] bilbo`.
3. mwe-mcp signs the JWT (now carrying `consumer_id=samvise-prod`),
   inserts a row in `consumer_delegations(samvise-prod, [frodo, ...],
   now, frodo)`, shows the JWT once. Admin pastes it into Samvise's
   env (`SAMVISE_MCP_TOKEN=eyJ...`).
4. Samvise restarts (systemd unit picks up the new env). On boot it
   only stores the token; the secret stays on the mwe-mcp side.
5. Galadriel writes "manca il detersivo" on Telegram. Samvise
   orchestrator resolves chat_id `6994940390 → galadriel` via its own
   `channel_user_pairings` table (lives in `orchestrator.db`, not in
   mwe-mcp's `engine.db`).
6. Samvise sends:
   ```http
   POST /mcp HTTP/1.1
   Authorization: Bearer eyJ...
   X-MWE-Act-As: galadriel
   Content-Type: application/json
   
   { "jsonrpc": "2.0", "method": "tools/call",
     "params": { "name": "wiki_ingest_message",
                 "arguments": { "message": "manca il detersivo" } } }
   ```
7. mwe-mcp middleware: JWT verifies, `consumer_id=samvise-prod`,
   `X-MWE-Act-As=galadriel`, delegation lookup says galadriel ∈
   allowed → effective sender = galadriel. The `wiki_ingest_message`
   tool runs as galadriel. ACL applied as galadriel.
8. Later admin removes `bilbo` from samvise-prod's delegation list.
   The token in Samvise's env stays valid. Subsequent calls from
   Samvise with `X-MWE-Act-As: bilbo` get rejected; calls for
   frodo/galadriel/gollum keep working. No bot restart, no token
   re-issuance.
9. Every ~25 days, Samvise's internal cron calls
   `POST /mcp/token-refresh` with its current token. Gets a fresh
   token in response, swaps it in process memory, continues running.

## Walkthrough: admin viewing another user's data — NOT act-as

Frodo is logged into the dashboard as admin. He opens
`/dashboard/audit` to debug something Galadriel reported, and clicks
"View Galadriel's tool execution log".

The dashboard does **not** set `X-MWE-Act-As: galadriel`. The
`effective_sender` stays `frodo`. The tool called is
`tool_log_search(target_sender_id="galadriel", since=...)` — a
tool with an explicit `target_sender_id` parameter that is gated by
`if !jwt_claims.is_admin: reject 403`.

The audit log entry for this action says "frodo (isAdmin) searched
galadriel's logs", not "galadriel searched her own logs". The
distinction between act-as (multi-user bot delegation) and admin
override (privileged operator with explicit cross-user view) is
preserved both in code and in audit trail.

## What ships today vs what's not yet implemented

| Concern | Today | Status |
|---|---|---|
| `issue` / `verify_offline` / `verify` / `revoke` | `mwe-core::jwt` | shipped |
| `BlacklistCache` with 60s TTL + explicit refresh | `mwe-core::jwt::BlacklistCache` | shipped |
| `token_blacklist` table | migrations `0010` + `0011_revoked_by` | shipped |
| CLI `token-issue` / `token-revoke` / `token-list` | `mwe-mcp-server::main` | shipped |
| `mwe-mcp init` + `MWE_TOKEN_SECRET` generation | identity-free; dashboard wizard creates the first admin | shipped |
| `enrollment::validate` + `mirror_to_db` (no YAML loader) | invoked by dashboard CRUD form handlers | shipped |
| `user_credentials` / `user_invitations` / `consumer_delegations` migrations | `0012` / `0013` / `0014` (+`0017` email column, `0018` profile_initialized) | shipped |
| `mwe-mcp admin-reset --user <id>` CLI (issues `user_invitations` row, prints accept URL; `--clear-2fa` drops a lost authenticator) | `mwe-mcp-server::main` | shipped |
| SMTP self-service recovery (`email:` config + Settings-page email editor, `forgot-password` / `reset-password`, `password_resets` table, anti-enumeration + rate limit) | `mwe-dashboard::{email, routes::email_settings, routes::password_reset}`, migration 0048 | shipped |
| TOTP 2FA (enrollment, login challenge, recovery codes, per-user + global enforcement, magic-link reconciliation) | `mwe-dashboard::{twofa, routes::two_factor}`, migration 0049 | shipped |
| HTTP middleware for `/mcp/*` (verify JWT + identity attach) | `mwe-mcp-server::mcp::auth::jwt_auth_middleware` | shipped |
| Dashboard routes (setup wizard, login, users CRUD, groups CRUD, tokens issue/edit/revoke, settings, welcome wizard) | `mwe-dashboard::routes::*` | shipped |
| `X-MWE-Act-As` header resolution via `consumer_delegations` lookup in MCP middleware | `mwe-core::delegations::DelegationCache` shared `Arc` between `McpState` and `DashboardState`; `auth::jwt_auth_middleware` reads the header, validates against the cache, rewrites `IdentityProfile.sender_id`; dashboard write paths refresh the cache on every upsert/edit | shipped |
| `/mcp/token-refresh` endpoint | contract documented above in [§Token refresh](#token-refresh--staying-alive-without-admin-intervention); no handler yet | not implemented (planned — see the roadmap) |
