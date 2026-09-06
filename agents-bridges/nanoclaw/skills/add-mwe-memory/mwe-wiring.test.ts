/**
 * The integration points `add-mwe-memory` splices into nanoclaw's own files.
 *
 * These are structural, not behavioural: they go red when an upgrade drops one
 * of the reach-ins, which is the failure mode a green test suite would
 * otherwise hide — the agent keeps answering, silently without memory, and
 * silently with a second memory tree on disk.
 */
import { describe, it, expect } from 'vitest';
import fs from 'fs';

import { DROPPED_BASE_SECTIONS } from './modules/mwe/base.js';

const read = (file: string): string => fs.readFileSync(file, 'utf-8');

describe('the fork carries the mwe reach-ins', () => {
  it('runs the per-turn contract before the provider query', () => {
    const pollLoop = read('container/agent-runner/src/poll-loop.ts');
    expect(pollLoop).toContain("import { beginTurn, endTurn } from './mwe/turn.js'");
    expect(pollLoop).toContain('await beginTurn(keep, formatMessagesWithCommands(');
    expect(pollLoop).toContain('await endTurn(event.text);');
  });

  it('opens a fresh provider session on every turn', () => {
    const pollLoop = read('container/agent-runner/src/poll-loop.ts');
    expect(pollLoop).toContain('continuation: mweStateless() ? undefined : continuation,');
    // Both writes must be gated: the one after the turn and the one on `init`.
    // Leaving the second would resurrect a session at the next container start.
    expect(pollLoop).toContain('if (!mweStateless()) setContinuation(providerName, event.continuation);');
    expect(pollLoop).toContain('if (!mweStateless() && result.continuation');
    // And a session already on disk is dropped, not resumed: a container that
    // outlived a host restart leaves one behind, and resuming it hands the
    // model the transcript the recall block exists to replace.
    expect(pollLoop).toContain('if (mweStateless() && continuation) {');
  });

  it('ends the query for a follow-up, and only when there is one', () => {
    const pollLoop = read('container/agent-runner/src/poll-loop.ts');
    // The guard sits AFTER the poller has decided there is a real trigger=1
    // follow-up to push. Placement is the whole behaviour: a guard that asked
    // `mweStateless()` on its own would fire on the poller's own 500ms
    // schedule and end every turn before the agent wrote a word, so the
    // ordering is asserted and the short-circuit form is denied.
    const guard = pollLoop.indexOf('if (mweStateless()) {\n          log(\'mwe: follow-up arrived');
    const decided = pollLoop.indexOf('if (!newMessages.some((m) => m.trigger === 1)) return;');
    expect(guard).toBeGreaterThan(-1);
    expect(decided).toBeGreaterThan(-1);
    expect(guard).toBeGreaterThan(decided);
    expect(pollLoop).not.toContain('if (mweStateless() || pending.some((m) => isRunnerCommand(m)))');
    // nanoclaw's own slash-command abort stays exactly as upstream wrote it.
    expect(pollLoop).toContain('if (pending.some((m) => isRunnerCommand(m))) {');
  });

  it('does not let nanoclaw name the agent', () => {
    const destinations = read('container/agent-runner/src/destinations.ts');
    expect(destinations).toContain("import { mweActive } from './mwe/active.js'");
    expect(destinations).toContain('if (assistantName && !mweActive()) {');
  });

  it('hands a memory group a shared base without the memory sections', () => {
    const compose = read('src/claude-md-compose.ts');
    expect(compose).toContain("import { mweSharedBase } from './modules/mwe/base.js'");
    expect(compose).toContain('const mweBase = mweSharedBase(process.cwd(), groupDir);');
    expect(compose).toContain('else writeAtomic(sharedLink, mweBase);');
  });

  it('names sections the shared base of nanoclaw actually has', () => {
    // The filter drops sections by heading. An upstream rename would leave the
    // heading unmatched and quietly ship the on-disk-memory doctrine back into
    // a memory agent's prompt, so the headings are pinned against the real file.
    const base = read('container/CLAUDE.md');
    for (const heading of DROPPED_BASE_SECTIONS) {
      expect(base).toContain(`## ${heading}`);
    }
  });

  it('skips the local memory tree and the session-start injection', () => {
    expect(read('container/agent-runner/src/memory/scaffold.ts')).toContain('if (mweActive()) return;');
    expect(read('container/agent-runner/src/memory/hook.ts')).toContain('mweActive() ? undefined : readSource()');
  });

  it('registers the memory tools and the host module', () => {
    expect(read('container/agent-runner/src/mcp-tools/index.ts')).toContain("import './mwe.js';");
    expect(read('src/modules/index.ts')).toContain("import './mwe/index.js';");
  });
});
