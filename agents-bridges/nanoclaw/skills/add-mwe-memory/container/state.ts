/**
 * mwe-mcp bridge — the turn state the MCP tools need.
 *
 * The runner and the in-process MCP server are two processes: the runner
 * drives the poll loop, and `mcp-tools/index.ts` is spawned separately to
 * serve the agent's tools. They share the session mount but no memory, so
 * anything the tools must know about the turn in flight travels through this
 * file — the same file-as-channel shape the hermes bridge uses between its
 * plugin halves.
 *
 * Two things live here:
 *
 * - **whose turn it is**, so `mwe_search` and `mwe_dashboard_link` act as the
 *   person the agent is talking to and never as somebody else;
 * - **the disambiguation waiting for an answer**, so the commit can replay the
 *   exact message the memory refused to store.
 *
 * A missing or unreadable file means "no turn state": the tools then answer
 * that they have nothing to act on, which is the truth.
 */
import fs from 'fs';
import path from 'path';

export interface DisambigState {
  text: string;
  channel: string;
  window: Array<{ role: 'user' | 'assistant'; text: string; timestamp: string }>;
}

export interface TurnState {
  /** `<channel>:<platform id>` of the person speaking this turn. */
  senderKey: string;
  /** `<channel>:<chat>` surface label. */
  channel: string;
  disambig?: DisambigState;
}

const SESSION_DIR = process.env.NANOCLAW_MWE_SESSION_DIR || '/workspace';
const STATE_FILE = 'mwe-turn.json';

function statePath(): string {
  return path.join(SESSION_DIR, STATE_FILE);
}

export function readTurnState(): TurnState | undefined {
  try {
    const parsed = JSON.parse(fs.readFileSync(statePath(), 'utf-8')) as Partial<TurnState>;
    if (typeof parsed.senderKey !== 'string' || !parsed.senderKey) return undefined;
    return { senderKey: parsed.senderKey, channel: String(parsed.channel ?? ''), disambig: parsed.disambig };
  } catch {
    return undefined;
  }
}

export function writeTurnState(state: TurnState): void {
  const target = statePath();
  const tmp = `${target}.tmp`;
  try {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(tmp, JSON.stringify(state));
    fs.renameSync(tmp, target);
  } catch (err) {
    console.error(`[mwe] could not persist the turn state: ${String(err)}`);
  }
}

/** Drop the pending disambiguation, keeping whose turn it is. */
export function clearDisambig(): void {
  const state = readTurnState();
  if (!state?.disambig) return;
  writeTurnState({ senderKey: state.senderKey, channel: state.channel });
}
