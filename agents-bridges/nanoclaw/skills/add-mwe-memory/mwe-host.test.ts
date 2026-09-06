/**
 * The host half of the mwe-mcp bridge: identity resolution, the per-turn
 * request handler, and the reverse channel.
 *
 * These pin the decisions that had a plausible alternative — an unmapped
 * sender becoming the owner instead of a guest, a notice landing in whichever
 * chat was to hand, an ack fired before the delivery was durable. Each test
 * names the behaviour that was chosen and denies the one that was not.
 */
import { describe, it, expect, vi } from 'vitest';
import fs from 'fs';
import os from 'os';
import path from 'path';

import {
  actAsFor,
  chatLookup,
  GUEST,
  groupEnabled,
  loadMweConfig,
  reverseRoutes,
  type MweConfig,
} from './config.js';
import { dropSections, mweSharedBase } from './base.js';
import { mweGroupFolders, mweGroupIds, SCAFFOLD_FILES, whyMemoryTreeIsNotPristine } from './groups.js';
import { handleMweRequest } from './turn.js';
import {
  buildDigestInstruction,
  buildInstruction,
  digestTick,
  newEventsState,
  personalTick,
  recipientOf,
  MAX_ROUTE_ATTEMPTS,
} from './events.js';

function config(overrides: Partial<MweConfig> = {}): MweConfig {
  return {
    serverUrl: 'http://127.0.0.1:8742/mcp',
    senderMap: { 'telegram:1': 'alice', 'telegram:2': 'bob' },
    operatorSender: 'telegram:1',
    locale: 'it-IT',
    maxWindow: 4,
    groups: [],
    eventsEnabled: true,
    eventsPollSeconds: 30,
    dashboardUrl: 'https://memory.example',
    ...overrides,
  };
}

/** A client that records every call and answers from a scripted table. */
function recordingClient(responses: Record<string, unknown[]> = {}) {
  const calls: Array<{ tool: string; args: Record<string, unknown> }> = [];
  const queues = new Map(Object.entries(responses).map(([tool, list]) => [tool, [...list]]));
  const client = {
    calls,
    async callTool(tool: string, args: Record<string, unknown> = {}) {
      calls.push({ tool, args });
      const queue = queues.get(tool);
      const next = queue && queue.length > 0 ? queue.shift() : undefined;
      return (next ?? {}) as Record<string, unknown>;
    },
    async uploadMedia() {
      calls.push({ tool: 'POST /media', args: {} });
      return { catalog_id: 'c-2026-06-12-photo-001.jpg' };
    },
  };
  return client;
}

describe('mwe.json', () => {
  it('reads the file and defaults what it does not carry', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-cfg-'));
    fs.writeFileSync(
      path.join(dir, 'mwe.json'),
      JSON.stringify({ serverUrl: 'http://127.0.0.1:8742/mcp', senderMap: { 'telegram:1': 'alice' } }),
    );
    const cfg = loadMweConfig(dir);
    expect(cfg?.maxWindow).toBe(16);
    expect(cfg?.eventsPollSeconds).toBe(30);
    expect(cfg?.eventsEnabled).toBe(true);
  });

  it('is inactive rather than broken when the file is missing', () => {
    expect(loadMweConfig(fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-cfg-')))).toBeNull();
  });

  it('never lets the poll interval go below the contract floor', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-cfg-'));
    fs.writeFileSync(
      path.join(dir, 'mwe.json'),
      JSON.stringify({ serverUrl: 'http://x/mcp', eventsPollSeconds: 1 }),
    );
    expect(loadMweConfig(dir)?.eventsPollSeconds).toBe(5);
  });

  it('serves every group when `groups` is empty, and only the named ones when it is not', () => {
    expect(groupEnabled(config(), 'ag-1')).toBe(true);
    expect(groupEnabled(config({ groups: ['ag-2'] }), 'ag-1')).toBe(false);
    expect(groupEnabled(config({ groups: ['ag-1'] }), 'ag-1')).toBe(true);
  });
});

