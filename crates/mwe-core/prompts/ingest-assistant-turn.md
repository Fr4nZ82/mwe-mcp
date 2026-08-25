---
name: ingest-assistant-turn
description: The `ingest` classifier's rules for a turn the consumer agent feeds back as its OWN prior reply (`author: assistant`) — keep the durable sediment it synthesised, drop the rest. Appended to the turn context by `ingest::build_prompt`, and ONLY on such a turn.
version: 1.0
default_version_at_bootstrap: v1.0
part_of: ingest
appended_when: the turn carries `author: assistant`
---

# Prompt: ingest-assistant-turn

A **part** of the `ingest` classifier, not a slot of its own. It carries the
rules that apply when the agent's own reply is fed back for extraction, and it
reaches the model only on those turns.

## Why it is a part and not a section of `ingest.md`

It is ~9 700 characters a normal turn has no use for — one turn in five on a
real week — and `ingest.md` is already ~100 000 characters against the
smallest model in the fleet, which measurably stops acting on what sits far
from the work.

It rides the **turn**, never the system prompt. The system half is what the
prompt cache keys on: it is byte-identical call to call, which is why two
thirds of this slot's prompt tokens are served from cache. A block appended to
it conditionally would make two prefixes out of one, and the rarer of them
would mostly pay the write instead of reading it.

## Runtime contract

- **Call site**: `crate::ingest::build_prompt`, at the top of the turn context,
  when `request.author` is `MessageRole::Assistant`.
- **Placeholders**: none.
- **Output**: none of its own — it shapes the `extractions` array the
  `ingest` prompt defines.

## Prompt

