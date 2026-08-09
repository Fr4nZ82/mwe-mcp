---
title: Ingest pipeline — wiki_ingest_message
area: design-notes
status: implemented
last_review: "2026-08-06"
---

# Ingest pipeline

[`mwe-core::ingest`](../../crates/mwe-core/src/ingest.rs) hosts the
single conversational entry point a consumer LLM agent talks to every
turn: `wiki_ingest_message`. It is the flagship MCP tool
([mcp-tools.md](../protocol/mcp-tools.md) + [tool-reference.md](../protocol/tool-reference.md)) — the agent
client knows about exactly **two** MCP surfaces (this one for chat,
`dashboard_link` for structural intent) and never touches the
`_internal.*` atomics directly.

## What the orchestrator does

The pipeline is six steps. Four of them are deterministic plumbing
around (at most) two LLM calls:

```text
1. recall context        recall::wiki_recall   (top_k hits, ACL filtered)
2. enumerate wikis       WikiTree::walk        (bounded compact list)
3. LLM intent + plan     llm::complete         (single call, JSON out)
4. route by intent       capture (standard wiki ⇒ buffer, unless requested-container ⇒ live | otherwise ⇒ direct write) | recall snippet | dashboard hint | noop
5. recall-block tail     recall_nav::gather_entry_points + navigate (optional) + recall::recall_due_soon
6. assemble response     IngestResponse        (context_snippet + rules + suggested_seed + capture_id)
```

