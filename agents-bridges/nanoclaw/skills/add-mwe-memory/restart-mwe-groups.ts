#!/usr/bin/env -S npx tsx
/**
 * Stop the session containers of every group carrying the `mwe` plugin.
 *
 * Restarting the host service is not enough. nanoclaw leaves a session's
 * container running when the service stops (systemd reports the docker
 * process as remaining after the unit), and the agent runner is a Bun
 * process that read its modules at boot — a read-only bind mount of
 * `container/agent-runner/src` does not reload them. So a fork patched
 * while an agent was awake keeps serving turns from the code as it was
 * before: no ingest, no recall block, and a session carried across turns,
 * with nothing in any log to say so.
 *
 * `ncl groups restart --id <group>` is nanoclaw's own answer: it kills the
 * running containers of that group, and without `--message` they come back
 * on the next message the agent receives.
 *
 *     npx tsx .claude/skills/add-mwe-memory/restart-mwe-groups.ts
 *
 * Re-running is a no-op for a group with nothing running.
 */
import { execFileSync } from 'child_process';
import path from 'path';

import { mweGroupIds, mweGroupFolders, type GroupRow } from '../../../src/modules/mwe/groups.js';

const ROOT = process.cwd();
const NCL = path.join(ROOT, 'bin/ncl');

/** Seconds to keep trying `ncl` for after a service restart. */
const SOCKET_WAIT_MS = 30_000;
const RETRY_MS = 1_000;

function ncl(args: string[]): string {
  return execFileSync(NCL, args, { cwd: ROOT, encoding: 'utf-8', stdio: ['ignore', 'pipe', 'pipe'] });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * `ncl groups list`, retried while the host socket comes back up: this
 * runs right after the service restart, and a refused connection there
 * means "not yet", not "no groups".
 */
async function listGroups(): Promise<GroupRow[]> {
  const deadline = Date.now() + SOCKET_WAIT_MS;
  let lastError = '';
  for (;;) {
    try {
      const answer = JSON.parse(ncl(['groups', 'list', '--json'])) as { ok?: boolean; data?: unknown };
      if (answer.ok && Array.isArray(answer.data)) return answer.data as GroupRow[];
      lastError = `ncl groups list answered ${JSON.stringify(answer).slice(0, 200)}`;
    } catch (err) {
      lastError = err instanceof Error ? err.message : String(err);
    }
    if (Date.now() >= deadline) throw new Error(lastError);
    await sleep(RETRY_MS);
  }
}

async function main(): Promise<number> {
  const folders = mweGroupFolders(ROOT);
  if (folders.length === 0) {
    console.log('no group carries the mwe plugin yet — nothing to restart');
    return 0;
  }
  let rows: GroupRow[];
  try {
    rows = await listGroups();
  } catch (err) {
    console.error(`FAIL could not reach the host through bin/ncl: ${err instanceof Error ? err.message : String(err)}`);
    console.error('Start the host, then run this script again — until you do, a running agent keeps serving');
    console.error('turns from the code as it was before the skill was applied.');
    return 1;
  }
  const ids = mweGroupIds(ROOT, rows);
  for (const id of ids) {
    ncl(['groups', 'restart', '--id', id]);
    console.log(`restarted containers for agent group ${id}`);
  }
  console.log(`mwe groups restarted: ${ids.length} of ${folders.length} carrying the plugin`);
  if (ids.length < folders.length) {
    console.log('A folder with no group row is a leftover directory; nanoclaw has no agent to restart for it.');
  }
  return 0;
}

process.exit(await main());
