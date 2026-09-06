/**
 * Offline smoke for the nanoclaw bridge — run via smoke.sh.
 *
 * Drives nanoclaw's REAL poll loop (`container/agent-runner/src/poll-loop.ts`,
 * patched by the skill) with the mock provider, against the recording stub
 * endpoint. The container talks to the host exactly as it does in production —
 * an `mwe_request` row on the outbound mailbox — and a pump in this file plays
 * the host, calling the bridge's own `handleMweRequest`. So both halves of the
 * bridge and the seam between them are the shipped code; only the model, the
 * memory server and Docker are absent.
 *
 * Runs under bun, because the agent runner does: `bun:sqlite` for the session
 * mailbox, `bun:test` for its own tests.
 *
 * Env (set by smoke.sh): NANOCLAW_FORK, MWE_STUB_URL, MWE_STUB_CALLS,
 * MWE_STUB_RESPONSES.
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

const FORK = process.env.NANOCLAW_FORK!;
const STUB_URL = process.env.MWE_STUB_URL!;
const CALLS_FILE = process.env.MWE_STUB_CALLS!;
const RESPONSES_FILE = process.env.MWE_STUB_RESPONSES!;

const RUNNER = path.join(FORK, 'container/agent-runner/src');
const HOST_MODULE = path.join(FORK, 'src/modules/mwe');

// The window and turn state live on the session mount in production; here they
// live in a scratch dir the runner modules are pointed at.
const SESSION_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-smoke-session-'));
process.env.NANOCLAW_MWE_SESSION_DIR = SESSION_DIR;
// The switch: a group is an mwe agent when the plugin is stamped into it.
const PLUGIN_MANIFEST = path.join(SESSION_DIR, 'plugin.json');
fs.writeFileSync(PLUGIN_MANIFEST, '{}');
process.env.NANOCLAW_MWE_PLUGIN_MANIFEST = PLUGIN_MANIFEST;
process.env.NANOCLAW_MWE_SDK_DIR = path.join(SESSION_DIR, 'projects');
process.env.TZ = 'Europe/Rome';

// The capability barrel registers the mailbox implementation, exactly as the
// runner's own entry point does before it starts the loop.
await import(path.join(RUNNER, 'modules/index.js'));
const { initTestSessionDb, getInboundDb } = await import(path.join(RUNNER, 'mailbox/sqlite/connection.js'));
const { getUndeliveredMessages } = await import(path.join(RUNNER, 'db/messages-out.js'));
const { MockProvider } = await import(path.join(RUNNER, 'providers/mock.js'));
const { runPollLoop } = await import(path.join(RUNNER, 'poll-loop.js'));
const { handleMweRequest } = await import(path.join(HOST_MODULE, 'turn.js'));
const { newEventsState, personalTick } = await import(path.join(HOST_MODULE, 'events.js'));
const { MweClient } = await import(path.join(HOST_MODULE, 'client.js'));
// The three tools as the agent's MCP process registers them. Importing the
// module registers them with nanoclaw's tool server too, which starts nothing:
// only the barrel's `startMcpServer()` does that.
const { MWE_TOOLS } = await import(path.join(RUNNER, 'mcp-tools/mwe.js'));

// ---------------------------------------------------------------------------
// assertions
// ---------------------------------------------------------------------------

let passed = 0;

function ok(label: string, condition: boolean, detail = ''): void {
  if (!condition) {
    console.error(`FAIL: ${label} ${detail}`);
    process.exit(1);
  }
  passed++;
  console.log(`ok   ${label}`);
}

// ---------------------------------------------------------------------------
// the stub, seen through the two files stub_runner.py keeps
// ---------------------------------------------------------------------------

interface StubCall {
  tool: string;
  arguments: Record<string, unknown>;
  headers: Record<string, string>;
}

function stubState(): { calls: StubCall[]; media: Array<Record<string, unknown>> } {
  try {
    return JSON.parse(fs.readFileSync(CALLS_FILE, 'utf-8'));
  } catch {
    return { calls: [], media: [] };
  }
}

function ingests(): StubCall[] {
  return stubState().calls.filter((c) => c.tool === 'wiki_ingest_message');
}

function scriptStub(responses: Record<string, unknown>): void {
  fs.writeFileSync(RESPONSES_FILE, JSON.stringify(responses));
  // stub_runner.py re-reads the file on its sync tick.
  Bun.sleepSync(200);
}

// ---------------------------------------------------------------------------
// the host, played by the bridge's own handler
// ---------------------------------------------------------------------------

const config = {
  serverUrl: STUB_URL,
  senderMap: { 'telegram:1': 'alice', 'telegram:2': 'bob' },
  operatorSender: 'telegram:1',
  locale: 'it-IT',
  maxWindow: 16,
  groups: [],
  eventsEnabled: true,
  eventsPollSeconds: 30,
  dashboardUrl: 'https://memory.example',
};

const answered = new Set<string>();
let hostRequests = 0;

/**
 * One pass of the host's delivery poll, restricted to what this bridge adds:
 * pick up `mwe_request` rows, answer them with the real handler, write the
 * frame back where the mailbox lookup finds it.
 */
