#!/usr/bin/env -S npx tsx
/**
 * Remove the memory scaffold nanoclaw created before this skill was applied.
 *
 * `ensureMemoryScaffold` runs at every container boot and copies three
 * template files into `groups/<folder>/memory/`. Applying the skill after
 * the setup wizard — the order the README recommends — therefore finds
 * that tree already there: the switch stops it being *re*created, but the
 * copy already on disk stays, and an agent told never to look in
 * `memory/` finds a `memory/` to look in.
 *
 * A directory is removed only when it holds exactly nanoclaw's three
 * templates and each is byte-identical to the template it came from.
 * Anything else — a file the agent wrote, an edited definition, an extra
 * folder — is somebody's work: it is kept, named, and left for the
 * operator, because deleting a memory that was never ours is the one
 * mistake this script must not make.
 *
 *     npx tsx .claude/skills/add-mwe-memory/clear-memory-scaffold.ts
 *
 * Re-running changes nothing once the trees are gone.
 */
import fs from 'fs';
import path from 'path';

import { mweGroupFolders, whyMemoryTreeIsNotPristine } from '../../../src/modules/mwe/groups.js';

const ROOT = process.cwd();
const TEMPLATES_DIR = path.join(ROOT, 'container/agent-runner/src/memory/templates');

function main(): number {
  if (!fs.existsSync(TEMPLATES_DIR)) {
    console.error(`FAIL ${TEMPLATES_DIR}: not found — is the working directory the fork root?`);
    return 1;
  }
  const folders = mweGroupFolders(ROOT);
  let removed = 0;
  let kept = 0;
  for (const folder of folders) {
    const memoryDir = path.join(ROOT, 'groups', folder, 'memory');
    if (!fs.existsSync(memoryDir)) continue;
    const reason = whyMemoryTreeIsNotPristine(memoryDir, TEMPLATES_DIR);
    if (reason) {
      kept++;
      console.log(`kept  groups/${folder}/memory — ${reason}`);
      continue;
    }
    fs.rmSync(memoryDir, { recursive: true });
    removed++;
    console.log(`removed groups/${folder}/memory — nanoclaw's untouched scaffold`);
  }
  console.log(
    `memory scaffold: ${removed} removed, ${kept} kept, across ${folders.length} group(s) carrying the plugin`,
  );
  if (kept > 0) {
    console.log('A kept tree is yours to read and delete — the memory server is this agent\'s only memory.');
  }
  return 0;
}

process.exit(main());