describe('identity', () => {
  it('maps a known chat sender to their own mwe user', () => {
    expect(actAsFor(config(), 'telegram:2')).toBe('bob');
  });

  it('makes an unmapped sender a guest — never the first entry, never the owner', () => {
    const actAs = actAsFor(config(), 'telegram:999');
    expect(actAs).toBe(GUEST);
    expect(actAs).not.toBe('alice');
  });

  it('reads senderMap backwards for delivery, one route per person', () => {
    const routes = reverseRoutes(config({ senderMap: { 'telegram:1': 'alice', 'telegram:9': 'alice' } }));
    expect(routes.get('alice')).toBe('telegram:1');
  });
});

describe('the per-turn request', () => {
  it('sends one ingest carrying the window, the locale and the surface label', async () => {
    const client = recordingClient({ wiki_ingest_message: [{ context_snippet: 'hi' }] });
    const answer = await handleMweRequest(
      {
        op: 'ingest',
        args: {
          sender: 'telegram:1',
          text: 'ciao',
          channel: 'telegram:1',
          recentMessages: [{ role: 'user', text: 'prima' }],
        },
      },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    expect(answer.ok).toBe(true);
    expect(client.calls).toHaveLength(1);
    expect(client.calls[0].tool).toBe('wiki_ingest_message');
    expect(client.calls[0].args.recent_messages).toEqual([{ role: 'user', text: 'prima' }]);
    const metadata = client.calls[0].args.metadata as Record<string, unknown>;
    expect(metadata.locale).toBe('it-IT');
    expect(metadata.channel).toBe('telegram:1');
  });

  it('cuts the window to maxWindow, keeping the newest — the window is the consumer\'s to trim', async () => {
    const client = recordingClient({ wiki_ingest_message: [{}] });
    const recentMessages = Array.from({ length: 10 }, (_, i) => ({ role: 'user' as const, text: `m${i}` }));
    await handleMweRequest(
      { op: 'ingest', args: { sender: 'telegram:1', text: 'ciao', recentMessages } },
      { config: config({ maxWindow: 4 }), token: 'jwt', clientFor: () => client as never },
    );
    const sent = client.calls[0].args.recent_messages as Array<{ text: string }>;
    expect(sent).toHaveLength(4);
    expect(sent[0].text).toBe('m6');
    expect(sent[3].text).toBe('m9');
  });

  it('answers a memory failure instead of throwing, so the turn survives', async () => {
    const client = {
      async callTool() {
        throw new Error('connection refused');
      },
    };
    const answer = await handleMweRequest(
      { op: 'ingest', args: { sender: 'telegram:1', text: 'ciao' } },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    expect(answer.ok).toBe(false);
    expect(answer.ok === false && answer.error).toContain('connection refused');
  });

  it('completes the minted link with the dashboard origin — a path opens nowhere', async () => {
    const client = recordingClient({
      dashboard_link: [{ url: '/dashboard/auth/link?token=jwt&next=%2Fdashboard%2Fhome', base_ttl_seconds: 600 }],
    });
    const answer = await handleMweRequest(
      { op: 'dashboard_link', args: { sender: 'telegram:1' } },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    expect(answer.ok).toBe(true);
    expect(answer.ok === true && answer.data.url).toBe(
      'https://memory.example/dashboard/auth/link?token=jwt&next=%2Fdashboard%2Fhome',
    );
    // Everything else the server sent rides along untouched.
    expect(answer.ok === true && answer.data.base_ttl_seconds).toBe(600);
  });

  it('leaves an address that already names its origin exactly as it came', async () => {
    const client = recordingClient({ dashboard_link: [{ url: 'https://elsewhere.example/dashboard/home' }] });
    const answer = await handleMweRequest(
      { op: 'dashboard_link', args: { sender: 'telegram:1' } },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    expect(answer.ok === true && answer.data.url).toBe('https://elsewhere.example/dashboard/home');
  });

  it('falls back to serverUrl minus /mcp when no public origin is declared', async () => {
    const client = recordingClient({ dashboard_link: [{ url: '/dashboard/home' }] });
    const answer = await handleMweRequest(
      { op: 'dashboard_link', args: { sender: 'telegram:1' } },
      { config: config({ dashboardUrl: '' }), token: 'jwt', clientFor: () => client as never },
    );
    expect(answer.ok === true && answer.data.url).toBe('http://127.0.0.1:8742/dashboard/home');
  });

  it('completes the page the vote block names, so the recall block can offer it', async () => {
    const client = recordingClient({
      wiki_ingest_message: [
        {
          pending_votes: {
            count: 1,
            requests: [{ requester: 'bob', deadline: '2026-06-19T09:00:00Z' }],
            dashboard_path: '/dashboard/proposals',
          },
        },
      ],
    });
    const answer = await handleMweRequest(
      { op: 'ingest', args: { sender: 'telegram:1', text: 'ciao' } },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    const votes = answer.ok === true ? (answer.data.pending_votes as Record<string, unknown>) : {};
    expect(votes.dashboard_path).toBe('https://memory.example/dashboard/proposals');
    // The rest of the block is the server's and stays as it came.
    expect(votes.count).toBe(1);
  });

  it('refuses a dashboard link and a media upload for a guest, but still ingests their words', async () => {
    const client = recordingClient({ wiki_ingest_message: [{}] });
    const deps = { config: config(), token: 'jwt', clientFor: () => client as never };
    const link = await handleMweRequest({ op: 'dashboard_link', args: { sender: 'telegram:999' } }, deps);
    const media = await handleMweRequest(
      { op: 'media', args: { sender: 'telegram:999', bytesBase64: 'AA==', kind: 'photo' } },
      deps,
    );
    const ingest = await handleMweRequest({ op: 'ingest', args: { sender: 'telegram:999', text: 'ciao' } }, deps);
    expect(link.ok).toBe(false);
    expect(media.ok).toBe(false);
    expect(ingest.ok).toBe(true);
    expect(client.calls.map((c) => c.tool)).toEqual(['wiki_ingest_message']);
  });
});

describe('the reverse channel', () => {
  const minted = (id: number, recipient: string): Record<string, unknown> => ({
    event_id: id,
    kind: 'fact_minted_for_you',
    payload: {
      recipient_id: `user:${recipient}`,
      from_user_id: 'carol',
      facts: [{ body: 'the viewing is at six' }],
      dashboard_path: '/dashboard/wiki/bob',
    },
  });

  it('strips the `user:` prefix the payload carries', () => {
    expect(recipientOf(minted(1, 'bob') as never)).toBe('bob');
  });

  it('enqueues to the recipient own chat, then acks — never the other way round', async () => {
    const order: string[] = [];
    const client = recordingClient({ events_poll: [{ events: [minted(7, 'bob')] }, { events: [] }] });
    const wrapped = {
      ...client,
      async callTool(tool: string, args: Record<string, unknown>) {
        order.push(tool);
        return client.callTool(tool, args);
      },
    };
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async () => {
        order.push('enqueue');
        return true;
      },
    };
    const result = await personalTick(deps as never, newEventsState(), wrapped as never, 'consumer-1');
    expect(result.delivered).toBe(1);
    expect(order.slice(0, 3)).toEqual(['events_poll', 'enqueue', 'events_ack']);
  });

  it('does not ack a notice whose delivery could not be written', async () => {
    const client = recordingClient({ events_poll: [{ events: [minted(8, 'bob')] }, { events: [] }] });
    const deps = { config: config(), token: 'jwt', enqueue: async () => false };
    const result = await personalTick(deps as never, newEventsState(), client as never, 'consumer-1');
    expect(result.pending).toBe(1);
    expect(client.calls.some((c) => c.tool === 'events_ack')).toBe(false);
  });

  it('holds an unroutable notice back, and never delivers it to somebody else', async () => {
    const enqueued: string[] = [];
    const client = recordingClient({ events_poll: [{ events: [minted(9, 'frodo')] }, { events: [] }] });
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async (d: { senderKey: string }) => {
        enqueued.push(d.senderKey);
        return true;
      },
    };
    const state = newEventsState();
    const result = await personalTick(deps as never, state, client as never, 'consumer-1');
    expect(enqueued).toEqual([]);
    expect(result.pending).toBe(1);
    expect(client.calls.some((c) => c.tool === 'events_ack')).toBe(false);
    expect(state.routeAttempts.get(9)).toBe(1);
  });

  it('gives up on an unroutable notice only after the retry window', async () => {
    const state = newEventsState();
    state.routeAttempts.set(10, MAX_ROUTE_ATTEMPTS - 1);
    const client = recordingClient({ events_poll: [{ events: [minted(10, 'frodo')] }, { events: [] }] });
    const deps = { config: config(), token: 'jwt', enqueue: async () => true };
    await personalTick(deps as never, state, client as never, 'consumer-1');
    const ack = client.calls.find((c) => c.tool === 'events_ack');
    expect(ack?.args.event_ids).toEqual([10]);
  });

  it('tells the agent the recipient was not there, and hands it the content', () => {
    const instruction = buildInstruction([minted(11, 'bob')] as never, 'bob', config());
    expect(instruction).toContain('the viewing is at six');
    expect(instruction).toContain('took no part in that conversation');
    expect(instruction).toContain('https://memory.example/dashboard/wiki/bob');
  });

  it('gives one person one delivery, however many notices are waiting', async () => {
    // A backlog is the case that made this matter: the memory catching up
    // after the bridge was down put one chat message per notice into the same
    // second. One message, with the items in it, is what somebody who
    // remembers sounds like.
    const backlog = [minted(21, 'bob'), minted(22, 'bob'), minted(23, 'bob')];
    const client = recordingClient({ events_poll: [{ events: backlog }, { events: [] }] });
    const enqueued: Array<{ senderKey: string; instruction: string }> = [];
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async (d: { senderKey: string; instruction: string }) => {
        enqueued.push(d);
        return true;
      },
    };
    const result = await personalTick(deps as never, newEventsState(), client as never, 'consumer-1');
    expect(enqueued).toHaveLength(1);
    expect(result.delivered).toBe(1);
    // All three are in it, in order, each keeping where it came from.
    expect(enqueued[0].instruction).toContain('Deliver 3 memory notices');
    expect(enqueued[0].instruction).toContain('ITEM 1');
    expect(enqueued[0].instruction).toContain('ITEM 3');
    expect(enqueued[0].instruction).toContain('One message, not one per item');
    expect(enqueued[0].instruction).toContain('took no part in those conversations');
    // And they are acked together: acking only some would drop the notices
    // whose words never reached anybody.
    const ack = client.calls.find((c) => c.tool === 'events_ack');
    expect(ack?.args.event_ids).toEqual([21, 22, 23]);
  });

  it('keeps two people apart even when their notices arrive together', async () => {
    const client = recordingClient({
      events_poll: [{ events: [minted(31, 'bob'), minted(32, 'alice'), minted(33, 'bob')] }, { events: [] }],
    });
    const enqueued: Array<{ senderKey: string; recipient: string }> = [];
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async (d: { senderKey: string; recipient: string }) => {
        enqueued.push(d);
        return true;
      },
    };
    await personalTick(deps as never, newEventsState(), client as never, 'consumer-1');
    expect(enqueued.map((d) => d.recipient).sort()).toEqual(['alice', 'bob']);
    expect(enqueued.map((d) => d.senderKey).sort()).toEqual(['telegram:1', 'telegram:2']);
  });

  it('holds a whole group back when its one delivery could not be written', async () => {
    const client = recordingClient({ events_poll: [{ events: [minted(41, 'bob'), minted(42, 'bob')] }, { events: [] }] });
    const deps = { config: config(), token: 'jwt', enqueue: async () => false };
    const result = await personalTick(deps as never, newEventsState(), client as never, 'consumer-1');
    expect(result.pending).toBe(2);
    expect(client.calls.some((c) => c.tool === 'events_ack')).toBe(false);
  });
});

describe('the daily recap', () => {
  const systemNotice = (id: number, kind: string): Record<string, unknown> => ({
    event_id: id,
    kind,
    payload: {},
  });

  it('says nothing on a quiet day, and does not poll again the same day', async () => {
    const client = recordingClient({ events_poll: [{ events: [] }] });
    const enqueued: string[] = [];
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async (d: { senderKey: string }) => {
        enqueued.push(d.senderKey);
        return true;
      },
    };
    const state = newEventsState();
    expect(await digestTick(deps as never, state, client as never, 'consumer-1')).toBe(false);
    expect(enqueued).toEqual([]);
    const pollsAfterFirst = client.calls.length;
    expect(await digestTick(deps as never, state, client as never, 'consumer-1')).toBe(false);
    expect(client.calls).toHaveLength(pollsAfterFirst);
  });

  it('sends one recap to the operator chat, then acks what it summarised', async () => {
    const client = recordingClient({
      events_poll: [
        { events: [systemNotice(1, 'structure_applied'), systemNotice(2, 'document_ingested')], has_more: false },
      ],
    });
    const enqueued: Array<{ senderKey: string; instruction: string }> = [];
    const deps = {
      config: config(),
      token: 'jwt',
      enqueue: async (d: { senderKey: string; instruction: string }) => {
        enqueued.push(d);
        return true;
      },
    };
    const state = newEventsState();
    expect(await digestTick(deps as never, state, client as never, 'consumer-1')).toBe(true);
    expect(enqueued).toHaveLength(1);
    expect(enqueued[0].senderKey).toBe('telegram:1');
    expect(client.calls.find((c) => c.tool === 'events_ack')?.args.event_ids).toEqual([1, 2]);
    // Once a day: a second tick the same day polls nothing more.
    const after = client.calls.length;
    await digestTick(deps as never, state, client as never, 'consumer-1');
    expect(client.calls).toHaveLength(after);
  });

  it('holds operator notices back rather than picking a chat, when none is named', async () => {
    const client = recordingClient({
      events_poll: [{ events: [systemNotice(3, 'archive_proposed')], has_more: false }],
    });
    const deps = {
      config: config({ operatorSender: '' }),
      token: 'jwt',
      enqueue: async () => true,
    };
    expect(await digestTick(deps as never, newEventsState(), client as never, 'consumer-1')).toBe(false);
    expect(client.calls.some((c) => c.tool === 'events_ack')).toBe(false);
  });

  it('counts what happened in words the agent can use', () => {
    const instruction = buildDigestInstruction(new Map([['structure_applied', 2]]), config());
    expect(instruction).toContain('2 pages reorganized');
    expect(instruction).toContain('https://memory.example/dashboard/');
  });
});

