---
name: core
version: 1.2.0
description: "Always-loaded mwe-mcp skill: cardinal rule, identity model (3-claim JWT), bootstrap dispatcher, auto recall+capture and the 3-way route (your operational wiki / a project wiki / the user's standard memory), skill catalog index, token lifecycle. Every consumer (smart or standard) loads this first."
depends_on: []
applies_to:
  consumer_class: any
status: implemented
---

# mwe-mcp / core skill

This is the always-loaded skill for any consumer agent that talks to
mwe-mcp via MCP. It is **not** a tutorial — it is the irreducible
contract: who you are on the wire, which deeper skill to load next,
and what to do when authentication breaks. The substantive per-class
behaviour lives in `smart-consumer`, `core-globalmemory`,
`standard-conversational`, and `smart-codebase`; this file
points you at the right one.

## Cardinal rule

mwe-mcp is the persistent memory layer of an LLM agent. Use it to
**store** what the user tells you that matters across sessions, and
to **recall** what was stored when the user references prior context.
The mechanics differ by consumer class:

- **Standard consumers** (openclaw, hermes, nanoclaw — anything that
  uses mwe-mcp's own LLM budget for routing) pass every user turn
  through `wiki_ingest_message`. mwe-mcp's server-side `ingest` slot
  does intent classification, recall, capture, structural proposals.
  The consumer is a thin passthrough. See skill
  `standard-conversational` for the full per-turn loop.
- **Smart consumers** (Claude Code, Cowork, Codex — agents with their
  own subscription LLM) bring their own classification budget. They
  use `wiki_search` directly for recall and `wiki_admin_push/pull` to
  author wikis themselves. A smart consumer is a **superset** of a
  standard one and writes to up to **three** places — see the routing
  below. See skill `smart-consumer` for cwd-bound project mode (it
  carries the per-message router) and `core-globalmemory` for the
  transversal mode.

## Auto recall + capture, and where memory goes (smart consumers)

You **recall and capture on your own initiative** — not only when the user asks.
Recall relevant memory before you answer (on the first prompt and on a topic
shift); capture durable facts as they come up. The mwe-mcp `instructions` and the
session-start nudge both remind you to.

**Route every captured item — never dump everything into the user's standard
memory.** The gross route per message is your judgement; the set is small:

| Goes to | What | How |
|---|---|---|
| **drop** | ephemeral ops ("re-run the tests") | nowhere |
| **your operational wiki** | your own general working memory, your **behaviour rules**, a dated `conversations.md` log — the wiki forged for you at sign-in (if you own one) | `wiki_admin_push` |
| **a project wiki** | durable project / codebase knowledge + decisions | `wiki_admin_push` (never `wiki_ingest_message`) |
| **the user's standard memory** | facts **about the user** — a preference, an appointment, who someone is | `wiki_ingest_message` (the server classifies + files it) |

Project content goes to a wiki you author, **never** through `wiki_ingest_message`
(the server filters smart wikis out of ingest anyway); facts about the *user*
stay canonical in their standard memory so recall and other consumers see them.
The deeper skills carry the details.

The dispatcher below picks which deeper skill applies.

## Connection and identity

Connect over **MCP Streamable HTTP** to `<mwe-mcp-host>/mcp` with an
`Authorization: Bearer <jwt>` header. Tokens are issued by the
operator via the dashboard or the `mwe-mcp token-issue` CLI.

The JWT carries three claims that decide your wire identity:

| Claim | Required | Meaning |
|---|---|---|
| `sender_id` | yes | The **human owner**. Captures land in `wikis/<sender_id>/`. Recall is ACL-scoped to this user. |
| `consumer_id` | optional | Your device / deployment label (e.g. `cc-laptop`, `samvise-prod`). Distinguishes multiple devices of the same user in `wiki_admin_op_log` and the cooperative lease. Required when `consumer_class=smart`. |
| `consumer_class` | optional, default `standard` | `smart` enables the `wiki_admin_*` tools. Standard consumers get `403 requires_consumer_class_smart` on those tools. |

You do **not** pass `sender_id` in any tool argument — if you include
it, the server validates it matches the token claim and rejects on
mismatch with `403 sender_token_mismatch`.

**Pattern B** (multi-user consumer bots like Samvise that serve N
end-users) additionally use the per-call header `X-MWE-Act-As: <real-user-id>`
to act on behalf of a delegated end-user. See AGENT_INSTRUCTIONS.md §3
for the operator-side setup of system users and delegations.

## Bootstrap dispatcher

```
on_session_start():
    load_skill("core")                          # this file, always

    if consumer_class == "smart":
        smart_bootstrap()                       # discover the wikis you own — your
                                                # operational wiki (if you signed in over
                                                # OAuth) + any project wikis + pending
                                                # briefing. Load + surface them.
        if cwd_has_mwe_state(".mwe/state.json"):
            load_skill("smart-consumer")        # cwd-bound project mode
            load_skill("smart-codebase")        # iff project_kind == software
        else:
            load_skill("core-globalmemory")     # transversal mode; a project wiki is
                                                # bootstrapped only on the user's explicit ask
        # then, per turn: recall before answering, and route each memory-worthy turn
        # (operational wiki / project wiki / wiki_ingest_message) — see above.

    else:
        load_skill("standard-conversational")   # per-turn ingest pattern
```

Skills are fetched via the `skill_list` / `skill_fetch` MCP tools (the
catalog ships server-side; no copy-paste required for the bundled
set). Only bundled skills exist — there are no custom user-scoped
skills.

## Skill catalog

| Skill | Prerequisite | Purpose |
|---|---|---|
| `core` | always | this file |
| `core-globalmemory` | `consumer_class=smart`, no `.mwe/state.json` in cwd | transversal recall across the user's standard wikis on first prompt |
| `smart-consumer` | `consumer_class=smart` + `.mwe/state.json` in cwd | authoritative `wiki_admin_*` management of a per-project companion-wiki |
| `smart-codebase` | `smart-consumer` + software project | concrete folder layout + ingest pre-existing docs / wiki |
| `standard-conversational` | `consumer_class=standard` (or absent) | `wiki_ingest_message` loop, `events_poll`, `pending_attention`, structural notices + undo routing |

## Token lifecycle

- **Internal token** (1 year TTL): local-device clients on the
  operator's own machine.
- **Exposed token** (30 day TTL): public-internet clients. Refresh
  proactively when `exp - now < 7 days` via `POST /mcp/token-refresh`.
- **Session cookie** (10 min sliding): dashboard browser only, never
  over MCP.

### Auth failure semantics

| Wire code | Caller behaviour |
|---|---|
| `401 invalid_token` | Signature mismatch / clock skew / unknown server. Hard configuration error — surface immediately, do not queue local writes. |
| `401 token_revoked` | JTI blacklisted (operator revoked the token). **Smart consumers**: keep the local `.mwe/wiki/` cache intact, queue local edits, prompt the operator for a new token. See `smart-consumer` §"Graceful degradation". **Standard consumers**: stop and surface the failure. |
| `401 expired` / `401 secret_rotated` | Same as `invalid_token` in caller behaviour: surface, do not retry. |
| `403 requires_consumer_class_smart` | You called a `wiki_admin_*` tool without the `smart` claim. Don't retry. |
| `403 wiki_owned_by_other_user` | You tried `wiki_admin_push/pull` on a wiki whose `owner_user` is not your `sender_id`. Note: read access can still be granted via `shared_with` (see `smart-consumer`). |
| `423 wiki_locked_by_lease` | Cooperative lease held by another smart consumer of the same user. Wait + retry, or back off. |

The blacklist propagates within ~60 s of a revoke; the
`consumer_delegations` cache has the same 60 s TTL.

## Cross-references

- Bootstrap document (delivered out-of-band):
  [`AGENT_INSTRUCTIONS.md`](AGENT_INSTRUCTIONS.md).
- Tool surface: the MCP tools across families A–L — full schemas + the
  current count at
  [`tool reference`](../../../wiki/protocol/tool-reference.md) and
  `wiki/protocol/mcp-tools.md`.
- Engineering wiki overview:
  `wiki/architecture/overview.md`.
- Companion-wikis design note:
  `wiki/design-notes/companion-wikis.md`.
