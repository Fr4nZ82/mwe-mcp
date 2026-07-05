---
title: MCP dispatcher — rmcp ServerHandler + per-tool wiring
area: design-notes
status: implemented
last_review: "2026-06-28"
---

# MCP dispatcher

`mwe-mcp-server::mcp` is the dispatcher that turns the in-process
[`mwe-core`](../architecture/overview.md) primitives into the MCP tool
surface documented in
[`tool-reference.md`](../protocol/tool-reference.md) (the live roster
and per-tool status live in [`mcp-tools.md`](../protocol/mcp-tools.md)).
The set is registered once in
[`schemas::all_tools()`](../../crates/mwe-mcp-server/src/mcp/schemas.rs),
which is the single source of truth — `dispatch()` matches the same
names and the `schemas` test asserts the roster. It wraps
[rmcp](https://docs.rs/rmcp/1.7.0/) 1.7 and exposes the Streamable HTTP
transport. mwe-mcp is **HTTP-only** by design: consumers, local
or remote, connect over HTTP with a per-call JWT.

## Module shape

```
crates/mwe-mcp-server/src/mcp/
├── mod.rs        — McpHandler + ServerHandler impl + dispatch()
├── state.rs      — McpState (handles) + IdentityProfile (auth result)
├── auth.rs       — Axum JWT bearer middleware for /mcp
├── error.rs      — ToolErrorClass + ToolError + into_mcp_error mapper
├── schemas.rs    — JSON Schema literals (one per tool) + `all_tools()`
└── tools.rs      — one handler function per tool
```

The split is deliberately shallow: every handler is `pub(super)` and
called from `dispatch` by name. No trait object — a plain `match`
keeps the dispatcher easy to grep.

## Transport

mwe-mcp is **HTTP-only** (see
[`jwt-and-session-model.md` §Transport is HTTP-only](jwt-and-session-model.md)).
There is no stdio transport: the dashboard (where proposals are
approved) is mandatory and shares the Axum listener, and the
single-writer lockfile precludes a second process on the same workdir.

| Transport | Endpoint | Auth | Wired by |
|---|---|---|---|
| Streamable HTTP | `POST /mcp` (rmcp Tower service) | `Authorization: Bearer <JWT>` | [`cmd_serve_http`](../../crates/mwe-mcp-server/src/main.rs) |

`auth::jwt_auth_middleware` verifies the bearer via
[`mwe_core::jwt::verify`](../../crates/mwe-core/src/jwt.rs) (signature +
`exp` + blacklist cache), attaches the resulting [`IdentityProfile`] to
the Axum request extensions; rmcp surfaces it in
`RequestContext.extensions.get::<http::request::Parts>()` and
`resolve_identity` lifts it back out. A call with no attached identity
is rejected (`SenderUnauthorized`).

## Identity → tool contract

[`IdentityProfile`] is the only auth shape per-tool handlers see:

```rust
pub struct IdentityProfile {
    pub sender_id: String,             // user_id
    pub device_label: String,          // for audit row
    pub rate_limit_id: String,         // config-driven profile
    pub consumer_id: Option<String>,   // bot/orchestrator id
    pub consumer_class: ConsumerClass, // standard | smart
    pub is_admin: bool,                // UI gating only — never bypasses ACL
}
```

Handlers enforce two invariants in a per-tool helper:

- `forbid_sender_mismatch(identity, args.sender_id)` ⇒ `403 sender_token_mismatch`
  if the tool's optional `sender_id` arg differs from the token.
- `enforce_consumer_match(identity, args.consumer_id)` ⇒ `403 sender_unauthorized`
  if a consumer-bound tool (`events_poll`/`ack`) is called by a token
  bound to a different consumer. Admin tokens get a fallback for
  debugging.

`is_admin` only widens the surface in two places:
[`tool_log_search`](../../crates/mwe-mcp-server/src/mcp/tools.rs)
(admins see every sender's rows) and
[`dashboard_link`](../../crates/mwe-mcp-server/src/mcp/tools.rs)
(admins can mint links for `settings` / `audit` / `costs`). It never
bypasses an ACL `can_read`.

`consumer_class` gates most of the **H family** (smart-wiki
admin): the write/read tools (`wiki_admin_push` / `wiki_admin_pull`)
and the two cooperative-lease tools (`wiki_admin_lease_acquire` /
`wiki_admin_lease_release`) refuse `Standard` tokens with
`403 requires_consumer_class_smart`, then check
`wiki.owner_user == token.owner_user` and the target wiki's
`smart:` flag (the per-wiki bool read straight from its
`_meta.md`) before
dispatching to
[`mwe_core::wiki_admin`](../../crates/mwe-core/src/wiki_admin.rs). The
remaining tool in the family, `wiki_admin_notify`, is intentionally
unrestricted on class (only on read access) so a standard openclaw can
relay a user observation into a smart consumer's `_briefing.md`. See
[`smart-wikis.md`](smart-wikis.md) for the full design.

## The tool roster

The authoritative list is
[`schemas::all_tools()`](../../crates/mwe-mcp-server/src/mcp/schemas.rs);
the table below is a human-readable mirror grouped by the same
families A–K.

| Tool | Status | Backing module |
|---|---|---|
| `wiki_ingest_message` | ✅ full | [`mwe_core::ingest`](../../crates/mwe-core/src/ingest.rs); smart wikis — those whose `_meta.md` `smart:` flag is `true` — are filtered from `available_wikis` before the LLM router call |
| `events_poll` | ✅ full | [`mwe_core::events::poll_events`](../../crates/mwe-core/src/events.rs) |
| `events_ack` | ✅ full | [`mwe_core::events::ack_events`](../../crates/mwe-core/src/events.rs) |
| `wiki_read` | ✅ full — per-region ACL projection via `render::render_for_sender` (the sole read gate; no wiki-level gate above it) | [`mwe_core::wiki::WikiHandle`](../../crates/mwe-core/src/wiki.rs) + [`mwe_core::render`](../../crates/mwe-core/src/render.rs); returns `content_rendered_for_sender` + `redacted_count` (there is no `fully_redacted` flag — the body equals the canonical `[!redacted]` callout when the whole page collapses) |
| `wiki_search` | ✅ full — carries the smart-wiki filter as the boolean `scope.smart` (`true` keeps smart-wiki-only hits, `false` excludes them) | [`mwe_core::recall::wiki_search`](../../crates/mwe-core/src/recall.rs) |
| `tool_log_search` | ✅ full | [`mwe_core::audit::search`](../../crates/mwe-core/src/audit.rs) |
| `wiki_lint` | 🟡 partial — implemented-check roster is canonical in [`mcp-tools.md`](../protocol/mcp-tools.md); the `Check` enum SSOT is [`mwe_core::lint`](../../crates/mwe-core/src/lint.rs) | [`mwe_core::lint`](../../crates/mwe-core/src/lint.rs) |
| `consumer_register` | ✅ full | [`mwe_core::consumers::register`](../../crates/mwe-core/src/consumers.rs) |
| `wiki_ingest_external` | 🟡 partial — `source.type=inline` only; `file` / `git` / `url` return `not_implemented_phase_c` | full forge/promote pipeline not yet implemented |
| `dashboard_link` | ✅ full | mints a **single-use** link JWT (10min, `device_label=dashboard-session`) whose URL targets `/dashboard/auth/link`; redemption burns the `jti` (`jwt::revoke_once`) and sets the sliding session cookie |
| `wiki_admin_push` | 🟡 partial — `mode=create` + `mode=upsert`; `snapshot_replace` mode and `expected_op_log_head` enforcement not yet supported. Carries `actor_kind` + `pre_image_json` columns on `wiki_admin_op_log` and an optional `mark_processed: Vec<String>` input + `marked_processed: Vec<String>` output | [`mwe_core::wiki_admin::push`](../../crates/mwe-core/src/wiki_admin.rs); writes `wiki_admin_op_log` row with `payload_hash` + `actor_kind` + `pre_image_json`; `mark_processed` batch-updates `wiki_briefing_items.processed_at` atomically inside the push transaction (fail-fast on unknown / cross-wiki ids → `400 unknown_briefing_item_id`; cap 50 ids → `400 too_many_briefing_items`) |
| `wiki_admin_pull` | 🟡 partial — full pull; `since_op_log_id` delta-pull not yet supported | [`mwe_core::wiki_admin::pull`](../../crates/mwe-core/src/wiki_admin.rs) |
| `wiki_admin_notify` | ✅ full — optional three-layer `kind` classification (`observation` / `reasoning` / `external`) routed via [`briefing::BriefingKind`](../../crates/mwe-core/src/briefing.rs) | [`mwe_core::briefing::notify`](../../crates/mwe-core/src/briefing.rs); appends to `_briefing.md` + inserts `wiki_briefing_items` row; rate-limited 50/wiki/h |
| `wiki_admin_lease_acquire` | ✅ full — smart only | [`mwe_core::wiki_admin_leases::acquire`](../../crates/mwe-core/src/wiki_admin_leases.rs); opt-in cooperative lease — while held, a `wiki_admin_push` from a different `(sender_id, consumer_id)` fails `423 wiki_locked_by_lease`; re-acquire by the same caller extends the row; TTL default 60 s, max 300 s |
| `wiki_admin_lease_release` | ✅ full — smart only | [`mwe_core::wiki_admin_leases::release`](../../crates/mwe-core/src/wiki_admin_leases.rs); one-shot — releasing an already-released / expired / foreign lease returns `404 not_found` |
| `skill_list` | ✅ full | [`mwe_core::skills::list_bundled`](../../crates/mwe-core/src/skills.rs); bundled via [`rust_embed`](https://docs.rs/rust-embed) from `crates/mwe-core/skills/` (bundled catalog only) |
| `skill_fetch` | ✅ full | [`mwe_core::skills::fetch`](../../crates/mwe-core/src/skills.rs); bundled lookup; carries `etag = sha256(content)[..32]` matching the `skill_list` value for cache short-circuits |
| `smart_bootstrap` | ✅ full — smart only | [`mwe_core::smart::bootstrap`](../../crates/mwe-core/src/smart.rs); walks the wiki tree filtering by (the `_meta.md` `smart:` flag + `scope == User(caller)`), aggregates `briefing::counts_by_kind` + `briefing::list_items` + latest `wiki_admin_op_log` per row, sorts by hint-match → recency → wiki_id |
| `recall_core_global` | ✅ full — smart only | [`mwe_core::smart::recall_core_global`](../../crates/mwe-core/src/smart.rs); thin wrapper over `recall::wiki_search` with caller-owned filter + smart-wiki post-exclusion (smart-flagged rows dropped); clamps `limit ∈ [1, 20]`, overfetches ×4 to compensate post-filter |

The three proposal write tools `structure_proposal_apply` / `_confirm`
/ `_revert` are **not** on MCP. The dashboard handlers in
[`crates/mwe-dashboard/src/routes/proposals.rs`](../../crates/mwe-dashboard/src/routes/proposals.rs)
call the chassis functions in `mwe-core::proposals` directly. The
agent consumer surfaces the `auto_applied` event link and lets the
user act in the browser.

The 🟡-partial tools whose unimplemented branches are reachable (e.g.
`wiki_ingest_external` with `source.type` of `file` / `git` / `url`)
return the wire-stable `not_implemented_phase_c` error class — kept
verbatim so consumers can branch forward-compat (call the tool, get a
clear `501`, decide whether to fall back).

## JSON Schema strategy

[`schemas::all_tools()`](../../crates/mwe-mcp-server/src/mcp/schemas.rs)
returns the roster of `rmcp::model::Tool` instances built from inline
`serde_json::json!` literals — it is the single source of truth for
the surface (don't hardcode a count here; read it from the code). We
do **not** derive schemas via `schemars` because:

- The wire shapes follow [`tool-reference.md`](../protocol/tool-reference.md)
  closely; a literal block keeps the schema next to the spec text.
- Inputs are deserialised through `serde_json::Value` anyway — the
  schema is a contract, not a serialisation source.
- The dispatcher already validates per-field semantics inside each
  handler (`parse_args::<T>` + custom checks).

Tests in `schemas.rs` (`all_twenty_tools_present_with_unique_names`)
assert the roster has the exact registered count, that every name is
unique, and that every schema has `type: object`. The count baked into
the test is the contract: bump it deliberately when a tool is added or
removed — the test name encodes the current cardinality (twenty).
The test also asserts the proposal write tools are NOT
present, to catch accidental re-additions, and a companion test pins
`wiki_admin_push`'s optional `mark_processed` array.

## HTTP companion endpoints

Beyond the MCP surface, sibling Axum sub-routers are mounted
alongside `/dashboard` and `/mcp` in `main.rs::cmd_serve_http`
(`/media`, `/skills`, the `/connect/hooks` bundles, and the public `/` +
`/bridges` bridge-distribution surface — the
full mount map is in the
[architecture overview](../architecture/overview.md)):

The bearer-gated `/media/*` byte pair
(`mwe_mcp_server::http_media::router()` — the same
`jwt_auth_middleware` as `/mcp`, act-as aware): `POST /media`
multipart upload and `GET /media/<catalog_id>` per-media-ACL serving.
Design SSOT: [media pipeline](media-pipeline.md).

And the public-read `/skills/*` sub-router via
`mwe_mcp_server::http_skills::router()`:

- `GET /skills` — JSON `{ skills: [...] }` with bundled metadata
  only (`Cache-Control: public, max-age=300, must-revalidate`).
- `GET /skills/<name>.md` (also `<name>` without `.md`) — raw
  markdown body with `Content-Type: text/markdown; charset=utf-8`,
  `ETag: "<sha256[..32]>"`, honours `If-None-Match → 304 Not
  Modified`.

Custom skills are intentionally **MCP-only**: the HTTP path has no
JWT context, so exposing custom would allow enumeration of other
owners' catalogs. The bundled catalog is documentation that ships
with the binary and is safe to serve unauthenticated.

## Audit trail

Every dispatcher call writes exactly one row in `tool_executions`
through [`record_audit`](../../crates/mwe-mcp-server/src/mcp/mod.rs).
The write is fire-and-forget (`tokio::spawn`) so a slow audit DB
never blocks the consumer response.

Fields per row:
- `timestamp` (server ISO 8601)
- `tool_name` (canonical wire string, never a label)
- `sender_id` / `device_label` (the HTTP-only path stamps
  `device_label = "mcp"`)
- `rate_limit_id` — the `rate_limit_id` JWT claim is **parsed into the
  `IdentityProfile` but not enforced** (no tower-governor wiring yet);
  the audit helper currently writes `NULL` for it
- `args_hash` (SHA-256 hex of the JSON args — never the raw payload;
  the `tool_executions` DDL lives in
  [`engine-db-and-migrations.md`](engine-db-and-migrations.md))
- `result_summary` (≤240 chars of the JSON response, or the error
  message on failure)
- `latency_ms` (wall-clock, dispatcher-measured)
- `error` (one of the `ToolErrorClass` wire strings, `NULL` on success)
- `cost_estimate` (still `NULL` — LLM token accounting is not yet
  wired)

The matching `tool_log_search` MCP tool reads back through
[`mwe_core::audit::search`](../../crates/mwe-core/src/audit.rs).

## Error mapping

[`ToolErrorClass`](../../crates/mwe-mcp-server/src/mcp/error.rs) is the
canonical, single-source set of wire strings (`as_str()` is the SSOT —
read the enum rather than trusting this excerpt to be exhaustive):

```
invalid_input | sender_unauthorized | sender_token_mismatch |
consumer_not_registered | not_found | internal_error |
service_unavailable | not_implemented_phase_c |

# family H (smart-wiki admin):
requires_consumer_class_smart | wiki_owned_by_other_user |
wiki_type_not_admin_writable |
wiki_type_not_briefing_capable | conflicting_op_log_head |
rate_limited | unknown_briefing_item_id | too_many_briefing_items |

# family H — cooperative lease:
wiki_locked_by_lease |

# family K (smart_bootstrap, recall_core_global):
# (no dedicated wire codes; reuses requires_consumer_class_smart +
#  invalid_input + internal_error)
```

`into_mcp_error` folds the class into both the `tool_executions.error`
column and the JSON-RPC error response (`data.error_class`), so a
consumer reading the audit log and a consumer reading the error
response see the same string.

## WAL recovery sweep

`bootstrap_state` runs the
[`mwe_core::wal::rollback_stale_proposals`](../../crates/mwe-core/src/wal.rs)
and `rollback_stale_rems` drivers with a [`NoopInverse`] at startup.
The drivers flip every stale row (`pending` / `in_progress` for
>5 minutes) to `failed` with `error_msg = "rolled_back_by_startup"`.

`NoopInverse` is the right shape today because:
- REM cycles are restartable; missing per-step rollback is OK.
- Per-kind apply handlers (`promote`, `dedup`, `forge`) rely on
  atomic-write idempotency rather than per-step WAL inverses; the only
  `bundle` kind that would need a multi-step inverse is still
  `KindNotYetImplemented`.

Per-kind inverses (filesystem snapshot restore, DB row delete) for a
`bundle` handler are not implemented today; they belong with that
handler. The driver shape is shared between both ops sources so
neither side has to roll its own recovery state machine.

## Tests

| File | Coverage |
|---|---|
| [`mcp::schemas::tests`](../../crates/mwe-mcp-server/src/mcp/schemas.rs) | roster count + unique names + object schemas; the dropped proposal write tools stay absent; `wiki_admin_push` advertises the optional `mark_processed` array |
| [`mcp::auth::tests`](../../crates/mwe-mcp-server/src/mcp/auth.rs) | missing bearer, bogus token, identity round-trip, class/admin claim mapping |
| [`crates/mwe-mcp-server/tests/dispatcher.rs`](../../crates/mwe-mcp-server/tests/dispatcher.rs) | per-tool dispatch + error class + admin gating + consumer scoping + sender mismatch + the H/J/K class gates |

These cover the dispatcher proper. The per-tool
business logic (capture / recall / events queue / lint / etc.) keeps
its existing coverage in `mwe-core`'s `tests` modules — the
dispatcher tests prove the wiring + error mapping, not the
underlying primitives.

## Current limitations

The dispatcher does not yet support the following (planned — see the
[roadmap](../roadmap.md)):

| Not yet supported | Blocker |
|---|---|
| Per-kind WAL inverses (`file_write`, `db_update`, `marker_propagation`) | needs the dashboard-only `proposals::apply_proposal` `bundle` handler |
| 5 remaining `wiki_lint` checks (`broken_crosslinks`, `acl_inconsistent`, `embed_missing`, `hub_outdated`, `superseded_chain`) | need recall pipeline + hub regeneration cycle |
| `wiki_ingest_external` source types `file` / `git` / `url` | external IO + import policy decisions |
| `cost_estimate` in audit rows | needs LLM token accounting |
| Rate limiting per `rate_limit_id` (claim parsed, not enforced) | needs tower-governor wiring |
| `OpenAI` LLM backend | the `llm.rs` config carries Anthropic + Google (Gemini) HTTP backends alongside the Ollama default; `OpenAI` is not wired |