async function pumpHost(): Promise<void> {
  for (const row of getUndeliveredMessages()) {
    if (row.kind !== 'system' || answered.has(row.id)) continue;
    let content: Record<string, unknown>;
    try {
      content = JSON.parse(row.content);
    } catch {
      continue;
    }
    if (content.action !== 'mwe_request') continue;
    answered.add(row.id);
    hostRequests++;
    const frame = await handleMweRequest(
      { op: content.op, args: content.args ?? {} },
      { config, token: 'test-jwt' },
    );
    getInboundDb()
      .prepare(
        `INSERT INTO messages_in (id, seq, kind, timestamp, status, trigger, content)
         VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages_in), 'system', datetime('now'), 'pending', 0, ?)`,
      )
      .run(`mwe-resp-${content.requestId}`, JSON.stringify({
        type: 'mwe_response',
        requestId: content.requestId,
        frame,
      }));
  }
}

let pumpHandle: ReturnType<typeof setInterval> | undefined;

function startHost(): void {
  pumpHandle = setInterval(() => void pumpHost(), 100);
}

function stopHost(): void {
  if (pumpHandle) clearInterval(pumpHandle);
}

// ---------------------------------------------------------------------------
// the session mailbox
// ---------------------------------------------------------------------------

function insertChat(id: string, sender: string, senderId: string, text: string, extra: Record<string, unknown> = {}): void {
  getInboundDb()
    .prepare(
      `INSERT INTO messages_in (id, seq, kind, timestamp, status, platform_id, channel_type, thread_id, content)
       VALUES (?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages_in), 'chat', datetime('now'), 'pending', 'chat-1', 'telegram', NULL, ?)`,
    )
    .run(id, JSON.stringify({ sender, senderId, text, ...extra }));
}

function outboundChat(): Array<Record<string, unknown>> {
  return getUndeliveredMessages()
    .filter((m) => m.kind === 'chat')
    .map((m) => JSON.parse(m.content));
}

async function waitFor(predicate: () => boolean, timeoutMs = 15_000, label = 'condition'): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await Bun.sleep(50);
  }
  console.error(`FAIL: timed out waiting for ${label}`);
  process.exit(1);
}

interface ToolResult {
  content?: Array<{ text?: string }>;
  isError?: boolean;
}

/**
 * Call one of the bridge's tools the way the agent does: the real handler, the
 * turn state it reads off the session mount, and the host round trip through
 * the mailbox.
 */
async function callMweTool(name: string, args: Record<string, unknown>): Promise<{ text: string; isError: boolean }> {
  const definition = (MWE_TOOLS as Array<{ tool: { name: string }; handler: (a: Record<string, unknown>) => Promise<ToolResult> }>)
    .find((t) => t.tool.name === name);
  if (!definition) {
    console.error(`FAIL: the bridge registers no tool called ${name}`);
    process.exit(1);
  }
  const result = await definition.handler(args);
  return { text: String(result.content?.[0]?.text ?? ''), isError: result.isError === true };
}

/** A mock provider that answers `reply` and keeps every prompt it was given. */
function recordingProvider(reply: string): { provider: unknown; prompts: string[] } {
  const prompts: string[] = [];
  const provider = new MockProvider({}, (prompt: string) => {
    prompts.push(prompt);
    return reply;
  });
  return { provider, prompts };
}

/** Run the real poll loop until `until` holds, then stop it. */
async function runTurn(provider: unknown, until: () => boolean, label: string): Promise<void> {
  const controller = new AbortController();
  const loop = runPollLoop({ provider, providerName: 'mock', cwd: SESSION_DIR, signal: controller.signal });
  loop.catch(() => {});
  await waitFor(until, 20_000, label);
  controller.abort();
  await Bun.sleep(200);
}

