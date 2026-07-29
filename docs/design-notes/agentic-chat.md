---
title: Dashboard agentic chat — registry, loop, tool wiring
area: design-notes
status: implemented
last_review: "2026-07-04"
---

# Dashboard agentic chat

The dashboard's chat panel is **not** a Q&A surface — it is the
operator's *operative tool on their own memory* (the framing: "the
chat is a tool, the user is the agent"; see
[`memory-model.md`](../concepts/memory-model.md) and
[`identity-and-acl.md`](../concepts/identity-and-acl.md)). The chat composes
`_internal.*` calls directly (no MCP, no JWT, no consumer-delegation
gymnastics) because it lives in-process: a whitelisted subset of
`mwe-core`'s internal APIs is exposed to a function-calling LLM loop
running against the operator-chat backend (the `operator_chat` slot,
falling back to `hub_writer`). ACLs are enforced row-level via
the connected user's `SenderContext`, the same way an external
consumer agent's calls would be ACL-filtered.

This page documents the registry shape, the loop architecture, the
read/write split, and the dependency on the `hub_writer` slot.

## Where it lives

- **Registry**:
  [`crates/mwe-dashboard/src/agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs)
  — the `AgenticTool` enum (the variant set is the SSOT — see the
  read/write table below), `tool_descriptors()` returning
  JSON Schema descriptors per tool, `AgenticContext` value type,
  `dispatch(name, args, ctx)` switch table.
- **HTTP entrypoint**:
  [`crates/mwe-dashboard/src/routes/chat.rs`](../../crates/mwe-dashboard/src/routes/chat.rs)
  exposes `POST /dashboard/chat/agentic` and the
  `agentic_submission` orchestrator that drives the loop. The
  conventional non-agentic surface (`process_submission`) lives in
  the same file and routes through `wiki_ingest_message` for
  consumer-style turns.
- **Backbone primitives**: `LlmBackend::chat` +
  `ChatRequest`/`ChatResponse`/`Tool`/`ToolCall` in
  [`mwe-core::llm`](../../crates/mwe-core/src/llm.rs). The Ollama
  backend wires `POST /api/chat` with `think: false`; see
  [`llm-functions.md`](llm-functions.md) for the slot wiring.

## Loop architecture

```text
agentic_submission(state, user, text, history):
   ctx := AgenticContext {
       pool, tree, embedder,
       sender_ctx: SenderContext::user(user.sender_id),
       backend := MemoryHandles::backend_for_chat()  // operator_chat, else hub_writer
   }
   messages := [system_prompt, ...history (user/assistant pairs), user_message]
   trace := []
   for iter in 0..MAX_AGENTIC_ITERATIONS:
       response := backend.chat(messages, tools)
       if response.tool_calls.is_empty():
           return AgenticTurn { message: response.content, trace }
       messages.push(assistant_turn(response.content, response.tool_calls))
       for call in response.tool_calls:
           result := agentic::dispatch(call.name, call.arguments, &ctx)
           trace.push(...)
           messages.push(tool_result(call.id, result))
   return AgenticTurn { budget_exhausted: true, trace }
```

`MAX_AGENTIC_ITERATIONS = 8` is the hard ceiling on loop iterations.
Past this the dashboard stops the conversation and surfaces a "tool
budget exhausted" message so the LLM cannot loop indefinitely on bad
inputs.

### Cross-turn confirmation — the replayed history window

The server keeps **no** conversation state, but each submission replays
a bounded recent `{user, assistant}` window so the propose → confirm →
act handshake works across turns. Without it the loop was effectively
stateless per submission, which **contradicted** the prompt's write-tool
rule ("confirm before acting"): the model proposed a delete in turn 1,
but a bare "sì" in turn 2 arrived with no trace of that proposal, so it
could never be carried out. The window closes that gap.

- `chat.js` sends the tail of its `localStorage` scrollback as a
  `history` form field (`recentTurns`, agentic turns with a non-empty
  final reply only — primer / error turns carry no assistant text).
- `routes::chat::parse_chat_history` parses + clamps it: malformed JSON
  degrades to empty (never fails the turn), empty-sided turns are
  dropped, the most recent `MAX_HISTORY_TURNS` survive in order, each
  field clamped to `MAX_HISTORY_CHARS`. The window is tight on purpose —
  the local workhorse degrades on a long replayed context.
- `agentic_submission` prepends the window as alternating
  `ChatMessage::user` / `ChatMessage::assistant` between the system
  prompt and the new user message.

Fact continuity beyond the window still rides `wiki_recall` +
autocapture, not the replay — the window exists for the operational
handshake, not as a memory.

> The loop **replays** the assistant turn (`messages.push(assistant_turn(...))`)
> on every subsequent iteration. On a **Gemini** slot each replayed
> `functionCall` must carry back the opaque `thoughtSignature` Gemini
> attached to it, or the next request 400s; the backend round-trips it
> transparently on `ToolCall::thought_signature` — see
> [`llm-functions.md` §4.1](llm-functions.md). The loop code is
> provider-agnostic and does not touch the field. Each tool call's `(name, arguments, result, is_error)` is
captured in the `trace` and rendered transparently in the panel — the
operator sees exactly what the model did before getting the textual
reply.

## Rendering the reply — Markdown to safe HTML

The model's final reply is Markdown, and the panel renders it so it
reads like a normal chat instead of literal `**` and backticks. The
`AgenticTurn` carries the reply twice:

- `final_message` — the **raw** text. This is what the chat panel
  replays into the next turn's `{user, assistant}` confirmation window,
  so it must stay free of HTML.
- `final_message_html` — the same text rendered to **safe HTML** by
  [`md_render::render`](../../crates/mwe-dashboard/src/md_render.rs)
  (the wiki preview's `pulldown-cmark` path: paragraphs / lists / bold /
  inline code, with raw source HTML stripped — XSS-safe by
  construction, since the reply is untrusted LLM output). Empty when
  `final_message` is empty (budget-exhausted turns).

`chat.js` shows `final_message_html` (via `renderHtmlBotBubble`,
`innerHTML`) and keeps `final_message` for the replay; entries persisted
in `localStorage` before HTML rendering existed carry no
`final_message_html` and fall back to the raw-text bubble. The two
form-to-chat primer bridges (`/dashboard/proposals` open-in-chat,
`/dashboard/facts` edit) serialise both fields into the
`window.__mweChatPrimer` payload so primed turns render identically.
The `.chat-panel-bot` markdown styling lives in `tailwind/app.css`
(recompile `assets/tailwind.css` after editing — see
[`build-run.md`](../development/build-run.md)).

## Read vs write tools

The `AgenticTool` variant set is the SSOT (read it off
[`agentic.rs`](../../crates/mwe-dashboard/src/agentic.rs); the live
descriptor list is `tool_descriptors()`). The split is explicit because
the system prompt teaches the model **a different turn shape** for the
write tools (confirm with the user in the current turn before
calling).

One blanket guard spans the wiki-management write verbs: **smart wikis
are refused** (`ensure_standard_wiki`, keying on the wiki's `smart`
meta flag — smart section rows carry `sender_id = NULL` and owner = the
scope principal, so no sender/owner gate would catch them). Smart wikis
are the consumer's, governed at the wiki level; the operator's
touchpoint for one is its briefing (see
[smart-wikis.md](smart-wikis.md)).

| Variant | Read/write | Purpose |
|---|---|---|
| `WikiRecall` | read | Semantic recall over `fact_index`, ACL-filtered. Default `top_k = 5` for chat-panel readability. |
| `WikiListPages` | read | List the active pages of a single wiki. |
| `WikiGetMeta` | read | Return the `_meta.md` view of a single wiki (title, type, slug, `scope`, parent). |
| `WikiGetFact` | read | Look up ONE fact by exact `fact_id` (ACL-projected via [`acl::can_read`](../concepts/identity-and-acl.md), admin-bypass), returning body + status (active / superseded / tombstoned) or `{"found": false}`. The verify-before-act tool: `WikiFactsFor` has no `fact_id` filter, so without it the model improvises (e.g. `wiki_facts_for(limit=1)`) and acts on the wrong fact. |
| `StructureProposalList` | read | List pending structure proposals. |
| `StructureProposalGet` | read | Fetch the full row of a single proposal (context, questions, status). |
| `StructureProposalApply` | **write** | Apply a pending structure proposal with user-supplied `answers`. First write tool exposed. |
| `WikiFactsFor` | read | Filtered listing of facts the user can see — by wiki, topic, `fact_type`, date range. |
| `WikiForget` | **write** | Tombstone a single fact (DB tombstone — the marker stays on disk for `wiki_lint` to flag later). **Sender-direct**: only the fact's author (its `sender`) or an admin may forget it (`acl::can_delete`); a non-sender owner is refused and pointed at `WikiRequestForget` (the request → vote path). A smart-wiki target is refused (smart section rows are not fact-governed — the consumer's next push would undo the tombstone). |
| `WikiSupersede` | **write** | Replace an existing fact with a corrected body in-place. Inherits owner / ACL / `fact_type` / topics from the targeted fact. **Owner-direct** (`acl::sender_owns` ‖ admin): editing content is an *update* — the subject's act, the same owner axis as the ingest supersede guard (`SupersedeCrossOwner`) and `acl_change`. Only the destructive `WikiForget` keys on `sender`. A smart-wiki target is refused (a supersede would write a marker-wrapped fact into the consumer's plain-markdown page). |
| `WikiChangeScope` | **write** | Re-parent a wiki (and its subtree) — renames the directory, rebases each fact's `source_path`; `wiki_id` stays stable so cross-links keep resolving. A move re-files **structure only**: it never rewrites an owner and never changes who can read a fact (ACL lives on the fact, independent of the wiki's place in the tree). A smart source wiki is refused — its wiki-level read audience derives from its position in the tree, so a re-parent would change effective read access on the next reindex/push; the core [`scope::wiki_change_scope`](../../crates/mwe-core/src/scope.rs) primitive carries the same guard. See [`scope-change.md`](scope-change.md). |
| `WikiMoveFact` | **write** | Move ONE fact on the operator's instruction — to another page of the same wiki (`promote::apply_paragraph_to_file_direct`) or into another wiki (`promote::apply_fact_refile_direct`, landing on the dest `index.md`). Same act-first engine the REM cross-wiki refile sweep + the comment-apply `move` op use. **Admin-only** (`enforce_move_admin`): a move re-categorises — it neither destroys the fact nor changes who can read it — so it is the operator's / REM's structure act, not the owner's. Smart source/dest refused. |
| `WikiDeletePage` | **write** | Delete a page, governed by the per-fragment **sender**: the deleter's own facts are tombstoned; a foreign-authored fact evacuates intact to its sender's home wiki when one exists, falling back to its owner's — a fact whose sender and owner both lack a home wiki is tombstoned ([`page::decide`](../../crates/mwe-core/src/page.rs)) — all wrapped in one revertible [`bundle`](proposal-apply-engine.md#bundle-handler). **Admin-only** — deleting structure (a page or wiki) is the operator's act; smart wikis refused. There is **no member vote** over a deletion (the admin/deleter's dashboard undo is the only post-deletion lever). The disposition is **move** (the sender-keyed default) or **tombstone all** (`delete_all_facts`, informed confirmation required); `dissolve` is whole-wiki only (a page cannot be dissolved — that is what the REM split/merge passes already do page by page) and the verb refuses it. **The verb disposes of the page's facts, not of the page file**: the husk is kept on purpose so the `bundle` has something to be reverted into, and the [nightly husk-GC](rem-cycle.md) drops it on the first cycle after every row is tombstoned past `proposals::REVERT_WINDOW` (a floor — that sweep is capped per cycle). So the report carries `page_file_retained_days`, and the prompt requires the answer to state both halves: recall stops now, the page stays in the explorer until the undo window runs out. Without that datum the model summarises a tombstone count as *"page deleted"* while the operator is looking at the page — the memory telling them something the screen contradicts. |
| `WikiRequestForget` | **write** | Open a **fact-forget request** for ONE fact the signed-in user does **not** author — the non-sender owner's path (the `fact_forget` [proposal kind](proposal-apply-engine.md#fact-forget-handler)). Caller must be the fact's `owner` (subject) or an owning-group member (or admin); a **sender** is refused and pointed at `WikiForget`. **Propose-first**: the fact stays active while its [`audience`](../../crates/mwe-core/src/acl.rs) votes (`StructureProposalVote`) — a NO-majority blocks it, silence forgets it; a sole-reader request forgets immediately. A smart-wiki target is refused (forget votes are per-fact governance; smart governance is wiki-level). [`votes::open_forget_request`](../../crates/mwe-core/src/votes.rs). |
| `StructureProposalRevert` | **write** | Undo a previously-applied proposal — the inverse of `StructureProposalApply`. Reuses the [`revert_proposal`](proposal-apply-engine.md) chassis. Headline: undo an applied `wiki_promote` — e.g. a group of pages the REM regrouped into a sub-wiki, refused when that wiki has since grown a page of its own or a fact landed on one of the carried pages (`revert_pages_to_subwiki`'s in-use guards). Also the deleter's / admin's undo of a page-deletion `bundle`. |
| `StructureProposalConfirm` | **write** | Confirm a sweep auto-applied proposal so it sticks — the counterpart of `StructureProposalRevert` on the `applied_pending_confirm → applied` edge. Thin shim over the [`confirm_proposal`](proposal-apply-engine.md) chassis (which gates by recipient/admin internally, so no separate pre-check); mints the `revert_token` + opens the 7-day window. |
| `StructureProposalVote` | **write** | Cast the signed-in member's vote (`yes`/`no`) on a pending **fact-forget request**. Votes **as** `sender_ctx.sender_id` (no voter argument → no impersonation); [`votes::cast_vote`](../../crates/mwe-core/src/votes.rs) refuses anyone outside the eligible set / the requester / a re-vote, tallies, and — propose-first — **blocks** the request on a NO majority **over the eligible set** (the fact's audience minus the requester; the fact stays), **applies** the forget on an all-voted quorum with no NO majority. The pull-only `pending_votes` recall reminder routes a member here — the proposal read tools are recipient-scoped to the requester, so a voter cannot browse the request from the panel. |

### Revert in the chat

`structure_proposal_revert` lets the chat *undo*, not just
apply/originate. It writes
**no** new revert logic — `dispatch_proposal_revert` is a thin shim over
the existing [`proposals::revert_proposal`](proposal-apply-engine.md)
chassis, replicating the form route's status-driven `RevertAuth`
selection exactly:

- A `SELECT status, revert_token, recipient_id` precedes the call (the
  same shape `proposal_needs_llm_for_apply` uses). Unknown id →
  `InvalidArguments`.
- **Recipient gate (0032):** for a non-admin operator, the dispatcher
  refuses (`InvalidArguments`) unless
  [`recipient_can_act`](../../crates/mwe-core/src/proposals.rs) passes —
  so revert is scoped to the addressee exactly like
  `structure_proposal_list` / `_get`, never a bypass. Admins always pass.
- `status == "applied"` → `RevertAuth::Token` (token read server-side;
  an applied row with no token is a defensive `InternalFailure`);
  `status == "applied_pending_confirm"` → `RevertAuth::Caller { sender,
  is_admin }`; anything else → `InvalidArguments` ("not in a revertable
  status").
- The chat's `hub_writer` is threaded through for forward-compat, but
  the revert kind handlers ignore the LLM (`dispatch_revert_kind`'s
  `_llm` is unused today).
- `RevertError` mapping: the user-actionable / refusal variants (a
  per-kind guard refusing via `HandlerData`) map to `InvalidArguments` so
  the model relays them as an ordinary refusal; the two infra variants
  (`Db`, `HandlerIo`) map to `InternalFailure`.

### Confirm in the chat

`structure_proposal_confirm` is the **counterpart of
`structure_proposal_revert`** on the `applied_pending_confirm → applied`
edge. It is where the operator *confirms* a proposal the nightly
auto-apply sweep has landed on their behalf, in the chat — the single
operational surface (see [`dashboard.md`](dashboard.md)).

Like the revert dispatcher it adds **no** new chassis logic —
`dispatch_proposal_confirm` is a thin shim over
[`proposals::confirm_proposal`](proposal-apply-engine.md). Unlike revert,
it does **not** pre-`SELECT` the row or pre-check the recipient: the
chassis already validates the status is `applied_pending_confirm`, checks
the confirm window, and gates by recipient/admin internally
(`ConfirmError::NotAuthorized` via
[`recipient_can_act`](../../crates/mwe-core/src/proposals.rs)). The
dispatcher just calls it and maps the error:

- success → `{ "confirmed": { proposal_id, kind, revert_deadline,
  revert_token } }` (the confirm mints the token + opens the 7-day revert
  window, so the chat can tell the user undo is still available).
- `ConfirmError` → `Db` maps to `InternalFailure`; the user-actionable
  variants (`NotFound`, `NotPendingConfirm`, `ConfirmWindowExpired`,
  `NotAuthorized`) map to `InvalidArguments` so the model relays them as
  an ordinary refusal.

`structure_proposal_list` accepts the `applied_pending_confirm` status
filter so the chat can find the candidates the sweep produced before
asking the user to confirm-or-revert.

The revert dispatches to the per-kind inverse via the
[`proposal-apply-engine.md`](proposal-apply-engine.md) chassis
(auto-promotion, dedup merge), each guarded by its own handler.

## System prompt — what teaches the LLM the contract

The `agentic-chat-panel` system prompt is the only thing standing
between the model and the write tools. It is **loaded via the hybrid
runtime loader** rather than baked as a `const &str` in
`routes::chat`: the bundled default ships at
[`crates/mwe-dashboard/prompts/agentic-chat-panel.md`](../../crates/mwe-dashboard/prompts/agentic-chat-panel.md)
(embedded via `include_str!` as `BUNDLED_AGENTIC_PROMPT_MD`) and an
override at `<workdir>/prompts/agentic-chat-panel.md` wins when
present; `agentic_submission` calls
`prompts::render("agentic-chat-panel", workdir, BUNDLED_AGENTIC_PROMPT_MD, &[("locale", …)])`
to resolve it per-request (no hot-reload cache — the per-call read is
microseconds against the LLM call). The `{locale}` placeholder in the
prompt's `LANGUAGE` section is substituted with the directive
[`mwe_core::locale::render_language_directive`](../../crates/mwe-core/src/locale.rs)
builds from `enrollment_users.locale` of the signed-in user; a NULL
column falls back to the "mirror the user's message" clause so
deployments without populated locales do not regress. The prompt is
**in English**.

It says, in essence:

- The chat panel is operative on the user's own memory — the model
  acts as a tool, not a consultant.
- Read tools may be called without confirmation; **write tools
  require an explicit user confirmation in the same turn**. If the
  user has not confirmed, the model must propose what it would do
  and stop, not execute.
- The recall → show candidate → propose corrected body → confirm →
  supersede (or `wiki_forget` for plain deletion) flow is the
  canonical pattern for the correction and batch tools.
- **Every** wired write tool carries its own mandatory flow block in
  the prompt — including the high-stakes ones with no code confirmation
  gate: `wiki_delete_page` (list-then-warn, foreign facts evacuate
  rather than erase, `delete_all_facts` needs a separate informed yes),
  `wiki_request_forget` (the non-author's forget request, which opens a
  member vote rather than deleting outright) and its counterpart
  `structure_proposal_vote` (cast the signed-in member's final vote on
  that request). A regression test
  (`every_wired_tool_is_named_in_the_bundled_prompt`) fails if a wired
  tool is left untaught.
- Tool budget is finite (`MAX_AGENTIC_ITERATIONS`); plan in
  small steps and surface progress to the user.
- **Presentation:** the reply is Markdown (the panel renders it — see
  "Rendering the reply" above), so the model uses lists / short
  paragraphs when it enumerates things, and it **never prints raw
  UUIDs** (proposals, facts, wikis) to the operator — it refers to each
  item by its content + an ordinal, keeping ids internal for its own
  tool calls. The prompt also pins the `wiki_promote` vocabulary
  (**paragraph→page** vs **page→sub-wiki**) so a batch of promotions is
  never mislabelled as "promoting paragraphs to a wiki".

There is **no** post-hoc whitelist veto. The safety surface is the
operator's authorship of the prompt: if the operator does not want
the chat to write, they restrict to read-only tools (the next
section). This matches the framing: "the chat is a tool, the
user is the agent". Operators can edit the prompt at runtime via
the admin-only dashboard editor `/dashboard/prompts` with
atomic save + `.bak` backup + reset-to-bundled + drift banner
against `default_version_at_bootstrap`.

## Dependency on the `operator_chat` slot (with `hub_writer` fallback)

The loop runs against a single `LlmBackend`, resolved at
`agentic_submission` entry by `MemoryHandles::backend_for_chat()`: it
prefers the dedicated `llm.operator_chat` slot and falls back to
`llm.hub_writer` when `operator_chat` is unconfigured. The chat is a
distinct workload from the `regenerate_index` sub-job that also rides
`hub_writer` — interactive, multi-step function-calling, faithful
fact-id handling — so an operator can point it at a **strong**
tool-calling model without inflating the index-regen cost. The fallback
keeps deployments that never set `operator_chat` working exactly as
before, with no new YAML key (only `SlotMissing` on the dedicated slot
falls through; a `BuildFailed` is surfaced, not masked). The per-slot
default knobs follow the same fallback via `chat_defaults()`. No
proposal kind needs an LLM at apply time today, so the dispatcher
applies proposals without threading a backend through `AgenticContext`.

A missing chat backend — **both** `llm.operator_chat` and the
`llm.hub_writer` fallback unconfigured — is a **hard refuse**: the chat
surfaces a validation error rather than degrading to a non-agentic turn.
The same treatment applies to the `llm.ingest` slot for the
consumer-side `process_submission` route.

The non-agentic `process_submission` route (which `routes::welcome`
uses to drive the first-login wizard primer) goes through
`wiki_ingest_message` instead, so it lives behind the `llm.ingest`
slot — orthogonal to this loop, even though both surfaces share the
same `/dashboard/chat` page.

## Tool dispatch — `AgenticToolError`

The dispatch switch returns a JSON-stringified result for the happy
path or one of three error variants:

- `UnknownTool(name)` — the model invented a tool name. The error is
  serialised back to the model as a `Role::Tool` content with the
  full enum of available names, so the model can self-correct on the
  next iteration.
- `InvalidArguments { tool, detail }` — JSON Schema accepted the
  arguments but a downstream semantic check rejected them (e.g.
  `wiki_id` not found, `fact_id` already tombstoned). Same recovery
  path: feed back to the model.
- `InternalFailure { tool, detail }` — irrecoverable (sql error,
  filesystem permission). The chat surfaces this to the operator
  via the trace panel; the loop continues so the model can choose to
  abandon gracefully.

Errors are **never** raised up the call stack. The agentic loop hands
them back to the LLM so the model can retry with corrected arguments,
ask the user, or give up. The chat panel only sees them as ordinary
tool results.

## `window.__mweChatPrimer` — server-side primer injection

For affordances like "Apri in chat" on `/dashboard/proposals` and the
welcome wizard submission, the server-side handler runs the agentic
loop with a canned primer first and then injects the resulting turn
into the chat panel's `localStorage` via the `window.__mweChatPrimer`
hook. On next page load, the chat panel reads the primer, hydrates
the conversation, and clears the hook. This is how a non-chat action
flows into the agentic conversation without losing the user's place.

The **form-to-chat bridge** extends this hook to a
*write* affordance: the "Modifica" deep-link on `/dashboard/facts`
opens an edit form (`GET /dashboard/facts/:fact_id/edit`) whose
**body / topics / `fact_type` supersede** (`POST /edit/submit`) runs a
`match`-based mapper (`compose_edit_message` with three macro-cases —
metadata-only sentence, body-only with fenced block, mixed metadata +
body) to compose a textual instruction, then runs
`chat::agentic_submission` with that instruction as the user turn and
primes the chat panel exactly the same way the proposals "Apri in chat"
path does. The write itself still goes through the chat's HARD RULE of
explicit confirmation — the form is just a better source than free prose
for the *delta*.

**ACL and validity do NOT ride this bridge.** They are structured
engine-direct fact actions (`POST /facts/:id/acl` /
`POST /facts/:id/validity` — owner-or-admin, standard-wikis only,
born-applied + revertible; see
[`dashboard-memory-mvp.md`](dashboard-memory-mvp.md)), because no chat
tool applied them deterministically. The form-to-chat bridge also does
not cover type CRUD: the memory model has no `wiki_type` registry to
CRUD against.

## Current gaps

These capabilities are not yet present in this module (planned — see
the roadmap):

- **`wiki_forge`** as a concrete `_internal.*` (instantiate a new
  wiki conversationally) is not yet exposed, so
  the natural-language flow "vorrei una wiki per i libri" does not yet
  close end-to-end with a real wiki row.
- **Multi-hop link resolution** is not wired into `WikiRecall` (or
  exposed as its own tool) — `wiki_multi_hop_facts` exists in
  `mwe-core::recall` but is not reachable from chat.
- **Batch fact move between wikis** is out of scope today (awaiting a
  concrete use case from the maintainer).
