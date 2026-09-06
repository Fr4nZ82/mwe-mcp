/**
 * mwe-mcp bridge — the per-turn contract, container side.
 *
 * One mechanical `wiki_ingest_message` per conversational turn, the recall
 * block at the head of the prompt, the consumer-owned window threaded through
 * and cut, per-sender attribution, and the agent's own reply fed back so it
 * remembers its half of the exchange. The credential lives on the host; every
 * call here crosses that boundary through `host-call.ts`.
 *
 * A turn that is not a person speaking — a scheduled task, a webhook, a
 * delivery instruction from the memory itself — is not ingested and does not
 * enter the window. Only chat is conversation.
 *
 * Degradation is absolute: every failure path below leaves the turn running
 * with no memory rather than failing it. A person talking to their assistant
 * must never see a stack trace because a server was slow.
 */
import { categorizeMessage, isSessionEcho } from '../formatter.js';
import { mweActive } from './active.js';
import type { MessageInRow } from '../db/messages-in.js';
import { TIMEZONE } from '../timezone.js';
import { assemblePrompt, renderConversation, renderRecallBlock, type IngestPayload } from './block.js';
import { callHost, type MweFrame } from './host-call.js';
import { writeTurnState } from './state.js';
import { appendToWindow, windowMessages, type WindowMessage } from './window.js';
import fs from 'fs';
import path from 'path';

/** The session mount, where the host saved this turn's incoming files. */
const SESSION_DIR = process.env.NANOCLAW_MWE_SESSION_DIR || '/workspace';

/** Content key the host sets on a delivery instruction it wrote itself. */
const NOTICE_MARKER = 'mweNotice';

/** Claude Agent SDK transcripts, one per session — see `reapSessionFiles`. */
const SDK_PROJECTS_DIR = process.env.NANOCLAW_MWE_SDK_DIR || '/home/node/.claude/projects';
const SDK_TRANSCRIPTS_KEPT = 20;

function log(msg: string): void {
  console.error(`[mwe] ${msg}`);
}

interface ParsedRow {
  row: MessageInRow;
  senderKey: string;
  text: string;
  attachments: Array<Record<string, unknown>>;
}

interface PendingTurn {
  senderKey: string;
  channel: string;
  /** The text of the turn — what a disambiguation commit has to replay. */
  text: string;
  /** The window as it was BEFORE this turn — what a replay must send again. */
  window: WindowMessage[];
  userMessages: WindowMessage[];
}

/**
 * The turn currently in flight. The loop runs one turn at a time in one
 * container, so a module-level slot is the whole bookkeeping. It is read by
 * `endTurn` in this same process; what the MCP tool process needs instead
 * goes to disk (`state.ts`).
 */
let pending: PendingTurn | undefined;

function parseContent(json: string): Record<string, unknown> {
  try {
    const parsed = JSON.parse(json) as unknown;
    return typeof parsed === 'object' && parsed !== null ? (parsed as Record<string, unknown>) : { text: json };
  } catch {
    return { text: json };
  }
}

/** The catalog `kind` for one nanoclaw attachment descriptor. */
function catalogKind(type: string): string {
  const t = type.toLowerCase();
  if (t.includes('photo') || t.includes('image')) return 'photo';
  if (t.includes('video')) return 'video';
  if (t.includes('audio') || t.includes('voice')) return 'audio';
  return 'doc';
}

/**
 * The conversational rows of a batch, in order.
 *
 * Excluded, each for its own reason: cross-session echoes (ambient copies of
 * another conversation — storing them would file the same words twice, under
 * the wrong surface), the memory's own delivery instructions, and every
 * non-chat kind.
 */
function conversationalRows(messages: MessageInRow[]): ParsedRow[] {
  const rows: ParsedRow[] = [];
  for (const row of messages) {
    if (row.kind !== 'chat' && row.kind !== 'chat-sdk') continue;
    if (isSessionEcho(row)) continue;
    const content = parseContent(row.content);
    if (content[NOTICE_MARKER] === true) continue;
    const text = String(content.text ?? '').trim();
    const attachments = Array.isArray(content.attachments)
      ? (content.attachments as Array<Record<string, unknown>>)
      : [];
    if (!text && attachments.length === 0) continue;
    rows.push({
      row,
      senderKey: categorizeMessage(row).senderId ?? '',
      text,
      attachments,
    });
  }
  return rows;
}

/** Consecutive rows from the same sender are one turn; a mixed batch is one turn each. */
function groupBySender(rows: ParsedRow[]): ParsedRow[][] {
  const groups: ParsedRow[][] = [];
  for (const row of rows) {
    const last = groups[groups.length - 1];
    if (last && last[0].senderKey === row.senderKey) last.push(row);
    else groups.push([row]);
  }
  return groups;
}

/** `<channel>:<chat>` — the surface label the memory tags this turn with. */
function channelLabel(row: MessageInRow): string {
  return `${row.channel_type ?? ''}:${row.platform_id ?? ''}`;
}

