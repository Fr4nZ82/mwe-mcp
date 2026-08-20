---
title: Narrative compiler — planner + the Cronista & the Record Writer
area: design-notes
status: partial
last_review: "2026-08-06"
---

# Narrative compiler

[`mwe-core::planner`](../../crates/mwe-core/src/planner.rs) is the
**topology stage** of the narrative compiler. It turns the flat
fact store — [`fact_index`](capture-and-dedup.md), fed by the
[light dream](narrative-buffer.md#promotion--the-light-dream) — into a
[`CompilationPlan`](../../crates/mwe-core/src/planner.rs): a page graph in
which every fact lives on **exactly one** page (the
one-fact-one-page invariant), and a
persistent [`ConceptRegistry`](../../crates/mwe-core/src/planner.rs) stops
the same concept page being re-invented run-to-run. The plan is the input
the **compiler** ([`mwe-core::compiler`](../../crates/mwe-core/src/compiler.rs))
turns into prose, page by page; the planner never writes prose itself.

This page is the **anchor** for the narrative compiler. The planner and
the prose compiler are built; the deterministic post-compile reviewer,
the REM cadence wiring, and human-edit reconciliation extend
it as they land. For where the compiler sits in the
broader picture — the buffer write path and the deterministic
promotion that feed it — see
[narrative-buffer.md](narrative-buffer.md); the conceptual rationale (why
mwe-mcp is the author, why prose is the accuracy mechanism and not
cosmetics) is the narrative-compiler milestone in the
roadmap.

## Where the planner sits in the pipeline

The pipeline has this shape:

```text
message → archive → classifier → BUFFER(captures) → facts → PLAN(topology) → wiki(.md compiled)
```

The [light dream](narrative-buffer.md#promotion--the-light-dream)
promotes buffered captures into recallable `fact_index` rows — cheaply,
deterministically, frequently. The planner runs **after** that, on
the slower nightly cadence: it reads the active facts across the narrative
wikis and decides the *shape* of the published memory — which pages exist,
which fact homes on which page, how pages link. It stops short of writing
the pages; the Cronista consumes the plan and renders prose. The
split keeps the expensive structural-judgment LLM calls and the prose-
generation LLM calls in separate, independently-tunable stages, and lets
the [incremental dirty set](#page_fingerprint--the-dirty-set) bound how
much prose the Cronista has to regenerate each night.

## The five stages

[`build_wiki_plan`](../../crates/mwe-core/src/planner.rs) is the
orchestrator; it runs four substantive stages plus the incremental
bookkeeping. The crate doc-comment on `planner.rs` is the SSOT for the
roster and order; the sketch below mirrors it.

```text
0. Fonditore       build_foundation_pages   deterministic, no LLM   identity wikis → person / group_theme pages
1. Cartografo      classify_facts           STRONG LLM, batched     one-fact-one-page assignment + proposed concept pages
1.5 Conciliatore   conciliate_new_pages     STRONG LLM, one call/wiki  fold duplicate proposed pages (redirects)
2. Architetto      build_compilation_plan   deterministic           materialise + GC + link graph + order
—. Incremental     build_wiki_plan          deterministic           carry-over + new-only classify + dirty set + persist
```

### Stage 0 — the Fonditore (deterministic foundation)

[`build_foundation_pages`](../../crates/mwe-core/src/planner.rs) seeds the
graph from three deterministic sources — no LLM, no facts. From
[`enrollment`](enrollment-loader.md): for each enrolled group it mints one
`group_theme` card (slug = the group id, scope prose carried on
`owner_scope`); for each enrolled user one `person` card (slug = the user
id), wiring `parent_hub` to the user's **first** known group and
`outgoing_links` to all of them. Groups are built first so a person can
link to them. From the **tree** (`seed_parking_pages`): **every** standard
wiki — identity wikis included — gets a `parking_page` node on its
`@notes.md` (slug = `<wiki slug>__notes`, `parent_hub` = its own card when
it has one else the parent wiki's foundation slug, description = the
`_meta` `scope` prose). Smart wikis never qualify: their consumer is the
sole writer.

**Two foundation pages per wiki.** The two
nodes are not interchangeable, because two different things arrive at the
[orphan fallback](#stage-2--the-architetto-deterministic-assembly):

| node | file | receives |
|---|---|---|
| `person` / `group_theme` — the **card** | `@profile.md` | the always-on identity core an ingest `salience: "high"` reserves |
| `parking_page` — the **parking page** | `@notes.md` | every other fact with no page yet; REM's reorg drains it onto real pages |

A topic wiki gets only the buffer: its subject is a topic — a person, a
pet, a project — never a user (maintainer 2026-07-05), so there is no
identity to card. Sending both kinds to one page is not a smaller version
of this design: it either buries the card under unsorted facts or promotes
every unplaced fact to identity.

**The card compiles under its own brief, and under a budget.** A card is
the one page the read path serves **whole, on every turn**
(`WHO IS SPEAKING`), so unlike any other page its length is paid for again
and again. The Cronista is told which kind of page it is writing —
`page_kind` on the PAGE line, `identity_card` only for a `person` /
`group_theme` node sitting on its reserved `@profile.md` — and the brief
tells it what a card is for (who this actor is: biography, health,
preferences, standing or period events) and to land under **~1800
characters**, never past **2500**.

The completeness rule is **not** relaxed by the budget: every fact still
gets its `<fN>` tag, and the model is told explicitly to let the page run
long rather than drop or merge a fact to fit. So an over-budget card is a
signal that material needs moving **off** it, not a licence to lose any —
and that decision belongs to REM, not to the writer. The compiler reports
it (`CompileReport::cards_over_budget`, a `warn!` with the numbers, and a
line on `mwe-mcp dream`) because the alternative is finding out at serve
time, every turn, from a truncation that drops whatever sorted last.
[`IDENTITY_CARD_CEILING_CHARS`](../../crates/mwe-core/src/compiler.rs) is
deliberately the same number as `IngestPolicy::max_sender_identity_chars`,
pinned by a test: a card written inside its authored bound must never be
cut when it is served.

**And it is measured on the same quantity, not on the file** (`served_chars`).
The YAML testata, the `{{f=…}}` marker pair wrapped around *every* fact
(~47 characters each) and full `[[wiki/page|alias]]` link syntax are all
machinery the read path resolves away before the prose reaches a turn. Counted
in, they made a card comfortably inside its authored budget report as over it
on every compile — and a warning that fires routinely is one nobody reads on
the day it is true. What is measured is the upper bound of what any reader is
served: per-reader redaction only removes more.

Foundation pages are **never garbage-collected**
([`PagePlan::is_foundation`](../../crates/mwe-core/src/planner.rs)) — an
enrolled user always has a card even with zero facts, and a buffer
survives its facts being drained onto sub-pages, which is what is supposed
to happen to everything that lands there. A slug collision is skipped with
a warning (enrollment wins over a topic wiki; the group wins over a
person), so the graph never has two pages on one slug — and the parking page's
`__` separator is **unreachable by `slugify`**, so no classifier-proposed
page name can ever claim a buffer's key. The same property is why the two
places that canonicalise a proposed slug (`build_compilation_plan` step 4
and `rehome_facts_in_persisted_plan`) leave a key the plan already holds
alone: re-slugifying `alice__notes` would mint a phantom `alice_notes`
leaf beside the real buffer and split the fact off from it.

### Stage 1 — the Cartografo (strong-model classification)

[`classify_facts`](../../crates/mwe-core/src/planner.rs) is the first LLM
stage. It runs on a **strong** model — the structural-judgment tier, a
config slot distinct from the 9B workhorse; see
[the strong-model tier](#the-strong-model-tier) below — in batches
(`CARTOGRAFO_BATCH` facts per call), **grouped by source wiki before
they are chunked** (`cartografo_batches`), so a batch never straddles two
wikis. That order is what lets one language directive be true for the
whole batch: this stage coins page titles and descriptions a person
reads. For each fact it returns the **one**
page slug the fact belongs on, and it may propose emergent `concept_leaf` pages
when a theme warrants its own page.

**The two tiers do not have the same licence, and the prompt says which one is
reading it** (the `{birth_floor}` block, `planner::birth_floor_directive`).

- **Hourly (cheap tier).** *Park rather than guess.* A fact is assigned only to
  a page that is a **strong** match; when none is, the fact is **omitted** and
  the orphan pass parks it on the wiki's parking page. And a new page may be
  proposed only when it groups at least
  [`PAGE_BIRTH_FLOOR`](../../crates/mwe-core/src/planner.rs) = **5** facts on
  one theme — enforced in code by `hold_to_birth_floor`, which drops an
  under-mass proposal and unassigns its facts, so the model's word is not the
  last one. Founder, 2026-08-18: *«il modello economico dovrebbe evitare di
  creare pagine e parcheggiare in `@notes.md` i fatti di cui non è molto
  sicuro»*.
- **Nightly (strong tier).** No floor: it is shown the whole wiki and its
  judgement is the point.

The floor works **only together with the re-offer**: at every light build each
wiki's parking page re-enters the to-place pool (`build_wiki_plan`'s `reopen`
set), so a parked fact is judged again next hour and a pile that has grown can
finally earn its page. Carried over instead, it would sit until the nightly pass
read it and the floor could never be reached. Neither half does anything alone.

Why a floor at all: a page born from one fact takes its card from that fact, and
the card is the only thing a reader is shown before deciding whether to open the
page — so a one-fact page is a page nobody can find on purpose. Its sibling one
level up is `RemPolicy::auto_promote_group_min_pages` (default 9): how many
pages must group before a **wiki** is born. The prompt
([`crates/mwe-core/prompts/cartografo.md`](../../crates/mwe-core/prompts/cartografo.md))
is handed the foundation pages and the existing concept pages (from
the registry plus any proposed earlier this run) so the model **reuses**
an existing page rather than minting a duplicate.

**The batch is one wiki; the page list is the whole forest.** A fact is free to
live in any wiki, and the engine putting one where the prose reads better is
its judgment, not damage (founder, 2026-08-10 — the compiler upholds exactly
that with `move_to_wiki`, and REM has a whole cross-wiki refile sweep). Read
permission is judged per fact on `subject ∪ allow ∪ sender`, never on the
container, so a placement exposes nothing and hides nothing. **The page list is
therefore the entire mechanism**: a page the model is not shown is a page a
fact can never reach, and a list scoped to the batch's own wiki meant a fact
could never be re-homed once it landed.

- `describe_foundation` — the batch's wiki's foundation pages, then **every
  other wiki's identity card**. Not foreign buffers: a buffer is where a fact
  of *that* wiki waits for a home, and parking a fact in somebody else's inbox
  is not a placement. Cards are capped by the product limits (24 users, 8
  groups), so this half never grows with the memory.
- `describe_concepts` — the batch's own wiki's concept pages first and never
  cut, then the rest of the forest's. Each foreign line carries `wiki: <id>`,
  because choosing a page is choosing a place and a slug alone does not say
  which.

The scoping this replaced was justified as a correctness property — a fact's
wiki is settled at capture by
[`derive_target_wiki`](../../crates/mwe-core/src/ingest.rs), so the structure
that should receive it is the one it is in. Capture decides where a fact
*starts*. The half that stayed true is that the identity-page discipline
governs which card may hold a fact, and that is enforced per fact by the
`identity_pages=` tag, not by the page list — fencing the list did not uphold
the discipline, it made the discipline's own instruction (*«home it on the
subject's own pages instead»*) impossible to follow, because the subject's card
lives in the subject's wiki.

**What one wiki per batch still buys is the language.** The pages a batch
*coins* are homed by `resolve_page_wiki` in its own facts' wiki, so `{locale}`
is the right answer for every title and description the call writes; a page it
merely *chooses* was titled by whoever coined it, in that wiki's language, and
this stage does not rewrite it. Hence the rule that closes the loop: **a fact
may be assigned to any page in the forest, and a new page is born in the
batch's own wiki** — `vet_proposal` still requires `parent_hub` to be a
foundation page of this wiki, no longer as a fence but because that is where
the proposed page will live.

**The ceiling.** The described list is complete while the memory fits one call
and cut when it does not (`FOREST_PAGE_CEILING`, 400 pages — the twin of the
Cronista's `CARD_INDEX_CACHE_CEILING_PAGES`). Below it the list is *identical
for every batch of the run*, so it rides the prompt's cached prefix and
completeness is also the cheap answer. Above it the batch's own wiki stays
whole and the rest of the forest is cut to the `FOREIGN_SELECTION_PAGES` (40)
nearest by page-card similarity, **nearest first and never re-sorted by slug**
— where a list is cut the order IS the selection. Ranking uses each foreign
page's *best* similarity to any of the asking wiki's own cards rather than to
their average: a user's wiki spans unrelated subjects and a centroid over them
is a point about none of them. The planner has no embedder and does not grow
one; cards are embedded by the reindex pipeline, so a page whose card never
embedded does not rank — smaller offer, never a wrong one
(`planner::foreign_page_offers`).

**`{taken_slugs}` keeps one of its two jobs** — bare page names, no titles, no
descriptions ([`describe_taken_slugs`](../../crates/mwe-core/src/planner.rs)).
It is no longer *«the other wikis' pages, which you may neither read nor file
into»*: those are described above now and choosing one is legitimate. What
survives is the collision half, and it survives intact. A plan is keyed by slug
across the whole forest (`CompilationPlan::pages`), so a name is unique
memory-wide, and a batch that **coined** a slug another wiki owns would have
its facts filed onto that page by step 4 of the Architetto — a destination
nobody chose. *Choosing* a page is a judgement; colliding with its name is an
accident. So the list holds exactly what the described lists leave out: the
foreign buffers, and past the ceiling the foreign pages the selection dropped.
It is **never truncated** and therefore carries no ordering rule: a collision
guard with a gap answers "free" for a taken name.

The engine enriches that context with **structural signals**
([`CartografoSignals`](../../crates/mwe-core/src/planner.rs)) — information
the model weighs; no identity-scope or count gate exists in Rust:

- **Identity-page scope, per fact.** Every fact line carries an
  `identity_pages=` tag: the `person` pages the fact's *subject* covers —
  the subject user's own page; for a group subject the member users' pages,
  expanded from enrollment by
  [`subject_scopes_for`](../../crates/mwe-core/src/planner.rs)
  (`enrollment::members_for`); `any` for the builtin global group (world
  context is never a foreign subject); `none` for a group with no enrolled
  members. The prompt's **identity-page discipline** reads the tag: an
  identity card (a `person` page — a `wiki-user`'s `@profile.md`, the agent
  wiki included) carries **one subject** and never takes a
  **foreign-subject** fact (subject = a different user, or a group the page's
  user is not a member of — a group they belong to is their own shared
  context, never foreign). The foreign detail is homed on the subject's own
  pages, split by content; the relation surfaces on the identity card only
  through the page-user's **own** facts (a coordinating own-fact is
  preferred when one exists) plus a `[[wikilink]]` to the subject's home —
  a bare link line is acceptable when no own-fact exists.
- **Fact mass, per page.** Every page line carries `facts: N` — the
  carried-over count entering this build plus the run's own assignments so
  far, so a later batch sees the pile grow. The prompt's **split-by-mass
  lever** reads the numbers: when the most pertinent page has grown past
  what still renders reliably as one page, the model splits the theme **by
  content** into multiple concept leaves (the seams are its judgment —
  sub-topic, period, aspect); splitting a grown page is normal maintenance,
  not an error. Mass is the signal; the split verdict is the model's.
There is **no container signal**, and its absence is the design. A `children: N`
count and a matching *container rule* lived here until 2026-08-10, six days
after the ruling that **a container is a wiki** and stopped anything
minting a container page. What survived the ruling was the half that told the
model how to *treat* one — including, verbatim, "a page whose facts are being
re-homed so it can settle into its real hub role", the deleted mechanism
described in full. Worse, the count could only ever fire on an error: it
back-referenced `parent_hub` across **registry** entries, foundation pages are
not registry entries, so a correctly-parented leaf contributed nothing and the
number appeared only when a leaf had been parented under another leaf — which
the prompt forbids two paragraphs below. The rule then told the model to treat
that malformed page as a legitimate hub and keep filling it.

`planner::vet_proposal` now enforces what the prompt states instead of
describing the breakage: every proposal is filed as a `concept_leaf`, and a
`parent_hub` that is not a foundation page **of the batch's wiki** is dropped
(the page is kept — its facts still need a home, and `resolve_page_wiki` then
homes it where its facts are rather than following an invented parent into a
foreign wiki). A negative test asserts the prompt no longer carries either the
section or the signal.

The *«of the batch's wiki»* half survived the un-fencing above, with a
different reason behind it. It is not a rule about where a fact may live — a
fact may be assigned to any page in the forest. It is what proposing a page
means: a new page is born where its facts are, this batch's facts are this
wiki's, so a parent in another wiki would be one the page does not live
under. `vet_accepted` asks only that the parent exist, because by the time the
Conciliatore runs a proposal may have been merged into a page already homed
elsewhere.

`build_wiki_plan` computes both signals (mass from the previous plan's
carried-over placements; scopes only when the Cartografo actually runs),
and the post-compile reviewer closes the loop with the
[`cross_subject_bloat` finding](#the-reviewer) — observability, never
refusal.

The stage is **resilient by design**: a batch whose LLM call errors or
whose JSON does not parse is *skipped softly* (logged, `continue`) — its
facts simply fall through to the Architetto's deterministic
[orphan fallback](#stage-2--the-architetto-deterministic-assembly). One
flaky batch never aborts the cycle. New-page slugs are slugified and
de-duplicated across batches as they accumulate, so two batches proposing
the same slug collapse to one.

**Cadence — both cadences place, and the difference is what they are allowed
to touch.** `build_wiki_plan` selects the path through the
[`NewFactPlacement`](../../crates/mwe-core/src/planner.rs) enum, and
[`dream::placement_for`](../../crates/mwe-core/src/dream.rs) is the single site
that maps a cadence onto it — factored out so the policy is pinned by a unit
test rather than buried in `run_compile`.

**Light (`NamedThenCartografo`) — the page the user named, then the model.**
Two halves, in this order, and the order is the design:

1. **What the user named is settled deterministically, with no LLM**, onto
   `fact_index.target_page` by
   [`ingest_placement_blueprint`](../../crates/mwe-core/src/planner.rs) — a
   `concept_leaf` per distinct target slug (a path like `recipes/dinner.md`
   flattens to one leaf; the light path does not nest), seeded with the
   proposed `style` + `page_description` so the page gets a testata — a
   **seed**, not the page's permanent card, see
   [the card heal](#the-card-heal--a-page-is-described-by-what-was-written-on-it).
   Since prompt v2.59 that input is exactly the two cases where the write
   could not wait: a `lista`, or a container the user asked for by name
   ([ingest-pipeline.md](ingest-pipeline.md)). **Those facts never reach the
   model.** A `lista` is a *set* — half a shopping list is a wrong answer, not
   a partial one — and the Cartografo is shown neither the style nor the
   proposed page ([`describe_facts`](../../crates/mwe-core/src/planner.rs)), so
   offering it a list item is how that item leaves the list it was added to,
   an hour after the user watched it land there.
2. **Everything the classifier left unplaced goes to the Cartografo on the
   cheap ingest tier.** That is every prose fact, and it is the reason this
   stage runs hourly at all: without it a prose fact has no page of its own,
   takes the orphan fallback to its wiki's parking page (`@notes.md`), and is then
   *carried over* by every later build — so the strong nightly Cartografo
   never sees it as new either, and the buffer only drains when REM's split or
   the reviewer's oversize nomination reaches it.

With no ingest slot wired there is no cheap tier to run half 2 on, and the
light pass degrades to half 1 alone (`NewFactPlacement::Ingest`), which is the
pre-2026-08-09 behaviour. The chosen placement is on the compile log
(`placement.label()`), so the degradation is visible rather than silent.

**Full (`Cartografo`) — the strong slot over everything, and the re-open park
is its alone.** The nightly pass re-runs the strong-model Cartografo to re-home
and reorganise. It is also the **only** placement that consumes the re-open
park, where the reviewer and the compile-failure ledger nominate *carried*
placements for a second judgement: consuming a nomination clears it, so
whichever build consumes it is the one that answers it, and a cheap hourly
build answering it is how a considered cross-wiki move gets silently reversed
before morning (observed live 2026-07-04). The light pass runs a Cartografo and
still carries the park forward untouched — its job is placing facts that never
had a page, never re-judging one the strong model already chose. In the code
this is deliberately `matches!(placement, NewFactPlacement::Cartografo(_))` and
**not** `placement.runs_cartografo()`.

**A `high`-salience fact (`fact_index.salience`) is in neither half.**
`ingest_placement_blueprint` leaves it unassigned so the orphan fallback homes
it on the actor-wiki's **identity card** (`@profile.md`), the always-on **base
context**, and the light Cartografo's remainder excludes it with the same test.
The routing *is* the reservation: an always-on fact (identity, health/safety,
hard standing constraints) overrides whatever theme page was proposed, so it is
not a decision to put in front of a model. A fact whose target is `index.md` /
empty falls through to the same orphan fallback, never a page named "index".

`OrphanFallback` remains for a Full pass with no strong slot.

### Stage 1.5 — the Conciliatore (strong-model dedup)

[`conciliate_new_pages`](../../crates/mwe-core/src/planner.rs) is a
strong-model call **per prospective wiki** that folds
**semantically-duplicate proposed pages** into existing ones. The
Cartografo, working batch by batch, cannot see the whole proposed set at
once; the Conciliatore does — it gets the memory's **concept** pages, that
wiki's first, and every page proposed this run for that wiki, and returns a
`redirects` map (`proposed_slug → existing_slug`) plus the genuinely-new
`accepted_new` list.

A proposal's prospective wiki is the source wiki of the first fact
assigned to it (`conciliatore_groups`, the same rule `slug_source_wiki`
applies one stage later); a proposal no assignment claims rides its own
group and is homed or dropped by the plan builder as before. The split is
what gives the stage a language: it picks which title and description
survive a merge, and those are read by a person. **The group's wiki decides
the order and the cut, not the membership** — `describe_existing` leads with
that wiki's pages and then offers the rest of the forest (the same ceiling and
the same nearest-first selection the Cartografo gets). Its old scoping was
justified as *«a redirect is a merge, so folding a proposal into another
wiki's page would move this wiki's facts there»*: true, and not a reason —
moving them there is legitimate, and a duplicate does not stop being one by
sitting in another wiki. The homeless bucket (a proposal no assignment claims)
has no wiki to order by and takes the forest as it comes. The
prompt
([`crates/mwe-core/prompts/conciliatore.md`](../../crates/mwe-core/prompts/conciliatore.md))
carries a **redirect bias**: when in doubt, consolidate — fewer
well-populated pages beat many scattered ones.

**Foundation pages are never offered and never accepted as merge targets.** A
card holds who a subject is and a buffer is where a fact waits until it has a
home; neither is a topic a page can become part of. They used to be rendered
*first* in the list, the parking node wearing its wiki's own title and scope as
its description, under that same redirect bias.

**Nothing the stage returns is trusted.** `vet_accepted` and `vet_redirects`
run over both halves of its output before either reaches the plan or the
concept registry, because the model is asked to re-emit `slug` /
`parent_hub` as free-form JSON while it decides merges:

| what comes back | what happens |
|---|---|
| redirect onto a slug that is neither an existing concept page nor a page accepted this run | dropped — the plan builder's fallback would otherwise mint a blank, style-less page under that name, turning *merge into X* into *create an empty X* |
| redirect onto a foundation page, or onto itself | dropped |
| accepted page named `index` / `rules` / `projects` / `profile` / `notes` | dropped — it would compile onto the file the wiki's card or buffer already owns |
| accepted page parented under a page | the parent is dropped — *a container is a wiki* |
| `parent_hub` naming no foundation page | cleared, and `resolve_page_wiki` homes the page by its facts |

A dropped redirect loses nothing: the proposal stays its own page and the next
cycle can still merge it correctly. The `parent_hub` check is deliberately
weaker than `vet_proposal`'s, which also demands the parent belong to *this*
wiki — a fact is free to live in any wiki, so what has to hold here is only
that the parent exists.

The conciliatore's output schema carries no writing `style`, so the code
re-attaches each accepted page's ingest-proposed style from the original
proposals, matched by slugified key (`backfill_accepted_new_style` in
[`planner.rs`](../../crates/mwe-core/src/planner.rs)); when two proposals
collide on the same slugified key, the **first** proposal's style wins —
the same collapse direction page materialisation uses for duplicate slugs.

This stage is **infallible**: on any failure (prompt render, LLM error,
unparseable output) it falls back to accepting *every* proposed page with
no merges — conservative, never loses a page; a near-duplicate that slips
through is still mergeable next cycle. When the redirect map is non-empty,
the orchestrator rewrites the blueprint's assignments through it before
the Architetto runs, so facts assigned to a redirected slug land on the
survivor.

**Cadence — both, tiered.** The Conciliatore runs at **both** cadences
([`conciliatore_backend`](../../crates/mwe-core/src/dream.rs)): it is the
**placement-time prevention front of semantic page consolidation** — a page
proposed on the light path (the deterministic
[ingest-placement blueprint](#stage-1--the-cartografo-strong-model-classification))
would otherwise materialise with **no near-synonym check at all**, which is how
the dogfood corpus grew three Paris pages. Tier per cadence as everywhere else:
the full pass uses the strong `rem_dedup_semantic` slot, the light dream the
cheap ingest-tier (Flash) backend, falling back to the strong slot on a
Flash-less deployment. A page the light dream creates and *carries over* is no
longer "new" at the next REM, so the redirect bias only catches genuinely-new
proposals; consolidating pages that already materialised is the
[REM page-merge sub-job](rem-cycle.md#page-merge-sub-job-semantic-page-consolidation)'s
work (the cure front of the same consolidation).

### Stage 2 — the Architetto (deterministic assembly)

[`build_compilation_plan`](../../crates/mwe-core/src/planner.rs) takes the
foundation, the registry, the blueprint, and the conciliation verdict and
materialises the final plan deterministically, top to bottom:

1. **Seed** the foundation pages (cleared of any carried-over children /
   facts / incoming links — foundation holds no DB facts of its own).
2. **Materialise** the registry concept pages (foundation overrides the
   registry on a slug clash), then the accepted-new concept pages, each
   recorded into the updated registry. Before materialising, a **staleness
   GC** drops any registry entry whose slug a foundation page owns: such an
   entry can never materialise again (the clash skip fires every run) and
   would only linger as a stale reuse/redirect target the Conciliatore keeps
   seeing — the shape an enrolled wiki leaves behind when it takes over a
   slug an old concept leaf held.
3. **Apply assignments** under one-fact-one-page, remapping each slug
   through the redirect map. An assignment whose `fact_id` no longer exists
   (superseded since classification) is skipped; an assignment to a page
   that does not exist mints a `concept_leaf` on the fly so the fact still
   has a home — **except when the name is a reserved stem** (`rules`,
   `projects`, `profile`, `notes`). The foundation nodes are keyed by
   `plan_slug_for_page` — a wiki's card takes the wiki's own slug, its buffer
   takes `<wiki>__notes` — so a bare `notes` misses the lookup and would mint a
   *second* plan page writing `@notes.md` in that same wiki. The assignment is
   dropped instead and the fact falls to the orphan pass, which has a real page
   for it.
4. **Orphan fallback** for any fact left unassigned — see the fix below.
5. **Style heal** for style-less registry entries: the Conciliatore's
   style backfill protects only pages accepted this run, so an entry that
   was persisted with `style: None` would stay demoted to full-prose
   compilation forever (step 2 reuses it as-is). When such an entry's page
   carries a **strict majority** of non-empty per-fact `fact_index.style`
   proposals agreeing on one style (each vote normalized to the closed
   palette by [`normalize_style`](../../crates/mwe-core/src/compiler.rs)),
   the majority style is adopted onto both the registry entry (persisted
   with the updated registry) and this plan's page, with an info trace.
   Idempotent — once the entry has a style it never re-fires.
6. **Dangling-parent heal**, then **parent → child**: a `parent_hub`
   naming no plan page (the pointer an absorbed or GC'd parent leaves on
   its children) is re-pointed to the page's own wiki foundation page when
   the plan has one, else cleared — on the plan page **and** the registry
   entry, or the pointer would resurrect next build. Then every page with
   a `parent_hub` is registered as a child of that parent; child lists are
   sorted.
7. **Fixpoint garbage-collection** of empty concept pages — see the fix
   below.
8. **Link graph**: build the directed adjacency (parent→child + foundation
   outgoing), make it **symmetric** (every edge gets its inverse), sort,
   and sync `outgoing_links` / `incoming_links` back onto each page.
9. **Compilation order**: a wiki's own pages (its card, its parking page)
   first, then everything else
   ([`PagePlan::order_rank`](../../crates/mwe-core/src/planner.rs)), ties
   broken by slug.

Two of these steps deserve calling out:

- **Deterministic orphan homing** (step 4). A fact the Cartografo never
  assigned (or whose batch was skipped) is homed by
  [`orphan_target`](../../crates/mwe-core/src/planner.rs): in the **subject's**
  wiki if it has one, else in the fact's **source wiki** — its identity card
  when the fact is identity-salient, otherwise its parking page; if neither
  wiki has either, dropped from the plan with a warning —
  **never an arbitrary page**. The home is a function of the
  fact's own subject and provenance, so the same orphan lands the same place
  every run. (A concept *page* the Cartografo leaves without a resolvable
  `parent_hub` — typically a `global` fact's page — is homed by
  [`resolve_page_wiki`](../../crates/mwe-core/src/planner.rs) in **its facts'
  source wiki**, never a root: a `global` fact captured from frodo compiles into
  `wikis/frodo/…`, its subject still `global`. The facts decide the page's
  wiki, which keeps `fact_index.wiki_id` and the compiled `source_path` in
  the same wiki — mwe-mcp's tree is a **forest** of top-level wikis with no
  materialised root.)
- **Fixpoint GC** (step 7). Empty concept pages are removed
  (an ordinary page with no facts) — but in a
  **loop until no removals**, not a single pass. A single pass would leave a
  stranded parent whose only child was an empty page removed *in that same
  pass*; the fixpoint catches the cascade — remove the empty page, the parent
  becomes childless, remove the parent too. Each iteration first
  **normalises** the shape the sweep must not eat: an emptied page that
  other pages still parent under is **kept** instead of being removed —
  removal would orphan every child's `parent_hub` (the dangling-pointer
  factory step 6 heals after) —
  which is how a fact-bearing container drained by a placement re-open stops
  being a container without taking its children down with it. Foundation
  pages are exempt.
  Each removal is recorded in `merged_pages` for audit and dropped from the
  registry. The GC's **on-disk half** runs at compile time:
  [`sweep_orphan_page_files`](../../crates/mwe-core/src/compiler.rs) (tail of
  `compile_dirty_pages`) deletes a concept-page **file** the plan no longer
  references — a live-write page whose fact the Conciliatore re-routed, or a
  leaf whose facts all moved away, otherwise survives as a zombie the recall
  navigator keeps reading. Three guards: never a plan page, never a reserved
  name (`@rules.md`, `_`-prefixed), and never a file ANY
  non-tombstoned `fact_index` row still points at (the DB-first rule — a
  pending render or a superseded row's audit marker keeps the file). All
  soft; the count lands in `CompileReport.orphan_files_swept`.

## The data model

All types live in [`planner.rs`](../../crates/mwe-core/src/planner.rs);
the definitions there are the SSOT.

- **What kind of page a plan node is — nobody stores it.** It is the page's
  **file name**: `@profile.md` is the wiki's identity card, `@notes.md` its
  parking page, anything else an ordinary page. `PagePlan` answers with
  `is_identity_card()` / `is_parking_page()` / `is_foundation()`, all derived
  from `page_path`, and `prompt_kind()` renders the word the placement prompt
  needs (`person` / `parking_page` / `concept_leaf`) at the
  moment it is shown. The wiki's own pages are never garbage-collected; an
  ordinary page goes when it holds no facts.

  ⚠️ **There used to be a `PageType` enum here, and removing it on 2026-08-19
  is the point of this section.** Its five values were the page taxonomy of the
  **pre-Rust engine** — a flat `pages/<topic>.md` space with `person` /
  `group_theme` / `concept_hub` / `concept_leaf` — carried over verbatim when
  the planner was ported on 2026-05-31. The migration plan of the time said
  what should have happened instead: *«I "concept_hub/group_theme" del vecchio
  diventano nodi wiki. I "concept_leaf" diventano pagine / sub-wiki»* — they
  were meant to dissolve into the wiki tree. What the enum actually held was a
  restatement of the file name, and the code showed it: one compiler branch
  asked the type AND the name together, and the planner ran a watchdog that
  logged an error whenever the two disagreed. Both are gone with it, along with
  the retired `ConceptHub`, which nothing had minted since 2026-08-04
  (*«un contenitore è una wiki»*).

  **The only classification of a page the founder designed is
  [`style`](#the-testata--per-page-style--description--keywords)**
  (`prosa` / `prosa-tecnica` / `lista`, 2026-06-05).
- **[`FactForPage`]** — a fact materialised onto a page. It carries the
  verbatim claim text, the classifier's `fact_type`, the full ACL triple
  (`subject` / `allow` / `sender`), the `source_wiki_id`, the optional
  **validity window** (`valid_from` / `valid_to`, a read-only projection of
  the [`fact_index`](capture-and-dedup.md) columns — see
  [the validity cue](#the-validity-cue)), and — crucially —
  the **stable `fact_id`**. Keeping the stable id on the
  page record lets the Cronista emit `{{… f=<id>}}` markers, so recall and
  supersede survive a recompile (the id is the same `UUIDv7` the claim was
  promoted under — the
  [id-stability invariant](narrative-buffer.md#id-stability) carried
  through `capture → fact → page`).
- **[`PagePlan`]** — one page's record: `slug` (the plan key), `title`,
  `description`, `style`, the optional `parent_hub`, `child_leaves`,
  `primary_facts`, the symmetric
  `outgoing_links` / `incoming_links`, and the page's **tree home** —
  `wiki_id` + `page_path`. That last one is also what says which kind of page
  this is: `@profile.md` for a card, `@notes.md` for a parking page,
  `<slug>.md` for an ordinary one; **never** `index.md`, a name no plan node
  may claim.
- **[`CompilationPlan`]** — the persisted artifact: `pages` (keyed by slug
  in a `BTreeMap`, sorted for determinism), `merged_pages` (the GC/redirect
  audit), `link_graph`, `compilation_order`, `generated_at`, `fact_count`,
  `dirty_pages` (the recompile set), and the signals the next build reads
  back: `force_dirty` (pages a structural change marked for rewrite even
  though their fingerprint matches), `refile_candidates` and `reopen_pages`
  (what the reviewer nominated for a second look).
- **[`ConceptRegistry`]** / **[`ConceptRegistryEntry`]** — the persistent
  record of emergent concept pages, so a page minted one night is
  recognised (and reused) the next, rather than re-invented. The Cartografo
  is shown the registry as its dedup context; the Architetto materialises
  registry pages and prunes GC'd ones from it.

## `page_fingerprint` + the dirty set

The incremental cost-guard is
[`page_fingerprint`](../../crates/mwe-core/src/planner.rs): a per-page
hash over **content and topology** —
`factId:contentHash,…|outgoingLinks|parentHub|childLeaves`, each list
sorted. Sorting is what makes it stable across runs; the per-fact content
hash is a deterministic FNV-1a (never the randomised `std` hasher, which
would not survive a round-trip through the persisted plan). Because the
fingerprint folds in the link neighbourhood, a page goes **dirty even with
no new facts** when a link, a parent, or a child changes — the prose names
the pages it links to, so a page whose neighbourhood moved is out of date
even when its own facts did not. And because it folds in each fact's
**render content** — the
claim text *plus* the validity fields (`valid_from` / `valid_to` /
`decay_reason`), everything of a fact that reaches the rendered page
(`fact_render_key` in [`planner.rs`](../../crates/mwe-core/src/planner.rs))
— two kinds of in-place mutation flip exactly one page's fingerprint: a
**correction** (same `fact_id`, new text, the shape a dashboard comment
produces — see [human edits](#human-edits-on-compiled-pages)) and a
**validity closure** (same id and text, `valid_to`/`decay_reason` stamped
by the closure verb — without the validity fields in the hash, a closed
window would never reach the prose). The `build_wiki_plan` early-skip is
content-aware for the same reason: it does not short-circuit when a
carried-over fact's render content drifted.

[`compute_dirty_pages`](../../crates/mwe-core/src/planner.rs) is then
**changed + new + removed**: pages whose fingerprint differs from the prior
plan, pages absent from the prior plan, and pages present in the prior plan
but gone from the next (so the Cronista can delete their `.md`). On a first
build (no prior plan) every page is dirty. This is the lever that keeps the
compile cheap — and it runs every hour, not only at night: the Cronista
regenerates only the dirty set, not the whole tree.

## Incremental orchestration

[`build_wiki_plan`](../../crates/mwe-core/src/planner.rs) wires the stages
together and adds the incremental bookkeeping:

- It gathers every active fact across the **narrative** wikis (every wiki
  whose `_meta.md` smart flag is false — "narrative" = "not
  smart", read per-wiki) in
  fact-id order, builds the foundation, and loads the prior plan. It **skips
  facts on the reserved channel pages** — `@rules.md` and `@projects.md`
  (`wiki::is_channel_page`). Those belong to their own channels and are read
  keyed on that path: behaviour rules by `recall_behaviour_rules`, project
  signposts by the recall slot that opens their project
  ([smart-wikis.md](smart-wikis.md)). Re-homing one onto another page would
  silently drop it from its channel
  ([ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-by-scope-outside-fact-memory)).
- It **carries over** prior assignments for facts that still exist, and
  classifies **only the new facts** through the Cartografo — last night's
  structural verdicts are not re-litigated.
- It **skips entirely** on `0 new, 0 removed`: it re-stamps the prior
  plan's `generated_at`, clears `dirty_pages`, persists, and returns. An
  idle night costs one DB scan and one write, no LLM calls.
- It computes `dirty_pages` via `compute_dirty_pages` against the prior
  plan (or the full order on a first build) and persists both artifacts.

## Act-first moves and the plan — the re-home seam

The act-first structural verbs (the REM split's
[`apply_paragraph_to_file_direct`](proposal-apply-engine.md), the page merge,
the `pages_to_subwiki` regrouping) rewrite disk bytes and `fact_index` rows —
but the planner's **carry-over reads
the persisted plan**, not the filesystem. Without reconciliation it re-assigns
every moved fact to its old slug, and the next recompile of the old page pulls
the fact back (the prepoint repoints its row), **silently undoing the move**
and leaving zombie markers on the target page.

[`rehome_facts_in_persisted_plan`](../../crates/mwe-core/src/planner.rs) is the
seam every act-first move calls after its apply: it detaches the moved facts
from whatever plan page holds them, appends them to the destination page —
seeding the page and a registry entry when the plan does not know it yet (a
[`RehomePageSeed`](../../crates/mwe-core/src/planner.rs): the single-segment
`<slug>.md` concept-leaf form for splits/merges, or the **`page_in_wiki`
form** for a cross-wiki refile, which maps the destination page to its plan
key through `plan_slug_for_page` — a wiki's reserved pages are foundation
nodes keyed per wiki, so `@profile.md` resolves to the card's slug and
`@notes.md` to the parking page's, never to a forest-wide `profile` / `notes` key)
— and drops any husk page a merge removed (plan + registry, audited in
`merged_pages`).

**A husk that is also a destination of the same call is not a husk.** A plan
slug is the page's *stem*, so a page that crosses into another wiki **under
its own name** keeps its key and only changes address — the emergence
variants both do exactly this. Two rules follow, and together they are what
makes such a move survive: a destination the plan already holds is
**relocated** to the seed's `wiki_id` / `page_path` when the seed names one
explicitly (a `page_in_wiki`-shaped seed means «this exact file»; a bare
concept seed only proposes a slug and never moves a page), and the husk pass
**skips** any slug the move pass landed facts on. Without the first the node
went on pointing at the wiki the page had just left, and the compiler wrote
it back there; without the second the husk pass deleted the node the move
pass had just filled — and since a plan with no pages
[reads back as *no plan*](../../crates/mwe-core/src/planner.rs), a small
corpus lost its whole plan rather than one page.
Because after the edit
the carried-over fingerprint *matches* the next build, the touched slugs are
parked on the plan's **`force_dirty`** list: `build_wiki_plan` unions them into
the dirty set (on the early-skip path they *are* the dirty set) and clears the
flag, so the destination page gets woven by the Cronista exactly once. The
shared row→plan projection ([`FactForPage::from_row`](../../crates/mwe-core/src/planner.rs))
guarantees a re-homed fact fingerprints identically to a gathered one — no
permanent dirty churn.

The seam's seeded page is a **bridge only**: at the next plan build the
Fonditore's buffer pass owns the slug with a `parking_page` foundation node
(path pinned to `@notes.md`, never garbage-collected), the staleness GC
drops the seam's transitional registry entry, and the carried facts follow
the slug onto the foundation node — so a registry round-trip can never
strand a topic wiki's content on a `<slug>.md` sibling file again (the
registry stores no `page_path`; before the foundation pass, one rebuild
was enough to drift it onto a file named after the slug).

## Persistence — a rebuildable cache at `wikis/_plan/`

The plan and registry persist as **pretty JSON** under
`<workdir>/wikis/_plan/` — `compilation-plan.json` and
`concept-registry.json` — written through the same crash-safe
`atomic_write` protocol as every page
([`save_plan`] / `save_concept_registry`; read back by `load_previous_plan`
/ `load_concept_registry`, which treat a missing or corrupt file as
"no prior plan" / "empty registry").

These files are a **rebuildable cache**, not source of truth: the plan is
fully derivable from `fact_index` + `enrollment`, so deleting them and
re-running just rebuilds the plan from scratch. That keeps the storage model intact: the plan directory is derived
state, and deleting it just rebuilds the plan from scratch. The
`_plan/` directory needs **no new exclusion machinery**: it carries no
`_meta.md`, so `tree.walk` skips it as a
non-wiki directory, and [`reindex`](reindex-pipeline.md) enumerates only
`.md` files, so the `.json` artifacts are ignored.

## The confirmed mwe-mcp tree model

How the abstract page graph lands on mwe-mcp's wiki tree was confirmed with
the maintainer (2026-05-31):

- **Foundation pages are the identity wikis.** A `person` page *is* the
  user's `wiki-user` wiki; a `group_theme` *is* the group's `wiki-group`
  wiki. Either way the page is that wiki's **card** (`@profile.md`), and its
  parking page (`@notes.md`) is the other foundation node.
- **A `concept_leaf` is a `.md` page** within the relevant standard wiki. A
  routine emergent concept page is *content the Cronista writes* — a new
  `.md` inside an existing wiki — and therefore needs **no
  `structure_proposal`**. (The "page that contains pages" was retired on
  2026-08-04 — that is what a wiki is for — and the last trace of it went with
  the page-type enum on 2026-08-19.)
- **A sub-wiki emerges from a GROUP of pages, never from one page.** The
  rung above a page reuses the **existing**
  [`wiki_promote` / `pages_to_subwiki`](proposal-apply-engine.md) machinery —
  the REM auto-promote sub-job ([rem-cycle.md](rem-cycle.md)), which is
  **act-first** (apply + born-applied receipt, no pending proposal). The
  planner does not reinvent promotion.

  ⚠️ A page that has grown too big is **split into more pages**, by the
  Cartografo's [split-by-mass lever](#stage-1--the-cartografo-strong-model-classification)
  at compile time and by the paragraph pass at REM
  (`auto_promote_min_page_facts`: 8 prose / 16 technical / a `lista` never).
  A **wiki** emerges from a different signal entirely — a *set* of existing
  pages that are one subject area (`auto_promote_group_min_pages`). Two
  mechanisms, two rungs of the *forma fisica* scale, and they never compete:
  a wiki is never born holding a single page. A one-page-to-sub-wiki variant
  survived unemitted until 2026-08-20 and was deleted: nothing proposed it,
  and a wiki holding one page is the shape this rule exists to forbid.

The consequence: **the planner adds no new proposal kind.** Routine concept-page
creation is prose the Cronista emits; the only structural action is
the page-group → sub-wiki emergence, and that already exists. (The
[`resolve_page_wiki`](../../crates/mwe-core/src/planner.rs) helper homes a new
concept page in **its facts' source wiki** (a factless page falls back to its
parent's wiki), so a page always has a tree home and lives in the same wiki as
its facts. There is no root wiki — the tree is a forest of top-level wikis.)

## The strong-model tier

In the **full** cadence the Cartografo and the Conciliatore both target a
**strong** model — the structural / semantic-judgment tier, **not** the 9B
workhorse. This is a config slot (e.g. the REM-strong tier; the concrete pick
lives in the operator's `mwe-mcp.config.yaml` per the
[admin LLM config](admin-llm-config.md) and the
[REM LLM functions](llm-functions.md)). The two LLM stages are wired
through `build_wiki_plan`'s `placement` ([`NewFactPlacement`](../../crates/mwe-core/src/planner.rs))
and `conciliatore` arguments; `NewFactPlacement::OrphanFallback` / `conciliatore
= None` degrade gracefully — the planner produces a foundation-only plan with
the deterministic orphan fallback homing every fact and all proposed pages
accepted as-is. No half-baked "structure without a verdict" path, and no hard
dependency on a configured strong slot just to get a usable plan. The light
cadence passes `NewFactPlacement::NamedThenCartografo` — the same Cartografo on
the **cheap** tier, over the remainder only — while the Conciliatore runs at
**both** cadences on the cadence's tier (see
[Stage 1](#stage-1--the-cartografo-strong-model-classification) /
[Stage 1.5](#stage-15--the-conciliatore-strong-model-dedup)).

**Tier per cadence — "the strong model works ONLY at
REM".** The strong tier above applies to the [`Cadence::Full`](../../crates/mwe-core/src/dream.rs)
compile (nightly REM + operator-driven compiles). Every LLM stage of the
frequent, cheap **light dream** (`Cadence::Light`) — Cartografo, Conciliatore,
Cronista — runs on the cheap **ingest-tier (Flash)** backend (the
same model the classifier runs on; it reaches `run_compile` as the bag's `apply`
slot), falling back to the strong slot only when no `ingest` slot is configured.
The Cartografo is the one that does not fall back: with no `ingest` slot the
light pass simply keeps its deterministic half
(`NewFactPlacement::Ingest`) rather than borrowing the Pro tier hourly. So a
light dream never touches the Pro tier; the nightly REM recompiles the same
pages at full quality — and re-judges the placements the reviewer nominated,
which the light pass never touches.
No new operator config — the slots already exist (`ingest` = Flash, `cronista` /
`rem_promotions` = strong — see
[LLM functions](llm-functions.md)); `run_compile` just selects per cadence
via the `tier_backend` helper.

## Determinism

The plan and its fingerprints are reproducible: pages are keyed in a
`BTreeMap`, facts are gathered in fact-id order, and every order-sensitive
step (child lists, link graph, compilation order) sorts explicitly. So the
same `fact_index` + `enrollment` snapshot yields the same plan byte-for-byte,
and the [dirty set](#page_fingerprint--the-dirty-set) does not churn
spuriously between runs that changed nothing. The
`#[cfg(test)]` module in [`planner.rs`](../../crates/mwe-core/src/planner.rs)
is the SSOT for the covered scenarios — among them the slugify canonical
form, the Architetto homing assigned + orphan facts, the fixpoint GC of an
empty parent/child chain, the fingerprint reacting to a link change, the
Cartografo blueprint parse + new-page dedup, the changed/new/removed dirty
set, and the end-to-end `build_wiki_plan` incremental-idempotency
(first build homes the fact, an unchanged second build yields zero dirty
pages).

## The compiler — Il Cronista + the Record Writer

[`mwe-core::compiler`](../../crates/mwe-core/src/compiler.rs) is the **prose
stage**: it consumes the [`CompilationPlan`](../../crates/mwe-core/src/planner.rs)
and turns each fact into the markdown a reader (and recall) sees. The planner
decided *where each fact lives*; the compiler decides *how it reads*.
[`compile_dirty_pages`](../../crates/mwe-core/src/compiler.rs) walks only the
[dirty set](#page_fingerprint--the-dirty-set) — a removed page (present in
`dirty_pages` but gone from `pages`) is skipped here; its on-disk file is
deleted by the [orphan-file sweep](#stage-2--the-architetto-deterministic-assembly)
at the tail of the same compile — and returns a `CompileReport`
(`leaves` / `lists` / `unchanged` / `degraded` / `orphan_files_swept` /
per-page soft errors).
Per-page LLM or parse failures are collected into the report and the run
continues — a leaf whose Cronista keeps failing lands in the
[degraded guard-only rewrite](#degraded-mode--the-guard-only-rewrite) rather
than freezing — and only infrastructure failures (DB, filesystem) bubble.
Every failed or degraded page also feeds the
[per-page failure ledger](rem-cycle.md#per-page-compile-failure-surfacing)
that surfaces persistent failures to the operator.

### The dispatcher — `lista` vs prose

[`compile_page`](../../crates/mwe-core/src/compiler.rs) routes each page:

1. A leaf whose ingest-decided `style` (`page.style`) is
   **`lista`** → the **Record Writer** (atomic records, no LLM); a leaf with
   **no facts at all** (a foundation page whose facts have not arrived yet —
   empty *concept* leaves never get here, the planner GCs them) →
   `compile_empty_leaf`, a deterministic minimal render (testata + the
   description one-liner, **no LLM**: handed an empty fact list the Cronista
   invents colour prose from the wikilinks alone — the dogfood re-run compiled
   Tolkien lore onto a zero-fact identity index); everything else
   — prose leaves, and a `person` / `parking_page` page carrying facts —
   goes to **Il Cronista**.

The two writers target different config slots — the Cronista the strong
tier, the Record Writer **no** model at all — so `lista` data never pays for
prose synthesis.

### Il Cronista — the leaf writer (strong model)

[`compile_leaf_page`](../../crates/mwe-core/src/compiler.rs) is **Il Cronista**,
wired to the **strong** model — the
[`LlmFunction::Cronista`](../../crates/mwe-core/src/config.rs) slot, the
faithful-fact→prose tier, **not** the 9B workhorse (a 9B invents and leaks;
faithful rendering needs the strong model). It runs the
[`cronista` prompt](../../crates/mwe-core/prompts/cronista.md) once per dirty
leaf, fed:

- its **own** `primary_facts` only — a **numbered** list, each line `N. [TYPE]
  text`. The Cronista does not write the marker, so `f=<fact_id>` and the raw
  `subject`/`allow`/`sender` are withheld — but a fact whose read audience is
  **narrower than public** carries a trailing `(audience: <names>)` hint (its
  read-set, projected by [`audience_hint`](../../crates/mwe-core/src/compiler.rs)),
  so the Cronista keeps that fact's substance **inside its `<fN>` span** and out
  of the page's default-visibility connective prose — the compiler half of the
  [redaction policy](redaction-policy.md). A fact that carries a validity window
  also gets a trailing `(validity: …)` hint (see [the validity cue](#the-validity-cue));
- the **starvation index** — every page as a `canonical wikilink →
  one-line description` line, **never** another page's facts
  ([`page_index_block`](../../crates/mwe-core/src/compiler.rs)). It includes
  the page being written, so the block is **one string per run** — built once,
  identical for every leaf, which is what makes it cacheable (see
  [the cacheable split](#the-cacheable-split--why-the-page-comes-last)); the
  prompt carries the rule that pays for it: never link a page to itself.

  **Above `CARD_INDEX_CACHE_CEILING_PAGES` (400) it changes shape and changes
  half.** `build_page_index` switches to a per-page **selection**: the
  `CARD_INDEX_SELECTION_PAGES` (40) pages whose cards sit closest to the card
  of the page being written, ranked from the vectors
  [`page_card`](engine-db-and-migrations.md#migration-ledger) holds. The
  ceiling is ten times the selection because that is where the arithmetic
  flips, not a guess at a corpus size: a cached whole index of `B` lines costs
  the first call `B` and the rest roughly `B/10`, while a per-page slice of
  `S` lines is paid in full every call, so the slice wins only while
  `S < B/10`. Because the slice differs per page it moves to the **task**
  half (`{page_index_task}`) — left in the cacheable half it would write one
  cache entry per page and read none, which is worse than not caching at all —
  and `{page_index}` keeps the rules plus a line saying where the pages are
  listed. The compiler has **no embedder and does not grow one**: cards are
  embedded by `reindex::refresh_one_card`, and a card with no vector simply
  does not rank. A page with no vector of its own (new this run, no
  description, an embedder that failed) falls back to its own wiki's pages
  **ordered by fact mass, biggest first** — that arm is taken by every page
  created in the run being compiled, so leaving it on the plan's slug order
  handed an alphabetical slice to exactly the pages most in need of good
  links. The slice is rendered **nearest first**, never re-sorted by slug:
  where a list is cut, the order is the selection;
- the recommended outgoing `[[wikilinks]]` from the plan's link graph;
- the wiki's prose tone (`resolve_tone`, cached per wiki within a run): the
  `is_agent` marker first — an agent's own wiki is its **autobiography**, so
  its pages are written in the first person
  (`agent-autobiography-first-person`) instead of as a dossier kept on it,
  and the marker has to be read before the type because that wiki is a
  `wiki-user` like a human's — then the bare `wiki_type` string for everyone
  else.

  **The first person is then narrowed per page** (`tone_for_page`), because a
  wiki is one container but not one subject: an agent's wiki accumulates pages
  whose subject is somebody else — misrouted before the agent-wiki guard went
  live ([ingest-pipeline.md](ingest-pipeline.md), and ~30% of the live
  assistant's wiki is such residue), and the residue does not vanish the day
  the guard starts working. A leaf gets the autobiography voice only when most
  of its facts have the agent as their subject; otherwise it keeps the ordinary
  identity voice. Narrating a user's pregnancy as the assistant's own life is a
  far worse failure than the third-person log the voice exists to fix.

Every link the compiler feeds a prose-writing prompt is rendered by
[`plan_page_wikilink`](../../crates/mwe-core/src/compiler.rs) in the
**canonical grammar** ([recall-pipeline.md §Link grammar](recall-pipeline.md#link-grammar)):
`[[wiki_id/page-slug]]`, always a **page** — never a bare plan slug, which
would read as a hop to a wiki that does not exist, and never the bare
`[[wiki_id]]` wiki hop either: it names a wiki and recall opens pages, so
it resolves to nothing and is not offered as a rail. The prompt's counterpart rule is
**copy-verbatim**: the model weaves the given links in character-for-character
and never mints or restyles a target (a hyphen flipped to the surrounding
underscore slug style is a dead rail). Non-canonical links still on compiled
pages converge as those pages recompile — prompt-driven, no mechanical corpus
rewriter.

Starvation is the **load-bearing invariant**, not an instruction: a writer
shown only a wikilink and a one-line description for another page *cannot*
copy a detail it was never given, so it must emit the `[[wikilink]]` instead
of paraphrasing. That mechanically enforces one-fact-one-page and is what makes
the prose a **non-redundant recall surface** rather than decoration — the same
rationale developed in the
roadmap (prose is the accuracy mechanism). On a user's
**identity index** the prompt adds a one-sentence reference-distance
belt-guard — another subject's detail is named by its `[[wikilink]]`, never
woven into the connective prose; the load-bearing protection stays the
plan-side
[identity-page discipline](#stage-1--the-cartografo-strong-model-classification),
since the Cronista only ever sees the facts the plan gave the page.
The Cronista writes flowing prose that makes the **relations** between facts
explicit, and marks **which span of prose is which fact** by wrapping it in a
lightweight tag `<fN>…</fN>` (N = the fact's 1-based number from the list above).
It writes **no** ACL, subject, `allow`, `sender`, braces, or `fact_id` — only the
span boundary. The unmarked connective prose between tags inherits the page's
default visibility. It returns one JSON object (`mergedBody`, `description`,
`style`); the `description` is the page's one-liner, and for a wiki's
**foundation** page it becomes the wiki's **abstract** (see
[the abstract sync](#the-abstract-sync--the-wikis-summary)). `description` +
`style` are also the page's **testata** (see
[the testata](#the-testata--per-page-style--description--keywords)).

The compiler then turns that into the on-disk page in two deterministic steps
([`compile_leaf_page`](../../crates/mwe-core/src/compiler.rs)):

1. **Expand** — [`expand_fact_tags`](../../crates/mwe-core/src/compiler.rs)
   replaces each `<fN>…</fN>` with the bare runtime
   `{{f=<fact_id>}}…{{/}}` region marker, rendered **by code**
   from the known fact via
   [`capture::render_marker`](../../crates/mwe-core/src/capture.rs) — the ACL
   lives in the [`fact_index`](capture-and-dedup.md) columns and gates the region
   by that key ([redaction policy](redaction-policy.md)). Because the LLM never
   writes the marker, it cannot miscount its braces or malform the region key.
   Orphan / duplicate / out-of-range tags are unwrapped to plain text.
2. **Forward completeness guard** — any assigned fact that produced no marker
   (the model failed to tag it) is **appended deterministically** as its own
   marked region, so no fact is silently lost and no non-global fact loses its
   protective ACL marker (the `missing_acl_markers` the reviewer would otherwise
   flag). A later full recompile can weave the appended facts back in.
   Both steps live in
   [`expand_and_complete_fact_markers`](../../crates/mwe-core/src/compiler.rs).

The same discipline then runs over the page's **links** — see
[the rail guard](#the-rail-guard--a-recommended-link-that-never-reached-the-prose).

The **on-disk runtime marker format is the bare** `{{f=…}}` — only the
Cronista's transient output uses `<fN>` tags; the parser, the capture path, and
every other prompt that references the marker share that one format (the full
`{{subject=… allow=… sender=… f=…}}` form is the export/interchange serialization
only — see [marker grammar §0](marker-grammar.md#0-runtime-form-vs-export-form--what-gets-written-when)).

The Cronista's **output budget scales with the page's fact mass**
([`cronista_max_tokens`](../../crates/mwe-core/src/compiler.rs):
`2000 + 200/fact`, clamped to `[3000, 32000]`): the reply carries the whole
page, so a flat ceiling silently truncates a big page's JSON and reads as a
Cronista failure (the 48-fact prod page failed exactly this way at a flat
3000). The rule is general — **output caps are resource valves, never
gates** (maintainer, 2026-07-02): verdict calls keep verdict-sized caps,
content calls scale with their input, and *any* reply that stops at the
ceiling is warned loudly by the llm layer itself (every backend checks
`FinishReason::MaxTokens` centrally; health probes opt out via
`CompletionRequest::truncation_expected`). A truncated Cronista reply also
names the cap in its failure reason instead of the generic "unparseable
JSON" — truncation must never masquerade as model flakiness.

### The cacheable split — why the page comes last

The Cronista's rendered prompt is one document that ships as **two halves**,
cut on the `=== PAGE TO WRITE ===` line by
[`split_cronista_prompt`](../../crates/mwe-core/src/compiler.rs):

| Half | Content | Rides |
|---|---|---|
| Before the marker | the standing brief + the page index | the **system** prompt, marked cacheable |
| From the marker on | this page's title / slug / parent / tone, its facts, its recommended links | the **user** turn, closed by the write instruction |

The split exists because of the shape of the spend, measured on the live
store: the brief plus the index is **~5.8k tokens and byte-identical for every
page of one compile run**, while a median page's own facts are ~170 tokens —
**97% of the input was the same block, re-bought once per page**, and input is
~70% of a page's cost (pages are short: median compiled body ~1.9k chars).
Split this way the stable half is a genuine prefix, so
[`CompletionRequest::with_cached_system`](../../crates/mwe-core/src/llm.rs)
marks it and only the first page of a run pays it in full.

Two invariants follow, and both are load-bearing:

- **nothing that identifies the page may appear before the marker.** The
  prompt's opening line forward-references the marker instead of naming the
  page; a title in the first line makes every prefix unique, which costs a
  cache *write* per page and earns no read — worse than not caching;
- **the page index includes the page being written**, so the block does not
  differ by one line per call.

The hint is honoured today only by the Anthropic backend, which puts
`cache_control` on the **last** system block (caching is a prefix match, so an
earlier breakpoint would leave the rest uncached) with the **1-hour** window:
a compile run interleaves LLM calls with disk writes and can outlive the
5-minute default, and the doubled write cost is repaid by the third read.
Every other backend ignores the flag and its wire shape is unchanged — the
light dream compiles on the ingest tier, where this is a no-op. An operator
prompt override with no marker degrades cleanly: the whole body stays in the
system prompt, nothing is marked cacheable.

### The rail guard — a recommended link that never reached the prose

The compiler hands the writer the links it must weave in — the page's
`link_graph` row
([`recommended_link_targets`](../../crates/mwe-core/src/compiler.rs)) — and
until this guard **nothing checked that any of
them landed**. The links are not suggestions: `link_graph` is parent→child plus
the page's own authored outgoing links, made symmetric (planner, step 9),
i.e. the structure the plan asserts. And a link that does not land is not a
cosmetic loss — the navigator harvests its next hops from the **prose**, and a
page is reachable by exactly three routes: a fact hit, a match on its own card,
or an inbound link somebody wrote. Nothing offers a page for merely sitting in
the same directory, so a dropped rail is a neighbour nobody can walk to.

Three parts, cheapest first:

1. **The rule.** The Cronista's brief (v1.22) makes RECOMMENDED LINKS
   mandatory the way fact completeness already was, with the reason stated and
   one prohibition: they may not be parked in a list at the end — a link
   explained by the prose around it is the point, a bare address is the weak
   form of it.
2. **The check, and one rewrite.** After a usable reply, the written links are
   parsed out of the body with
   [`recall::extract_wikilinks`](../../crates/mwe-core/src/recall.rs) — the
   same function the recall funnel harvests rails with, so the two cannot
   disagree about what a link is — and compared by address
   (`wiki_id` + page stem, `.md` and `|display` alias normalised away:
   [`missing_rails`](../../crates/mwe-core/src/compiler.rs)). A gap buys the
   leaf **one rewrite** naming exactly the dropped links
   ([`cronista_relink`](../../crates/mwe-core/src/compiler.rs)), reusing both
   prompt halves so the cached prefix still engages. The second reply is kept
   **only if it carries more of them**: a rewrite that trades one dropped rail
   for another has bought nothing, and the first draft stands. No gap, no call.
3. **The floor.** Whatever the prose still will not carry is **appended** to
   the page as its own line
   ([`append_missing_rails`](../../crates/mwe-core/src/compiler.rs)), and the
   page is reported on `CompileReport.rails_appended`. This is deliberately the
   *weaker* form of a link — a bare edge carries the label without the why,
   against this page's own prose-first thesis — and it is written anyway
   because an unreachable neighbour is worse. The report is what keeps the
   trade visible: how often the writer declines a rail is a **prompt** signal,
   and it used to be invisible.

⚠️ The size of the loss is **unmeasured on this engine**. It was 111 of 334
links (33 %) on the corpus compiled before 2026-08-04, which is a static
property of files that no longer exist — and those pages were written under
the older prompt, before the compiler stopped minting `[[wiki_id]]` links
that pointed at an unopenable page. What is certain is the mechanism, which
was read off the code: nothing checked.

### Degraded mode — the guard-only rewrite

A Cronista reply that is **unusable** — a transport/backend error, or output
that is not parseable JSON — costs the page **one retry**
([`cronista_with_retry`](../../crates/mwe-core/src/compiler.rs)): a fresh call
whose user message reminds strict JSON (the system prompt is unchanged; no
prompt machinery). Transport errors and parse failures are handled
identically, per page — one flaky call can never abort the compile pass (the
REM **reorg**'s own LLM-transport-fatal model is a separate, deliberate policy
— see [rem-cycle.md](rem-cycle.md#cycle-invariants-and-crash-semantics)).

**A rejected request is not flakiness and buys no retry.**
[`LlmError::Invalid`] (the request itself was refused — a 400) and
[`LlmError::Auth`] (bad or missing credential) go straight to the degraded
rewrite, because a second identical call can only be refused identically.
Observed live: with the API answering *"credit balance too low"*, a whole
compile run spent two calls per page to be told the same thing twice. The
report names the reason (`Cronista failed (not retryable): …`) so the
distinction is visible in the Dream console, not just in the logs.

If the retry is also unusable, the page falls back to the **guard-only
rewrite** ([`compile_degraded_leaf`](../../crates/mwe-core/src/compiler.rs))
instead of freezing:

- the existing on-disk page — prose, testata, markers — is kept
  **byte-for-byte**;
- every planned fact **without a marker on the page yet** is appended as its
  own marked region, canonical claim text only (the exact shape of the forward
  completeness guard above) — the degraded path **never invents content**;
- a page that never compiled is born as its plan testata plus the marked
  regions.

So every fact reaches disk with a marker — recall, redaction, and the
[repoint](#the-fact_id-repoint--recall-returns-prose-text-stays-canonical)
all work (offsets are stamped for the appended regions and for any pre-pointed
pending render whose marker already sits on the page) — while the beautiful
full rewrite waits for the next successful compile, which rewrites the page
wholesale and supersedes the appended tail. The append is **idempotent**: an
appended fact now carries its marker on disk, so a second degraded pass finds
nothing missing and writes nothing — no duplicated regions across failing
cycles.

A degraded page is **not cleanly settled**: the outcome is recorded distinctly
(`CompileReport.degraded`, surfaced by the
[dream journal](rem-cycle.md#run-history-journal)), and the slug is parked on
the persisted plan's `force_dirty`
([`planner::park_force_dirty_in_persisted_plan`](../../crates/mwe-core/src/planner.rs))
so the next build retries the proper rewrite even on an otherwise idle night.
Failed pages (infrastructure soft errors) are parked the same way. Each failed
or degraded compile also increments the
[per-page failure ledger](rem-cycle.md#per-page-compile-failure-surfacing);
only a clean full rewrite resets it.

### The validity cue

A fact can carry a **validity window** (`valid_from` / `valid_to`, ISO-8601) in
its [`fact_index`](capture-and-dedup.md) row — *until* when the claim holds, the
[temporal-validity signal](../concepts/memory-model.md). The compiler **projects** that window into the prose so the recall
navigator sees it: [`primary_facts_text`](../../crates/mwe-core/src/compiler.rs)
appends a compact `(validity: …)` hint to a fact's numbered line
when (and only when) it carries a *meaningful* bound, and the
[`cronista` prompt](../../crates/mwe-core/prompts/cronista.md) instructs
the model to weave a brief, natural validity cue (*"valido fino all'11 giugno"*,
*"a partire da lunedì"*) into that fact's `<fN>` span — never the raw ISO bound,
never its own sentence. A durable fact (both bounds `None`) gets no hint and no cue.

A **closed** window may also carry its *why*: when `decay_reason` is stamped
(the [`fact_index::decay`](../../crates/mwe-core/src/fact_index.rs) vocabulary —
`completed` / `retracted` / `contradicted`), the hint becomes
`(validity: … , closed: <reason>)` and the prompt tells the Cronista to phrase
the closure with that meaning (*"comprato il 7 giugno"*, *"progetto
abbandonato"*) instead of a generic *"fino al"* — the reason token itself never
prints.

The one subtlety is the **open-ended** case (`valid_from` set, `valid_to`
`None`). [`validity_hint`](../../crates/mwe-core/src/compiler.rs) compares
`valid_from` against the compile-time `now`: a **future** start (an announced
onset like *"da lunedì cambio ufficio"*) keeps the dated `(validity: from <t>,
open-ended)` form, but a start that is **not** in the future is just the
record/freshness timestamp — *when we learned the fact*, not a biographical
onset — so it collapses to a dateless `(validity: open-ended)`. This stops the
Cronista from narrating a false "*known as Sméagol since June 2026*" / "*lives in
Ferrara since today*" onset on identity and durable facts: the record date is
withheld precisely so it cannot bleed into the prose.

This is a **one-way projection from the DB**, deliberately: the validity stays
**authoritative in `fact_index`** (DB-authoritative per-fact metadata — see [redaction-policy.md](redaction-policy.md))
and the rendered cue is **never parsed back** — it is a recall aid (*"prose is the
accuracy mechanism for recall"*), not a persistence mechanism. The code only hands
the dates over; the *wording* is the LLM's call (no hard-coded format gate).

> **Live on the standard-wiki path.** The Cronista only
> compiles facts gathered from **narrative** wikis. The narrative
> `buffer → promote` path threads validity:
> [`buffer_capture`](../../crates/mwe-core/src/capture_buffer.rs) stages
> `valid_from` / `valid_to` on the capture, and
> [`promote_one`](../../crates/mwe-core/src/dream_light.rs)
> copies them into `fact_index` — so a dated narrative fact (e.g. an appointment)
> reaches the Cronista with a real window and the cue renders. Validity reaches
> `fact_index` on **both** paths (the direct path and the standard-wiki path).
> Exercised end-to-end
> (`promotion_carries_validity_into_fact_index` in `dream_light`) and at the unit
> level (`primary_facts_text_appends_validity_hint_only_when_present`).

### The provenance link — link, don't duplicate

The same projection mechanism carries a fact's **provenance breadcrumbs**
(`fact_index.authored_refs`, a JSON array of plain `[[wiki_id/page]]`
wikilinks — the [smart-consumer superset](smart-wikis.md) §4). A smart
consumer that just wrote detail to its **project
wiki** via `wiki_admin_push` carries the breadcrumbs that call returned into the
turn's `wiki_ingest_message` (`metadata.authored_refs`); they ride capture →
light-dream → fact, and on compilation
[`primary_facts_text`](../../crates/mwe-core/src/compiler.rs) appends a
`(detail at: [[…]])` hint to the fact's numbered line. The
[`cronista` prompt](../../crates/mwe-core/prompts/cronista.md) then tells the
model to write a **terse reference** weaving in the `[[wiki_id/page]]` link
inside that fact's `<fN>` span — *not* to reproduce the detail it was never
shown (the same "don't duplicate another page's content" rule that governs
ordinary inter-page wikilinks). So personal memory keeps the **shape of the
work + a navigable pointer**; the body stays where it is authoritative. The
`[[…]]` form is followed by recall-as-navigation and kept honest by the REM
backlink-reciprocity detector. A fact with no breadcrumbs is an ordinary
personal fact, written in full.

The hint is **existence-vetted** (`compiler::authored_ref_resolves`): each
ref resolves against the live tree — the wiki must exist, a page ref's file
too — and a ref whose target vanished (an absorbed dossier stub, a renamed
page) is filtered out, the hint dropped entirely when nothing survives. The
DB row keeps the dead ref as audit provenance; it just never reaches prose
as a dead rail — the same posture the
[link grammar](recall-pipeline.md#link-grammar) takes everywhere else.

Unit-pinned by
`primary_facts_text_appends_provenance_hint_only_when_present`,
`primary_facts_text_filters_dead_authored_refs_from_the_hint` and
`authored_ref_resolves_vets_against_the_live_tree`; the storage
round-trip (capture → journal → fact) by
`authored_refs_survive_journal_reindex_round_trip`.

### The succession pointer — one hop from the obituary to today's truth

The third projection on the fact line closes the **eulogy gap**: a page that
narrates only closed facts ("non più attuale, sostituita da indicazioni
successive") with no pointer to where the replacement lives — recall landing
on a well-written obituary. A **live** closed row can carry
`fact_index.successor_fact_id` — the fact that replaced it, stamped by
[`close_validity`](../../crates/mwe-core/src/fact_index.rs) whenever the
closer knows the successor (the [REM contradiction sweep](rem-cycle.md)
passes the seed's superseding fact to its satellites; the completion sweep
its evidence fact; a `None` never wipes an earlier pointer). It is distinct
from `superseded_by`, which is welded to the `superseded_at` tombstone —
a superseded row leaves the page entirely, while a closed row keeps
narrating with its closure cue. The pointer is part of the plan's
`fact_render_key`, so stamping it recompiles the page; the `validity_close`
receipt snapshots and restores it on revert.

On compilation, [`successor_wikilink`](../../crates/mwe-core/src/compiler.rs)
resolves the successor to its **planned home page** (the plan is
forest-wide, so the hop may cross wikis — the current meal-prep truth can
live in another consumer's wiki) and `primary_facts_text` appends a
`(current: [[wiki_id/page]])` hint to the closed fact's line. Resolution is
**placement-vetted by construction**: an unplaced successor yields no hint
(the dead-rail discipline of `ref_alive`), and a successor homed on the
*same* page yields none either — the Cronista already narrates both facts
side by side there. The [`cronista` prompt](../../crates/mwe-core/prompts/cronista.md)
(v1.11, SUCCESSION block) weaves the pointer into the closure prose inside
the fact's own `<fN>` span — *"non più attuale — la versione corrente è in
[[…]]"* — copying the link verbatim and never restating the successor's
content.

Unit-pinned by `primary_facts_text_appends_succession_hint_via_resolver`,
`successor_wikilink_resolves_within_the_plan`,
`close_validity_stamps_and_restore_round_trips` and
`close_validity_without_successor_keeps_an_earlier_pointer`.

### The testata — per-page style + description + keywords

Every compiled page carries a **testata** (header) in its frontmatter: a
`style:` tag, a free-text `description:`, and a compile-synced `keywords`
entry (see [the page-keyword sync](#keyword-sync--fact-topics-into-_meta-and-the-page-testate-recall-navigation)).
This is the **generic / per-page**
level of the two-level header — the level for a wiki whose
pages are heterogeneous (a user/group wiki, or a mixed emergent wiki). The
**specialized** level, where a homogeneous wiki lifts `style` onto its `_meta`, is
later work (it is born with emergence), not this one.

- **`style`** is the page's dominant **writing style** from the closed palette
  `prosa` / `prosa-tecnica` / `lista` — a **recall read-hint** that tells a future
  navigator *how* to read the page (follow the prose thread vs scan point-by-point
  vs deterministic record lookup), not a gate. **Two sources, with a preference
  order:** the ingest classifier proposes a per-page `style` that is
  carried through the plan as [`PagePlan.style`](../../crates/mwe-core/src/planner.rs),
  and the leaf-page **Cronista** also picks a `style` at compile time. The testata
  **prefers the ingest plan's proposal** (`page.style`) and **falls back to the
  Cronista** (`body.style`) when ingest proposed none —
  `normalize_style(page.style.or(body.style))`. The
  [`cronista` prompt](../../crates/mwe-core/prompts/cronista.md) picks
  `prosa` (interconnected knowledge) or `prosa-tecnica` (itemizable / technical
  content) — it writes prose, so it never returns `lista` (atomic-record pages it
  does not author). [`normalize_style`](../../crates/mwe-core/src/compiler.rs)
  coerces the value into the palette; absent / unrecognised → `prosa`.
- **`description`** is the page's **card** — the one line the recall navigator
  decides from, since it is shown a page's name, its keywords and this line and
  never its prose. It is **written**: the Cronista's fresh `description` for
  the page it just wrote (v1.7).
  It falls back to the plan's
  [`PagePlan.description`](../../crates/mwe-core/src/planner.rs) only when the
  writer returned none. Besides the testata it feeds the
  [abstract sync](#the-abstract-sync--the-wikis-summary). It serves both
  **recall** (orient before opening) and **placement** (where to file a new
  fact).

[`render_page_file`](../../crates/mwe-core/src/compiler.rs) writes both into the
frontmatter (`description` is omitted when empty, quotes / newlines flattened).
It does **not** write a `page_type` line: that line was read by nothing and sat
one above `style`, teaching every reader that a page carries two parallel
classifications. It carries one — `style`. What kind of page it is, is its file
name (see [the data model](#the-data-model)). The `style` tag records the page's **dominant**
read-strategy, and it matches the body: a `lista` testata
sits over a [record body](#the-record-writer--lista-pages-no-llm), never prose.

### No page lists other pages

No page is an overview of other pages — not a group's card, not a wiki's
root. Founder: *«perché dovrei avere un elenco di pagine? Dalle wiki utente lo
abbiamo già tolto … l'elenco delle pagine, ognuna col suo biglietto, arriva al
motore leggendo i file e i frontmatter, non serve un indice che poi va pure
mantenuto.»*

So the compiler has **two** writers — the Cronista for prose, the record
renderer for `lista` pages — and a page with no facts renders as its card. What
a group is lives in its `_meta.md`: its title, its `scope` prose (which the
ingest classifier reads as a placement signal) and its one-line abstract. Its
only foundation node is its parking page, the same shape a topic wiki has always
had.

The `regenerate-index` prompt is deleted with the pass it fed.

### The Record Writer — `lista` pages (no LLM)

[`compile_list_page`](../../crates/mwe-core/src/compiler.rs) handles a leaf whose
ingest-decided `style` is **`lista`** — a shopping list, a filmography:
**atomic-record data** scanned / looked-up at a stroke, not prose to be
*understood*. The facts are already atomic, so there is nothing to synthesise:
the Record Writer renders each fact **deterministically** as one bullet record
wrapped in its bare runtime `{{f=<fact_id>}}…{{/}}` marker
(via [`capture::render_marker`](../../crates/mwe-core/src/capture.rs), a single
line — newlines in the claim flatten to spaces; the ACL gates from the DB by
that key) and writes the page directly,
**bypassing Il Cronista entirely** (no strong-model call). The Cronista
itself never emits `lista` ([`cronista` prompt](../../crates/mwe-core/prompts/cronista.md)
§STYLE), so `page.style` is the sole source of a record page.

One record per fact means **every fact keeps its protective per-fragment ACL**
with no forward-completeness guard needed (unlike the prose path, where the LLM
can drop a tag). The testata is `style: lista` (the ingest choice that routed
here) + the plan's ingest-proposed `description` (there is no Cronista on this
path to author one). Like a leaf, the Record Writer **repoints** each fact's
`fact_index` row onto its compiled record region (so recall returns the rendered
line while `fact_index.text` stays the canonical claim) and, for a
foundation page, syncs the `_meta` abstract. The outcome is counted as a `lists` page in
the `CompileReport`.

> **Validity on a record — the done-cue.** The per-fact validity window stays
> **authoritative in `fact_index`**; what a record re-surfaces inline is exactly
> one thing: an **explicit closure**. A fact carrying a `decay_reason` renders
> with a deterministic, language-free cue — `latte · ✓ 2026-06-07` for a spent
> intention (`completed`), `· ✗` for a retracted/contradicted one
> ([`record_closure_cue`](../../crates/mwe-core/src/compiler.rs)) — the Record
> Writer's counterpart of the prose [validity cue](#the-validity-cue)
> ("comprato il 7 giugno"), glyph-shaped because this path has no LLM to match
> the user's language. The cue lives **inside the marker region**, so redaction
> hides the closure together with the fact it describes. A window without an
> explicit closure (a future end, a mere expiry) gets no cue — a dated item
> like an appointment is `prosa-tecnica` anyway, which the Cronista writes.
> The closed record **stays on its list** marked done; the consumption *event*
> lands on the list's **registry twin** page (`spesa` → `spesa_registro` — the
> [ingest closure verb](ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture)),
> so the list page itself stays current. Registry entries age out through
> organic forgetting (roadmap group 11).

### The card heal — a page is described by what was written on it

[`PagePlan::description`](../../crates/mwe-core/src/planner.rs) is seeded once,
from the ingest classifier's `page_description` proposal, and the
[concept registry](../../crates/mwe-core/src/planner.rs) then persists that
first guess. Nothing used to read back what the page turned out to say — while
the writer puts **its own card** in the testata on every compile. So the two
diverged, and the stale one was the copy the models saw: the Cronista's
[page index](#the-cacheable-split--why-the-page-comes-last) carries every
page's description, its own line included, and the `{snippet}`
carries its children's.

**That is how an invented frame becomes permanent.** The confirmed production
case (card 57, 2026-07-24): a turn complaining that an assistant had signed the
sender up for a fair minted a page described as *«Progetti e attività relativi
a …»* — a body of work nobody had described. The **fact** was a defensible
paraphrase; the **frame** was invented, it was fed back to the compiler on
every cycle, and the compiled page grew a paragraph about managing external
collaborations out of it. Every guard we have judges facts.

[`heal_page_cards`](../../crates/mwe-core/src/planner.rs) closes the loop: each
plan build reads the page's testata `description:` and adopts it onto the plan
page and its registry entry. The card the writer produced **from the page's
actual facts** replaces the guess, so a bad first frame is self-correcting
rather than permanent.

Three properties worth keeping:

- it runs on the **plan-reuse path too**, and mostly there — a page's written
  card changes when the page is *compiled*, which is exactly a build with no
  new facts. Reached only by the full-rebuild branch it would almost never run;
- it **never marks a page dirty**: [`page_fingerprint`](../../crates/mwe-core/src/planner.rs)
  does not carry the description, so the correction rides the next compile that
  happens for its own reasons instead of buying one;
- it reads the **testata fence only**. A `description:` line in the body is
  prose somebody wrote, not the page's card.

Best-effort and idempotent: an unreadable or testata-less page is left alone,
and the registry is rewritten only when something actually healed.

### The abstract sync — the wiki's `summary`

When the compiler (re)writes one of a wiki's **own pages** — its identity
**card** (`@profile.md`) or its **parking page**
(`@notes.md`); ordinary pages use `<slug>.md` and are skipped — it persists a
one-line **abstract** into that wiki's `_meta.md` (`extra["summary"]`) via
[`meta_annotate::sync_wiki_summary`](../../crates/mwe-core/src/meta_annotate.rs).
The source is the page's own **card**, written by Il Cronista (v1.7 — until then the writer emitted prose and no
one-liner, so a group's abstract was the plan's literal `Group <slug>`). A
**`lista`** wiki still uses the plan's
[`PagePlan.description`](../../crates/mwe-core/src/planner.rs): the Record
Writer has no LLM to author one.
It keys on
[`PagePlan::is_foundation`](../../crates/mwe-core/src/planner.rs) — the
property that is actually meant, rather than any one file name.

The write is **best-effort** (a
`_meta` hiccup is logged, never fails the page) and **idempotent** (rewritten
only when the abstract changed; the `_meta` prose body is preserved), so it
refreshes exactly when the wiki's foundation page is recompiled.

This is the LLM-authored companion to the deterministic
[topic-keyword sync](#keyword-sync--fact-topics-into-_meta-and-the-page-testate-recall-navigation):
together they fill the per-wiki `summary` + `keywords`. Those are **write-side**
vocabulary — the filer's, not a reader's: nothing on the read side is shown a
wiki, a wiki card, or a list of them
([recall pipeline](recall-pipeline.md)).

### The `fact_id` repoint — recall returns prose, `text` stays canonical

The `f=<fact_id>` on each marker is the **stable id threaded from the plan**
([the data model](#the-data-model) keeps fact identity through render). After
writing a leaf, the compiler **repoints** each
fact's `fact_index` row at the compiled marker region: it re-parses the written
page, and for every marker whose `f=` matches a known fact it calls
[`fact_index::move_to_wiki`](capture-and-dedup.md) with the page's `wiki_id`,
the new `source_path` + byte offsets
([`repoint_facts`](../../crates/mwe-core/src/compiler.rs); `prepoint_plan_moves`
does the same for the pending-render pre-point). Moving `wiki_id` alongside
`source_path` is what upholds the invariant that **a fact's `wiki_id` is always
the wiki whose page physically carries it** — the facts decide the page's wiki,
so a fact rendered onto another wiki's page re-homes there rather than leaving a
stale `wiki_id` behind (plain `move_region`, which never touched `wiki_id`, was
the source of the earlier `wiki_id`/`source_path` divergence). The
effect is a clean split of duties: **recall** returns the *compiled prose
passage* (offsets now point into the published page, not the *no page yet*
address the fact was promoted with — see
[narrative-buffer.md](narrative-buffer.md)), while
`fact_index.text` keeps the **canonical claim** used for embedding and dedup.
The write itself is **idempotent** — the compiler renders the full file
(frontmatter included), compares it to what is on disk, and only writes on a
difference, reporting an `unchanged` page otherwise; `created:` is read back
from the prior file and preserved across recompiles. The **repoint runs on the
unchanged path too**: a fact pre-pointed at the page as a pending render (see
the commit point below) whose marker already sits in the on-disk content still
gets its offsets stamped.

To make canonical markers possible, [`capture::render_marker` and
`new_fact_id`](capture-and-dedup.md) were lifted to `pub(crate)` so the compiler
emits markers in the same canonical form as the capture path and mints ids when
needed.

### Cross-page moves — the DB-first commit point

When a new plan **reassigns a fact from page A to page B** (a Cartografo
re-home, a Conciliatore redirect), A's recompile rewrites it without the fact's
marker. If the row still pointed at A at that moment, the
[orphan sweep](reindex-pipeline.md#the-orphan-sweep-guard) would read the
missing marker as the operator's forget gesture and tombstone the live fact —
the same race the promote machinery closed for REM moves, observed live in the
dogfood rebuild (one fact in 377 silently lost on a page rewrite). So before
**any** page write, [`compile_dirty_pages`](../../crates/mwe-core/src/compiler.rs)
runs [`prepoint_plan_moves`](../../crates/mwe-core/src/compiler.rs): every
dirty-page fact whose row still lives on a **different** file is repointed onto
its planned page as a **pending render** (NULL offsets — sweep-exempt per the
[pending-render invariant](reindex-pipeline.md)); the per-page repoint then
stamps the real offsets once the marker is on disk. The failure mode degrades
safely: a destination page whose Cronista fails ends in the
[degraded guard-append](#degraded-mode--the-guard-only-rewrite) (its facts
reach disk marked, offsets stamped); an infrastructure soft-fail leaves its
facts as pending renders — recall serves the canonical claim until the next
compile repairs the render. Never a silent tombstone.

### Reindex exclusion of standard pages

Because standard pages are now compiler **output**,
[`reindex_full`](reindex-pipeline.md) **skips the marker sweep** on them
(it reads each wiki's `_meta.md` smart flag and `continue`s past any
non-smart wiki's page enumeration — only smart wikis keep
the marker reindex). Without this exclusion a reindex would parse the compiled
prose region and **overwrite the canonical claim text** in `fact_index.text`
with the rendered passage, undoing the
[repoint's text/offset split](#the-fact_id-repoint--recall-returns-prose-text-stays-canonical).
**Structured** (lists / cron / contacts) and **smart** wikis keep
the full marker reindex — they are human/agent-authored, not compiled.

### Perimeter

The Cronista is documented as skipping structured wiki types, but that guard is
satisfied **upstream**: the planner gathers facts only from standard wikis, so
a structured or smart wiki never enters the plan and therefore never reaches
the compiler. No per-page structured/smart check is needed in the compiler
itself.

## Keyword sync — fact topics into `_meta` and the page testate (recall navigation)

After the dirty pages are compiled, [`run_compile`](../../crates/mwe-core/src/dream.rs)
runs two more **deterministic, zero-LLM** passes from
[`mwe_core::meta_annotate`](../../crates/mwe-core/src/meta_annotate.rs), one per
card level:

- **`sync_wiki_keywords`** — for each wiki, the **sorted union of every active
  fact's `topics`** (via [`fact_index::find_by_filters`](capture-and-dedup.md))
  written back as the `topics` entry of that wiki's `_meta.keywords` — one
  comma-joined scalar.
- **`sync_page_keywords`** — the same union grouped one level down, by
  `fact_index.source_path`: each page's testata gets a `keywords.topics` entry
  carrying the topics of the facts **living on that page**. The wiki-level union
  orients the hop *into* a wiki; the page-level entry orients the hop *inside*
  it. A page whose facts were moved away (e.g. a REM split) sheds its stale
  entry on the next compile; a page without a frontmatter testata is left
  untouched — the card is compiler output, never invented by the sync.

Both unions apply the **ACL card boundary**
([`identity-and-acl.md`](../concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry)):
only facts at the wiki's default visibility (subject `global` or the resolved
`scope` principal) contribute topic words — an off-default region never
surfaces on a card readable at wiki level. The Cronista's `description`
(prompt v1.7) carries the same contract on the prose side.

Both writes are **idempotent**: a file whose `topics` already match is left
untouched (body and sibling fields preserved), so a steady-state compile rewrites
nothing; and both passes are **best-effort** — like the reviewer they are logged
on failure and never fail the dream (a missing annotation degrades recall, it
does not corrupt the wiki). They sit inside the `cronista`-gated section, so a
deployment with no strong model neither compiles prose nor syncs keywords.

This is the **producer** for the recall-navigation entry-points
([recall pipeline](recall-pipeline.md#entry-point-gathering--recall_nav-navigation-phase-1)): the
populated wiki-level `topics` keyword is what
[`wiki_navigate`](recall-pipeline.md#consumer-facing-deep-recall--the-wiki_navigate-tool) substring-matches
against when it gathers entry **pages**; the page-level entries are the per-page
cards the navigator decides each hop from. Neither is a wiki offered to a
reader — [there is no such thing](recall-pipeline.md). It is the **deterministic floor**
of the compile-time enrichment that design calls for; its LLM-authored companion
is the per-wiki [abstract](#the-abstract-sync--the-wikis-summary). The
remaining annotations (annotated `[[slug|hint]]` links, the typed link graph)
attach to the compiler's output in later work.

## The reviewer

After the Cronista writes the dirty pages, [`crate::reviewer::review`] runs a
set of **deterministic, zero-LLM** invariant checks over the plan and the
compiled page bodies and returns a non-blocking [`ReviewReport`] — it never
mutates the corpus, it surfaces problems for a maintainer or a later cycle.
The checks (the [`ReviewReport`](../../crates/mwe-core/src/reviewer.rs)
fields are the roster):

- **empty page** — an ordinary page with zero facts (the Architetto's
  fixpoint GC should have removed it; a hit is a planner regression).
- **duplicate fact home** — a `fact_id` on two or more pages (a
  one-fact-one-page violation).
- **asymmetric link** — an `a → b` edge with no `b → a` back-edge (the
  Architetto makes the graph symmetric, so any asymmetry is a regression).
- **duplicate prose** — two pages whose stripped bodies share a
  char-6-gram Jaccard ≥ `PROSE_DUP_THRESHOLD` (reuses `recall::jaccard_6gram`;
  fact-less pages excluded). The starvation invariant should make this near-zero, so a
  hit means a leaf leaked another's content — or that two near-synonym pages
  cover one concept: these pairs feed the
  [REM page-merge sub-job](rem-cycle.md#page-merge-sub-job-semantic-page-consolidation)
  as merge candidates.
- **missing ACL marker** — a non-global fact (subject ≠ `global`) on a page
  whose compiled body carries no non-public `{{… f=<fact_id>}}` marker for it.
  This is the **ACL-leak guard**: a claim about a named subject rendered as
  unmarked prose would be world-readable. Since the Cronista's **forward
  completeness guard** appends, at write time, any fact it omitted (with its
  full marker), this should be near-zero in practice; the reviewer remains the
  independent backstop.
- **cross-subject bloat** — an identity index whose **plan** carries a
  foreign-subject fact: subject is a different user, or a group the page's
  user is not a member of (a group they belong to is their own shared
  context, never foreign; global never qualifies). Identity-card detection
  reads the `_meta` `wiki_type` — the page is a `wiki-user`'s `@profile.md`
  (the agent wiki included; group wikis never qualify) — via the
  enrollment-fed
  [`IdentityContext`](../../crates/mwe-core/src/reviewer.rs), which
  `dream::run_compile` loads best-effort (a load failure only disables this
  one check). This is the observability half of the Cartografo's
  [identity-page discipline](#stage-1--the-cartografo-strong-model-classification)
  — a count in the report/log, never a gate. The check is plan-level on
  purpose: plan placement is the load-bearing channel (the Cronista renders
  only the facts the plan gives a page), so a plan-clean identity index
  converges to a clean disk page at its next compile.
- **leaf with children** — a page other pages parent under: the
  two-rank topology violated by a fact-bearing container (the *empty*
  container case never reaches the reviewer — the Architetto's GC keeps it
  while it has children and removes it once it has none). Parked as a
  placement re-open so the Cartografo re-homes its facts.
- **oversized page** — a fact-bearing page at/over
  `OVERSIZED_PAGE_THRESHOLD` (a nomination constant beside
  `PROSE_DUP_THRESHOLD`, tunable in code). Mass alone re-opened nothing
  before this check, so a **subject-clean grown page could never split**
  (bloat and the compile-failure streak were the only re-open sources) —
  and the refile sweep's deliberate land-on-the-buffer had no
  redistribution leg. Parked as a placement re-open; the Cartografo's
  split-by-mass lever still owns the verdict, so a page the model judges
  coherent stays whole (and simply re-nominates next review — the cost of
  keeping the gate out of Rust).

### The findings→healing bridge

The report is no longer log-and-drop: `dream::park_bridge_signals`
persists what tonight's review (and the
[compile-failure ledger](rem-cycle.md#per-page-compile-failure-surfacing))
learned onto the compilation plan — the reviewer runs at the dream's
tail, after this cycle's refile and plan build, so its findings can only
act on cycle N+1 (the `force_dirty` park pattern). All
**nominations, never verdicts**:

- each `cross_subject_bloat` **fact** parks as a `refile_candidates`
  entry — drained by the next
  [refile sweep](rem-cycle.md#cross-wiki-refile-sweep-sub-job), which
  seeds it straight to the refile judge past the cosine margin (the
  reviewer already nominated it; the judge still decides, and refuses
  what does not apply);
- each `cross_subject_bloat` **page**, each topology-anomalous page
  (`leaf_with_children`), each `oversized` page, plus
  every page whose compile keeps failing
  (`compile_failures::persistent`, streak ≥ 2), parks as a
  `reopen_pages` entry — consumed by the next **Cartografo**
  [`build_wiki_plan`](../../crates/mwe-core/src/planner.rs),
  which drops that page's facts from the carry-over so the Cartografo
  **re-judges their placement** with the mass + identity + container
  signals live. This is the healing half the carried-placement model
  lacked: carried placements are re-emitted as-is by design, so without
  the re-open an old misplacement never heals and split-by-mass never
  fires on an old page.

**Only a build that runs the Cartografo may consume the re-open park.**
A light (`Ingest`) or degraded-full (`OrphanFallback`) build carries it
forward untouched: those placements would re-settle the re-opened facts
on stale ingest `target_page` hints / the subject's foundation page —
burning the nomination and silently **reversing** considered moves
(observed live 2026-07-04: a light build undid the refile judge's
cross-wiki move within three hours, re-filing the facts by hints that
predated the move).

Both parks survive plan rebuilds until consumed (`refile_candidates` is
drained only by the sweep; `reopen_pages` only by a Cartografo build),
and a nomination the cap squeezed out converges anyway — the next review
re-parks whatever still stands. Pinned by
`reopened_pages_re_enter_the_to_place_pool_and_parks_drain`,
`refile_sweep_seeds_parked_reviewer_candidates_past_the_margin` and
`persistent_lists_streaks_at_or_over_the_bar`.

## The cadence — wired into REM

The planner + compiler + reviewer are composed in one place —
[`mwe-core::dream`](../../crates/mwe-core/src/dream.rs) (`run_compile`: rebuild
the plan incrementally → compile the dirty pages →
[sync the `_meta` + page-testata topic keywords](#keyword-sync--fact-topics-into-_meta-and-the-page-testate-recall-navigation)
→ review) — driven from
**both** cadences (light + full) plus the manual CLI/dashboard triggers, sharing
one `Arc`-held LLM bag. The scheduler, the CLI, and the dashboard all
delegate to `dream`:

- **Light dream** (frequent — `rem.schedule.light_*`): after the promotion
  step ([dream-light](narrative-buffer.md)), if anything was promoted it runs
  the compile pass — so fresh captures become readable prose **without waiting
  for the night** (maintainer option 2). Cost-guarded: the compiler only
  touches dirty pages, and a plan with nothing new is a cheap no-op.
- **REM full cycle** (nightly — `rem.schedule.interval_secs`): after the reorg
  sub-jobs (dedup / decay / archive) settle the fact set, it runs the same
  compile pass over the now-stable corpus.

The compile pass is skipped when the `cronista` slot is unconfigured — a
configuration onboarding is meant to prevent
([admin-llm-config.md](admin-llm-config.md#the-models-are-mandatory)); the light
dream then drains the queue deterministically so it does not grow for ever, and
the prose waits. The two LLM stages map onto
existing strong-tier REM slots: Cartografo → `rem_promotions`, Conciliatore →
`rem_dedup_semantic`, Cronista → `cronista`. The CLI
hatch `mwe-mcp rem run-compile` drives one pass out of band.

## Human edits on compiled pages

A compiled standard page is **machine-owned**. The model is deliberately neither a
shadow-diff three-way merge nor a marker reindex of the prose — the pieces mwe-mcp
already has make both unnecessary (maintainer 2026-05-31):

- **Influence a page → leave a comment, not a hand-edit.** The dashboard's inline
  comment affordance parks an unprocessed `wiki_briefing_items` row anchored to a
  heading. There is **no submit** for narrative comments — a memory edit must
  never let a user kick off a token-burning job. The comments **stay until a
  dream applies them all together** (read in one batch).
- **Undo a change → a proposal.** Reverting a change is mwe-mcp's reversible
  `structure_proposals` / forge path.
- **A stray hand-edit is harmless and ephemeral.** Because standard pages are
  [excluded from the marker reindex](reindex-pipeline.md), editing the compiled
  prose by hand never pollutes the canonical claim in `fact_index`; the next
  recompile of that page simply overwrites it. Manual edits are discouraged, and
  the system makes them a no-op rather than fighting them.

When a dream applies the parked comments ([`mwe_core::comment_apply`](../../crates/mwe-core/src/comment_apply.rs)),
each comment becomes a **fact-level op** against the facts of the page it is
anchored to — `correct` (claim fixed in place), `remove` (tombstone), or `add`
(new fact). The interpreter runs on the **ingest** strong tier (turning a
free-text correction into precise ops is the same judgment as ingesting a
message — no new operator slot). Two invariants hold the cost rule and the ACL:

- **Containment.** A comment only ever touches its anchored page's facts; a
  `fact_id` not on that page is refused (a hallucinated / cross-page id never
  mutates a stranger's fact). A `correct` keeps the same `fact_id`, so the
  [content-aware fingerprint](#page_fingerprint--the-dirty-set) marks **only that
  page** dirty — the recompile is one page, never a whole-wiki rescan.
- **A fact is a fact.** A `correct` preserves the fact's subject/`allow`/sender
  (it touches claim text only); an `add` carries its **own** ACL under the same
  rules as a captured message — the interpreter decides the `subject` (who the
  fact is about) and `allow` (audience) from the comment, the page's wiki
  `scope`, and the commenter's group scopes, defaulting to `user:<commenter>` /
  `[]`, with `sender` = the human who left the comment (`author_sender_id`). It
  is **never an arbitrary existing fact's subject** (a standard page can hold
  facts from several principals — the Cartografo homes by topic, not by
  subject — one of which may be broader). When the comment has no recorded
  author **and** the LLM emits no `subject_id`, the `add` falls back to the
  wiki's scope principal — never inventing a sender.

The application is wired into the **REM full cycle** (the batched, nightly-or-
admin-triggered dream), not the frequent light dream — so comments accumulate and
are read together, and the strong-tier interpretation runs on a pass that would
run anyway. It upgrades the briefing-processor sub-job from
[mark-passive to action-taking](rem-cycle.md) for standard wikis only; smart-wiki
and structured wikis are untouched. An unparseable / failed page is soft-skipped
(its comments wait for the next cycle).

## Build status

The narrative compiler is fully built — the planner, the prose compiler, the
reviewer, the REM cadence wiring, and human-edit handling:

| Stage | What it adds | Status |
|---|---|---|
| **compilation planner** | Builds the `CompilationPlan` (foundation + classification + dedup + assembly), persisted under `wikis/_plan/`, with the incremental dirty set. | **landed** (this page) |
| **compiler (Cronista + Record Writer)** | Consumes the plan and writes the published `.md` pages — prose pages via the strong Cronista (facts → prose + `{{… f=}}` markers), `lista` pages via the no-LLM [Record Writer](#the-record-writer--lista-pages-no-llm) (atomic records); repoints `fact_index` off the *no page yet* address onto the compiled page; reindex skips standard pages. | **landed** (this page) |
| **deterministic reviewer** | Post-compile QA over the plan + written pages: empty leaves, duplicate fact homes, asymmetric links, cross-page prose duplication, the missing-ACL-marker leak guard, and the cross-subject-bloat identity-index check. Non-blocking (`crate::reviewer`). | **landed** ([above](#the-reviewer)) |
| **cadence wiring** | Wires the compile pass into both REM cadences (light = dirty sync after promotion, REM night = full reorg then recompile), sharing the LLM bag; `mwe-mcp rem run-compile` CLI hatch. | **landed** ([above](#the-cadence--wired-into-rem)) |
| **human edits via comments** | Parked dashboard comments on standard pages are applied by the REM dream as contained, ACL-safe fact ops (`mwe_core::comment_apply`); the fingerprint folds fact content so an in-place correction recompiles only its page. | **landed** ([above](#human-edits-on-compiled-pages)) |
