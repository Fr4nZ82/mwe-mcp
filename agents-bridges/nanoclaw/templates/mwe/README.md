# `mwe` — the memory-first assistant template

The default consumer agent of an [mwe-mcp](https://github.com/Fr4nZ82/mwe-mcp)
memory server, as a nanoclaw agent template (Agent Plugins 1.0.0).

Stamp it into a nanoclaw fork:

```bash
ncl groups create --template mwe --name mwe --new
```

`--name` is yours to choose — it becomes the group folder and how the agent
shows up in `ncl`. It is **not** what the agent answers to: the bridge stops
nanoclaw injecting a configured name, so the agent's name is a fact of the
memory (`WHO YOU ARE`) that anybody it serves can give it, in chat. The product
imposes no character either: it is the household's assistant, not a brand.

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

Both arrive from the same bridge, and either order works:

- **Template first** is what nanoclaw's setup wizard does — it stamps the first
  agent before you have a skill to apply. Applying the skill afterwards clears
  the memory tree that first boot wrote, so this is the ordinary path.
- **Skill first** on a fork that already runs: apply it, then stamp the group.

Either way the skill's last step restarts the agent containers as well as the
service: a container that keeps running answers with the code it booted with.
`groups` in `mwe.json` stays empty — it is an opt-*out* list, and what makes a
group ask for the memory is this plugin.

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
