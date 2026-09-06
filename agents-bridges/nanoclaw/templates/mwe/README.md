# `mwe` — the memory-first assistant template

The default consumer agent of an [mwe-mcp](https://github.com/Fr4nZ82/mwe-mcp)
memory server, as a nanoclaw agent template (Agent Plugins 1.0.0).

Stamp it into a nanoclaw fork:

```bash
ncl groups create --template mwe --name mwe --new
```

`--name` is yours to choose — it becomes the agent's name and the group folder.
The product imposes no character: pick the name the household will use.

## What it gives the agent

The persona in `ai.nanoco.nanoclaw/context/instructions.md` is prepended to the
agent's system prompt on every spawn, under every provider. It teaches one
thing: **mwe-mcp is this agent's only memory**, capture and recall are
mechanical, and there is no file on disk to keep a second copy in.

## What it does not do on its own

The template is the persona. The mechanics — one ingest per turn, the recall
block, per-sender attribution, the reverse channel — are the
`add-mwe-memory` skill, which patches the fork. A group stamped from this
template without that skill installed is a normal nanoclaw agent that has been
told about a memory it cannot reach.

Install order, both from the same bridge:

1. the skill (`.claude/skills/add-mwe-memory`), applied to the fork,
2. this template (`templates/mwe`), stamped into a group,
3. the group id added to `groups` in `mwe.json`.

The bridge README has the full walkthrough.

## No `mcp.json`

The memory server is deliberately **not** declared as an MCP server here. Two
reasons, both structural:

- The bridge speaks for one sender per turn (`X-MWE-Act-As`), and a static
  header in `mcp.json` cannot vary per sender.
- nanoclaw refuses a plugin MCP server whose URL host reaches the container host
  (`src/templates/mcp.ts`), which is exactly where a self-hosted memory server
  sits.

`mwe_search` and `mwe_dashboard_link` are registered by the skill, on the
container's own in-process MCP server, with the speaker's act-as attached.
