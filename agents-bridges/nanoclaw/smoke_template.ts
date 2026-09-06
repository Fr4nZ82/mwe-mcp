/**
 * Read `templates/mwe` through nanoclaw's own template parser.
 *
 * Stamping a group needs a database and a running host; parsing does not, and
 * the parser is where every rule that can reject this template lives — the
 * manifest schema, the plugin-name charset, the containment walk, the
 * credential lint. So the smoke validates the template exactly as `ncl groups
 * create` would read it, and then checks the two things a passing parse does
 * not: that the persona is really there, and that it says what the bridge
 * actually implements.
 *
 * Run from the fork root under bun.
 */
import path from 'path';

const FORK = process.cwd();
const { parseTemplate } = await import(path.join(FORK, 'src/templates/parse.ts'));

let passed = 0;

function ok(label: string, condition: boolean, detail = ''): void {
  if (!condition) {
    console.error(`FAIL: ${label} ${detail}`);
    process.exit(1);
  }
  passed++;
  console.log(`ok   ${label}`);
}

const template = parseTemplate(path.join(FORK, 'templates/mwe'));

ok('the plugin manifest parses', template.name === 'mwe', JSON.stringify(template.name));
ok('nothing in the template is reported as invalid', (template.report ?? []).length === 0, JSON.stringify(template.report));
ok('the agent name is precompiled to mwe', template.agentName === 'mwe', String(template.agentName));
ok('no MCP server is declared', Object.keys(template.mcpServers ?? {}).length === 0);

const persona = String(template.instructions ?? '');
ok('the persona is present', persona.length > 0);
ok('the persona is under the size a reader can hold', persona.split('\n').length < 200, `${persona.split('\n').length} lines`);

// Every sentence of a persona is a live instruction, so the sections it names
// must be the ones the server actually sends and the tools the bridge really
// registers.
for (const section of [
  'YOUR RULES',
  'RECENT EXCHANGES ON YOUR OTHER CHANNELS WITH THIS USER',
  'WHO YOU ARE',
  'WHO IS SPEAKING',
  'YOUR RECENT HISTORY WITH THIS USER',
  'RELEVANT MEMORY',
  'NAVIGATED PAGES',
  'UPCOMING',
]) {
  ok(`the persona names the ${section} section`, persona.includes(section));
}
for (const tool of ['mwe_search', 'mwe_dashboard_link', 'mwe_disambig_commit']) {
  ok(`the persona names ${tool}`, persona.includes(tool));
}
ok('the persona names the block the bridge actually injects', persona.includes('<memory-context>'));
ok('the persona names the window the bridge actually injects', persona.includes('<recent-conversation>'));
ok('the persona forbids a second memory on disk', persona.includes('Never write a memory file'));
ok('the persona covers guests', persona.toLowerCase().includes('guest'));
ok(
  'the persona does not promise a seed the bridge never injects',
  !persona.includes('suggested_seed') && !persona.includes('seed'),
);

console.log(`\n${passed} template assertions passed`);