/**
 * Upload the group's media out of band and return the catalog ids.
 *
 * Bytes never ride the MCP call: the host uploads them and hands back ids
 * (per-turn contract, point 7). A guest turn stores nothing and the endpoint
 * refuses it, so the host answers `ok: false` and the turn simply carries no
 * attachments — the words are still ingested.
 */
async function uploadAttachments(group: ParsedRow[], senderKey: string): Promise<Array<Record<string, unknown>>> {
  const out: Array<Record<string, unknown>> = [];
  for (const { attachments } of group) {
    for (const attachment of attachments) {
      const localPath = String(attachment.localPath ?? '');
      if (!localPath) continue;
      const full = path.join(SESSION_DIR, localPath);
      let bytes: Buffer;
      try {
        bytes = fs.readFileSync(full);
      } catch (err) {
        log(`attachment ${localPath} unreadable (${String(err)}) — the turn goes without it`);
        continue;
      }
      const frame = await callHost('media', {
        sender: senderKey,
        bytesBase64: bytes.toString('base64'),
        filename: String(attachment.name ?? attachment.filename ?? path.basename(localPath)),
        kind: catalogKind(String(attachment.type ?? '')),
        caption: attachment.caption ? String(attachment.caption) : undefined,
      });
      if (!frame.ok) {
        log(`media upload refused (${frame.error}) — the turn goes without it`);
        continue;
      }
      const catalogId = frame.data.catalog_id;
      if (typeof catalogId === 'string') out.push({ catalog_id: catalogId });
    }
  }
  return out;
}

/**
 * Ingest one sender's turn and return the response payload.
 *
 * The window sent is the thread BEFORE this turn: the current message is the
 * `text` argument, and repeating it in `recent_messages` would show the
 * classifier the same words twice.
 */
async function ingestGroup(
  group: ParsedRow[],
  window: WindowMessage[],
): Promise<{ payload: IngestPayload; maxWindow?: number } | undefined> {
  const senderKey = group[0].senderKey;
  const attachments = await uploadAttachments(group, senderKey);
  const text = group.map((r) => r.text).filter(Boolean).join('\n') || '[media]';
  const frame: MweFrame = await callHost('ingest', {
    sender: senderKey,
    text,
    recentMessages: window,
    channel: channelLabel(group[0].row),
    timezone: TIMEZONE,
    attachments: attachments.length > 0 ? attachments : undefined,
  });
  if (!frame.ok) {
    log(`ingest failed (${frame.error}) — the turn proceeds without memory`);
    return undefined;
  }
  return { payload: frame.data as IngestPayload, maxWindow: frame.maxWindow };
}

/**
 * Run the turn's memory work and return the prompt the provider should get.
 *
 * `formatted` is what nanoclaw already built for this batch; the block and the
 * window go in front of it, never inside it.
 */
export async function beginTurn(messages: MessageInRow[], formatted: string): Promise<string> {
  pending = undefined;
  carriedNotices = countNotices(messages);
  noticeWaitLogged = false;
  if (!mweActive()) return formatted;

  const rows = conversationalRows(messages);
  if (rows.length === 0) {
    // A task, a webhook, or a delivery instruction from the memory. Nothing to
    // remember and nothing to recall: it already carries what it needs.
    return formatted;
  }

  const window = windowMessages();
  const groups = groupBySender(rows);
  const now = new Date().toISOString();
  let last: { payload: IngestPayload; maxWindow?: number } | undefined;

  // One ingest per sender, in order. A batch from one person is one turn; a
  // batch that mixes people is one turn each, so nobody's words are filed
  // under somebody else's name.
  for (const group of groups) {
    const result = await ingestGroup(group, window);
    if (result) last = result;
  }

  const userMessages: WindowMessage[] = rows.map((r) => ({
    role: 'user' as const,
    text: r.text || '[media]',
    timestamp: now,
  }));

  const lastGroup = groups[groups.length - 1];
  pending = {
    senderKey: lastGroup[0].senderKey,
    channel: channelLabel(lastGroup[0].row),
    text: lastGroup.map((r) => r.text).filter(Boolean).join('\n') || '[media]',
    window,
    userMessages,
  };
  // Whose turn it is, for the tools in the other process — and, when the
  // memory asked one, the disambiguation they may commit.
  writeTurnState({
    senderKey: pending.senderKey,
    channel: pending.channel,
    disambig: last?.payload.needs_disambig
      ? { text: pending.text, channel: pending.channel, window }
      : undefined,
  });

  // The window advances now: the next turn's ingest must see this one.
  appendToWindow(userMessages, last?.maxWindow);

  if (!last) return formatted;
  // The recall block shown is the one for the person who spoke last — the one
  // the agent is answering. Two identity cards in one prompt would tell the
  // model two different things about who it is talking to.
  return assemblePrompt(renderRecallBlock(last.payload), renderConversation(window), formatted);
}

