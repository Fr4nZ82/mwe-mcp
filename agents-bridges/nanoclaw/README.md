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
  injection, no `conversations/` transcript. A tree an earlier boot already
  wrote is removed when it is still nanoclaw's untouched templates, and
  nanoclaw's shared `CLAUDE.md` reaches the group without the two sections
  that teach that store.
- **Its name comes from the people it serves.** nanoclaw puts a configured
  name at the top of every system prompt; a memory agent is not given one
  there. Its name is a fact of the memory — the recall block's `WHO YOU ARE` —
  so telling it what to be called is something anybody it serves can do, in
  chat, and it holds.
- **No session is carried between turns.** Every turn is a fresh provider
  query; its conversational context is the bridge's own recent window plus the
  recall block. Nothing is ever compacted or summarized.
- **The memory can start a conversation, once.** A fact minted for somebody
  else, or a commitment coming due, is delivered to that person's own chat,
  phrased by the agent, in their language. Everything waiting for one person
  in one round travels as **one** message — a backlog is a paragraph with the
  items in it, never a burst of notifications.
- **What the memory needs from a person reaches them, when it is due.** A
  request to forget a fact they are part of rides every turn of its seven-day
  window, but the agent is told to raise it only in the **last day** — with the
  dashboard link where the vote is cast and what silence costs. Before that it
  is told to leave it alone, and to answer only if they ask. A message long
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

1. **Get nanoclaw at the tested ref, with its registry branches.**

   ```bash
   git clone https://github.com/nanocoai/nanoclaw
   cd nanoclaw && git checkout v2.3.0
   git fetch --depth 1 origin channels:refs/remotes/origin/channels
   git fetch --depth 1 origin providers:refs/remotes/origin/providers
   ```

   The last two lines are not optional. nanoclaw ships no channel adapter in
   trunk: `/add-telegram` and its siblings copy their files with
   `git show origin/channels:<path>`, and the alternative providers come from
   `origin/providers` the same way. A clone of one ref tracks only that ref, so
   without those refs the setup wizard dies at the channel step with
   `fatal: invalid object name 'origin/channels'`.

2. **Copy the two directories in.**

   ```bash
   mkdir -p .claude/skills templates
   cp -R <bridge>/templates/mwe          templates/mwe
   cp -R <bridge>/skills/add-mwe-memory  .claude/skills/add-mwe-memory
   ```

3. **Run nanoclaw's setup** if this is a fresh install (`bash nanoclaw.sh`).

   Name the template first and the wizard offers it instead of asking you to
   find it:

   ```bash
   echo 'NANOCLAW_TEMPLATE_PATH=mwe' >> .env
   ```

   That is the one key nanoclaw's wizard reads out of `.env` (`setup/auto.ts`
   bridges it into the run); it then asks you to confirm the `mwe` template and
   stamps the first agent. Decline, and you pick it by hand: **Local
   templates**, then `mwe`. On an existing install, stamp it yourself:

   ```bash
   ncl groups create --template mwe --name mwe --new
   ```

   `--name` is yours: it becomes the group folder, and nanoclaw's own default
   for the agent's name. It is not what the agent answers to — that is the
   memory's business, and anybody it serves can tell it in chat.

   **What the wizard still asks**, whatever is in `.env`: where the sandbox
   image comes from (**Build it here** needs no account), the OneCLI vault, the
   Claude sign-in it opens in your browser, and — when you connect a channel —
   the pairing code you send the bot.

4. **Apply the skill.** From Claude Code, `/add-mwe-memory`. It asks three
   questions — the endpoint, your chat id, your mwe user id — then copies the
   modules in, splices the reach-ins into seven of nanoclaw's own files,
   clears a memory tree an earlier boot left behind, writes `mwe.json`, builds
   and tests.

   Without Claude Code, the same steps are in
   [`skills/add-mwe-memory/SKILL.md`](skills/add-mwe-memory/SKILL.md) as
   ordinary shell commands.

5. **Put the token in `.env`.**

   ```
   MWE_TOKEN=<the consumer token from the dashboard>
   ```

   The installer never carries it, never logs it, and never asks for it on a
   command line.

