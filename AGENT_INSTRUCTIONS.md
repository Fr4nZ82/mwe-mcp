---
title: AGENT_INSTRUCTIONS — mwe-mcp consumer agent usage spec (slim core)
status: canonical
last_review: "2026-05-30"
---

# AGENT_INSTRUCTIONS

This document is the **slim bootstrap** for any LLM agent that
consumes mwe-mcp via MCP. It defines: who you are on the wire, how
to load the rest of the contract via the skill machinery, and the
identity-side prerequisites that depend on whether your deployment
is mono-user (Pattern A) or multi-user (Pattern B).

The substantive per-class behaviour — the conversational loop, the
smart-wiki management, the structural-notice handling, the
anti-patterns — lives in the **bundled skills** fetched via
`skill_list` / `skill_fetch`. Read this file once to bootstrap your
agent, then let the skills carry the rest.

## 0. Audience

This file is for the **LLM client that integrates mwe-mcp via MCP**.
It is NOT for an agent working on the mwe-mcp codebase itself. If you
find yourself on this page while editing files under `crates/mwe-core/`
you took a wrong turn — close this.

---

## 1. Cardinal rule (TL;DR)

mwe-mcp is the **persistent memory** of an LLM agent. What you call
depends on your `consumer_class` claim:

- **Standard consumers** (openclaw, hermes, nanoclaw): every user
  turn → `wiki_ingest_message` (thin passthrough; mwe-mcp's
  server-side `ingest` slot does classification, recall, capture).
  Media (photos, voice notes, documents) travel out of band: upload
  the bytes via `POST /media` first, then pass the minted
  `catalog_id` in the same turn's `attachments` array (with non-empty
  `text` — the caption, or `[media]`) — the server describes and
  files them as facts. Full per-turn loop in the
  **`standard-conversational`** skill.
  When the user hands you a **document to read into memory** (a file
  they want remembered or consultable, a long transcript), do not push
  it through the turn loop: call `wiki_ingest_external` with
  `source.type=media` (the uploaded `catalog_id`; pass `text` yourself
  for non-textual files like PDF) or `source.type=inline`. The memory
  decides whether it stays a consultable unit, a dossier with selective
  extraction, or dissolves into facts — force `disposition` only when
  the user's words carry the intent ("tienimelo da parte" → `consult`;
  "ricordati tutto" → `dissolve`). You get a `job_id` receipt at once
  and a `document_ingested` notice on `events_poll` when it lands —
  relay it.
  **The server backstops this routing**: document-shaped text — pasted
  into the chat (an oversized turn) or sent as `source.type=inline` —
  is auto-promoted to the media rail (verbatim blob + `catalog_id`), so
  the original stays preserved and citable even when the caller misses
  the gesture. The `promote: always | never` dial overrides the
  heuristic in either direction; when a turn response carries
  `document_promoted`, tell the user their document was archived and is
  being read into memory.
- **Smart consumers** (Claude Code, Cowork, Codex — own subscription
  LLM): bring their own classification budget. `wiki_search` for
  recall, `wiki_admin_push/pull` for authoritative smart-wiki
  management. Full pattern in the **`smart-consumer`** skill
  (cwd-bound mode) or **`core-globalmemory`** (transversal mode).

The dispatcher in skill `core` picks which deeper skill to load. The
choice is derived from your JWT (`consumer_class`) + cwd
(`.mwe/state.json` present or not).

---

## 2. Connection

Connect over **MCP Streamable HTTP** to `<mwe-mcp-host>/mcp` with an
`Authorization: Bearer <jwt>` header. The JWT is issued by the
operator via the dashboard tokens page (`/dashboard/tokens`) or the
`mwe-mcp token-issue` CLI.

Three claims decide your wire identity:

| Claim | Required | Meaning |
|---|---|---|
| `sender_id` | yes | The human owner. Captures land in `wikis/<sender_id>/`. |
| `consumer_id` | optional (required when `smart`) | Your device label (e.g. `cc-laptop`, `samvise-prod`). Distinguishes devices of the same user in the audit log and the cooperative lease. |
| `consumer_class` | optional, default `standard` | `smart` enables `wiki_admin_*`. |

