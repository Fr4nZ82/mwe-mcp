---
name: add-mwe-memory
description: Give this NanoClaw's agents persistent, governed memory from an mwe-mcp server. Every turn is remembered and recalled mechanically, per speaker, with the built-in memory tree switched off and no session carried between turns.
---

# Add mwe-mcp memory

[mwe-mcp](https://github.com/Fr4nZ82/mwe-mcp) is a memory server: Markdown
wikis, one per person, with per-fragment access control, served over MCP. This
skill wires NanoClaw to it so that memory stops being something the model
remembers to do and becomes something the turn does.

What changes for an agent in a group carrying the `mwe` plugin:

- **Every message is stored, and every turn arrives with what it calls up.**
  One `wiki_ingest_message` per turn, the recall block at the top of the
  prompt. No save tool, no recall tool, no holes where the model forgot.
- **Each person is remembered as themselves.** The chat sender is mapped to
  their own memory; somebody the server does not know speaks as `guest`, whose
  turns store nothing.
- **The built-in memory tree is off.** `memory/index.md` is not created and
  the session-start hook injects nothing: a second, ungoverned store would go
  stale, skip per-reader redaction and give the model a place to look instead
  of the recall block. A scaffold an earlier boot already wrote is removed
  when it is still NanoClaw's untouched templates, and NanoClaw's shared
  `CLAUDE.md` reaches the group without the sections that teach that store.
- **The agent's name comes from the people it serves.** NanoClaw puts a
  configured name at the top of the system prompt; a memory agent is not
  given one there. Its name is a fact of the memory, in the recall block's
  `WHO YOU ARE`, and until somebody has given it one it does not have one.
- **No session is carried between turns.** Each turn opens a fresh provider
  query whose conversational context is the recent window plus the recall
  block, so nothing ever needs compacting and no summary is ever written.
- **The memory can start a conversation.** When it stores something that
  belongs to somebody else, or a commitment comes due, the agent delivers it
  to that person's own chat.

Everything else NanoClaw gives an agent — chat, the web, its own container,
scheduled tasks — is untouched.

## Before you start

You need a running mwe-mcp server, and from its dashboard:

- a **consumer token** for this NanoClaw (Users → Tokens → a standard
  consumer), and
- a **delegation** for every person this NanoClaw will speak for, plus the
  `guest` pseudo-identity ticked. Without the `guest` delegation an
  unrecognised sender's turn is refused rather than answered anonymously.

Note the MCP endpoint (`https://…/mcp`, or `http://127.0.0.1:8742/mcp` when the
server is on this machine), your own chat id on the channel you use, and your
own mwe user id.

### The endpoint

The MCP endpoint of the memory server, ending in `/mcp`.

```nc:prompt mwe_server_url validate:^https?://\S+/mcp$ normalize:trim
What is the mwe-mcp endpoint? (e.g. http://127.0.0.1:8742/mcp)
```

### Who is installing this

The bridge attributes each turn to the person who sent it, so it needs to know
who you are on both sides. The chat side is `<channel>:<your platform id>` —
`telegram:123456789`, `discord:987…`. It is the same id NanoClaw stores in its
`users` table and the same one `NANOCLAW_ADMIN_USER_IDS` uses.

```nc:prompt mwe_admin_sender validate:^[a-z0-9-]+:\S+$ normalize:trim
Your own chat id, as <channel>:<platform id> (e.g. telegram:123456789)
```

The memory side is your mwe user id: lowercase letters and digits, no
underscores and no hyphens.

```nc:prompt mwe_admin_user validate:^[a-z0-9]+$ normalize:lower
Your mwe user id (lowercase letters and digits, e.g. alice)
```

## Install the bridge

### 1. The host module

The memory server is reached from the host, never from inside an agent
container: NanoClaw refuses a container whose environment carries a
secret-shaped value, so the bearer token stays on this side of the boundary and
the container asks the host to act for it. This is that host side — the MCP
client, the configuration, which groups carry the memory, the shared
`CLAUDE.md` they receive, the per-turn handler, the reverse channel and the
registration that wires them in — plus the tests that guard them.

```nc:copy
host/config.ts -> src/modules/mwe/config.ts
host/groups.ts -> src/modules/mwe/groups.ts
host/base.ts -> src/modules/mwe/base.ts
host/client.ts -> src/modules/mwe/client.ts
host/turn.ts -> src/modules/mwe/turn.ts
host/events.ts -> src/modules/mwe/events.ts
host/index.ts -> src/modules/mwe/index.ts
mwe-host.test.ts -> src/modules/mwe/mwe-host.test.ts
mwe-wiring.test.ts -> src/mwe-wiring.test.ts
```

### 2. The container module

The agent-runner side: the line to the host, the recent window, the recall
block, and the three tools the agent can call.

```nc:copy
container/active.ts -> container/agent-runner/src/mwe/active.ts
container/host-call.ts -> container/agent-runner/src/mwe/host-call.ts
container/window.ts -> container/agent-runner/src/mwe/window.ts
container/state.ts -> container/agent-runner/src/mwe/state.ts
container/block.ts -> container/agent-runner/src/mwe/block.ts
container/turn.ts -> container/agent-runner/src/mwe/turn.ts
container/tools.ts -> container/agent-runner/src/mcp-tools/mwe.ts
mwe-turn.test.ts -> container/agent-runner/src/mwe/mwe-turn.test.ts
```

### 3. The reach-ins

Seven of NanoClaw's own files need a line or two: the poll loop runs the turn's
memory work and opens a fresh session each time, the memory scaffold and its
session-start hook stand down, the system prompt stops naming the agent, the
`CLAUDE.md` composer hands a memory group a base without the sections that
teach an on-disk store, and two barrels pick up the new modules. Each edit is
anchored on exact upstream text and applied at most once, so running this again
after an upgrade is safe and says so.

```nc:run effect:wire
pnpm exec tsx .claude/skills/add-mwe-memory/apply-fork-patches.ts
```

### 4. The memory tree an earlier boot left behind

If a group was stamped and woken before this skill was applied — the order the
README recommends — its first container already copied NanoClaw's three memory
templates into `groups/<folder>/memory/`. The switch stops them being written
again; it does not remove the copy. This does, for every group carrying the
plugin, and **only** when the tree is still those three files byte for byte.
Anything else is somebody's work: it is kept and named, and yours to read and
delete.

```nc:run effect:wire
pnpm exec tsx .claude/skills/add-mwe-memory/clear-memory-scaffold.ts
```

### 5. The configuration

`mwe.json` at the fork root holds the endpoint and who is who. An existing file
is merged into, so a senderMap you have been building up survives.

```nc:run effect:wire
pnpm exec tsx .claude/skills/add-mwe-memory/seed-config.ts {{mwe_server_url}} {{mwe_admin_sender}} {{mwe_admin_user}}
```

Add one `senderMap` line per person as they arrive. The key is their
`<channel>:<platform id>`, the value their mwe user id. Anyone not listed
speaks as a guest, which is the safe answer — never somebody else's identity.

### 6. The token

The bearer token is the one thing this skill will not touch: it is not an
argument, it is not logged, and it never passes through an installer.

```nc:operator
Put the consumer token in the fork's `.env`, on its own line:
MWE_TOKEN=<the token you minted in the mwe-mcp dashboard>
The host reads it from there and never puts it in the environment of a child
process, so it does not reach an agent container. Keep `.env` out of version
control.
```

## Build, test and restart

```nc:run effect:build
pnpm run build
```

```nc:run effect:test
pnpm exec vitest run src/mwe-wiring.test.ts src/modules/mwe/mwe-host.test.ts
```

```nc:run effect:test
cd container/agent-runner && bun test src/mwe/mwe-turn.test.ts
```

The agent-runner source is bind-mounted into the container, so there is no
image to rebuild. Two things have to restart, though, and the second is the one
that is easy to miss.

```nc:run effect:restart
source setup/lib/install-slug.sh && systemctl --user restart $(systemd_unit)
```

On macOS the restart is `launchctl kickstart -k gui/$(id -u)/$(launchd_label)`
instead.

**A running agent container survives that.** NanoClaw does not stop a session's
container when the host service stops, and the runner is a process that read
its modules at boot — a read-only mount of the patched source does not reload
them. Until the container is replaced, the agent keeps answering with the code
as it was before: no ingest, no recall block, a session carried between turns,
and nothing in any log saying so. This stops the containers of every group
carrying the plugin; they come back on the next message.

```nc:run effect:restart
pnpm exec tsx .claude/skills/add-mwe-memory/restart-mwe-groups.ts
```

## Give an agent the memory

The skill wires the machinery; a group opts in by carrying the `mwe` plugin.
That is what the `mwe` template stamps, and it is the only switch: the persona
that explains the memory and the mechanics that provide it arrive together.

```bash
ncl groups create --template mwe --name mwe --new
```

`--name` becomes the group folder and how the agent shows up in `ncl`. It is
not what the agent answers to: that name is a fact of the memory, and anybody
it serves can tell it in chat. Then connect a channel the usual way
(`/manage-channels`, or `ncl wirings create`) and restart both halves. A group
without the plugin is an ordinary NanoClaw agent, untouched.

## Check it worked

Send the agent a message. The first thing to confirm is that you are talking to
a **new** container — an old one answers perfectly well and remembers nothing.

```bash
docker ps --filter label=nanoclaw-session --format '{{.Names}}\t{{.CreatedAt}}'
docker logs "$(docker ps -q --filter label=nanoclaw-session | head -1)" 2>&1 | grep '\[mwe\]'
```

(The label, not the name: `nanoclaw-session` is one of the four canonical
labels every session container carries, while the container's *name* is the
session driver's business and differs between them.)

`[mwe] memory is on for this agent` is the runner saying it read the patched
code and found the plugin. A container older than your restart, or one whose
log has no `[mwe]` line at all, is the pre-skill runner: stop it with
`ncl groups restart --id <agent-group-id>` and send another message.

Then the turn itself, from the two sides that record it:

```bash
journalctl --user -u nanoclaw -n 50 | grep mwe
ncl sessions list
```

A working turn logs `mwe_request` handling on the host, and the session's
outbound mailbox (`data/*/<session>/outbound.db`, table `messages_out`) carries
`system` rows whose content names `mwe_request`. No such row means the turn
never asked the memory anything.

Then ask the agent something you told it in an earlier conversation: it should
answer without being reminded. To see what it stored, ask it for your dashboard
link.

## Troubleshooting

### The agent answers, but remembers nothing

The turn is degrading, which is by design: a memory failure never kills a turn.
The reason is in the host log.

```bash
journalctl --user -u nanoclaw -n 200 | grep 'mwe_request failed'
```

`MWE_TOKEN missing from .env` and `mwe.json missing or unreadable` say exactly
what to fix. A connection error means the endpoint in `mwe.json` is wrong or
the server is down.

### Everyone is a guest

The turn is reaching the memory but no `senderMap` entry matches. Compare the
key in `mwe.json` with the id NanoClaw actually sees:

```bash
grep -n senderMap -A5 mwe.json
```

The key is `<channel>:<platform id>` — the channel name as NanoClaw spells it,
then a colon, then the raw platform id, with no spaces.

### `403 act_as_not_delegated`

The memory server has not been told this NanoClaw may speak for that person.
Tick them (and `guest`) in the consumer's delegation roster on the mwe-mcp
dashboard; the change takes effect within a minute, with no restart here.

### Notices never arrive

The reverse channel needs three things: `eventsEnabled` not set to false, a
token carrying a `consumer_id` claim, and a `senderMap` entry for the person a
notice is addressed to. An unroutable notice is retried for about ten minutes
and then logged as `UNDELIVERABLE`; the facts stay in that person's memory
either way.

### After a NanoClaw upgrade

Run this skill again. Every step reports what is already in place and only
does what is missing; if an upgrade moved one of the reach-ins, the wiring
step says which one, and `pnpm exec vitest run src/mwe-wiring.test.ts` says the
same thing from the other side.
