---
title: Tool reference — per-tool I/O contract
area: protocol
status: implemented
last_review: "2026-07-19"
---

# Tool reference

This page is the **canonical per-tool I/O contract** for the public MCP
surface that mwe-mcp exposes to consumer agents. It complements
[`mcp-tools.md`](mcp-tools.md), which is the roster + status overview;
this page documents the wire shapes — inputs, outputs, error enums, and
the partials — for every registered tool, plus the in-process surfaces
(`mwe-core` internal APIs and the dashboard agentic chat) that are *not*
reachable over MCP.

> **Source of truth.** The set of tools and their JSON Schemas is
> defined in
> [`crates/mwe-mcp-server/src/mcp/schemas.rs`](../../crates/mwe-mcp-server/src/mcp/schemas.rs)
> (`schemas::all_tools()`). Dispatch is wired in
> [`crates/mwe-mcp-server/src/mcp/mod.rs`](../../crates/mwe-mcp-server/src/mcp/mod.rs)
> (`dispatch`); per-tool handlers live in
> [`crates/mwe-mcp-server/src/mcp/tools.rs`](../../crates/mwe-mcp-server/src/mcp/tools.rs);
> the canonical error classes are in
> [`crates/mwe-mcp-server/src/mcp/error.rs`](../../crates/mwe-mcp-server/src/mcp/error.rs).
> When this page and the code disagree, **the code wins** — fix the
> page. Do not hardcode a tool count here; read it off `all_tools()`.

The tool surface is organised into families **A through L**. The
`initialize` handshake advertises this in its `instructions` field
("mwe-mcp tool surface organised into families A–L"). The family
ordering and membership match the `all_tools()` vector exactly:

| Family | Theme | Tools |
|---|---|---|
| **A** | Conversation | `wiki_ingest_message` |
| **B** | Events | `events_poll`, `events_ack` |
| **D** | Read for consumer UI | `wiki_read`, `wiki_search`, `wiki_navigate` |
| **E** | Audit / health | `tool_log_search`, `wiki_lint` |
| **F** | Setup | `consumer_register`, `wiki_ingest_external` |
| **G** | Dashboard | `dashboard_link` |
| **H** | Smart-wiki admin (smart) | `wiki_admin_push`, `wiki_admin_pull`, `wiki_admin_notify`, `wiki_admin_lease_acquire`, `wiki_admin_lease_release` |
| **I** | Skill catalog | `skill_list`, `skill_fetch` |
| **K** | Smart bootstrap & contextual recall (smart) | `smart_bootstrap`, `recall_core_global` |
| **L** | Forget (authority-routed) | `wiki_forget`, `wiki_forget_bulk` |

---

## 0. Global conventions

### Family prefixes and naming

Tool names carry a family prefix so a registry browser or an agentic
loop can group them at a glance:

- `wiki_*` — memory read / write / navigation / lint / ingest
  (`wiki_ingest_message`, `wiki_read`, `wiki_search`, `wiki_lint`,
  `wiki_ingest_external`).
- `wiki_admin_*` — smart-wiki authoritative writes and the
  cooperative lease (family H).
- `events_*` — the async event queue (family B).
- `tool_log_*` — the audit trail (family E).
- `structure_proposal_*` — the structural-proposal flow (family C; only
  the read-only `_list` is on MCP).
- `skill_*` — the server-served skill catalog (family I).
- `smart_*` / `recall_core_global` — the atomic primitives for the
  smart-consumer hook bundle (family K).
- `consumer_register`, `dashboard_link` — setup and dashboard hand-off.

The prefix is a readability convention, not a separate MCP namespace.

### Shape philosophy: code-execution friendly

Every tool is designed to be **code-execution friendly**: typed,
structured inputs and outputs, deterministic shapes, no free-form
prose used as a data channel. A consumer can model the client side as
serialised types (TypeScript interfaces, Pydantic dataclasses, Rust
structs) without parsing LLM-generated text. The philosophy extends to
errors: they are emitted as machine-readable enum classes
(`invalid_input`, `requires_consumer_class_smart`, …), never as natural
language.

### Authentication model

mwe-mcp is **HTTP-only**. Every `/mcp` call carries a signed
**JWT** in the `Authorization: Bearer …` header. The
`jwt_auth_middleware` verifies it, checks the revocation blacklist, and
attaches an `IdentityProfile` to the request extensions; the dispatcher
reads that profile (see
`jwt-and-session-model.md`
and `mcp-dispatcher.md`).

The verified `TokenClaims` carry:

| Claim | Wire name | Required | Meaning |
|---|---|---|---|
| `sender_id` | `sender_id` | yes | The user the call acts on behalf of. ACL is evaluated against this principal. |
| `device_label` | `device_label` | yes | Free-form device tag, written to `tool_executions.device_label` for audit. |
| `rate_limit_id` | `rate_limit_id` | yes | Bucket key for quotas. **Parsed but not enforced today** (see below). |
| `jti` | `jti` | yes | JWT id, used by the revocation blacklist. |
| `iat` / `exp` | `iat` / `exp` | yes | Standard issued-at / expiry timestamps (Unix seconds). |
| `is_admin` | `isAdmin` | no (default `false`) | UI gating only — it does **not** bypass ACL. Widens `tool_log_search`, unlocks admin `dashboard_link` intents, and allows draining any consumer's events. |
| `consumer_id` | `consumer_id` | no | Set on bot / orchestrator tokens. Required for `events_poll` / `events_ack`. |
| `consumer_class` | `consumer_class` | no (default `standard`) | `smart` unlocks the smart-wiki families (H smart subset, K). A token with no claim deserialises as `standard`. |

There is no separate `owner_user` claim: the **owner_user is derived
from `sender_id`** (the smart-consumer write families treat
`identity.sender_id` as the owning user of a smart-wiki).

Tokens are minted by `mwe-mcp token-issue …` (add `--class smart` for a
smart consumer) or via the `/dashboard/tokens/issue` form. A tool that
accepts an explicit `sender_id` argument validates it against the
token: a mismatch returns **`403 sender_token_mismatch`**
(`forbid_sender_mismatch`). This applies to `wiki_ingest_message`, `wiki_read`, `wiki_search`, and
`dashboard_link`.

**The `guest` effective sender.** A standard consumer delegated for the
builtin `guest` pseudo-identity ([identity-and-acl.md §1](../concepts/identity-and-acl.md))
may set `X-MWE-Act-As: guest`; guest can never be the token holder
(`token-issue` and the dashboard form refuse it). On guest calls the
read tools behave normally (the ACL yields the public slice),
`wiki_ingest_message` short-circuits to its ephemeral response, and
`wiki_ingest_external`, `wiki_admin_notify`, `consumer_register`,
`tool_log_search` and `dashboard_link` return
**`403 sender_unauthorized`** (`forbid_guest`); `POST /media` returns
`403 guest_cannot_upload`.

### Common error classes

Every per-tool handler returns one of a small, audited set of error
classes (`ToolErrorClass` in `error.rs`). The dispatcher folds the
class string into both `tool_executions.error` and the JSON-RPC error
response: the wire class rides as `data.error_class` so a consumer can
branch on the same string the audit log records, regardless of the
JSON-RPC numeric code.

| HTTP-ish code | Class string | Meaning |
|---|---|---|
| `400` | `invalid_input` | Malformed or missing parameter. |
| `401` | `invalid_token` | Token unsigned, expired, or revoked (raised at the middleware before dispatch). |
| `403` | `sender_unauthorized` | Sender not authorised for this op. |
| `403` | `sender_token_mismatch` | `sender_id` argument ≠ token sender. |
| `403` | `consumer_not_registered` | Bot / orchestrator not in `consumers`. |
| `404` | `not_found` | Entity missing. |
| `500` | `internal_error` | Unexpected bug — generic message. |
| `503` | `service_unavailable` | Backing infra down, or the feature is not yet implemented. |
| `501` | `not_implemented_phase_c` | Tool exists, full implementation is not yet wired (e.g. `wiki_ingest_external` non-inline sources). |

The smart-wiki admin tools add their own classes
(`requires_consumer_class_smart`, `wiki_owned_by_other_user`,
`wiki_type_not_admin_writable`,
`smart_does_not_notify_own_wiki`,
`standard_uses_ingest_for_memory`,
`consumer_class_wiki_family_mismatch`, `conflicting_op_log_head`,
`rate_limited`, `wiki_locked_by_lease`,
`unknown_briefing_item_id`, `too_many_briefing_items`,
`wiki_type_requires_parent`). They are documented in
the per-tool sections below. The full enum is the source of truth in
`error.rs`.

### Common types