6. **Fill in `senderMap`** — one line per person (below) — and restart. Both
   halves: the host service, and the agent containers.

   ```bash
   source setup/lib/install-slug.sh && systemctl --user restart $(systemd_unit)
   pnpm exec tsx .claude/skills/add-mwe-memory/restart-mwe-groups.ts
   ```

   The second line is the one that is easy to miss. nanoclaw leaves a session's
   container running when the host service stops, and the agent runner is a
   process that read its modules at boot — a read-only mount of the patched
   source does not reload them. Until the container is replaced, the agent
   answers with the code as it was before the skill: no ingest, no recall
   block, a session carried between turns, and **nothing in any log saying
   so**. The script stops the containers of every group carrying the plugin;
   they come back on the next message.

7. **Connect a channel** and talk to it.

8. **Check you are talking to the new container**, before you believe anything
   about the memory:

   ```bash
   docker logs "$(docker ps -q --filter label=nanoclaw-session | head -1)" 2>&1 | grep '\[mwe\]'
   ```

   (The label, not the name: `nanoclaw-session` is one of the four canonical
   labels every session container carries, and the container's name belongs to
   whichever session driver started it.)

   `[mwe] memory is on for this agent` is the runner saying it read the patched
   code and found the plugin. No `[mwe]` line means the pre-skill runner is
   still serving that chat: `ncl groups restart --id <agent-group-id>`, then
   send another message. The turn itself shows up as `mwe_request` in the host
   log and as a `system` row in the session's outbound mailbox
   (`data/v2-sessions/<agent group>/<session>/outbound.db`).

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
| `senderMap` | `<channel>:<platform id>` → mwe user id. **Explicit entries only.** Anyone not listed is a `guest`; there is no fallback to the owner. A key with no channel in it (`alice`) is dropped at load with one warning — it names no chat, so nothing could be delivered to it. |
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
| nothing is compacted or summarized, and no `conversations/` transcript is written | consequence of the above. Both archive calls sit behind a session: the pre-compact hook, which a one-turn context never reaches, and the transcript rotation, which runs only when a stored session is about to be resumed. A session found on disk at startup is dropped before that check, so a container replaced mid-life writes none either |
| the shared `CLAUDE.md` arrives without its `Memory` and `Conversation history` sections | `src/claude-md-compose.ts` — a memory group gets the base filtered by `src/modules/mwe/base.ts` instead of the symlink to it. Host-side, so this one is not in the container |
| the system prompt does not name the agent | `container/agent-runner/src/destinations.ts` — the identity section is not built; the name is the memory's `WHO YOU ARE` |

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

**One delivery per person per round.** Whatever is waiting for the same
recipient is composed into a single instruction — each item keeping its own
source line and its own link — and the agent is told to answer with one
message. This is what a backlog looks like when the bridge has been down for a
while, and one message per notice would be a burst of system alerts rather than
somebody who remembers. Two people's notices in the same round stay two
deliveries. A group is acked as a group: acking part of it would drop the
notices whose words never reached anybody.

**A turn carrying notices is never cut short.** A person's message can be
dropped mid-turn and lose nothing — it is already ingested and it sits in the
recent window — but a delivery instruction is stored nowhere and was acked when
it was enqueued. So a follow-up arriving while the agent is delivering notices
waits for that turn to end instead of ending it.

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
forget-request vote and the promoted document reaching the agent, neither
line showing up on a turn that did not earn it, and the vote raised because
its deadline is inside the day; the media upload and its
catalog id; a memory outage that leaves the turn answering; no continuation
between turns; and the reverse channel's poll → enqueue → ack order, including
a notice that must not be delivered to the wrong person.

Three of those turns run **on the clock**: a provider that takes longer to
answer than the follow-up poller's interval, with the host answering slowly
too. A turn that outlives that poller must still reach the person; a follow-up
arriving mid-query must end the query rather than ride it; and a turn carrying
a backlog of notices must deliver every one of them even when somebody writes
in halfway, with that person served next and nothing lost either way. A mock
that answers in the same microtask never lets that timer fire, so none of these
three can be seen without real time on the clock.

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

**What two of them answer.** The memory mints a dashboard link as a *path*
whenever its operator has not declared the address it is reached at, so the host
puts the dashboard origin in front of it and `mwe_dashboard_link` hands the agent
an address a person can open. `mwe_disambig_commit` is an ingest like any other, so it
answers with the recall block for the message it just stored, in the same
`<memory-context>` fence a turn arrives in, under one line saying it is stored;
the response's operational fields never reach the agent.

## Known limits

- **A follow-up message arriving mid-turn ends the query** and starts a fresh
  one, so it is a little slower than nanoclaw's default of pushing it into the
  live stream. That is the price of every turn having its own ingest and recall.
  The exception is a turn delivering memory notices, which runs to its end
  first — so a message sent during one waits that turn out.
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
