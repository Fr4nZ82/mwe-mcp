/**
 * mwe-mcp bridge — the three tools the agent may call.
 *
 * Recall and capture are mechanical and need no tool; these are for the
 * moments a person asks for something explicitly. All three act as the
 * speaker, whose key the runner leaves in the turn state — so what comes back
 * is what **that person** is allowed to see, never more.
 *
 * They run in the MCP server process, not the runner's, which is why the
 * sender key travels through `state.ts` rather than a shared variable — and
 * why this is the one file of the bridge that lands in `mcp-tools/`, beside
 * the tools it registers with, rather than in `mwe/` with the rest.
 */
import { registerTools } from './server.js';
import type { McpToolDefinition } from './types.js';
import { renderCommitAnswer, type IngestPayload } from '../mwe/block.js';
import { callHost } from '../mwe/host-call.js';
import { clearDisambig, readTurnState } from '../mwe/state.js';

function ok(payload: unknown) {
  return { content: [{ type: 'text' as const, text: JSON.stringify(payload) }] };
}

function prose(body: string) {
  return { content: [{ type: 'text' as const, text: body }] };
}

function err(text: string) {
  return { content: [{ type: 'text' as const, text: `Error: ${text}` }], isError: true };
}

const SEARCH: McpToolDefinition = {
  tool: {
    name: 'mwe_search',
    description:
      'Search the persistent memory the current speaker is allowed to read. Recall is automatic on ' +
      'every turn — use this only when they ask you to look something up, or when the recall block ' +
      'points at more material than it brought.',
    inputSchema: {
      type: 'object',
      properties: {
        query: { type: 'string', description: 'What to search for.' },
        top_k: { type: 'number', description: 'Maximum hits (default 10, cap 50).' },
      },
      required: ['query'],
    },
  },
  handler: async (args) => {
    const state = readTurnState();
    if (!state) return err('no turn in flight — there is nobody to search as');
    const frame = await callHost('search', {
      sender: state.senderKey,
      query: String(args.query ?? ''),
      topK: typeof args.top_k === 'number' ? args.top_k : 10,
    });
    return frame.ok ? ok(frame.data) : err(frame.error);
  },
};

const DASHBOARD: McpToolDefinition = {
  tool: {
    name: 'mwe_dashboard_link',
    description:
      'Mint a short-lived link to the current speaker\'s own memory dashboard, where they can read, ' +
      'correct and steer what is remembered about them. Surface the returned URL as a link. Not ' +
      'available to a guest.',
    inputSchema: {
      type: 'object',
      properties: {
        intent: {
          type: 'string',
          enum: ['home', 'modify_wiki', 'view_wiki', 'answer_proposal', 'archive_view', 'audit', 'costs', 'settings'],
          description: 'Where the link lands (default home).',
        },
      },
      required: [],
    },
  },
  handler: async (args) => {
    const state = readTurnState();
    if (!state) return err('no turn in flight — there is nobody to mint a link for');
    const frame = await callHost('dashboard_link', {
      sender: state.senderKey,
      intent: typeof args.intent === 'string' ? args.intent : 'home',
    });
    return frame.ok ? ok(frame.data) : err(frame.error);
  },
};

const DISAMBIG: McpToolDefinition = {
  tool: {
    name: 'mwe_disambig_commit',
    description:
      'Commit the disambiguation the memory asked for: after the person picked one of the candidates ' +
      'listed in the recall block, call this with the id they chose. The message is stored only once ' +
      'this is done, and the answer is the memory for it, in the framing a recall block arrives in. ' +
      'Valid only while a disambiguation is pending.',
    inputSchema: {
      type: 'object',
      properties: { candidate_id: { type: 'string', description: 'The id the person picked.' } },
      required: ['candidate_id'],
    },
  },
  handler: async (args) => {
    const state = readTurnState();
    const pending = state?.disambig;
    if (!state || !pending) return err('no disambiguation pending');
    const candidateId = String(args.candidate_id ?? '').trim();
    if (!candidateId) return err('candidate_id required');
    // The commit is the same ingest again, this time naming the choice — that
    // second call is what actually stores the message the memory held back.
    const frame = await callHost('ingest', {
      sender: state.senderKey,
      text: pending.text,
      recentMessages: pending.window,
      channel: pending.channel,
      disambigChoice: candidateId,
    });
    clearDisambig();
    return frame.ok ? prose(renderCommitAnswer(frame.data as IngestPayload)) : err(frame.error);
  },
};

/** The three definitions, exported so the bridge's smoke drives the handlers the agent calls. */
export const MWE_TOOLS: McpToolDefinition[] = [SEARCH, DASHBOARD, DISAMBIG];

registerTools(MWE_TOOLS);
