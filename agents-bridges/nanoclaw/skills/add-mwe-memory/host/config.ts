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
 */
export function loadMweConfig(root: string): MweConfig | null {
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
  for (const [key, value] of Object.entries(asRecord(obj.senderMap))) {
    const sender = asString(value);
    if (key.trim() && sender) senderMap[key.trim()] = sender;
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

/** Base URL for dashboard links: the declared origin, else `serverUrl` minus `/mcp`. */
export function dashboardOrigin(config: MweConfig): string {
  return config.dashboardUrl || config.serverUrl.replace(/\/mcp\/?$/, '');
}
