/**
 * mwe-mcp bridge — the container's line to the host.
 *
 * The agent container holds no memory credential: nanoclaw refuses a session
 * spec whose env carries a secret-shaped value (`src/drivers/types.ts`,
 * `isSecretShaped`), and that refusal is the whole reason this file exists.
 * Every call to the memory is written as one `mwe_request` system action on
 * the outbound mailbox; the host answers it and writes the frame back to the
 * inbound mailbox, where the mailbox's existing `requestId` lookup finds it.
 *
 * Round trip: the host's active delivery poll runs once a second and this side
 * polls four times a second, so a call costs up to about a second and a
 * quarter.
 * The ingest sits on the turn's critical path and pays that deliberately —
 * better memory over first-token latency, the same trade the hermes bridge
 * makes.
 *
 * Same file-as-protocol shape the runner's own CLI client uses (`../cli/ncl.ts`
 * in this tree); the host and the container share no module, so keep this
 * envelope in step with `src/modules/mwe/index.ts` on the other side.
 */
import { findCliResponse, markCompleted } from '../db/messages-in.js';
import { writeMessageOut } from '../db/messages-out.js';

/** What the host may be asked to do. */
export type MweOp = 'ingest' | 'search' | 'dashboard_link' | 'media';

export type MweFrame =
  | { ok: true; data: Record<string, unknown>; actAs: string; maxWindow: number }
  | { ok: false; error: string; code?: string; actAs?: string };

const POLL_INTERVAL_MS = 250;
const DEFAULT_TIMEOUT_MS = 45_000;

function log(msg: string): void {
  console.error(`[mwe] ${msg}`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function requestId(): string {
  return `mwe-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

/**
 * Ask the host to do one thing against the memory.
 *
 * Never throws: a timeout, a broken host module or a memory outage all come
 * back as `{ ok: false }`. The callers act on that by proceeding without
 * memory — the turn is never the thing that dies.
 */
export async function callHost(
  op: MweOp,
  args: Record<string, unknown>,
  timeoutMs = DEFAULT_TIMEOUT_MS,
): Promise<MweFrame> {
  const id = requestId();
  try {
    await writeMessageOut({
      id,
      kind: 'system',
      content: JSON.stringify({ action: 'mwe_request', requestId: id, op, args }),
    });
  } catch (err) {
    return { ok: false, error: `could not reach the host: ${String(err)}` };
  }

  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL_MS);
    let row;
    try {
      row = findCliResponse(id);
    } catch (err) {
      return { ok: false, error: `mailbox read failed: ${String(err)}` };
    }
    if (!row) continue;
    markCompleted([row.id]);
    try {
      const parsed = JSON.parse(row.content) as { frame?: MweFrame };
      if (parsed.frame) return parsed.frame;
      return { ok: false, error: 'host answered without a frame' };
    } catch (err) {
      return { ok: false, error: `unreadable host answer: ${String(err)}` };
    }
  }
  log(`host did not answer "${op}" within ${Math.round(timeoutMs / 1000)}s — the turn proceeds without memory`);
  return { ok: false, error: 'the host did not answer in time' };
}
