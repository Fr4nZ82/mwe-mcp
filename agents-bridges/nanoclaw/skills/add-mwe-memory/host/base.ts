/**
 * mwe-mcp bridge — the shared CLAUDE.md a memory group receives.
 *
 * nanoclaw composes `groups/<folder>/CLAUDE.md` on every spawn: the
 * template's persona first, then `@./.claude-shared.md`, which is a
 * symlink to `container/CLAUDE.md` mounted read-only at `/app/CLAUDE.md`.
 * That base teaches an agent where its memory lives on disk and tells it
 * to read `conversations/` to recall prior context. For a group whose
 * only memory is the memory server, those sentences are live
 * instructions to go and look somewhere the persona forbids — and they
 * sit *after* the persona, where a model reads them as the later word.
 *
 * So a group carrying the plugin gets the base with those sections
 * removed, written as a real file where the symlink would have gone. The
 * composer regenerates it every spawn, so it cannot drift from the fork's
 * own base, and a group without the plugin keeps the symlink untouched.
 *
 * Deliberately free of nanoclaw imports — the offline smoke drives it.
 */
import fs from 'fs';
import path from 'path';

import { groupCarriesMwe } from './groups.js';

/** The base nanoclaw mounts at `/app/CLAUDE.md`, host-side. */
const SHARED_BASE_SUBPATH = path.join('container', 'CLAUDE.md');

/**
 * Headings of the base's own sections a memory group must not receive.
 * `Memory` names the on-disk tree and calls durable facts its business;
 * `Conversation history` sends the agent to `conversations/` to recall
 * prior context. Recall arrives in the block instead, per reader and
 * redacted, and `mwe-wiring.test.ts` pins both headings so an upstream
 * rename fails there rather than silently shipping the sections again.
 */
export const DROPPED_BASE_SECTIONS = ['Memory', 'Conversation history'];

/**
 * The shared base for one group, or `null` when the group does not carry
 * the plugin and should keep nanoclaw's own base verbatim.
 *
 * Reading the base is not guarded: a fork without `container/CLAUDE.md`
 * has nothing to mount at `/app/CLAUDE.md` either, so failing here is
 * failing where the cause is visible.
 */
export function mweSharedBase(root: string, groupDir: string): string | null {
  if (!groupCarriesMwe(groupDir)) return null;
  const base = fs.readFileSync(path.join(root, SHARED_BASE_SUBPATH), 'utf-8');
  return dropSections(base, DROPPED_BASE_SECTIONS);
}

/**
 * Drop each named `## ` section — its heading and everything under it up
 * to the next `## ` heading or the end. Headings the base does not carry
 * are simply not there to drop.
 */
export function dropSections(markdown: string, headings: readonly string[]): string {
  const drop = new Set(headings.map((heading) => `## ${heading}`));
  const kept: string[] = [];
  let dropping = false;
  for (const line of markdown.split('\n')) {
    if (line.startsWith('## ')) dropping = drop.has(line.trim());
    if (!dropping) kept.push(line);
  }
  return `${kept.join('\n').trimEnd()}\n`;
}
