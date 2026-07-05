---
title: Web agent OAuth — the inbound authorization server for OAuth-connected smart consumers
area: design-notes
status: implemented
last_review: "2026-06-29"
---

# Web agent OAuth (`webagentoauth`)

An OAuth MCP client connects to a running mwe-mcp as the **user's own Smart
consumer** by running a standard OAuth dance against an **inbound OAuth 2.1
authorization server** mwe-mcp exposes. The user logs in to mwe-mcp from inside
the client and approves the connection; **no token is minted or copied by hand**.
Two kinds of client use the same flow, told apart by their OAuth **redirect**:

- the **claude.ai web app** (or any hosted, bridge-less custom connector) — an
  `https` redirect → the **`Web`** connection profile (trimmed tool surface,
  mirror-less, one dedicated wiki);
- **Claude Code** and other local CLIs — a loopback `http://localhost:<port>`
  redirect (RFC 8252 native app) → the **`Local`** profile (full tool surface,
  the project-bound smart-consumer posture).

Once connected the agent is a first-class smart consumer: it reads and writes its
own wikis through the `wiki_admin_*` family. A web client gets one dedicated wiki;
a local CLI gets a dedicated **operational** wiki (its general memory, behaviour
rules, a `conversations.md` log) **plus** the per-project wikis it authors as it
works. This is the same OAuth server that, before, only fronted claude.ai — the
profile split is what lets Claude Code drop its old static-token + hooks bridge
and connect token-lessly here too.

This is the **opposite direction** from mwe-mcp's own subscription login
([config schema → Anthropic / Claude Code OAuth](../protocol/config-schema.md)),
where mwe-mcp is the OAuth *client* signing in to Anthropic for its LLM calls.
Here mwe-mcp is the OAuth *authorization server* an external MCP client signs in to.

The flow is named and mounted under its own **`webagentoauth`** URL namespace (its
own issuer, discovery, and `/authorize` · `/token` · `/register` endpoints), so a
second inbound OAuth flow can coexist as a disjoint route set. `/mcp`'s
protected-resource discovery names **this** issuer specifically, so an MCP client
only ever follows the `webagentoauth` flow.

## The model

- **It is the existing `Smart` class, not a new consumer class.** `consumer_class`
  (`smart` ⇄ `standard`) is a *capability* axis — whose LLM does the memory work
  and how the wiki is authored. claude.ai brings its own LLM and authors its wiki
  authoritatively, so it is **Smart**. "Web" is a *transport / integration mode*
  (remote, OAuth-fronted, bridge-less, explicit-invocation), **orthogonal** to the
  class; it is carried as a `ConsumerProfile` (`Local` ⇄ `Web`) on the JWT, not a
  third `ConsumerClass`. The product-facing "Connect claude.ai" affordance is a
  preset in the [Bridges catalog](../development/agents-bridges.md), not an enum
  variant.
- **Governance is wiki-level ACL + owner-match, with no scopes.** The issued token
  is simply "smart consumer owned by user X". What confines it is the existing
  smart-wiki write contract in
  [`wiki_admin.rs`](../../crates/mwe-core/src/wiki_admin.rs): `consumer_class=smart`
  **and** `wiki.owner_user == token.owner_user` **and** `_meta.smart == true`, so
  the agent can write only that user's own smart wikis. **No per-fragment ACL is
  involved** — that is the standard-wiki / `wiki_ingest_message` world
  ([smart-wikis](smart-wikis.md), [redaction-policy](redaction-policy.md)); a smart
  wiki carries one wiki-level ACL in `_meta` (`scope` + `shared_with`,
  read+notify only). **No OAuth scopes** — the ACL is the governance.
- **The consumer name is chosen at consent, not pre-provisioned.** The smart token
  carries `owner_id` (the logged-in user, from the OAuth identity) and
  `consumer_id`/`device_label` (the name of this connection), confirmed on the
  consent screen and defaulting to the `client_name` the client declared at
  Dynamic Client Registration. `consumer_register` is idempotent, so
  re-authentication reuses the same consumer identity and keeps op-log attribution
  coherent.
