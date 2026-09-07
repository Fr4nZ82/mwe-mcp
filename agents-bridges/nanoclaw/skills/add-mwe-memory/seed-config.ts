#!/usr/bin/env -S npx tsx
/**
 * Write (or top up) `mwe.json` at the fork root.
 *
 * Only the settings the installer can know go in: the endpoint, and the one
 * person setting this up. Everything else keeps its default until an operator
 * edits the file. An existing file is merged into, never overwritten — the
 * senderMap an operator has been building up is theirs.
 *
 *     npx tsx .claude/skills/add-mwe-memory/seed-config.ts <serverUrl> <senderKey> <mweUserId>
 *
 * The bearer token is deliberately not an argument. It lives in `.env` as
 * `MWE_TOKEN`, put there by the person who minted it, and it never passes
 * through an installer, a log line or a command history.
 */
import fs from 'fs';
import path from 'path';

const [serverUrl, senderKey, mweUserId] = process.argv.slice(2);
if (!serverUrl || !senderKey || !mweUserId) {
  console.error('usage: seed-config.ts <serverUrl> <channel>:<platform-id> <mwe-user-id>');
  process.exit(2);
}
if (!/^[a-z0-9]+$/.test(mweUserId)) {
  console.error(`"${mweUserId}" is not a mwe user id — they are lowercase letters and digits, nothing else`);
  process.exit(2);
}

const target = path.join(process.cwd(), 'mwe.json');
let existing: Record<string, unknown> = {};
if (fs.existsSync(target)) {
  try {
    existing = JSON.parse(fs.readFileSync(target, 'utf-8')) as Record<string, unknown>;
  } catch {
    console.error('mwe.json exists but is not valid JSON — fix or remove it, then run this again');
    process.exit(1);
  }
}

const senderMap = { ...((existing.senderMap as Record<string, string>) ?? {}), [senderKey]: mweUserId };
const merged = {
  serverUrl,
  senderMap,
  operatorSender: existing.operatorSender ?? senderKey,
  locale: existing.locale ?? '',
  maxWindow: existing.maxWindow ?? 16,
  groups: existing.groups ?? [],
  unroutable: existing.unroutable ?? [],
  eventsEnabled: existing.eventsEnabled ?? true,
  eventsPollSeconds: existing.eventsPollSeconds ?? 30,
  dashboardUrl: existing.dashboardUrl ?? '',
};
fs.writeFileSync(target, `${JSON.stringify(merged, null, 2)}\n`);
console.log(`mwe.json written: ${serverUrl}, ${Object.keys(senderMap).length} sender(s) mapped`);
