---
name: standard-conversational
version: 1.3.0
description: "Default conversational pattern for standard consumers (openclaw, hermes, nanoclaw): wiki_ingest_message passthrough, recent_messages window, disambiguation, locale plumbing, pending_attention nudges, events_poll cadence, structural notices + undo routing, on-the-fly date corrections + sharing changes on the owner's own facts, no wiki_admin_* writes."
depends_on: ["core"]
applies_to:
  consumer_class: standard
status: implemented
---

# mwe-mcp / standard-conversational skill

This skill defines the per-turn conversation loop for **standard
consumers** — agents that do **not** bring their own subscription LLM
budget and route every user turn through mwe-mcp's server-side
`ingest` LLM slot. Concrete examples shipped today: openclaw (Telegram
bridge), hermes (CLI), nanoclaw (small embedded surface). The same
pattern applies to any future consumer that wants mwe-mcp to do the
classification + recall + capture work for them.

## When this skill applies

Loaded by the dispatcher when your JWT carries
`consumer_class: standard` (or omits the claim, which defaults to
standard). If your token has `consumer_class: smart`, you should be
loading `core-globalmemory` or `smart-consumer` instead — see `core`
for the dispatcher.

## The cardinal rule — passthrough

**For every conversational turn from the user, call
`wiki_ingest_message` and nothing else.** mwe-mcp's server-side
`ingest` slot does:

- Intent classification (capture vs recall vs structural vs skip).
- Recall (vector + full-text + multi-hop) with ACL filtering.
- Capture / supersede / forget routing.
- **Operation-path edits on the owner's own stored facts** — a date
  correction ("the milk expires on the 20th, not the 25th") and a sharing change
  ("make this one visible to everyone", "share it with the family group") are
  recognized in the same `wiki_ingest_message` turn and applied act-first,
  revertible from the dashboard (the owner gate is enforced server-side;
  standard memory wikis only). You still just pass the raw message through.
- Structural proposal emission (questionnaire when a new wiki or
  type should emerge).
- Topic extraction (server-internal, never exposed).

You — the consumer — are a **thin passthrough**: pass the raw user
message in, receive a context snippet + a reply seed back, weave them
into your natural-language reply. You do **not** classify intent
client-side. You do **not** decide "this is structural, I'll route
differently". The server does all of that, including for greetings
("hi, how are you") and acks ("yes, that's right"), which it classifies
as `skip` with a canned reply seed.

```
User: "have I lost 1 kg since Thursday?"
Agent → wiki_ingest_message(text="have I lost 1 kg since Thursday?",
                            recent_messages=[<last ~6-10 turns>])
Server → { intent_classified: "recall",
           context_snippet: "Frodo weighed 72 kg on 10 May.
                             Today is 18 May, Frodo recorded
                             71 kg yesterday, 17 May.",
           suggested_seed:  "You recorded 71 kg yesterday, down
                             from 72 kg 8 days ago.",
           pending_attention: null, ... }
Agent → injects context_snippet into its own system prompt for this
        turn, rephrases suggested_seed in product voice
Agent → "Yes, you are at 71 kg — a kilo down since Thursday, just as you remembered."
```

The only call you make **outside** `wiki_ingest_message` during a
normal turn is to surface a URL the server tells you about (see
§"Events and dashboard URLs" below) and the proactive nudge from the
`pending_attention` block.

## The per-turn loop in detail

For each user message:

1. **Maintain a rolling window** of the last ~6–10 conversation turns
   in `recent_messages`. The server caps it at 6 internally and uses
   it for **coreference** ("the dentist from yesterday" → which dentist?),
   not for recall. The window is *not* persisted server-side — your
   buffer, your responsibility.
2. **Call `wiki_ingest_message`** with the raw text + the window. You
   may pass `context_hint` (`conversation` default,
   `dashboard_command`, `import`) and `metadata.locale` (see
   "Locale plumbing").
3. **Inject `context_snippet`** (when present) into your own LLM's
   system prompt for this turn. Preformatted prose of recalled MEMORY; do
   not parse, do not re-summarize.
3b. **Inject `rules`** (when present) too, as standing BEHAVIOUR
   directives to APPLY this turn (how to converse / operate with this
   user) — not material to relay. Keep it distinct from
   `context_snippet`: the latter is what you remember, this is how you
   must behave. Privacy is never here (it is ACL-enforced memory-side).
4. **Use `suggested_seed`** as a starting point for your reply.
   Rephrase to match your product voice; do not change the substance.
5. **Check `pending_attention`** — see "The pending_attention block".
6. **Check `needs_disambig`** — see "Disambiguation".
7. **Compose your reply** and send it back through your channel.

