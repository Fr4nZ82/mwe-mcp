#!/usr/bin/env -S npx tsx
/**
 * The edits `add-mwe-memory` makes to nanoclaw's own files.
 *
 * Everything else the skill installs is a whole new file. These are splices
 * into upstream code, across seven files, so they live here rather than in
 * prose: each is an exact anchor plus a replacement carrying an `// mwe:`
 * marker, applied at most once and removable by the same script with
 * `--remove`. Run it twice and the second run changes nothing and says so;
 * that is what makes the skill survive an upgrade.
 *
 *     npx tsx .claude/skills/add-mwe-memory/apply-fork-patches.ts [--remove]
 *
 * If an anchor is gone, upstream moved the seam. The script says which one and
 * exits non-zero rather than guessing — a half-applied poll loop is worse than
 * an unapplied one.
 */
import fs from 'fs';
import path from 'path';

const ROOT = process.cwd();

interface Patch {
  file: string;
  /** Exact upstream text this patch keys on. */
  anchor: string;
  /** What the anchor becomes. Carries an `// mwe:` comment so a reader of the
   * fork can see at a glance which lines are the bridge's.
   *
   * Both fields are template literals, so a backtick in the code or in a
   * comment has to be escaped (`\``) — an unescaped one ends the literal and
   * the file stops parsing. */
  replacement: string;
}

const POLL_LOOP = 'container/agent-runner/src/poll-loop.ts';
const SCAFFOLD = 'container/agent-runner/src/memory/scaffold.ts';
const MEMORY_HOOK = 'container/agent-runner/src/memory/hook.ts';
const DESTINATIONS = 'container/agent-runner/src/destinations.ts';
const MCP_BARREL = 'container/agent-runner/src/mcp-tools/index.ts';
const HOST_BARREL = 'src/modules/index.ts';
const CLAUDE_MD_COMPOSE = 'src/claude-md-compose.ts';

