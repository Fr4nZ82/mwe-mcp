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
  });

  it('sends a follow-up back through the outer loop instead of the live stream', () => {
    expect(read('container/agent-runner/src/poll-loop.ts')).toContain(
      'if (mweStateless() || pending.some((m) => isRunnerCommand(m))) {',
    );
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
