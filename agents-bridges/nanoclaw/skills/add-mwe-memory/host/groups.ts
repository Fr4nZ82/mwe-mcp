/**
 * mwe-mcp bridge — what this bridge asks of a group folder on disk.
 *
 * Two questions, both answered by files rather than settings:
 *
 * 1. **Does this group carry the memory?** The `mwe` plugin stamped into
 *    it. Asked by the CLAUDE.md composer (a memory group must not be
 *    handed nanoclaw's own memory doctrine), by the scaffold cleaner and
 *    by the restart step; the container asks the same of its own mount in
 *    `mwe/active.ts`. Persona and mechanics arrive from the same template,
 *    so there is no second switch that can disagree with the first.
 * 2. **Is the memory tree still nanoclaw's untouched scaffold?** Which
 *    decides whether the cleaner may delete it or must leave it alone.
 *
 * Deliberately free of nanoclaw imports, like `config.ts` — the offline
 * smoke drives it directly.
 */
import fs from 'fs';
import path from 'path';

/** Where a stamped plugin's manifest sits inside a group folder. */
const PLUGIN_MANIFEST = path.join('plugins', 'mwe', 'plugin.json');

/** Group folders live here, under the fork root. */
const GROUPS_SUBDIR = 'groups';

/** Does this group folder carry the `mwe` plugin? */
export function groupCarriesMwe(groupDir: string): boolean {
  return fs.existsSync(path.join(groupDir, PLUGIN_MANIFEST));
}

/**
 * Folder names of every group carrying the plugin, sorted. Missing
 * `groups/` — a fork that has never spawned an agent — is an empty list,
 * not an error.
 */
export function mweGroupFolders(root: string): string[] {
  const groupsDir = path.join(root, GROUPS_SUBDIR);
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(groupsDir, { withFileTypes: true });
  } catch {
    return [];
  }
  return entries
    .filter((entry) => entry.isDirectory() && groupCarriesMwe(path.join(groupsDir, entry.name)))
    .map((entry) => entry.name)
    .sort();
}

/** One `agent_groups` row, as `ncl groups list --json` returns it. */
export interface GroupRow {
  id?: unknown;
  folder?: unknown;
}

/**
 * Agent-group ids to act on: the rows whose folder carries the plugin.
 *
 * The folder is the only thing the plugin is stamped into, and the id is
 * the only thing `ncl groups restart` accepts, so the two have to be put
 * side by side somewhere. A row missing either is skipped, never guessed
 * at — restarting the wrong group would kill somebody else's agent.
 */
export function mweGroupIds(root: string, rows: readonly GroupRow[]): string[] {
  const folders = new Set(mweGroupFolders(root));
  return rows
    .filter((row) => typeof row.id === 'string' && typeof row.folder === 'string' && folders.has(row.folder))
    .map((row) => String(row.id));
}

/** The three files `ensureMemoryScaffold` copies into a group, as relative paths. */
export const SCAFFOLD_FILES = ['index.md', 'system/index.md', 'system/definition.md'];

/** Every file under `dir`, as relative paths with `/` separators, sorted. */
function walk(dir: string, prefix = ''): string[] {
  const found: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) found.push(...walk(path.join(dir, entry.name), rel));
    else found.push(rel);
  }
  return found.sort();
}

/**
 * `null` when `memoryDir` is nanoclaw's scaffold and nothing else — the one
 * case in which deleting it destroys nothing. Otherwise the reason it is not,
 * in words an operator can act on.
 *
 * Byte comparison, not a filename check: an edited `definition.md` is somebody
 * changing the doctrine, and a memory that was never ours is not ours to
 * remove.
 */
export function whyMemoryTreeIsNotPristine(memoryDir: string, templatesDir: string): string | null {
  const present = walk(memoryDir);
  const extra = present.filter((rel) => !SCAFFOLD_FILES.includes(rel));
  if (extra.length > 0) return `it holds ${extra.join(', ')}`;
  const missing = SCAFFOLD_FILES.filter((rel) => !present.includes(rel));
  if (missing.length > 0) return `${missing.join(', ')} is missing, so this is not the untouched scaffold`;
  const edited = SCAFFOLD_FILES.filter(
    (rel) => !fs.readFileSync(path.join(memoryDir, rel)).equals(fs.readFileSync(path.join(templatesDir, rel))),
  );
  if (edited.length > 0) return `${edited.join(', ')} differs from nanoclaw's template`;
  return null;
}
