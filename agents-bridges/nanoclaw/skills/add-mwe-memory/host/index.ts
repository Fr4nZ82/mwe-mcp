/**
 * mwe-mcp bridge — host wiring.
 *
 * The one file of the module that touches nanoclaw. It does three things:
 *
 * 1. reads `mwe.json` and the `MWE_TOKEN` secret — host-side, and only here,
 *    because the token must never reach an agent container (nanoclaw refuses
 *    a session spec carrying a secret-shaped env value, `src/drivers/types.ts`
 *    `isSecretShaped`);
 * 2. answers the container's `mwe_request` action, so the runner can run the
 *    per-turn contract without a credential of its own;
 * 3. starts the reverse-channel daemon on host start, and stops it on
 *    shutdown.
 *
 * Registration is inert: nothing here opens a socket or reads memory at
 * import time.
 */
import { TIMEZONE } from '../../config.js';
import { registerDeliveryAction } from '../../delivery.js';
import { getMessagingGroupAgents, getMessagingGroupByPlatform } from '../../db/messaging-groups.js';
import { readEnvFile } from '../../env.js';
import { unguarded } from '../../guard/index.js';
import { onHostStart, onHostShutdown } from '../../host-lifecycle.js';
import { log } from '../../log.js';
import { resolveSession, writeSessionMessage } from '../../session-manager.js';
import { chatLookup, groupEnabled, loadMweConfig, type MweConfig } from './config.js';
import { startEventsDaemon, type Delivery } from './events.js';
import { handleMweRequest, type MweOp, type MweRequestArgs } from './turn.js';

/**
 * The content marker that tells the runner not to ingest a delivery
 * instruction. Duplicated container-side in `mwe/turn.ts` — the two processes
 * share no module, so keep the spelling in step.
 */
const NOTICE_MARKER = 'mweNotice';

function token(): string {
  return readEnvFile(['MWE_TOKEN']).MWE_TOKEN ?? '';
}

function config(onWarn?: (message: string) => void): MweConfig | null {
  return loadMweConfig(process.cwd(), onWarn);
}

/**
 * Put a delivery instruction in the recipient's own chat.
 *
 * It is written as an ordinary chat row so the agent's reply travels the
 * normal outbound path back to that chat — a task row would switch the turn
 * to one-door delivery and the reply would never leave. The row carries
 * `mweNotice: true`, which is how the runner knows this is the memory
 * speaking and not a person: it is not ingested, and it does not enter the
 * recent window.
 *
 * Returns true only once the write is durable — the caller acks on that.
 */
async function enqueueDelivery(delivery: Delivery): Promise<boolean> {
  const lookup = chatLookup(delivery.senderKey);
  if (!lookup) {
    log.warn('mwe: malformed senderMap key, cannot route a notice', { senderKey: delivery.senderKey });
    return false;
  }
  let group = null;
  for (const platformId of lookup.platformIds) {
    group = await getMessagingGroupByPlatform(lookup.channelType, platformId);
    if (group) break;
  }
  if (!group) {
    log.warn('mwe: no messaging group for this chat yet — the notice waits', { senderKey: delivery.senderKey });
    return false;
  }
  // The first agent wired to the chat delivers. Several agents in one chat is
  // a fan-out, and a personal notice is not something to say twice.
  const agents = await getMessagingGroupAgents(group.id);
  const agent = agents[0];
  if (!agent) {
    log.warn('mwe: no agent wired to this chat — the notice waits', { senderKey: delivery.senderKey });
    return false;
  }
  // Thread `null`: a notice is not a reply to anything, so it opens the chat's
  // main session. Under `per-thread` that is the session with no thread, which
  // is where a fresh message from that person would land anyway.
  const { session } = await resolveSession(agent.agent_group_id, group.id, null, agent.session_mode);
  await writeSessionMessage(agent.agent_group_id, session.id, {
    id: `mwe-notice-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    kind: 'chat',
    timestamp: new Date().toISOString(),
    platformId: group.platform_id,
    channelType: group.channel_type,
    threadId: null,
    content: JSON.stringify({
      sender: 'mwe-memory',
      text: delivery.instruction,
      [NOTICE_MARKER]: true,
    }),
    trigger: true,
  });
  return true;
}

registerDeliveryAction(
  'mwe_request',
  async (content, session) => {
    const requestId = String(content.requestId ?? '');
    if (!requestId) {
      log.warn('mwe_request without a requestId', { sessionId: session.id });
      return;
    }
    const cfg = config();
    const secret = token();
    const answer = !cfg
      ? { ok: false as const, error: 'mwe.json missing or unreadable' }
      : !groupEnabled(cfg, session.agent_group_id)
        ? { ok: false as const, error: 'this agent group is not served by mwe.json `groups`' }
        : !secret
          ? { ok: false as const, error: 'MWE_TOKEN missing from .env' }
          : await handleMweRequest(
              { op: content.op as MweOp, args: (content.args ?? {}) as MweRequestArgs },
              { config: cfg, token: secret },
            );
    if (!answer.ok) log.warn('mwe_request failed', { requestId, error: answer.error, sessionId: session.id });

    // The response rides the same shape the runner's CLI bridge uses, so the
    // container finds it with the mailbox's existing `requestId` lookup and no
    // new mailbox surface is needed. `trigger: false` — an answer to a call in
    // flight must not wake the agent.
    await writeSessionMessage(session.agent_group_id, session.id, {
      id: `mwe-resp-${requestId}`,
      kind: 'system',
      timestamp: new Date().toISOString(),
      content: JSON.stringify({ type: 'mwe_response', requestId, frame: answer }),
      trigger: false,
    });
  },
  unguarded('transport envelope — identity is resolved host-side from senderMap, never from the container'),
);

let stopEvents: (() => void) | undefined;

onHostStart(() => {
  // The one read that reports what the configuration dropped: a malformed
  // senderMap is named once here, not on every turn that loads the file.
  const cfg = config((message) => log.warn(message));
  if (!cfg) return;
  const secret = token();
  if (!secret) {
    log.warn('mwe: mwe.json is present but MWE_TOKEN is missing from .env — the bridge is inactive');
    return;
  }
  stopEvents = startEventsDaemon({
    config: cfg,
    token: secret,
    enqueue: enqueueDelivery,
    log: (level, message) => log[level](`mwe-events: ${message}`),
    // A commitment falls due at a wall-clock time somebody has to act on, so
    // it is said in this install's own zone — the one its agents reason in.
    timezone: TIMEZONE,
  });
});

onHostShutdown(() => {
  stopEvents?.();
  stopEvents = undefined;
});
