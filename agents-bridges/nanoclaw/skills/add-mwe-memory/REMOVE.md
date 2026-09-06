# Remove mwe-mcp memory

Every step is idempotent — safe to run even if some were never applied.

## 1. Undo the reach-ins

The same script that made them takes them back out, anchored on the exact text
it wrote:

```bash
pnpm exec tsx .claude/skills/add-mwe-memory/apply-fork-patches.ts --remove
```

If it reports a missing anchor, the file was hand-edited after the install;
open it and remove the lines marked `// mwe:` by hand.

## 2. Delete the modules and their tests

```bash
rm -rf src/modules/mwe container/agent-runner/src/mwe
rm -f src/mwe-wiring.test.ts container/agent-runner/src/mcp-tools/mwe.ts
```

## 3. Delete the configuration

```bash
rm -f mwe.json
```

Then remove the `MWE_TOKEN` line from `.env`, and revoke that token in the
mwe-mcp dashboard — a token that is no longer used is still a token that works.

## 4. Take the plugin off the agents that carried it

An agent group keeps its memory persona until the plugin goes:

```bash
rm -rf groups/*/plugins/mwe
rm -f groups/*/instructions.prepend.md
```

`instructions.prepend.md` is the stamped persona. Delete it only for the groups
that were stamped from the `mwe` template — a group stamped from a different
template has its own persona in that same file.

## 5. Rebuild and restart

```bash
pnpm run build
source setup/lib/install-slug.sh

# Linux
systemctl --user restart $(systemd_unit)

# macOS
launchctl kickstart -k gui/$(id -u)/$(launchd_label)
```

At the next restart the built-in memory tree comes back: `memory/index.md` and
`memory/system/definition.md` are recreated in each group's workspace and the
session-start hook injects them again.

## 6. What stays behind, and where

Removing the bridge removes the wiring, not the memory. Everything the agents
stored is on the mwe-mcp server and is reachable from its dashboard; nothing of
it was ever kept in this fork.

Two small runtime files are left in each session folder — the recent window and
the turn state:

```bash
rm -f data/v2-sessions/*/*/mwe-window.json data/v2-sessions/*/*/mwe-turn.json
```