You do **not** pass `sender_id` in any tool argument — if you include
it, the server validates it matches the token claim and rejects on
mismatch with `403 sender_token_mismatch`.

---

## 3. Connection patterns — set by your `consumer_class`, not chosen

Your connection pattern is **not** a free choice: it follows your
`consumer_class` claim (the **diagonal identity model**). A **smart**
consumer is Pattern A; a **standard** consumer is Pattern B. The JWT
shape, the per-call HTTP headers, and which wiki holds the captures all
follow from that.

|  | Pattern A — smart consumer (mono-user) | Pattern B — standard consumer (multi-user) |
|---|---|---|
| **Who** | a coding agent maintaining a project / smart wiki on one developer's machine | a conversational assistant (Telegram, home automation, mail) serving several people |
| **`sender_id` is** | the **human owner** (an account with login credentials) | a **system user** — the bot's own credential-less identity, with its own wiki |
| **JWT claims** | `sender_id = <human-user-id>` + `consumer_id = <device-label>` + `consumer_class = smart` | `sender_id = <bot-system-user-id>` + `consumer_id = <deployment-id>` (`consumer_class = standard` is the default, wire-omitted) |
| **`X-MWE-Act-As` header** | Never set. Setting it returns `403 act_as_requires_standard`. | Optional per-call. Set to act on behalf of a real user; omit to act as the bot itself. |
| **Where captures land** | `wikis/<human-user-id>/` | With header: `wikis/<real-user-id>/`. Without: `wikis/<bot-system-user-id>/`. |
| **Prerequisite** | Your human user exists in `enrollment_users` *with* a `user_credentials` account. | The bot's identity exists as a **system user** in `enrollment_users` (no credentials), is bound to the consumer (`consumers.system_user_id`, set at `consumer_register`), and the dashboard records the **delegation list** (which real users the bot may act as). |

### Pattern B mechanics (A.17)

For each MCP call the bot picks one of:

