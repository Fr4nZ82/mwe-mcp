/**
 * mwe-mcp bridge — assembling the turn's prompt.
 *
 * The per-turn contract puts the recall block **after the stable content and
 * before the conversation** (point 3), so a provider's prompt cache keeps
 * hitting on the prefix. In nanoclaw the stable content is the system prompt:
 * the template persona (`groups/<folder>/CLAUDE.md`, imported first) plus the
 * runtime addendum the runner builds. Everything volatile therefore belongs at
 * the head of the turn prompt, which is what this module builds:
 *
 *     <memory-context>          ← recalled memory, this turn only
 *     <recent-conversation>     ← the consumer-owned window, verbatim
 *     <context timezone=…/>     ← nanoclaw's own formatted messages
 *     <message …>               ← what the agent must actually answer
 *
 * The two blocks are fenced and labelled because they are reference material,
 * not something a person said to the agent now — the persona says so, and the
 * fence is what makes that instruction checkable.
 */
import type { WindowMessage } from './window.js';

export const MEMORY_TAG = 'memory-context';
export const CONVERSATION_TAG = 'recent-conversation';

export interface DisambigCandidate {
  candidate_id?: string;
  description?: string;
}

/** The fields of the ingest response this bridge injects. */
export interface IngestPayload {
  rules?: string;
  recent_window?: string;
  context_snippet?: string;
  needs_disambig?: boolean;
  disambig_candidates?: DisambigCandidate[];
}

function escapeXml(value: string): string {
  return value.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

/**
 * The recall block, in the contract's canonical order.
 *
 * `rules` leads: they are binding directives about how to behave with this
 * person, kept structurally apart from recalled memory so a rule is never
 * mistaken for a fact. Each field is already self-labelled server-side, so it
 * is injected verbatim with no preamble of the bridge's own.
 *
 * `suggested_seed` is deliberately absent. It is a pre-drafted reply for a
 * consumer with no model of its own; nanoclaw always has one, and splicing a
 * ready-made answer into the turn invites the model to continue it instead of
 * treating it as reference.
 */
export function renderRecallBlock(payload: IngestPayload): string {
  const parts: string[] = [];
  const rules = (payload.rules ?? '').trim();
  if (rules) parts.push(rules);
  const recent = (payload.recent_window ?? '').trim();
  if (recent) parts.push(recent);
  const snippet = (payload.context_snippet ?? '').trim();
  if (snippet) parts.push(snippet);

  if (payload.needs_disambig) {
    const lines = (payload.disambig_candidates ?? [])
      .map((c) => `- ${c.candidate_id}: ${c.description}`)
      .join('\n');
    parts.push(
      'The memory cannot tell who or what this message meant, and will not store it until it can. ' +
        'Ask the person which one they mean, in one plain question, then call mwe_disambig_commit ' +
        `with the id they picked:\n${lines}`,
    );
  }
  if (parts.length === 0) return '';
  return `<${MEMORY_TAG}>\n${parts.join('\n\n')}\n</${MEMORY_TAG}>`;
}

/**
 * The recent window, oldest first, as the thread this turn continues.
 *
 * Under the sessionless model the provider gets a fresh query every turn, so
 * without this the agent would have no memory of the last thing it said. The
 * newest messages — the ones to answer — are the formatted ones below, not
 * these.
 */
export function renderConversation(messages: WindowMessage[]): string {
  if (messages.length === 0) return '';
  const rendered = messages
    .map(
      (m) =>
        `  <turn role="${m.role}" time="${escapeXml(m.timestamp)}">${escapeXml(m.text)}</turn>`,
    )
    .join('\n');
  return `<${CONVERSATION_TAG}>\n${rendered}\n</${CONVERSATION_TAG}>`;
}

/** Stable content is already in the system prompt: block, then window, then the turn. */
export function assemblePrompt(recallBlock: string, conversation: string, formattedMessages: string): string {
  return [recallBlock, conversation, formattedMessages].filter((part) => part.trim()).join('\n\n');
}