## Wire shape

Verified against
`crates/mwe-mcp-server/src/mcp/tools.rs::call_wiki_ingest_message`.

**Input**:

```typescript
{
  text: string;                          // raw user message body
  sender_id?: string;                    // optional; if set, must match token claim
  recent_messages?: Array<{
    role: "user" | "assistant";
    text: string;
    timestamp?: string;                  // ISO 8601, optional
  }>;
  context_hint?: "conversation"          // default
                | "dashboard_command"    // user typing in dashboard chat
                | "import";              // bulk ingestion
  metadata?: {
    disambig_choice?: string;            // see "Disambiguation"
    locale?: string;                     // BCP-47 primary subtag, see "Locale plumbing"
    [key: string]: any;
  };
}
```

**Output**:

```typescript
{
  intent_classified: "capture" | "recall" | "structural" | "skip";
  context_snippet?: string;              // recalled MEMORY, preformatted
  rules?: string;                        // standing BEHAVIOUR directives to APPLY (not relay)
  suggested_seed?: string;               // reply draft
  capture_id?: string;                   // audit-only; do not echo to user
  needs_disambig: boolean;
  disambig_candidates: Array<{
    candidate_id: string;
    description: string;
  }>;
  llm_used: string;                      // diagnostic
  took_ms: number;
  pending_attention?: {                  // present only when count > 0
    pending_count: number;
    applied_pending_confirm_count: number;
    dashboard_path: string;              // typically "/dashboard/proposals"
    note: string;                        // opaque metadata; don't branch on value
  };
}
```

## Disambiguation

When the server isn't sure which fact the user is referring to
("the dentist" with multiple dentist entries on file), the response
has `needs_disambig: true` and a populated `disambig_candidates`.
Surface the choices to the user. When they pick one, call
`wiki_ingest_message` **again** with the same text but with
`metadata.disambig_choice = "<candidate_id>"`. The server picks up
the chosen candidate and finalizes the turn.

## Locale plumbing

The server's `ingest` and `agentic-chat-panel` prompts include a
`LANGUAGE:` directive. The resolve chain, in order:

1. `metadata.locale` if your consumer passes one explicitly (BCP-47
   primary subtag, e.g. `"it"`, `"en"`, `"es"`).
2. `enrollment.locale_for(sender_id)` — operator-configured per-user
   locale in `enrollment_users.locale`.
3. **Mirror fallback**: the model is told to respond in the same
   language as the user's most recent message.

Pass `metadata.locale` explicitly when your consumer already knows
the user's language (e.g. a Telegram bot reading `language_code` from
the user object). Otherwise the mirror fallback handles it.

## The `pending_attention` block

When `wiki_ingest_message` returns a `pending_attention` block,
**surface a short reminder to the user in your reply** before or
after the main answer — your call where it fits, but don't drop it
silently. Example wording, phrased in the user's own language:

> *"You've got 1 structural proposal pending and 2 changes I made
> automatically waiting for your confirmation — check them when you
> can: [dashboard link]"*

Mechanics:

- The block is present **only when** `pending_count +
  applied_pending_confirm_count > 0`. When zero, omitted — no noise
  on the default wire shape.
- `dashboard_path` is the relative path inside the dashboard. Compose
  the full URL by calling `dashboard_link(intent="answer_proposal",
  ...)` and using the signed URL, or — in known-trusted
  environments — concatenate `<dashboard_host>` + `dashboard_path`
  directly.
- `note` is opaque metadata. Today `"scoped_to_recipient"`, meaning the
  count is already filtered to the acting user — proposals addressed to
  them (the `recipient_id` column, 0032) plus the unaddressed ones; an
  admin caller sees the deployment-wide count. Because you call
  `wiki_ingest_message` with `X-MWE-Act-As: <human>`, the block you get
  back is already that human's — surface it to them, not to others.
  Treat the value as opaque; don't branch on it.
- This is your **proactive reminder channel**. `events_poll` (below)
  covers "what just happened"; `pending_attention` covers "what's
  still open" so the user is reminded every turn.
