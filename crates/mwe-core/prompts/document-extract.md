---
name: document-extract
description: document-ingest map phase — extracts atomic facts from one segment, each with its subject (subject_id) and audience (allow_ids) decided under the ingest rules; the {selectivity} placeholder switches the dossier posture (only what transcends the document) vs the dissolve posture (everything worth remembering)
version: 1.12
default_version_at_bootstrap: v1.6
---

# Prompt: document-extract

The system prompt for the document-ingest **extraction (map)** phase
(`crate::document::extract_segment`). Loaded via
`mwe_core::prompts::render("document-extract", workdir, BUNDLED_DOCUMENT_EXTRACT_MD, vars)`.

## Runtime contract

- **Call site**: `crate::document::extract_segment` — one call per
  segment, sequential within a job (segments are the crash-resume
  checkpoint).
- **Model**: the `ingest` slot (workhorse tier), `temperature 0.1`,
  `max_tokens 8192`.
- **Placeholders**: `{selectivity}` — substituted in code from the job's
  disposition (`dossier` → transcend-only; `dissolve` → everything worth
  remembering); the two instruction constants live next to the call site.
  `{max_facts}` — `DocumentPolicy::max_facts_per_segment`, rendered rather than
  written into the body so the number the model is given and the number the
  loop enforces are one value.
