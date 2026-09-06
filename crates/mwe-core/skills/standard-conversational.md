---
name: standard-conversational
version: 1.8.0
description: "Default conversational pattern for standard consumers (openclaw, hermes, nanoclaw): wiki_ingest_message passthrough, recent_messages window, disambiguation, locale plumbing, events_poll cadence, on-the-fly date corrections + sharing changes on stored facts, no wiki_admin_* writes."
depends_on: ["core"]
applies_to:
  consumer_class: standard
status: implemented
---

# mwe-mcp / standard-conversational skill

This skill defines the per-turn conversation loop for **standard
consumers** — agents that do **not** bring their own subscription LLM
budget and route every user turn through mwe-mcp's server-side
`ingest` LLM slot. Two hosts have a bridge today: **nanoclaw**, the
ready-made assistant an operator installs when they have no agent of
their own, and **hermes**, a plugin bridge for hermes-agent, whose
Telegram gateway is where the conversation arrives. The same pattern
applies to any other host that wants mwe-mcp to do the classification
+ recall + capture work for it.

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
- **Operation-path edits on stored facts** — a date correction ("the milk
  expires on the 20th, not the 25th") and a sharing change ("make this one
  visible to everyone", "share it with the family group") are recognized in
  the same `wiki_ingest_message` turn and applied act-first. The gate is
  server-side and differs between the two: a date correction takes the fact's
  **subject**, whoever recorded it, or anyone it was shared with; a sharing
  change takes the **subject** alone — the user it is about, or a member of
  the group it is about. Standard memory wikis only. You still just pass the
  raw message through.
- Structural turns (a new wiki, a move, a scope change): the turn
  applies nothing and the reply seed nudges the user to the dashboard.
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
                             from 72 kg 8 days ago.", ... }
Agent → injects context_snippet into its own system prompt for this
        turn, rephrases suggested_seed in product voice
Agent → "Yes, you are at 71 kg — a kilo down since Thursday, just as you remembered."
```

The optional blocks are **absent**, not null, when they do not apply:
`pending_votes` appears only when the user owes a vote on a pending
forget request, and `document_promoted` only when an oversized paste
was moved to the media rail.

The only call you make **outside** `wiki_ingest_message` during a
normal turn is to surface a URL the server tells you about (see
§"Events and dashboard URLs" below).

## The per-turn loop in detail

For each user message:

1. **Maintain a rolling window** of the last ~6–10 conversation turns
   in `recent_messages`. The server keeps the **last 16** of whatever you
   send and uses them for **coreference** ("the dentist from yesterday" →
   which dentist?), not for recall. The window is *not* persisted
   server-side — it is yours to keep.
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
5. **Check `needs_disambig`** — see "Disambiguation".
6. **Compose your reply** and send it back through your channel.

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
  recent_window?: string;                // the user's live thread from their OTHER
                                         //   surfaces, preformatted; inject verbatim
  capture_id?: string;                   // id of the fact this turn captured; for your
                                         //   own bookkeeping, do not echo it to the user
  needs_disambig: boolean;
  disambig_candidates: Array<{
    candidate_id: string;
    description: string;
  }>;
  llm_used: boolean;                     // diagnostic: the classifier answered
  took_ms: number;
  pending_votes?: {...};                 // key present ONLY when the user owes a vote
  document_promoted?: {...};             // key present ONLY when an oversized paste was
                                         //   promoted to the media rail
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

## Read access on smart wikis owned by the user

Standard consumers of the same `sender_id` as the smart wiki's
owner can **read** smart wikis (visible in `wiki_search`,
fetchable via `wiki_read`) and can **notify** their `_briefing.md`
via `wiki_admin_notify`. They cannot **write** — `wiki_admin_push` /
`wiki_admin_pull` return `403 requires_consumer_class_smart`.

Concrete scenario: Frodo says in Telegram "note this down: document the
recovery codes in the MFA flow". hermes (standard consumer) routes
that through `wiki_ingest_message` first. Ingest never targets a
smart wiki: smart wikis are filtered out of the classifier's
`available_wikis` window, so the capture lands in Frodo's standard
personal memory, not the project's smart wiki. To get the note in
front of the project's smart consumer, hermes calls
`wiki_admin_notify(wiki_id=frodo-lnprint, topic="recovery codes",
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
  → { events: Array<{ event_id, kind, wiki_id, fact_id, payload,
                      emitted_at }>,
      has_more }
```

### Event kinds

| Kind | What happened | What you do |
|---|---|---|
| `structure_applied` | Somebody changed a fact that belongs to **another** user — a validity closure, a sharing change, a move | Payload names the affected user (`recipient_id`) and carries `variant` and source → target. Forward it to **that** user: it is their fact somebody touched |
| `archive_proposed` | An archive proposal exists for a stale page | Surface with dashboard URL |
| `fact_minted_for_you` | Somebody stated a fact **about** the user, and they can read it | Tell that user what was said and who said it |
| `reminder_due` | A dated commitment the memory already holds has come round. It is never an alarm the user set, and never a plan a later message closed | Tell them what is due |
| `document_ingested` | A document-ingest job the user started has finished | Tell them what the memory now holds |
| `compile_failure_streak` | The narrative compiler failed or degraded the **same page** in consecutive passes and hit a notice threshold | Operator notice, addressed to nobody in particular. Payload carries `slug`, `source_path`, `consecutive`, `last_error` and a `dashboard_path` — surface it to whoever runs the server |
| `recall_tuning_proposed` | The same fact kept missing recall and no local repair could be proved, so the fix needs a human | Operator notice. Payload carries the fact, its home, the miss count, a sample query and the gate outcome. Never auto-applied — surface the evidence and let the operator decide |
| `budget_threshold_reached` | Today's metered spend crossed the deployment's daily budget — the warning line first, then the budget itself | Operator notice, addressed to nobody in particular, at most once per threshold per day. Payload carries `threshold` (`warn` or `stop`), the `day`, `spent`, `limit`, `percent`, `currency`, a `dashboard_path`, and `stopped` — whether paid model calls are actually being refused right now. While `stopped` is true, turns still answer but come back degraded and say nothing was saved; it clears when the operator raises the budget, unlocks the day, or the day turns over |

### The nightly cycle is silent

**mwe-mcp does not report its own housekeeping.** Every night the memory
reorganises itself — pages split, near-duplicates merge, facts move to
where they belong — and none of that produces an event. It is the
memory's own business, the way you do not narrate your own thinking, and
a nightly diary of splits and merges is noise the user never asked for.

What *does* reach the user is what somebody **did to their facts**: a
change to a fact about them, or a fact somebody stated about them. That
is a different question, and the reason these events exist at all.

None of that reshuffling asks the user anything. The memory keeps
itself; the user steers it by talking to you.

### Polling cadence

For a chat bot, every ~30 s is fine. For an active session where the
user is typing, piggyback the poll on user turns (one poll per turn,
in parallel with the `wiki_ingest_message` call). Don't poll faster
than ~5 s — events are usually minutes-to-days-old, and the server
has rate limits, so a low polling cadence is fine.

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

An orphan-`tool_use` rejection also masks what went wrong underneath:
the error names the orphan, not whatever the turn was really failing
at. Configure your agent's truncation to be cycle-aware.

### Keep `recent_messages` short

The server carries the last 16 of them into the classifier prompt, for
coreference; sending more than that is harmless but wastes tokens. The
"real" context the LLM needs — past facts, decisions, the user's
preferences — comes back in `context_snippet`, because that's where
mwe-mcp's memory lives. Recent messages are only for short-term pronoun
resolution in the current conversation.

### Do not cache `wiki_read` / `wiki_search` results across senders

ACL is applied per-render: the same `wiki_id` can produce different
markdown for different senders depending on group memberships and
the per-fragment ACLs. If you cache `wiki_read` output keyed by
`wiki_id` alone, you leak content across users. Key the cache by
`(wiki_id, sender_id)` if you cache at all; for cross-user bots
(`X-MWE-Act-As` in play, Pattern B) include the **effective** sender,
not the bot's own `sender_id`.

### Use opaque `wiki_id`, never paths

Tool outputs do not return filesystem paths like
`wikis/frodo/note.md`. Where a wiki keeps its files is an
implementation detail mwe-mcp may rearrange in any minor release. A
`wiki_search` (or `wiki_navigate`) hit carries the pair you need —
`wiki_id` plus `path`, the page spelled relative to that wiki — and
those two are exactly `wiki_read`'s arguments. (`capture_id` from
`wiki_ingest_message` is not one of them: it is the id of the fact the
turn captured, which `wiki_forget` takes.)

## Anti-patterns

- ❌ **Client-side intent classification.** Do not pattern-match on
  user text and pick an enum for `dashboard_link`. The server
  classifies, and on a structural turn the `suggested_seed` it returns
  is already the nudge toward the dashboard.
- ❌ **Trying to call any `structure_proposal_*` tool over MCP.**
  There is no `structure_proposal_*` family on the MCP surface. The dashboard
  is the only surface for those actions. Surface a `dashboard_link` URL
  instead.
- ❌ **Inventing a tool name.** Anything outside the roster
  `tools/list` returns is answered `not_found` — the dispatcher matches
  on the name and has no other branch. Use `wiki_ingest_message` for
  everything conversational.
- ❌ **Re-routing on `intent_classified`.** The `intent_classified`
  field is **audit-only** (debug, logging). Don't branch your code
  on it. The `suggested_seed` already carries whatever the server
  wants the user to see.
- ❌ **`wiki_admin_*` writes.** Off-limits to standard consumers
  (`403 requires_consumer_class_smart`). Notify-only is allowed
  (`wiki_admin_notify`) and is the canonical way to relay a user
  observation into a smart wiki.
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
| D | `wiki_read` / `wiki_search` | explicit recall when the user asks for it |
| F | `consumer_register` | first-time daemon registration |
| F | `wiki_ingest_external` | bulk import (`source: {type: "inline", content: "…"}`; `media` takes an uploaded `catalog_id`, and `file` / `git` / `url` answer `not_implemented_phase_c`) |
| G | `dashboard_link` | mint a one-shot URL into the dashboard |
| H | `wiki_admin_notify` | relay observations into a smart wiki's `_briefing.md` |

## Cross-references

- Bootstrap document: [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).
- Sibling skills: `core-globalmemory`, `smart-consumer`,
  `smart-codebase`.
