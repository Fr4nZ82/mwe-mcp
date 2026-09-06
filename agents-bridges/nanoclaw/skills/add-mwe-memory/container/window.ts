/**
 * mwe-mcp bridge — the consumer-owned recent window.
 *
 * The memory keeps no server-side transcript, so the bridge owns the last few
 * messages of the conversation and hands them to every ingest (per-turn
 * contract, point 2). Under the sessionless model the same window is also the
 * only conversational continuity the agent has: each turn opens a fresh
 * provider query, so what the model sees of the exchange is this window plus
 * the recall block — never a summary, because recall replaces the summary.
 *
 * It lives on disk beside the session (`/workspace/mwe-window.json`, the
 * session mount) so a container restart does not amputate the thread. Writes
 * are atomic through a temp file: a half-written window would be read back as
 * a corrupt one every turn after.
 *
 * The bound is the operator's `maxWindow`, which rides back on every host
 * answer — there is no second copy of the setting on this side.
 */
import fs from 'fs';
import path from 'path';

export interface WindowMessage {
  role: 'user' | 'assistant';
  text: string;
  timestamp: string;
}

export const DEFAULT_MAX_WINDOW = 16;

/**
 * The session mount. The group workspace (`/workspace/agent`) is shared by
 * every session of a group; the window belongs to one conversation, so it
 * lives one level up, beside the mailbox.
 */
const SESSION_DIR = process.env.NANOCLAW_MWE_SESSION_DIR || '/workspace';
const WINDOW_FILE = 'mwe-window.json';

interface WindowFile {
  maxWindow: number;
  messages: WindowMessage[];
}

function windowPath(): string {
  return path.join(SESSION_DIR, WINDOW_FILE);
}

function log(msg: string): void {
  console.error(`[mwe] ${msg}`);
}

/** Read the window. An unreadable file degrades to an empty window, never to a throw. */
function readWindow(): WindowFile {
  try {
    const parsed = JSON.parse(fs.readFileSync(windowPath(), 'utf-8')) as Partial<WindowFile>;
    const messages = Array.isArray(parsed.messages)
      ? parsed.messages.filter(
          (m): m is WindowMessage =>
            !!m && (m.role === 'user' || m.role === 'assistant') && typeof m.text === 'string',
        )
      : [];
    const maxWindow = Number.isFinite(parsed.maxWindow) ? Math.max(2, Number(parsed.maxWindow)) : DEFAULT_MAX_WINDOW;
    return { maxWindow, messages: messages.slice(-maxWindow) };
  } catch {
    return { maxWindow: DEFAULT_MAX_WINDOW, messages: [] };
  }
}

function writeWindow(file: WindowFile): void {
  const target = windowPath();
  const tmp = `${target}.tmp`;
  try {
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(tmp, JSON.stringify(file));
    fs.renameSync(tmp, target);
  } catch (err) {
    log(`could not persist the recent window (${String(err)}) — this turn's history stays in memory only`);
  }
}

/** The window as the ingest tool takes it, oldest first. */
export function windowMessages(): WindowMessage[] {
  return readWindow().messages;
}

/** Append messages and cut to the bound. `maxWindow` updates when the host names a new one. */
export function appendToWindow(messages: WindowMessage[], maxWindow?: number): void {
  const current = readWindow();
  const bound = maxWindow && Number.isFinite(maxWindow) ? Math.max(2, maxWindow) : current.maxWindow;
  const next = [...current.messages, ...messages].slice(-bound);
  writeWindow({ maxWindow: bound, messages: next });
}

/** Drop the window — the `/clear` path, where the person asked for a clean slate. */
export function clearWindow(): void {
  try {
    fs.rmSync(windowPath(), { force: true });
  } catch (err) {
    log(`could not clear the recent window: ${String(err)}`);
  }
}
