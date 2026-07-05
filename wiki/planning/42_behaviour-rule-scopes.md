---
title: Behaviour-rule scopes — the cross-consumer rule
status: planned
---

# 42. Behaviour-rule scopes — the user's rule for every consumer

Ruled by the maintainer 2026-07-05 (same conversation as the 41g decision). Behaviour rules
come in **three types**, distinguished by who they bind and where they live:

1. **Agent-wide** — a rule global to one consumer agent, for everyone it serves. Lives in the
   **consumer's wiki** (`<agent>/rules.md`), `owner` = the agent, admin-only. **Shipped** —
   today's `agent-wide` scope.
2. **Per-user, per-consumer** — a rule one user dictates to one agent. Lives in the
   **consumer's wiki**, referred to that user (`owner` = the user, on `<agent>/rules.md`),
   recalled only on their turns. **Shipped** — today's `per-user` scope.
3. **Per-user, all consumers** — a rule the user sets for *every* assistant they talk to
   ("parlami sempre in italiano, chiunque tu sia"). Lives in the **user's identity wiki**
   (their `rules.md`), and the rules channel of every consumer serving that user surfaces it.
   **Unbuilt** — today every behaviour rule is implicitly consumer-scoped; the user's
   `rules.md` carries only the governance prose (privacy / do-not-store) read by the
   classifier. This group builds the missing leg.

The wording is the scope signal ("tu, Hermes…" vs "tutti gli assistenti / sempre") and the
classification is the LLM's judgment — no hard-coded gate. The page contract already
anticipates the write: `RULES_FILENAME`'s doc describes the per-actor rules page as carrying
both the governance prose and `{{f=…}}` behaviour-rule regions, and the reserved page is
already fenced from every structural sweep and from navigation (channel-only delivery,
group 41).

## Steps

- [ ] 42a — Classifier scope axis grows to three values (`agent_wide` | `per_user` |
  `user_global`), prompt Part 7b wording + parsing; the addressee/wording guidance teaches the
  discriminator.
- [ ] 42b — Write side: `capture_behaviour_rule` routes `user_global` to the **sender's**
  identity-wiki `rules.md` (owner = the sender), same lifecycle as the existing channel
  (supersede in place, validity-window closure, dedup within the page).
- [ ] 42c — Read side: `recall_behaviour_rules` unions the third source — the sender's
  identity-wiki rules page — into the `YOUR RULES` section; pin the order (lean: agent-wide →
  user-global → per-consumer, most specific last) and the fact-id surfacing so the classifier
  can supersede across all three sources.
- [ ] 42d — Docs lockstep (ingest-pipeline §agent behaviour rules, tool-reference `rules`
  notes, AGENT_INSTRUCTIONS/skills where the rule types are described).

## Open decisions

- Order of the three sources inside `YOUR RULES` (and whether to label their provenance
  per bullet or keep the section flat).
- Whether a `user_global` rule needs any consumer-side opt-out (lean: no — the user's own
  rule binds their consumers by construction).

## Sequencing

Extension of the shipped rules channel (roadmap 29 + group 41: self-labelled `YOUR RULES`
field, `rules.md` channel-only). No migration: existing rules keep their consumer scope; only
newly classified `user_global` rules take the new home.