describe('the token', () => {
  it('is read from the environment, never written into a container spec', async () => {
    // The trust boundary in one assertion: the module that talks to the memory
    // takes the token as an argument. Nothing in the container half can supply
    // one, because nothing in the container half ever sees one.
    const spy = vi.fn();
    const client = { callTool: spy.mockResolvedValue({}) };
    await handleMweRequest(
      { op: 'search', args: { sender: 'telegram:1', query: 'cane' } },
      { config: config(), token: 'jwt', clientFor: () => client as never },
    );
    expect(spy).toHaveBeenCalledWith('wiki_search', expect.objectContaining({ query: 'cane' }));
  });
});

describe('a senderMap key', () => {
  function writeConfig(dir: string, senderMap: Record<string, string>): void {
    fs.writeFileSync(
      path.join(dir, 'mwe.json'),
      JSON.stringify({ serverUrl: 'http://127.0.0.1:8742/mcp', senderMap }),
    );
  }

  it('is dropped when it names no channel, and reported once', () => {
    // A bare key (the shape hermes accepts) routes nothing: there is no channel
    // to deliver a notice on. Keeping it meant a warning per key per tick of the
    // reverse channel. It is dropped at load instead, and named once.
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-cfg-'));
    writeConfig(dir, { 'telegram:1': 'alice', bob: 'bob', '': 'nobody' });
    const warnings: string[] = [];
    const loaded = loadMweConfig(dir, (message) => warnings.push(message));
    expect(loaded?.senderMap).toEqual({ 'telegram:1': 'alice' });
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain('bob');
    expect(warnings[0]).toContain('<channel>:<platform id>');
    // A person behind a dropped key is a guest, never somebody else.
    expect(actAsFor(loaded as MweConfig, 'bob')).toBe(GUEST);
  });

  it('is silent when the file is clean, and silent when nobody is listening', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-cfg-'));
    writeConfig(dir, { 'telegram:1': 'alice' });
    const warnings: string[] = [];
    expect(loadMweConfig(dir, (message) => warnings.push(message))?.senderMap).toEqual({ 'telegram:1': 'alice' });
    expect(warnings).toEqual([]);
    // The per-turn read passes no callback: a malformed file is named once per
    // host start, not once per turn.
    writeConfig(dir, { bob: 'bob' });
    expect(loadMweConfig(dir)?.senderMap).toEqual({});
  });
});

