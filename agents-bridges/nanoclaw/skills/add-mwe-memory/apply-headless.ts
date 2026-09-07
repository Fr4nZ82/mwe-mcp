#!/usr/bin/env -S npx tsx
/**
 * Apply a NanoClaw skill with no human in the room.
 *
 * NanoClaw's apply engine is a library, not a command: `scripts/skill-apply.ts`
 * run from the shell only *plans*. The pipeline posture the engine documents is
 * to call `applySkill` with an `inputs` map and no `resolveInput`, so an
 * unanswered prompt defers instead of hanging; `inputsFromEnv` builds that map
 * from `NC_INPUT_<VAR>` variables, one per `nc:prompt` in the SKILL.md.
 *
 * Run from the fork root:
 *
 *     NC_INPUT_MWE_SERVER_URL=… NC_INPUT_MWE_ADMIN_SENDER=… NC_INPUT_MWE_ADMIN_USER=… \
 *     NC_SKIP_EFFECTS=restart \
 *     pnpm exec tsx .claude/skills/add-mwe-memory/apply-headless.ts .claude/skills/add-mwe-memory
 *
 * The skill directory is the one argument, so the same driver applies this
 * bridge's skill and NanoClaw's own `add-telegram` — which is how an installer
 * connects a channel without the pairing handshake.
 *
 * `NC_SKIP_EFFECTS` is a comma-separated list of `nc:run effect:` names the
 * caller performs itself, or performs differently:
 *   - `restart` when the caller restarts once at the end, after every skill;
 *   - `step` for an operator-interactive step (a pairing code, a QR link) the
 *     caller replaces with `ncl` calls it can make from what it already knows.
 *
 * Exits non-zero when the skill did not fully apply, naming what is missing:
 * a deferred prompt has no `NC_INPUT_` for it, and a bounced step is a real
 * failure with the prose beside it as the hint.
 */
import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';

const FORK = process.cwd();
const skillDir = process.argv[2];
if (!skillDir) {
  console.error('usage: apply-headless.ts <skill-dir relative to the fork root>');
  process.exit(2);
}

const { applySkill, fullyApplied, firstFailureHint } = await import(path.join(FORK, 'scripts/skill-apply.ts'));
const { inputsFromEnv } = await import(path.join(FORK, 'scripts/skill-inputs.ts'));

const inputs = inputsFromEnv(fs.readFileSync(path.join(FORK, skillDir, 'SKILL.md'), 'utf-8'));
const skipEffects = (process.env.NC_SKIP_EFFECTS ?? '')
  .split(',')
  .map((e) => e.trim())
  .filter(Boolean);

const result = await applySkill(skillDir, FORK, {
  inputs,
  exec: (cmd: string) => execSync(cmd, { cwd: FORK, encoding: 'utf-8', stdio: ['ignore', 'pipe', 'pipe'] }),
  skipEffects,
  onEvent: (event: { type: string; label?: string | null; kind?: string; ok?: boolean; text?: string }) => {
    if (event.type === 'step-end') console.log(`  ${event.ok ? 'ok  ' : 'FAIL'} ${event.label ?? event.kind}`);
    if (event.type === 'operator') console.log(`  manual step:\n${event.text}`);
  },
});

if (!fullyApplied(result)) {
  for (const deferred of result.deferred) console.error(`deferred: ${deferred}`);
  for (const task of result.agentTasks) console.error(`line ${task.line}: ${task.reason}`);
  const hint = firstFailureHint(result);
  if (hint) console.error(`${hint.headline}\n${hint.hint}`);
  process.exit(1);
}

console.log(`${skillDir} applied`);
