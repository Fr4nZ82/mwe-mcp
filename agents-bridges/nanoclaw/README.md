# nanoclaw bridge

The default consumer agent of an mwe-mcp memory server, built on
[nanoclaw](https://github.com/nanocoai/nanoclaw): a chat assistant that
remembers every conversation, recalls the right part of it on every turn, and
knows what it may say to whom.

Two pieces, installed together:

| | what it is | where it lands in the fork |
|---|---|---|
| **`templates/mwe`** | the agent template — the persona and the plugin that switches the memory on | `templates/mwe/` |
| **`skills/add-mwe-memory`** | the fork skill — the per-turn contract, the host module, the reverse channel | `.claude/skills/add-mwe-memory/` |

Tested against nanoclaw **v2.3.0** (the `pin` in `bridge.toml`). Implements the
per-turn contract **v1**
([`INTEGRATING.md`](../../INTEGRATING.md#the-per-turn-contract-v1--wiring-a-host-bridge)).

## What an agent gets

- **Every turn is stored, and every turn arrives with what it calls up.** One
  `wiki_ingest_message` per conversational turn, mechanically — no save tool
  for the model to forget, no recall tool for it to skip. The recall block goes
  in front of the turn's messages, after the stable system prompt, so the
  provider's prompt cache keeps hitting.
- **Each person is remembered as themselves.** The chat sender is mapped to
  their own mwe user through `senderMap`, and the call carries
  `X-MWE-Act-As`. Somebody unmapped speaks as `guest`: their turn recalls only
  public memory and stores nothing.
- **The built-in memory is off.** No `memory/index.md`, no session-start
  injection, no `conversations/` transcript.
- **No session is carried between turns.** Every turn is a fresh provider
  query; its conversational context is the bridge's own recent window plus the
  recall block. Nothing is ever compacted or summarized.
- **The memory can start a conversation.** A fact minted for somebody else, or
  a commitment coming due, is delivered to that person's own chat, phrased by
  the agent, in their language.
- **What the memory needs from a person reaches them.** A request to forget a
  fact they are part of comes back as a heads-up on their next turn, with the
  dashboard link where the vote is cast and what silence costs; a message long
  enough to be a document is archived whole, and the agent says so.

Everything else nanoclaw gives an agent — chat, the web, its own container,
scheduled tasks, several channels at once — is untouched.

## Before you install

- **nanoclaw's own prerequisites**: Node 22+, pnpm 10+, Docker, and Claude Code
  if you want to drive the skill conversationally.
- **A running mwe-mcp server** ([`INSTALL.md`](../../INSTALL.md)), and from its
  dashboard:
  - a **consumer token** for this nanoclaw (a *standard* consumer — the bot is
    a credential-less system user speaking for delegated humans);
  - a **delegation** for every person it will speak for, plus **`guest`**.
    Without the `guest` delegation an unrecognised sender's turn is refused
    (`403 act_as_not_delegated`) instead of being answered anonymously.
- A bot user id made of plain lowercase letters and digits — `samnano`, say. No
  underscores, no hyphens.

## Install

The dashboard serves this as one command:

```bash
curl <your-mwe-origin>/bridges/nanoclaw/install.sh | sh
```

These are the steps it performs, if you would rather do them by hand. Run them
from your nanoclaw fork.

1. **Get nanoclaw at the tested ref.**

   ```bash
   git clone https://github.com/nanocoai/nanoclaw
   cd nanoclaw && git checkout v2.3.0
   ```

2. **Copy the two directories in.**

   ```bash
   mkdir -p .claude/skills templates
   cp -R <bridge>/templates/mwe          templates/mwe
   cp -R <bridge>/skills/add-mwe-memory  .claude/skills/add-mwe-memory
   ```

3. **Run nanoclaw's setup** if this is a fresh install (`bash nanoclaw.sh`).
   At **Template setup** choose **Local templates**, then `mwe`; it stamps the
   first agent for you. On an existing install, stamp it yourself:

   ```bash
   ncl groups create --template mwe --name mwe --new
   ```

   `--name` is yours: it becomes the agent's name and its group folder. The
   product imposes no character.

4. **Apply the skill.** From Claude Code, `/add-mwe-memory`. It asks three
   questions — the endpoint, your chat id, your mwe user id — then copies the
   modules in, splices the reach-ins into five of nanoclaw's own files,
   writes `mwe.json`, builds and tests.

   Without Claude Code, the same steps are in
   [`skills/add-mwe-memory/SKILL.md`](skills/add-mwe-memory/SKILL.md) as
   ordinary shell commands.

5. **Put the token in `.env`.**

   ```
   MWE_TOKEN=<the consumer token from the dashboard>
   ```

   The installer never carries it, never logs it, and never asks for it on a
   command line.

6. **Fill in `senderMap`** — one line per person (below) — and restart:

   ```bash
   source setup/lib/install-slug.sh && systemctl --user restart $(systemd_unit)
   ```

7. **Connect a channel** and talk to it.

## Configuration — `mwe.json`

At the fork root, beside `package.json`. The installer writes it; everything
after the first two lines is yours to edit.

```json
{
  "serverUrl": "http://127.0.0.1:8742/mcp",
  "senderMap": {
    "telegram:123456789": "alice",
    "telegram:987654321": "bob"
  },
  "operatorSender": "telegram:123456789",
  "locale": "it-IT",
  "maxWindow": 16,
  "groups": [],
  "eventsEnabled": true,
  "eventsPollSeconds": 30,
  "dashboardUrl": "https://memory.example"
}
```

| key | what it does |
|---|---|
| `serverUrl` | the MCP endpoint, ending in `/mcp`. Plain HTTP is fine for a loopback server; anything else should be HTTPS. |
| `senderMap` | `<channel>:<platform id>` → mwe user id. **Explicit entries only.** Anyone not listed is a `guest`; there is no fallback to the owner. |
| `operatorSender` | the chat that gets the daily recap of what the memory did. Empty = no recap. |
| `locale` | BCP-47 tag sent as `metadata.locale`. Empty = each user's own server-side default. |
| `maxWindow` | how many messages of recent conversation ride each turn (default 16, the server's cap). |
| `groups` | agent groups the host serves. Empty — the normal case — means all of them, and what makes a group ask is the `mwe` plugin. Fill it to take one group off the memory without restarting its container. |
| `eventsEnabled` | the reverse channel. `false` stops the poll loop entirely. |
| `eventsPollSeconds` | how often it polls (default 30, floor 5). |
| `dashboardUrl` | the public origin every dashboard link hangs on — the one `mwe_dashboard_link` mints, the page a vote block names, the page a notice offers. Empty = `serverUrl` minus `/mcp`, which is right only when the server is reachable at that address from a phone. |

### The switches that turn nanoclaw's own memory off

All of them are in the container, and all of them turn on the same question —
is the `mwe` plugin stamped into this group?

| what stops | where |
|---|---|
| the `memory/` tree (`index.md`, `system/definition.md`) is not created | `container/agent-runner/src/memory/scaffold.ts` — `ensureMemoryScaffold` returns early |
| the session-start hook injects nothing | `container/agent-runner/src/memory/hook.ts` — the hook stays registered, and emits no context |
| no session is resumed and none is stored | `container/agent-runner/src/poll-loop.ts` — `continuation` is `undefined` on every query, and both `setContinuation` writes are skipped |
| a follow-up never rides a live query | the same file — a pending message ends the stream and starts a new turn instead |
| nothing is compacted or summarized | consequence of the above: a turn's context is one turn, so the provider's compaction window is never approached and the transcript-rotation path is never entered |

Take the plugin off a group (`rm -rf groups/<folder>/plugins/mwe`) and every
one of them reverts at the next container start.

## Connecting a channel — Telegram, worked through

1. Install the channel the nanoclaw way: `/add-telegram` from Claude Code, or
   `bash nanoclaw.sh` → channels. It asks for the bot token and pairs your
   account.
2. Wire the agent to the chat: `/manage-channels`, or
   `ncl wirings create --group <agent-group-id> --messaging-group <chat>`.
3. Note each person's numeric Telegram id — nanoclaw stores it in its `users`
   table as `telegram:<id>`, and that is the `senderMap` key. For a private
   chat the sender id **is** the chat id, which is what lets a notice find its
   way back.
4. Add one `senderMap` line per person and restart.

Another channel works the same way; only the `<channel>:` prefix changes. A
notice can only be delivered to a chat the bridge can name, so a person with no
`senderMap` entry receives nothing — their memory is still theirs, they are
just not reachable.

## Media

A photo, a voice note or a document sent in chat is uploaded to the memory out
of band and rides the same turn as a catalog id, so the memory files it as a
described fact. Nothing special to configure: nanoclaw already saves an
incoming attachment into the session's inbox, and the bridge reads it from
there.

Two limits worth knowing. A **guest** turn uploads nothing — the endpoint
refuses it and the words are ingested alone. And a **document somebody wants
read into memory** (a manual, a long transcript) is a different gesture from a
photo in passing; that path is `wiki_ingest_external`, and this bridge does not
drive it yet.

## The reverse channel

Every `eventsPollSeconds` the host polls the memory for notices:

- **`fact_minted_for_you`** — somebody else's conversation minted facts that
  belong to this person. The daemon routes `recipient_id` back through
  `senderMap` **read backwards**, finds the chat that person talks to the agent
  in, and puts a delivery instruction there. The agent writes the message: in
  their language, saying where it came from, never implying they were present.
- **`reminder_due`** — something they committed to has come round.

The notice is acked only once the instruction is durably written, so a crash
between the two costs a repeat, never a loss. A recipient with no `senderMap`
entry is retried for about ten minutes — the map can be fixed live — and then
logged as `UNDELIVERABLE`; the facts stay in their memory either way.

The remaining kinds are the operator's business (pages reorganized, documents
ingested, proposals waiting) and are batched into **one recap a day** in
`operatorSender`'s chat. A quiet day is silent.

## Smokes

```bash
python3 ../_harness/run_smokes.py      # every bridge, from agents-bridges/
./smoke.sh                             # this bridge only
```

The offline smoke needs `bun`, `pnpm`, `python3` and `git`, and no network
beyond the clone. It fetches nanoclaw at the pin, installs the skill and the
template the way an operator does, and then drives **nanoclaw's own poll loop**
with the mock provider against a recording stub of the MCP endpoint. What it
asserts: one ingest per turn and one per reply; the window threaded, trimmed
and persisted; act-as per sender and `guest` for the unmapped; the recall block
ahead of the formatted batch; the disambiguation and its commit; the owed
forget-request vote and the promoted document reaching the agent, and neither
line showing up on a turn that did not earn it; the media upload and its
catalog id; a memory outage that leaves the turn answering; no continuation
between turns; and the reverse channel's poll → enqueue → ack order, including
a notice that must not be delivered to the wrong person.

`NANOCLAW_SRC=/path/to/a/local/checkout` clones from disk instead of GitHub;
`MWE_SMOKE_KEEP=1` leaves the scratch fork behind to poke at.

The live smoke ([`smoke_live.md`](smoke_live.md)) is operator-run against a
real server and costs model calls.

## Design choices

**Replace, never run alongside.** nanoclaw's built-in memory is switched off
rather than left to coexist. A second store accumulates stale duplicates, skips
the per-reader redaction that is the point of this product, and — injected
globally at session start — leaks one person's notes into another's turn.
Capture needs no save tool: the per-turn ingest is the capture path.

**The token never enters the container, because it cannot.** nanoclaw refuses
to start an agent container whose environment carries a secret-shaped value
(`src/drivers/types.ts`, `isSecretShaped` — `MWE_TOKEN` matches by name, and a
JWT by value), and mounting a credential into an agent is denied by the same
policy. That is not an obstacle to work around: it is the host's central
invariant, and the bridge is shaped by it. The token stays on the host, read
from `.env` by the one module that talks to the memory, and the container asks
the host to act for it over nanoclaw's existing container→host action channel.
The agent's shell cannot read a credential that is not there.

What that leaves, stated plainly: a compromised model could invoke the bridge's
own host action and name a mapped sender key, and the host would act as that
person. The host never accepts a raw mwe user id from the container — only a
chat key it maps itself — so the blast radius is the people already in
`senderMap`, and no credential leaks. Closing it further needs a host-side
lookup of an inbound row by id, which nanoclaw 2.3.0 does not expose.

**One ingest per turn, one per sender.** A batch from one person is one turn. A
batch that mixes people is one turn each, so nobody's words are filed under
somebody else's name. The recall block shown is the one for whoever spoke last
— the person the agent is answering — because two identity cards in one prompt
would tell the model two different things about who it is talking to.

**Stateless per turn.** The provider gets no continuation, so a turn's context
is exactly: the system prompt (persona + destinations), the recall block, the
recent window, this turn's messages. Long-range continuity is recall's job, not
a summary's. The visible consequence is that the memory, not a transcript,
decides what the agent remembers — which is the whole product.

**An unmapped sender is a guest, never the owner.** Falling back to a real
identity would file a stranger's words as that person's facts and hand the
stranger that person's recall. The bridge would rather answer a guest.

**No `suggested_seed`.** The ingest response carries a pre-drafted reply for
consumers with no model of their own. nanoclaw always has one, and splicing a
ready-made answer into the turn invites a model to continue it instead of
treating it as reference — and launders the classifier's guesses into the
agent's mouth. The recalled facts are what the agent needs.

**No `mcp.json`.** The memory is not declared as an MCP server for the agent to
call at will. Two reasons: a static header cannot carry a per-sender act-as,
and nanoclaw rejects a plugin MCP server whose host reaches the container host
(`src/templates/mcp.ts`) — which is exactly where a self-hosted memory sits.
The three explicit tools are registered on the container's own in-process MCP
server instead, each acting as the person speaking.

**What two of them answer.** The memory mints a dashboard link as a *path* — it
does not know the origin it is reached at — so the host puts the dashboard
origin in front of it and `mwe_dashboard_link` hands the agent an address a
person can open. `mwe_disambig_commit` is an ingest like any other, so it
answers with the recall block for the message it just stored, in the same
`<memory-context>` fence a turn arrives in, under one line saying it is stored;
the response's operational fields never reach the agent.

## Known limits

- **A follow-up message arriving mid-turn ends the query** and starts a fresh
  one, so it is a little slower than nanoclaw's default of pushing it into the
  live stream. That is the price of every turn having its own ingest and recall.
- **Each memory call costs a host round trip** — up to about a second and a
  quarter, because the host's delivery poll runs once a second and the
  container polls for the answer four times a second. The ingest sits on the
  turn's critical path and pays it deliberately: better memory over
  first-token latency.
- **`/update-nanoclaw` does not refresh this skill.** Its refresh pass only
  discovers channels and providers. After an upgrade, run the skill again; it
  reports what is already in place, and `src/mwe-wiring.test.ts` goes red if an
  upgrade moved one of the reach-ins.
- **`wiki_ingest_external`** — handing the memory a document to read as a unit
  — is not wired. A file sent in chat is filed as an attachment; a paste long
  enough for the server to promote it is what becomes a document.