- **Routing async events (0032).** When you drain `events_poll`
  for the bot, the `structure_applied` / `dedup_proposed` /
  `auto_applied` payloads carry `recipient_id` (e.g. `"user:frodo"`,
  or `null` if unaddressed). Strip the `user:` prefix and send *that*
  human the notification — call `dashboard_link` with
  `X-MWE-Act-As: <that human>` and relay the returned URL (pointing at
  the payload's `dashboard_path`). On `null`, fall back to the
  operator/admin. For `structure_applied` this matters most: the change
  is **already applied**; the notice tells you whom to inform and where
  they can undo it.

## Read access on companion-wikis owned by the user

Standard consumers of the same `sender_id` as the companion-wiki's
owner can **read** companion-wikis (visible in `wiki_search`,
fetchable via `wiki_read`) and can **notify** their `_briefing.md`
via `wiki_admin_notify`. They cannot **write** — `wiki_admin_push` /
`wiki_admin_pull` return `403 requires_consumer_class_smart`.

Concrete scenario: Frodo says in Telegram "note this down: document the
recovery codes in the MFA flow". openclaw (standard consumer) routes
that through `wiki_ingest_message` first. Ingest never targets a
smart wiki: smart wikis are filtered out of the classifier's
`available_wikis` window, so the capture lands in Frodo's standard
personal memory, not the project's smart wiki. To get the note in
front of the project's smart consumer, openclaw calls
`wiki_admin_notify(wiki_id=w_lnprint_xy, topic="recovery codes",
body=<...>, source={kind: "user", ref: "telegram"})` — the item
lands in `_briefing.md` and Frodo's smart consumer (Claude Code on
laptop) surfaces it at the next session.

## Events and dashboard URLs

mwe-mcp emits events when something happens that the user should
know about. Your daemon polls `events_poll` periodically and
dispatches events to the right user; after dispatching, call
`events_ack` with the event ids so the server stops re-delivering.

```typescript
events_poll({ consumer_id, since?, kinds?, top_k? })
  → { events: Array<{ event_id, kind, payload, emitted_at, ... }>,
      has_more, took_ms }
```

### Event kinds

| Kind | What happened | What you do |
|---|---|---|
| `structure_applied` | REM **applied a structural change directly** (paragraph→page split or page→sub-wiki emergence) — apply + notice, no approval step | Payload names the affected user (`recipient_id`) and carries `variant`, source → target, `revert_deadline`, `dashboard_path`. Forward it to that user: "I reorganized X — undo here: [dashboard link]" |
| `auto_applied` | A questionnaire proposal auto-applied at its 24h `pending_timeout` (dedup lifecycle) | Payload has `dashboard_path` + `summary`; surface as "I did X — review or revert: [dashboard link]" |
| `dedup_proposed` | Merge proposal (REM dedup) pending the user | Surface with dashboard URL |
| `archive_proposed` | An archive proposal exists for a stale page | Surface with dashboard URL |

For **everything** structural, the canonical action is: get the
dashboard URL, present it to the user. The dashboard handles the
undo/declass buttons (and, for the dedup questionnaire kinds, the
confirmation buttons). **There are no MCP tools to list, apply,
confirm, or revert yourself** (see "Anti-patterns").

### Apply + notice (structural changes are act-first)

A structural change is **not** a blocking proposal: mwe-mcp applies it
directly during REM and tells you afterwards with a
`structure_applied` notice. There is nothing to approve — the
contract is "this happened — here's the undo". The affected user has
**7 days** (`revert_deadline`) to undo or declass from the dashboard;
silence means the change simply stands.

The dedup questionnaire kinds (`dedup_proposed`) still ride the
pending lifecycle: the user has **24 hours** to answer in the
dashboard; if silent, mwe-mcp auto-applies the `recommended` answers
(`auto_applied` event), and **silence past 7 more days is silent
confirmation**. A manual revert remains possible within the 7-day
`revert_deadline` of the apply.

### Polling cadence

For a chat bot, every ~30 s is fine. For an active session where the
user is typing, piggyback the poll on user turns (one poll per turn,
in parallel with the `wiki_ingest_message` call). Don't poll faster
than ~5 s — events are usually minutes-to-days-old, and the server
has rate limits. The `pending_attention` block already gives you a
per-turn reminder of open state, so a low polling cadence is fine.

## Consumer self-configuration

These are recommendations for how *your* agent should be configured
to play well with mwe-mcp. They are not enforced by the server — they
live on the consumer side.

### Do not truncate chat history mid tool-use cycle

When your agent's LLM produces an assistant message containing a
`tool_use` block, the LLM API requires the next assistant turn to be
preceded by a matching `tool_result`. If your consumer applies a
sliding-window FIFO truncation policy and the truncation drops the
assistant's `tool_use` but keeps a later message, the LLM API
rejects with "orphan tool_use" errors.

**Rule**: truncation policy is applied only at the **boundaries** of
a complete turn — *before* you send the next user message, or
*after* you finish composing the final assistant reply that includes
no pending tool calls. **During** an in-flight tool-use cycle
(`wiki_ingest_message` round-trip; agentic loop with multiple tool
calls; presentation of an event-driven dashboard URL chained from a
previous turn), the history is sacred.

