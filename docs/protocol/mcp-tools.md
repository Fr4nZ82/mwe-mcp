---
title: MCP tools — public surface
area: protocol
status: implemented
last_review: "2026-06-29"
---

# MCP tools

mwe-mcp exposes **22 MCP tools** to consumer agents, grouped into
ten families (A–B, D–I, K and L). Anything else — atomic
capture/recall/forge operations, plus the whole structure-proposal
chassis — lives inside `mwe-core` as `_internal.*` / dashboard-only
APIs and is **not callable over MCP**. A consumer agent that tries to
call an `_internal.*` operation receives `403 not_exposed`.

> Status: the tools run through the
> `mcp` dispatcher. Most are at
> full coverage; a handful carry a documented partial caveat; the two
> smart-wiki bulk tools (`wiki_admin_push` / `wiki_admin_pull`)
> ship at MVP with `expected_op_log_head` optimistic-concurrency
> **enforced** on upsert (`409 conflicting_op_log_head`); only the
> `since_op_log_id` delta-pull is still deferred (see
> `smart-wikis.md` for the
> deferred-scope table). The whole `structure_proposal_*` family is
> off the surface: structural changes apply directly in REM and reach
> the consumer as `structure_applied` notices over `events_poll`; the
> dashboard is the undo surface (it calls `mwe-core::proposals`
> directly, no MCP round-trip). The per-tool wiring is canonical in
> `mcp-dispatcher.md`; the formal
> per-tool reference (parameters, returns, errors, side effects) lives
> in [`tool-reference.md`](./tool-reference.md). The roster itself is
> the SSOT `schemas::all_tools()`
> (`crates/mwe-mcp-server/src/mcp/schemas.rs`) — the table below
> mirrors it but the code is authoritative.
>
> **`tools/list` is profile-filtered.** `schemas::tools_for(profile)` shapes the
> advertised catalog to the caller's [`ConsumerProfile`](config-schema.md): the
> default `Local` (every bridged / local-FS consumer) sees the full roster
> below; a `Web` connection — the claude.ai web app over the `webagentoauth`
> flow — sees only the trimmed `WEB_TOOLS` whitelist (`wiki_search`,
> `wiki_navigate`, `wiki_read`, `wiki_ingest_message`, `wiki_ingest_external`,
> `wiki_admin_notify`, `recall_core_global`, `smart_bootstrap`, `wiki_admin_push`,
> `wiki_admin_pull`).
> It **keeps the full smart-wiki management surface** — `smart_bootstrap` (discover
> the wiki it owns), `wiki_admin_pull` (read its whole wiki), `wiki_admin_push`
> (write it) — because those are server-side reads/writes a web consumer still needs
> (it has no local copy, but the wiki lives on the server). It drops only what a
> bridge-less client genuinely can't use: the `wiki_admin_lease_*` pair (local
> multi-device coordination), the event loop, ops/registration plumbing, and the
> skill catalog (claude.ai uploads its own skill). **Call-time authorization is
> unchanged** — the filter only cuts routing noise; it never widens or narrows what
> a token may actually execute (that stays the per-tool class/ACL gate).
>
> **The `guest` effective sender is call-time-gated too.** When a standard
> consumer acts as the builtin `guest` pseudo-identity (unidentified human —
> [identity-and-acl.md §1](../concepts/identity-and-acl.md)), the reads behave
> normally (the ACL confines them to the public slice), `wiki_ingest_message`
> runs its ephemeral short-circuit, and `forbid_guest`
> (`crates/mwe-mcp-server/src/mcp/tools.rs`) refuses `wiki_ingest_external`,
> `wiki_admin_notify`, `consumer_register`, `tool_log_search` and
> `dashboard_link` with `sender_unauthorized` (plus `POST /media` → 403
> `guest_cannot_upload`). Not a profile: the catalog advertised is unchanged.

## The 22 tools at a glance