The LLM is asked for **one JSON object** that encodes both the intent
classification (`capture | recall | structural | skip`) and — for
capture — the facts to file. The plan is **multi-fact and array-only**:
every captured fact lives in an `extractions` array on
[`LlmIngestPlan`](../../crates/mwe-core/src/ingest.rs), one
self-contained capture plan per atomic fact (`target_wiki_id`,
`target_page`, `owner_id`, `allow_ids`, `fact_type`, the validity
interval `valid_from`/`valid_to`, the per-page `style` and
`page_description`, the per-fact `salience`, the `engine_rule` governance
flag, `topics`, `body`, `supersede_target`).
A turn that states several things ("Vivo a Bologna
e lavoro da remoto per AcmeCorp") yields several extractions; a turn
that states one thing yields a **one-element** array; a turn with
nothing memorable yields an empty array (intent `skip`). The turn-level
fields (`intent`, `suggested_seed`, disambiguation) stay on the plan.
The prompt's schema has **no top-level fact fields** — they survive on
the Rust struct only as a tolerant defensive fallback
(`capture_units` synthesises one unit from them *only* when
`extractions` is empty). One call is cheaper than three (intent →
routing → seed) and stays inside the ~500 ms–2 s conversational budget
(see [runtime-topology.md](../architecture/runtime-topology.md)).

> **The intent field carries two decisions, and they are independent**
> *(prompt v2.53)*. `intent` settles both whether the turn WRITES and
> whether it READS, and the two do not follow from each other:
>
> - **`recall` is a test of an unresolved REFERENCE** — not of grammar,
>   and not of how much work the turn asks for. Does the turn point at a
>   person, thing, preference, plan or event *by description* rather than
>   by value, so that only what this memory holds could say which one is
>   meant? A command qualifies: mwe performs no actions — the consumer
>   agent does — so what a command asks of memory is the reference inside
>   it. *"Put on a playlist Galadriel and I both like"* is `recall` with
>   an empty `extractions` array; *"put on Vivaldi's Four Seasons"*,
>   *"turn the volume down in the living room"*, *"what time is it?"* and
>   a tool instruction carrying its own `device_id` are not — **doing work
>   is not remembering**.
> - **`capture` always wins the tie.** The reference test never takes a
>   turn away from `capture`: a turn that states something new *and* asks
>   a question is a `capture`, because a `recall` writes nothing and the
>   stated facts are lost forever, while filing it as `capture` costs
>   nothing on the reading side — the recall block and the navigated pass
>   are served on a `capture` turn exactly as on a `recall` one.
> - **Store-nothing does not imply recall-nothing.** A one-shot command
>   is *"just conversation, never a rule"* and stores nothing (prompt
>   Part 7) — but that settles the write side only. Nearly all such
>   commands are `skip` because their references are resolved; the rare
>   one that carries a description only memory can resolve is `recall`.
> - **`skip` covers the incomplete turn.** A bare fragment that would
>   only mean something as the answer to a question nobody asked (*"the
>   volume"*, *"Paris"*, *"this morning"*) names no subject to search and
>   states nothing to store. Context can only **rescue** a fragment,
>   never reject one: when `recent_messages` shows the preceding turn
>   asked for exactly this, the fragment is complete and is classified on
>   its resolved meaning; with the window empty or unrelated it stays
>   `skip`. Length is not the test.
>
> This matters beyond the write path because **intent gates the
> navigator** (below): a turn misfiled as `skip` loses the multi-hop pass
> *and* has its flat recall slot discarded.

> **Per-fact validity + per-page style/description.**
> Each extraction carries a **validity interval** — `valid_from`/`valid_to` (RFC3339 UTC instants;
> `valid_to = null` = an OPEN horizon, "true now, no known end"). The knowledge/state distinction
> is expressed as dates — an open `valid_to` is durable knowledge, a set `valid_to` a transient state —
> decided per fact from the horizon, not a label (the Berlin-vs-Lisbon test: a durable
> profile gets `valid_to: null`, a transient state a short concrete `valid_to`). Each extraction
> also carries the **target page's** `style` (`prosa` | `prosa-tecnica` | `lista`) and a one-line
> `page_description` of what the page holds. Validity is **persisted on the direct capture
> path** — `valid_from`/`valid_to` thread from the extraction through `CaptureRequest` into
> [`fact_index`](../../crates/mwe-core/src/fact_index.rs) (`decay_reason` stays
> NULL at insert — a fresh fact is alive). Both bounds are **normalized at the capture inlet**
> (`normalize_capture_bound`, wrapping the same `normalize_iso_bound` the validity-edit and
> closure paths use): a present value must parse as an RFC3339 instant, because `fact_index`
> compares these columns lexicographically (due-soon ranges, expiry judgements). A malformed
> bound (an unresolved relative phrase like "domani sera") degrades to open (`NULL`) with a
> warn — never stored verbatim, never replaced with the turn's own instant. The **narrative buffer→promote path carries validity too**:
> `buffer_capture` stages `valid_from`/`valid_to` on the capture, the per-wiki `_captures.md`
> journal mirrors them (`vf`/`vt` attributes — `rm engine.db` + reindex regenerates them), and the light dream's
> `promote_one` copies them into `fact_index`, where they drive the validity render in compiled prose.
> The ingest **placement axis** (`target_page` + `style` + `page_description`) **reaches the fact** and is
> **consumed**: `buffer_capture` stages `style`/`page_description` alongside the
> `target_page` it already carries, the journal mirrors them (`style`/`desc`, the free-text `desc`
> percent-escaped), and `promote_one` copies the whole axis into `fact_index` (0035). Since the
> classifier stopped proposing a destination, `target_page` and `page_description` are only ever
> populated when the write cannot wait — `lista` material, or a container the user
> just asked for (see that section below); anything else carries the buffer page
> and no description. In the **light** cadence `build_wiki_plan` settles each
> fact that carries a `target_page` **deterministically, with no LLM**
> (`ingest_placement_blueprint`), and hands everything else — i.e. all prose —
> to the Cartografo on the cheap tier
> (`NewFactPlacement::NamedThenCartografo`). The **strong** Cartografo, and the
> re-open park it alone answers, stay REM-only
> (see [narrative-compiler.md](narrative-compiler.md#stage-1--the-cartografo-strong-model-classification)).
> The page frontmatter's `style` **prefers the ingest-proposed `style`** (carried on the plan as
> `PagePlan.style`), falling back to the **Cronista's** compile-time choice when ingest proposed none
> (`normalize_style(page.style.or(body.style))`); the `description` is the Cronista's. Validity,
> `style` and `fact_type` are **independent axes** — not copied from one another. Validity lives **per fact in
> `fact_index`** (a shopping list is ten items with ten horizons), never on the page frontmatter or the lean
> ACL marker; see [memory-model.md](../concepts/memory-model.md) and
> [redaction-policy.md](redaction-policy.md) (per-fact metadata is DB-authoritative).

> **Per-fact salience.** Each extraction also carries a `salience`
> (`high` | `normal` | `low`; absent = unspecified) — how always-relevant the fact is to its owner. `high` is
> the scarce always-on set that belongs in the owner's **base context**: the **identity core** (name, aliases,
> role(s), relations, birthdate, address, language, timezone, contacts — the whole identity card, not just the
> name), **health/safety**, and **hard standing constraints**. The classifier decides it (no hardcoded gate, an
> independent axis from `fact_type`/validity/`style`). A `high`-salience identity core is how the
> guaranteed identity core is achieved — by routing, not by a rigid formatter or a typed field. (Birthdates are
> stored and shown as the **date** itself, not converted to an age.) Salience is **stored end-to-end** —
> threaded from the extraction through `CaptureRequest`/`BufferedCapture`
> into [`fact_index.salience`](../../crates/mwe-core/src/fact_index.rs) (0037) on both the direct and the
> standard-wiki paths (the `_captures.md` journal mirrors it as the `sal=` attribute, like `vf`/`vt`). **The light
> compile cadence reads it**: [`ingest_placement_blueprint`](../../crates/mwe-core/src/planner.rs)
> routes a `high`-salience fact to the actor-wiki's `profile.md` identity card, overriding its proposed `target_page`
> (see [narrative-compiler.md](narrative-compiler.md)).

### The guest short-circuit — ephemeral turns for the unidentified human

The builtin `guest` pseudo-identity (an unrecognized voice on a
satellite, an unknown sender in a group chat — see
[identity-and-acl.md §1](../concepts/identity-and-acl.md)) exits the
pipeline right after step 1. Recall **does** run — with the guest
`SenderContext`, so the ACL confines every slot (flat, fresh) to the
public slice — but the wiki enumeration and the classifier never
happen: the turn files **nothing** (no capture, no closures, no
behaviour rules, no buffer row) and no LLM is billed. The response is
`intent: skip`, `capture_id: null`, `llm_used: false`, the
public-slice hit list as `context_snippet`, **no** `suggested_seed`
(the fallback's "I've noted that." would be a lie on a turn that
stores nothing), and the [`rules`](#the-rules-field--behaviour-directives-kept-apart-from-memory)
channel carrying the fixed `GUEST_RULES_NOTICE` directive — the
consumer is told to behave reservedly and never promise memory. An
identity boundary like redaction, not a semantic gate: what the
consumer *says* on the turn stays the consumer's judgment. Pinned by
`guest_turn_is_ephemeral_and_recalls_public_slice_only`
([`ingest.rs`](../../crates/mwe-core/src/ingest.rs)).

### Destination — derived, not chosen

The classifier is shown **no wikis**. It was, until the block was measured
against what it bought: one decision out of the twenty-odd the call makes,
paid at full per-turn price (the window rides the *user* message, not the
cached system prefix), by the one participant that could not see the pages
it was routing into. It also carried a one-line abstract of every
principal's memory into every turn's prompt.

[`derive_target_wiki`](../../crates/mwe-core/src/ingest.rs) resolves the
destination instead, in four descending preferences:

1. **An explicit `target_wiki_id`**, when something still emits one. The
   bundled prompt does not; an operator override may, and honouring it
   keeps those deployments working. Gated on the enumeration, so an
   invented or smart-managed id is ignored rather than fatal — the fact
   falls through to the next arm instead of being dropped.
2. **The list page the turn names.** A `target_page` matching an entry of
   the `list_pages` inventory takes that page's own wiki. This is the one
   route by which a turn still files into a wiki that is not its owner's,
   and it fires exactly when the user pointed at the list themselves.
3. **The subject's own wiki.** An identity wiki's id *is* its principal's
   id, so `user:marco` → `marco` and `group:famiglia` → `famiglia`. The
   ordinary path.
4. **The sender's own wiki** — for a `global`-owned fact (no wiki carries
   that principal) and for a subject with no wiki of their own.

Nothing left → `MissingTargetWiki`, and the extraction is dropped. That is
now a deployment fault (the sender has no writable wiki) rather than a
model fault. Pinned by
`derive_target_wiki_walks_list_then_subject_then_sender`.

[`available_wikis`](../../crates/mwe-core/src/ingest.rs) survives as the
**internal** enumeration behind those arms — and as the window the
[document extractor](document-ingest.md) still renders, since a document's
segments may legitimately belong anywhere in the tree. On the
conversational path it is now called **uncapped**: the cap existed solely
to bound a prompt block that no longer exists. It still drops smart wikis
first — they are authoritatively managed by smart consumers via the
`wiki_admin_*` family, and routing a capture into one here would both
double-bill the smart consumer's LLM budget (server-side `ingest` runs
*after* the consumer already paid to classify the message) and bypass the
audit row in `wiki_admin_op_log`. The flag is read straight from each
wiki's `_meta.md`; see [smart-wikis.md](smart-wikis.md). Every arm of the
derivation inherits that exclusion for free.

### The list-page inventory — the one placement surface left

`list_pages` ([`fact_index::list_pages_readable_by`](../../crates/mwe-core/src/fact_index.rs))
is what replaced the wiki window, and it is not the same block in a smaller
hat. A wiki is an *address* the engine can compute; the file name of the
shopping list is knowledge only the store holds, and a recalled fact
carries its wiki, its owner and its audience but **never the page it sits
on**. Without the inventory, "add detergent to the shopping list" invents a
name, and — because a requested container is written live — mints a second
shopping list in front of the user.

It serves every `lista`-style page the sender may read, from **both**
halves of the write path: `fact_index` for the lists already on disk, and
`capture_buffer` for one created minutes ago that the light dream has not
promoted yet. Serving only the first is exactly the gap that splits a list
when two items are added inside one light-dream interval. Entries are
deduplicated per page, ACL-filtered by the query (the three-axis
`readable_by` predicate, shared with recall), capped at
`max_list_pages_in_prompt`, and rendered as a page name plus the `holds`
line recorded when the page was proposed — **never** the wiki it lives in,
which stays the engine's business.


**What each entry of the document extractor's window says about itself.**
`wiki_id`, `title`, `wiki_type`, `is_agent` when set — then the
description, as up to two lines, each omitted when empty:

| line | what it is | who writes it |
|---|---|---|
| `scope:` | the **authored intent** — "what goes in here" | a person, by hand (`_meta.md`); on a **smart** wiki the same field is the [door sign](smart-wikis.md), a family this window never carries |
| `holds:` | the **compiled abstract** — "what it actually holds" | the compiler, from the card the wiki's own foundation page was written with (`_meta.extra["summary"]`), seeded at emergence from the promoting model's description |

They are kept as **two keys, not merged**: they are different claims with
different authors, and folding them together would have the nightly compile
overwrite an operator's authored line. A wiki with neither renders
`about: (not described yet)` — the honest answer, and shorter than two
empty keys.

`holds` is the one that is actually populated today: nothing writes `scope`
on a standard wiki, which is why removing the window from the
conversational prompt cost that path no audience signal it was actually
receiving.

**The conversation is a superset, not an exclusion** (roadmap group 17).
The filter keeps a *capture* out of the smart project wiki — it does
**not** keep a smart consumer's conversation out of personal memory. A
smart consumer (e.g. Claude Code on a codebase) authors its project
wiki via `wiki_admin_*` **and** routes the user↔agent conversation
through `wiki_ingest_message`, which lands in the user's standard
personal wiki exactly as a standard consumer's turn does. There is **no
`consumer_class` gate** on this path — `IngestRequest` carries no such
field; the project wiki and personal memory are joined by **links, not
duplicated detail** (see [the provenance link](narrative-compiler.md)
and the `smart-consumer` skill's message router). The double-bill
concern above is about **bulk project-doc maintenance**, not the
low-volume per-turn conversational memory — the consumer's own router
keeps that small by dropping ephemeral ops before any server call.

### Narrative-vs-direct split

The wikis that survive the smart-family filter are routed two ways by the
[`Capture`](#the-four-intents-and-what-each-one-does) arm: a **standard-wiki**
target (a prose page the compiler writes) is **buffered**; a live
`requested_container` is a **direct write**. **"standard" = "not
smart"** — every wiki that survives the smart-family filter is on the
non-smart path, read from the per-wiki `_meta.md` smart flag rather
than a registry set.
The full model and the buffer it feeds live in
[narrative-buffer.md](narrative-buffer.md). **This is the standard /
standard-wiki perimeter only — the smart path is untouched.**

**The two write speeds — narrative sediment vs. live containers.** The split
is not a fast path bolted onto an async model as an exception — it mirrors
the two natures a captured claim can have, and the classifier judges which
one it is on every extraction. *Accumulated knowledge* (a detail in a story —
"we ran out of milk while shopping") gains value by consolidating: it can
ripen in the buffer and come out as compiled prose, and nothing about the
turn needs it back sooner — recall serves buffered captures through the fresh
slot meanwhile. Batching is also the write economics: the compiler renders
many ripened facts in one pass instead of re-prosing a paragraph on every
message. An *operational container* (a list, a collection, a named note —
"add milk to the shopping list") carries a **read-your-writes contract**:
the user's next question is seconds away ("what's on the list?"), so waiting
would be wrong *by the content's nature*, not slow by implementation. So the
classifier emits a per-extraction `requested_container` flag, and step 4
routes a flagged capture down the **direct-write path even into a standard
wiki** (`route_to_buffer = target_is_standard && !requested_container`) —
`wiki_capture` creates the page if absent and writes the fact's marker
immediately; the dream later refines it. The classifier decides — there is
no hard-coded gate. Only accumulated knowledge (`requested_container: false`,
the default) waits in the buffer.

> **Collapsing the two was proposed on 2026-08-05 and rejected.** The
> argument for it was that the fresh slot already makes a buffered claim
> recallable, so the page could lag — and that removing the fork would settle
> dedup in one place. Both halves failed on inspection, and the reasoning is
> kept here so it is not re-proposed:
>
> - **Recallable is not the same as complete.** The fresh slot is a *ranked
>   top-K* (`recall_fresh_top_k`, default 3). That is the right shape for
>   "the most relevant things memory holds" and the wrong shape for a list,
>   whose value is that it is a **set**: add six items inside one dream
>   interval and three of them are not offered at all. An incomplete list is
>   a wrong answer, not a partial one. Pinned by
>   `the_fresh_slot_cannot_serve_a_whole_list`.
> - **Dedup was never decided twice.** The scan is one function
>   ([`capture::best_dedup_candidate`](capture-and-dedup.md)) called from the
>   write path and from promotion. What differs is *when* it runs, not what
>   it decides.
>
> What the live path costs, now that the embedding is computed at staging
> time either way: one dedup scan over the wiki's active facts, one atomic
> page write, one row insert — moved into the turn instead of into the dream.
> No model call, no extra embedding.

### Prose-only classification

The classifier is **prose-only** — no `wiki_type`/`fields`/`purpose` on an
extraction, no `structured_types` roster in the prompt. Every fact files as
prose onto a **subject-page** carrying a per-fact **validity interval**
(`valid_from`/`valid_to` in `fact_index`) and a per-page **style + description**
(in the page frontmatter); see the design SSOT
[memory-model.md](../concepts/memory-model.md).

The reference-time injection: a dated commitment (an appointment, a
deadline) needs a concrete date, so `build_prompt` injects a `current_time:`
line — the turn's reference instant (UTC ISO-8601 + weekday), passed in as a
`now` parameter (deterministic, unit-testable). The prompt resolves every
relative date against it ("giovedì alle 17" → a full ISO datetime) so the fact
files with a concrete date in its `body`. The instant is the turn's **semantic
clock**, resolved once per turn: `metadata.occurred_at` when the consumer set
it (a backlog replay or import re-lives the turn at utterance time — relative
dates, validity windows and the due-soon window all read the same clock),
otherwise the server's now. A `user_timezone:` line rides along when a zone
resolves, most specific wins: the **sender's own** zone
(`enrollment_users.timezone` — users page / welcome wizard; two users of one
deployment can live in different places) over the deployment-wide
`recall.ingest_timezone`; absent both, spoken wall-clock times are read as UTC
(the historical behaviour). A per-turn zone from the consumer (device time,
covers travel) is a tracked protocol extension. Operational timestamps
(`created_at`) stay wall-clock: the audit trail records when the engine saw
the message, not when it was uttered.

> **Forward direction (decided 2026-06-03, not yet built).** The standalone
> proposal **GUI page** (`/dashboard/proposals` with its questionnaire/apply
> forms) is slated for **removal**: every memory operation — answering the
> dream's proposals, modifying or undoing an applied structure proposal —
> migrates to the **dashboard chat** (the agentic internal LLM driving the
> `_internal.*` tools), with a help button. Proposals stay as **records**
> (audit + undo); only the form-GUI surface goes away. This is the
> "form-to-chat bridge" taken to its conclusion: one
> conversational surface. Plan: [agentic-chat.md](agentic-chat.md).

### The anchor rule — re-word freely, never sharpen (prompt v2.55)

The classifier **may re-word; it may not add specificity the turn did not
carry.** The test is not *was the detail written* but **is there an anchor in
the turn it follows from** — "tomorrow", "Saturday", "the 5th" are anchors, and
resolving them against `current_time` is the wanted behaviour above. What the
model knows from its own training is not an anchor.

Two confirmed production cases shaped the rule (card 57, measured offline over
444 production and 125 demo facts — **1 confirmed in 444**, so the phenomenon is
narrow, and most detector flags were correct date resolution):

- **Knowledge imported as a personal fact.** A car's road-tax deadline became
  *«can be paid without penalty until 30 September, at a cost of about €185»*.
  Neither the second date nor the amount nor the grace period was in the turn:
  an Italian tax rule the model knows, filed as a fact about this household. It
  is the most dangerous class because it is **unfalsifiable from inside** — no
  author to contradict it, and the night's confirmers judge facts against other
  *facts*, never against the turn that produced them.
- **An invented frame.** A turn complaining that an assistant had signed the
  sender up for a fair produced a defensible `body` — and a **new page**
  described as *«Progetti e attività relativi a …»*. "Project" occurs zero times
  in the message: an event was promoted into a body of work, inside a group
  wiki, and the compiler then grew prose from the description. The invention was
  in the **container**, and every guard we had judged facts.

So the rule binds the frame as hard as the body: `target_page` and
`page_description` are covered by the same clause, and **opening a NEW list is
called out as the moment to be conservative** — the subject must be one the turn
named, the description must be answerable from the turn, and when in doubt an
existing list wins, because a misfiled item is moved later while an invented list
outlives the turn that caused it.

The clause now has far less surface to guard, because the classifier can only
name a page for a list at all — see below.

### A page name is honoured exactly when the write cannot wait

`target_page` and `page_description` survive validation in **two** cases;
everywhere else the classifier's proposal is discarded and the fact takes the
wiki's buffer page (`IngestPolicy::default_page`, `notes.md`).

- **`style: "lista"`.** A list is a *set*. Half a shopping list is not a partial
  answer, it is a wrong one — which is also why a ranked top-K cannot serve one.
  It has to land on the page it belongs to, **by its exact name**, and
  `list_pages` exists so that name can be copied rather than guessed.
- **`requested_container: true`.** That extraction bypasses the buffer and
  reaches disk inside the turn, so the page must exist inside the turn.
  Independent of `style` by design — "keep me a note about project X" is prose
  and still needs the name the user gave it. Binding the page to `style` alone
  was a regression this rule's first cut introduced (2026-08-06, caught the same
  day): a requested note landed on `notes.md` instead of the page asked for.

Everything else waits, and waiting is what makes the name unnecessary: no single
page is the answer before a fact is seen beside its neighbours, the classifier is
shown no prose pages to check against, and any name it emits is a guess the
consolidation would have to undo. Undoing it is not free — a guessed name becomes
a real file on disk, and pages outlive the turn that minted them.

Enforced in `validate_capture_plan`, not merely asked for in the prompt: a name
that reaches disk is a page. Pinned by
`a_page_name_is_honoured_only_when_the_write_cannot_wait`, which exercises both
switches.


**Who chooses the page instead: the light dream, within the hour.** The
decision did not disappear with the classifier's name for it, it moved to the
one participant that can see the pages. The hourly light compile places every
unnamed fact with the Cartografo on the cheap tier
(`NewFactPlacement::NamedThenCartografo` — see
[narrative-compiler.md](narrative-compiler.md#stage-1--the-cartografo-strong-model-classification)),
and what the user *did* name — a `lista`, a container asked for by name — is
settled before the model is called and never shown to it.

So the wiki's buffer page (`notes.md`) is what a fact passes through between
being captured and the next light cycle, not where it waits for the night. It
is still the designed holding place for anything the Cartografo declines to
place, and REM's reorg still drains it.

Nothing about the *reading* depends on this: the recall block renders the
fact's own text, and a fact captured this minute is served from the capture
buffer's own slot. What placement buys is the **shape** of the memory a person
opens — and the links, since a page is now reachable only through a fact hit,
its card, or an authored `[[wikilink]]`.

The sibling clause: **an unresolved reference stays unresolved.** A gap the turn
did not fill keeps the turn's own wording; the fact is neither dropped nor
completed with a plausible specific. A missing detail is recoverable by asking,
an invented one is not — afterwards it reads exactly like something that was
said.

Two things carry the rule downstream, because a prompt rule alone would not have
held: the writer is told that a page's index line is a **filing label, not
evidence** (cronista v1.23), and the plan stops treating the classifier's
description as permanent — see
[the card heal](narrative-compiler.md#the-card-heal--a-page-is-described-by-what-was-written-on-it).
A page the machine invents also leaves a
[receipt](narrative-compiler.md) (`page_create`, born-applied and revertable).

**Not built, deliberately**: a stated/inferred axis on every fact. It is
invasive, and it would have caught **neither** confirmed case — the road-tax
fact would have been marked inferred and kept, and the frame invention is not in
a fact at all.

## The contract the orchestrator honours

Two invariants drive almost every design choice:

1. **Always return something the agent can render.** The consumer
   never sees an "ingest exploded" error — its job is to keep the
   conversation alive. Every soft failure (LLM transport down,
   malformed JSON, invalid capture plan) demotes to `IntentKind::Skip`
   with a canned `suggested_seed` and the recall hits as
   `context_snippet`. Only **infrastructure failures** (DB, embedder,
   filesystem) surface as [`IngestError`] for the transport layer to
   map onto MCP error codes.
2. **The agent stays agnostic of structure.** The orchestrator emits
   no filesystem paths and no wiki internals — only `capture_id`
   (audit-only) plus the two free-text fields the agent actually uses
   (`context_snippet`, `suggested_seed`). Per
   [tool-reference.md](../protocol/tool-reference.md) +
   [mcp-tools.md](../protocol/mcp-tools.md), even `wiki_id`
   is opaque from the agent's perspective.

The `llm_used` boolean is the audit-side companion: `true` when the
LLM responded (even with garbage), `false` when the call failed at the
transport layer. It is the only way to distinguish a real
classification of `skip` from a fallback `skip`.

## The replay differential — measuring a prompt change

`ingest.md` is ~26k tokens and renders on every classified turn, so every
proposal to shrink or split it is really a bet that the decisions stay the
same. That bet is now measurable rather than argued.
[`ingest_replay`](../../crates/mwe-core/examples/ingest_replay.rs) takes real
requests out of the [training spool](llm-functions.md#6-the-training-spool--teacher-traces-for-local-slot-distillation),
re-runs them against a modified system prompt, and diffs the two plans field
by field.

```text
cargo run -p mwe-core --example ingest_replay --release -- --spool <dir> --out base-a.jsonl
cargo run -p mwe-core --example ingest_replay --release -- --spool <dir> --out base-b.jsonl
cargo run -p mwe-core --example ingest_replay --release -- --spool <dir> --variant gated --out gated.jsonl
cargo run -p mwe-core --example ingest_replay --   --compare base-a.jsonl base-b.jsonl
```

Three properties make it trustworthy, and all three were learned the hard way:

- **The reference is a fresh baseline, not the recorded answer.** Spooled
  completions came from older prompt versions; only a re-run of today's
  bundled prompt over the same requests is a fair control.
- **Two baselines, always.** The second one is the noise floor — see below,
  it is large. A variant means nothing except relative to it.
- **An absent field is its schema default.** A model that omits
  `engine_rule` has not decided differently from one that writes
  `false`; comparing raw JSON inflates every diff by tens of points of
  pure noise.

It reports two rates: agreement on *every* compared field, and agreement on
the consequential subset — intent, how many facts, and each fact's wiki,
owner and audience. Free prose (`body`, `page_description`, `topics`) is
never compared. Runs resume, so a rate-limited pass is re-runnable for the
cost of what it missed, and `--dry-run` prints what a pass would send
without sending it. It is a manual example, never part of `cargo test`: it
spends real tokens.

### What it measured first: the classifier is not reproducible

Replaying 340 production turns twice through the **identical** prompt:

| | agreement, run vs identical run |
|---|---|
| every compared field | **51.2%** |
| intent + fact count + wiki + owner + audience | **67.9%** |
| …restricted to turns that captured something | **45.7%** |

So on a turn that files memory, the same message with the same recalled
context lands the same way barely half the time — the wiki differs on 16%
of turns, the owner on 13%, the audience on 16%.

This is not a prompt-size effect. The `ingest` slot runs on Gemini 3, which
**mandates `temperature: 1.0`** — the backend clamps the caller's requested
`0.1` up to it, because sub-1 values make the model loop
([llm.rs `GEMINI_TEMPERATURE`](../../crates/mwe-core/src/llm.rs)). The
classifier is therefore sampling at full temperature over a genuinely
ambiguous task, and the spread is the honest consequence.

**Read those figures as an upper bound.** They come from comparing raw plan
fields, and a large part of what that counts is *notation*, not decision —
see the next section. The comparison now measures the reader set instead;
the numbers above predate the fix and have not been re-run.

One thing does follow regardless: **no prompt A/B on this slot can resolve an
effect smaller than that floor**, which is why the roadmap-49 variants below
came back "indistinguishable" rather than "good" or "bad".

### What it costs on disk: a spelling problem, not a governance one

Measured on a read-only production snapshot, 1 001 active facts (2026-07-28).
Of 28 near-identical fact pairs living side by side, **not one diverges on
who may read it** — every divergence is `owner_id` and/or `wiki_id`.

The reason is that read access is `owner ∪ allow ∪ sender`
([identity-and-acl.md](../concepts/identity-and-acl.md#the-single-rule)) —
**all three, and none of them sufficient alone** — so one audience has
several equally valid spellings. For a fact about one member that the whole
group may read, production holds both, in the same notebook:

```
owner=user:galadriel  allow=["group:famiglia"]    74 rows
owner=group:famiglia  allow=[]                    38 rows
```

**54% of active facts sit in a (notebook, audience) group written more than
one way.** The residue of genuine defects is small: one escaped duplicate,
~6 agent-activity notes with no settled owner, 50 rows that name the group
twice. No leak, no wrong audience.

Two hypotheses this killed, both worth not re-forming:

- **Dedup/supersede does not absorb the spread.** Of 238 supersedes only
  **7** replaced a near-identical text; the rest is ordinary information
  updating done by the classifier during conversation (spread across the day,
  not the nightly window; 199 of 217 successors absorb exactly one
  predecessor). There is no cleanup layer catching classifier noise.
- **Deterministic code cannot take the audience decision off the model.**
  The obvious rule — *the notebook implies the audience* — holds for only
  **56%** of facts, because a personal notebook legitimately mixes private,
  family-shared and agent-operational material.

**Production is deliberately left as it is.** The notation has no effect on
recall — `can_read` unions all three fields, so every spelling retrieves
identically — and only owner-axis queries ("everything about X") see the
difference. The house rule for *new* writes: when a fact is shared with a
group, the group goes in `allow`; it may additionally be the `owner` when the
group really is the subject. Rewriting history is not on the table: a
group-owned row cannot be re-attributed without re-reading the sentence,
since `sender_id` records who *said* a fact, not who it is *about*.

## Why one LLM call (and not many)

A multi-stage pipeline — classify intent
(small model) → if capture, ask "where" (separate call) → ask "compose
a seed" (third call) — is the obvious alternative. Single-call beats it
for three reasons:

- **Latency.** Each round trip is ~400-800 ms on Ollama with Qwen
  3.5 9B. Three calls land outside the conversational budget on
  consumer hardware; one stays inside it.
- **Coherence.** The same prompt context (recall hits + available
  wikis + recent messages + sender groups + current message) drives
  both the intent decision and the plan. Splitting the calls makes the
  second call re-derive context, which the model frequently disagrees
  with.
- **Failure semantics.** One call has one failure mode. Three calls
  have eight. The fallback policy "demote to skip + canned seed" is
  trivial with one boundary.

## The four intents and what each one does

| Intent | Side effects | Output | Notes |
|---|---|---|---|
| `Capture` | **per filed fact**: standard target ⇒ [`capture_buffer::buffer_capture_staged`](narrative-buffer.md) (journal append + `capture_buffer` index carrying the staged vector and origin fingerprint, **no** `.md` write, **no** `fact_index` row) — **unless** the classifier flagged `requested_container`, which routes live; a requested container, or a smart target, ⇒ `wiki_capture` / `wiki_supersede` (embed + dedup + atomic append + `fact_index` insert). **Per closed fact** (the plan's `closures` array): validity stamped act-first + one born-applied receipt — see [the closure verb](#the-closure-verb--completion--the-relayed-forget-gesture). | `capture_id` (the **first** filed fact) + optional `context_snippet` if recall hits exist | Multi-fact: a bad extraction is skipped, the rest are filed. Legacy single-fact: any plan-validation failure ⇒ demote whole turn to skip. Nothing valid filed **and** nothing closed ⇒ skip. |
| `Recall` | none | `context_snippet` is the deterministic hit-list rebuilt from the flat hits ([`format_snippet`]) | Recall counter already bumped during step 1. |
| `Structural` | **per filed fact, when the hybrid message carries content** (the same filing loop as `Capture`: explicit `extractions` + `closures` only, no legacy synthesis) | `suggested_seed` (LLM or canned `structural_suggested_seed`) | Never demotes to skip — the dashboard nudge is the turn's outcome even when nothing files. |
| `Skip` | none | LLM `suggested_seed` if any, else `fallback_suggested_seed` | Greetings, acks, no-ops. |

The `Capture` arm **loops over the facts to file**. The plan's
[`capture_units()`](../../crates/mwe-core/src/ingest.rs) yields one
borrowed `CaptureUnit` per `extractions` element; when `extractions` is
empty it synthesises **one** unit from the legacy top-level fields. The
model always emits the array (even a single atomic fact
is a one-element array), so this synthesis is a **tolerant defensive
fallback** — it keeps a stray old-style response, or an operator's older
override, parseable rather than driving normal behaviour.
For each unit the arm validates the plan + supersede target, then forks on the
target wiki's class (resolved from the `available` window — smart wikis are
already gone from it, filtered by their `_meta.md` smart flag). When the
target is standard (smart flag `false`) and the unit is not a live
`requested_container`, the orchestrator calls
[`capture_buffer::buffer_capture_staged`](narrative-buffer.md): the claim is
appended to the wiki's `_captures.md` journal and indexed in
`capture_buffer`, with the validated supersede target carried through as
`supersede_hint` (no `.md` is touched, no `fact_index` row is written,
and the supersede is *not* applied now — it is recorded for the light
dream to honour at promotion time). Two things ride along with the claim: its
**embedding**, computed here over the marker-stripped body so neither the
fresh recall slot nor promotion has to compute it again, and the
**fingerprint of the turn** it came from, which is how the fresh slot knows
not to restate something the agent is already reading. Otherwise the arm keeps
the synchronous direct-write path: `wiki_supersede` when the LLM proposed a
valid supersede target, else `wiki_capture`. Every fact files as prose; there
is no structured route-or-create step.

The failure contract is split by plan shape. A **legacy single-fact**
plan keeps the old "one bad plan demotes the whole turn to skip"
behaviour — an invalid plan, an unresolvable supersede target, or a
supersede target that vanished after recall returns the fallback skip. A
**multi-fact** plan is more forgiving: a bad extraction is logged and
**skipped**, and the remaining valid facts are still filed. Either way,
if *nothing* valid ends up filed (empty plan, or every extraction
invalid) the turn demotes to skip with the canned seed. The turn's
`capture_id` anchors on the **first** fact that was filed — the rest are
durable but the consumer only ever sees one audit id per turn. A
per-extraction `body` is **required** (`CapturePlanError::MissingBody`):
the raw-message fallback applies only to the legacy single unit, because
filing the whole message under every extraction would duplicate it.
A standard-wiki capture is durably **buffered**; the light dream promotes it
to a recallable fact and the nightly compiler renders the prose — see
[narrative-buffer.md](narrative-buffer.md) and
[rem-cycle.md](rem-cycle.md).

**Length never gates fact-extraction** (roadmap group 17). The classifier
sees the full `current_message` — `build_prompt` appends it untruncated — and
a long body is no reason to skip a capture: a durable fact can hide inside it
(a dentist appointment buried in a pasted email). Length decides only whether
to store the body *verbatim*: a paste the user explicitly asks to keep whole
is a [document-import](document-ingest.md) (its own page + pointer, on
`context_hint=import`), never atomised into `extractions`. A
**conversation-borne dated commitment** is no different from any other dated
fact: the classifier resolves its date against `current_time` and stamps
`valid_to` (the reference-time injection under
[Prose-only classification](#prose-only-classification)), so it surfaces in
the recall [due-soon slot](recall-pipeline.md) — the same group-7 mechanism,
reached from a conversational turn.

The `Structural` arm is the integration point with `dashboard_link`
(family G): the agent reads the canned seed and renders a UI nudge
("open the dashboard to continue"). The structural arm emits no
`structure_proposal` at ingest time; the **closure path below is the one
ingest-side emitter** (a born-applied receipt, never a pending one). (The
explicit "I want a notebook for X" structural request is the flow
migrating to the dashboard chat per the forward note above.)

**The hybrid structural turn files its content.** A message can be a
container request AND carry real content ("voglio creare un ricettario:
aggiungi gli spaghetti all'amatriciana — guanciale, pecorino…"): the
structural arm shares the capture arm's filing loop, so the plan's
`extractions` and `closures` ride the message normally while the nudge
stays the turn's answer (prompt Part 1, the hybrid case — the dogfood
re-run lost the recipe this way). Two guards keep the old contract: a
structural turn only files the explicit multi-fact array (the legacy
top-level synthesis would capture the container request itself), and it
never demotes to the skip fallback — the nudge IS its outcome even when
nothing files.

## The reconciliation stage — OPEN WORK, and why it is not the classifier's

Four operations decide the fate of a fact that **already exists**: closing it
(completion / forget gesture), **superseding** it, correcting its **dates**,
changing **who may read it**. Until prompt v2.59 all four were the classifier's,
decided against the ten-odd facts the flat recall happened to surface.

**They were removed from the classifier and are not yet re-implemented
anywhere.** Between v2.59 and the stage described below, nothing closes,
nothing supersedes, no date is corrected and no sharing is changed from chat —
a deliberate interim, taken while production was down and this area was being
rebuilt (founder, 2026-08-06). The engine's `apply_plan_closures` /
`apply_plan_validity_edits` / `apply_plan_acl_changes` and the matching
`LlmIngestPlan` fields are **kept on purpose**: they are this stage's substrate,
and an operator-overridden prompt that still emits them keeps working.

### The rule that decides where a judgement belongs

> **A slot may reconcile against a set it is shown COMPLETE. It may never
> reconcile against a sample.**

The classifier is shown three complete sets, and reconciling against those
stays its job:

| set | what reconciling means there |
|---|---|
| `list_pages` | reuse an existing list's exact name instead of minting a second. It sees WHICH lists exist, never what is ON them — so it cannot act on an individual item. |
| `agent_behaviour_rules` | revise a standing directive with `supersede_target`; the block is every directive in force. |
| `sender_rules` | honour the policy, and do not append a governance rule already in force. |

It is shown **one sample** — `recalled_memory`, top-K by similarity over a store
that may hold thousands. Every judgement that needs the store and gets a sample
fails the same way: silently, by omission, and the failure compounds. Superseding
is the worst of the four, because the new fact is filed while the contradicted
one stays alive beside it; a missed closure merely leaves something open.

### Where the stage goes, and what it sees

After the navigator, before the response is assembled — the last point in the
turn where the engine has actually *read* the memory. Its candidate set is the
**union**, deduplicated by `fact_id`:

1. the flat recall hits (the classifier's own window),
2. the fresh buffered captures — a fact captured this morning is on no page, so
   the navigator cannot reach it by construction,
3. **the active facts on the pages the navigator opened.** `NavigatedFragment`
   carries `wiki_id` + `page`, and `fact_index` is indexed on `source_path`
   (`idx_fact_path`), so this is one cheap indexed read, not a second search.

Why the union and not (3) alone: the navigator opens pages to **answer** the
turn, guided by the classifier's own seeds. Usually the fact to close is on one
of them; not always. And on a deployment with no navigator slot the stage still
runs — with (1) + (2), i.e. exactly today's coverage, done by the right
component.

**Leg (3) is built:
[`recall::facts_on_pages`](../../crates/mwe-core/src/recall.rs).** For each page
the turn opened it returns **every** readable fact on it — not a ranked sample
of it — which is the whole point: within an opened page, nothing is hidden by
ranking, so the stage is shown that page complete. ACL-filtered by the same
`row_visible_to` as every other slot (a fact the reader may not see can neither
be shown nor closed), score always `1.0` (membership of a page is structural),
and one indexed read per page against `idx_fact_path`. Its `cap` is a resource
bound on the whole set and it **logs when it bites** — a candidate silently
dropped here is a fact that quietly cannot be closed. Pinned by
`facts_on_pages_returns_the_whole_page_and_only_what_the_reader_may_see`.

**Still to build**, in order: the stage that assembles the three legs and makes
the call; generalising `prompts/ingest-closures.md` from one verb to four; the
supersede write. Note for whoever picks it up: the orchestrator currently
retains only the turn's **first** filed fact (`capture_id`, the turn's anchor),
so supersede needs the capture loop to collect all of this turn's ids — a
superseding fact has to be nameable before it can inherit anything. And
`nothing_filed` (`ingest.rs`) is computed *before* this stage would run, so it
has to be recomputed after it: a turn that files nothing but closes something
is not a turn that did nothing.

### What it decides, and what it must not lose

One cheap-tier call, one prompt, four answers: what this turn closes, what it
supersedes, whose dates it corrects, whose sharing it changes. The prompt shape
already exists — `prompts/ingest-closures.md` takes `{message}`,
`{current_time}` and a `{candidates}` list rendered as `id · validity · text`;
the stage generalises it from closures to all four.

**The one thing that must be carried over.** Today a superseding fact
**inherits the superseded fact's `allow`** before it is written, because "the
classifier can restate the content but must not be relied on to restate the
ACL" — without it a re-statement silently re-privatises a shared fact. Deciding
the supersede *after* the write means the new fact already carries whatever
audience the classifier chose, so the inheritance becomes a second write. It is
not optional: it is the difference between a memory that governs sharing and
one that loses it on every restatement.

**The precision rule gets stricter, not looser.** A wider candidate set is more
chances to hit the right fact *and* more chances to retire the wrong one. The
existing clause — close only what the message plainly states, never what you
suspect — has to be carried into the new prompt with the same force.

**One accepted limit:** the reply of the turn that triggers the reconciliation
was composed from material collected earlier, so it may still show a closed item
as open. It is right from the next turn; re-composing is not worth the spend.

### Its prerequisite, already landed: a capture turn always reaches the reading

A `capture` that filed nothing used to return early, demoted to a skip. That was
sound while "filed nothing" meant "nothing happened" — a forget gesture carried
its own closures, and a closure counted as activity. Since reconciliation left
the classifier, a gesture files **nothing**: "forget what I told you about the
greenhouse" is a capture with an empty `extractions` array. The early return
would therefore have (a) answered the user out of an empty context and (b)
skipped the navigator, which is exactly where this stage reads from — breaking
the one case it exists for.

So the demotion is gone: a capture turn always flows on to the reading. Nothing
is lost — unclaimed media is still filed by the deterministic pass, and a turn
whose reading *also* comes back empty still gets the canned seed, so the engine
never promises a write that did not happen. Four tests moved from asserting
`Skip` to asserting `Capture` on that path; the fifth is renamed
`ingest_capture_with_empty_extractions_still_reads`.

## The closure verb — completion + the relayed forget gesture

The plan's top-level **`closures` array**
([ingest prompt](../../crates/mwe-core/prompts/ingest.md) Part 8) is the
ingest half of the verb *"ingest closes the validity of existing
facts"*. Two fronts share it, per the maintainer's decisions (2026-06-11,
recorded in the decision log):

- **Completion** — the message states an open item is spent: *"ieri sera
  abbiamo visto Jumanji"* closes the recalled watchlist fact as
  `completed`; *"ho comprato il latte"* closes the open shopping item.
  Completion is distinct from contradiction (`supersede_target`) by
  design: a watched film contradicts nothing, yet the intention is
  spent — this verb lets a consumable close without a contradicting
  fact.
- **The relayed forget/abandon gesture** — *"dimentica quello che ti ho
  detto sulla serra: ho abbandonato il progetto"* closes every recalled
  serra fact as `retracted`. The **blast radius is the LLM's judgment**
  (no hardcoded gate): the prompt instructs it to close the recalled
  facts the gesture covers, and usually to also capture one small
  outcome fact ("ha abbandonato il progetto della serra") so the wiki
  narrates the abandonment.

**The topic pass — aim correction for a recall-starved gesture.** The
whole-message embedding can wash the gesture's topic out of the first
recall window (the dogfood re-run measured it: *"dimentica quello che ti
ho detto sulla serra…"* ranked a dozen shopping items above the serra
facts), and a classifier that cannot see its targets must not guess —
the prompt's **precision rule**: close nothing rather than a doubtful
target. Instead it names the gesture's topics in the top-level
`closure_topics` array; the orchestrator re-recalls each topic as its
own focused query (promoted facts + the fresh buffered slot, capped by
`CLOSURE_TOPICS_CAP` and the recall `top_k` — resource caps only) and a
strict confirmer on the same ingest slot
([`confirm_topic_closures`](../../crates/mwe-core/src/ingest.rs),
[`ingest-closures` prompt](../../crates/mwe-core/prompts/ingest-closures.md))
picks the closures from the candidate union — an empty answer is always
valid, and the confirmer is bound to its candidates by the same
anti-hallucination rule. Confirmed closures merge with the plan's own
and apply through the single act-first path below.

Per element: `target` (a `fact_id` — same **anti-hallucination rule** as
`supersede_target`: it must appear in this turn's `recalled_memory`,
enforced by [`validate_closure`](../../crates/mwe-core/src/ingest.rs)),
`reason` (`completed` | `retracted` | `contradicted`, mapped onto the
[`fact_index::decay`](../../crates/mwe-core/src/fact_index.rs)
vocabulary), and an optional `valid_to` (resolved against
`current_time`; ISO-validated through the same `normalize_iso_bound`
the validity-edit path uses, so an absent **or malformed** bound falls
back to the turn's instant rather than poisoning the stored row). The third reason
is the **cluster front**: a cancellation supersedes the
fact it directly replaces, and its recalled *satellites* — itinerary
days, packing list — fall with it as `contradicted` closures; the
cluster's extent is the LLM's judgment, and the
[REM contradiction sweep](rem-cycle.md#contradiction-sweep-sub-job)
catches the satellites ingest could not see.

[`apply_plan_closures`](../../crates/mwe-core/src/ingest.rs) executes
**act-first**: each target is stamped via `fact_index::close_validity` —
falling through to `capture_buffer::close_validity` when the target is a
**still-buffered capture** (the same-day flow; the id is stable across
promotion, and the fresh-capture recall slot is what surfaced it) — then
ONE born-applied `wiki_promote` receipt (variant
[`validity_close`](proposal-apply-engine.md#promote-handler)) records the
turn's closures with each target's previous window snapshotted, and a
`structure_applied` event points at the dashboard, **where the closure
can be reverted** within the standard window. Closure is a validity
statement, never a tombstone: the fact row stays, the page recompiles on
the next dream (the validity fields are in the page fingerprint), and
the prose then narrates the closure ("comprato il…", "progetto
abbandonato").

**The registry twin — list pages stay current.** A `completed` closure
usually arrives together with the outcome extraction recording the event
(*"Galadriel ha comprato il latte"*). When the spent item lives on an
open-items `lista` page, the prompt routes that event fact onto the
list's **registry twin** (Part 8: `spesa` → `spesa_registro`, a watchlist
→ the watched log — the twin's name is the LLM's choice in the user's
language, `style: lista`), never back onto the list itself: the list
holds what is still open, the twin holds the consumption history. The
closed item stays on its list, rendered with the Record Writer's
[done-cue](narrative-compiler.md#the-record-writer--lista-pages-no-llm);
the consolidation prompts
([`conciliatore`](../../crates/mwe-core/prompts/conciliatore.md),
[`rem-merge`](../../crates/mwe-core/prompts/rem-merge.md)) are instructed
never to fold a registry twin back onto its list. Registry entries are
not kept forever — they age out through organic forgetting
(roadmap group 11).

Every step is soft — an invalid closure, a vanished target, or a DB
hiccup is logged and skipped, never killing the turn. A turn whose only
content is closures is real activity (intent `capture`, empty
`extractions`): it does **not** demote to the skip fallback. What stays
`structural` is the **time-ranged batch wipe** ("cancella tutti i fatti
di ieri") — erasure by time range is a container-level operation for the
dashboard, not a content gesture.

The REM-side **safety-net sweep** (closing completions ingest could not
see in its recall window) is the other half of the verb — the
[completion sweep sub-job](rem-cycle.md#completion-sweep-sub-job), which
ends in the same `validity_close` receipt + notice.

## Operation-path edits — validity-edit + acl-change

Two sibling verbs ride the same act-first / warn-and-skip / receipt
shape as the closure verb, letting the owner repair a *stored* fact
straight from a conversation. Both are **standard memory wikis only** (a
smart wiki is markerless — its facts carry no per-fragment validity or
ACL), both target an explicit fact in this turn's `recalled_memory` (the
same anti-hallucination rule as the closure verb), and both apply a
single deterministic gate — the **owner gate** — leaving every semantic
decision to the LLM:

> The sender may edit a recalled fact's dates or sharing **only when they
> own it** (`hit.owner_id == user:<sender>`,
> [`validate_validity_edit`](../../crates/mwe-core/src/ingest.rs) /
> [`validate_acl_change`](../../crates/mwe-core/src/ingest.rs)). A
> non-owner's element is silently skipped — owner-or-admin is the only
> deterministic gate, the LLM resolves all natural-language semantics.

- **`validity_edits`** ([prompt](../../crates/mwe-core/prompts/ingest.md)
  Part 10) — *corrects* a fact's validity interval: *"il latte scade il
  20, non il 25"*, *"il progetto è iniziato a marzo"*. Distinct from a
  closure: a correction repairs `valid_from` / `valid_to` and **never
  touches `decay_reason`** (a wrong date is not a completion/retraction).
  Each element gives `target` + at least one of `valid_from` /
  `valid_to`; a `null` bound LEAVES that bound unchanged (the
  COALESCE-in-Rust in
  [`fact_index::set_validity`](../../crates/mwe-core/src/fact_index.rs),
  of which `close_validity` / `restore_validity` are special cases),
  any provided bound must parse as ISO-8601.
  [`apply_plan_validity_edits`](../../crates/mwe-core/src/ingest.rs)
  stamps the fact row first, falling through to a still-buffered capture
  ([`capture_buffer::set_validity`](../../crates/mwe-core/src/capture_buffer.rs)),
  then emits one born-applied `validity_edit` receipt + the
  `structure_applied` notice; the dashboard reverts via
  `restore_validity_interval`.
- **`acl_changes`** ([prompt](../../crates/mwe-core/prompts/ingest.md)
  Part 11) — changes *who can read* a fact: *"esponi questa cosa a
  tutti"* → `allow_ids: [global]`, *"condividila col gruppo famiglia"* →
  add `group:famiglia`. The LLM resolves the scope to principals (the
  `allow_ids` list REPLACES the old one) — and because `recalled_memory`
  now surfaces each fact's current `owner` and `allow` (carried on the
  recall hit), it starts from the existing list and adds/removes against
  it instead of dropping principals it cannot see; `owner_id` defaults to
  the existing owner when omitted.
  [`apply_plan_acl_changes`](../../crates/mwe-core/src/ingest.rs) writes
  the ACL via
  [`fact_index::set_acl`](../../crates/mwe-core/src/fact_index.rs)
  (buffer fallback `capture_buffer::set_acl`), computes the
  **disclosure-widening** signal
  ([`acl::widens`](../../crates/mwe-core/src/acl.rs): true when a new
  principal enters the effective read-set), records an immutable
  [`disclosure_audit`](../../crates/mwe-core/src/disclosure_audit.rs) row
  per change (migration `0043`), then emits one born-applied `acl_change`
  receipt + notice. Widening is applied act-first (no pre-confirm) — the
  audit row + the revertible receipt are the accountability. The
  dashboard reverts via `restore_acl` and stamps the audit row reverted.

Both are soft end to end (an invalid element, a vanished target, or a DB
hiccup is logged and skipped, never killing the turn) and both count as
real activity — a turn whose only content is an edit does **not** demote
to the skip fallback. The dashboard fact-action surface that lets the
operator drive the same edits by hand is a separate step.

## Media attachments — the turn's photos become described facts (prompt v2.27)

A turn may carry media uploaded out of band via `POST /media`
([media pipeline](media-pipeline.md) — the full design SSOT). The
dispatcher resolves each `attachments[]` entry against the media
catalog (id must parse, exist and be **readable by the effective
sender**; the row's `kind` is authoritative) and threads
`IngestAttachment`s into the request. The orchestrator then:

1. **Backfills annotations** — a caption/description arriving with the
   ingest fills the catalog row's empty slots (fill-only, soft).
2. **Loads the vision bytes** — every `photo` without a
   consumer-supplied `description` rides the classifier call as an
   inline image part (`CompletionRequest.images`; caps 4 images /
   8 MiB per turn; soft-skip per item). A `description` is trusted as
   the consumer's own vision and the bytes stay home — this is also
   the degrade path for a text-only ingest slot (`all-local`).
3. **Prompts the claim** — the `attachments:` prompt section lists
   id/kind/caption/description; the prompt (v2.27, Part 9) instructs
   the model to fuse what it sees with the caption into an extraction
   `body` and claim the id in that extraction's `attachments` array —
   the model **never writes marker syntax**.
4. **Renders the markers by code** — claimed ids are validated against
   the turn's attachment window (unknown ids dropped with a warning,
   the `supersede_target` anti-hallucination stance) and the
   orchestrator appends `{{embed=…}}` to the validated body
   (`capture::render_embed_marker`), inside the fact's future region.
5. **Widens the media ACL** — after the fact files, each linked
   catalog row's `allow_ids` is unioned with the fact's read set
   (monotone widening only, soft-fail).
6. **Never strands described media** — attachments the routed plan did
   not claim (a skip/recall turn with a photo, a forgotten claim, an
   unparseable plan, the LLM down) are filed by a deterministic
   fallback as one buffered fact each into the sender's identity wiki,
   description (or caption) as body plus the marker. An unclaimed
   attachment with **no describing text at all** files nothing: a fact
   whose whole body would be the kind word ("audio") has no recall
   surface and only pollutes the page — the blob stays catalogued and
   reachable (dashboard/media), outside the wiki. The transcript of a
   voice note is the message text itself, already captured as facts;
   the recording adds nothing a bare embed could recall.

The similarity surfaces (embedding, n-gram dedup, the fresh-slot
re-embed) compare marker-stripped text — the catalog id is a key, not
prose; the stored body keeps the marker.

## Pasted documents — the oversized turn promotes itself (roadmap 46)

The inverse gesture of an attachment: the document arrives *as* the
turn text (a forwarded email, a pasted report). Before the
conversational pipeline runs, the dispatcher applies the verbatim
source promotion backstop
([document-ingest §promotion](document-ingest.md#verbatim-source-promotion--the-promote-dial)):
an **oversized document-shaped user turn** (deterministic shape
heuristic behind a `message_min_chars` pre-gate; the caller's
`promote: always | never` dial wins) is materialised verbatim as a
`doc` blob + catalog row and enqueued as a **document job**, and the
turn the per-turn pipeline actually ingests becomes a bounded head
excerpt + a hand-off note, with the promoted document riding the same
turn as a linked attachment (the seam above — embed marker, ACL
widening and the no-stranding fallback all apply unchanged). One paste,
two coherent memories: the conversational fact "sent this document"
with the link, and the document's own consult/dossier/dissolve
lifecycle citing the preserved original. Guests never promote;
`dashboard_command` and assistant-authored turns are exempt; the
response carries `document_promoted`.

## The recall block — recalled memory (the `rules` field is separate)

`context_snippet` is the **recall block** of recalled **memory** only —
standing **directives** ride the dedicated [`rules`](#the-rules-field--behaviour-directives-kept-apart-from-memory)
field, never here (roadmap 29d). The block is a sequence of
**role-labelled sections** in a canonical order
([`assemble_recall_block`](../../crates/mwe-core/src/ingest.rs)): each
section opens with a stable UPPERCASE English header, carries one bullet
per line, and is **omitted entirely when empty** (header included);
all-empty keeps the `None` contract. Sections are fitted **whole-bullet**
against their budget (`fit_bullets`: newest-first, the oldest tail falls
off — never a mid-word cut; only a pathological first bullet longer than
the whole budget is char-truncated with an ellipsis). The `YOUR RULES`
section of the injected turn context is the `rules` field, which the host
places adjacent to this block (the hermes bridge leads with it).

1. **`WHO YOU ARE`** — the agent's identity (roadmap 27d read side): the
   agent wiki's `_meta.summary` line leads (the compiled autobiography's
   abstract, refreshed by the compiler's abstract sync), then the
   identity self-facts — `salience high` **or** `fact_type bio`, always
   user-agnostic (identity facts are never partner-tagged at capture).
   Budget: `max_agent_identity_chars` (default 900).
2. **`WHO IS SPEAKING`** — the sender's identity card, and the one
   **deterministic** slot in the block: a label line
   `<sender_id> — <their wiki's _meta.summary>`, then the sender's
   **card (`profile.md`) itself**. Serving the page rather than an abstract of it
   is what makes the identity core — name, birthdate, contacts, family
   ties, but also the standing health constraints and the characterising
   preferences a `bio`-typed query would miss — arrive on *every* turn, at
   no walk, no model decision and no page open. It is the page because the
   routing decision was already made once, at write time, by the component
   that read the sentence: **the identity page is what the classifier
   decided belongs on the identity page**, and re-deriving that at read
   time from two columns is a second, worse copy of it.

2b. **`PEOPLE THIS TURN NAMES`** — the same treatment for the enrolled
   people the turn **names**, served beside the speaker's card and bounded
   by `max_mentioned_cards` (default 2). Deterministic and free:
   `recall::turn_subjects` is a word match over the roster, no model call.

   *Why it is served rather than found.* Similarity does not reliably surface
   who someone **is**. Measured on the live corpus (`examples/subject_gate.rs`)
   the phrase *«sta sera cucino io, cosa faccio per carol?»* returns ten
   facts about household expenses, meal prep and baby clothes and **neither
   the coeliac disease nor the pregnancy**; reworded to *«cosa cucino stasera
   per carol?»* both appear, at ranks 3 and 8. The same question, reworded,
   gets a different answer, because that turn's candidates sit inside a
   0.585–0.612 band where the wording decides the order. Nor does the walk
   save it: what a real navigator reaches is *«Mini Muffin (senza glutine e
   senza lattosio)»* — a property of a muffin, not a constraint on a person.

   *Why the coarse gate is the right one.* The card holds what is worth
   knowing about someone **whenever they come up**, so a passing mention is
   not a false positive — it is a cheap piece of context. What bounds the slot
   is the count, not a cleverer gate.

   *Projected for the reader, never the subject.* [`identity_card`] receives
   the **sender**, so a region on a third party's card that this reader may
   not see is redacted before it reaches them. This slot is what makes that
   invariant load-bearing rather than a no-op.

   Both slots feed the same three dedups as `WHO IS SPEAKING`: the served
   pages leave the flat hit list, are handed to `navigate` as already
   visited, and count as surfaced in the recall log.

   **And a fourth thing, added 2026-08-04: their rails.** Every card served
   here also reaches `navigate` as `Served::cards` — the same projection with
   its `[[wikilinks]]` intact and before the character cut. A page marked
   already-visited never reaches `open_target`, which is the only place a rail
   is harvested, so until then a card's own links were the one part of the
   corpus the funnel could reach by no route at all. They are offered as
   tier-2 `card` candidates, below the whole entry fan, because a card arrives
   whatever was asked and so says nothing about *this* turn — see
   [recall-pipeline.md](recall-pipeline.md#inside-the-funnel--where-the-doors-come-from-hop-by-hop).

   Four properties hold whatever REM later writes there:

   - **Projected per sender** (`render::render_for_sender_segments`), never
     a raw marker and never a precomputed blob — which fragments a reader
     may see depends on who is asking.
   - **Testata dropped, `[[wikilinks]] rendered plainly`** — at injection
     only. The links are the navigator's rails and are never touched on
     disk; a link becomes its `|display` alias, or the last path segment.
   - **Injected once, and never re-read.** The page's prose can also arrive
     as a flat hit, which drops it by source path; the recall log counts the
     card among the pages the turn surfaced, so restating a card fact is not
     logged as a recall miss. It cannot arrive as a navigated fragment at
     all: the funnel is handed the page as **already visited**, so it is
     neither offered as a candidate nor opened by any route — fan seed,
     directory sibling or `[[wikilink]]`. **The identity page is not a
     navigation destination for its own owner** (founder's ruling,
     2026-08-03: the recalled facts already land on the pages that answer
     the turn, so the hub's routing buys nothing and a page open is the
     scarcest thing the walk has). A *subject's* card is a different page
     and stays navigable — nothing has served it.
   - **A page with no readable fact is scaffolding, not a card** — a freshly
     seeded `profile.md` is a heading and some connective tissue. The slot
     then degrades to the label line alone, and is omitted entirely when
     there is neither card nor summary.

   Budget: `max_sender_identity_chars` (default 2500) — a **failsafe**, not
   a curation knob. It fits **whole paragraphs** and logs a warning when it
   fires, because firing means the card outgrew the ceiling REM curates it
   toward and something the author put last stopped reaching the turn.
3. **`YOUR RECENT HISTORY WITH THIS USER`** — the agent's episodes
   **with the speaking user only**, newest first. Scoping is the
   **exclusive partner tag**: at capture
   (`capture_agent_self_fact`) the served user's id is force-tagged as a
   topic and any *other* enrolled user's id the classifier put in
   `topics` is stripped (a mere mention must not become someone else's
   history); the read side then filters `topics ∋ sender`. Budget:
   `max_agent_history_chars` (default 1400).
4. **`RELEVANT MEMORY`** — the flat hit-list rebuilt from the step-1
   hits ([`format_snippet`]; carries the `Recent (not yet consolidated):`
   fresh slot and the
   `Project documentation (reference — never file this as a fact):`
   slot, which opens when the message **names a project** (before the
   classifier) or when a project **signpost surfaced** and the classifier
   judged the docs worth opening (after it) — see
   [recall-pipeline.md](recall-pipeline.md#the-project-docs-slot--two-entry-points-at-two-different-stages)).
   The recalled facts themselves are **facts only**: smart-wiki
   documentation lives in another table and only enters through that one
   labelled slot, which is why an ordinary conversational turn is no
   longer buried under project docs. The label is
   load-bearing — the ingest prompt's REFERENCE, NOT MEMORY rule keys on
   it so a documentation paragraph is never filed back as a fact about
   the sender. It is
   **deterministic**, never an LLM recap: the classifier runs before the
   navigator on the shallow hits alone, so a prose recap it wrote here
   could assert a false negative the navigator then contradicts two
   sections down — composing the answer is the consumer's job. Each
   line ends with an **in-band trust tag** — `[noted <date>]`, plus
   `· valid to <date>` when the fact carries a validity horizon. The tag
   is dates only, raw: no expired/stale verdict is computed in Rust —
   the consumer model knows today's date and judges staleness itself
   (validity is a signal, never a filter). Two filters keep the slot
   honest: a durable hit **homed on a page the navigator injected below
   is dropped** (rendering happens after navigation, so the dedup is by
   the page's workdir-relative source path; fresh hits have no page and
   are never deduped), and a hit homed on a `rules.md` page is skipped —
   directives are channel-only.

   A third gate sits above those two per-hit filters, and unlike them it
   is **turn-level, not per-hit**: `relevance_floor`
   ([`recall::DEFAULT_RELEVANCE_FLOOR`] = `0.0`, i.e. it ships **off** —
   the number it once carried was never earned; operator-overridable as
   `recall.relevance_floor`) is compared against the **maximum** score
   among the turn's promoted (non-fresh) hits — computed over every
   promoted hit, before the two filters above ever run, so the outcome
   never depends on which pages the navigator happened to open this same
   turn. Below the floor, **none** of the promoted hits render — the
   promoted half of the slot is not opened at all, not "the weak hits are
   trimmed". At or above it, **every** promoted hit renders, including
   ones individually weaker than the floor. A per-hit threshold cannot do
   this job: measured on 60 real user turns pulled from the training
   spool, a real answer's score band and injected noise's score band
   overlap — on one turn the answer scored `0.4813`/`0.4811` while on
   another the noise it replaced ran to `0.4306` — so any per-hit cut
   that removes one removes the other too (card 61, §37-39, where the
   full distribution and the 0.45-vs-0.48 reasoning live). The **fresh**
   sub-slot above is never gated by it — a different signal, kept even
   when the promoted half renders nothing — and neither is `UPCOMING`
   (6, below) or the project-docs slot (both already have their own
   floor). `0` disables the gate, same idiom as the smart-corpus funnel.
5. **`NAVIGATED PAGES`** — the prose the navigator funnel collected,
   one `(wiki/page)`-headed, sender-projected fragment per opened page.
   The header also carries the page's **freshness** —
   `(wiki/page · updated <date>)`, the `MAX(updated_at)` of the page's
   active facts (`fact_index::latest_page_activity`; recall-counter
   bumps never move it) — so the model can weigh how current the prose
   is without dashboard inspection. Best-effort: a lookup failure drops
   the annotation, never the fragment.
   This is the [recall-as-navigation](recall-pipeline.md#entry-point-gathering--recall_nav-navigation-phase-1)
   runtime path: `recall_nav::gather_entry_points` builds the seed fan and
   `recall_nav::navigate` runs the funnel. The reserved `rules.md` policy
   page is **not navigable** (roadmap 41e): the sibling fan and wikilink
   hops never offer it, a RAG hit homed on it seeds the wiki root
   instead, and `open_target` discards it as a fail-safe even when the
   navigator asks for it verbatim.
6. **`UPCOMING`** — facts whose validity window closes inside the
   operator horizon (`recall::recall_due_soon`), most imminent first, each
   rendered with its `valid_to` (`[due <ISO>]`).

Division of labour and cost discipline:

- **The navigator is a separate, optional backend.** `wiki_ingest_message`
  takes `navigator: Option<&dyn LlmBackend>` next to the classifier `llm` —
  two different config slots by design (the classifier wants the fast
  workhorse profile, the navigator the strong-but-cheap `navigator` slot).
  `None` = navigation off; both production call sites (the MCP dispatcher
  and the dashboard chat) build it from the `navigator` slot **best-effort**
  — a missing or unbuildable slot degrades to flat-only recall, never a
  failed turn.
- **Navigation runs only when the intent justifies the LLM spend**:
  `capture`, `recall`, or a disambiguation turn. A pure `skip` or a
  `structural` nudge never pays a navigator completion. (Gate to validate
  on the dogfood; an always-on flag is a one-line change.)
- **The navigation seeds are the classifier's own output, reused**:
  topics = the union of the plan's capture-unit `topics`, owners = the
  units' parsed `owner_id`s, RAG seeds = the step-1 flat hits (fresh
  included — they seed their wiki's root). For a recall intent topics and
  owners are typically empty, leaving the principal + RAG fan — the
  designed degenerate case. Situational seeds stay empty until the host
  adapter supplies them (context model, group 3).
- **The due-soon slot is time-driven, not query-driven**, and a plain DB
  pull (no LLM) — so it runs on **every** LLM-routed turn regardless of
  intent: an imminent commitment surfaces even when the message asks
  nothing (this is the in-turn half of reminder delivery; active firing is
  group 8).
- **Every tail failure is soft**: gather, navigate, and due-soon each
  log a warning and drop their section; the turn survives on whatever the
  flat path produced.

### The `rules` field — behaviour directives kept apart from memory

Standing **behaviour directives** are not memory and never ride
`context_snippet`. They are a dedicated first-level field on the response,
`IngestResponse.rules`
([`assemble_rules_block`](../../crates/mwe-core/src/ingest.rs), roadmap 29d), so
the consumer can tell a binding rule apart from a remembered fact and **apply**
it (rather than relay it). It carries the behaviour rules in force for the
served user, order pinned and most specific last — the agent-wide rules, then
the user's **user-global** rules (their own identity-wiki `rules.md`, roadmap
42), then the user's per-user rules for this agent (see
[Agent behaviour rules](#agent-behaviour-rules--routed-by-scope-outside-fact-memory))
— led by a one-shot **notice** when a non-admin's agent-wide change was refused
this turn. The directives section is **self-labelled** with the stable
`YOUR RULES (…)` role header (apply-don't-relay wording included), one rule
per line, whole-bullet fitted against `max_sender_rules_chars` — so a bridge
injects the field verbatim, with no preamble of its own, wherever it places
it in the assembled turn context (the hermes bridge leads with it, before
the recall block). Both halves are behaviour-only: privacy/sharing never
appears here — it is enforced memory-side by the per-fragment ACL, so the
agent simply never recalls what it may not see. `None` when the turn
surfaced no directive; the degraded (LLM-down) path computes none.

## Plan JSON — strict shape, lenient parser

The system prompt asks for strict JSON. Real LLMs reliably wrap their
output in markdown fences or leading prose, so [`parse_plan`] scans for
the first balanced `{...}` block and ignores everything else
(`parse_plan_extracts_json_after_prose` test). String contents
containing `{` or `}` are handled with a state machine, not regex
(`parse_plan_handles_nested_braces_in_strings` test).

The deserialiser accepts unknown fields silently (forward-compat) and
defaults every optional field. Required: only `intent`.

One turn-level field is a **judgement rather than data**:
`needs_project_docs` (prompt v2.45, roadmap 48i). When the recall block
carried a project **signpost**, the classifier answers whether reading
that project's documentation would help *answer this turn* — true for a
symptom or a capability question, false when the project is merely
around the message (an invoice, an appointment, an errand). The
orchestrator then runs the second half of the project-docs slot
([recall-pipeline.md](recall-pipeline.md#why-the-second-gate-is-a-judgement-and-not-a-threshold)).
It defaults to `false`, so an older prompt or any fallback plan simply
never digs — the expensive direction is never the default. It is a
judgement because it was first built as a similarity threshold and
measured: no similarity signal separates the two cases, and the model
already runs, so the field is free.

The per-fact fields are validated through `CaptureUnit` — one per `extractions`
element, or one synthesised from the legacy top-level fields — so the
reconstruction below applies uniformly whether the plan is multi-fact
or single-fact:

- `target_wiki_id` ⇒ not read from the model at all (derived — see
  "Destination" above). `MissingTargetWiki` now means the *derivation*
  exhausted every arm: the sender has no writable wiki.
- `target_page` ⇒ on a `lista` extraction, normalised by
  `normalize_capture_page` (elsewhere discarded before it gets here): the
  LLM-proposed page is untrusted, so it runs through
  [`planner::canonical_page_path`](../../crates/mwe-core/src/planner.rs)
  — every path segment through the canonical `planner::slugify`
  (lowercase, runs of non-alphanumerics → `_`), `.md` re-appended; the
  REM auto-promote target shares the same chokepoint
  ([rem-cycle.md](rem-cycle.md)) — and anything that still fails
  `is_safe_page_path` (a traversal-laden name, a segment that slugifies
  to nothing) falls back to `policy.default_page` (`notes.md`, the buffer page). One
  topic therefore always lands on one page even when the model spells it
  differently across turns (`lista-spesa` / `Lista della Spesa` /
  `lista_spesa` → `lista_spesa.md`); extension-less page files (which
  break the `.md`-page convention every reader walks and stay hidden from
  a `.md`-only `wiki_read`) and the hard `internal_error` a non-safe
  page would otherwise raise are prevented by the same pass. The
  classifier prompt lists wikis but never page names, so
  canonicalisation cannot fight a name the model copied from disk.
- `owner_id` ⇒ `user:<sender>`.
- `body` ⇒ for the legacy single unit, falls back to raw `request.text`
  when the model omits it; a multi-fact extraction **must** carry its
  own `body` (`MissingBody`) so the whole message is not duplicated
  under every fact.
- The classifier is **prose-only**: there are no `wiki_type` / `fields` /
  `purpose` fields and no route-or-create step — every fact files as prose
  to its `target_wiki_id`.

## ACL projection

The orchestrator never bypasses ACL. Recall (step 1) goes through
[`recall::wiki_recall`] which already applies `acl::can_read` to every
candidate, so the LLM only sees rows the sender is authorised to read.
The capture step (step 4) writes a region tagged with the LLM's
`owner_id` + `allow_ids` (validated as `Principal`), so the same ACL
discipline carries to the new row. One normalization applies on this
LLM-fed path: a classifier that echoes the **sender** into `allow_ids`
is expected noise, and `validate_capture_plan` strips it — capture's
`SenderRedundantInAllow` lint stays strict for hand-written callers but
can never kill an ingest turn.

A second normalization is the **enrollment guard on `owner_id`** (the
engine floor under the 2026-06-30 subject-owner ruling): an owner that
parses as a `Principal` but that enrollment does not back — a principal
the classifier coined despite the prompt's `known_users` roster — is
cleared before validation
([`enrollment::principal_exists`](../../crates/mwe-core/src/enrollment.rs),
fail-open on a DB error, `warn`-logged), so the unit re-owns to the
sender default. An **enrolled** third-party subject (a reciprocal
relationship fact, a fact filed for another family member) passes
untouched — the owner axis is the subject, not the interlocutor. The
document path applies the same guard on its filing loop
([document-ingest.md](document-ingest.md)).

A third normalization is the **agent-wiki guard on the target**. Owner
and `target_wiki_id` are deliberately **independent** axes: the owner is
the subject and the ACL, the wiki is conceptual organisation — so a
`group:`-owned fact may legitimately live in a user's wiki and a
`user:`-owned one in a group wiki (the classifier prompt says so
explicitly, with worked examples), and the engine does not couple them.
The one placement that is never legitimate is **somebody else's** fact
filed in an **agent's** wiki: that wiki holds one subject, the agent. So
when the resolved target carries `is_agent` (the flag rides
`AvailableWiki` from the wiki's `_meta`), `validate_capture_plan`
**redirects** the write to the owner's own wiki when it is in the turn's
window, and **drops** the extraction when it is not
(`TargetIsAgentWiki` → skip + warn, rather than misfile).

Two things the guard deliberately does *not* do, both of which would
destroy facts rather than place them:

- **The owner being the agent itself is exempt.** An identity wiki's id
  *is* its principal's id, so `home == target` means the agent's wiki is
  the owner's own home — the one place the fact belongs. This is the
  ordinary case of a **user** stating something about the assistant
  ("sei bravo con le pratiche INPS"): the `owner_id: "self"` sentinel
  upstream only fires on an *assistant* turn, so on a user turn the fact
  arrives here owned by the agent's own principal. Without the exemption
  the guard would hunt for a non-agent wiki named after the agent, find
  none, and drop every user-stated fact about the assistant.
- **The redirect looks for the owner's home and nothing else** — not for
  "a home that is not an agent's". With two bots enrolled, a fact about
  bot B aimed at bot A's wiki has a perfectly good home in B's own wiki.

Behaviour-rule and `self` facts never reach this function at all (both
pin their wiki in code, upstream). Every other owner⊥wiki placement the
classifier proposes is honoured.

The guard is the net, and since the classifier stopped naming wikis it is
also the belt: an agent's entry in the internal enumeration carries `is_agent:
true` (emitted only when set — a human's entry stays lean), and the prompt
rules an agent's wiki out as a destination for anyone else's fact. Without
that line the classifier saw `type: wiki-user, title: Hermes` and had only
the title to guess from, so the guard was doing the routing. Note this
depends on the marker being **on disk**: it rides `AvailableWiki` from the
wiki's `_meta.md`, so an agent marked only in the DB left the whole
mechanism inert — which is why the marker is now stamped on every standard
connect ([identity-and-acl.md](../concepts/identity-and-acl.md) §1.5).

The subject-owner axiom has a **delivery half**: a fact that files
owned by an enrolled user who is not the human of the conversation is
news *to that user*, and the recipient must not have to stumble on it
via recall. The filing loop accumulates such facts per beneficiary and,
after the loop, emits one **`fact_minted_for_you`** event per recipient
on the reverse channel (`events_poll`) — batched, so a turn that mints
five facts for the same user is one notice, not five. The payload
carries the fact bodies themselves (the consumer's agent delivers the
content, not a pointer), `from_user_id` (the human whose turn minted
them — on an assistant turn `request.sender_id` stays the interlocutor;
the roadmap-27 flip touches only the fact's `sender` axis), and
`origin` (`user_turn` | `assistant_turn`). A dedup-skipped direct write
emits nothing (nothing new was minted), group-owned facts are communal,
and agent principals are skipped (`is_agent` — no inbox). Emission is
non-fatal: a lost notice never demotes the turn. The document path
mirrors this per job ([document-ingest.md](document-ingest.md));
draining and out-of-turn delivery are the bridge's job
(`INTEGRATING.md` step 8 — the hermes poll/ack daemon is roadmap 3j).

Because that payload carries the bodies, **the drain is scoped to the
addressee**: a consumer receives an addressed event only when it serves that
person — the caller's own token identity, the consumer's `system_user_id`, or
one of its `consumer_delegations.allowed_sender_ids` (the predicate lives in
the `events_poll` query, so a row we may not deliver is never read). Delivery
authority and act-as authority are deliberately the **same** table: a bot
allowed to speak *for* Bob is exactly the bot allowed to be told things *about*
Bob. Unaddressed and `group:` / `global` notices stay broadcast. See
[tool-reference §events_poll](../protocol/tool-reference.md#events_poll-read-only).

A pathological LLM that asks to capture into `user:bob`'s wiki when
`sender=alice` is **not rejected** at this layer — the
cross-user-attribution invariant lives in
[identity-and-acl.md](../concepts/identity-and-acl.md) and
is not yet enforced here, because the capture surface is still
"trusted internal LLM only" (planned — see the
roadmap). For now, the
agent's prompt + the explicit `sender_id` marker on the captured
region make the audit trail recoverable.

## Group-scope routing — `sender_groups` in the prompt

`owner_id` is the ACL principal a captured region belongs to. Deciding
`group:<id>` instead of `user:<sender>` is exactly what makes a fact
reach a shared family/team memory — but the model can only make that
call if it knows *which groups the sender belongs to* and *what each
group's memory is for*. That is operator knowledge: it lives in the
`enrollment_groups.scope` prose column the admin fills in from the
dashboard (e.g. for a `famiglia` group: "shopping and lists, house
rules, shared plans, presence/commitments, the kids' school; NOT:
personal facts irrelevant to the others").

`build_prompt` therefore injects a `sender_groups` section:

```text
sender_groups:
  - id: famiglia
    scope: Spesa e lista della spesa; regole della casa; piani condivisi; …
```

The orchestrator fetches the pairs with
[`enrollment::groups_with_scope_for`](../../crates/mwe-core/src/enrollment.rs)
— the scope-carrying sibling of `groups_for` — in a single round-trip,
and derives the bare-id [`SenderContext::sender_groups`] (used by the
ACL paths) from the same result. The number of groups is capped at
`policy.max_groups_in_prompt` (default 8) and each scope is truncated to
`policy.max_group_scope_chars` (default 1000 — sized to keep the scope's
exclusion clause, which is what teaches the model *not* to over-share).
A sender in no groups renders `sender_groups:\n  (none)`.

The `owner_id` instructions in the bundled prompt body tell the model to
compare the *meaning* of the fact against each group's scope and route
to `group:<id>` when it falls inside that domain — even when the message
never names the group — while honouring any exclusions the scope states.
Without `enrollment_groups.scope` reaching the classifier, family facts
fall back to private captures unless the message echoes a prompt few-shot
near-verbatim; injecting the scope is what lets the model route on meaning.

## User policy — `rules.md` read *and* written via ingest (prompt v2.21)

Where `sender_groups` is *operator* knowledge, `rules.md` is the *user's
own* standing **engine policy**: a prose page
([`wiki::RULES_FILENAME`](../../crates/mwe-core/src/wiki.rs)) whose free
prose holds **governance rules for the memory engine** — two families,
*privacy/sharing* and *do-not-store*. Per-agent behaviour rules like
"address me formally" belong to the **consumer's** own wiki, not here; the
user's **user-global** behaviour rules (roadmap 42) *do* share this page,
but as `{{f=…}}` fact regions the governance read strips — they ride the
dedicated rules channel, never the policy prose. The governance half is
*all prose, no metadata*: no rule is ever materialised onto the
wiki-level `scope` primitive (maintainer 2026-06-08
"tutto-prosa-nei-file"); enforcement is the soft read below. `rules.md`
is the user's front-end onto the per-fragment ACL pillar: the
"Epstein-files" granularity is dictated *once*, in prose, instead of
per-message.

**Read side.** The orchestrator reads the **sender's**
`rules.md` best-effort
([`ingest::sender_rules`](../../crates/mwe-core/src/ingest.rs): locate the
sender's identity wiki, read the page's free prose — fact regions
stripped, so a user-global behaviour rule is never injected as policy or
twice; absent/unreadable/no prose → `None`) and
`build_prompt` injects it as a `sender_rules:` section (truncated to
`policy.max_sender_rules_chars`, default 1500; `(none)` when absent). The
bundled prompt body tells the model to **honour the privacy/sharing rules
when it assigns each fact's `owner_id`/`allow_ids`** — an explicit user
rule ("keep health private", "always share X with the family")
*overrides* the scope-routing default above — and to **honour
do-not-store rules by dropping** the matching extraction (no capture at
all). It is an **aid, never a hard gate**: a sender with no `rules.md`
just gets `(none)` and the
classifier decides on its own (pillar: the LLM decides).

**Write side.** `rules.md` is also *written* through ingest —
the same universal classifier prompt, no special "wizard" path. When the
model marks an extraction `engine_rule: true` (a standing governance
directive — Part 7 of the prompt), the orchestrator routes it to
[`append_engine_rule`](../../crates/mwe-core/src/wiki.rs) instead of
[`capture::wiki_capture`]: the rule's `body` is appended as a prose bullet
to the **sender's** `rules.md` (seeded from the default body if missing)
and **nothing is filed in `fact_index`**
([`ingest::append_sender_rule`](../../crates/mwe-core/src/ingest.rs),
best-effort: a sender with no locatable wiki drops the rule rather than
failing the turn; a real IO error bubbles). This closes a tight
write→read loop: a rule written this turn is injected as `sender_rules`
the next. The discriminator is the LLM's, not a gate — a world/household
`rule` fact ("in casa non si fuma") stays a normal fact
(`engine_rule: false`); only a directive addressed to the *memory itself*
goes to `rules.md`. The rule's `body` is written **in the sender's own
language**: `rules.md` is the user's own
policy prose, appended verbatim and read straight back to them as
`sender_rules`, so it is never translated to English. The **first-login
wizard** is just a 3-step prompt composer over this same path;
it materialises nothing.

## Agent behaviour rules — routed by scope, outside fact memory

A **behaviour rule** is a directive about *how an agent converses or
operates* — as opposed to a *fact about the user* (the normal pipeline) or a
*governance rule for the memory engine* (the user's own `rules.md` prose, read
back as `sender_rules`). It is the **fourth ingest destination**, and unlike
the other three it never pollutes the user's fact memory: it lands on a
reserved `rules.md` page — the **consumer agent's own wiki** for the two
agent-scoped kinds, the **sender's identity wiki** for the user-global kind
(roadmap 42). The body is stored in the **imperative** ("Usa sempre Claude
Code", "Dammi del tu"), not the third person — the agent reads it and acts on
it.

**Scope is read from the addressee, and scope is the governance.** Every
behaviour rule carries a `behaviour_scope` the classifier sets from the
grammatical addressee (Part 7b), and scope alone drives home + owner +
authority:

- **per-user** — addressed to the speaker (*"-mi / con me / le mie"*) or a bare
  imperative with no audience: how THIS agent behaves WITH THIS USER. It touches
  only them, so **anyone may set one**; filed in the agent's wiki,
  `owner = the user`, recalled only for that user on that agent.
- **agent-wide** — impersonal / universal (*"con tutti / con chiunque"*, or a
  how-the-agent-works directive with no per-speaker scope): how the agent behaves
  for EVERYONE. So it is **admin-only**; filed in the agent's wiki,
  `owner = the agent`, recalled for every user.
- **user-global** — the user explicitly addresses EVERY assistant they talk to
  (*"tutti gli assistenti", "con qualunque assistente", "chiunque tu sia"*):
  how every assistant behaves WITH THIS USER. It binds only their own
  conversations, so **anyone may set one**; filed in the **sender's identity
  wiki**, `owner = the sender`, recalled by every consumer serving them —
  whichever consumer happened to hear it.

So a user shapes how the agent behaves *with them* — on one agent or on all of
theirs — but only the operator (`enrollment_users.is_admin` — one per
deployment) may change how one agent behaves *for everybody*. The classifier
only *classifies* the scope from the addressee; the engine enforces authority.
*soul vs operational* (style vs tools) is an **optional content tag** that
routes nothing — scope comes from the addressee alone, so every quadrant is
expressible: *"per le mie cose usa claude-code"* is operational AND per-user →
anyone may set it, `owner = the user`.

**Recognition.** The classifier marks an extraction `behaviour_rule: true` and
tags `behaviour_scope` (Part 7b); the LLM decides, no keyword gate, and defaults a
bare imperative to **per-user** (the open side). A `THE BOUNDARY` clause keeps a
directive-to-the-agent ("usa il Max quando lanci Claude Code") distinct from a
fact about the user ("Franz ha il Max", normal pipeline). A directive the user
makes explicitly universal across assistants ("voglio che TUTTI gli
assistenti…") stays a behaviour rule too — `behaviour_scope: "user-global"`,
**never** a `salience: "high"` identity fact (roadmap 42 retired that
workaround: a directive to assistants is conduct, not knowledge about the
user); the first-login wizard / dashboard identity fields remain the
identity-card route. Two more clauses guard the rule's durability and its
referents: **standing vs one-shot** — a behaviour rule must outlive the exchange,
so a command consumed by the very next reply ("di' solo: collegamento voce
funzionante", a channel test) is conversation, never a rule, and stores nothing;
and **naming deixis** — a rule body is read back cold, so the user naming the
AGENT ("ti chiamerò Hermes") is stored from the agent's side ("Il tuo nome per
questo utente è Hermes."), never as the referent-inverting "Chiamami Hermes.",
which the prompt reserves for the user asking to BE called something ("chiamami
Franz"). Renaming further requires an **explicit naming predicate** ("ti chiami
X", "il tuo nome è X", "chiamati X"): the agent's name used as an *address*
inside an unrelated command ("Hermes, abbassa il volume") never renames it,
however the address is spelled. The discriminator is the speech act, not
spelling proximity — there is deliberately **no edit-distance guard**, so an
explicit rename to a near-identical name still works, while a mangled vocative
(speech recognition gluing the next word onto the name) cannot silently
supersede the rule.

The classifier also holds the **engine-vs-behaviour routing boundary** (roadmap
29e), so privacy stays ACL-enforced and the consumer field stays behaviour-only:
*"don't **share/store** X"* → an `engine_rule` (ACL / do-not-store, memory-side);
*"don't **say/bring up** X with me"* → a `behaviour_rule`, per-user (a
conversational gag). A privacy directive is **one** engine/ACL rule — the
consumer-side silence is the automatic consequence of ACL-filtered recall (the
agent cannot leak what it never recalls), not a second stored rule.

**Write side.** For the two agent-scoped kinds the orchestrator resolves the
calling consumer's own wiki — its bound **system user**
([`consumers::system_user_for`](../../crates/mwe-core/src/consumers.rs), keyed by
the `consumer_id` threaded through `IngestRequest` from the auth layer) — and
files the rule there as a **live** fact on its **`rules.md`** page (roadmap 29c —
reclaiming the dead scaffolded slot; in the *agent's* wiki this page holds
behaviour facts, no collision since `sender_rules` never reads an agent wiki, the
agent never being a sender). A **user-global** rule skips the consumer
resolution entirely: its home is the **sender's identity wiki** `rules.md`,
alongside the governance prose (the page contract anticipates both — prose plus
`{{f=…}}` regions). Home + ownership are the scope
([`ingest::capture_behaviour_rule`](../../crates/mwe-core/src/ingest.rs) taking a
`BehaviourScope`):

- **per-user** → the agent's wiki, `owner = the served user`, so different
  users' rules are distinct facts and owner-scoped dedup
  ([capture-and-dedup.md](capture-and-dedup.md)) never folds one user's into
  another's (while still folding a user's own repeat).
- **agent-wide** → the agent's wiki, `owner = the agent`, one policy deduped
  across the agent's own standing rules. The dispatch reaches this write only
  after confirming the sender is the admin
  ([`enrollment::is_admin`](../../crates/mwe-core/src/enrollment.rs)). A
  **non-admin's agent-wide directive is refused**: nothing is filed, and the
  `rules` field carries a one-shot notice steering the agent to decline politely
  this turn (their own per-user preference it may still honour).
- **user-global** → the sender's identity wiki, `owner = the sender` — the same
  open authority as per-user (their own conversations only), with reach across
  every consumer serving them.

The user **revises** a rule by superseding it: the rules in force — all three
sources — are shown to the classifier with their `fact_id`s and scope tokens,
and a `supersede_target` among them routes through `wiki_supersede` instead of
an additive write (cross-wiki safe: the new region lands in its own scope's
home, the old one is stripped from its page wherever it lives). Authority
follows the target too: **only the admin may supersede an agent-wide rule** — a
non-admin's revision of the floor drops the supersede and files their directive
additively at its own scope, leaving the floor intact. The prompt guards the
verb (the same restatement guard as the completion sweep): a supersede requires
the new text to **change** the directive — the user merely *repeating* a rule
already in force is a dedup case, folded against the existing rule, never a
supersede. When no consumer wiki resolves — a **smart** consumer *is* its user —
the agent-scoped write falls back to the sender's own wiki, where per-user and
user-global deliberately collapse: everything on that page is the user's own
everywhere-set, served through the user-global source of the dedicated channel.

**Read side.** Recall for a turn served to user *X* unions the three sources
([`ingest::recall_behaviour_rules`](../../crates/mwe-core/src/ingest.rs) over
the dedicated
[`fact_index::find_behaviour_rules`](../../crates/mwe-core/src/fact_index.rs)
query), order pinned and most specific last: the **agent-wide** rules (the
agent's wiki, `owner = the agent` — the floor, applied for everyone), then
**X's user-global** rules (X's identity wiki, `owner = user:X` — their
everywhere-set), then **X's per-user** rules for this agent (the agent's wiki,
`owner = user:X`). A smart consumer (no distinct agent wiki) draws only the
user-global source. They are returned in the dedicated
[`rules`](#the-rules-field--behaviour-directives-kept-apart-from-memory)
field (roadmap 29d), flat — the per-rule scope rides only the classifier
injection (`agent_behaviour_rules`, for supersede targeting), not the consumer
section — and structurally apart from the recalled facts, so the agent
applies "how to behave with me" as an instruction rather than mistaking it for
memory. The page-scope keeps the agent's own self-facts (roadmap 27d,
`owner = agent` on its content pages) out of this channel, and the agent-wide
rules out of the self-context block. Two invariants live **in the SQL, before
the per-call cap** (`BEHAVIOUR_RULES_RECALL_CAP`, a resource cap):

- **the rules-page predicate** — the query matches `source_path` whose file
  name is `rules.md` (the SQL mirror of
  [`wiki::is_rules_page`](../../crates/mwe-core/src/wiki.rs)), so the cap
  counts *rules only*: however many newer facts the agent wiki accumulates
  under the same owner (self-facts above all), old rules never starve out of
  the `LIMIT` window;
- **the validity filter** — a rule whose validity window is closed at *now*
  (`valid_to` set and past) stops being served, while the fact stays (closing
  is never deleting), so the conversational closure path (`retracted`) really
  retires a directive. For ordinary facts a closed window is a recall
  *down-rank signal*, never a filter — the rules channel is the **deliberate
  exception**: a retracted rule must stop steering the agent.

**Durability — every structural door skips `rules.md`.** A behaviour rule's
home *is* the page: `recall_behaviour_rules` keys on it, and a rule leaves the
channel only via supersede or tombstone — so every pipeline that could re-home
or fold a fact treats the reserved policy page as the rules pipeline's
perimeter, exactly as they all skip smart wikis:

- the **narrative compiler's** fact-gather
  ([`planner::gather_standard_facts`](../../crates/mwe-core/src/planner.rs))
  skips every `rules.md` fact — otherwise a behaviour-rule fact (written by the
  direct path, so absent from the persisted plan) would look *new* on the next
  dream and orphan-fall-back onto the owner's buffer page;
- the **REM refile sweep** never *nominates* a `rules.md` fact (a per-user rule
  naturally embeds toward its user's wiki — a confirmed move would land it on a
  foreign wiki's buffer); rules facts still count in the similarity pools;
- **dedup never crosses the rules-page boundary** — a pair is nominable only
  when both sides are `rules.md` facts or neither is, at capture time
  ([capture-and-dedup.md](capture-and-dedup.md)) and in the REM revisor
  ([rem-cycle.md](rem-cycle.md)) alike, so an episodic restatement can neither
  absorb a rule nor be absorbed by one; rule-vs-rule still dedups.

These are structural **channel invariants** (which facts the channel sees,
which facts a sweep may nominate), not semantic gates — the LLM still decides
content.

**Governance stays controllable.** A per-user rule is `owner = the user`, so it
stays visible and correctable by that user from the dashboard facts browser — it
leaves the user's *fact* memory without leaving the user's *control*. A
user-global rule lives in the user's own identity wiki, owned by them — the most
direct control of all. An agent-wide rule is `owner = the agent`, the agent's
standing operation, editable by the admin who set it.

## The assistant pass — the agent remembers its own turn (agent-authored memory)

The **assistant turn is a second, special-ruled extraction source** — the
agent's own prior reply is fed back through ingest, so what the agent
concluded, advised, or worked out persists as memory instead of living only in
the reply. (The user's message stays the primary source; the assistant's turn
otherwise rides the prompt only as `recent_messages` *context*.)

A turn carries an `author` (`crate::ingest::IngestRequest::author`, a reused
`MessageRole`): `user` is the silent default; `assistant` means the `text` is the
agent's OWN prior reply, fed back for extraction. The wire is a flag on the
existing `wiki_ingest_message` (not a separate tool) — so the whole orchestrator
is reused, **including recall**, which the anti-loop guard below depends on. On
the bridge the consumer's answering behaviour is untouched: hermes's `sync_turn`
fires the pass on a daemon thread after the reply has gone out (act-as the user;
the server resolves the agent provenance from the consumer token), so it adds no
turn latency and a hiccup never kills a turn.

**The discriminator — prompt Part 9.** Wholesale ingest of the agent's prose is
poison (noise, opinions, regenerable world knowledge, feedback loops). Part 9 is
gated hard on the `author: assistant` context line `build_prompt` injects — on a
normal turn it is ignored and the prompt is byte-identical to before. When armed,
it classifies each thing the reply states into six kinds and keeps three:
**episodic sediment** ("discussed X, concluded Y, on D" — `owner_id:
user:<sender>`) and **personalised advice / a decision** — owned by its
**subject**: the sender in the normal case, another **enrolled** user when the
turn explicitly establishes that person must know and act on it (the
`owner_id` section's ABOUT-includes-FOR necessity test; the owner axis is the
subject, not the interlocutor) — plus the **self-fact** (`owner_id: "self"`,
below). When the owner is not the sender, the prompt's **beneficiary rule**
governs the body wording: the fact says the advice *passed through* the sender
(«ha spiegato a X cosa Y deve controllare»), never that the agent interacted
with the absent subject — a delivery that never happened must not be asserted
(the notification leg is the reverse channel's job, roadmap 3j). Filler, generic regenerable
knowledge, and **recall echoes** (anything `recalled_memory` already holds — you
recalled it, you did not derive it) are skipped; the default is hard-skip. The
canonical echo is **identification** — the reply reciting the user's identity
card from recall captures *nothing*, not even a "correctly identified the user"
episode. **Routine execution is not sediment** either (ran a command, deleted a
temp folder, answered a question): no self-fact, no user-side fact. And the two
sides of one event file **only when each is independently durable with its
subject matching its wiki** — a sentence whose grammatical subject is the agent
is never a user-wiki fact; one event never files twice just because two wikis
exist. The **no-transcript rule** keeps the engine's no-server-transcript
invariant: store the *distilled episode fact*, never the exchange. The
user-correction kind — the user **correcting or scolding** the agent — is not an
assistant-turn matter at all; it arrives on the *user's* turn and rides Part 7b
(widened to capture reprimands) into the agent's own wiki.

**Attribution.** At the capture site `cap_req.sender` is flipped to the agent
principal — resolved via `consumers::system_user_for(consumer_id)`, the *same*
binding the behaviour-rule path above uses — while `owner` stays whoever the fact
is about (the user, for episodic/advice; `global` for kept generic knowledge). So
the synthesis lands in the user's wiki and surfaces on their recall, but carries
the agent's provenance. That provenance **is** the trust tier: agent-derived
facts are inferences, not user-asserted ground truth, and stay down-weightable /
auditable / purgeable by their `sender=<agent>` — no new column, feeding the
gold-set discipline of item 15. When no consumer binding resolves (a smart
consumer IS its user) the flip no-ops and attribution is unchanged, exactly as a
user turn — the pass degrades to nothing rather than inventing a provenance.

This unlocks "ne avevamo già parlato — non ricordi?" (ordinary recall now
surfaces the agent's own episodic facts), accountability to past advice, and
proactive follow-up. It **complements** document text extraction (9j/21): that
captures the source exhaustively; this captures the agent's *synthesis*.

**The self side — the agent has a loaded sense of who it is (roadmap 27d core).**
The facts above are about the *user* (`owner=user`, in their wiki). The same
exchange also has an agent's-eye side, and that lands in the **agent's own**
wiki: Part 9's `owner_id: "self"` sentinel routes an extraction through
`capture_agent_self_fact` into the calling agent's wiki, **owned by the agent**
(`owner == sender == the agent` ⇒ no separate sender), auto-tagged with the
served user — *except* a high-salience **identity** fact, which stays untagged so
it is user-agnostic. **The sentinel has two accepted spellings**: the literal
`self` the prompt prescribes, and the agent's own principal written out
(`user:<agent>`) — the identical claim, and the form a model that knows its own
id reaches for instead. Both route here, because a claim the engine does not
recognise does not degrade gracefully: it files the diary entry in whatever wiki
`target_wiki_id` named, scattering the agent's history across its users' wikis.
The alias is unambiguous by construction — the agent principal resolves only on
a turn the agent authored, so on a user turn an owner naming the agent keeps its
ordinary meaning. **The engine chooses the page too**
([`agent_self_fact_page`](../../crates/mwe-core/src/ingest.rs)): an identity
self-fact lands on the agent's buffer page, from which the next compile's
`orphan_target` lifts it onto the agent's `profile.md` card (`salience: high`)
and the REM consolidates the autobiography; a relationship self-fact lands on `esperienze_<served-user>.md`
(through the same `normalize_capture_page` chokepoint). The classifier's own
`target_page` is **ignored** for self-facts — the same "the engine knows the
home" treatment the wiki axis already gets, and the reason the diary cannot
collapse into one heterogeneous catch-all page: a per-person history is a
coherent subject the REM can grow and split, a grab-bag of every user's episodes
is not. The read side is page-agnostic (it buckets by the served-user tag), so
the routing is a write-time concern only. So one INPS reply yields two facts:
"the user's deadline is 27/6" (`owner=user`, her wiki, surfaces on *her* recall) and "the agent helped
the user with the INPS filing" (`owner=self`, the agent's wiki, its own history)
— the two sides of one event, mirroring how two people each remember a
conversation from their own side. The read side closes the loop:
`recall_agent_self` leads the recall block (`context_snippet`) with WHO YOU ARE
(the agent's high-salience identity, always) + YOUR HISTORY WITH THIS USER (its
facts tagged with the served sender, scoped — one user's relationship never
surfaces in another's turn), as the first `assemble_recall_block` slot ahead of
the recalled facts. The self-facts it surfaces are **counted like any other
recall hit** (`fact_index::bump_recall_hits`, best-effort), so the agent's
autobiography accrues real `recall_count_30d` / `last_recall_at` instead of
reading as permanently cold to every recall-weighted REM pass. (Behaviour
rules ride the dedicated
[`rules` field](#the-rules-field--behaviour-directives-kept-apart-from-memory),
never this block.) The agent thus answers conscious of
itself and the relationship, not only
of the user. The **soul emerges from use** — the agent's wiki starts empty and
fills as it works; there is no seed file. The whole mechanism mirrors the
behaviour-rule read/write paths, attributed to the agent instead of the user.

The agent wiki self-describes via the `is_agent` `_meta` marker (a mirror of
the authoritative `consumers.system_user_id` binding — see
[memory-model.md](../concepts/memory-model.md)), so it is recognisable as an
agent's without a DB lookup, and the consolidation passes read it: the wiki's
pages are compiled in the **first person** (`compiler::resolve_tone` →
`agent-autobiography-first-person`, and `wiki::subject_directive` on the index
writer) instead of as a third-party dossier about the agent, and the dedup
confirmer is told that *who* an episode was lived with is part of the fact, so
two near-identical episodes with two different people stay two memories
([rem-cycle.md](rem-cycle.md)). What remains of **27d-rem** is organic
forgetting (item 11), so the agent's self decays like a human's rather than
only accreting — a first-class member with its own autobiography, not a
routing sink.

## Cross-user attribution — `known_users` in the prompt

Group-scope routing decides between `user:<sender>` and `group:<id>`.
The third `owner_id` target is **another named person**: a message from
Alice that says "Bob prefers tea" should file under `user:bob`, not under
Alice. The classifier can only make that call if it knows who is
enrolled and by what names. `build_prompt` therefore injects a
`known_users` block — the roster fetched once per turn by
[`enrollment::list_users`](../../crates/mwe-core/src/enrollment.rs)
(returns `Vec<EnrolledUserLite>`, each an `id` + the operator-declared
`aliases`):

```text
known_users:
  - id: bob
    aliases: Bob, Bobby, Roberto
  - id: hermes1
    aliases: Gandalf
    is_agent: true
```

The assistant is in the roster too — under the diagonal identity model it is
an enrolled user like any other — and `is_agent` (the `enrollment_users`
column, migration 0050) says which entry it is. Without it the "you" of every
turn reads as one more stranger in the list, and a sentence addressed **to**
the assistant is indistinguishable from a sentence **about** a third party;
with it the prompt can state the asymmetry outright: the agent is a real
principal (a fact whose subject is the agent is owned by it, Part 9's
`owner_id: "self"`), but a human name in the message never resolves onto that
entry, and a name addressed to it is address, not attribution — the same
discrimination the naming-deixis rule makes. Emitted only when true, so a
roster of humans carries no extra weight.

The prompt's `owner_id` rule tells the model to resolve the named person
to a canonical `user_id` through this roster (matching id or any alias)
and set `owner_id: "user:bob"` — **only** when that person appears in
`known_users`. A reference to someone not enrolled stays under
`user:<sender>` (a note the sender holds about a stranger), so the model
can never mint an `owner_id` for a principal that does not exist. The
roster is capped at `policy.max_users_in_prompt` (default 24, alphabetical
by id) to bound the context budget on large deployments.

Resolution is strictly **one-way**: it maps names and aliases the sender
actually wrote onto roster entries, never the reverse. The prompt's
relationships rules («Explicitly stated ONLY — never inferred», anchored
by the `bundled_ingest_prompt_carries_the_explicit_relationship_gate`
test) forbid the classifier from picking a roster entry as the identity
of a person the message leaves unnamed ("viene anche mio fratello"): a
relationship between two people is filed only when the sender states the
tie in so many words and names the other person in the turn's text.
An unnamed relative yields at most a single sender-side fact with the
identity left open — never an identification, never a reciprocal write
onto another user's page.

`known_users` is the identity-context sibling of `sender_groups`:
together they give the classifier the full picture it needs to route
`owner_id` to a group or to a third party. Their arrival closes the
injection half of the dogfood **F-A** finding (the classifier was blind
to the group domain *and* to who else exists); the judgement-quality
half is closed by pointing the slot at a strong model (below).

## Locale plumbing

The `ingest` system prompt carries a `{locale}` placeholder in its
`LANGUAGE` section. The orchestrator resolves the locale in this
order and substitutes the directive before the prompt reaches the
model:

1. `IngestRequest.metadata.locale` — supplied by the MCP consumer
   (the bot orchestrator forwards a BCP-47 tag like `it-IT` when it
   has one, e.g. derived from the chat platform's user profile).
2. `enrollment_users.locale` — the per-user default the admin set
   via the dashboard (migration `0020`, lookup
   [`mwe_core::enrollment::locale_for`](../../crates/mwe-core/src/enrollment.rs)).
3. None — both above came up empty. The renderer
   [`mwe_core::locale::render_language_directive`](../../crates/mwe-core/src/locale.rs)
   emits the legacy "mirror the user's message" clause so a
   deployment that never populates any locale keeps today's
   behaviour. Unknown BCP-47 primary subtags also do *not* drop to
   the mirror — the directive cites the raw tag (`User locale:
   pt-BR. Respond in the language indicated by BCP-47 tag pt-BR`)
   instead of silently degrading.

The MCP wire shape stays additive: `metadata.locale` is one
optional string in the existing `metadata` object alongside
`disambig_choice` — agents that never set it see no change.

## Configuration: `mwe-mcp.config.yaml` + env-vars

[`mwe-core::config`](../../crates/mwe-core/src/config.rs) parses
the `llm:` section in addition to `logging:`. Five canonical slots
(`hub_writer`, `ingest`, `rem_promotions`, `rem_dedup_semantic`,
`cronista`) each carry `{backend, model, api_key_env?, base_url?}`.
Five backends are wired in
[`LlmFunctionConfig::build_backend`]: `ollama` (local, no key),
`anthropic` (`AnthropicBackend`, key from `api_key_env`), `gemini`
(`GeminiBackend`, hits `generativelanguage.googleapis.com`, key from
`api_key_env`), `openai` (`OpenAiBackend`, Chat Completions) and
`openrouter` (the OpenAI-compatible aggregator). Any other backend string
raises `ConfigError::UnsupportedLlmBackend`. So pointing a single slot — say
`ingest` — at a hosted model is a config change, not new code: set
`backend: gemini`, a `model`, and `api_key_env` (the key itself lives
in `<workdir>/mwe-mcp.env`, loaded via `dotenvy`). This is how the
finer ingest judgments the 9B workhorse cannot make reliably get routed
to a stronger model while the local model keeps the cases it handles
well.

The classifier **targets** a strong model for the *judgement* calls: the
structural-vs-capture boundary, the public→`global` cue, and the
group-scope and cross-user `owner_id` routing. This is a **config-profile
choice on the existing `LlmFunction::Ingest` slot** — point `ingest` at a
strong backend (e.g. Gemini) and the whole classifier moves; there is no
new plumbing. The dogfood findings confirm the need: the 9B workhorse
under-triggers **F-A** (group-scope / cross-user attribution), **F-F**
(public-fact → `global`), and **F-H** (recognising structural intent).
The strong model closes the judgement quality on all three. F-A and F-F
land entirely in this prompt; F-H's *recognition* lives here too, but the
emergent-structure **action** (actually forging or reshaping a container)
is the planner's job — the classifier only routes the turn to
`structural`.

The **multi-fact split** is a separate matter, and crucially it is **not**
a model-capability gap — it is a function of the **prompt shape**.
`extractions[]` is the **sole** fact container and "extract every atomic
fact" is the lead instruction, which is what makes the model split a
multi-fact turn into one extraction per atomic fact. The
[`GeminiBackend`](../../crates/mwe-core/src/llm.rs) forces
Gemini's mandated `temperature: 1.0` and `maxOutputTokens: 65536` with
`thinkingLevel: minimal` (sub-1.0 temperature loops/degrades Gemini 3), so the
call-site `with_temperature(0.1)` / `with_max_tokens(4096)` bind only on
the Ollama/Anthropic path; the token cap is sized so a
multi-fact array is not clipped there. See
[llm-functions.md](llm-functions.md).

One operational footgun is worth internalising: the loader lets an
operator override at `<workdir>/prompts/ingest.md` **win** over the bundled
default, so a *stale* override silently shadows the shipped prompt. A dogfood
memory reset must therefore also clear `<workdir>/prompts/` — see
[build-run.md](../development/build-run.md).

Env-var overrides follow the
[config-schema.md](../protocol/config-schema.md) convention
`MWE_LLM_<FUNC>_<KEY>` where `<KEY>` is `MODEL`, `BACKEND`,
`API_KEY_ENV`, or `BASE_URL`. The override layer is injected as a
closure so the unsafe-free crate can test it without mutating the
process environment:

```rust
let n = llm.apply_env_overrides(|k| std::env::var(k).ok());
```

An override that names a function not present in YAML **creates** the
slot — convenient for "I want to flip just the model without editing
the config file".

## Policy knobs

[`IngestPolicy`] groups the operator-tunable defaults so the orchestrator's
signature stays stable as the policy grows:

| Knob | Default | Why |
|---|---|---|
| `recall_top_k` | 5 | Enough context for the LLM, small enough not to blow the prompt budget. |
| `dedup_threshold` | `recall::DEFAULT_DEDUP_THRESHOLD` (0.85) | Mirrors capture's default — a turn that paraphrases an existing fact should be deduped. |
| `max_recent_messages` | 16 | The "keepTurns×2" sliding window — wide enough for coreference and the classifier's multi-fact split. The consumer owns the transcript and supplies the window via `IngestRequest.recent_messages`; this caps how much of it the prompt carries. |
| `max_recent_message_chars` | 280 | One tweet-length per turn keeps the prompt compact. |
| `max_list_pages_in_prompt` | 32 | Lists are few by nature; this bounds a pathological corpus, not an ordinary one. Past the cap a turn adding to a dropped list proposes a fresh name instead. |
| `max_groups_in_prompt` | 8 | Cap on the `sender_groups` entries injected for group-scope routing; a sender in more groups gets the first 8 (alphabetical). |
| `max_group_scope_chars` | 1000 | Per-group `scope` truncation — large enough to keep the scope's exclusion clause, bounded so a pathological scope can't blow the prompt budget. |
| `max_users_in_prompt` | 24 | Cap on the `known_users` roster injected for cross-user attribution; a deployment with more enrolled users gets the first 24 (alphabetical by id). |
| `default_page` | `notes.md` | The **buffer page** — where a fact lands when no page fits. Never the wiki's `index.md`: that page is the wiki's map (*where does a fact belong*), it holds no facts, and the read path never opens it. REM's reorg sweep drains `notes.md` onto real pages. |
| `fallback_suggested_seed` | `"I've noted that."` | English placeholder; operator-overridable per deployment. |
| `structural_suggested_seed` | `"This looks like a structural change — open the dashboard to continue."` | Same. |
| `nav` | `recall_nav::NavigatorPolicy::default()` | The navigator funnel's resource knobs (hops, pages/hop, char budget, candidate cap) — see [recall-pipeline.md](recall-pipeline.md). Inert when no navigator backend is wired. |
| `due_soon_top_k` | 3 | Size of the `UPCOMING` (due-soon) slot; `0` disables it. |
| `due_soon_horizon_hours` | 168 (7 days) | Look-ahead window of the due-soon pull, hours from the turn's clock. |
| `max_agent_identity_chars` | 900 | Budget of the `WHO YOU ARE` section (whole-bullet fitting; a resource cap, not a semantic gate). |
| `max_agent_history_chars` | 1400 | Budget of the `YOUR RECENT HISTORY WITH THIS USER` section (same fitting, newest first). |
| `max_sender_identity_chars` | 2500 | Failsafe on the `WHO IS SPEAKING` identity card (whole-**paragraph** fitting; warns when it fires). `0` serves the label line alone. Also the per-card budget of `PEOPLE THIS TURN NAMES`. |
| `max_mentioned_cards` | 2 | How many **named third parties** get their card served (`PEOPLE THIS TURN NAMES`). `0` disables the slot. |

The recall-block rows (`recall_top_k`, `recall_fresh_top_k`, `nav`,
`due_soon_*`, `max_agent_*_chars`, `max_sender_identity_chars`) are
operator-overridable per deployment through the
[`recall:` config section](../protocol/config-schema.md#recall) — both
production call sites build the policy via
`RecallConfig::resolved_ingest_policy()` from a shared hot-reloadable
handle, edited from the dashboard recall-settings page
(`/dashboard/admin/recall-settings`). The classifier prompt-budget knobs
(recent messages, wiki/group/user caps) keep their Rust defaults.

The canned seeds are intentionally bland — the agent rewrites them in
the user's language and tone before sending the reply.

## Why no `recent_messages` weighting yet

[`wiki_recall`] accepts a `recent_messages` slice but ignores it for
scoring (documented in [`recall-pipeline.md`](recall-pipeline.md)).
The ingest pipeline follows the same line: the recent messages reach
the LLM as **prompt context** (so coreference works), but
`wiki_recall`'s cosine
score still computes against the current message alone. The
weighting belongs in the LLM-side context cache, not the vector
ranker, because the LLM owns the conversational state.

The same principle decides where the raw transcript lives. **The
consumer owns the transcript**: it supplies the sliding window via
`IngestRequest.recent_messages` (the prompt cap is
`max_recent_messages = 16`, the "keepTurns×2" window). mwe-mcp keeps
**no unbounded** server-side raw-message archive — the architecture
diagram's "archive" stage is the consumer's responsibility. The one
carve-out (43-P, founder-confirmed 2026-07-15) is the **cross-consumer
recent window** below: a capped, TTL'd serving buffer, not an archive.

## The cross-consumer recent window (group 43)

> **The contract this restates** *(founder-confirmed 2026-07-15)*. The rule
> used to read "the window stays local — the server keeps no transcript".
> The intent survives, the wording does not:
>
> > The server keeps no **unbounded** transcript. It retains, per user, the
> > same bounded, short-lived recent window a consumer itself would keep —
> > because serving it back is what makes memory multi-surface.
>
> The TTL semantics are the load-bearing half. Serving yesterday's last
> exchange as if just said makes the agent resume a dead thread, which is
> why every rendered entry carries its relative age. And the window is
> served to **every** consumer, smart ones included, with no per-consumer
> switch: the section is cheap, a capable model distinguishes reference
> context on its own, and the isolation that matters is the per-USER key
> (windows never cross users — enforced in the fetch, pinned by test), not
> a per-consumer gate. `recent_window_entries: 0` is the deployment-wide
> off switch. On GDPR: bounded retention plus the user-deletion cascade
> plus never being indexed makes this defensible as transient processing,
> and the buffer is included in the user export for completeness.


The recall block carries facts; the live **thread of discourse** used to
live only in each consumer's local window — say a thing to the voice
assistant, continue on Telegram, and the conversation didn't follow. Since
the server already receives every turn (the user's ingest plus the
group-27 assistant pass), `mwe_core::recent_window` retains a bounded
rolling buffer per user (`recent_exchanges`, migration 0060: hard cap
`recent_window_entries` = 32 AND TTL `recent_window_ttl_hours` = 4, both
enforced in the write path; never indexed, never embedded, never
REM-processed; deleted with the user) and every ingest response serves it
back as the self-labelled **`recent_window`** field — `RECENT EXCHANGES ON
YOUR OTHER CHANNELS WITH THIS USER (reference — the thread may have moved
on; do not re-answer these):`, entries tagged `[<relative age> · via
<consumer>/<channel>] <speaker>: <text>`, oldest first, newest winning the
`recent_window_chars` (1200) budget. The TTL is short **by design**: the
window serves the thread, not history — a thread is live on the scale of
minutes to hours, and anything older has either sedimented into facts
through the ordinary ingest or expired with the conversation it belonged
to. Self-echo exclusion keeps the section purely additive: the requester
declares its surface via the optional `metadata.channel` label and gets
every surface but its own — (consumer, channel) when the label is present,
the whole consumer when it is not. Guests get nothing and contribute
nothing; the degraded/fallback paths serve `None`.

**Fresh-session resume (43j).** The self-echo exclusion applies only to a
requester that *brought its own local window*: the turn's `recent_messages`
argument is the signal. A turn that carries none has no context a served
thread could duplicate — a reborn/blank session (hermes idle-expiry,
upstream hermes-agent#43008) or a consumer that keeps no window at all —
so it is served every surface **including its own**: its own channel's
tail is exactly the thread the user is continuing. The fetch runs before
the turn's own buffer write, so a requester served its own surface is
never handed the message it is speaking right now. Net effect: a consumer
on this contract never wakes up amnesiac — the window IS the session
resume, no host-side support needed. (Known soft edge: a consumer whose
local window is emptier-lived than its host transcript — hermes after a
gateway restart — gets one turn of mild reference-tagged duplication,
self-healing on the next turn.)

## Test coverage

- **Pure helpers**: 8 tests (intent parser canonical + default, plan
  parser pure JSON / prose-wrapped / nested-braces / garbage / unterminated,
  capture-plan validation: missing target / default owner / bad principal).
- **Prompt building** (the `build_prompt_*` tests in the module): wiki
  list + current message rendered, long recent message truncated with
  `…` sentinel, oldest messages dropped at policy cap, `sender_groups` +
  scope rendered with the no-scope placeholder, `(none)` when the sender
  has no groups, group count capped at `max_groups_in_prompt`, recall
  `fact_id` emitted, and the `known_users` block — rendered with
  aliases and the `(none)` placeholder when the roster is empty.
- **Snippet formatting**: 1 test (`(wiki_id) text` join).
- **Wire-shape sanity**: 4 tests (intent / context-hint / message-role
  wire strings + policy default uses recall dedup threshold).
- **End-to-end orchestrator** (the `ingest_*` tests):
  - `ingest_rejects_empty_text` → `IngestError::EmptyText`.
  - `ingest_skip_intent_returns_seed_no_write` → no fact_index write.
  - `ingest_capture_intent_writes_fact_index_row` → row visible in
    `fact_index::find_by_id` post-call.
  - `ingest_recall_intent_surfaces_snippet_no_write` → snippet returned,
    no new fact written.
  - `ingest_structural_intent_returns_dashboard_seed` → canned seed
    when LLM omits one.
  - `ingest_invalid_capture_plan_demotes_to_skip` → missing
    `target_wiki_id` demotes to skip + fallback seed, no write.
  - `ingest_unparseable_llm_response_demotes_to_skip` → garbage LLM
    output ⇒ skip + `llm_used=true`.
  - `ingest_llm_unavailable_returns_canned_skip` → transport error ⇒
    skip + `llm_used=false`.
  - `ingest_dashboard_command_uses_structural_seed_on_fallback` →
    `ContextHint::DashboardCommand` picks the structural seed.
  - `ingest_finish_reason_does_not_change_parse_outcome` → JSON parse
    is the only gate, finish reason is informational.
  - **Array contract** — `ingest_multi_fact_extractions_each_buffered`
    (a multi-fact turn files every extraction; standard wiki ⇒ each buffered),
    `ingest_single_atomic_fact_files_one_via_array` (an atomic message
    files exactly one fact via a one-element array), and
    `ingest_capture_with_empty_extractions_demotes_to_skip` (capture intent
    with an empty array has nothing to file ⇒ skip).
  - **Recall-block tail** — `assemble_recall_block_*` (section join order,
    all-empty → `None`), `nav_seeds_*` (topic union + owner parse),
    `ingest_recall_turn_appends_navigated_memory_section` (scripted
    navigator opens a principal-seeded page; the section lands after the
    flat snippet), `ingest_skip_turn_never_consults_the_navigator` (the
    intent gate, proven with a panicking navigator double), and
    `ingest_surfaces_due_soon_slot_even_on_skip_turns` (time-driven pull
    fires on a skip turn, renders `valid_to`, honours the `0` off switch).
- **YAML setup sanity**: 1 test (the test fixture's `_meta.md`
  round-trips through `WikiMeta::parse`).

Plus the `mwe-core::config` additions: 8 tests covering env-prefix
naming, hybrid-profile YAML, slot lookup, env overrides (creates slot
/ replaces existing / no-op), and `build_backend` (accepts ollama with
custom base URL, rejects anthropic with `UnsupportedLlmBackend`).

Total workspace: the full suite is green (`cargo test --workspace`).

## What is intentionally out of scope

| Not yet supported | Why |
|---|---|
| **`structure_proposal` from the `Structural` intent** | The explicit-request structural path (`"make me a notebook for X"`) only nudges to the dashboard — it migrates to the dashboard chat per the forward note. |
| **Disambiguation follow-up** (`metadata.disambig_choice`) | The MCP dispatcher round-trip needs to be live before a second call with a chosen candidate makes sense. |
| **Audit log row** in `tool_log_search` | Waits on the `tool_executions` audit table. |
| **Cloud LLM backends beyond Anthropic / Gemini** (e.g. OpenAI) | `UnsupportedLlmBackend` keeps the operator's mistake visible at startup instead of crashing mid-turn. |
| **`recent_messages` weighting in recall scoring** | LLM-side context-cache work, not vector-ranker work — same rationale as `wiki_recall`'s. |
| **Cross-user attribution enforcement** | The [identity-and-acl.md](../concepts/identity-and-acl.md) invariant tightens when the trusted-internal-LLM-only writer surface goes away. |
