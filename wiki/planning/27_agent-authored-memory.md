---
title: Agent-authored memory — the consumer remembers its own turn
status: gated
---

# 27. Agent-authored memory (the consumer remembers its own half of the turn)

**Core shipped 2026-06-26 (27a–27d).** Ingest used to capture facts from the user's message only;
the assistant's turn rode the prompt as `recent_messages` context but was never an extraction
*source*, so the consumer was amnesiac about its own half — what it concluded, advised, or was
corrected on (surfaced live: an agent read an INPS maternity letter and stated the deadline in its
reply, yet nothing persisted). Now the **assistant turn is a second, special-ruled extraction
source** (`author: "assistant"`, prompt Part 12: skip filler / store episodic / store personalised
advice / skip regenerable / route a correction into the agent's own wiki), attributed
`sender=<agent>` with `owner` staying the user; the agent also **writes facts about itself**
(`owner_id: "self"`) and every turn the recall block **leads with its self-context**
(`recall_agent_self`). Current state:
[ingest-pipeline.md](../design-notes/ingest-pipeline.md),
[tool-reference §author](../protocol/tool-reference.md#wiki_ingest_message).

## The forward vision — the agent as a first-class member (27d-rem)

mwe-mcp is multi-user and every principal is treated alike — the per-fragment ACL governs any
`sender`/`owner`, human or not. If the agent is genuinely a team member, it is **a user**, and its
wiki (`is_agent`, 4i) is its **autobiography**, not just a routing sink. The capture side is
shipped; what remains is to give that wiki the same memory machinery a human's gets, so the agent
accrues a **continuous self** with growth and relationships remembered rather than a flat
append-only log.

## Remaining work

- [ ] 27d-rem — **Deepen the emergent self.** REM consolidation already covers the agent wiki (a
  normal `wiki-user`, no agent/system-user exclusion in `dream`/`rem`), so the generic
  promote → compile → reorg pipeline consolidates its self-facts (the high-salience identity onto
  its index). What remains: **organic forgetting** (item 11, a separate unbuilt subsystem) so the
  agent's self decays like a human's rather than only accreting, plus any agent-specific REM tuning
  that surfaces once the self-corpus grows in real use.

## Relations

- **Complements document extraction (9j / 21):** that is the exhaustive source capture; this is the
  synthesis capture.
- **Item 4c (behaviour-rule routing):** the "feedback / correction" category rides the same
  consumer-own-wiki + `sender`-attributed routing.
- **Item 15 (self-correcting REM):** the "remember when corrected" category is the human-feedback
  intake for that loop. Agent-derived facts carry a lower trust tier (their `sender=<agent>`
  provenance), so they can be down-weighted / audited.
- **Item 8 (cross-consumer reminders):** capturing a deadline as a dated fact is here; firing an
  active reminder on it is still item 8.