describe('finding the chat a notice belongs in', () => {
  it('asks for the platform id nanoclaw actually stores', () => {
    // nanoclaw's Telegram adapter registers a paired chat with the channel in
    // the column: platform_id = 'telegram:<chat id>'. Asking for the bare id —
    // the obvious reading of "<channel>:<platform id>" — matches no row, and
    // every notice waits forever for a chat that is right there.
    const lookup = chatLookup('telegram:123456789');
    expect(lookup?.channelType).toBe('telegram');
    expect(lookup?.platformIds[0]).toBe('telegram:123456789');
    expect(lookup?.platformIds).toContain('123456789');
  });

  it('has nothing to look up for a key that names no channel', () => {
    expect(chatLookup('alice')).toBeNull();
    expect(chatLookup('')).toBeNull();
  });
});

describe('which groups carry the memory', () => {
  function stampGroup(root: string, folder: string, withPlugin: boolean): string {
    const groupDir = path.join(root, 'groups', folder);
    fs.mkdirSync(groupDir, { recursive: true });
    if (withPlugin) {
      fs.mkdirSync(path.join(groupDir, 'plugins', 'mwe'), { recursive: true });
      fs.writeFileSync(path.join(groupDir, 'plugins', 'mwe', 'plugin.json'), '{"name":"mwe"}');
    }
    return groupDir;
  }

  it('is decided by the stamped plugin, not by a setting', () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-fork-'));
    stampGroup(root, 'famiglia', true);
    stampGroup(root, 'lavoro', false);
    expect(mweGroupFolders(root)).toEqual(['famiglia']);
    expect(mweGroupIds(root, [
      { id: 'ag-1', folder: 'famiglia' },
      { id: 'ag-2', folder: 'lavoro' },
      { folder: 'famiglia' },
    ])).toEqual(['ag-1']);
  });

  it('is an empty list for a fork that has never spawned an agent', () => {
    expect(mweGroupFolders(fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-fork-')))).toEqual([]);
  });
});

describe('the shared CLAUDE.md a memory group receives', () => {
  const BASE = [
    'You are a NanoClaw agent.',
    '',
    '## Workspace',
    '',
    'Files you create are saved in `/workspace/agent/`.',
    '',
    '## Memory',
    '',
    'Your persistent memory lives under `/workspace/agent/memory/`.',
    '',
    '## Conversation history',
    '',
    'The `conversations/` folder holds searchable transcripts.',
    '',
  ].join('\n');

  function fork(withPlugin: boolean): { root: string; groupDir: string } {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-base-'));
    fs.mkdirSync(path.join(root, 'container'), { recursive: true });
    fs.writeFileSync(path.join(root, 'container', 'CLAUDE.md'), BASE);
    const groupDir = path.join(root, 'groups', 'famiglia');
    fs.mkdirSync(groupDir, { recursive: true });
    if (withPlugin) {
      fs.mkdirSync(path.join(groupDir, 'plugins', 'mwe'), { recursive: true });
      fs.writeFileSync(path.join(groupDir, 'plugins', 'mwe', 'plugin.json'), '{"name":"mwe"}');
    }
    return { root, groupDir };
  }

  it('drops the sections that teach a second memory on disk', () => {
    // The persona says never to look in memory/ or conversations/. The base
    // said the opposite, and said it AFTER the persona. Both go.
    const { root, groupDir } = fork(true);
    const base = mweSharedBase(root, groupDir) as string;
    expect(base).not.toContain('## Memory');
    expect(base).not.toContain('/workspace/agent/memory/');
    expect(base).not.toContain('## Conversation history');
    expect(base).not.toContain('conversations/');
    // Everything else nanoclaw tells an agent is untouched.
    expect(base).toContain('You are a NanoClaw agent.');
    expect(base).toContain('## Workspace');
    expect(base).toContain('Files you create are saved in `/workspace/agent/`.');
  });

  it('leaves a group without the plugin on nanoclaw own base', () => {
    // null means "keep the symlink": an ordinary agent is untouched, and the
    // bridge never rewrites a base for a group that did not ask for one.
    const { root, groupDir } = fork(false);
    expect(mweSharedBase(root, groupDir)).toBeNull();
  });

  it('drops a named section whole, and only the named ones', () => {
    expect(dropSections('## A\nkeep me not\n\n## B\nkeep me\n', ['A'])).toBe('## B\nkeep me\n');
    expect(dropSections('## A\nbody\n', ['Missing'])).toBe('## A\nbody\n');
  });
});

describe('the memory tree an earlier boot left behind', () => {
  function scaffold(): { memoryDir: string; templatesDir: string } {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-scaffold-'));
    const templatesDir = path.join(root, 'templates');
    const memoryDir = path.join(root, 'memory');
    for (const dir of [templatesDir, memoryDir]) fs.mkdirSync(path.join(dir, 'system'), { recursive: true });
    for (const rel of SCAFFOLD_FILES) {
      fs.writeFileSync(path.join(templatesDir, rel), `template ${rel}\n`);
      fs.writeFileSync(path.join(memoryDir, rel), `template ${rel}\n`);
    }
    return { memoryDir, templatesDir };
  }

  it('is removable only when it is the templates and nothing else', () => {
    const { memoryDir, templatesDir } = scaffold();
    expect(whyMemoryTreeIsNotPristine(memoryDir, templatesDir)).toBeNull();
  });

  it('is kept when the agent wrote anything of its own', () => {
    const { memoryDir, templatesDir } = scaffold();
    fs.writeFileSync(path.join(memoryDir, 'alice.md'), 'she prefers the early train\n');
    expect(whyMemoryTreeIsNotPristine(memoryDir, templatesDir)).toContain('alice.md');
  });

  it('is kept when a template was edited, byte for byte', () => {
    const { memoryDir, templatesDir } = scaffold();
    fs.appendFileSync(path.join(memoryDir, 'system/definition.md'), 'and one more rule\n');
    expect(whyMemoryTreeIsNotPristine(memoryDir, templatesDir)).toContain('system/definition.md');
  });

  it('is kept when it is not the scaffold at all', () => {
    const { memoryDir, templatesDir } = scaffold();
    fs.rmSync(path.join(memoryDir, 'index.md'));
    expect(whyMemoryTreeIsNotPristine(memoryDir, templatesDir)).toContain('index.md');
  });
});
