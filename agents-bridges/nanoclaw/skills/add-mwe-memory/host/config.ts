/**
 * mwe-mcp bridge — host configuration (`mwe.json` at the fork root).
 *
 * Deliberately free of nanoclaw imports: this file, `client.ts`, `turn.ts`
 * and `events.ts` are the portable half of the host module and are driven
 * directly by the bridge's offline smoke. `index.ts` is the only file that
 * touches nanoclaw's registries, and it is where the secret is read.
 */
import fs from 'fs';
import path from 'path';

export interface MweConfig {
  /** MCP endpoint, e.g. `http://127.0.0.1:8742/mcp`. */
  serverUrl: string;
  /** `<channel>:<platform id>` → mwe user id. Explicit entries only. */
  senderMap: Record<string, string>;
  /**
   * `<channel>:<platform id>` of the chat the operator recap goes to. Empty
   * means no recap: an operator notice waits rather than landing in a chat
   * nobody chose for it.
   */
  operatorSender: string;
  /** BCP-47 tag forwarded as `metadata.locale`; empty = the per-user server default. */
  locale: string;
  /** Messages kept in the consumer-owned recent window, per session. */
  maxWindow: number;
  /**
   * Agent groups the host serves. Empty — the normal case — means every group
   * that asks, and what makes a group ask is the `mwe` plugin being stamped
   * into it. Fill it to take one group off the memory without restarting its
   * container.
   */
  groups: string[];
  eventsEnabled: boolean;
  eventsPollSeconds: number;
  /** Public origin of the dashboard; empty = derived from `serverUrl`. */
  dashboardUrl: string;
}

const MWE_CONFIG_FILE = 'mwe.json';

/**
 * A `senderMap` key is `<channel>:<platform id>` — that is what makes a
 * chat sender resolvable back to the chat they speak in. A bare key
 * (`alice`) names no channel, so nothing can route a notice to it; it is
 * dropped at load and reported once, rather than failing on every tick of
 * the reverse channel.
 */
const SENDER_KEY = /^[a-z0-9-]+:\S+$/;

/** The act-as value for a person the host cannot resolve to an enrolled user. */
export const GUEST = 'guest';

const DEFAULT_MAX_WINDOW = 16;
const DEFAULT_POLL_SECONDS = 30;
const MIN_POLL_SECONDS = 5;

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function asString(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

function asInt(value: unknown, fallback: number): number {
  const n = typeof value === 'number' ? value : Number.parseInt(String(value ?? ''), 10);
  return Number.isFinite(n) ? n : fallback;
}

/**
 * Read `<root>/mwe.json`. Returns null when the file is absent or unusable —
 * the bridge is then simply inactive, which is the configured-off state, not
 * an error.
 *
 * `onWarn` is how the parser reports what it dropped without importing a
 * logger. The host wiring passes one where a configuration is taken into
 * use and leaves it off on the per-request read, so a malformed file is
 * named once per host start instead of once per turn.
 */
export function loadMweConfig(root: string, onWarn?: (message: string) => void): MweConfig | null {
  let raw: unknown;
  try {
    raw = JSON.parse(fs.readFileSync(path.join(root, MWE_CONFIG_FILE), 'utf-8'));
  } catch {
    return null;
  }
  const obj = asRecord(raw);
  const serverUrl = asString(obj.serverUrl);
  if (!serverUrl) return null;

  const senderMap: Record<string, string> = {};
  const malformed: string[] = [];
  for (const [key, value] of Object.entries(asRecord(obj.senderMap))) {
    const sender = asString(value);
    const senderKey = key.trim();
    if (!senderKey || !sender) continue;
    if (!SENDER_KEY.test(senderKey)) {
      malformed.push(senderKey);
      continue;
    }
    senderMap[senderKey] = sender;
  }
  if (malformed.length > 0) {
    onWarn?.(
      `mwe: ignoring ${malformed.length} senderMap key(s) that are not <channel>:<platform id>: ` +
        `${malformed.join(', ')} — those people speak as guests until the keys are fixed`,
    );
  }

  return {
    serverUrl,
    senderMap,
    operatorSender: asString(obj.operatorSender),
    locale: asString(obj.locale),
    maxWindow: Math.max(2, asInt(obj.maxWindow, DEFAULT_MAX_WINDOW)),
    groups: Array.isArray(obj.groups) ? obj.groups.map(asString).filter(Boolean) : [],
    eventsEnabled: obj.eventsEnabled !== false,
    eventsPollSeconds: Math.max(MIN_POLL_SECONDS, asInt(obj.eventsPollSeconds, DEFAULT_POLL_SECONDS)),
    dashboardUrl: asString(obj.dashboardUrl),
  };
}

/**
 * The `X-MWE-Act-As` value for one chat sender.
 *
 * A sender with no explicit `senderMap` entry is a **guest** — never the
 * owner, never any other enrolled user. Falling back to a real identity would
 * file a stranger's words as that person's facts and hand the stranger that
 * person's recall, which is the misattribution `guest` exists to prevent
 * (INTEGRATING.md, per-turn contract, point 9).
 */
export function actAsFor(config: MweConfig, senderKey: string): string {
  return config.senderMap[senderKey.trim()] || GUEST;
}

/**
 * mwe user id → chat sender key, from `senderMap` read backwards.
 *
 * Only explicit entries qualify, and the first one per user wins. A personal
 * notice that cannot be routed waits and is then logged; it never lands in
 * somebody else's chat.
 */
export function reverseRoutes(config: MweConfig): Map<string, string> {
  const routes = new Map<string, string>();
  for (const [senderKey, user] of Object.entries(config.senderMap)) {
    if (!routes.has(user)) routes.set(user, senderKey);
  }
  return routes;
}

/** Does the host serve this agent group? An empty `groups` serves every one. */
export function groupEnabled(config: MweConfig, agentGroupId: string, groupFolder = ''): boolean {
  if (config.groups.length === 0) return true;
  return config.groups.includes(agentGroupId) || (!!groupFolder && config.groups.includes(groupFolder));
}

/**
 * How one `senderMap` key is looked up in nanoclaw's `messaging_groups`, or
 * `null` when the key names no channel and nothing can be routed to it.
 *
 * nanoclaw's adapters store their own `channelId` in `platform_id`, and for
 * Telegram that `channelId` already carries the channel: a paired chat is the
 * row `channel_type='telegram'`, `platform_id='telegram:<chat id>'`
 * (`src/channels/telegram.ts` — "platformId is telegram:<chatId>"). A
 * senderMap key is spelled the same way, so **the key is the row**. An
 * adapter whose `channelId` carries no prefix stores the bare id, so the tail
 * is offered second. Two spellings of one lookup, in the order the shipped
 * channels use them — asking for the tail alone finds nothing at all.
 */
export function chatLookup(senderKey: string): { channelType: string; platformIds: string[] } | null {
  const key = senderKey.trim();
  if (!SENDER_KEY.test(key)) return null;
  const [channelType, ...rest] = key.split(':');
  const tail = rest.join(':');
  return { channelType, platformIds: key === tail ? [key] : [key, tail] };
}

/** Base URL for dashboard links: the declared origin, else `serverUrl` minus `/mcp`. */
export function dashboardOrigin(config: MweConfig): string {
  return config.dashboardUrl || config.serverUrl.replace(/\/mcp\/?$/, '');
}