- **One dedicated wiki per agent, bound at consent.** A web client has no working
  directory or VCS, so its smart wiki cannot be *project*-bound the way Claude
  Code's is. Each connecting agent gets **one dedicated smart wiki per user**, named
  and forged at consent (e.g. claude.ai → `<user>-<connection>`, a second agent →
  its own), a child of the user's identity wiki. On first connection it is forged
  via `wiki_admin::push mode=create` (smart, owned by the user); the call is
  idempotent, so re-auth reuses it.
- **Mirror-less, stateless per session.** Claude Code holds a local working copy
  (`.mwe/wiki/` + `.mwe/state.json`, checksums, op-log reconciliation); a web client
  has no filesystem, so none of that persists. The web agent operates **stateless
  per session**: `smart_bootstrap` → `wiki_admin_pull` into context → edit →
  `wiki_admin_push` of **only the touched pages** (upsert, never re-emit the whole
  wiki). This is served by a dedicated mirror-less skill (see
  [Skills and recall without a bridge](#skills-and-recall-without-a-bridge)), not a
  reuse of `smart-consumer.md`, which assumes the local mirror and lease discipline.
- **Self-service per enrolled user.** Any enrolled user — not only the admin — can
  connect their own agent and own the resulting dedicated wiki. The `/authorize`
  login authenticates whoever signs in; the smart token is owned by that user.

## What it reuses

The OAuth server is a thin façade over primitives that already ship; it adds no new
auth subsystem and **does not change `/mcp` validation**:

- **Human authentication** — `enrollment_users` + `user_credentials` (email +
  Argon2id), `/dashboard/login`, the sliding session cookie
  ([jwt-and-session-model](jwt-and-session-model.md)). The `/authorize` consent
  screen sits *behind this existing login*; an unauthenticated visitor is bounced to
  `/dashboard/login?next=…` and back.
- **Token minting** — `jwt::issue(&secret, &claims)`
  ([`jwt.rs`](../../crates/mwe-core/src/jwt.rs)), the exact call
  `/dashboard/tokens/issue` already uses. The `/token` endpoint returns the **smart
  JWT itself** as the `access_token`, so the existing bearer-JWT middleware on `/mcp`
  validates it with zero changes.
- **Single-use code pattern** — the same compare-and-set shape the magic-link
  redemption uses; here it lives in the store's `DELETE … RETURNING` on the
  authorization code.
- **Smart-consumer runtime** — the whole `wiki_admin_*` + `smart_bootstrap` +
  briefing surface is reused verbatim ([smart-wikis](smart-wikis.md)). This area
  changes only *how the consumer authenticates* and adds the mirror-less skill and
  the web tool profile.

## The endpoints

Implemented in [`mwe_dashboard::routes::webagentoauth`](../../crates/mwe-dashboard/src/routes/webagentoauth.rs),
merged at the root of the HTTP tree via `webagentoauth_public_router` (OAuth 2.1
shape: PKCE mandatory; mwe-mcp is both authorization server and protected resource):

- **Discovery** — `/.well-known/oauth-authorization-server` (endpoint metadata) and
  `/.well-known/oauth-protected-resource` (advertising `/mcp` as the resource and
  pointing at the AS). The protected-resource document also answers at the RFC 9728
  path-insertion variant `/.well-known/oauth-protected-resource/mcp`, and the `/mcp`
  401 carries `WWW-Authenticate: Bearer resource_metadata="…"` so a client can
  discover the issuer. The origin is derived from the request `Host`.
- **Dynamic Client Registration** — `POST /webagentoauth/register`; the client
  self-registers and its `client_name` is kept as the default consumer label.
  Registration is open — the human consent gate stands between a registered client
  and any token.
- **Consent / authorization** — `GET`/`POST /dashboard/webagentoauth/authorize`
  (authorization-code + PKCE). It validates the client + PKCE, self-verifies the
  dashboard session (bouncing to `/dashboard/login?next=…` when absent), renders a
  consent page that names the connection and the dedicated wiki it binds to, and on
  approve forges the smart wiki, registers the consumer, and issues a single-use
  authorization code → redirect back. Redirect-URI validation is exact, **except a
  loopback redirect matches port-agnostically** (RFC 8252 §7.3) — a native CLI's
  callback port is assigned per run, so an exact-port check would reject it.
- **Token** — `POST /webagentoauth/token` for the `authorization_code` and
  `refresh_token` grants. It mints a **short-lived** (1 h) smart-JWT access token via
  `jwt::issue` (`consumer_class=smart`, `profile=web`, `owner_id` = the approving
  user, `consumer_id` from the code, `is_admin` re-resolved from `enrollment_users`),
  plus a 30-day **refresh token** rotated on every use. Short access token + refresh
  is chosen over a long-lived JWT because this endpoint is public to a third-party
  client; an access token otherwise lapses in ≤ 1 h.

## The data layer

[`mwe_core::oauth_server`](../../crates/mwe-core/src/oauth_server.rs) over migration
`0044_webagentoauth.sql` ([engine DB and migrations](engine-db-and-migrations.md)):
`register_client` / `lookup_client`, `issue_auth_code` / `consume_auth_code`
(`DELETE … RETURNING`, single-use), `issue_refresh_token` / `rotate_refresh_token`
/ `revoke_connection`, `list_connections`, and `verify_pkce_s256`. `is_admin` is
re-resolved at mint time, never stored. The write paths clean up after
themselves: `issue_auth_code` purges expired codes on every issue (abandoned
flows are the only rows redemption never deletes), and `issue_refresh_token` /
`rotate_refresh_token` prune the connection's revoked/expired rows in the same
transaction that lands the fresh active row — a connection holds one live row,
not one per rotation.

## The connection profile

A `ConsumerProfile` (`Local` ⇄ `Web`) rides the JWT
([`jwt.rs`](../../crates/mwe-core/src/jwt.rs)). The `webagentoauth` `/token` mint
**derives it from the client's registered redirect_uris**: all-loopback (RFC 8252
native app — Claude Code, any CLI) → `Local`; otherwise (an `https` hosted
redirect — claude.ai) → `Web`. It is re-resolved at every mint, so it also holds
across a refresh with no extra state to persist. `schemas::tools_for(profile)`
then shapes what `tools/list` advertises: `Local` sees the full `all_tools()`
roster (including `skill_fetch`, the cooperative leases, the event loop — a local
CLI can use them); `Web` sees a trimmed `WEB_TOOLS` whitelist — the conversational
+ smart-authoring tools a bridge-less, no-local-filesystem client can actually use:

`wiki_search`, `wiki_navigate`, `wiki_read`, `wiki_ingest_message`,
`wiki_ingest_external`, `wiki_admin_notify`, `recall_core_global`,
`smart_bootstrap`, `wiki_admin_push`, `wiki_admin_pull`.

It keeps the full smart-wiki management surface — `smart_bootstrap` (discover its
wiki), `wiki_admin_pull` (read its whole wiki), `wiki_admin_push` (write it): these
are **server-side** reads/writes a web consumer still needs even with no local copy.
It drops only the genuinely local/plumbing tools: the `wiki_admin_lease_*` pair
(local multi-device coordination), the event-drain loop, registration / ops
plumbing, and the skill catalog. **Call-time authorization is unchanged** — the
smart-class / ACL / owner-match gates still enforce; the profile only cuts routing
noise. To steer routing for a model that never loads a skill, the recall-vs-search
rule lives in the **tool descriptions** themselves (`recall_core_global` is
owner-scoped → use `wiki_search` for other entities; see
[mcp-tools](../protocol/mcp-tools.md)).

## Skills and recall without a bridge

A bridge-less client gets mwe-mcp's value only through **explicit tool invocation**;
two host-bridge affordances are absent and handled here:

- **The smart-consumer contract is delivered without a hook.** The MCP `instructions`
  field returned on `initialize` points a web smart consumer at the bundled
  [`web-smart-consumer`](../../crates/mwe-core/skills/web-smart-consumer.md) skill —
  the mirror-less protocol (stateless `smart_bootstrap` → pull → push-delta, never
  re-emit the whole wiki) plus the explicit-invocation posture. For claude.ai, whose
  native flow is **custom-skill upload** rather than MCP `skill_fetch`, the same skill
  is also served as a name+description `.md` at `GET /webagentoauth/skill.md`, funnelled
  from the consent page and the Bridges tab ("download the skill → upload it in the
  client").
- **No always-on recall.** Without a per-turn bridge hook there is no automatic recall
  block; the agent recalls and saves only when it (or the user — "search in MwE", "save
  this chat") chooses to call a tool. This is explicit-invocation memory by
  construction, and is the accepted posture for the web transport. The web consumer's
  working model: personal/daily-life facts → `wiki_ingest_message` (the server files
  them into the user's personal memory); project/design knowledge and a dated
  conversation chronicle (a `conversations.md` bullet list) → `wiki_admin_push` to its
  own dedicated wiki, reloaded each session via `smart_bootstrap` + `wiki_admin_pull`.

## Lifecycle and connection management

A **Web agent connections** section on the
[tokens page](../../crates/mwe-dashboard/src/routes/tokens.rs) lists active
connections (`oauth_server::list_connections`, one row per `(sender, consumer,
wiki)`) with a **Disconnect** action (`POST /tokens/connection/revoke` →
`revoke_connection`): it revokes the refresh tokens so renewal stops, the
short-lived access token lapses on its own (≤ 1 h — no per-jti access-token
tracking, a deliberate call for a 1 h window), and the dedicated wiki is **kept**.
A "Connect the claude.ai web app" section on the
[Bridges catalog](../../crates/mwe-dashboard/src/routes/bridges.rs) (public + the
dashboard tab) shows the `<origin>/mcp` URL to paste and walks the OAuth approve —
instructions, not an installer.

## Housekeeping

[`mwe_core::housekeeping::run`](../../crates/mwe-core/src/housekeeping.rs) drains
the residue the inline paths above cannot reach retroactively. It runs at `serve`
boot and again after a dashboard wiki deletion
([`wiki_view::delete_apply`](../../crates/mwe-dashboard/src/routes/wiki_view.rs)),
is best-effort (a failure logs and the server keeps serving), and reports counts
per sweep:

- **Expired authorization codes** — deleted outright.
- **Stale refresh rows** — revoked/expired rows are deleted, but per
  `(sender, consumer, wiki)` connection the **newest stale row is kept while no
  active row exists**: the `consumers` table carries no wiki column, so that row
  is the durable record binding a web-agent consumer to its smart wiki, and the
  dangling-consumer sweep reads the binding from it.
- **Dangling web-agent consumers** — a consumer whose **every** bound wiki no
  longer exists on disk is removed with its delegations and OAuth rows (the
  smart wiki was deleted; nothing the row points at can ever serve again).
  Token-registered consumers (`system_user_id` set) are never touched, and a
  merely-disconnected consumer whose wiki survives is kept — a reconnect reuses
  it. Presence is read from a **single up-front tree walk**: a wiki counts as
  gone only when that successful walk does not list it. If the walk itself
  fails (a malformed or half-written `_meta.md`, a transient IO fault), the
  sweep is **skipped**, not run against a phantom-empty tree — an unreadable
  tree means "cannot judge", never "everything gone", so one bad file can't
  wipe every consumer's registration. The post-delete invocation makes the
  common case immediate: deleting a smart wiki from the dashboard sweeps its
  consumer in the same request.

## Deployment dependency

claude.ai is a hosted app and cannot reach `localhost`, so this needs the server on
a **public HTTPS endpoint**. rmcp's streamable-HTTP server defaults `allowed_hosts`
to loopback and would 403 a public `Host` as a DNS-rebinding guard, so it is
disabled (`disable_allowed_hosts()`) — safe because `/mcp` is Bearer-gated, not
cookie-gated. A public endpoint also makes first-run-secure-setup matter (the
"first visitor to `/dashboard/setup` becomes admin" land-grab must be gated); that
hardening track is owned separately.

## Known edges

- **No optimistic concurrency.** Mirror-less push carries no `expected_op_log_head`,
  so a dashboard edit racing a web-agent push could lose an update. Rare for a single
  user + single agent; revisit if multi-writer on one wiki becomes real.
- **Slug collision on a second connection of the same agent.** Two claude.ai
  workspaces default to the same slug; the second needs a disambiguation suffix (the
  consent screen surfaces the existing binding first).