const PATCHES: Patch[] = [
  // 1. The memory tree. The scaffold is called unconditionally from the
  // runner's main(), and a wiring test pins that call, so the switch goes
  // inside the function instead of around the call.
  {
    file: SCAFFOLD,
    anchor: `import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';`,
    replacement: `import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

import { mweActive } from '../mwe/active.js'; // mwe: the memory switch`,
  },
  {
    file: SCAFFOLD,
    anchor: `export function ensureMemoryScaffold(baseDir = '/workspace/agent'): void {
  const memoryDir = path.join(baseDir, 'memory');`,
    replacement: `export function ensureMemoryScaffold(baseDir = '/workspace/agent'): void {
  // mwe: the memory server is this agent's only memory. A second tree on disk
  // would go stale, skip per-reader redaction, and give the model a place to
  // look instead of the recall block.
  if (mweActive()) return;
  const memoryDir = path.join(baseDir, 'memory');`,
  },
  // 2. The session-start hook. It stays registered — the Claude provider
  // throws without it — and simply emits nothing.
  {
    file: MEMORY_HOOK,
    anchor: `const source = readSource();
const context = source ? memoryContextForSessionStart(source, process.argv[2]) : undefined;
if (context) console.log(context);`,
    replacement: `// mwe: with the memory server on there is no local tree to inject, and the
// recall block already arrives with every turn.
const { mweActive } = await import('../mwe/active.js');
const source = mweActive() ? undefined : readSource();
const context = source ? memoryContextForSessionStart(source, process.argv[2]) : undefined;
if (context) console.log(context);`,
  },
  // 3. The agent's name. nanoclaw injects a configured one at the top of
  // every system prompt; with the memory on, the name is a fact the memory
  // holds and the people it serves give, so the section is not injected.
  {
    file: DESTINATIONS,
    anchor: `import { getAgentMailbox } from './mailbox/index.js';
import type { Destination } from './mailbox/types.js';`,
    replacement: `import { getAgentMailbox } from './mailbox/index.js';
import type { Destination } from './mailbox/types.js';

import { mweActive } from './mwe/active.js'; // mwe: the memory switch`,
  },
  {
    file: DESTINATIONS,
    anchor: `  const sections: string[] = [];

  if (assistantName) {`,
    replacement: `  const sections: string[] = [];

  // mwe: the agent's name is a fact of the memory — the \`WHO YOU ARE\`
  // section of the recall block, given by the people it serves. A name
  // configured on this side would name it before anybody had.
  if (assistantName && !mweActive()) {`,
  },
  // 4. The shared CLAUDE.md. The base teaches an on-disk memory tree and a
  // conversations/ folder to recall from; for a memory group those are
  // instructions to look where the persona forbids, and they are composed
  // AFTER the persona, where a model reads them as the later word.
  {
    file: CLAUDE_MD_COMPOSE,
    anchor: `import { readGroupPersona } from './group-persona.js';
import type { AgentGroup } from './types.js';`,
    replacement: `import { readGroupPersona } from './group-persona.js';
import { mweSharedBase } from './modules/mwe/base.js'; // mwe: the base a memory group gets
import type { AgentGroup } from './types.js';`,
  },
  {
    file: CLAUDE_MD_COMPOSE,
    anchor: `  const sharedLink = path.join(groupDir, '.claude-shared.md');
  syncSymlink(sharedLink, SHARED_CLAUDE_MD_CONTAINER_PATH);`,
    replacement: `  const sharedLink = path.join(groupDir, '.claude-shared.md');
  // mwe: a group carrying the plugin gets the base with its memory sections
  // removed, written where the symlink would have gone. Recomposed on every
  // spawn, so it cannot drift from the fork's own base; a group without the
  // plugin keeps the symlink and the base entire.
  const mweBase = mweSharedBase(process.cwd(), groupDir);
  if (mweBase === null) syncSymlink(sharedLink, SHARED_CLAUDE_MD_CONTAINER_PATH);
  else writeAtomic(sharedLink, mweBase);`,
  },
  // 5. The per-turn contract: the ingest and the recall block go in front of
  // the batch nanoclaw already formatted, and the turn opens a fresh provider
  // session every time.
  {
    file: POLL_LOOP,
    anchor: `import type { AgentProvider, AgentQuery, ProviderEvent, ProviderExchange } from './providers/types.js';`,
    replacement: `import type { AgentProvider, AgentQuery, ProviderEvent, ProviderExchange } from './providers/types.js';
import { beginTurn, endTurn, mayEndForFollowUp } from './mwe/turn.js'; // mwe: the per-turn contract
import { mweStateless } from './mwe/active.js';
import { clearWindow } from './mwe/window.js';`,
  },
  {
    file: POLL_LOOP,
    anchor: `    const prompt = formatMessagesWithCommands(keep, config.provider.supportsNativeSlashCommands);`,
    replacement: `    // mwe: one ingest for this batch, then the recall block and the recent
    // window in front of it. Returns the formatted batch unchanged when the
    // memory is off or unreachable.
    const prompt = await beginTurn(keep, formatMessagesWithCommands(keep, config.provider.supportsNativeSlashCommands));`,
  },
  {
    file: POLL_LOOP,
    anchor: `    const query = config.provider.query({
      prompt,
      continuation,
      cwd: config.cwd,
      systemContext: config.systemContext,
    });`,
    replacement: `    const query = config.provider.query({
      prompt,
      // mwe: the sessionless model. A turn's conversational context is the
      // bridge's own window plus the recall block, so no continuation is ever
      // carried across turns and no compaction pass has anything to compact.
      continuation: mweStateless() ? undefined : continuation,
      cwd: config.cwd,
      systemContext: config.systemContext,
    });`,
  },
  {
    file: POLL_LOOP,
    anchor: `      if (result.continuation && result.continuation !== continuation) {
        continuation = result.continuation;
        setContinuation(config.providerName, continuation);
      }`,
    replacement: `      // mwe: nothing to remember about a session that is never resumed.
      if (!mweStateless() && result.continuation && result.continuation !== continuation) {
        continuation = result.continuation;
        setContinuation(config.providerName, continuation);
      }`,
  },
  {
    file: POLL_LOOP,
    anchor: `        // container died between \`init\` and \`result\`, the SDK session was
        // effectively orphaned and the next message started a blank
        // Claude session with no prior context.
        setContinuation(providerName, event.continuation);`,
    replacement: `        // container died between \`init\` and \`result\`, the SDK session was
        // effectively orphaned and the next message started a blank
        // Claude session with no prior context.
        // mwe: the stored id would be resumed at the next container start,
        // which is exactly what the sessionless model must not do.
        if (!mweStateless()) setContinuation(providerName, event.continuation);`,
  },
  {
    file: POLL_LOOP,
    anchor: `        // Accumulated context must not engage a warm query by itself.
        if (!newMessages.some((m) => m.trigger === 1)) return;

        const newIds = newMessages.map((m) => m.id);`,
    replacement: `        // Accumulated context must not engage a warm query by itself.
        if (!newMessages.some((m) => m.trigger === 1)) return;

        // mwe: a follow-up is a new turn, and a new turn is a new query with
        // its own ingest and recall block. Pushing it into the live stream
        // would hand the model a message the memory never saw. So end this
        // query and leave the rows pending and unclaimed — the outer loop
        // picks them up on its next pass, exactly as it does a slash command.
        // It sits here, after the poller has found a real trigger=1 follow-up,
        // and not at the top: asked earlier it would fire on the poller's own
        // schedule and end every turn before the agent had written a word.
        //
        // mayEndForFollowUp() is what says no: a turn carrying the memory's own
        // delivery instructions must not be dropped, because nothing stores one
        // and the daemon acked it when it enqueued it. The follow-up is still
        // pending when the turn ends, so it costs one turn's wait.
        if (mweStateless()) {
          if (!mayEndForFollowUp()) return;
          log('mwe: follow-up arrived — ending this query so the next turn gets its own ingest and recall');
          endedForCommand = true;
          query.abort();
          return;
        }

        const newIds = newMessages.map((m) => m.id);`,
  },
  // A session on disk is a session carried between turns, so the stateless
  // model has to drop one at startup as well as never write one: a container
  // that outlived a host restart, or a group wired after its first turns,
  // both leave one behind.
  {
    file: POLL_LOOP,
    anchor: `  let continuation: string | undefined = migrateLegacyContinuation(config.providerName);`,
    replacement: `  let continuation: string | undefined = migrateLegacyContinuation(config.providerName);

  // mwe: a stored session would be resumed here and hand the model the very
  // transcript the recall block replaces. Dropped once, at startup, so "no
  // session is carried between turns" also holds for the first turn after a
  // container is replaced.
  if (mweStateless() && continuation) {
    log('mwe: dropping a stored session — a turn is the recall block plus the recent window');
    clearContinuation(config.providerName);
    continuation = undefined;
  }`,
  },
  {
    file: POLL_LOOP,
    anchor: `        midTurnSent = 0;
        turnStartSeq = maxOutboundSeq();
        midTurnTail = '';`,
    replacement: `        // mwe: at the turn boundary, once the reply has gone out, feed it back
        // for extraction so the agent remembers its own half of the exchange —
        // a deadline it worked out, advice it gave. The host round trip costs
        // the next turn's start, never this turn's answer.
        await endTurn(event.text);
        midTurnSent = 0;
        turnStartSeq = maxOutboundSeq();
        midTurnTail = '';`,
  },
  {
    file: POLL_LOOP,
    anchor: `        log('Clearing session (resetting continuation)');
        continuation = undefined;
        clearContinuation(config.providerName);`,
    replacement: `        log('Clearing session (resetting continuation)');
        continuation = undefined;
        clearContinuation(config.providerName);
        clearWindow(); // mwe: a clean slate means the recent window too`,
  },
  // 6. The two barrels.
  {
    file: MCP_BARREL,
    anchor: `import './self-mod.js';`,
    replacement: `import './self-mod.js';
import './mwe.js'; // mwe: mwe_search, mwe_dashboard_link, mwe_disambig_commit`,
  },
  {
    file: HOST_BARREL,
    anchor: `import './self-mod/index.js';`,
    replacement: `import './self-mod/index.js';
import './mwe/index.js'; // mwe: the memory host module and its reverse channel`,
  },
];

