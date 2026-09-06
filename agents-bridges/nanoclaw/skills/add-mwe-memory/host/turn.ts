/**
 * mwe-mcp bridge — the host half of the per-turn contract.
 *
 * The agent container never holds the consumer token, so every call to the
 * memory crosses the host boundary as one `mwe_request` action on the
 * session's outbound mailbox. This module answers those requests: it resolves
 * the chat sender to an act-as identity through `senderMap`, calls the memory,
 * and hands the payload back.
 *
 * Identity is resolved **here**, never taken from the container: the request
 * names a `<channel>:<platform id>` chat sender and this module maps it, so a
 * caller cannot name an arbitrary mwe user id. An unmapped sender is a
 * `guest`, never the owner.
 *
 * Every failure is returned as `{ ok: false }` rather than thrown at the
 * caller: the turn's degradation contract is that a memory hiccup costs the
 * turn its memory, never the turn itself.
 */
import { MweClient, MweError } from './client.js';
import { actAsFor, GUEST, type MweConfig } from './config.js';

/** A message of the consumer-owned recent window, as the ingest tool takes it. */
export interface RecentMessage {
  role: 'user' | 'assistant';
  text: string;
  timestamp?: string;
}

export interface MweRequestArgs {
  /** `<channel>:<platform id>` of the human this call speaks for. */
  sender?: string;
  text?: string;
  author?: 'user' | 'assistant';
  recentMessages?: RecentMessage[];
  /** Surface label for `metadata.channel` (`<channel>:<chat>`). */
  channel?: string;
  timezone?: string;
  disambigChoice?: string;
  attachments?: Array<Record<string, unknown>>;
  query?: string;
  topK?: number;
  intent?: string;
  /** Media upload: base64 bytes plus the catalog `kind`. */
  bytesBase64?: string;
  filename?: string;
  kind?: string;
  mime?: string;
  caption?: string;
}

export type MweOp = 'ingest' | 'search' | 'dashboard_link' | 'media';

export interface MweRequest {
  op: MweOp;
  args: MweRequestArgs;
}

export type MweResponse =
  | {
      ok: true;
      data: Record<string, unknown>;
      actAs: string;
      /**
       * The effective recent-window bound. It rides every answer so the
       * container trims its local window to the operator's number without a
       * second copy of the setting living container-side.
       */
      maxWindow: number;
    }
  | { ok: false; error: string; code?: string; actAs?: string };

export interface TurnDeps {
  config: MweConfig;
  token: string;
  /** Client factory — the smoke substitutes a recording one. */
  clientFor?: (actAs: string) => MweClient;
}

/** One client per act-as identity, for the lifetime of the deps object. */
const pools = new WeakMap<TurnDeps, Map<string, MweClient>>();

function clientFor(deps: TurnDeps, actAs: string): MweClient {
  if (deps.clientFor) return deps.clientFor(actAs);
  let pool = pools.get(deps);
  if (!pool) {
    pool = new Map();
    pools.set(deps, pool);
  }
  let client = pool.get(actAs);
  if (!client) {
    client = new MweClient(deps.config.serverUrl, deps.token, actAs);
    pool.set(actAs, client);
  }
  return client;
}

function errorOf(err: unknown, actAs: string): MweResponse {
  if (err instanceof MweError) return { ok: false, error: err.message, code: err.code, actAs };
  return { ok: false, error: err instanceof Error ? err.message : String(err), actAs };
}

/** `metadata` every ingest variant carries. */
function turnMetadata(config: MweConfig, args: MweRequestArgs): Record<string, unknown> {
  const metadata: Record<string, unknown> = {};
  if (config.locale) metadata.locale = config.locale;
  if (args.channel) metadata.channel = args.channel;
  if (args.timezone) metadata.timezone = args.timezone;
  if (args.disambigChoice) metadata.disambig_choice = args.disambigChoice;
  return metadata;
}

async function ingest(deps: TurnDeps, args: MweRequestArgs, actAs: string): Promise<Record<string, unknown>> {
  const call: Record<string, unknown> = {
    text: args.text,
    context_hint: 'conversation',
  };
  if (args.author === 'assistant') call.author = 'assistant';
  // The window is the consumer's to own and to cut (per-turn contract,
  // point 2): trim to the operator's `maxWindow` before it goes on the wire.
  const window = (args.recentMessages ?? []).slice(-deps.config.maxWindow);
  if (window.length > 0) call.recent_messages = window;
  if (args.attachments?.length) call.attachments = args.attachments;
  const metadata = turnMetadata(deps.config, args);
  if (Object.keys(metadata).length > 0) call.metadata = metadata;
  return clientFor(deps, actAs).callTool('wiki_ingest_message', call);
}

/**
 * Answer one container request. `sender` decides the act-as identity; the two
 * write paths that a guest may not take are refused here rather than at the
 * server, so the caller gets a usable reason instead of a 403.
 */
export async function handleMweRequest(request: MweRequest, deps: TurnDeps): Promise<MweResponse> {
  const args = request.args ?? {};
  const actAs = actAsFor(deps.config, args.sender ?? '');
  const maxWindow = deps.config.maxWindow;
  try {
    switch (request.op) {
      case 'ingest': {
        if (!String(args.text ?? '').trim()) return { ok: false, error: 'ingest needs non-empty text', actAs };
        return { ok: true, data: await ingest(deps, args, actAs), actAs, maxWindow };
      }
      case 'search': {
        const data = await clientFor(deps, actAs).callTool('wiki_search', {
          query: String(args.query ?? ''),
          top_k: Math.max(1, Math.min(50, args.topK ?? 10)),
        });
        return { ok: true, data, actAs, maxWindow };
      }
      case 'dashboard_link': {
        // A guest has no dashboard: the tool answers `sender_unauthorized`.
        // Refusing here keeps the agent from offering a link that cannot work.
        if (actAs === GUEST) return { ok: false, error: 'a guest has no memory dashboard', actAs };
        const data = await clientFor(deps, actAs).callTool('dashboard_link', {
          intent: args.intent || 'home',
        });
        return { ok: true, data, actAs, maxWindow };
      }
      case 'media': {
        // Guest turns store nothing, and the upload endpoint answers 403.
        if (actAs === GUEST) return { ok: false, error: 'guest turns do not upload media', actAs };
        const data = await clientFor(deps, actAs).uploadMedia({
          bytes: Buffer.from(String(args.bytesBase64 ?? ''), 'base64'),
          filename: String(args.filename ?? 'blob'),
          kind: String(args.kind ?? 'doc'),
          mime: args.mime,
          caption: args.caption,
        });
        return { ok: true, data, actAs, maxWindow };
      }
      default:
        return { ok: false, error: `unknown mwe op: ${String(request.op)}`, actAs };
    }
  } catch (err) {
    return errorOf(err, actAs);
  }
}
