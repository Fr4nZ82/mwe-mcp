/**
 * Apply `add-mwe-memory` to a fork with no human in the room.
 *
 * nanoclaw's apply engine is a library, not a command: `scripts/skill-apply.ts`
 * run from the shell only *plans*. The pipeline posture the engine documents is
 * to call `applySkill` with an `inputs` map and no `resolveInput`, so an
 * unanswered prompt defers instead of hanging. `inputsFromEnv` builds that map
 * from `NC_INPUT_<VAR>` variables.
 *
 * Run from the fork root, with the three inputs in the environment:
 *
 *     NC_INPUT_MWE_SERVER_URL=… NC_INPUT_MWE_ADMIN_SENDER=… \
 *     NC_INPUT_MWE_ADMIN_USER=… bun smoke_apply.ts
 *
 * The restart is skipped — CI owns process lifecycle — and every operator step
 * is printed, because a step a machine cannot take is exactly what the human
 * must be handed.
 */
import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';

const FORK = process.cwd();
const SKILL_DIR = '.claude/skills/add-mwe-memory';

const { applySkill, fullyApplied, firstFailureHint } = await import(path.join(FORK, 'scripts/skill-apply.ts'));
const { inputsFromEnv } = await import(path.join(FORK, 'scripts/skill-inputs.ts'));

const inputs = inputsFromEnv(fs.readFileSync(path.join(FORK, SKILL_DIR, 'SKILL.md'), 'utf-8'));

const result = await applySkill(SKILL_DIR, FORK, {
  inputs,
  exec: (cmd: string) => execSync(cmd, { cwd: FORK, encoding: 'utf-8', stdio: ['ignore', 'pipe', 'pipe'] }),
  skipEffects: ['restart'],
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

// The one thing the skill will not do for anybody: the token. The smoke needs
// one on disk for the host module to consider itself configured, and a fake is
// the only kind that belongs in a test.
fs.appendFileSync(path.join(FORK, '.env'), '\nMWE_TOKEN=test-jwt\n');

console.log('add-mwe-memory applied');