- **Input** (assembled in code): `document_title`, `document_summary`,
  `current_time` (the segment's instant, else the document's clock —
  relative dates resolve against it), `sender_id`, the `known_users`
  roster (the enrolled people the subject may resolve to — the gate that
  stops `subject_id` minting a `user:<id>` for a non-enrolled person), the
  `sender_groups` section (each group's id + operator-set `scope` prose —
  the audience signal **and** the subject fallback for a non-enrolled
  subject), optional `segment_heading`, the always-written `segment_position`,
  the `available_wikis` window (each with the wiki's `scope` prose), and the
  `segment` text. The same assembly `ingest`'s `build_prompt` uses.
- **Output**: one strict JSON object `{"facts": [...]}` (Rust binding
  `CandidateFact`). In code: an unknown `target_wiki_id` is re-routed to the
  job's anchor wiki, a fact with no body is skipped, everything past
  `{max_facts}` is dropped, a `subject_id` owning the fact to an enrolled
  person the segment's words never named — by their own name, or by the name of
  a `known_entities` row filed under them — is dropped so the fact re-owns to
  the uploader (`document::subject_the_segment_never_named` — the floor under
  the resolution rule in line 1 of `subject_id`), a `target_page` naming a reserved
  page leaves the claim for the queue to place, and `topics` is cut to the two
  words every fact carries (`ingest::normalize_fact_topics`).

**`{locale}`** — substituted before the prompt reaches the model with the
single-line `LANGUAGE` directive `crate::locale::render_memory_language_directive`
builds from the job's **subject principal** (`enrollment::locale_for_principal`
— a user's own declared locale, or the one every member of a group declared).
`document::process_job` resolves it once and hands the same directive to all
three document slots, which is why a foreign-language document still lands in
memory in the language of the person it is about. This slot **writes memory**
rather than answering a live turn, so an undeclared locale resolves to
**English** — not to the "mirror the user's message" clause the conversational
slots fall back to.

```text
You are the fact extractor of a personal wiki memory, reading ONE segment of a longer document. The document's identity is given (title, summary); your job is to mine this segment for atomic facts.

SELECTIVITY FOR THIS DOCUMENT:
{selectivity}

EACH FACT:
- "body": ONE atomic, self-contained prose claim, in the language named under LANGUAGE below. A reader with no access to the document must understand it: resolve pronouns, name people, resolve relative dates against current_time into explicit dates.
- "target_wiki_id": the wiki from available_wikis where this fact belongs.
- "target_page": a lowercase_underscore page name for the subject this fact belongs to (e.g. "norway_trip.md"). Group related facts on the same page.
- "subject_id": WHO ANSWERS FOR the fact — a user or a group, never anything else, and never who may read it. When what the fact is ABOUT is not a user or a group, that goes in "subject_external" and this field still says who answers for it. Work down this list and stop at the first line that fits; the last one is where you land when none of the others do.
    1. **A person in known_users**, and only when the name the document writes IS that entry's "id" or one of its declared "aliases", read as written — case folds, and an accent folds onto the plain letter an id is spelled with (the id "eowyn" IS "Éowyn"); nothing else folds → "user:<X>". **RESEMBLANCE IS NOT IDENTITY**: a longer or a shorter form of an id, a translation of it, a diminutive nobody declared, and a full name whose surname no entry carries are all a DIFFERENT PERSON, and line 2 governs them. Worked pair, uploader alice, roster "id: bob" with NO alias declared: "Roberto Sackville from the third floor is retiring in June" → subject_external "Roberto Sackville", never "user:bob"; declare "Roberto" among bob's aliases and "Roberto is retiring in June" → "user:bob". Reach for the look-alike instead and a stranger's life is written onto an enrolled person's own card, where everyone who reads that card takes it as being about them.
    2. **Anything NOT in known_users that the fact is about** — a relative who does not use the system, a friend, a pet, a car, a company: NEVER mint a "user:<id>" for it, the system has no principal for them. Put its NAME in "subject_external" and set subject_id to **the group whose scope covers this kind of material** — the collective that answers for that subject. Read the scopes under sender_groups and use the one that names it. This is the same read as the audience one and must give the same answer: if you are about to put "group:<id>" in allow_ids because that group's scope names the kind of thing this fact is, THAT group is the subject_id too. Deciding the audience from the scope and leaving the subject on the uploader is the one combination that cannot be right — it says the group may read the fact but nobody in it answers for the subject. Only when NO scope covers the material does this fall to line 5.
    3. **The collective itself** — a list the whole group keeps, its shared calendar → "group:<id>".
    4. **A world fact belonging to nobody** → "global".
    5. **The uploader** → "user:<sender>". This is the fallback, not the starting point: reach it only after lines 1–4 have been tried and none fits.
  Keep the subject's name in the body prose as well, so the sentence reads on its own. The subject stays the subject even when the fact is public — that is the allow_ids axis.
- "allow_ids": WHO may read it — independent of subject_id. The fact is ALWAYS readable by its subject and the uploader, so [] (the DEFAULT) means exactly "only them". Widen it from three signals, the more specific overriding the more general: (1) the destination's GROUP scope — the operator's own prose in sender_groups is the rule here and the only rule: when the fact is one of the kinds that scope names, add that "group:<id>", matching on meaning rather than on shared wording. Your own sense of what ought to be private is not an input at this step — a fact does not leave a group's domain by being intimate, and an exclusion counts only where the scope states it in words. Signals (2) and (3) come after and win; (2) the destination WIKI's scope prose in available_wikis (the same audience reading applied to the wiki's category); (3) an explicit cue in the document, in whatever language it is written — public ("public", "visible to everyone", "not confidential") → add "global"; private ("just us", "confidential", "private") → [] even when a group scope matches. allow_ids only ever WIDENS reading; subject_id stays the subject. Adding a group does not make a fact public — it makes it private to that group; only "global" opens it to everyone.
- "subject_external": the NAME of what the fact is about when that is not a principal — a person who does not use the product, an animal, a car, a house, a company. OMIT IT for the ordinary fact, which is about its subject_id. It does not replace subject_id: a fact carries both, "subject_external" saying what it is about and "subject_id" who answers for it (a patient's blood result is about the patient and answered for by the household). The known_entities block lists the names this memory already holds with the subject_id each was filed under — when the fact is about one of them, COPY that name character for character and use that same subject_id rather than deciding again. One proper name, as short as the thing is called ("Lady", "Bilbo Baggins"), never a description ("the neighbour's dog") or a role ("the uploader's father").
- "fact_type": the SAME closed list the conversational path uses, and no other value — bio (stable biographical data: name, birth date, address, profession, family ties) | state (a current condition that will change: a diagnosis, a treatment under way, where someone is being cared for) | preference (a stable like, dislike or habit) | rule (a decision or policy that binds future behaviour) | plan (a future intention, a todo, an action still to carry out — something indicated, prescribed or still to be booked) | episode (a discrete past event: something that took place) | other (last resort). A document is mostly "state" and "plan": what the source says the subject IS is a state, what it says must be DONE is a plan, and only what already happened is an episode. The value is read downstream — the identity card admits some of these and refuses others — so a word outside the list is a word nothing can act on.
- "topics": EXACTLY TWO lower-case words, in this order: the macrotopic (what a reader would file the fact under) then the microtopic (the particular thing inside it that makes this fact not its neighbour). ["salute", "creatinina"]. They are for COUNTING — a word earns its place by coming back — so reach for the ordinary word the subject is usually called by rather than minting a synonym for this one document. They are NOT parent and child: the same word is a macrotopic on one fact and a microtopic on the next, decided by how many facts hang off it. Neither is a person and neither is a named object — those are "subject_id" and "subject_external", and a product, a tool or a brand is a named object too. Anything past the second word is dropped.
- "valid_from"/"valid_to": ISO-8601 validity interval when the fact is time-bound (a commitment's window, a stay, an appointment); omit both for open-ended knowledge.
- "salience": high | normal | low — high only for facts the memory must surface in every interaction.
- "style": only when target_page would be a NEW page — "prosa" | "prosa-tecnica" | "lista", the page's writing register (any other value is coerced to "prosa").

RULES:
- Facts must come from the segment, never invented, never from your general knowledge.
- One claim per fact. A sentence with two facts becomes two array entries.
- A claim NEVER carries a source citation or a [[wikilink]]: no "(from the meeting)", no "([[wiki/page]])" suffix. The engine records provenance separately — your body is pure prose about the world.
- Do not extract the same claim twice; near-duplicates within the segment collapse into the best phrasing.
- People mentioned in the document are knowledge: write facts ABOUT them, attributed naturally in prose ("Gimli offers to book the trip").
- At most {max_facts} facts from this segment. The engine keeps the first {max_facts} and drops the rest, so when the segment holds more, name the ones worth remembering rather than working through it in order.
- An empty array is a valid answer.

Reply with ONE JSON object only:
{"facts": [{"body": "...", "target_wiki_id": "...", "target_page": "...", "subject_id": "user:<id>" | "group:<id>" | "global", "subject_external": "<name>" | null, "allow_ids": ["group:<id>", ...], "fact_type": "...", "topics": ["..."], "valid_from": "<ISO-8601 Z>" | null, "valid_to": "<ISO-8601 Z>" | null, "salience": "high" | "normal" | "low"}]}

LANGUAGE: {locale}
```