This lesson cost real production bugs in the predecessor `mwe`
deployment running under OpenClaw — truncation fired mid-cycle,
orphaned the `tool_use`, and the LLM API rejection masked the actual
underlying bug for hours. Configure your agent's truncation to be
cycle-aware.

### Keep `recent_messages` short

The server caps `recent_messages` at 6 internally for coreference.
Sending more is harmless but wastes tokens. The "real" context the
LLM needs — past facts, decisions, the user's preferences — comes
back in `context_snippet`, because that's where mwe-mcp's memory
lives. Recent messages are only for short-term pronoun resolution in
the current conversation.

### Do not cache `wiki_read` / `wiki_search` results across senders

ACL is applied per-render: the same `wiki_id` can produce different
markdown for different senders depending on group memberships and
the per-fragment ACLs. If you cache `wiki_read` output keyed by
`wiki_id` alone, you leak content across users. Key the cache by
`(wiki_id, sender_id)` if you cache at all; for cross-user bots
(`X-MWE-Act-As` in play, Pattern B) include the **effective** sender,
not the bot's own `sender_id`.

### Use opaque `wiki_id`, never paths

Tool outputs do not return filesystem paths like `wiki/frodo/note.md`.
The filesystem layout is an implementation detail mwe-mcp may
rearrange in any minor release. Use the `wiki_id` you get from
`wiki_ingest_message.capture_id` or `wiki_search` results; pass that
opaque id back when calling `wiki_read`.

## Anti-patterns

- ❌ **Client-side intent classification.** Do not pattern-match on
  user text and pick an enum for `dashboard_link`. The server
  classifies. If a structural change happens, the server applies it
  and emits a `structure_applied` notice (or surfaces
  `pending_attention` on the next ingest); the consumer routes the
  user to the dashboard.
- ❌ **Trying to call any `structure_proposal_*` tool over MCP.**
  The whole family (list / apply / confirm / revert) was removed from
  the MCP surface. The dashboard is the only surface for those
  actions. Surface a `dashboard_link` URL instead.
- ❌ **Calling `_internal.*` tools directly.** They return `403
  not_exposed`. Use `wiki_ingest_message` for everything
  conversational.
- ❌ **Re-routing on `intent_classified`.** The `intent_classified`
  field is **audit-only** (debug, logging). Don't branch your code
  on it. The `suggested_seed` already carries whatever the server
  wants the user to see.
- ❌ **Dropping `pending_attention` silently.** When the block is
  present, surface a short reminder alongside your normal reply.
- ❌ **`wiki_admin_*` writes.** Off-limits to standard consumers
  (`403 requires_consumer_class_smart`). Notify-only is allowed
  (`wiki_admin_notify`) and is the canonical way to relay a user
  observation into a companion-wiki.
- ❌ **Path-shaped `wiki_id`.** See "Use opaque `wiki_id`".
- ❌ **Forgetting `X-MWE-Act-As` when capturing a real user's memory.**
  You are a standard (Pattern B) consumer: your `sender_id` is your own
  bot identity (a system user with its own wiki), **not** the person you
  are talking to. Set `X-MWE-Act-As: <real-user-id>` on every call that
  captures or recalls *their* memory, or it lands in your own wiki.
  Acting as a user the operator has not delegated returns
  `403 act_as_not_delegated`. See AGENT_INSTRUCTIONS.md §3.
- ❌ **Treating `events_poll` as synchronous.** It's cooperative —
  ack only what you have actually presented to the user.

## Tools used

| Family | Tool | Purpose |
|---|---|---|
| A | `wiki_ingest_message` | the workhorse — every user turn |
| B | `events_poll` / `events_ack` | polling cycle |
| C | `structure_proposal_list` | (read-only; how many proposals open) |
| D | `wiki_read` / `wiki_search` | explicit recall when the user asks for it |
| F | `consumer_register` | first-time daemon registration |
| F | `wiki_ingest_external` | bulk import (`variant: inline` only today) |
| G | `dashboard_link` | mint a one-shot URL into the dashboard |
| H | `wiki_admin_notify` | relay observations into a companion-wiki's `_briefing.md` |

## Cross-references

- Bootstrap document: [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).
- Sibling skills: `core-globalmemory`, `smart-consumer`,
  `smart-codebase`.
- Engineering wiki: `docs/protocol/mcp-tools.md`.
- Lifecycle reference: engine DB and migrations (5-state model in
  `structure_proposals`).
