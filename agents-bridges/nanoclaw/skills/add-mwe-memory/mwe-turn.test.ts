/**
 * The container half of the mwe-mcp bridge: the recent window, the recall
 * block, and which rows of a batch count as somebody speaking.
 *
 * Runs under bun, beside the runner code it guards.
 */
import { describe, it, expect, beforeEach } from 'bun:test';
import fs from 'fs';
import os from 'os';
import path from 'path';

const WINDOW_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'mwe-window-'));
process.env.NANOCLAW_MWE_SESSION_DIR = WINDOW_DIR;

const { appendToWindow, clearWindow, windowMessages, DEFAULT_MAX_WINDOW } = await import('./window.js');
const { renderRecallBlock, renderConversation, assemblePrompt, MEMORY_TAG, CONVERSATION_TAG } = await import(
  './block.js'
);

beforeEach(() => {
  clearWindow();
});

describe('the recent window', () => {
  it('keeps the newest messages and drops the oldest', () => {
    appendToWindow(
      Array.from({ length: 20 }, (_, i) => ({ role: 'user' as const, text: `m${i}`, timestamp: 't' })),
      4,
    );
    const kept = windowMessages();
    expect(kept).toHaveLength(4);
    expect(kept[0].text).toBe('m16');
    expect(kept[3].text).toBe('m19');
  });

  it('adopts the bound the host names, and keeps it for later turns', () => {
    appendToWindow([{ role: 'user', text: 'a', timestamp: 't' }], 2);
    appendToWindow([{ role: 'assistant', text: 'b', timestamp: 't' }]);
    appendToWindow([{ role: 'user', text: 'c', timestamp: 't' }]);
    expect(windowMessages().map((m) => m.text)).toEqual(['b', 'c']);
  });

  it('survives being read back from disk — a container restart keeps the thread', () => {
    appendToWindow([{ role: 'user', text: 'prima', timestamp: 't' }]);
    expect(JSON.parse(fs.readFileSync(path.join(WINDOW_DIR, 'mwe-window.json'), 'utf-8')).messages).toHaveLength(1);
    expect(windowMessages()[0].text).toBe('prima');
  });

  it('starts empty rather than throwing when the file is unreadable', () => {
    fs.writeFileSync(path.join(WINDOW_DIR, 'mwe-window.json'), 'not json');
    expect(windowMessages()).toEqual([]);
  });

  it('defaults to the contract window when nobody has named one', () => {
    expect(DEFAULT_MAX_WINDOW).toBe(16);
  });
});

describe('the recall block', () => {
  it('leads with the rules, then the other surfaces, then the recalled memory', () => {
    const block = renderRecallBlock({
      rules: 'YOUR RULES (standing directives)\n- speak Italian',
      recent_window: 'RECENT EXCHANGES ON YOUR OTHER CHANNELS WITH THIS USER: …',
      context_snippet: 'WHO IS SPEAKING\nalice',
    });
    expect(block.indexOf('YOUR RULES')).toBeLessThan(block.indexOf('RECENT EXCHANGES'));
    expect(block.indexOf('RECENT EXCHANGES')).toBeLessThan(block.indexOf('WHO IS SPEAKING'));
    expect(block.startsWith(`<${MEMORY_TAG}>`)).toBe(true);
  });

  it('never forwards the seed — nanoclaw writes its own reply', () => {
    const block = renderRecallBlock({
      context_snippet: 'WHO IS SPEAKING\nalice',
      // A seed in the payload must not reach the model.
      ...({ suggested_seed: 'Ciao Alice, ecco la risposta pronta.' } as Record<string, unknown>),
    });
    expect(block).not.toContain('risposta pronta');
  });

  it('names the candidates and the tool that commits the choice', () => {
    const block = renderRecallBlock({
      needs_disambig: true,
      disambig_candidates: [{ candidate_id: 'c1', description: 'Alice Rossi' }],
      context_snippet: '',
    });
    expect(block).toContain('c1: Alice Rossi');
    expect(block).toContain('mwe_disambig_commit');
  });

  it('is nothing at all when the memory returned nothing', () => {
    expect(renderRecallBlock({})).toBe('');
  });
});

describe('the prompt', () => {
  it('puts the volatile block and the window in front of the turn messages', () => {
    const prompt = assemblePrompt(
      renderRecallBlock({ context_snippet: 'RELEVANT MEMORY\nthe dog is called Frodo' }),
      renderConversation([{ role: 'user', text: 'prima', timestamp: '2026-09-06T10:00:00Z' }]),
      '<context timezone="Europe/Rome" />\n<message sender="alice">e il cane?</message>',
    );
    expect(prompt.indexOf(`<${MEMORY_TAG}>`)).toBe(0);
    expect(prompt.indexOf(`<${CONVERSATION_TAG}>`)).toBeGreaterThan(prompt.indexOf(`</${MEMORY_TAG}>`));
    expect(prompt.indexOf('<context timezone')).toBeGreaterThan(prompt.indexOf(`</${CONVERSATION_TAG}>`));
    expect(prompt.indexOf('e il cane?')).toBeGreaterThan(prompt.indexOf('prima'));
  });

  it('is the untouched batch when the memory gave nothing back', () => {
    expect(assemblePrompt('', '', '<message sender="alice">ciao</message>')).toBe(
      '<message sender="alice">ciao</message>',
    );
  });
});