// ---------------------------------------------------------------------------
// the smoke
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  initTestSessionDb();
  getInboundDb()
    .prepare(
      `INSERT INTO destinations (name, display_name, type, channel_type, platform_id, agent_group_id)
       VALUES ('famiglia', 'Famiglia', 'channel', 'telegram', 'chat-1', NULL)`,
    )
    .run();
  scriptStub({});
  startHost();

  // -- turn 1: one mechanical ingest, the block in front of the batch --------
  insertChat('m1', 'Alice', '1', 'ciao, sono tornata');
  const seenPrompts: string[] = [];
  const provider = new MockProvider({}, (prompt: string) => {
    seenPrompts.push(prompt);
    return '<message to="famiglia">bentornata</message>';
  });
  await runTurn(provider, () => outboundChat().length > 0 && ingests().length >= 2, 'turn 1');

  const turn1 = ingests();
  ok('one ingest for the user turn, one for the reply', turn1.length === 2, `got ${turn1.length}`);
  ok('the user turn is ingested as the user', turn1[0].arguments.author === undefined);
  ok('the text is the user message', turn1[0].arguments.text === 'ciao, sono tornata');
  ok('act-as is the mapped sender', turn1[0].headers['x-mwe-act-as'] === 'alice');
  ok('the bearer token rides the call', turn1[0].headers.authorization === 'Bearer test-jwt');
  const meta1 = turn1[0].arguments.metadata as Record<string, unknown>;
  ok('the locale is forwarded', meta1.locale === 'it-IT');
  ok('the surface label is the chat', meta1.channel === 'telegram:chat-1');
  ok('the timezone rides the turn', meta1.timezone === 'Europe/Rome');
  ok('turn 1 has no window yet', turn1[0].arguments.recent_messages === undefined);

  const prompt1 = seenPrompts[0] ?? '';
  ok('the recall block leads the prompt', prompt1.startsWith('<memory-context>'), prompt1.slice(0, 60));
  ok('the recalled facts are in it', prompt1.includes('(stub) nothing relevant on file'));
  ok('the thread from other channels is injected verbatim', prompt1.includes('RECENT EXCHANGES ON YOUR OTHER CHANNELS'));
  ok('the seed is NOT forwarded — nanoclaw writes its own reply', !prompt1.includes('stub seed'));
  ok(
    'the block comes before the messages nanoclaw formatted',
    prompt1.indexOf('</memory-context>') < prompt1.indexOf('<context timezone'),
  );
  ok(
    'the agent reply is fed back as the assistant',
    turn1[1].arguments.author === 'assistant' && turn1[1].arguments.text === 'bentornata',
  );
  ok('the reply reached the channel', outboundChat()[0].text === 'bentornata');

  // -- turn 2: the window is threaded, and no session is reused --------------
  insertChat('m2', 'Alice', '1', 'e il cane?');
  const continuations: Array<string | undefined> = [];
  const provider2 = new MockProvider({}, (prompt: string) => {
    seenPrompts.push(prompt);
    return '<message to="famiglia">Frodo sta bene</message>';
  });
  const originalQuery = provider2.query.bind(provider2);
  provider2.query = (input: { continuation?: string; prompt: string }) => {
    continuations.push(input.continuation);
    return originalQuery(input);
  };
  await runTurn(provider2, () => ingests().length >= 4, 'turn 2');

  const turn2 = ingests();
  const window2 = turn2[2].arguments.recent_messages as Array<{ role: string; text: string }>;
  ok('turn 2 threads the window', Array.isArray(window2) && window2.length === 2, JSON.stringify(window2));
  ok('the window holds both halves of turn 1', window2[0].text === 'ciao, sono tornata' && window2[1].text === 'bentornata');
  ok('the window is oldest first', window2[0].role === 'user' && window2[1].role === 'assistant');
  ok('the window stops before this turn — the message itself is the text', !window2.some((m) => m.text === 'e il cane?'));
  ok(
    'no continuation is carried into a new turn',
    continuations.length > 0 && continuations.every((c) => c === undefined),
    JSON.stringify(continuations),
  );
  const prompt2 = seenPrompts[seenPrompts.length - 1] ?? '';
  ok('the previous exchange is rendered as the recent conversation', prompt2.includes('<recent-conversation>'));
  ok('the agent sees what it said last turn', prompt2.includes('bentornata'));

  // -- a second sender in the same chat gets their own attribution -----------
  insertChat('m3', 'Bob', '2', 'io invece cerco le chiavi');
  await runTurn(recordingProvider('<message to="famiglia">le cerco</message>').provider, () => ingests().length >= 6, 'bob turn');
  const bobIngest = ingests()[4];
  ok('a different sender speaks as themselves', bobIngest.headers['x-mwe-act-as'] === 'bob');

  // -- an unmapped sender is a guest, never somebody else --------------------
  insertChat('m4', 'Stranger', '999', 'chi siete?');
  await runTurn(
    recordingProvider('<message to="famiglia">siamo di casa</message>').provider,
    () => ingests().length >= 8,
    'guest turn',
  );
  const guestIngest = ingests()[6];
  ok('an unknown sender is a guest', guestIngest.headers['x-mwe-act-as'] === 'guest');
  ok('a guest is not the first mapped user', guestIngest.headers['x-mwe-act-as'] !== 'alice');

  // -- media: upload out of band, ride the turn as a catalog id -------------
  const mediaDir = path.join(SESSION_DIR, 'inbox', 'm5');
  fs.mkdirSync(mediaDir, { recursive: true });
  fs.writeFileSync(path.join(mediaDir, 'foto.jpg'), Buffer.from([0xff, 0xd8, 0xff, 0xd9]));
  insertChat('m5', 'Alice', '1', 'guarda', {
    attachments: [{ name: 'foto.jpg', type: 'photo', localPath: 'inbox/m5/foto.jpg' }],
  });
  await runTurn(
    recordingProvider('<message to="famiglia">che bella</message>').provider,
    () => ingests().length >= 10,
    'media turn',
  );
  const mediaState = stubState();
  ok('the bytes went out of band', mediaState.media.length === 1, JSON.stringify(mediaState.media));
  ok('the upload declared its kind', mediaState.media[0].kind === 'photo');
  ok('the upload is attributed to the sender', (mediaState.media[0].headers as Record<string, string>)['x-mwe-act-as'] === 'alice');
  const mediaIngest = ingests()[8];
  const attachments = mediaIngest.arguments.attachments as Array<{ catalog_id: string }>;
  ok('the turn carries the catalog id', Array.isArray(attachments) && attachments[0].catalog_id.includes('photo'));

  // -- the window is cut to the operator's bound ----------------------------
  for (let i = 0; i < 12; i++) {
    insertChat(`w${i}`, 'Alice', '1', `messaggio ${i}`);
    await runTurn(
      recordingProvider(`<message to="famiglia">ok ${i}</message>`).provider,
      () => ingests().length >= 10 + (i + 1) * 2,
      `filler turn ${i}`,
    );
  }
  const lastWindow = ingests()[ingests().length - 2].arguments.recent_messages as Array<{ text: string }>;
  ok('the window never exceeds maxWindow', lastWindow.length <= config.maxWindow, `got ${lastWindow.length}`);
  ok('the window keeps the newest, not the oldest', !lastWindow.some((m) => m.text === 'ciao, sono tornata'));

  // -- the dashboard link the agent offers is an address, not a path --------
  // The memory mints it as a path: it does not know the origin it is reached
  // at. What the agent puts in front of a person has to open from a phone.
  const link = await callMweTool('mwe_dashboard_link', {});
  ok('the link tool answers', !link.isError, link.text.slice(0, 160));
  const minted = JSON.parse(link.text) as { url: string };
  ok(
    'the minted link carries the dashboard origin',
    minted.url === `${config.dashboardUrl}/dashboard/auth/link?token=stub-jwt&next=%2Fdashboard%2Fhome`,
    minted.url,
  );
  await waitFor(
    () => stubState().calls.some((c) => c.tool === 'dashboard_link'),
    5_000,
    'the link call to reach the stub',
  );
  ok(
    'it acts as the person speaking this turn',
    stubState().calls.filter((c) => c.tool === 'dashboard_link').pop()?.headers['x-mwe-act-as'] === 'alice',
  );

  // -- disambiguation: the candidates are surfaced, the next call commits ----
  scriptStub({
    wiki_ingest_message: {
      intent_classified: 'capture',
      context_snippet: 'Recall: (stub) nothing relevant on file.',
      needs_disambig: true,
      disambig_candidates: [{ candidate_id: 'c1', description: 'Alice Rossi' }],
      llm_used: 'stub',
    },
  });
  insertChat('d1', 'Alice', '1', 'ho visto Alice');
  const disambigRun = recordingProvider('<message to="famiglia">quale Alice?</message>');
  const beforeDisambigReplies = outboundChat().length;
  await runTurn(
    disambigRun.provider,
    () => outboundChat().length > beforeDisambigReplies,
    'disambig turn',
  );
  const disambigPrompt = disambigRun.prompts[0] ?? '';
  ok('the candidates reach the agent', disambigPrompt.includes('c1: Alice Rossi'));
  ok('the agent is told how to commit', disambigPrompt.includes('mwe_disambig_commit'));
  const turnState = JSON.parse(fs.readFileSync(path.join(SESSION_DIR, 'mwe-turn.json'), 'utf-8'));
  ok('the pending disambiguation is left for the tool process', turnState.disambig?.text === 'ho visto Alice');
  ok('the tool will act as the right person', turnState.senderKey === 'telegram:1');

  const commitBefore = ingests().length;
  const commit = await callMweTool('mwe_disambig_commit', { candidate_id: 'c1' });
  ok('the commit is accepted', !commit.isError, commit.text.slice(0, 160));
  await waitFor(() => ingests().length > commitBefore, 5_000, 'the commit to reach the stub');
  const committed = ingests()[commitBefore];
  ok(
    'the second call names the choice',
    (committed.arguments.metadata as Record<string, unknown>).disambig_choice === 'c1',
  );
  ok('the commit replays the same message', committed.arguments.text === 'ho visto Alice');
  // The commit is an ingest, so the agent is handed the memory for the message
  // it just stored, in the fence a turn arrives in — not the server's JSON.
  ok(
    'the agent gets the framed block back, not the raw response',
    !commit.text.trimStart().startsWith('{') &&
      commit.text.includes('Stored:') &&
      commit.text.includes('<memory-context>'),
    commit.text.slice(0, 160),
  );
  ok('the memory for the stored message rides the answer', commit.text.includes('(stub) nothing relevant on file'));
  ok(
    "the response's operational fields stay away from the agent",
    !commit.text.includes('llm_used') && !commit.text.includes('intent_classified'),
  );
  ok('the memory does not ask twice: naming the choice settles it', !commit.text.includes('mwe_disambig_commit'));
  ok(
    'the pending disambiguation is gone once committed',
    JSON.parse(fs.readFileSync(path.join(SESSION_DIR, 'mwe-turn.json'), 'utf-8')).disambig === undefined,
  );
  scriptStub({});

  // -- the two governance blocks reach the agent ----------------------------
  // The wire shape is the server's (`call_wiki_ingest_message`): both keys are
  // absent unless the turn earned them, which is why the plain turn above is
  // asserted to carry neither.
  ok(
    'a plain turn says nothing about a vote or a document',
    !prompt1.includes("waiting on this person's vote") && !prompt1.includes('as a document'),
  );
  ok(
    'a plain turn leaves no blank paragraph in the fence',
    !prompt1.slice(0, prompt1.indexOf('</memory-context>')).includes('\n\n\n'),
  );
  scriptStub({
    wiki_ingest_message: {
      intent_classified: 'capture',
      context_snippet: 'Recall: (stub) nothing relevant on file.',
      pending_votes: {
        count: 1,
        requests: [
          {
            proposal_id: 'p-forget-1',
            fact_id: 'f-2026-06-12-0001',
            requester: 'bob',
            deadline: '2026-06-19T09:00:00Z',
            dashboard_path: '/dashboard/proposals/p-forget-1/open-in-chat',
          },
        ],
        dashboard_path: '/dashboard/proposals',
        note: 'vote_no_to_block_silence_is_consent',
      },
      document_promoted: { catalog_id: 'c-2026-06-12-doc-001.txt', job_id: 'j-1', existing: false },
      llm_used: 'stub',
    },
  });
  insertChat('g1', 'Alice', '1', 'ti incollo il regolamento intero');
  const governance = recordingProvider('<message to="famiglia">ricevuto</message>');
  const beforeGovernance = outboundChat().length;
  await runTurn(governance.provider, () => outboundChat().length > beforeGovernance, 'governance turn');
  const governancePrompt = governance.prompts[0] ?? '';
  ok('the owed vote reaches the agent', governancePrompt.includes("waiting on this person's vote"));
  ok(
    'the vote line names who asked and by when',
    governancePrompt.includes('asked by bob, open until 2026-06-19T09:00:00Z'),
  );
  ok(
    // The page is named as an address, for the same reason the minted link is.
    'it sends the person to the dashboard, the only place a vote is cast',
    governancePrompt.includes('mwe_dashboard_link') &&
      governancePrompt.includes(`${config.dashboardUrl}/dashboard/proposals`) &&
      governancePrompt.includes('nowhere else'),
  );
  ok(
    'it carries the consent rule and forbids inventing the fact',
    governancePrompt.includes('Saying nothing until the deadline is consent') &&
      governancePrompt.includes('do not guess them'),
  );
  ok(
    'the reminder is raised once, not repeated on every message',
    governancePrompt.includes('do not raise it again'),
  );
  ok(
    'the promoted document reaches the agent',
    governancePrompt.includes("kept this turn's message as a document") &&
      governancePrompt.includes('do not ask them to send it again'),
  );
  ok(
    'governance rides ahead of the recalled memory, inside the same fence',
    governancePrompt.indexOf("waiting on this person's vote") <
      governancePrompt.indexOf('(stub) nothing relevant on file') &&
      governancePrompt.indexOf("kept this turn's message as a document") <
        governancePrompt.indexOf('</memory-context>'),
  );
  scriptStub({});

  // -- degradation: the memory falls over and the turn still answers --------
  scriptStub({ wiki_ingest_message: '__fail__' });
  insertChat('f1', 'Alice', '1', 'e adesso?');
  const before = outboundChat().length;
  const degraded = recordingProvider('<message to="famiglia">eccomi</message>');
  const failPrompts = degraded.prompts;
  await runTurn(degraded.provider, () => outboundChat().length > before, 'degraded turn');
  ok('the turn answered without memory', outboundChat().some((m) => m.text === 'eccomi'));
  ok('no recall block was invented', !(failPrompts[0] ?? '').includes('<memory-context>'));
  ok('the message itself still reached the agent', (failPrompts[0] ?? '').includes('e adesso?'));
  scriptStub({});

  // -- the reverse channel: poll, enqueue, ack, in that order ---------------
  const events = [
    {
      event_id: 1,
      kind: 'fact_minted_for_you',
      payload: {
        recipient_id: 'user:bob',
        from_user_id: 'alice',
        facts: [{ body: 'la visita è alle sei' }],
        dashboard_path: '/dashboard/wiki/bob',
      },
    },
    {
      event_id: 2,
      kind: 'fact_minted_for_you',
      payload: { recipient_id: 'user:frodo', facts: [{ body: 'niente' }] },
    },
  ];
  scriptStub({ events_poll: { events, has_more: false } });
  const order: string[] = [];
  const enqueued: Array<{ senderKey: string; instruction: string }> = [];
  const eventsClient = new MweClient(STUB_URL, 'test-jwt');
  const recordingClient = {
    async callTool(tool: string, args: Record<string, unknown>) {
      order.push(tool);
      return eventsClient.callTool(tool, args);
    },
  };
  const state = newEventsState();
  const result = await personalTick(
    {
      config,
      token: 'test-jwt',
      enqueue: async (delivery: { senderKey: string; instruction: string }) => {
        order.push('enqueue');
        enqueued.push(delivery);
        return true;
      },
    },
    state,
    recordingClient,
    'consumer-1',
  );
  ok('the routable notice was delivered', result.delivered === 1);
  ok('it went to the recipient own chat', enqueued[0].senderKey === 'telegram:2');
  ok('it carries the content, not a pointer', enqueued[0].instruction.includes('la visita è alle sei'));
  ok('it tells the agent the recipient was not there', enqueued[0].instruction.includes('took no part'));
  ok('poll, then enqueue, then ack', order.slice(0, 3).join('>') === 'events_poll>enqueue>events_ack');
  await waitFor(
    () => stubState().calls.some((c) => c.tool === 'events_ack'),
    5_000,
    'the ack to reach the stub',
  );
  const ack = stubState().calls.filter((c) => c.tool === 'events_ack').pop();
  ok('only the delivered notice is acked', JSON.stringify(ack?.arguments.event_ids) === '[1]');
  ok('the unroutable one is held for a retry', state.routeAttempts.get(2) === 1);
  ok('it was never delivered to somebody else', enqueued.length === 1);

  // -- and the host really was the only one holding a credential ------------
  ok('every memory call went through the host', hostRequests > 0);
  const runnerSource = fs.readFileSync(path.join(RUNNER, 'mwe/host-call.ts'), 'utf-8');
  ok('the container half never reads a token', !runnerSource.includes('MWE_TOKEN'));

  stopHost();
  console.log(`\n${passed} assertions passed`);
}

await main();
