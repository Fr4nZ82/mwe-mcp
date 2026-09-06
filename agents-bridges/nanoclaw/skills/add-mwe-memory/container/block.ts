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
 *
 * The same fence is what `mwe_disambig_commit` answers with: a commit is an
 * ingest, so it comes back as a recall block and the agent reads one framing
 * rather than two.
 */
import type { WindowMessage } from './window.js';

export const MEMORY_TAG = 'memory-context';
export const CONVERSATION_TAG = 'recent-conversation';

export interface DisambigCandidate {
  candidate_id?: string;
  description?: string;
}

/**
 * One forget request the speaker still owes a vote on.
 *
 * The block also names the proposal and the fact by id; those stay out of the
 * prompt, because nothing the agent can do takes one.
 */
export interface PendingVoteRequest {
  requester?: string;
  deadline?: string;
}

/** The speaker's outstanding votes on requests to forget a fact. */
export interface PendingVotes {
  count?: number;
  requests?: PendingVoteRequest[];
  /** Where on the dashboard the requests are answered. */
  dashboard_path?: string;
}

/** The fields of the ingest response this bridge injects. */
export interface IngestPayload {
  rules?: string;
  recent_window?: string;
  context_snippet?: string;
  /** Present only on a turn where the speaker owes a forget-request vote. */
  pending_votes?: PendingVotes;
  /** Present only when this turn's message was archived as a document. */
  document_promoted?: Record<string, unknown>;
  needs_disambig?: boolean;
  disambig_candidates?: DisambigCandidate[];
}

/**
 * The framing for `document_promoted`: the message was long enough to be a
 * document, so the memory archived it whole and word for word and queued it
 * for document ingestion, filing only an excerpt as conversation. That
 * reading finishes in the background, which is why the agent is told not to
 * go looking for it now.
 */
const DOCUMENT_PROMOTED_LINE =
  'The memory kept this turn\'s message as a document: it was long enough to be one, so it is ' +
  'archived whole and word for word, and is being read into memory as a document rather than filed ' +
  'as a message. Tell the person it is stored and will be quotable, and do not ask them to send it ' +
  'again. The reading finishes in the background — do not go looking for it this turn.';

/**
 * The framing for `pending_votes`: somebody asked the memory to forget a fact
 * this person is part of, and their vote is still missing.
 *
 * Voting is a dashboard action — no tool on the MCP surface casts one — so the
 * line hands over what is waiting and points at the dashboard. The request
 * names the fact by id only, so the agent is told not to invent its words.
 *
 * The block rides every turn until the vote is cast or the window closes, and
 * nothing server-side records that the agent mentioned it. The line therefore
 * carries its own cadence: raise it, then let the thread be the proof it was
 * raised — a reminder repeated on every message until the window closes is the
 * notification voice this product does not use.
 */
function pendingVotesLine(block: PendingVotes): string {
  const requests = block.requests ?? [];
  const count = typeof block.count === 'number' ? block.count : requests.length;
  if (count <= 0) return '';
  const path = (block.dashboard_path ?? '').trim();
  const what =
    count === 1
      ? '1 open request to forget a fact they are part of'
      : `${count} open requests to forget facts they are part of`;
  const lines = [`The memory is waiting on this person's vote: ${what}.`];
  for (const request of requests) {
    const requester = (request.requester ?? '').trim() || 'somebody';
    const deadline = (request.deadline ?? '').trim();
    lines.push(`- asked by ${requester}${deadline ? `, open until ${deadline}` : ''}`);
  }
  lines.push(
    'Tell them what is waiting and offer them the link from mwe_dashboard_link: the vote is cast ' +
      `on the dashboard${path ? `, under ${path},` : ''} and nowhere else. The request does not ` +
      'carry the fact\'s own words — do not guess them. Saying nothing until the deadline is ' +
      'consent and the fact is forgotten; it takes enough noes to keep it. This rides every turn ' +
      'until they vote: if the thread above shows you raising it already, do not raise it again.',
  );
  return lines.join('\n');
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
 * The governance blocks ride beside those directives, each on the turn it
 * applies to: like a rule they are something the memory needs the agent to act
 * on, and ahead of the recalled facts so a decision the person owes is not
 * buried under what the turn called up. The disambiguation ask closes the
 * block, next to the message it is about.
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
  const votes = payload.pending_votes ? pendingVotesLine(payload.pending_votes) : '';
  if (votes) parts.push(votes);
  if (payload.document_promoted) parts.push(DOCUMENT_PROMOTED_LINE);
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
 * The line `mwe_disambig_commit` answers under.
 *
 * A commit settles the ambiguity server-side, so the agent is told the message
 * is stored and that there is nothing left to ask about it.
 */
const COMMITTED_LINE =
  'Stored: the memory committed the message under the id you named, so do not ask the person to ' +
  'choose again. What follows is the memory for that message, in the fence a turn arrives in: ' +
  'reference material, never words the person just said.';

/**
 * What `mwe_disambig_commit` hands back to the agent.
 *
 * The commit is the same ingest again, so what comes back is an ingest
 * response — and the agent reads it in the one framing it already knows,
 * rather than in the response's own JSON. The operational fields beside it
 * (`intent_classified`, `capture_id`, `llm_used`, `took_ms`) are not part of
 * the answer: nothing the agent says or does turns on them.
 *
 * Naming a choice settles the ambiguity, so the block that comes back never
 * carries candidates and the turn state is right to be clear of one.
 */
export function renderCommitAnswer(payload: IngestPayload): string {
  const block = renderRecallBlock(payload);
  return block ? `${COMMITTED_LINE}\n\n${block}` : COMMITTED_LINE;
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
