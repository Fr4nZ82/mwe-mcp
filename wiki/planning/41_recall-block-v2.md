---
title: Recall block v2 — role-labelled sections
status: in-progress
---

# 41. Recall block v2 — role-labelled sections

Opened by the maintainer 2026-07-05, from reading a real injected block verbatim (recall trace
54, the "buongiorno Gandalf" turn): *"dovrebbe avere delle sezioni chiare: chi sei, le regole
generali e per l'utente, le tue ultime azioni (con l'utente che parla! non con altri), chi è
l'utente…"*. The composer rework **landed 2026-07-05** (steps 41a–41f + 41h): the block is now
role-labelled sections in a canonical order, whole-bullet fitted — current state in
[ingest-pipeline.md §recall block](../design-notes/ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate).
This page keeps the audit's verification results and the remaining work.

## What the 41a verification established (trace 54 + prod, 2026-07-05)

- **The rules-delivery path was never lost**: standing directives ride the per-turn
  `IngestResponse.rules` field (roadmap 29d), which the hermes bridge injects at the head of
  the same `<memory-context>` block — `system_prompt_block` carries only static instructions.
  Trace 54's `rules_block` did deliver the two rules active for franz (claude-code delegation +
  the Gandalf naming rule).
- **The "missing" TTS rules were data, not plumbing**: of the 6 behaviour rules on
  `hermes1/rules.md`, the 3 TTS rules are closed (`decay_reason=contradicted`, 2026-07-01/02)
  and the Ernest naming rule belongs to morgana (owner-scoped, correctly absent from a franz
  turn). Whether the TTS closures were wanted is a separate data question for the maintainer.
- **The real defect was the renderer**: both the rules block and the self block went through
  `truncate()`, which flattens every newline to a space and cuts mid-word at a raw char budget
  — that alone produced the one-line, "pers…"-cut block the audit read.

## Remaining

- [ ] 41i — Live watch after the next deploy: read fresh traces on the dashboard Traces page
  and tune the per-section budgets (`max_agent_identity_chars` 900 / `max_agent_history_chars`
  1400, recall-settings panel) against real blocks; verify the rules channel renders unflattened
  on the wire and `WHO IS SPEAKING` picks up the identity-wiki summaries.

## 41g — executed 2026-07-05 (decision: owner = the page's user, sender = the agent)

The class assessment had widened the two delegation-protocol facts to **9 active prod rows**
with `owner=user:hermes1, allow=[]` homed on other wikis' pages — each unreadable for the very
user whose page carried it (era-bug captures predating the both-sides discipline). The
maintainer chose **option B**: the fact moves into the sphere of the person the page is about —
`owner` = the page's principal, `sender` stays `user:hermes1` (agent provenance), `allow`
stays empty (the owner reads it directly). Swept the same day: 2 rows → `user:franz`, 5 →
`user:morgana`, 2 → `group:famiglia` (the subject is the collective). DB-only surgery
(`fact_index.owner_id`; the on-disk markers are key-only, the DB is the ACL authority), backup
`engine.db.bak-41g-*` kept beside the DB; the class query now returns zero.

## Decisions pinned by 41a (recorded, shipped)

- **YOUR RULES stays on the dedicated `rules` field** (29d stands): the field is now
  self-labelled server-side (`YOUR RULES (…)` header, apply-don't-relay wording), bridges
  inject it verbatim with no preamble; `rules.md` left the navigable set (channel-only).
- **WHO IS SPEAKING is always at most the sender wiki's one-line `_meta.summary`** — the full
  index prose only ever arrives via `NAVIGATED PAGES`, so the same prose is never injected
  twice.
- **Per-section budgets, not one shared budget** — nomination-style resource caps
  (`max_agent_identity_chars`, `max_agent_history_chars`, config-overridable + dashboard
  recall-settings), whole-bullet fitting, newest-first, oldest tail drops.
- **Strict history scoping via the exclusive partner tag** — at capture, an agent self-fact
  keeps only the served user's id among enrolled-user topics (mentions of other users are
  stripped); pre-existing double-tagged rows degrade gracefully.

## Sequencing

Sibling of the shipped recall pipeline (the composer lives in `ingest.rs`; the funnel itself
did not change) and of [group 3](3_context-model.md) (the bridge contract defines *where* the
block lands; this group defined *what it says*). Independent of the group-38 watch. The
recall-trace viewer (shipped) is the observation instrument for the 41i before/after
comparison.
