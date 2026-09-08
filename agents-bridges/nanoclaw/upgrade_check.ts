/**
 * The upgrade path of the reach-ins, driven over the scratch fork — run via
 * smoke.sh, from the fork root.
 *
 * A fork is upgraded by running `apply-fork-patches.ts` again, over files the
 * PREVIOUS version of the skill wrote. That is the run nobody sees fail: the
 * edits still apply, and an edit whose own lines changed shape ends up in the
 * file twice — the new form beside the old one — which bun tolerates and `tsc`
 * does not.
 *
 * So this rebuilds those older installs and re-applies. The old text is not
 * copied here: it comes from each patch's own `upgrades` list, which is where
 * the script keeps what an earlier version wrote. Every entry gives the older
 * install — the earlier form of the lines, alone — and an entry whose lines
 * the current edit rewrites in place gives the doubled file too: the earlier
 * form beside the current one, which is what an upgrade run without this
 * handling leaves behind. Both shapes are out there on installed forks.
 *
 * Each must come back byte for byte as a freshly applied fork, and `--remove`
 * on each must give nanoclaw's own file back.
 */
import { spawnSync } from 'child_process';
import fs from 'fs';
import path from 'path';

const FORK = process.cwd();
const PATCHER = path.join(FORK, '.claude/skills/add-mwe-memory/apply-fork-patches.ts');

const { PATCHES } = (await import(PATCHER)) as {
  PATCHES: Array<{
    file: string;
    anchor: string;
    replacement: string;
    upgrades?: Array<{ from: string; to: string }>;
  }>;
};

/**
 * Every patched file as this fork has it, applied.
 *
 * The patcher works on all of them at once — a `--remove` run below strips the
 * whole skill, not the one file under test — so the check puts every one of
 * them back after each state it drives, and the fork is left applied.
 */
const applied = new Map<string, string>(
  [...new Set(PATCHES.map((p) => p.file))].map((file) => [
    file,
    fs.readFileSync(path.join(FORK, file), 'utf-8'),
  ]),
);

function restoreAll(): void {
  for (const [file, text] of applied) fs.writeFileSync(path.join(FORK, file), text);
}

let checked = 0;

function fail(message: string): never {
  console.error(`FAIL: ${message}`);
  process.exit(1);
}

/** The patcher, as an operator runs it. */
function run(...args: string[]): void {
  const proc = spawnSync('bun', [PATCHER, ...args], { cwd: FORK, encoding: 'utf-8' });
  if (proc.status !== 0) fail(`the patcher exited ${proc.status}: ${proc.stdout}${proc.stderr}`);
}

/** nanoclaw's own file, as the clone has it in git. */
function pristine(file: string): string {
  const proc = spawnSync('git', ['show', `HEAD:${file}`], { cwd: FORK, encoding: 'utf-8' });
  if (proc.status !== 0) fail(`could not read ${file} from the fork's git: ${proc.stderr}`);
  return proc.stdout;
}

/**
 * The lines one edit adds — its replacement with the anchor taken out.
 *
 * An edit that rewrites its own anchor line has no such delta, and its
 * upgrades name a `to` instead of an empty one; those are handled by
 * substitution and never need this.
 */
function addedLines(patch: { anchor: string; replacement: string }): string {
  return patch.replacement.replace(patch.anchor, '');
}

for (const patch of PATCHES) {
  for (const upgrade of patch.upgrades ?? []) {
    const current = applied.get(patch.file)!;
    if (!current.includes(patch.replacement)) fail(`${patch.file} is not in its applied state — run the patcher first`);

    const states: Array<{ what: string; text: string }> = [];
    if (upgrade.to === '') {
      // The current edit writes these lines itself, so the older install has
      // the earlier form where the current one now sits, and the doubled file
      // has both. Which side the leftover lands on follows from the anchor:
      // an edit that opens with its anchor got the new lines spliced in above
      // the old ones, and an edit that closes with it got them below.
      const now = addedLines(patch);
      const newFirst = patch.replacement.startsWith(patch.anchor);
      states.push({ what: 'the older install', text: current.replace(now, upgrade.from) });
      states.push({
        what: 'the doubled file',
        text: current.replace(now, newFirst ? now + upgrade.from : upgrade.from + now),
      });
    } else {
      states.push({ what: 'the older install', text: current.replace(upgrade.to, upgrade.from) });
    }

    for (const state of states) {
      if (state.text === current) fail(`could not build ${state.what} of ${patch.file} — the upgrade entry misses`);

      fs.writeFileSync(path.join(FORK, patch.file), state.text);
      run();
      const upgradedFile = fs.readFileSync(path.join(FORK, patch.file), 'utf-8');
      restoreAll();
      if (upgradedFile !== current) {
        fail(`${state.what} of ${patch.file} did not upgrade to a freshly applied fork`);
      }

      fs.writeFileSync(path.join(FORK, patch.file), state.text);
      run('--remove');
      const removedFile = fs.readFileSync(path.join(FORK, patch.file), 'utf-8');
      restoreAll();
      if (removedFile !== pristine(patch.file)) {
        fail(`--remove on ${state.what} of ${patch.file} did not restore nanoclaw's own file`);
      }

      checked++;
      console.log(`ok   ${state.what} of ${patch.file} upgrades in place, and removes clean`);
    }
  }
}

if (checked === 0) fail('no patch carries an earlier form — the check ran over nothing');
console.log(`${checked} upgrade state(s) checked`);