- `iso8601` — ISO 8601 date/time string (e.g. `2026-05-17T14:32:00Z`).
- `principal` — `user:<id> | group:<id> | global`.
- `wiki_id` — opaque canonical id of a wiki. **Never a filesystem path.**
- `fact_id` — UUIDv7, lowercase with dashes
  (`018f1234-5678-7abc-9def-0123456789ab`).
- `proposal_id` — `p-YYYY-MM-DD-NNN`.
- `event_id` — note that `events_*` ids are **integers** on the wire
  (`events_ack.event_ids` is `array<integer>`); the historical
  `event_id` UUID string is the legacy shape.

### Pagination

For list-returning tools: `top_k` is optional, default **20**, max
**50** (audit search is the exception — default 50, max 500). There is
**no cursor** in the MVP. Clients needing deep pagination narrow with
filters (date range, scope, status).

### No filesystem-path exposure

No MCP tool ever returns a filesystem path
(`wikis/zoe/giardinaggio/index.md`) — only opaque `wiki_id`s. The rule
extends to `wiki_ingest_message`, which never reveals the destination
`wiki_id` of a capture in its output (the consumer sees only the
`suggested_seed` and, when relevant, a dashboard hint).

### Rate limiting is parsed-but-not-enforced today

`rate_limit_id` is a required JWT claim and is threaded through the
audit pipeline, **but no rate limiter is wired**. The MCP audit helper
hardcodes `rate_limit_id: None` and `cost_estimate: None` when writing
`tool_executions` (see `record_audit` in `mod.rs`), and no tool checks
a quota before executing. The one exception is `wiki_admin_notify`,
which enforces a **per-wiki notify cap of 50/hour** inside
`mwe-core::briefing` (surfaced as `429 rate_limited`) — that is a
hand-rolled cap, not the token-bucket limiter the `rate_limit_id`
claim anticipates. Treat global rate limiting as a forward-compatible
hook, not a working limiter.

Similarly, `cost_estimate` / budget caps are part of the audit schema
but no budget enforcement runs: the field is recorded where available,
never used to refuse a call.

---

## A. Conversation

### `wiki_ingest_message`