function read(file: string): string {
  return fs.readFileSync(path.join(ROOT, file), 'utf-8');
}

function write(file: string, content: string): void {
  fs.writeFileSync(path.join(ROOT, file), content);
}

function apply(remove: boolean): number {
  let changed = 0;
  let already = 0;
  for (const patch of PATCHES) {
    let source: string;
    try {
      source = read(patch.file);
    } catch {
      console.error(`FAIL ${patch.file}: not found — is the working directory the fork root?`);
      return 1;
    }
    // One question decides both directions: is the replacement in the file?
    // It must be the replacement and not the anchor, because a replacement
    // usually CONTAINS its anchor — keying on the anchor would re-apply an
    // applied patch and duplicate every added line.
    const isApplied = source.includes(patch.replacement);
    if (remove ? !isApplied : isApplied) {
      already++;
      continue;
    }
    const from = remove ? patch.replacement : patch.anchor;
    const to = remove ? patch.anchor : patch.replacement;
    if (!source.includes(from)) {
      console.error(
        `FAIL ${patch.file}: the anchor is gone. Upstream moved this seam; ` +
          `re-derive the patch instead of forcing it.\n--- expected ---\n${from}\n----------------`,
      );
      return 1;
    }
    write(patch.file, source.replace(from, to));
    changed++;
  }
  console.log(
    `${remove ? 'removed' : 'applied'}: ${changed} edit(s)` + (already > 0 ? `, ${already} already in place` : ''),
  );
  return 0;
}

const remove = process.argv.includes('--remove');
process.exit(apply(remove));