- **Acting on behalf of a real user.** Set `X-MWE-Act-As: <real-user-id>`.
  The middleware validates `(consumer_id, real-user-id)` against
  `consumer_delegations` (60s TTL cache, refreshed immediately on
  dashboard write), rewrites the request sender to the real user.
  From there the call is indistinguishable from a direct call by
  that user. This is also how you route a **structural-change
  notice**: drain `events_poll`, read `recipient_id` from the
  `structure_applied` / `dedup_proposed` / `auto_applied` payload,
  strip the `user:` prefix, and call `dashboard_link` with
  `X-MWE-Act-As: <that user>` — then relay the returned single-use URL
  (pointing at the notice's `dashboard_path`, the undo surface) to that
  human (e.g. on Telegram). On a `null` recipient, fall back to the
  admin.
- **Acting as the bot itself.** Omit the header. The effective
  sender stays the bot's synthetic id; captures and recall resolve
  against the bot's own wiki.
- **Acting as `guest` — the unidentified human.** When the person
  speaking cannot be resolved to an enrolled user (unrecognized voice,
  unknown chat sender), set `X-MWE-Act-As: guest` (works once the
  operator ticked `guest` in your delegation roster). The turn is
  **ephemeral**: recall returns public memory only, nothing is stored,
  and the ingest response's `rules` field tells you to behave
  reservedly — apply it: don't disclose household information beyond
  the returned context, don't act on enrolled users' behalf, and never
  promise to remember ("I won't remember this — ask <admin> to enroll
  you if I should"). Tools that leave permanent state
  (`wiki_ingest_external`, media upload, `wiki_admin_notify`,
  `consumer_register`) and operator surfaces (`tool_log_search`,
  `dashboard_link`) answer `sender_unauthorized` on guest turns —
  expected, not an error to retry.

Stable error wire codes:

| Code | When |
|---|---|
| `403 act_as_requires_standard` | Header set on a **smart** token. Smart consumers are mono-user (Pattern A) and may not delegate, even though they carry a `consumer_id`. |
| `403 act_as_requires_consumer` | Header set but the JWT has no `consumer_id`. |
| `403 act_as_not_delegated` | The `(consumer_id, real-user-id)` pair is not in `consumer_delegations`. |
| `403 act_as_malformed` | Empty or malformed header value. |

### Operator-side setup for Pattern B

Performed once by the human operator via the dashboard:

1. **Create the synthetic identity as a system user.** From
   `/dashboard/users/new`, create a user with `user_id =
   <bot-synthetic-id>` (e.g. `samvise-bot`). The dashboard inserts
   into `enrollment_users` and materializes `wikis/samvise-bot/`.
   It generates a single-use invitation link — **discard it** so the
   account has no `user_credentials` and stays a non-loggable
   system user.
2. **Issue the consumer token.** From `/dashboard/tokens`, set
   `sender_id = samvise-bot`, check "Consumer token", fill
   `consumer_id = samvise-prod` (deployment id), pick the allowed
   senders for the delegation list. Token is shown **once** —
   copy immediately (A.7 policy).
3. **Hand the token to the consumer dev.** That JWT goes in
   `Authorization: Bearer …` on every MCP call.

---

## 4. Bootstrap dispatcher

```
on_session_start():
    load_skill("core")                          # always

    if cwd_has_mwe_state(".mwe/state.json"):
        load_skill("smart-consumer")           # cwd-bound mode
        load_skill("smart-codebase")   # iff project_kind == software
        smart_bootstrap()                       # see smart-consumer

    elif consumer_class == "smart":
        load_skill("core-globalmemory")         # transversal recall mode

    else:
        load_skill("standard-conversational")   # per-turn ingest pattern
```

Skills are fetched via `skill_list` (catalog) + `skill_fetch` (one
skill body, with ETag caching). The catalog is **bundled only**; a
smart wiki's shape is the smart consumer's own concern, not a registered
type.

---

## 5. Skill catalog

| Skill | When to load | Defines |
|---|---|---|
| `core` | always | identity claims, dispatcher, token lifecycle, auth error codes |
| `core-globalmemory` | smart consumer, **no** cwd marker | transversal recall on first prompt (forked-subagent pattern) |
| `smart-consumer` | smart consumer + `.mwe/state.json` in cwd | `smart_bootstrap`, `wiki_admin_*`, cooperative lease, `_briefing.md` lifecycle, graceful degradation on token revoke |
| `smart-codebase` | `smart-consumer` + software project | `docs/` conversion, modules/decisions/runbooks/architecture layout, `source_ref` + `last_synced` discipline |
| `standard-conversational` | standard consumer (or absent claim) | `wiki_ingest_message` loop, wire shape, disambiguation, `pending_attention`, `events_poll`, structural notices + undo routing, consumer self-configuration |

How to consume skills (three modes, by preference):

1. **Native skill files** (Claude Code, Cursor, IDE agents with a
   skill mechanism): `skill_fetch` → install into the agent's skill
   directory.
2. **System-prompt augmentation** (any LLM client): concatenate the
   skill body to the system prompt. High prompt-cache hit ratio on
   stable content.
3. **`InitializeResult.instructions`** (future): the MCP `initialize`
   handshake will push the relevant skills. Until shipped, mode 1
   or 2.

The pagination metadata (`etag`) lets your consumer skip the
re-download when the skill hasn't changed (HTTP `If-None-Match` →
304). Cache locally.

For per-consumer onboarding (pointing a consumer at `/mcp`, issuing the
JWT, wiring a bridged host) see the dashboard home's *Connect a consumer*
card and the **Bridges** tab — the operator-facing handoff. The bundled
hook envelopes for hook-capable hosts remain at
`/connect/hooks/<consumer>.json`.

---

## 6. Tool surface — families A–K

The public MCP surface is organised by **family** (A–K); the exact
roster and tool count are **canonical in the engineering wiki** at
[`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md) (mirrored
from the SSOT `schemas::all_tools()` in the code), with the full
per-tool contract (parameters, returns, errors, side effects) in
[`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md).
Don't pin a count here — it drifts. What you need to know to
bootstrap is which family covers which job:

| Family | Covers | Who calls it |
|---|---|---|
| **A — Conversation** | The standard-consumer workhorse: every user turn → server-side ingest (classify / recall / capture / structural hint). | standard |
| **B — Events** | Cooperative polling + acknowledgement of async events (incl. `structure_applied` notices — the undo surface is the dashboard). | any |
| **D — Read (consumer UI)** | ACL-filtered page read + vector / full-text search. | any |
| **E — Audit / health** | Audit-trail query + wiki lint pass. | admin |
| **F — Setup** | First-time consumer registration + bulk external import. | any |
| **G — Dashboard** | Mint a one-shot signed URL into the built-in PWA. | any |
| **H — Smart-wiki admin** | Authoritative smart-wiki management: push / pull, briefing notify, cooperative lease. | smart |
| **I — Skills** | Enumerate + fetch skill bodies (bundled). | any |
| **J** *(unused)* | `J` is a hole in the MCP family scheme; a wiki's shape is decided per fact, not by a registered type. | — |
| **K — Smart-consumer bootstrap** | Session-start smart-wiki landscape + transversal contextual recall (hook-driven). | smart |

The server also composes a larger set of `_internal.*` operations
(atomic capture / recall / supersede / forget / navigate / forge,
etc.) internally when handling `wiki_ingest_message` or the dashboard
chat panel. They are **not exposed via MCP** — the dispatcher returns
`403 not_exposed` on direct calls. The illustrative roster lives
alongside the public surface in
[`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md); it is
`mwe-core`'s own seam and is not a stable API.

---

## 7. Token lifecycle

- **Internal token** (1 year TTL): local-device clients on the
  operator's own machine.
- **Exposed token** (30 day TTL): public-internet clients. Refresh
  proactively when `exp - now < 7 days` via `POST /mcp/token-refresh`.
- **Session cookie** (10 min sliding): dashboard browser only, never
  over MCP.

On `401 revoked` / `401 secret_rotated` / `401 expired` the bot
**stops** and logs fatal. Admin intervention required (re-issue a
token, rotate the secret). Smart consumers degrade gracefully —
keep local `.mwe/wiki/` intact, queue writes, replay on new token.
See `smart-consumer` §"Graceful degradation" and `core` §"Auth
failure semantics" for the per-code behaviour.

The blacklist propagates within ~60s of a revoke. The
`consumer_delegations` cache has the same 60s TTL and is refreshed
immediately on any dashboard write.

---

## 8. Anti-patterns (the irreducible short list)

- ❌ **Client-side intent classification.** mwe-mcp's server classifies
  for standard consumers; smart consumers use `wiki_search` directly.
  Neither pattern wants you to pattern-match on user text.
- ❌ **Calling `_internal.*` tools directly.** Returns `403 not_exposed`.
- ❌ **Path-shaped `wiki_id`.** Use the opaque id returned by
  `capture_id` or `wiki_search` results.
- ❌ **`wiki_admin_*` writes from a standard consumer.** Returns `403
  requires_consumer_class_smart`. Notify-only (`wiki_admin_notify`) is
  open.
- ❌ **Mixing `X-MWE-Act-As` with a mono-user token.** Returns `403
  act_as_requires_consumer`.
- ❌ **Truncating chat history mid tool-use cycle.** Orphan `tool_use`
  blocks reject the next LLM API call. See
  `standard-conversational` §"Consumer self-configuration".

Per-class anti-patterns are exhaustive in each skill body.

---

## 9. References

The documentation set ([`docs/`](docs/)) is the reference for what the
system is and does (kept in lockstep with code):

- [`docs/protocol/mcp-tools.md`](docs/protocol/mcp-tools.md) —
  public tool surface (roster + families).
- [`docs/protocol/tool-reference.md`](docs/protocol/tool-reference.md) —
  exhaustive shape of every tool (input, output, errors, paging).
- [`docs/protocol/config-schema.md`](docs/protocol/config-schema.md) —
  protocol / config overview, auth, transport.
- [`docs/concepts/memory-model.md`](docs/concepts/memory-model.md) —
  identity model, wiki structure, ingest classifier philosophy, the
  `structure_proposals` lifecycle.
- [`docs/architecture/overview.md`](docs/architecture/overview.md) —
  what ships today, per crate.
- [`docs/architecture/runtime-topology.md`](docs/architecture/runtime-topology.md) —
  runtime / cost topology, REM cycle.
- [`docs/examples/scenarios.md`](docs/examples/scenarios.md) —
  end-to-end usage scenarios.
The smart-consumer contract (what a smart agent may and must do) is
§6–§8 of this document.
