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
const { renderRecallBlock, renderCommitAnswer, renderConversation, assemblePrompt, MEMORY_TAG, CONVERSATION_TAG } =
  await import('./block.js');

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

  /** A deadline `hours` from now, in the RFC 3339 the server sends. */
  const deadlineIn = (hours: number): string => new Date(Date.now() + hours * 3_600_000).toISOString();

  it('hands over the owed vote: what is waiting, where it is cast, what silence costs', () => {
    const block = renderRecallBlock({
      context_snippet: 'RELEVANT MEMORY\nnothing much',
      pending_votes: {
        count: 1,
        requests: [{ requester: 'bob', deadline: deadlineIn(6) }],
        // The host completes the page before the container sees it, so this is
        // the shape that actually arrives.
        dashboard_path: 'https://memory.example/dashboard/chat',
      },
    });
    expect(block).toContain('1 open request');
    expect(block).toContain('asked by bob, open until ');
    expect(block).toContain('mwe_dashboard_link');
    expect(block).toContain('https://memory.example/dashboard/chat');
    expect(block).toContain('Saying nothing until the deadline is consent');
    // The block names the fact by id: an agent that fills the gap in would be
    // telling the person what somebody wants forgotten, invented.
    expect(block).toContain('do not guess them');
    // Six hours out is inside the day: this is the turn to speak.
    expect(block).toContain('The deadline is within a day, so raise it this turn');
    // Governance is not memory: it rides with the rules, ahead of the facts.
    expect(block.indexOf('waiting on this person')).toBeLessThan(block.indexOf('RELEVANT MEMORY'));
  });

  it('keeps quiet about a vote whose deadline is days away', () => {
    // The window is seven days and the block rides every turn of it. Raising
    // it on all of them is the notification voice this product does not use;
    // raising it in the last day is a person remembering something for you.
    const block = renderRecallBlock({
      pending_votes: {
        count: 1,
        requests: [{ requester: 'bob', deadline: deadlineIn(5 * 24) }],
        dashboard_path: 'https://memory.example/dashboard/chat',
      },
    });
    expect(block).toContain('The deadline is still more than a day away: do not bring this up');
    expect(block).not.toContain('raise it this turn');
    // It is still there to answer a direct question with, and the link with it.
    expect(block).toContain("waiting on this person's vote");
    expect(block).toContain('https://memory.example/dashboard/chat');
  });

  it('speaks when the nearest of several deadlines is close, not when the last is', () => {
    const block = renderRecallBlock({
      pending_votes: {
        count: 2,
        requests: [{ requester: 'bob', deadline: deadlineIn(5 * 24) }, { requester: 'carol', deadline: deadlineIn(3) }],
      },
    });
    expect(block).toContain('The deadline is within a day, so raise it this turn');
  });

  it('treats a deadline it cannot read as due — a missed vote costs the fact', () => {
    const block = renderRecallBlock({
      pending_votes: { count: 1, requests: [{ requester: 'bob', deadline: 'whenever' }] },
    });
    expect(block).toContain('The deadline is within a day, so raise it this turn');
  });

  it('says a long paste became a document, and not to go looking for it', () => {
    const block = renderRecallBlock({
      document_promoted: { catalog_id: 'c-2026-06-12-doc-001.txt', job_id: 'j-1', existing: false },
    });
    expect(block).toContain("kept this turn's message as a document");
    expect(block).toContain('do not ask them to send it again');
    expect(block).toContain('do not go looking for it this turn');
  });

  it('stays silent about both when the turn carries neither', () => {
    const block = renderRecallBlock({ context_snippet: 'RELEVANT MEMORY\nthe dog is called Frodo' });
    expect(block).not.toContain('vote');
    expect(block).not.toContain('document');
    // An empty framing line would leave a blank paragraph in the fence.
    expect(block).not.toContain('\n\n\n');
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

describe('the answer a disambiguation commit gives', () => {
  it('is the recall block for the stored message, framed — not the response as JSON', () => {
    const answer = renderCommitAnswer({
      rules: 'YOUR RULES (standing directives)\n- speak Italian',
      context_snippet: 'RELEVANT MEMORY\nthe dog is called Frodo',
      ...({ intent_classified: 'capture', capture_id: 'f-1', llm_used: 'sonnet', took_ms: 812 } as Record<
        string,
        unknown
      >),
    });
    expect(answer.startsWith('{')).toBe(false);
    expect(answer).toContain('Stored:');
    expect(answer).toContain('do not ask the person to choose again');
    expect(answer).toContain(`<${MEMORY_TAG}>`);
    expect(answer).toContain('YOUR RULES');
    expect(answer).toContain('the dog is called Frodo');
    // The operational fields are the server's bookkeeping: nothing the agent
    // says or does turns on them, so they are not part of the answer.
    for (const field of ['intent_classified', 'capture_id', 'llm_used', 'took_ms']) {
      expect(answer).not.toContain(field);
    }
  });

  it('says the message is stored even when the memory had nothing to recall', () => {
    const answer = renderCommitAnswer({});
    expect(answer).toContain('Stored:');
    expect(answer).not.toContain(`<${MEMORY_TAG}>`);
  });

  it('carries the governance a commit turn earns, not only the facts', () => {
    const answer = renderCommitAnswer({
      pending_votes: {
        count: 1,
        requests: [{ requester: 'bob', deadline: new Date(Date.now() + 3_600_000).toISOString() }],
        dashboard_path: 'https://memory.example/dashboard/chat',
      },
    });
    expect(answer).toContain("waiting on this person's vote");
    expect(answer).toContain('https://memory.example/dashboard/chat');
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