| Family | Tool | Status | Purpose |
|---|---|---|---|
| **A — Conversation** | `wiki_ingest_message` | full | Default conversational entrypoint. Internally drives `_internal.*` recall/intent/routing/capture via the LLM `ingest` slot — one round-trip returning strict JSON (intent + capture plan) routed to capture / recall snippet / structural dashboard hint / skip. Smart wikis (those whose `_meta.md` `smart:` flag is `true`) are filtered out of `available_wikis` before the LLM call — they are authoritatively-managed by smart consumers via `wiki_admin_*` and routing into them via ingest would double-bill the consumer's LLM budget. Optional `attachments` array links media uploaded out of band via `POST /media` — undescribed photos ride the classifier call as images, the captured fact carries the code-rendered `{{embed=…}}` marker (media pipeline). |
| **B — Events** | `events_poll` | full | Cooperative polling of async events (including the `structure_applied` notices for structural changes REM applied directly — each names the affected user and carries the undo `dashboard_path`). Reads from `wiki_events` filtered by `consumer_id` ACK state via `json_extract` on the `acks` JSON map. |
| | `events_ack` | full | Acknowledge presented events. Updates the `acks` JSON map; consumer-specific retention. |
| **D — Read (for consumer UI)** | `wiki_read` | full | Read a page with per-region ACL projection via `mwe_core::render::render_for_sender` (returns `content_rendered_for_sender` + `redacted_count`; the per-region predicate is the sole read gate — see `redaction-policy.md`). The page **frontmatter (testata) is stripped** before rendering (it is unmarked card metadata derived from the facts; `title`/`wiki_type`/`owner` are returned as structured fields instead), mirroring `recall_nav::open_projected`. `path` selects the page relative to the wiki dir (default `index.md`, `is_safe_page_path`-validated, unknown → `not_found`); body and ACL map resolve to the *same* page. Standard wikis redact marked regions; smart wikis are markerless, so their reads are governed by the wiki-level ACL projected onto their content-indexed rows (same `can_read`). The `format` / `include_archived` args are accepted but not yet honored. |
| | `wiki_search` | full | Top-K semantic search + recall-counter bump. The `scope` object's boolean `smart` selects the **corpus before ranking**: `false` searches the fact store (`fact_index`, standard-wiki memory), `true` searches the section index (`wiki_sections`, smart-wiki documentation), omitted searches both and merges the ranking. Because it picks a table rather than discarding hits afterwards, `top_k` is always honoured. Each result carries a `kind` (`fact` \| `section`); a fact result carries `fact_id`, a section result carries `section` (`<source_path>#<ord>`), `source_path` and `heading_path`. |
| | `wiki_navigate` | full | **Deep** recall — the funnel navigator (`mwe_core::recall_nav`) exposed as a tool: whole visible corpus, ACL-filtered, one LLM hop at a time, returning the navigated `(wiki, page)` path **plus** the flat hits (a superset of `wiki_search`). Seed cascade C→B→A: caller `topics`/`owners` → query extraction on the `navigator` slot → principal+RAG only. Smart wikis are funnel-skipped (surfaced via the flat hits). Degrades to flat-only without a `navigator` slot. The standard consumer keeps the automatic ingest injection instead; this is the **smart** consumers' explicit deep-recall tool (positioning, not a class gate). See recall-pipeline.md. |
| **E — Audit / health** | `tool_log_search` | full | Browse the audit trail of past tool calls (admin scope). Backed by `mwe_core::audit::search`. |
| | `wiki_lint` | partial | 4 of 8 advertised checks implemented (`MarkerMalformed`, `OrphanFacts`, `MetaInvalid`, `EmbedMissing`); the other 4 (`BrokenCrosslinks`, `AclInconsistent`, `HubOutdated`, `SupersededChain`) advertise zero-count summary entries and are still pending (they need the wikilink resolver / redaction projection from the recall pipeline). |
| **F — Setup** | `consumer_register` | full | Onboard a new consumer agent — idempotent on `consumer_id`, mints a 32-byte hex secret on a fresh row, preserves it on refresh. Gates `events_poll` / `events_ack`. |
| | `wiki_ingest_external` | implemented | Document ingest (long-form content that is not a turn): async job onto the disposition dial — `consult` (document page + blob, nothing scattered) / `dossier` (page + selective extraction) / `dissolve` (full extraction). Sources `media` (catalog id) and `inline`; the `text` trusted seam covers non-textual blobs; completion notice via `events_poll`. `file`/`git`/`url` still `501` (document ingest). **Verbatim source promotion**: document-shaped inline text is auto-promoted to the media rail (blob + catalog row) so facts cite the preserved original — `promote: always \| never` overrides; the same backstop guards oversized `wiki_ingest_message` turns (the paste-into-chat case). |
| **G — Dashboard** | `dashboard_link` | full | Mint a one-shot URL into the built-in PWA with a 10-min sliding-TTL session JWT (`mwe_core::jwt::issue` with `DASHBOARD_LINK_TTL=10min`). Admins may target `settings` / `audit` / `costs`; non-admin senders are restricted to their own routes. |
| **H — Smart-wiki admin** *(smart only)* | `wiki_admin_push` | partial (MVP) | Smart-consumer write into a smart wiki: `mode=create` instantiates a new wiki under `scope = user:<owner_user>` and stamps `project_id`; `mode=upsert` writes/deletes pages and refuses `_meta.md`. Auth gates: `consumer_class=smart` + `wiki.owner_user == token.owner_user` + the target wiki's `_meta.md` `smart:` flag is `true`. Append-only audit row in `wiki_admin_op_log` with `payload_hash` (sha256 of canonical input, never raw content) — also stamps `actor_kind='smart_consumer'` + `pre_image_json` (JSON snapshot of touched pages before the write) so the dashboard revert button can roll back the op. Optional `mark_processed: [bi_id, ...]` argument batch-marks listed `wiki_briefing_items` as `processed_at = NOW()` atomically inside the push transaction (validation fail-fast on unknown or cross-wiki ids → `400 unknown_briefing_item_id`; cap 50 ids/push → `400 too_many_briefing_items`). Optional `expected_op_log_head` (upsert) is the **enforced** optimistic-concurrency guard: a stale head — a newer `push_*` op (including a dashboard revert) landed since the caller synced — is rejected with `409 conflicting_op_log_head` (pull/notify rows never trip it). Only the `since_op_log_id` delta-pull is still deferred. |
| | `wiki_admin_pull` | partial (MVP) | Dual of push: returns every page of a smart wiki (one whose `_meta.md` `smart:` flag is `true`) + the current `op_log_head`. Used to reconstruct a missing local `.mwe/wiki/` cache or to realign after token revoke. Same 3-gate auth. `since_op_log_id` delta-pull is not yet implemented. |
| | `wiki_admin_signpost` | full | Write project **signposts** into the *owner's* standard wiki, on the reserved `projects.md`: a non-technical `description` of what the project is (≤400 chars, one per project) and an `activity` line for one day (≤250 chars, one per project per day, kept for a rolling 5 days). Smart consumers only, owner of the target smart wiki only. A signpost is a pointer, not a record — its job is to let a turn that never *names* the project still reach its documentation (the recall slot opens the project when a signpost surfaces). Deterministic: dedup off, identity is the topic key, unchanged text is a no-op. Over a cap ⇒ `400 invalid_input` with the measured length, **never truncated**. Read access mirrors the project's `shared_with`. `wiki_admin_push` returns `signpost_hint` when a description or today's line is missing. |
| | `wiki_admin_notify` | full (MVP) | Append a briefing item, gated by the `consumer_class × wiki_family` matrix (`gate_notify_target_matrix` in `mwe-core::briefing`) — NOT a flat "any reader" gate. The caller's `consumer_class` is crossed with the target wiki's `_meta.md` `smart:` flag: **standard consumer × smart wiki** is the canonical openclaw relay — appends a row to `wiki_briefing_items` **and** the rendered section to `_briefing.md` (file created on demand with `type: session_briefing` frontmatter, `last_updated` bumped in place). **smart consumer × standard wiki** appends a DB-only queue row (no `_briefing.md` exists on a standard wiki; REM Briefing-processor sub-job drains it). **smart consumer × smart wiki** is rejected with `403 smart_does_not_notify_own_wiki` — a smart consumer administers its own smart wiki via `wiki_admin_push`, not by notifying itself. **standard consumer × standard wiki** is rejected with `403 standard_uses_ingest_for_memory` — the canonical channel for a standard consumer on a standard wiki is `wiki_ingest_message`. (A forward-compat fallback `400 consumer_class_wiki_family_mismatch` guards any future unmatched combination.) Whichever cell passes, read access to the wiki is still resolved (owner always passes; `shared_with` roster) and the **50 notify/wiki/hour** cap applies. Optional `kind ∈ {observation, reasoning, external}` provides three-layer semantic classification — REM auto-routes its dispatcher findings (`observation`) and backlink-reciprocity recommendations (`reasoning`). Optional `target_cite` of the form `wiki://<wiki_id>/<page_path>(#<heading-slug>)?` points at the specific wiki section the item is about; validated server-side via `briefing::parse_cite` (`400 invalid_input` on malformed handles) and rendered inline in the briefing markdown as an Obsidian autolink so the smart consumer can jump straight to the relevant region. |
| | `wiki_admin_lease_acquire` | full (MVP) | Acquire (or extend) an opt-in cooperative lease on a smart-wiki. Smart consumers only. Returns `{ lease_id, wiki_id, sender_id, consumer_id, acquired_at, expires_at, renewed }`. Re-acquire by the same `(sender_id, consumer_id)` extends the existing row (`renewed: true`); different consumer ⇒ `423 wiki_locked_by_lease` with `held_by_consumer_id` + `expires_at` payload. TTL default 60s, server cap 300s. The lease is "I am authoritative on this wiki now", not a syntactic mutex — `wiki_admin_push` from any other consumer fails until release or expiry. |
| | `wiki_admin_lease_release` | full (MVP) | Release a lease the caller currently holds. Smart consumers only. Releasing an already-released / expired / foreign lease returns `404 not_found`. |
| **I — Skills** *(open to every token)* | `skill_list` | full (MVP) | List the **bundled** skills available to the caller (`core`, `core-globalmemory`, `smart-consumer`, `standard-conversational`, `smart-codebase` — all `status: implemented`) embedded via `rust-embed` from `crates/mwe-core/skills/` (each skill's `version` is read from its own frontmatter, the SSOT — don't hardcode it here). Returns `{ name, version, description, depends_on, etag, source: bundled }` per entry. `consumer_class` filter accepted but currently unused (reserved for future class-aware filtering). Only bundled skills are listed; custom smart-family skills are not supported. |
| | `skill_fetch` | full (MVP) | Fetch the full markdown body of a single bundled skill. Returns `{ name, version, description, content, etag, source }`. The `etag` matches `skill_list` so consumers can short-circuit on cache hit. `version` pin accepted but currently unused (reserved for the future `/skills/<name>/<version>.md` HTTP plumbing). |
| **K — Smart-consumer bootstrap & contextual recall** *(smart only)* | `smart_bootstrap` | full | Surface the caller's smart-wiki landscape at session start. The smart consumer calls it itself at session start — the model-driven path, nudged by the token-less `SessionStart` hook in the Claude Code bundle (served at `/connect/hooks/claude-code.json`) and the `smart-consumer` skill. Called with `{}`, the server returns every smart wiki (those whose `_meta.md` `smart:` flag is `true`) the caller owns, each row carrying `wiki_id` / `wiki_type` / `title` / `slug` / `project_id` / `briefing_counts` (per-kind buckets) / `recent_briefing` (pending items capped via `briefing_limit_per_wiki ∈ [1, 50]`, default 5) / `last_op_log_id` / `last_op_log_ts` / `matches_project_hint`. Sort order: hint-match first, last-op-log activity next, `wiki_id` alphabetical. Hint matching: case-insensitive substring against `_meta.md.extra.project_id` + slug + title. Smart-only (`403 requires_consumer_class_smart`). Read-only annotation. |
| | `recall_core_global` | full | Canonical transversal recall wrapper — the call a smart consumer makes for transversal recall (model-driven, or from a host's recall hook): wraps `wiki_search` with the caller-owned + companion-excluded filter from the bundled skill `core-globalmemory.md` (it sets `scope.smart = false`). Input: `query` (required) + optional `limit ∈ [1, 20]` (default 8). The server clamps the limit, trims the query (empty → `400 invalid_input`), applies the filter, and returns `{ query, filter_applied: { owner_user, excluded_wiki_types }, hits[] }`. `excluded_wiki_types` echoes the smart-wiki stems the server pre-filtered so the caller's audit trail is unambiguous. Smart-only. Read-only. |
| **L — Forget** *(authority-routed)* | `wiki_forget`, `wiki_forget_bulk` | full | Forget one fact by id on behalf of the connected sender, **routed by authority**. Loads the fact (`fact_index::find_by_id`; unknown → `404 not_found`, already tombstoned → idempotent `{ outcome: "already_forgotten" }`). If the caller may delete directly (`acl::can_delete` — the fact's author, or an admin) it is tombstoned now (`mark_forgotten`, reason `consumer_forget`) → `{ outcome: "forgotten", fact_id }`. Otherwise a non-sender **owner** (subject / owning-group member, `acl::sender_owns`) is **not** opened into a vote here — forgetting a fact you did not author needs an audience vote, and a vote is opened **only from the dashboard**, never started in the background by the agent (maintainer 2026-06-29): the response is `{ outcome: "request_from_dashboard", fact_id, detail }`, steering the user to open the request there. An unrelated caller (not author, owner, or owning-group member) → `403 sender_unauthorized`. Marked **destructive** in its annotations. **Opening the request and casting the vote are both dashboard-only — no consumer path starts a vote.** `wiki_forget_bulk` is the **bulk self-delete**: it tombstones every still-active fact the caller authored (`fact_index::mark_forgotten_by_sender`, reason `consumer_forget_bulk`), narrowed by `scope` — `all` (every wiki), `wiki` (one `wiki_id`), or `page` (one `wiki_id` + `page`). Only the caller's own facts (`sender == ` the JWT principal) are ever touched and no vote opens (a self-delete is always allowed). Also **destructive**. |

## Where the atomic operations live

Anything an LLM-driven `wiki_ingest_message` turn or the dashboard does
under the hood — capture a fact, supersede an old one, recall topical
context, forge a new wiki type — is an `_internal.*` API of `mwe-core`,
**not** an MCP tool. The conventional list:

- `_internal.wiki_capture`, `_internal.wiki_supersede`,
  `_internal.wiki_forget`, `_internal.wiki_link`,
  `_internal.wiki_attach_file`, `_internal.wiki_write_page`
- `_internal.wiki_recall`, `_internal.wiki_facts_for`,
  `_internal.wiki_recall_topic`, `_internal.resume_memory`
- `_internal.wiki_list_pages`, `_internal.wiki_get_meta`,
  `_internal.wiki_catalog_list`
- `_internal.wiki_forge`, `_internal.wiki_promote`
- `_internal.users_resolve`, `_internal.groups_describe`,
  `_internal.enrollment_reload`
- `_internal.media_search`, `_internal.media_resolve`,
  `_internal.media_link_block`, `_internal.media_catalog_list`
- `_internal.archive_search`, `_internal.archive_propose_list`,
  `_internal.archive_approve`, `_internal.archive_reject`,
  `_internal.archive_restore`, `_internal.archive_query_target`
- `_internal.dream_trigger`, `_internal.wiki_change_scope`,
  `_internal.structure_proposal_emit`

The full per-tool reference (parameters, returns, errors, side
effects) for the public surface lives in
[`tool-reference.md`](./tool-reference.md). The `_internal.*` roster is
not a stable API — it is `mwe-core`'s own seam and changes without
notice; treat the list above as illustrative, not contractual.

## Transport

- **Streamable HTTP** (the only transport — mwe-mcp is HTTP-only)
  — Axum endpoint exposing the rmcp tool set, plus the dashboard mounted at `/dashboard/*` and
  the skill catalog mounted at `/skills/*` (`GET /skills` for
  JSON metadata, `GET /skills/<name>.md` for raw markdown body with
  `ETag` + `If-None-Match` support). The skill catalog is
  **bundled-only**.

## Auth

Every tool call carries a **JWT** in the MCP request envelope. The
payload includes `sender_id`, `device_label`, `rate_limit_id`,
optional `isAdmin`, optional `consumer_id`, optional `consumer_class`
(`standard` default, `smart` for self-managed agents — see
`jwt-and-session-model.md`
§Consumer class), plus standard claims (`exp`, `iat`, `jti`). Tokens
are issued by `mwe-mcp token-issue …` (or `--class smart`) or via the
`/dashboard/tokens/issue` form (its Smart/Standard consumer-class radio);
verifiable revocations live in `token_blacklist`.

The `consumer_class=smart` claim is what unlocks family H — every
`wiki_admin_*` tool refuses standard tokens with
`403 requires_consumer_class_smart` (except `wiki_admin_notify`, which
is gated by the `consumer_class × wiki_family` matrix above
rather than a flat class check). See
`jwt-and-session-model.md`
for the claim set and the dispatch surface in
`mcp-dispatcher.md`.

**Honest partial — `rate_limit_id`.** The `rate_limit_id` claim is
parsed off the JWT, stored, and echoed in the audit log + the
dashboard token view, but **no per-family rate-limit enforcement reads
it today** — there is no bucketing call site keyed on
`rate_limit_id`. The one rate limit that *is* enforced is the
per-wiki **50 notify/wiki/hour** cap inside
`mwe-core::briefing::notify_append`, and that cap is keyed on the
target `wiki_id`, not on `rate_limit_id`. The per-family limiter is
not yet implemented; treat `rate_limit_id` as a forward-compatibility
claim.

## For agent authors

If you are wiring an LLM agent (Claude, Cursor, custom) to mwe-mcp,
read [`../../AGENT_INSTRUCTIONS.md`](../../AGENT_INSTRUCTIONS.md). It
documents the decision tree for which tools to call in
each conversation turn and the anti-patterns to avoid (the
`_internal.*` trap, the "split structural intent into atomic writes"
trap, the stale `wiki_read` cache trap).
