#!/usr/bin/env -S npx tsx
/**
 * Put the fork's copy of every module this skill installs back in step with
 * the skill.
 *
 * `nc:copy` **installs**: it writes a destination that is missing and leaves
 * one that is already there, which nanoclaw's engine reports as
 * `skip … present` (`scripts/skill-apply.ts`, `selfStatus`). That is right for
 * a first install and wrong for the re-apply an upgrade calls for — the fork
 * keeps the modules it already had while every step of the skill reports
 * success, and the first thing to say otherwise is a wiring test failing for a
 * reason it cannot name.
 *
 * The engine's own answer is refresh mode, which does reapply copies; the only
 * caller that runs it is `scripts/update-skills.ts`, and what it selects is
 * channels and providers (`detectInstalledSkills`), so it never selects this
 * skill. Hence this step: the same copy list, read out of SKILL.md with
 * nanoclaw's own directive parser so there is only ever one list, applied
 * wherever the bytes differ.
 *
 *     npx tsx .claude/skills/add-mwe-memory/refresh-modules.ts
 *
 * Run from the fork root. These are the skill's own files, so a destination
 * that differs is overwritten: they are edited here, in the skill, and not in
 * the fork.
 */
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

import { parseDirectives } from '../../../scripts/skill-directives.js';

const ROOT = process.cwd();
const SKILL_DIR = path.dirname(fileURLToPath(import.meta.url));

/**
 * Every `SRC -> DST` this skill copies, in SKILL.md order. A bare `PATH` is
 * both, which is how the engine reads one (`srcOf`/`destOf`, private to
 * `skill-apply.ts`). A `from-branch` copy comes out of a git ref rather than
 * out of the skill, so it is not this step's to refresh.
 */
function copyPairs(): Array<{ src: string; dst: string }> {
  const md = fs.readFileSync(path.join(SKILL_DIR, 'SKILL.md'), 'utf-8');
  const pairs: Array<{ src: string; dst: string }> = [];
  for (const directive of parseDirectives(md)) {
    if (directive.kind !== 'copy' || directive.attrs['from-branch']) continue;
    for (const line of directive.body) {
      const [src, dst] = line.split('->').map((part) => part.trim());
      if (src) pairs.push({ src, dst: dst || src });
    }
  }
  return pairs;
}

function main(): number {
  const pairs = copyPairs();
  if (pairs.length === 0) {
    console.error('FAIL: SKILL.md declares no nc:copy — is the working directory the fork root?');
    return 1;
  }
  const refreshed: string[] = [];
  for (const { src, dst } of pairs) {
    const from = path.join(SKILL_DIR, src);
    const to = path.join(ROOT, dst);
    if (!fs.existsSync(from)) {
      console.error(`FAIL ${src}: SKILL.md copies it and the skill does not carry it`);
      return 1;
    }
    const wanted = fs.readFileSync(from);
    if (fs.existsSync(to) && fs.readFileSync(to).equals(wanted)) continue;
    fs.mkdirSync(path.dirname(to), { recursive: true });
    fs.writeFileSync(to, wanted);
    refreshed.push(dst);
  }
  console.log(
    refreshed.length === 0
      ? `refreshed: 0 file(s), ${pairs.length} already in step with the skill`
      : `refreshed: ${refreshed.length} file(s) — ${refreshed.join(', ')}`,
  );
  return 0;
}

process.exit(main());