/** The `<message to="…">` bodies the agent delivered this turn. */
function deliveredBodies(text: string): string {
  const withoutInternal = text.replace(/<internal\b[\s\S]*?<\/internal>/gi, '');
  const bodies: string[] = [];
  const re = /<message\s+to="[^"]+"\s*>([\s\S]*?)<\/message>/g;
  let match: RegExpExecArray | null;
  while ((match = re.exec(withoutInternal)) !== null) {
    const body = match[1].trim();
    if (body) bodies.push(body);
  }
  return bodies.join('\n\n');
}

/**
 * How many of the memory's own delivery instructions this turn is carrying.
 *
 * A person's message is safe to interrupt: it was ingested before the query
 * opened and it sits in the recent window, so the turn that follows still has
 * it. **A delivery instruction is not.** Nothing stores it — the daemon acked
 * it to the server the moment it was enqueued — so a turn dropped halfway
 * through a batch of them loses the ones it had not spoken yet. The poll loop
 * asks this before ending a query for a follow-up.
 */
let carriedNotices = 0;

function countNotices(messages: MessageInRow[]): number {
  let count = 0;
  for (const row of messages) {
    if (row.kind !== 'chat' && row.kind !== 'chat-sdk') continue;
    if (parseContent(row.content)[NOTICE_MARKER] === true) count++;
  }
  return count;
}

let noticeWaitLogged = false;

/**
 * May the poll loop end this query because a follow-up arrived?
 *
 * Yes for an ordinary turn: the follow-up needs an ingest and a recall block
 * of its own, and what this turn was serving survives being dropped. No while
 * the turn is carrying notices, which nothing can give back — the follow-up is
 * still pending when the turn ends, so it costs one turn's wait and no more.
 *
 * The poller asks every 500 ms, so the wait is said once.
 */
export function mayEndForFollowUp(): boolean {
  if (carriedNotices === 0) return true;
  if (!noticeWaitLogged) {
    noticeWaitLogged = true;
    log(`follow-up arrived while delivering ${carriedNotices} memory notice(s) — it waits for this turn to finish`);
  }
  return false;
}

/**
 * Feed the agent's own reply back for extraction, so it remembers its half of
 * the turn — a deadline it worked out, advice it gave, a decision reached.
 *
 * `author=assistant` tells the memory to keep only the durable sediment and
 * to attribute it to the agent rather than to the person. The reply has
 * already gone out by the time this runs, so the cost is the next turn's
 * start, never this turn's answer.
 *
 * What it sees is the turn's `<message to="…">` blocks. An agent that answers
 * only through the `send_message` tool leaves no block in its result text, and
 * that reply is not fed back — the user's half of the turn is stored either
 * way, so the memory is thinner, never wrong.
 */
export async function endTurn(resultText: string | null): Promise<void> {
  // The turn reached its boundary: whatever it was carrying has been spoken.
  carriedNotices = 0;
  const turn = pending;
  pending = undefined;
  if (!turn || !mweActive()) return;
  const reply = deliveredBodies(resultText ?? '').trim();
  if (!reply) return;

  const now = new Date().toISOString();
  const frame = await callHost('ingest', {
    sender: turn.senderKey,
    text: reply,
    author: 'assistant',
    recentMessages: [...turn.window, ...turn.userMessages],
    channel: turn.channel,
    timezone: TIMEZONE,
  });
  if (!frame.ok) log(`assistant-turn ingest failed (${frame.error}) — the reply stays out of memory`);
  appendToWindow([{ role: 'assistant', text: reply, timestamp: now }], frame.ok ? frame.maxWindow : undefined);
  reapSessionFiles();
}

/**
 * Keep the SDK transcript directory from growing without bound.
 *
 * A fresh provider session per turn means a fresh transcript per turn, and
 * the provider's own rotation only ever runs on a resumed session — which
 * never happens here. Nothing reads a past transcript under this model, so
 * the newest few are kept for `/upload-trace` and debugging and the rest go.
 */
function reapSessionFiles(): void {
  try {
    if (!fs.existsSync(SDK_PROJECTS_DIR)) return;
    for (const project of fs.readdirSync(SDK_PROJECTS_DIR)) {
      const dir = path.join(SDK_PROJECTS_DIR, project);
      if (!fs.statSync(dir).isDirectory()) continue;
      const transcripts = fs
        .readdirSync(dir)
        .filter((name) => name.endsWith('.jsonl'))
        .map((name) => {
          const full = path.join(dir, name);
          return { full, mtime: fs.statSync(full).mtimeMs };
        })
        .sort((a, b) => b.mtime - a.mtime);
      for (const stale of transcripts.slice(SDK_TRANSCRIPTS_KEPT)) fs.rmSync(stale.full, { force: true });
    }
  } catch (err) {
    log(`could not prune old session transcripts: ${String(err)}`);
  }
}
