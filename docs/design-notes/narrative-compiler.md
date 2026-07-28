---
title: Narrative compiler — planner + the Cronista, Hub Writer & Record Writer
area: design-notes
status: partial
last_review: "2026-07-05"
---

# Narrative compiler

[`mwe-core::planner`](../../crates/mwe-core/src/planner.rs) is the
**topology stage** of the narrative compiler. It turns the flat
fact store — [`fact_index`](capture-and-dedup.md), fed by the
[light dream](narrative-buffer.md#promotion--the-light-dream) — into a
[`CompilationPlan`](../../crates/mwe-core/src/planner.rs): a hub→leaf page
graph in which every fact lives on **exactly one** page (the
one-fact-one-page invariant), hubs hold only narrative + links, and a
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
`group_theme` hub (slug = the group id, scope prose carried on
`owner_scope`); for each enrolled user one `person` page (slug = the user
id), wiring `parent_hub` to the user's **first** known group and
`outgoing_links` to all of them. Groups are built first so a person can
link to them. From the **tree** (`seed_topic_wiki_indexes`): every
standard non-identity wiki — the sub-wikis `pages_to_subwiki` mints,
any hand-forged topic wiki — gets its `index.md` as an
`emerged_index` node (slug = `slugify(wiki_id)`, `parent_hub` = the parent
wiki's foundation slug when the graph has it, description = the `_meta`
`scope` prose). A topic container carries **no identity semantics**: its
subject is a topic — a person, a pet, a project — never a user (maintainer
2026-07-05); smart wikis and identity wikis never qualify.

These foundation pages map directly onto mwe-mcp's **wiki roots**: a
`person` page is the user's `wiki-user` wiki, a `group_theme` hub is the
group's `wiki-group` wiki, an `emerged_index` is a topic wiki's front
page. They are **never garbage-collected**
([`PageType::is_foundation`](../../crates/mwe-core/src/planner.rs)) — an
enrolled user always has a page even with zero facts, and an emerged index
survives its facts moving down onto sub-pages. A slug collision is skipped
with a warning (enrollment wins over a topic wiki; the group wins over a
person), so the graph never has two pages on one slug. Because the
`emerged_index` slug is exactly the slug the pre-existing content leaf of
an old emergence carried, the foundation node **takes the slug over** at
the first build: the carried facts re-attach to the wiki's `index.md` and
the shadowed registry entry is GC'd — the topic converges to one front
page, and the oversized nomination later hands the pile to the Cartografo
to split by content.

### Stage 1 — the Cartografo (strong-model classification)

[`classify_facts`](../../crates/mwe-core/src/planner.rs) is the first LLM
stage. It runs on a **strong** model — the structural-judgment tier, a
config slot distinct from the 9B workhorse; see
[the strong-model tier](#the-strong-model-tier) below — in batches
(`CARTOGRAFO_BATCH` facts per call), **grouped by source wiki before
they are chunked** (`cartografo_batches`), so a batch never straddles two
wikis. That order is what lets one language directive be true for the
whole batch: this stage coins page titles and descriptions a person
reads. Only the batch composition narrows — the model is still shown
every foundation and concept page of the whole forest, so a fact can
still be assigned to a page that lives elsewhere. For each fact it returns the **one**
page slug the fact belongs on, and it may propose emergent `concept_hub` /
`concept_leaf` pages when a theme warrants its own page. The prompt
([`crates/mwe-core/prompts/cartografo.md`](../../crates/mwe-core/prompts/cartografo.md))
is handed the foundation pages and the existing concept pages (from
the registry plus any proposed earlier this run) so the model **reuses**
an existing page rather than minting a duplicate.

The engine enriches that context with **structural signals**
([`CartografoSignals`](../../crates/mwe-core/src/planner.rs)) — information
the model weighs; no ownership or count gate exists in Rust:

- **Identity-page scope, per fact.** Every fact line carries an
  `identity_pages=` tag: the `person` pages the fact's *subject* covers —
  the owner user's own page; for a group-owned fact the member users' pages,
  expanded from enrollment by
  [`subject_scopes_for`](../../crates/mwe-core/src/planner.rs)
  (`enrollment::members_for`); `any` for the builtin global group (world
  context is never a foreign subject); `none` for a group with no enrolled
  members. The prompt's **identity-page discipline** reads the tag: an
  identity index (a `person` page — a `wiki-user`'s `index.md`, the agent
  wiki included) carries **one subject** and never takes a
  **foreign-subject** fact (owner = a different user, or a group the page's
  user is not a member of — a group they belong to is their own shared
  context, never foreign). The foreign detail is homed on the subject's own
  pages, split by content; the relation surfaces on the identity index only
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
- **Container shape, per page.** A concept-page line whose slug other pages
  parent under carries `children: N` (registry `parent_hub` back-references,
  computed in `describe_concepts`). The prompt's **container rule** reads
  it: a page with children functions as a hub — facts go on the matching
  child or a proposed new child leaf, never on the container itself, even
  when its line still says `concept_leaf` (that is a fact-bearing container
  being drained so the assembly can settle it into its real hub role).

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

**Cadence — the Cartografo is REM-only too.** Like the Conciliatore below, the
strong-model Cartografo runs only in
the **full** cadence. The frequent **light dream** (`Cadence::Light`) does
**not** call it: new facts are placed **deterministically, with no LLM**, onto
the page the **ingest classifier already proposed** (`fact_index.target_page`)
by [`ingest_placement_blueprint`](../../crates/mwe-core/src/planner.rs) — a
`concept_leaf` per distinct target slug (a path like `recipes/dinner.md`
flattens to one leaf; the light path does not nest), seeded with the ingest-proposed
`style` + `page_description` so the page gets a testata. A fact whose target is
`index.md` / empty falls through to the deterministic orphan fallback (its
foundation page), never a page named "index". **A `high`-salience fact
(`fact_index.salience`) is routed the same way regardless
of its target_page**: `ingest_placement_blueprint` leaves it unassigned so the
orphan fallback homes it on the actor-wiki's foundation page — whose `page_path`
*is* `index.md`, the always-on **base context**. The routing *is* the
reservation: an always-on fact (identity, health/safety, hard standing
constraints) overrides whatever theme page the classifier proposed. `build_wiki_plan` selects between
the two paths via the [`NewFactPlacement`](../../crates/mwe-core/src/planner.rs)
enum (`Ingest` for light, `Cartografo` for full, `OrphanFallback` for a Full
pass with no strong slot). So a fact is **catalogued at ingest and laid down
cheaply by the light dream**, then the nightly REM re-runs the strong Cartografo
to re-home and reorganise.

### Stage 1.5 — the Conciliatore (strong-model dedup)

[`conciliate_new_pages`](../../crates/mwe-core/src/planner.rs) is a
strong-model call **per prospective wiki** that folds
**semantically-duplicate proposed pages** into existing ones. The
Cartografo, working batch by batch, cannot see the whole proposed set at
once; the Conciliatore does — it gets all foundation + registry pages and
every page proposed this run for that wiki, and returns a `redirects` map
(`proposed_slug → existing_slug`) plus the genuinely-new `accepted_new`
list.

A proposal's prospective wiki is the source wiki of the first fact
assigned to it (`conciliatore_groups`, the same rule `slug_source_wiki`
applies one stage later); a proposal no assignment claims rides its own
group and is homed or dropped by the plan builder as before. The split is
what gives the stage a language: it picks which title and description
survive a merge, and those are read by a person. **What each call sees
does not narrow** — `describe_existing` is computed once, outside the
loop, and every group is shown the whole forest, so a proposal can still
be folded into a page that lives in another wiki exactly as before. The
prompt
([`crates/mwe-core/prompts/conciliatore.md`](../../crates/mwe-core/prompts/conciliatore.md))
carries a **redirect bias**: when in doubt, consolidate — fewer
well-populated pages beat many scattered ones.

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
   has a home.
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
   naming no plan page (the pointer an absorbed or GC'd hub leaves on its
   children) is re-pointed to the page's own wiki foundation page when the
   plan has one, else cleared — on the plan page **and** the registry
   entry, or the pointer would resurrect next build. Then every page with
   a `parent_hub` is registered as a child of that hub; child lists are
   sorted.
7. **Fixpoint garbage-collection** of empty concept pages — see the fix
   below.
8. **Link graph**: build the directed adjacency (hub→child + foundation
   outgoing), make it **symmetric** (every edge gets its inverse), sort,
   and sync `outgoing_links` / `incoming_links` back onto each page.
9. **Compilation order**: hubs → persons → leaves
   ([`PageType::order_rank`](../../crates/mwe-core/src/planner.rs)), ties
   broken by slug, so a hub is always written after its children are
   placed.

Two of these steps deserve calling out:

- **Deterministic orphan homing** (step 4). A fact the Cartografo never
  assigned (or whose batch was skipped) is homed by
  [`orphan_target`](../../crates/mwe-core/src/planner.rs): the **owner's**
  `person` / `group_theme` page if it exists, else the fact's **source
  wiki's** foundation page, else dropped from the plan with a warning —
  **never an arbitrary page**. The home is a function of the
  fact's own owner and provenance, so the same orphan lands the same place
  every run. (A concept *page* the Cartografo leaves without a resolvable
  `parent_hub` — typically a `global` fact's page — is homed by
  [`resolve_page_wiki`](../../crates/mwe-core/src/planner.rs) in **its facts'
  source wiki**, never a root: a `global` fact captured from frodo compiles into
  `wikis/frodo/…` with an `{{owner=global …}}` marker. The facts decide the
  page's wiki, which keeps `fact_index.wiki_id` and the compiled `source_path` in
  the same wiki — mwe-mcp's tree is a **forest** of top-level wikis with no
  materialised root.)
- **Fixpoint GC** (step 7). Empty concept pages are removed
  (`concept_leaf` with no facts, `concept_hub` with no children) — but in a
  **loop until no removals**, not a single pass. A single pass would leave a
  stranded hub whose only child was an empty leaf removed *in that same
  pass*; the fixpoint catches the cascade — remove the empty leaf, the hub
  becomes childless, remove the hub too. Each iteration first
  **normalises** the shape the sweep must not eat: an emptied leaf that
  other pages still parent under **flips to `concept_hub`** (plan page +
  registry entry) instead of being removed — removal would orphan every
  child's `parent_hub` (the dangling-pointer factory step 6 heals after) —
  which is how a fact-bearing container drained by a placement re-open
  settles into its real hub role; the type flip alone marks the page dirty
  (`compute_dirty_pages` compares the type beside the fingerprint, since
  hubs render through a different writer). Foundation pages are exempt.
  Each removal is recorded in `merged_pages` for audit and dropped from the
  registry. The GC's **on-disk half** runs at compile time:
  [`sweep_orphan_page_files`](../../crates/mwe-core/src/compiler.rs) (tail of
  `compile_dirty_pages`) deletes a concept-page **file** the plan no longer
  references — a live-write page whose fact the Conciliatore re-routed, or a
  leaf whose facts all moved away, otherwise survives as a zombie the recall
  navigator keeps reading. Three guards: never a plan page, never a reserved
  name (`index.md`, `rules.md`, `_`-prefixed), and never a file ANY
  non-tombstoned `fact_index` row still points at (the DB-first rule — a
  pending render or a superseded row's audit marker keeps the file). All
  soft; the count lands in `CompileReport.orphan_files_swept`.

## The data model

All types live in [`planner.rs`](../../crates/mwe-core/src/planner.rs);
the definitions there are the SSOT.

- **[`PageType`]** — the kinds the topology distinguishes: `Person`,
  `GroupTheme` and `EmergedIndex` (foundation, never GC'd — the third is a
  topic wiki's front page, no identity semantics) and `ConceptHub` /
  `ConceptLeaf` (emergent, GC-eligible). A hub holds no facts (links only);
  a leaf holds facts and has a parent hub; an emerged index holds facts
  like a leaf while the topic is small and renders as the hub overview
  once its facts have moved onto children.
- **[`FactForPage`]** — a fact materialised onto a page. It carries the
  verbatim claim text, the classifier's `fact_type`, the full ACL triple
  (`owner` / `allow` / `sender`), the `source_wiki_id`, the optional
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
  `description`, `page_type`, optional `owner_scope` (group prose) and
  `parent_hub`, `child_leaves`, `primary_facts`, the symmetric
  `outgoing_links` / `incoming_links`, and the page's **tree home** —
  `wiki_id` + `page_path` (foundation hubs and `concept_hub`s use
  `index.md`; a `concept_leaf` uses `<slug>.md`).
- **[`CompilationPlan`]** — the persisted artifact: `pages` (keyed by slug
  in a `BTreeMap`, sorted for determinism), `merged_pages` (the GC/redirect
  audit), `link_graph`, `compilation_order`, `generated_at`, `fact_count`,
  and `dirty_pages` (the recompile set).
- **[`ConceptRegistry`]** / **[`ConceptRegistryEntry`]** — the persistent
  record of emergent concept pages, so a hub/leaf minted one night is
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
no new facts** when a link, a parent, or a child changes — the Cronista
must rewrite a hub when its child set shifts, even though the hub holds no
facts itself. And because it folds in each fact's **render content** — the
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
nightly compile cheap: the Cronista regenerates only the dirty set, not the
whole tree.

## Incremental orchestration

[`build_wiki_plan`](../../crates/mwe-core/src/planner.rs) wires the stages
together and adds the incremental bookkeeping:

- It gathers every active fact across the **narrative** wikis (every wiki
  whose `_meta.md` smart flag is false — "narrative" = "not
  smart", read per-wiki) in
  fact-id order, builds the foundation, and loads the prior plan. It **skips
  facts on the reserved channel pages** — `rules.md` and `projects.md`
  (`wiki::is_channel_page`). Those belong to their own channels and are read
  keyed on that path: behaviour rules by `recall_behaviour_rules`, project
  signposts by the recall slot that opens their project
  ([smart-wikis.md](smart-wikis.md)). Re-homing one onto `index.md` would
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
`<slug>.md` concept-leaf form for splits/merges, or the **`wiki_index` form**
for the emergence, whose destination is the emerged wiki's `index.md` — slug =
`slugify(wiki_id)`, path pinned to `index.md`) — and drops any husk page a
merge or an emergence removed (plan + registry, audited in `merged_pages`).
Because after the edit
the carried-over fingerprint *matches* the next build, the touched slugs are
parked on the plan's **`force_dirty`** list: `build_wiki_plan` unions them into
the dirty set (on the early-skip path they *are* the dirty set) and clears the
flag, so the destination page gets woven by the Cronista exactly once — for an
emergence that first weave is what turns the verbatim-copied index into real
compiled prose. The
shared row→plan projection ([`FactForPage::from_row`](../../crates/mwe-core/src/planner.rs))
guarantees a re-homed fact fingerprints identically to a gathered one — no
permanent dirty churn.

The seam's seeded page is a **bridge only**: at the next plan build the
Fonditore's topic-wiki pass owns the slug with an `emerged_index`
foundation node (path pinned to `index.md`, never garbage-collected), the
staleness GC drops the seam's transitional registry entry, and the carried
facts follow the slug onto the foundation node — so a registry round-trip
can never strand the emerged content on a `<slug>.md` sibling file again
(the registry stores no `page_path`; before the foundation pass, one
rebuild was enough to drift the index content onto a file named after the
slug).

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
`.md` files, so the `.json` artifacts are ignored — the same reasoning
that keeps `_captures.md` out of the index, applied to a sibling cache.

## The confirmed mwe-mcp tree model

How the abstract page graph lands on mwe-mcp's wiki tree was confirmed with
the maintainer (2026-05-31):

- **Foundation pages are the identity wikis.** A `person` page *is* the
  user's `wiki-user` wiki; a `group_theme` hub *is* the group's
  `wiki-group` wiki (its `index.md` is the hub).
- **A `concept_leaf` is a `.md` page** within the relevant standard wiki;
  a **`concept_hub` is an `index.md`** hub. A routine emergent concept page
  is *content the Cronista writes* — a new `.md` inside an existing wiki —
  and therefore needs **no `structure_proposal`**.
- **Escalation** of a grown concept page into a dedicated **sub-wiki**
  reuses the **existing** [`wiki_promote` / `pages_to_subwiki`](proposal-apply-engine.md)
  machinery — the REM auto-promote sub-job ([rem-cycle.md](rem-cycle.md)),
  which is already proposal-gated. The planner does not reinvent promotion.

The consequence: **the planner adds no new proposal kind.** Routine concept-page
creation is prose the Cronista emits; the only gated structural action is
the sub-wiki escalation, and that already exists. (The
[`resolve_page_wiki`](../../crates/mwe-core/src/planner.rs) helper homes a new
concept page in **its facts' source wiki** (a factless hub falls back to its
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
cadence passes `NewFactPlacement::Ingest` deliberately — the strong Cartografo
is REM-only — while the Conciliatore runs at **both** cadences on the cadence's
tier (see [Stage 1](#stage-1--the-cartografo-strong-model-classification) /
[Stage 1.5](#stage-15--the-conciliatore-strong-model-dedup)).

**Tier per cadence — "the strong model works ONLY at
REM".** The strong tier above applies to the [`Cadence::Full`](../../crates/mwe-core/src/dream.rs)
compile (nightly REM + operator-driven compiles). The frequent, cheap
**light dream** (`Cadence::Light`) does **not** run the Cartografo at all
(placement is the deterministic [ingest-hint path](#stage-1--the-cartografo-strong-model-classification),
`NewFactPlacement::Ingest`); the remaining LLM stages — Conciliatore, Cronista,
Hub Writer — run on the cheap **ingest-tier (Flash)** backend (the same model
the classifier runs on; it reaches `run_compile` as the bag's `apply` slot),
falling back to the strong slot only when no `ingest` slot is configured. So a light dream never
touches the Pro tier; the nightly REM recompiles the same pages at full quality.
No new operator config — the slots already exist (`ingest` = Flash, `cronista` /
`rem_promotions` = strong, `hub_writer` = workhorse — see
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
empty hub/leaf chain, the fingerprint reacting to a link change, the
Cartografo blueprint parse + new-page dedup, the changed/new/removed dirty
set, and the end-to-end `build_wiki_plan` incremental-idempotency
(first build homes the fact, an unchanged second build yields zero dirty
pages).

## The compiler — Il Cronista + the Hub Writer

[`mwe-core::compiler`](../../crates/mwe-core/src/compiler.rs) is the **prose
stage**: it consumes the [`CompilationPlan`](../../crates/mwe-core/src/planner.rs)
and turns each fact into the markdown a reader (and recall) sees. The planner
decided *where each fact lives*; the compiler decides *how it reads*.
[`compile_dirty_pages`](../../crates/mwe-core/src/compiler.rs) walks only the
[dirty set](#page_fingerprint--the-dirty-set) — a removed page (present in
`dirty_pages` but gone from `pages`) is skipped here; its on-disk file is
deleted by the [orphan-file sweep](#stage-2--the-architetto-deterministic-assembly)
at the tail of the same compile — and returns a `CompileReport`
(leaves / `lists` / hubs / `unchanged` / `degraded` / per-page soft errors).
Per-page LLM or parse failures are collected into the report and the run
continues — a leaf whose Cronista keeps failing lands in the
[degraded guard-only rewrite](#degraded-mode--the-guard-only-rewrite) rather
than freezing — and only infrastructure failures (DB, filesystem) bubble.
Every failed or degraded page also feeds the
[per-page failure ledger](rem-cycle.md#per-page-compile-failure-surfacing)
that surfaces persistent failures to the operator.

### The dispatcher — hub vs `lista` vs prose

[`compile_page`](../../crates/mwe-core/src/compiler.rs) routes each page in two
steps:

1. A page is a **hub** when it has **zero facts**, **at least one child**, and a
   `page_type` of `concept_hub`, `group_theme` or `emerged_index` → the
   **Hub Writer** (the emerged index rides both arms: prose while it still
   carries facts, hub overview once they moved onto its children).
2. Otherwise, a leaf whose ingest-decided `style` (`page.style`) is
   **`lista`** → the **Record Writer** (atomic records, no LLM); a leaf with
   **no facts at all** (a foundation page whose facts have not arrived yet —
   empty *concept* leaves never get here, the planner GCs them) →
   `compile_empty_leaf`, a deterministic minimal render (testata + the
   description one-liner, **no LLM**: handed an empty fact list the Cronista
   invents colour prose from the wikilinks alone — the dogfood re-run compiled
   Tolkien lore onto a zero-fact identity index); everything else
   — prose leaves, and a `person` / `emerged_index` page carrying facts —
   goes to **Il Cronista**.

The three writers target different config slots (the Cronista on the strong
tier, the Hub Writer on the cheap tier, the Record Writer on **no** model at
all), so each kind of work is independently tunable and `lista` data never pays
for prose synthesis.

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
  `owner`/`allow`/`sender` are withheld — but a fact whose read audience is
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
  prompt carries the rule that pays for it: never link a page to itself;
- the recommended outgoing `[[wikilinks]]` from the plan's link graph;
- the wiki's prose tone (resolved by matching the wiki's actor kind — the
  bare `wiki_type` string — in `resolve_tone`, cached per wiki within a run).

Every link the compiler feeds a prose-writing prompt is rendered by
[`plan_page_wikilink`](../../crates/mwe-core/src/compiler.rs) in the
**canonical grammar** ([recall-pipeline.md §Link grammar](recall-pipeline.md#link-grammar)):
`[[wiki_id/page-slug]]` for a page, collapsing to the bare `[[wiki_id]]` wiki
hop for a wiki's own `index.md` — never a bare plan slug, which would read as
a hop to a wiki that does not exist. The prompt's counterpart rule is
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
It writes **no** ACL, owner, `allow`, `sender`, braces, or `fact_id` — only the
span boundary. The unmarked connective prose between tags inherits the page's
default visibility. It returns one JSON object (`mergedBody`, `description`,
`style`); the `description` is the page's one-liner, and for a wiki's
`index.md` overview page it becomes the wiki's **abstract** (see
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

The **on-disk runtime marker format is the bare** `{{f=…}}` — only the
Cronista's transient output uses `<fN>` tags; the parser, the capture path, and
every other prompt that references the marker share that one format (the full
`{{owner=… allow=… sender=… f=…}}` form is the export/interchange serialization
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
| From the marker on | this page's title / slug / hub / tone, its facts, its recommended links | the **user** turn, closed by the write instruction |

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
> `valid_from` / `valid_to` on the capture (mirrored in the `_captures.md` journal
> as `vf` / `vt`), and [`promote_one`](../../crates/mwe-core/src/dream_light.rs)
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
  coerces the value into the palette; absent / unrecognised → `prosa`. A **hub** is
  an overview/navigation page, always `prosa` (it holds no facts → takes no ingest
  style).
- **`description`** is the page's «what goes in here» one-liner — for a leaf the
  Cronista's fresh `description`, for a hub the plan's
  [`PagePlan.description`](../../crates/mwe-core/src/planner.rs). Besides the testata
  it also feeds the [abstract sync](#the-abstract-sync--the-wikis-summary) for an
  `index.md` overview page. It serves both **recall** (orient before opening) and
  **placement** (where to file a new fact).

[`render_page_file`](../../crates/mwe-core/src/compiler.rs) writes both into the
frontmatter (after `page_type`; `description` is omitted when empty, quotes /
newlines flattened). The `style` tag records the page's **dominant**
read-strategy, and it matches the body: a `lista` testata
sits over a [record body](#the-record-writer--lista-pages-no-llm), never prose.

### The Hub Writer — the overview (cheap model)

[`compile_hub_page`](../../crates/mwe-core/src/compiler.rs) writes a hub's
overview on the cheap `HubWriter` slot. Rather than a new prompt it **reuses the
existing [`regenerate-index`](../../crates/mwe-core/prompts/regenerate-index.md)
prompt** — the same one a normal `index.md` write already uses — fed from the
plan's children (each child as its canonical `plan_page_wikilink` +
`description`; the REM regenerator consumer feeds child *wikis* as
`[[wiki_id]]` hops instead). It emits raw markdown that cites **every** child
as a canonical `[[wikilink]]` (the prompt says copy-verbatim) and carries
**no ACL markers** — a hub holds no facts, so there is nothing to mark or
repoint.

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
line while `fact_index.text` stays the canonical claim) and, for an `index.md`
overview, syncs the `_meta` abstract. The outcome is counted as a `lists` page in
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

### The abstract sync — the wiki's `summary`

When the compiler (re)writes a wiki's **`index.md` overview page** — its
foundation `person` or `group_theme` page; concept pages use `<slug>.md` and are
skipped — it persists a one-line **abstract** into that wiki's `_meta.md`
(`extra["summary"]`) via
[`meta_annotate::sync_wiki_summary`](../../crates/mwe-core/src/meta_annotate.rs).
The source is the freshest one-liner available: a **person** wiki uses Il
Cronista's `description` (an LLM summary of the page it just wrote — rich); a
**hub** or **`lista`** wiki uses the plan's
[`PagePlan.description`](../../crates/mwe-core/src/planner.rs) (the Hub Writer
emits prose, not a one-liner, and the Record Writer has no LLM to author one).
The write is **best-effort** (a
`_meta` hiccup is logged, never fails the page) and **idempotent** (rewritten
only when the abstract changed; the `_meta` prose body is preserved), so it
refreshes exactly when the overview page is recompiled.

This is the LLM-authored companion to the deterministic
[topic-keyword sync](#keyword-sync--fact-topics-into-_meta-and-the-page-testate-recall-navigation):
together they fill the per-wiki `summary` + `keywords` the catalog
(`wiki_catalog_list[_for]`) and the rendered **root index** surface, so a recall
navigator can pick a branch from the abstract without opening the wiki
([recall pipeline](recall-pipeline.md#entry-point-gathering--recall_nav-navigation-phase-1)).

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
passage* (offsets now point into the published page, not the
[`_captures.md` journal](narrative-buffer.md) the fact was promoted from), while
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
the marker reindex). It still rebuilds their captures buffer from the
`_captures.md` journal — only the per-page marker reindex is skipped. Without this exclusion a reindex would parse the compiled
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
only facts at the wiki's default visibility (owner `global` or the resolved
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
against, and what the catalog (`wiki_catalog_list` / `wiki_catalog_list_for`) and
the rendered **root index** (`wiki::render_root_index`) surface so a navigator can
orient itself before descending into prose; the page-level entries are the
per-page cards the future intra-wiki hops read. It is the **deterministic floor**
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

- **empty leaf** — a `concept_leaf` with zero facts (the Architetto's
  fixpoint GC should have removed it; a hit is a planner regression).
- **duplicate fact home** — a `fact_id` on two or more pages (a
  one-fact-one-page violation).
- **asymmetric link** — an `a → b` edge with no `b → a` back-edge (the
  Architetto makes the graph symmetric, so any asymmetry is a regression).
- **duplicate prose** — two leaf pages whose stripped bodies share a
  char-6-gram Jaccard ≥ `PROSE_DUP_THRESHOLD` (reuses `recall::jaccard_6gram`;
  hubs excluded). The starvation invariant should make this near-zero, so a
  hit means a leaf leaked another's content — or that two near-synonym pages
  cover one concept: these pairs feed the
  [REM page-merge sub-job](rem-cycle.md#page-merge-sub-job-semantic-page-consolidation)
  as merge candidates.
- **missing ACL marker** — an owned fact (owner ≠ `global`) on a page whose
  compiled body carries no non-public `{{… f=<fact_id>}}` marker for it. This
  is the **ACL-leak guard**: an owned claim rendered as
  unmarked prose would be world-readable. Since the Cronista's **forward
  completeness guard** appends, at write time, any fact it omitted (with its
  full marker), this should be near-zero in practice; the reviewer remains the
  independent backstop.
- **cross-subject bloat** — an identity index whose **plan** carries a
  foreign-subject fact: owner is a different user, or a group the page's
  user is not a member of (a group they belong to is their own shared
  context, never foreign; global never qualifies). Identity-index detection
  reads the `_meta` `wiki_type` — the page is a `wiki-user`'s `index.md`
  (the agent wiki included; group wikis and emergent sub-wiki indexes never
  qualify) — via the enrollment-fed
  [`IdentityContext`](../../crates/mwe-core/src/reviewer.rs), which
  `dream::run_compile` loads best-effort (a load failure only disables this
  one check). This is the observability half of the Cartografo's
  [identity-page discipline](#stage-1--the-cartografo-strong-model-classification)
  — a count in the report/log, never a gate. The check is plan-level on
  purpose: plan placement is the load-bearing channel (the Cronista renders
  only the facts the plan gives a page), so a plan-clean identity index
  converges to a clean disk page at its next compile.
- **leaf with children** — a `concept_leaf` other pages parent under: the
  two-rank topology violated by a fact-bearing container (the *empty*
  container case never reaches the reviewer — the Architetto's GC
  normalises it to hub). Parked as a placement re-open so the Cartografo
  re-homes its facts; once drained, the flip settles it.
- **hub with facts** — a `concept_hub` / `group_theme` carrying facts
  (hubs hold no facts; the shape a degraded orphan-fallback build or an
  old plan can leave). Parked as a placement re-open.
- **oversized page** — a fact-bearing page at/over
  `OVERSIZED_PAGE_THRESHOLD` (a nomination constant beside
  `PROSE_DUP_THRESHOLD`, tunable in code). Mass alone re-opened nothing
  before this check, so a **subject-clean grown page could never split**
  (bloat and the compile-failure streak were the only re-open sources) —
  and the refile sweep's deliberate land-on-`index.md` had no
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
  (`leaf_with_children`, `hub_with_facts`), each `oversized` page, plus
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
on stale ingest `target_page` hints / the owner's foundation page —
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

The compile pass is skipped when the `cronista` slot is unconfigured (the light
dream still promotes; it just cannot write prose). The two LLM stages map onto
existing strong-tier REM slots: Cartografo → `rem_promotions`, Conciliatore →
`rem_dedup_semantic`, Cronista → `cronista`, Hub Writer → `hub_writer`. The CLI
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
- **A fact is a fact.** A `correct` preserves the fact's owner/`allow`/sender
  (it touches claim text only); an `add` carries its **own** ACL under the same
  rules as a captured message — the interpreter decides `owner` (subject) and
  `allow` (audience) from the comment, the page's wiki `scope`, and the
  commenter's group scopes, defaulting to `user:<commenter>` / `[]`, with
  `sender` = the human who left the comment (`author_sender_id`). It is **never
  an arbitrary existing fact's owner** (a standard page can hold facts from
  several principals — the Cartografo homes by topic, not by owner — one of
  which may be broader). When the comment has no recorded author **and** the LLM
  emits no `owner_id`, the `add` falls back to the wiki's scope principal —
  never inventing a sender.

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
| **compiler (Cronista + Hub Writer + Record Writer)** | Consumes the plan and writes the published `.md` pages — prose leaves via the strong Cronista (facts → prose + `{{… f=}}` markers), `lista` leaves via the no-LLM [Record Writer](#the-record-writer--lista-pages-no-llm) (atomic records), hubs via the cheap Hub Writer; repoints `fact_index` off `_captures.md` onto the compiled page; reindex skips standard pages. | **landed** (this page) |
| **deterministic reviewer** | Post-compile QA over the plan + written pages: empty leaves, duplicate fact homes, asymmetric links, cross-page prose duplication, the missing-ACL-marker leak guard, and the cross-subject-bloat identity-index check. Non-blocking (`crate::reviewer`). | **landed** ([above](#the-reviewer)) |
| **cadence wiring** | Wires the compile pass into both REM cadences (light = dirty sync after promotion, REM night = full reorg then recompile), sharing the LLM bag; `mwe-mcp rem run-compile` CLI hatch. | **landed** ([above](#the-cadence--wired-into-rem)) |
| **human edits via comments** | Parked dashboard comments on standard pages are applied by the REM dream as contained, ACL-safe fact ops (`mwe_core::comment_apply`); the fingerprint folds fact content so an in-place correction recompiles only its page. | **landed** ([above](#human-edits-on-compiled-pages)) |