```text
When `author: assistant`, the `text` of this turn is **your own previous reply** to the user named by `sender_id` — fed back to you so the memory keeps YOUR half of the conversation, not just the user's. The server otherwise forgets everything you concluded, advised, or worked out: a deadline you read off a document, a recommendation you gave, a decision you reached together. Your job here is to mine your own words for the **durable sediment** and drop everything else.

DEFAULT HARD TO SKIP. Most replies carry nothing new — they answer, rephrase, or restate what the user already said (already captured on the user's own turn). Capture ONLY genuinely new, durable synthesis that is YOURS. When in doubt, `intent: "skip"` with an empty `extractions` array. Intent on an assistant turn is only ever `capture` (something durable survived) or `skip` — never `recall` or `structural`.

Classify what your reply states into one of six kinds; three ever produce an extraction (2, 3, 6):

1. **Pleasantries / filler / meta** — "of course, I will see to it!", "hugs 💕", "let me know", "there you go" → **skip**.
2. **Episodic / relational sediment** — that a topic was discussed and what you concluded or worked out, anchored to the turn's date. Emit ONE compact extraction, third person, `subject_id: "user:<sender>"`, `fact_type: "episode"` (or `"plan"` when it is a forward commitment with a date). Store the **distilled** episode, never your phrasing. This is what later lets the agent say "we had talked about this already".
3. **Personalised advice / a decision tied to a specific person** — a recommendation you gave, or a choice reached together, bound to someone's situation. Store it, `fact_type: "plan"` or `"preference"` as fits, owned by its **subject** — the enrolled user whose plan or situation it is. That is the sender in the normal case (`subject_id: "user:<sender>"`). But when this turn's text explicitly establishes that ANOTHER `known_users` entry is the one who must know and act on it — the sender said THAT person will do it, the advice exists FOR them — the subject is that user (the `subject_id` section's ABOUT-includes-FOR necessity test: would THEY need this fact in their own memory to act?): the subject axis is the subject, not the interlocutor. Resolve the subject with the same discipline as a relationship fact: named in the text AND present in `known_users`; the roster never supplies an identity the conversation did not give, and a mere mention is not a subject. A non-enrolled beneficiary leaves the fact owned by the sender, the name in the prose. When the subject is not the sender, THE BENEFICIARY RULE below governs the `body`.
4. **Generic, regenerable knowledge** — a how-to or definition you produced from general knowledge ("how to boil an egg", "what an IBAN is") → **skip**. You can regenerate it any time; filing it in the user's wiki is pollution. Keep it ONLY if it is durable, notable, AND you set `subject_id: "global"` — and even then prefer skip. The line: *regenerable on its own → skip; bound to this user or this conversation → store.*
5. **The user correcting you** is NOT here — a reprimand rides the USER's turn as a `behaviour_rule` (Part 7b). On an assistant turn you are reading your OWN words, so there is no user correction to capture.
6. **About YOURSELF — your own activity, what you did for this user, or a lesson about yourself** — "I helped the user with application X", "I tend to forget deadlines". This is your *own-eye* view, distinct from a fact about the user. Emit an extraction with `subject_id: "self"`: the engine files it in YOUR own wiki, owned by you and tagged with this user — your **emergent identity** (set `salience: "high"` for a defining trait, so it consolidates onto your own `@profile.md` card) and your **history with this user**. ROUTINE EXECUTION IS NOT SEDIMENT: running a command the user asked for, deleting a temp folder, sending a file, answering a question, confirming known data — none of these earns a self-fact (nor a user-side fact). A kind-6 fact must be durable ABOUT YOU: a lesson learned, a recurring pattern, a capability exercised for the first time, a milestone in the relationship. The SAME exchange can yield BOTH a fact about the user (kind 2/3, `subject_id: "user:<sender>"`, in their wiki) AND a self-fact (kind 6, `subject_id: "self"`, in yours) — but ONLY when each side is INDEPENDENTLY durable and each side's subject matches its wiki: the user-side fact must state something about the USER or their world that stands on its own; a sentence whose grammatical subject is "the agent" is NEVER a user-side fact — it is kind 6 alone, in your wiki, or nothing. One event never files twice just because two wikis exist.

THE RESOLVED-VALUE RULE — the case that matters most. When your reply states a concrete value you WORKED OUT — a deadline computed from a document, a date resolved, an amount calculated — and it is durable and NOT already in `recalled_memory`, that is exactly kind 2/3: capture it, with the resolved value in the `body` and the validity interval set (Part 3, resolve against `current_time`). This is the synthesis the server would otherwise lose, because the user never stated it — you did.

THE BENEFICIARY RULE — the `body` narrates what actually happened on THIS channel. You were talking to `sender_id`; a third party was not in the conversation and was told nothing. When a kind-3 fact is owned by another enrolled user (the subject rule above), write the body as advice that PASSED THROUGH the sender — «The agent explained to <sender> what <subject> must check…» — NEVER as an interaction with the subject («gave <subject> a checklist», «briefed <subject>»): that phrasing asserts a conversation and a delivery that never happened, and the subject will later read their own memory and find an exchange they never had. The delivery to the subject is the sender's job (or a future notification channel's), not a fact you may state.

NO TRANSCRIPT. Store the sediment, never the exchange. One distilled fact per durable point; never quote yourself or the user, never save the reply verbatim.

ANTI-LOOP — do not re-capture what you recalled. If something your reply states is already present in `recalled_memory`, you RECALLED it, you did not derive it — **skip** it. The recall block shows you what is already stored; re-saving it inflates confidence in a loop. Only newly-synthesised material survives. The canonical echo is IDENTIFICATION: the user asks who they are or what you know about them, and your reply recites their identity card from recall ("You are Francesco B., born on …, who works as …"). NOTHING in that reply is new — no bio extraction, and no episode either ("the agent correctly identified the user" is routine operation, not durable sediment): the whole turn is a `skip`.

ATTRIBUTION IS AUTOMATIC. The engine stamps every fact you emit on an assistant turn as agent-derived (`sender =` you, a lower-trust inference) — you do NOT express it. You only choose `subject_id`: the SUBJECT for kinds 2–3 — `"user:<sender>"` in the normal case, another enrolled user only per kind 3's necessity test — `global` for a kept kind 4, and `"self"` for kind 6 (the engine routes a `"self"` fact into your own wiki — it knows which one that is). Do NOT emit `engine_rule` or `behaviour_rule` on an assistant turn (those are the USER's directives to the system, not yours). When your reply records completing or abandoning something ("done, I have sent it"), that is ordinary kind-2 sediment: write it down as a fact like any other.

Worked calls (`author: assistant`):
- Your reply "I have read the letter you uploaded: the deadline to file the guardianship order is 27 June 2026." → TWO extractions, the two sides of the one event: (a) kind 2/3 about the USER — `subject_id: "user:<sender>"`, `fact_type: "plan"`, `body: "From the letter the user uploaded, the deadline to file the guardianship order is 27 June 2026."`, `valid_to: "2026-06-27T00:00:00Z"` (the synthesis the user never stated — it lived only in YOUR reply); (b) kind 6 about YOU — `subject_id: "self"`, `fact_type: "episode"`, `body: "The agent helped the user with the maternity claim, pinning down the order's deadline."` (your-eye view, filed in your own wiki).
- "Of course! I will see to it, hugs 💕" → `skip` (pleasantry).
- "A hard-boiled egg takes about 8 to 9 minutes." → `skip` (generic, regenerable).
- "I suggest going to an advice centre about the maternity claim." → ONE extraction, kind 3, `subject_id: "user:<sender>"`, `fact_type: "plan"`.
- (talking to Frodo, who said Galadriel will do the viewing; `galadriel` in `known_users`) "Here is what to check when you view the used car: oil leaks, the clutch, the state of the wheels." → ONE extraction, kind 3, `subject_id: "user:galadriel"` — the inspection plan is HERS to act on (necessity test) — with the body phrased per THE BENEFICIARY RULE: `body: "The agent explained to Frodo what Galadriel must check when viewing the used car: oil leaks, the clutch, the state of the wheels."` — NOT «gave Galadriel a checklist» (no such exchange happened).
- Same reply, but nobody else is named — Frodo does the viewing himself → kind 3, `subject_id: "user:<sender>"` as usual.
- "As you were telling me, you live in Bologna." → `skip` (recall echo — already in `recalled_memory`).
```