The flagship tool and the **single conversational channel**. The
consumer passes a raw user message; mwe-mcp runs its internal `ingest`
LLM to do all the pre-processing (intent classification → recall →
routing → capture / supersede / skip) in one round-trip and returns
only what the consumer needs to compose a natural reply. Always on,
never optional. See
`ingest-pipeline.md` for the
internal pipeline.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `text` | string | yes | Raw user message body. Empty / whitespace → `400 invalid_input`. |
| `sender_id` | string | no | Optional override of the token's `sender_id`; must match or `403 sender_token_mismatch`. |
| `author` | enum `user` \| `assistant` | no (default `user`) | Who wrote `text`. `assistant` feeds the agent's OWN prior reply back for extraction (agent-authored memory, roadmap 27): the classifier then applies the agent-turn discriminator (keep only the durable sediment it synthesised — an episode/decision, advice tied to the user — and skip filler/regenerable knowledge/recall echoes), and any captured fact is attributed `sender = <the calling agent>` (resolved from the consumer↔system-user binding) while its `owner` stays the acting user. An unknown value → `400 invalid_input`. |
| `recent_messages` | `array<{ role: "user"\|"assistant", text, timestamp? }>` | no | Recent turns for coreference resolution. |
| `context_hint` | enum `conversation` \| `dashboard_command` \| `import` | no (default `conversation`) | Hints the intent classifier. An unknown value → `400 invalid_input`. |
| `metadata` | object | no | Free-form. The dispatcher honours five keys: `disambig_choice` (string, the second-turn commit), `locale` (BCP-47 tag — an explicit LANGUAGE directive that overrides the per-user `enrollment_users.locale` default), `occurred_at` (ISO-8601 instant the message was originally uttered — the turn's semantic clock for backlog replays/imports: relative dates, validity windows and the due-soon horizon resolve against it instead of the server clock, while operational timestamps stay wall-clock; malformed → `400 invalid_input`), `authored_refs` (array of plain `[[wiki_id/page]]` wikilinks — a smart consumer echoes the `authored_refs` its preceding `wiki_admin_push` returned so personal memory links to the project page instead of duplicating its body; blank/non-string entries ignored — group 17), and `channel` (opaque surface label, e.g. `telegram:42` — multi-channel consumers tag their surfaces apart so the cross-consumer recent window, group 43, excludes only the requesting surface from `recent_window`; blank normalises to unset). |
| `attachments` | `array<{ catalog_id, kind?, caption?, description? }>` | no | Media riding this turn (media pipeline). Bytes travel out of band: upload via `POST /media` first, pass the minted `catalog_id` here. Every id must parse (`400 invalid_input`), exist (`400 invalid_input`) and be readable by the effective sender (`403 sender_unauthorized`); the catalog row's `kind` is authoritative. Undescribed photos ride the classifier call as images; a `description` is trusted as the consumer's own vision. The captured fact's body carries the code-rendered `{{embed=…}}` marker; unclaimed attachments are filed by a deterministic fallback so catalogued media on an accepted turn is never dead — but `text` must still be non-empty (send the caption or a `[media]` placeholder for a captionless photo). |
| `promote` | enum `always` \| `never` | no | **Verbatim source promotion** override (the paste-into-chat backstop): absent = an oversized document-shaped user turn (non-guest, not `dashboard_command`, `author=user`) is auto-promoted — archived verbatim as a `doc` blob + catalog row, enqueued as a document job, and ingested as a bounded excerpt plus the attachment link. `always` forces it, `never` forbids it. An unknown value → `400 invalid_input`. |

**Output**

```jsonc
{
  "intent_classified": "capture",      // capture | recall | structural | skip (audit/debug)
  "context_snippet": "…",              // recalled MEMORY pre-formatted for the agent's system prompt (null when empty)
  "rules": "…",                        // standing BEHAVIOUR directives to APPLY this turn — kept apart from memory (null when none)
  "suggested_seed": "…",               // a natural-reply proposal the agent may refine
  "recent_window": "…",                // the user's live thread from their other surfaces (group 43) — self-labelled, inject verbatim (null when nothing to serve). A turn sent WITHOUT recent_messages is treated as a blank-context session and served its OWN surface too (fresh-session resume, 43j) — minus the current message
  "capture_id": "018f…",               // fact_id of the new block, if a capture happened (audit-only)
  "needs_disambig": false,             // true when ambiguous candidates need a user choice
  "disambig_candidates": [             // present (possibly empty array) alongside needs_disambig
    { "candidate_id": "…", "description": "…" }
  ],
  "llm_used": true,                    // true when the LLM round-tripped a parseable plan (audit)
  "took_ms": 312,
  "pending_attention": {               // present ONLY when pending + applied_pending_confirm > 0 (silent otherwise)
    "pending_count": 2,                // structure_proposals.status = 'pending'
    "applied_pending_confirm_count": 1,
    "dashboard_path": "/dashboard/proposals",
    "note": "scoped_to_recipient"
  },
  "document_promoted": {               // present ONLY when the paste-into-chat backstop fired
    "catalog_id": "c-2026-07-19-doc-001.txt",
    "job_id": "0198…",                 // the enqueued document job
    "existing": false                  // true when job dedup absorbed a retry
  }
}
```

**Errors**: `400 invalid_input` (empty text, bad `context_hint` or
`recent_messages.role`), `403 sender_token_mismatch`,
`503 service_unavailable` (the `ingest` LLM slot is unconfigured or the
backend cannot be built). A transport-level LLM failure *during* the
call is absorbed inside `mwe-core::ingest` and surfaces as a
degraded-but-successful response (e.g. `intent_classified: "skip"`),
never as an `Err`.

**Caveats / partials**

- `capture_id` is audit-only. Do not use it to build chat-level
  cross-links.
- `rules` is **behaviour-only and structurally separate from
  `context_snippet`** (ingest pipeline):
  it carries the standing behaviour directives in force for the served user —
  agent-wide, then the user's **user-global** rules (the ones they set for
  every assistant, recalled from their own identity-wiki `rules.md`), then
  their per-user rules for this agent (the agent's own `rules.md`) — plus,
  leading, any one-shot governance notice (e.g. an agent-wide change refused
  for a non-admin this turn). The consumer
  **applies** these as instructions, never relays them. Privacy/sharing
  never rides this field — it is enforced memory-side by the per-fragment
  ACL, so the agent cannot leak what recall never surfaces. `null` when the
  turn surfaced no directive.
- `pending_attention` is **scoped to the caller (0032)**: a non-admin
  sees the count of proposals addressed to them (`recipient_id =
  "user:<id>"`) plus the unaddressed / admin-fallback ones; an admin
  sees the deployment-wide count. The block is structured (not prose)
  so consumers in any locale compose their own warning.
- **Smart-family routing exclusion:** smart wikis are
  filtered out of the ingest LLM's `available_wikis` window — they are
  authoritatively managed by smart consumers via `wiki_admin_*`, and
  routing a capture into them through ingest would double-bill the
  consumer's LLM budget. Because a smart wiki never appears in the
  window, the classifier cannot legitimately target one; if it
  hallucinates an off-window `target_wiki_id` anyway, the capture-plan
  validator (`validate_capture_plan` in `mwe-core::ingest`) rejects it
  with `TargetWikiNotAvailable` and the turn is demoted to a skip with a
  warn log rather than written. (The standard-vs-smart **write** gate
  proper lives on the admin path: `wiki_admin_push`/`_pull` against a
  non-smart wiki is refused `400 wiki_type_not_admin_writable`.)

---

## B. Events

The async event queue. A registered consumer (orchestrator, bot,
plugin) polls for events mwe-mcp emits (structure proposals, archive
proposals, auto-apply notifications, …) and acknowledges delivery.

### `events_poll` *(read-only)*

Drain pending events for a registered consumer.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `consumer_id` | string | yes | Must match the token's `consumer_id` (or the caller must be admin). |
| `since` | iso8601 | no | Only events newer than this are returned. |
| `kinds` | `array<string>` | no | Whitelist of event kinds. |
| `top_k` | integer 1–50 | no (default 20) | Page cap. |

**Output**

```jsonc
{
  "events": [
    {
      "event_id": 4711,                // integer on the wire
      "kind": "structure_proposal",    // one of the closed kind set (below)
      "wiki_id": "…",
      "fact_id": "…",
      "payload": { /* kind-specific */ },
      "emitted_at": "2026-05-26T09:55:00Z"
    }
  ],
  "has_more": true
}
```

**`kind` is an extensible string.** The kinds currently emitted are
`dedup_proposed`, `structure_applied`, `archive_proposed`,
`auto_applied` (REM), `document_ingested` (a document-ingest job
finished — payload: `job_id`, resolved `disposition` and `title`, the
anchor `document_page` when one exists, `facts_buffered`, `source_ref`,
`recipient_id`), `compile_failure_streak` (the narrative compiler
failed/degraded the same page in consecutive compile passes — payload:
`slug`, `source_path`, `consecutive`, `last_error`, `dashboard_path`;
see the failure ledger), and `fact_minted_for_you` (ingest filed one or
more facts owned by an enrolled human who was not the human of that
turn / the uploader — payload: `recipient_id` (`user:`-prefixed
beneficiary), `from_user_id` (bare id of the human whose turn or upload
minted them), `origin` (`user_turn` | `assistant_turn` | `document`,
plus `job_id`/`title` on the document path), `facts` (array of
`fact_id`/`wiki_id`/`body` — the content rides the notice so the
consumer's agent can deliver it without a recall round-trip), and
`dashboard_path`; batched per (beneficiary, turn), so one turn emits at
most one event per recipient; group-owned facts and agent principals
never emit it).
The `kind` column is `TEXT`, so new kinds are additive —
a consumer that does not recognise a kind just receives the JSON payload
and decides what to do. `structure_applied` is the **notice** for a
structural change REM applied directly (paragraph→page split or
page→sub-wiki emergence): its payload carries the receipt
`proposal_id`, the `variant`, source → target, the `revert_deadline`,
the undo `dashboard_path`, and the `recipient_id` of the affected user.

**Errors**: `403 consumer_not_registered`, `403 sender_unauthorized`
(the `consumer_id` argument does not match the token, or a non-admin
token with no `consumer_id` tried to poll).

**Caveats**

- A token with no `consumer_id` is treated as a human user; only an
  admin token may then drain another consumer's queue (an
  admin-debugging surface).
- **Polling cadence:** consumers poll on a ~30 s default loop (or
  long-poll where the transport supports it). After dispatching an
  event to its destination, the consumer **must** call `events_ack` to
  stop re-delivery.
- **Recipient routing (0032):** the `structure_applied`,
  `dedup_proposed`, `auto_applied`, and `fact_minted_for_you` payloads
  carry a `recipient_id` field — the addressee as a `Principal` wire
  string (`"user:<id>"`), or `null` when the change is unaddressed
  (admin-fallback; `fact_minted_for_you` is always addressed). A multi-human consumer (one bot serving several
  users via act-as) reads `recipient_id`, strips the `user:` prefix, and
  routes the notification (e.g. a Telegram message carrying a
  `dashboard_link`) to that specific human; on `null` it falls back to
  the operator/admin. For `structure_applied` this is the heart of the
  apply-and-notice contract: the notice **names the affected user** so
  the agent knows whom to forward it to.
- **Retention & multi-consumer GC:** events are retained 30 days
  after *every* registered consumer has acked. The ack state is a
  **per-consumer JSON map** in the `wiki_events.acks` column (read via
  `json_extract` on the polling consumer's id), not a single global
  flag — so GC stays correct when several consumers drain the same
  queue independently.

### `events_ack`

Acknowledge delivery of the listed events so they stop reappearing in
`events_poll` for this consumer.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `consumer_id` | string | yes | Same match rule as `events_poll`. |
| `event_ids` | `array<integer>` (min 1) | yes | Event ids to ack. Empty → `400 invalid_input`. |

**Output**

```jsonc
{ "acked": 3, "unknown": [/* event_ids that were already acked / expired */] }
```

**Errors**: `403 consumer_not_registered`, `403 sender_unauthorized`,
`400 invalid_input`.

---

## Structural changes: apply + notice (no proposal family)

There is **no proposal family on the MCP surface**. A structural change
(the REM paragraph→page split, the page→sub-wiki emergence) is **not** a
blocking proposal the user must approve: REM applies it directly,
records a **born-applied receipt** (a `structure_proposals` row in
status `applied` with a `revert_token` + 7-day `revert_deadline`), and
the consumer learns about it from the **`structure_applied` notice** on
`events_poll` (family B). The notice names the affected user
(`recipient_id`) and carries the undo `dashboard_path` — the agent
forwards it and points that user to the dashboard to undo or declass.
The dashboard is the *undo* surface, not an approval surface.

The dashboard write path (`mwe-core::proposals`, **not** MCP) drives the
remaining lifecycle with `apply_proposal` (manual `pending → applied`),
`confirm_proposal` (`applied_pending_confirm → applied`),
`revert_proposal`, and three sweeps (`auto_apply_overdue_proposals`,
`auto_finalize_unconfirmed_proposals`, `expire_overdue_proposals`) —
only the questionnaire kinds (`dedup_merge` today) still enter
`pending`. Two facts off that path are load-bearing for anyone
reasoning about what a structural change can do:

- **Revert window is 7 days.** A born-applied receipt (and a manual
  `apply` / `confirm`) mints a `revert_token` and a
  `revert_deadline = applied_at + 7d`. After the window the
  token-authorised revert path closes.
- **Two revert authorities.** `RevertAuth::Token(t)` reverts an
  already-`applied` row (verifies the token + the 7-day deadline);
  `RevertAuth::Caller(sender_id)` reverts an `applied_pending_confirm`
  row without a token (the user is annulling an auto-apply before
  confirmation).
- **`bundle` kind is not implemented.** Of the three canonical kinds
  (`proposals::kind::ALL` = `wiki_promote`, `dedup_merge`, `bundle`),
  the first two ship `apply` + `revert`;
  `bundle` (a multi-op coordinator) returns
  `ApplyError::KindNotYetImplemented` and the dashboard surfaces it as a
  flash error. A `bundle` proposal is never emitted today — the kind is
  reserved for a consumer that needs forge+promote in one transaction.

See `proposal-apply-engine.md`
for the full per-kind coverage and state transitions.

---

## D. Read for consumer UI

For consumers that render raw memory content (a custom viewer, an
advanced dashboard). Not the typical conversational path — that goes
through `wiki_ingest_message`.

### `wiki_read` *(read-only)*

Read a specific page of a wiki (default `index.md`), with ACL redaction applied
for the sender. See `redaction-policy.md`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `wiki_id` | string | yes | Opaque id. |
| `sender_id` | string | no | Validated against the token. |
| `path` | string | no (default `index.md`) | Page path relative to the wiki dir (e.g. `recipes/pasta.md`). `is_safe_page_path`-validated (bad → `400 invalid_input`); unknown page → `404 not_found`. The body and the per-fact ACL map resolve to the *same* page. |
| `include_archived` | boolean | no (default `false`) | **Accepted but not honoured** — the archive surface is not yet implemented. |
| `format` | enum `markdown` \| `json_blocks` | no (default `markdown`) | **Accepted but not honoured** — the floor always returns continuous-text markdown. |

**Output**

```jsonc
{
  "wiki_id": "alice-tecnica",
  "page": "index.md",                  // the page actually served
  "title": "…",
  "wiki_type": "wiki-tech",
  "owner": "user:alice",               // resolved scope principal (or "inherit")
  "content_rendered_for_sender": "…",  // page BODY (testata stripped) with redacted regions collapsed to a callout
  "redacted_count": 0,                 // number of regions hidden for this sender
  "children": [ { "wiki_id": "…", "slug": "…", "wiki_type": "…" } ],
  "parent_wiki_id": "…"
}
```

**Errors**: `400 invalid_input` (bad `wiki_id`), `404 not_found`,
`403 sender_token_mismatch`.

**Caveats / partials**

- Redaction *is* applied via `render_for_sender`: hidden regions
  collapse and `redacted_count` reports how many. When every region is
  private the body is replaced by a single canonical callout. The output
  carries **no** `fully_redacted` boolean — a caller distinguishes
  "entirely private" by inspecting `content_rendered_for_sender`
  itself.
- The page **frontmatter (testata) is stripped** before rendering, so
  `content_rendered_for_sender` is body-only. The testata is card metadata
  derived from the page's facts (`description` / `keywords.topics`); it carries
  no ACL markers, so `render_for_sender` would otherwise pass it through verbatim
  and leak the themes of facts the sender cannot read. The structured fields the
  consumer needs (`title`, `wiki_type`, `owner`) are returned separately above.
  (The navigator already strips the testata in `recall_nav::open_projected`.)

### `wiki_search` *(read-only)*

Semantic search over the corpus accessible to the sender. Top-K cosine
+ ACL post-filter. See
`recall-pipeline.md`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `query` | string | yes | — |
| `sender_id` | string | no | Validated against the token. |
| `top_k` | integer 1–50 | no (default 20) | — |
| `scope.owner_ids` | `array<principal>` | no | Honoured. Only the first entry is applied as the owner filter. |
| `scope.wiki_types` | `array<string>` | no | Post-filter: keeps hits whose resolved `wiki_type` is in the set. |
| `scope.smart` | boolean | no | Corpus selector, applied **before** ranking: `true` searches only `wiki_sections` (smart-wiki documentation), `false` only `fact_index` (standard-wiki memory), omitted searches both and merges. Not a post-filter — `top_k` is honoured either way. |
| `scope.valid_at` | string (ISO-8601) | no | The dated query: keeps only facts whose validity window contains the instant ("what was true on June 4th?"). Without it a closed window only down-ranks a hit (signal, never filter). Malformed values are `invalid_input`. |
| `scope.include_archived` | boolean | no (default `false`) | **Accepted but not honoured.** |

**Output**

```jsonc
{
  "results": [
    { "wiki_id": "…", "kind": "fact", "fact_id": "018f…", "snippet": "…", "score": 0.83 },
    { "wiki_id": "…", "kind": "section", "section": "wikis/alice/proj/design.md#3",
      "source_path": "wikis/alice/proj/design.md", "heading_path": "Design > Auth",
      "snippet": "…", "score": 0.79 }
  ],
  "total": 5,                          // count of hits after filtering (= results.len())
  "scope_hint": null                   // legacy wire-compat field, always null
}
```

A `fact` result is keyed by its `fact_id`; a `section` result is keyed by
its positional `section` handle (`<source_path>#<ord>`), which is stable
across reindexes.

**Errors**: `400 invalid_input` (bad `scope.owner_ids[0]`),
`403 sender_token_mismatch`, `500 internal_error`.

**Caveats**: a **fact** hit is gated by the per-fragment ACL (`can_read`), so a
fact the sender cannot read never appears — and a wiki the sender can read
nothing in simply contributes no hits (visibility is derived from the facts,
not a wiki-level flag; see
[`identity-and-acl.md` §5](../concepts/identity-and-acl.md#5-wiki-visibility-is-derived--there-is-no-wiki-level-access-gate)).
A **section** hit is gated at the wiki level instead — its wiki's owner +
`shared_with`, resolved once per wiki — because that is where a smart
wiki's ACL lives.
The `wiki_types` filter resolves each hit's
`wiki_type` through a cached tree walk — hits whose parent wiki was
deleted between recall and filter resolve to `None` and drop out of any
type filter.

---

### `wiki_navigate` *(read-only)*

**Deep** recall — the funnel navigator (`mwe_core::recall_nav`) as a tool: whole
visible corpus, ACL-filtered, one LLM hop at a time. The **deep counterpart of
`wiki_search`**; returns the navigated `(wiki, page)` path **and** the flat hits
(a superset). Reach for it on a question that needs depth or to connect things
across pages; use `wiki_search` for a quick one-line lookup. See
`recall-pipeline.md`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `query` | string | yes | What to recall, natural language. |
| `sender_id` | string | no | Validated against the token. |
| `top_k` | integer 1–50 | no (default 20) | Cap on the flat hits (and the RAG seeds feeding the funnel). |
| `topics` | `array<string>` | no | Seed family **C** — subjects to look up. Supplying `topics` or `owners` skips server-side extraction (B). |
| `owners` | `array<principal>` | no | Seed family **C** — `user:<id>`/`group:<id>` the query is about. Unparseable entries dropped. |

**Output**

```jsonc
{
  "navigated": [
    { "wiki_id": "…", "page": "recipes/pasta.md", "text": "…" }  // sender-projected prose, in opening order
  ],
  "hops": 2,                           // navigator LLM calls spent
  "truncated": false,                  // char budget cut material short
  "flat": [
    { "wiki_id": "…", "fact_id": "018f…", "snippet": "…", "score": 0.83 }
  ],
  "navigator_available": true          // false → no navigator slot wired, flat-only (navigated == [])
}
```

**Errors**: `403 sender_token_mismatch`, `500 internal_error`.

**Caveats**: smart wikis are not funnel-navigated (their content surfaces only in
`flat`). Without a `navigator` LLM slot the tool degrades to flat-only
(`navigator_available: false`), never an error. Seed derivation: caller
`topics`/`owners` (C) → query extraction on the navigator slot (B) → principal +
RAG only (A).

---

## E. Audit / health

### `tool_log_search` *(read-only)*

Query the audit trail of past tool calls. Backed by
`mwe_core::audit::search`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `sender_id_filter` | string | no | Non-admins are forced to their own `sender_id`; passing another → `403 sender_unauthorized`. |
| `tool_name_filter` | string | no | — |
| `date_range` | `{ from?: iso8601, to?: iso8601 }` | no | — |
| `result_status` | enum `success` \| `error` | no | Unknown → `400 invalid_input`. |
| `top_k` | integer 1–500 | no (default 50) | — |

**Output**

```jsonc
{
  "entries": [
    {
      "timestamp": "…",
      "tool_name": "wiki_ingest_message",
      "sender_id": "alice",
      "device_label": "claude-code-laptop",
      "rate_limit_id": "default",
      "args_hash": "sha256…",          // hash of the args, never the raw args
      "result_status": "success",      // derived: "error" iff error_code present
      "latency_ms": 312,
      "cost_estimate": null,           // recorded where available; budget caps not enforced
      "error_code": null               // the wire error_class on a failed call
    }
  ],
  "total": 17                          // here total = entries.len()
}
```

**Caveats**: admins (`isAdmin: true`) see every row; everyone else is
scoped to their own. `cost_estimate` is part of the row but no budget
enforcement runs.

### `wiki_lint` *(read-only)*

Run consistency checks over the corpus (read-only, no auto-fix).

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `scope.wiki_ids` | `array<wiki_id>` | no | Restrict the scan. |
| `checks` | `array<string>` | no (default = all) | Subset of the eight check names; an unknown name → `400 invalid_input`. |

The eight advertised check names are `broken_crosslinks`,
`marker_malformed`, `orphan_facts`, `meta_invalid`, `acl_inconsistent`,
`embed_missing`, `hub_outdated`, `superseded_chain`.

**Output**

```jsonc
{
  "issues": [
    { "severity": "warning", "check": "orphan_facts", "wiki_id": "…", "fact_id": "…", "message": "…", "suggested_fix": "…" }
  ],
  "summary": {
    "total": 1,
    "by_severity": { "info": 0, "warning": 1, "error": 0 },
    "by_check": { "orphan_facts": 1 }
  }
}
```

`severity` is the **closed set** `info | warning | error`
(`mwe-core::lint::Severity`). Issues are sorted by `(severity desc,
check asc, wiki_id asc, fact_id asc)`, and `summary.by_severity` always
carries all three keys (pre-seeded to `0`) so a consumer can index it
without a presence check; `by_check` is pre-seeded with the requested
(active) check names.

**Caveat — four of eight checks are live.** Only `marker_malformed`,
`orphan_facts`, `meta_invalid` and `embed_missing` (every `{{embed=…}}`
must resolve to a `media_catalog` row whose blob exists — see
media pipeline) actually run in
`mwe-core::lint`. The other four (`broken_crosslinks`,
`acl_inconsistent`, `hub_outdated`, `superseded_chain`) are accepted by
the schema and return zero issues without error — even when requested
explicitly. A green result means "the four shipped checks found
nothing," not "the corpus is provably clean."

---

## F. Setup

### `consumer_register`

Idempotent registration of a consumer (orchestrator, bot, plugin).
Required before the first `events_poll`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `consumer_id` | string | yes | Stable id chosen by the consumer. |
| `display_name` | string | no | — |
| `callback_url` | string (uri) | no | For future webhook delivery. |
| `kinds_subscribed` | `array<string>` | no | Default: all kinds. |
| `metadata` | object | no | — |

**Output**

```jsonc
{
  "registered": true,
  "fresh_registration": true,          // false on a refresh of an existing row
  "consumer_secret": "…",              // 32-byte hex — present ONLY on a fresh registration
  "registered_at": "2026-05-26T10:00:00Z"
}
```

**Caveats**: on a fresh registration persist `consumer_secret` (it is
omitted on a refresh, where only mutable fields are updated). The
`consumer_secret` would sign webhook deliveries once that path ships.

### `wiki_ingest_external`

Ingest a **document** — long-form content that is not a conversational
turn: a catalogued `doc` attachment, an inline text, a recorder
transcript. Async by design: the call returns a job receipt; a worker
classifies the document onto the **disposition dial**
(`consult` / `dossier` / `dissolve` — document-as-unit by default),
extracts, and notifies completion via `events_poll`
(`document_ingested`). Design narrative:
document ingest.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `source.type` | enum `media` \| `inline` \| `file` \| `git` \| `url` | yes | `file` / `git` / `url` return `501`. |
| `source.catalog_id` | string | for `media` | An already-uploaded catalog id (`POST /media`); the caller must be in its read set. |
| `source.content` | string | for `inline` | The document text. |
| `text` | string | no | **Trusted seam**: consumer-supplied extraction of the source bytes (required for non-textual media, e.g. PDF — the server reads UTF-8 `text/*`/markdown blobs only). |
| `disposition` | enum `consult` \| `dossier` \| `dissolve` | no | Forces the dial; absent = the classifier proposes. |
| `format` | enum `prose` \| `dialogue` | no | Forces the segmentation shape; `dialogue` threads per-utterance timestamps into per-fact validity. |
| `title` | string | no | Title hint (e.g. the original filename). |
| `occurred_at` | iso8601 | no | The document's semantic clock (relative dates resolve against it). Defaults to the catalog row's timestamp for `media`. |
| `promote` | enum `always` \| `never` | no | Inline only: forces (or forbids) **verbatim source promotion**; absent = document-shaped inline text is auto-promoted to the media rail (blob + catalog row, kind `doc`) so facts cite the preserved original. |
| `dry_run` | boolean | no (default `false`) | Classify + segment synchronously, write nothing (reports `would_promote`). |
| `force` | boolean | no (default `false`) | Bypass the (document, owner) idempotency check. |

**Output (enqueue)**

```jsonc
{
  "job_id": "0197…",
  "status": "queued",        // "existing" on an idempotency hit
  "existing": false,
  "size_chars": 48211,
  "note": "queued — the worker classifies (consult/dossier/dissolve), extracts, and notifies via events_poll (document_ingested)"
}
```

A promoted inline enqueue additionally carries
`promoted_catalog_id` — the minted catalog id the extracted facts will
cite.

**Output (`dry_run`)**: the classifier's proposal — `disposition`,
`format`, `title`, `target_wiki_id`, `document_page`, `summary`,
`segments_planned`, `would_promote` — with nothing written (a dry run
never mints a catalog row).

**Errors**: `400 invalid_input` (unknown enum, empty/oversized text,
non-textual blob without `text`, missing `catalog_id`/`content`),
`403 sender_unauthorized` (catalog row not readable by the effective
sender), `503 service_unavailable` (no `llm.ingest` slot — the job
would never run), `501 not_implemented_phase_c` (`file`/`git`/`url`).

**Caveats**

- Enqueue is idempotent by (document sha256, owner) across non-failed
  jobs; `force` mints a fresh job. A promoted inline retry is
  idempotent on both layers (the blob bytes are the text verbatim, so
  blob dedup and job dedup key on the same content).
- ACL: extracted facts inherit the source catalog row's **current** read
  set; un-promoted inline sources are owner-only for the effective
  sender (a promoted one starts owner-only too — its fresh catalog row
  has an empty allow list — and then widens with the anchor's read set
  like any media source).
- Corpus→pages import (a document becoming *its own pages* in its own
  container) is a deferred extension
  (extensions); real bulk wiki import
  still goes through `mwe-core` as a library.

---

## G. Dashboard

### `dashboard_link`

Mint a **single-use** link + URL into the built-in dashboard PWA.
The consumer surfaces it as a button or inline link when it recognises
a structural intent. See
`jwt-and-session-model.md`
and `dashboard.md`.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `intent` | enum (below) | yes | The initial dashboard page. |
| `sender_id` | string | no | Validated against the token. |
| `context` | object | no | `wiki_id` (required for `modify_wiki` / `view_wiki`), `proposal_id` (required for `answer_proposal`), `chat_seed` (pre-fills the omnipresent chat), plus arbitrary extra keys. |
| `channel` | enum `telegram` \| `discord` \| `slack` \| `browser` \| `vscode` \| `voice_fallback` | no | **Accepted but not used** for output shaping today. |

`intent` ∈ `home`, `modify_wiki`, `view_wiki`,
`answer_proposal`, `archive_view`, `audit`, `costs`, `settings`.

**Output**

```jsonc
{
  "url": "/dashboard/auth/link?token=…&next=%2Fdashboard%2Fwiki%2Falice-tecnica",
  "token_expires_at": "2026-05-26T10:10:00Z",
  "base_ttl_seconds": 600
}
```

**Errors**: `400 invalid_input` (unknown intent, or a missing required
`context` field), `403 sender_unauthorized` (`settings` / `audit` /
`costs` are admin-only), `403 sender_token_mismatch`,
`500 internal_error` (JWT minting failure).

**When to use it.** The consumer calls `dashboard_link` when it
recognises a structural intent it must not handle inline. Canonical
mappings (the full decision tree lives in
[`AGENT_INSTRUCTIONS.md`](../../AGENT_INSTRUCTIONS.md)):

- User: *"I want to edit my gardening wiki"* →
  `dashboard_link(intent="modify_wiki", context={ wiki_id, chat_seed })`.
- The consumer drains a `structure_proposal` (or `archive_proposal`)
  event from `events_poll` →
  `dashboard_link(intent="answer_proposal", context={ proposal_id })`,
  and forwards the link instead of dispatching the questionnaire
  inline.

**Caveats**: the `url` points at the **single-use** redemption endpoint
`/dashboard/auth/link` (0032). Opening it once verifies + burns the link
token (a compare-and-set on its `jti`), sets the real sliding session
cookie, and redirects to the `next` deep-link; a second open shows a
"link already used" page. The link token (10-minute TTL) is scoped to
the caller's `sender_id` — in the diagonal model, the act-as human — so
it cannot be handed to another user to impersonate them, and it is *not*
the session cookie it mints. The spec's `delivery_hint` field is not in
the current output.

---

## H. Smart-wiki admin (smart consumers)

**Smart** consumers (Claude Code, Codex, Cowork —
any MCP-compatible agent whose token carries `consumer_class=smart`)
can act as custodians of the writes into the user's **smart wikis**.
A smart-wiki is one whose `_meta.md` `smart:` flag is `true` (a per-wiki
bool; the REM cycle loads these into a cycle-scoped `SmartWikiIndex`,
and `wiki_admin` derives the flag on
`create` from the `wiki-companion` type-string prefix).
See `smart-wikis.md`.

Common auth gates for the write tools:

1. `consumer_class=smart` (else `403 requires_consumer_class_smart`).
2. The wiki's owner_user matches the token's sender-derived owner_user
   (else `403 wiki_owned_by_other_user`).
3. The wiki's `_meta.md` `smart:` flag is `true` (else
   `400 wiki_type_not_admin_writable`).

`wiki_admin_notify` is the exception — it is open to any token with
read access (see below).

### `wiki_admin_push`

Authoritative write into a smart-wiki. No server-side LLM — content
is written verbatim.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `mode` | enum `create` \| `upsert` | yes | `create` forges a new wiki under `parent_wiki_id/slug`; `upsert` overwrites pages and applies `deletes` on an existing wiki. |
| `wiki_id` | string | for `upsert` | Forbidden on `create` (derived from parent + slug). |
| `parent_wiki_id` | string | for `create` | New wiki lands as a child. |
| `slug` | string | for `create` | Directory slug. |
| `title` | string | for `create` | Display title. |
| `wiki_type` | string | for `create` | A bare, free-form type string. A `wiki-companion*` value makes the new wiki smart, stamped into its `_meta.md` `smart:` flag. |
| `project_id` | string | no | Stable opaque project id; stamped into `_meta.md.extra.project_id`. |
| `pages` | `array<{ path, content }>` | yes | `content` is full markdown including frontmatter. |
| `deletes` | `array<string>` | no | Honoured only on `upsert`. `_meta.md` is not deletable. |
| `mark_processed` | `array<string>` | no | Briefing-item ids (`bi_42`; the `bi_` prefix is optional) to mark `processed_at = NOW()` atomically with this push. Capped at 50. |

**Output**

```jsonc
{
  "wiki_id": "alice-acme",
  "ops_applied": { "created": 12, "updated": 2, "deleted": 0 },
  "op_log_id": "wol_2026-05-26-001",
  "warnings": [ "page 'misc/notes.md' deviates from the recommended folder_structure; allowed but flagged" ],
  "marked_processed": [ "bi_42" ],
  "authored_refs": [ "[[alice-acme/index]]", "[[alice-acme/modules/auth]]" ],
  "section_indexing": "queued"
}
```

`section_indexing` reports how the touched pages reach the section index:
`"queued"` (normal serve) — they were handed to the background reindex
queue and this ack returned immediately, so recall over brand-new
sections may lag by the queue depth; `"inline"` — indexed synchronously
before the ack (fallback when the queue is not wired: tests, degraded
boot). Either way the push itself is already committed.

`authored_refs` is one `[[wiki_id/page]]` provenance breadcrumb per written
page (`_meta.md` and deletes excluded). A smart consumer echoes these into the
next `wiki_ingest_message` (`metadata.authored_refs`) so personal memory
records a **reference** to the project page instead of duplicating its body —
the "link, don't duplicate" provenance tube (roadmap group 17).

**Errors**: `403 requires_consumer_class_smart`,
`403 wiki_owned_by_other_user` (also covers the ambiguous-owner case),
`400 wiki_type_not_admin_writable`, `400 wiki_type_requires_parent`
(the smart-family child-only gate — a smart wiki created without
a `parent_wiki_id`), `404 not_found`, `400 invalid_input` (bad mode / id
/ slug), `423 wiki_locked_by_lease` (another smart consumer holds a
lease), `400 unknown_briefing_item_id` and `400 too_many_briefing_items`
(the `mark_processed` validation — the whole push rolls back),
`500 internal_error`. (A non-`wiki-companion` type from a smart consumer
is rejected as `400 wiki_type_not_admin_writable`, not an "unknown type"
— there is no registry to be unknown against.)

**Caveats**: each push writes an append-only `wiki_admin_op_log` row
(`actor_kind='smart_consumer'`, `payload_hash` = sha256 of the
canonical input — never raw content — plus a `pre_image_json` snapshot
so the dashboard revert button can roll back). `expected_op_log_head`
optimistic concurrency **is enforced** on `upsert`: a push whose
`expected_op_log_head` is behind the wiki's latest `push_*` op
(`push_create` / `push_upsert`, including a dashboard revert's
compensation row — `pull` / `notify` rows do not count) is rejected with
`409 conflicting_op_log_head` before any disk write; omit it for
last-writer-wins. The schema's `snapshot_replace` mode stays reserved —
the current modes are `create` / `upsert` only. **No total push-size cap is
enforced today**: the only hand-rolled bound in the push path is
`mark_processed`'s `MARK_PROCESSED_CAP_PER_PUSH` (50). There is no
page-count or byte ceiling on `pages` / `deletes` and no
`push_too_large` wire class — the formal spec's 200-page / 2 MB soft
cap (`429 push_too_large`) is unimplemented; do not model it.

### `wiki_admin_pull` *(read-only)*

Dual of push. Returns every page of a smart-wiki the caller owns,
plus the latest `op_log_head`.

**Input**: `{ wiki_id }` (required).

**Output**

```jsonc
{
  "wiki_id": "alice-acme",
  "pages": [ { "path": "index.md", "content": "…" } ],
  "op_log_head": "wol_2026-05-26-001"
}
```

**Errors**: same auth gates as push, plus `404 not_found`.

**Caveats**: the `op_log_head` is the value to stamp and pass back as
the next push's `expected_op_log_head` — the optimistic-concurrency gate
**is wired** (a stale head → `409 conflicting_op_log_head`). Delta-pull
(`since_op_log_id`) is not supported — the pull is always a full pull. Used to rebuild a missing local `.mwe/wiki/`
cache or realign after a token revoke.

### `wiki_admin_notify`

Append an item to a smart-wiki's `_briefing.md` (and a mirror row
in `wiki_briefing_items`). **Open to any token with read access to the
target wiki** — a standard consumer (e.g. openclaw) must be able to
relay a user observation into the smart consumer's briefing.
Rate-limited 50/wiki/hour.

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `wiki_id` | string | yes | Family resolved server-side. |
| `topic` | string | yes | ≤ 200 bytes. |
| `body` | string (markdown) | yes | ≤ 4 KB. |
| `source` | `{ kind: enum user\|rem\|consumer\|dashboard, ref: string }` | yes | Attribution (`user:frodo`, `cc-laptop`, …). |
| `kind` | enum `observation` \| `reasoning` \| `external` | no (default `observation`) | Three-layer classification. |
| `target_cite` | string | no | Stable handle `wiki://<wiki_id>/<page_path>(#<heading-slug>)?`. Server-validated; rendered as an inline link. |
| `ts` | iso8601 | no | Default server now. |

**Output**

```jsonc
{ "briefing_item_id": "bi_2026-05-26-007", "ts": "2026-05-26T10:00:00Z" }
```

**Errors** — gated by a `consumer_class × wiki-family` matrix:

| Caller `consumer_class` | Target family | Outcome |
|---|---|---|
| `smart` | `true` | `403 smart_does_not_notify_own_wiki` — a smart consumer administers its own smart wiki via push, not notify. |
| `smart` | `false` | Append to `wiki_briefing_items` (DB; standard wikis have no `_briefing.md`). |
| `standard` | `true` | Append to `_briefing.md` + `wiki_briefing_items` (the classic relay channel). |
| `standard` | `false` | `403 standard_uses_ingest_for_memory` — the channel for a standard consumer on a standard wiki is `wiki_ingest_message`. |

Plus `400 consumer_class_wiki_family_mismatch` (forward-compat
fallback), `403 sender_unauthorized` (folds both read-access denied and
ambiguous owner — `tools.rs::briefing_error_to_tool_error`),
`404 not_found` (a missing target wiki), `429 rate_limited`,
`400 invalid_input`. (`400 wiki_type_not_briefing_capable` is preserved
for REM-internal callers; the public path uses the matrix variants.)

### `wiki_admin_lease_acquire`

Acquire (or extend) an opt-in **cooperative lease** on a smart-wiki.
Smart consumers only. While a lease is active, a
`wiki_admin_push` from any *other* consumer fails with
`423 wiki_locked_by_lease`. Re-acquire by the same caller extends the
existing row. It is a coordination layer, not a mutex.

**Input**: `{ wiki_id (required), ttl_sec? (1–300, default 60) }`.

**Output**

```jsonc
{
  "lease_id": "…",
  "wiki_id": "alice-acme",
  "sender_id": "alice",
  "consumer_id": "cc-laptop",
  "acquired_at": "…",
  "expires_at": "…",
  "renewed": false                     // true when the same caller extended an existing lease
}
```

**Errors**: `403 requires_consumer_class_smart`, `400 invalid_input`
(bad TTL / id), `423 wiki_locked_by_lease`, `500 internal_error`.

### `wiki_admin_lease_release`

Release a lease the caller holds. Smart consumers only. The
lease is one-shot: releasing an already-released, expired, or foreign
lease returns `404 not_found`.

**Input**: `{ lease_id }` (required).

**Output**: `{ lease_id, wiki_id, released_at }`.

---

## I. Skill catalog

Server-served operational instructions. Open to **every**
authenticated token — listing and fetching skills is discovery, not a
write. Skills are **bundled only** — they ship in the binary via
`rust-embed`. There are no custom smart-family skills. The same catalog is
also exposed over plain HTTP at `/skills` / `/skills/<name>.md`.

### `skill_list` *(read-only)*

**Input**

| Field | Type | Required | Notes |
|---|---|---|---|
| `consumer_class` | enum `smart` \| `standard` | no | **Accepted but unused** — reserved for future class-aware filtering. |

**Output**

```jsonc
{
  "skills": [
    {
      "name": "core",
      "version": "…",
      "description": "…",
      "depends_on": [],
      "etag": "sha256:…",              // content hash; matches skill_fetch
      "source": { "kind": "bundled" }
    }
  ]
}
```

The bundled skill names live in `crates/mwe-core/skills/`. Do not
hardcode the bundled list here — read it off the embedded directory.

### `skill_fetch` *(read-only)*

Fetch the full markdown body of one bundled skill.

**Input**: `{ name (required), version? }`. `version` is **accepted but
ignored** today (the only on-disk version is the current one; the
`/skills/<name>/<version>.md` HTTP path is future plumbing). Empty
`name` → `400 invalid_input`.

**Output**: the `skill_list` summary for that entry **plus** a
`content` field with the markdown body. `etag` matches `skill_list` so
a consumer can short-circuit on a cache hit.

**Errors**: `404 not_found`, `400 invalid_input`, `500 internal_error`.

---

## K. Smart bootstrap & contextual recall (smart consumers)

Two atomic primitives for the Claude Code hook bundle (served at
`/connect/hooks/claude-code.json`), both gated on `consumer_class=smart`.
They are thin orchestration over existing recall / pull /
briefing primitives, exposed as dedicated tools so a hook handler can
call them by name and inherit the filter logic without composing
`wiki_search` itself. Both are `read-only`.

### `smart_bootstrap` *(read-only)*

Surface the smart consumer's session-start landscape — every
smart-wiki the caller owns, with pending briefing items and last
op-log activity. Designed for the `SessionStart` hook (fires with
`{}`).

**Input**: `{ project_hint? (string), briefing_limit_per_wiki? (1–50, default 5) }`.
`project_hint` is a case-insensitive substring matched against each
candidate's `_meta.md.extra.project_id`, slug, and title; matches float
to the top.

**Output**

```jsonc
{
  "caller_sender_id": "alice",
  "project_hint": "acme",
  "smart_wikis": [
    {
      "wiki_id": "alice-acme",
      "wiki_type": "wiki-companion",
      "title": "Acme", "slug": "acme",
      "project_id": "acme-monorepo",
      "matches_project_hint": true,
      "last_op_log_id": 47,
      "last_op_log_ts": "2026-05-26T10:00:00Z",
      "briefing_counts": {
        "pending_observation": 2, "pending_reasoning": 1,
        "pending_external": 0, "pending_unclassified": 0, "total": 12
      },
      "recent_briefing": [
        {
          "briefing_item_id": "bi_42",
          "kind": "observation",
          "topic": "…", "body": "…",
          "target_cite": "wiki://alice-acme/modules/auth.md#mfa-flow",
          "ts": "2026-05-26T09:55:00Z"
        }
      ]
    }
  ]
}
```

Sort order: `matches_project_hint` desc, `last_op_log_ts` desc,
`wiki_id` alphabetical.

**Errors**: `403 requires_consumer_class_smart`, `400 invalid_input`
(`briefing_limit_per_wiki < 1`), `500 internal_error`.

### `recall_core_global` *(read-only)*

Canonical "transversal recall" wrapper around `wiki_search`. Filters to
the caller's own `scope = user:<sender>` wikis **and** excludes
smart wikis (smart flag `true`), so project-bound memory does not
leak into unrelated work — the contract the bundled `core-globalmemory`
skill documents. The canonical transversal-recall call a smart consumer
makes (model-driven, or from a host's recall hook).

**Input**: `{ query (required), limit? (1–20, default 8) }`. The query
is trimmed; empty after trim → `400 invalid_input`. The limit is
server-clamped.

**Output**

```jsonc
{
  "query": "remind me about the lnprint MFA flow",
  "filter_applied": {
    "owner_user": "alice",
    "excluded_wiki_types": ["wiki-companion", "wiki-companion-acme"]
  },
  "hits": [
    { "wiki_id": "alice-tecnica", "wiki_type": "wiki-tech", "fact_id": "fact_…", "snippet": "…", "score": 0.83 }
  ]
}
```

`excluded_wiki_types` echoes the smart-family stems the server
pre-filtered, so the caller's audit trail is unambiguous.

**Errors**: `403 requires_consumer_class_smart`, `400 invalid_input`,
`500 internal_error`.

---

## L. Forget (authority-routed)

### `wiki_forget` *(destructive)*

Forget one fact by id on behalf of the connected sender, **routed by the
caller's authority over the fact**. This is the consumer-MCP half of the
forget model: a sender
deletes their own contribution directly; a non-sender owner opens an audience
vote; everyone else is refused. **Casting the vote is dashboard-only — there is
deliberately no consumer vote tool.**

**Input**: `{ fact_id (required), reason? }`. `fact_id` is a UUIDv7 (as
returned in a `wiki_search` hit's `fact_id`); a malformed id → `400
invalid_input`. `reason` is accepted as a free-form audit note (not yet
persisted).

**Routing** (after loading the fact via `fact_index::find_by_id`):

- **missing** → `404 not_found`.
- **already tombstoned** (`deleted_at` set) → idempotent success,
  `{ "outcome": "already_forgotten", "fact_id": "…" }` (never an error).
- **caller may delete directly** — `acl::can_delete(fact.sender_id, caller,
  is_admin)` is `true` (the caller is the fact's `sender`/author, or an admin) →
  forget now via `capture::wiki_forget(reason = "consumer_forget")` — the
  `deleted_at` tombstone plus the best-effort excision of the region's on-disk
  bytes (capture pipeline) →
  `{ "outcome": "forgotten", "fact_id": "…" }`.
- **non-sender owner** (subject / owning-group member, `acl::sender_owns`) —
  forgetting a fact you did not author needs an **audience vote**, and a vote is
  opened **only from the dashboard**, never started in the background by the agent
  (maintainer 2026-06-29). The tool does **not** open a request; it steers the
  user there → `{ "outcome": "request_from_dashboard", "fact_id": "…", "detail":
  "…" }` (the agent surfaces the steer, e.g. with a `dashboard_link`).
- **anyone else** (not author, owner, or owning-group member) → `403
  sender_unauthorized`.

On the consumer MCP path the caller acts as the JWT's `sender_id` and is never an
admin (`is_admin = false`), so the author-or-admin branch reduces to "the caller
authored this fact".

**Output** (one of)

```jsonc
{ "outcome": "forgotten", "fact_id": "0190a0c8-…" }
```

```jsonc
{
  "outcome": "request_from_dashboard",
  "fact_id": "0190a0c8-…",
  "detail": "You own this fact but did not author it … open the request from the dashboard."
}
```

```jsonc
{ "outcome": "already_forgotten", "fact_id": "0190a0c8-…" }
```

**Errors**: `404 not_found`, `400 invalid_input`, `403 sender_unauthorized`,
`500 internal_error`.

### `wiki_forget_bulk` *(destructive)*

Bulk **self-delete**: tombstone every still-active fact the connected sender
authored, narrowed by `scope`. The bulk primitive of the forget model —
a contributor clears their own contributions in one act. There is no vote and no path to another author's
fact: only rows whose `sender_id` equals the caller's own principal are touched.

**Input**: `{ scope (required), wiki_id?, page?, reason? }`.

- `scope: "all"` — every fact the caller authored, across all wikis (`wiki_id` /
  `page` ignored).
- `scope: "wiki"` — those in `wiki_id` (**required**).
- `scope: "page"` — those on one page: `wiki_id` + `page` (**both required**);
  `page` is the page's file name within the wiki (e.g. `"vacanze.md"`; `.md` is
  appended if omitted). The canonical match is `source_path = wikis/<wiki_id>/<page>`.

A missing required id, or a `scope` outside the three, is `400 invalid_input`.
`reason` is a free-form audit note (not yet persisted). Backed by
`fact_index::mark_forgotten_by_sender(sender = caller, …, reason =
"consumer_forget_bulk")`, which skips already-tombstoned rows (idempotent);
each tombstoned fact's on-disk region is then excised best-effort
(`reindex::strip_fact_region` — leftovers ride the light-dream hygiene
sweep, redaction-policy).

**Output**

```jsonc
{ "outcome": "forgotten_bulk", "scope": "wiki", "wiki_id": "famiglia",
  "source_path": null, "forgotten": 4 }
```

**Errors**: `400 invalid_input`, `500 internal_error`.

---

## Internal surfaces NOT exposed over MCP

Two operative surfaces are part of the product but are **not** reachable
over MCP. A consumer agent never calls them. They are documented here so
the contract is complete.

### `_internal.*` — `mwe-core` library APIs

Anything an LLM-driven `wiki_ingest_message` turn, the dashboard chat,
or the nightly REM cycle does under the hood — capture a fact, supersede
an old one, recall topical context, forge a type, promote a paragraph —
is an `_internal.*` API of `mwe-core`, **not** an MCP tool. They are
reachable only by importing `mwe-core` as a library (migration scripts,
tests, batch admin tooling). A consumer that tries to invoke one over
MCP gets `not_found` ("unknown tool") from the dispatcher.

The conventional families (status varies — many are still planned;
read the symbols off `crates/mwe-core/src/`):

- **Capture / write**: `wiki_capture`, `wiki_supersede`, `wiki_forget`,
  `wiki_link`, `wiki_attach_file`, `wiki_write_page`.
- **Recall**: `wiki_recall`, `wiki_facts_for`, `wiki_recall_topic`,
  `resume_memory`.
- **Navigation**: `wiki_navigate`, `wiki_list_pages`, `wiki_get_meta`,
  `wiki_catalog_list`.
- **Identity**: `users_resolve`, `groups_describe`,
  `enrollment_reload`.
- **Archive**: `archive_search`, `archive_propose_list`,
  `archive_approve`, `archive_reject`, `archive_restore`,
  `archive_query_target` (the human approval / restore flow is not yet
  implemented).
- **System**: `run_cycle` (forces a REM cycle — see
  `rem-cycle.md`),
  `wiki_change_scope`, `structure_proposal_emit`.

#### Two closed taxonomies the capture path carries

These two enums are part of the internal capture contract — they live
nowhere else on the public surface, but a consumer that ever inspects a
`fact_index` row (via `wiki_facts_for`-style internal tooling, or by
reading the raw markers) needs the closed sets:

- **`fact_type`** (the taxonomy hint on `wiki_capture` / `fact_index.fact_type`)
  is a **closed 7-value set**: `bio | state | preference | rule | plan |
  episode | other`. Note it is stored as a free `Option<String>` at the
  capture layer (the closed set is the contract the `ingest` LLM emits
  and the dashboard / REM honour, not a column constraint).
- **`wiki_link.predicate`** (the subject–predicate–object link helper,
  which appends an Obsidian wikilink and produces **no** `fact_index`
  row) is a **closed 8-value set** in the MVP: `lavora_a | abita_a |
  conosce | parente_di | usa | possiede | partecipa_a | menziona`.
  Consumer-defined custom predicates are not yet supported (planned —
  see the roadmap).

#### Dashboard-only write paths (also not on MCP)

The proposal write engine (`apply_proposal`, `confirm_proposal`,
`revert_proposal`, and the auto-apply / auto-finalize / expire sweeps —
see family C above and
`proposal-apply-engine.md`)
lives here. So do two dashboard surfaces:

- **`wiki_admin::op_revert`** (`mwe-core` public API) rolls back a
  single `wiki_admin_op_log` row from the dashboard op-log view. The
  algorithm: (1) load the target row — fail `NoPreImage` if
  `pre_image_json IS NULL`; (2) **conflict
  check** — scan op-log rows on the same wiki with a later `ts` and
  refuse if any touched an overlapping page
  (`TargetChanged { conflicting_ops, conflicting_pages }`); (3)
  otherwise restore each `(path, content)` from the pre-image (deleting
  pages whose pre-image `content` was `null`) and re-run the
  `fact_index` pipeline; (4) insert a compensating `actor_kind='system'`
  row. The dashboard wire-error mapping is `404 op_not_found`,
  `409 op_log_target_changed_since` (payload carries the conflicting ops
  + pages), `400 op_not_revertable` (no pre-image, or a non-write
  `op_kind`). Policy is strict: no force/merge revert — on conflict the
  user resolves manually.
- **The citation resolver `GET /cite/<bi_id>`** (no auth — it only
  translates, the destination page enforces auth) resolves a
  `wiki_briefing_items` citation id to a deep link: (1)
  `SELECT target_cite WHERE id = ?`; (2) `404` if the id is unknown,
  `404` ("no anchor") if `target_cite IS NULL`; (3) `parse_cite` →
  `{ wiki_id, path, anchor }`; (4) `302` redirect to
  `/dashboard/wiki/<wiki_id>/<path>#<anchor>`. Smart consumers embed
  `/cite/bi_042`-style links in their replies so the user lands on the
  exact region.

Structural writes live on the dashboard, not the chat surface; the
revert + citation routes live there too.

### Dashboard agentic chat — `AgenticTool`

The omnipresent chat panel in the built-in dashboard runs its own LLM
agentic loop whose tools are a typed whitelist over `_internal.*`. They
live in
[`crates/mwe-dashboard/src/agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)
(`AgenticTool`) and are reachable **only** from the dashboard chat —
not MCP, no JWT round-trip (the loop runs in-process and carries the
connected `SessionUser`'s `SenderContext`, so the same ACL filters
apply). See `agentic-chat.md`. The
chat is an **operative** tool on the memory (CRUD / structure), not a
conversational memory — it deliberately refuses "what do you know about
me?".

The registered `AgenticTool` variants (read the live set off
`agentic.rs`):

| Tool | R/W | Purpose |
|---|---|---|
| `wiki_recall` | read | Semantic recall over `fact_index`, ACL-filtered. |
| `wiki_list_pages` | read | Active pages of one wiki. |
| `wiki_get_meta` | read | `_meta.md` view of one wiki (title, type, slug, `scope`, parent). |
| `structure_proposal_list` | read | List proposals (internal cap 20). |
| `structure_proposal_get` | read | Full row of one proposal. |
| `structure_proposal_apply` | write | Apply a proposal with answers (threads `hub_writer` for forge kinds). |
| `wiki_facts_for` | read | Filtered fact listing (wiki / topic / fact_type / date). |
| `wiki_forget` | write | Tombstone a single fact — authority-routed (sender-direct; a non-sender owner is steered to the dashboard). |
| `wiki_supersede` | write | Replace a fact in place, inheriting owner / ACL / fact_type / topics. |
| `wiki_change_scope` | write | Re-parent a wiki (or promote to root); stable `wiki_id`. |
| `structure_proposal_revert` | write | Undo a previously-applied proposal (inverse of `structure_proposal_apply`); reuses `revert_proposal`. Headline: undo an act-first structured-wiki emergence — the emerged wiki is deleted unless it is "in use". Status-driven `RevertAuth` + 0032 recipient gate. |
| `structure_proposal_confirm` | write | Confirm a sweep auto-applied proposal (`applied_pending_confirm → applied`) so it sticks — counterpart of `structure_proposal_revert`; reuses `confirm_proposal` (gates by recipient/admin internally). Mints the `revert_token` + opens the 7-day window. |

Write tools are gated by the system prompt requiring an explicit user
confirmation in the same turn — the safety is the operator's authorship
of the prompt, not a whitelist veto. The loop is capped at
`MAX_AGENTIC_ITERATIONS` (8) per submission. Agentic errors
(`AgenticToolError`: `UnknownTool`, `InvalidArguments`,
`InternalFailure`) are serialised back to the model as tool results so
it can recover — they are not the MCP `ToolError` classes.

---

## Related pages

- [`mcp-tools.md`](mcp-tools.md) — roster + per-tool status overview.
- `mcp-dispatcher.md` — dispatch,
  audit, and error-mapping internals.
- `jwt-and-session-model.md`
  — token claims, consumer class, sliding-TTL sessions.
- `smart-wikis.md` — the
  smart-wiki model behind families H / J / K.
- `ingest-pipeline.md` — what
  `wiki_ingest_message` runs internally.
- `agentic-chat.md` — the dashboard
  agentic loop.
- [`../../AGENT_INSTRUCTIONS.md`](../../AGENT_INSTRUCTIONS.md) — the
  consumer-agent decision tree (which tool to call per turn).
