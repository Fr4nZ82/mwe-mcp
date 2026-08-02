---
title: Recall pipeline — the read-side orchestrators and the entry-point gatherer
area: design-notes
status: implemented
last_review: "2026-08-01"
---

# Recall pipeline

[`mwe-core::recall`](../../crates/mwe-core/src/recall.rs) hosts the
read-side orchestrators that complement [capture](capture-and-dedup.md).
The module is layered: pure helpers at the top (n-gram + jaccard +
cosine) and the async orchestrators at the bottom.

## The shape of a turn — where each knob acts

Two diagrams, because the single most common misreading is that the recalled
facts decide the navigator's doors. They do — **on the first hop only**.

```mermaid
flowchart TB
    T["the user's turn"] --> RAG

    subgraph P1["1 · flat recall — always on the FACT corpus"]
        RAG["wiki_search over fact_index<br/>cosine over every candidate vector"]
        RAG --> TOPK["take top_k<br/><b>recall_top_k</b>"]
    end

    TOPK --> CLS["2 · classifier<br/>intent + topics + owners"]
    CLS -->|"capture / recall / disambig"| FAN
    CLS -->|"skip / structural"| BLOCK

    subgraph P3["3 · entry fan — deterministic, no LLM"]
        FAN["gather_entry_points"]
        S1["principal — sender wiki + groups<br/>weight 0.6, always present"] --> FAN
        S2["<b>rag</b> — each recalled fact mapped<br/>to its HOME PAGE via source_path<br/>weight = the hit's score"] --> FAN
        S3["topic — classified topics vs page cards<br/>0.6 wiki / 0.8 page"] --> FAN
        S4["situational — host strings<br/>0.4 / 0.5"] --> FAN
    end

    FAN --> NAV["4 · navigator funnel<br/>see below"]
    NAV --> BLOCK["5 · injected block<br/>WHO YOU ARE · WHO IS SPEAKING · HISTORY<br/>RELEVANT MEMORY · NAVIGATED PAGES · UPCOMING"]
```

The section corpus is **not** in this picture: a conversational turn never
scans it. Documentation reaches the turn only through the bounded project-docs
slot, and the whole-corpus `wiki_search` reaches it only behind
[the signpost funnel](#the-smart-corpus-funnel--a-project-opens-on-its-own-description).

### Inside the funnel — where the doors come from, hop by hop

```mermaid
flowchart TB
    FAN["entry fan<br/>(principal + rag + topic + situational)"] --> POOL0

    subgraph H0["hop 0"]
        POOL0["candidate pool = the fan<br/>typically 6-8 entries"]
        POOL0 --> PRUNE0["prune_pool → truncate to<br/><b>max_candidates</b>"]
        PRUNE0 --> LLM0["navigator LLM reads each card<br/>(keywords + one-line summary)<br/>answers open[] / done"]
        LLM0 --> OPEN0["Rust vets every pick against<br/>what was offered, reads the pages,<br/>projects them per-sender<br/>≤ <b>pages_per_hop</b>"]
    end

    OPEN0 --> GROW

    subgraph GROW["the pool GROWS — this is the part that surprises"]
        G1["entering a wiki adds<br/><b>ALL its pages</b> as candidates<br/>(measured: 22 and 44 on two real wikis)"]
        G2["links found in the opened prose<br/>add their destinations"]
    end

    GROW --> POOL1

    subgraph H1["hop 1 … up to max_hops"]
        POOL1["candidate pool = siblings + link targets<br/>often 40+ entries"]
        POOL1 --> PRUNE1["prune_pool → rank by tier<br/>(link > entry fan > sibling page),<br/>THEN truncate to <b>max_candidates</b>"]
        PRUNE1 --> LLM1["navigator LLM decides again"]
        LLM1 --> OPEN1["open, collect, grow the pool"]
    end

    OPEN1 --> STOP{"stop?"}
    STOP -->|"done — it judged it has enough"| OUT["collected prose"]
    STOP -->|"<b>max_hops</b> spent"| OUT
    STOP -->|"char_budget spent"| OUT
    STOP -->|"otherwise"| POOL1
```

**So the two knobs size different hops.** `recall_top_k` sizes the *fan*, i.e.
hop 0: more recalled facts means more distinct home pages to start from. From
hop 1 onward the doors are no longer the facts' pages at all — they are every
page of the wikis already entered, plus whatever the prose links to. That pool
is set by the corpus's own shape, not by `top_k`, which is why
`max_candidates` has to be sized against **how many pages a wiki has**, not
against how many facts were recalled.

## The two corpora

Recall reads **two** stores, and which one a caller gets is an explicit
choice made by calling a different function — there is no flag that can
be forgotten:

| Corpus | Table | What it holds | Entry point |
|---|---|---|---|
| **Facts** | `fact_index` | Standard-wiki memory: governed claims with per-fragment ACL, supersedence, validity, attribution | `wiki_search` (and everything built on it) |
| **Sections** | `wiki_sections` | Smart-wiki documentation: heading-delimited chunks of pages a smart consumer authored, ACL held per wiki in `smart_wikis` | `search_sections` |
| Both | — | facts always; a project's sections only behind the [signpost funnel](#the-smart-corpus-funnel--a-project-opens-on-its-own-description) | `search_all` |

`wiki_search` returns [`RecallHit`]s and **cannot** return
documentation — the two live in different tables, and `SectionHit` is a
different type. That is the point: the ingest turn, `recall_core_global`,
the recall gate and the eval harness all take the fact corpus, so
project documentation can no longer crowd out personal memory in a
conversational turn. Only the two consumer surfaces whose contract is
"search everything I can see" — `wiki_search` (MCP) and `wiki_navigate` —
reach for the merged view, and on `search_all` that view is itself gated:
see [the smart-corpus funnel](#the-smart-corpus-funnel--a-project-opens-on-its-own-description).

**Size is why the gating matters.** On the reference deployment the two
corpora are not comparable: 1 086 facts / 185 KB of text / 4.4 MB of
vectors against 4 907 sections / 4 713 KB / 20.1 MB. Documentation is
**96 % of the indexed characters**, so an ungated merged ranking spends
its budget — and its scan — there on every turn, whatever the turn is
about.

## The smart-corpus funnel — a project opens on its own description

[`recall::admitted_smart_wikis`](../../crates/mwe-core/src/recall.rs)
decides, **before any section vector is loaded**, which projects a
whole-corpus query may read. The contract is the founder's: *the project
corpus stays out of recall unless the turn is explicitly about a
project.* Two ways in, and nothing else:

1. **The turn names the project** — the same contiguous-token slug rule
   `recall_named_project_docs` uses, no floor. The turn declared its scope.
2. **The project's signpost description clears the floor** — one cosine
   against the one short authored line per project that
   [`signposts`](../../crates/mwe-core/src/signposts.rs) keeps on the
   owner's reserved `projects.md`.

So the decision costs a handful of dot products against authored lines,
not a scan of thousands of sections — and a turn that opens nothing never
touches the 20 MB of section vectors at all.

**It fails closed.** A project with no description, or whose description
the reader cannot see, is **not** admitted; naming it is the only way in.
That is what makes the description load-bearing rather than decorative.
The description is an ordinary `fact_index` row, so its **stored**
embedding is reused (no per-turn re-embed) and its per-fragment ACL is the
gate: a reader who cannot see a project's signpost cannot open its docs,
and cannot learn the project exists. A floor of **0** is the explicit
"funnel off" switch — it admits every readable smart wiki, including the
undescribed ones.

**Where the default comes from.** Measured on the production corpus over
24 probes — 16 ordinary personal turns that must open nothing, 8 project
turns that must open something:

| rule | personal turns wrongly opened | project turns caught |
|---|---|---|
| name only | 0 / 16 | 2 / 8 |
| description ≥ 0.35 | **7** / 16 | 7 / 8 |
| description ≥ 0.40 | 3 / 16 | 4 / 8 |
| **description ≥ 0.45** (`DEFAULT_SMART_CORPUS_FLOOR`) | **0** / 16 | 3 / 8 |
| description ≥ 0.50 | 0 / 16 | 3 / 8 |

No threshold separates the two groups cleanly — the same shape the
project-docs bench found for `DEFAULT_SIGNPOST_FLOOR`. The default is
therefore set where the *contract* points: precision first, at the
highest-recall value that opens nothing on an ordinary personal turn.

The project turns it misses are the ones phrased in a project's **internal
vocabulary** («il player Tizen non parte», «i contenuti sono fermi da 10
giorni») while the description is written in end-user language. That is a
property of the description, not of the threshold: a description that
names what its project is about lifts those turns over the floor. The
funnel is only as wide as the line somebody wrote. Tunable per deployment
as `recall.smart_corpus_floor` (operator panel, hot-reloaded).

End-to-end on the live corpus, 18 probes: **14 of 14 personal turns return
zero documentation** (they returned 95 of 140 slots' worth before), and 3
of 4 project turns open the right project.

### The project-docs slot — two entry points, at two different stages

Facts-only would leave a real gap: ask a **standard** consumer (Telegram,
hermes) *"come funziona questa cosa di AcmeSigns?"* and the engine would
have the answer indexed and never look at it. So the per-turn recall has
two narrow openings. They are **not** equivalent, and that is why they
run at different points of the turn:

| | trigger | what it is | when it runs |
|---|---|---|---|
| [`recall_named_project_docs`](../../crates/mwe-core/src/recall.rs) | the message **names** a project | an instruction — the turn declared its own scope | **before** the classifier, so the docs are in front of it when it decides the intent |
| [`recall_signposted_project_docs`](../../crates/mwe-core/src/recall.rs) | a **signpost** surfaced among the recalled facts | a guess — the memory noticed the project exists | **after** the classifier, gated by the judgement it returned |

1. **The name.** The message's tokens are matched against the `slug` of
   every smart wiki *the sender can read* (`smart_wikis.slug`, mirrored
   from `_meta.md`). No LLM, and no query embedding at all when nothing
   is named. No floor: the turn asked.
2. **The signpost.** A signpost is a fact on the owner's reserved
   `projects.md` ([`signposts`](../../crates/mwe-core/src/signposts.rs))
   saying a project exists; when one comes back in the turn's ordinary
   fact recall, that project *can* be opened. Whether it *should* be is
   the classifier's call — the `needs_project_docs` field of the JSON it
   already returns (see below). Read access is re-checked against
   `smart_wikis`, never inferred from the signpost. Costs one query, and
   only when a signpost actually surfaced.
3. **Selection — scoped, then ranked by both passes.** Only the candidate
   wikis' sections are ranked, which is why this beats a score threshold
   over everything: it answers from the project in play rather than from
   whatever scored well somewhere. Within that scope the cosine ranking is
   fused with exact-term search — see [the two
   passes](#the-section-corpus-is-ranked-by-two-passes-fused), including
   why the floor is applied *before* the fusion.
4. **Budget.** `project_docs_top_k` (default 3) and
   `project_docs_char_budget` (default 3 000) bound the slot across
   *both* stages — the second pass gets what the first left. Sections are
   kept **whole**: a hit that would overrun is dropped, never truncated,
   because half a section reads as a broken quote. The budget always
   admits its *first* hit whatever the size (an empty slot is worse), so
   the index-time section cap is what keeps one hit from starving the
   others — see [reindex-pipeline](reindex-pipeline.md).

#### Why the second gate is a judgement and not a threshold

It was built as a threshold first, and measured. `bge-m3` against the
real AcmeSigns corpus (2 112 sections at the current chunk policy), best
cosine per turn:

| turn | raw turn | distilled claim |
|---|---|---|
| «stasera ceniamo alle otto da mia sorella» | 0.427 | 0.490 |
| «cosa ho fatto di lavoro questa settimana?» | 0.494 | 0.525 |
| «domani alle 17:00 devo andare da questo cliente che ha il display che non funziona» — must **not** dig | **0.608** | **0.622** |
| «mi ha chiamato un cliente che dice che i contenuti sono fermi da 10 giorni» — must dig | 0.602 | 0.586 |
| «come fa acmesigns a inviare i contenuti ai display?» | 0.651 | 0.627 |

On a 17-sentence bench (6 must-dig, 5 must-not sharing the same
vocabulary — invoicing, a payment instalment, a bracket, a desk monitor)
**no similarity signal separated the two groups**: raw turn A [0.515,
0.602] vs B [0.515, 0.608]; distilled claim A [0.521, 0.586] vs B [0.523,
0.622]; topics only A [0.482, 0.612] vs B [0.539, 0.564]. Distilling the
claim first *compresses* the range (the dinner control climbs to 0.490)
and makes the worst case worse.

The reason is structural: the question is not *how similar is this text
to the docs* but *would reading the docs help answer this* — a
judgement, not a distance. Two sentences about a client and a screen sit
at the same distance from the corpus whether they concern a payment or a
fault. So the decision belongs to the model
(`[[feedback-no-hardcoded-gates-llm-decides]]`), and it costs **no extra
call**: `needs_project_docs` rides the JSON the classifier already
returns, next to `needs_disambig`.

`project_docs_signpost_floor` (default 0.55) survives underneath as a
cheap backstop — an unrelated turn scores 0.42–0.49, far below it — not
as the discriminator.

The judge was then verified rather than assumed: the same bench replayed
through the shipped rule, on the deployment's own workhorse model, with a
signpost planted in the recall block — **14 of 14 decidable cases
correct**, including the two that no similarity signal could tell apart.
Re-run it after editing that section of the prompt.

**Matching rule.** A slug matches as a **contiguous token sequence**,
never as a substring and never token-by-token. That is what keeps the
trigger safe on a compound slug: `cc-pc-lavoro` fires only on the whole
"cc pc lavoro", so an ordinary Italian message about *lavoro* does not
drag in a project's docs; and `acmesigns` never fires from inside a
longer word. Slugs shorter than
[`MIN_SLUG_MATCH_LEN`](../../crates/mwe-core/src/recall.rs) never
trigger.

**Only the slug — deliberately not the title.** Titles carry generic
words ("Claude (claude2)", "… engineering wiki"); matching those would
fire the trigger on ordinary conversation.

The hits render in their own labelled slot of the recall block
(`Project documentation (reference — never file this as a fact):`, see
[`ingest::format_snippet`](../../crates/mwe-core/src/ingest.rs)), and the
ingest prompt carries a matching REFERENCE, NOT MEMORY rule. Both halves
are load-bearing: an unlabelled documentation paragraph in the recall
block looks exactly like a recalled fact, and the classifier would file
it straight back as a fact about the sender.

**ACL is resolved once per wiki, not once per row.** `search_sections`
loads the `smart_wikis` registry (a handful of rows), keeps the wikis the
sender may read (`owner ∪ shared_with`, the same effective set
[`acl::can_read`] evaluates), and only then loads those wikis' sections.
An unreadable wiki's bytes never leave the DB. A sharing revoke is a
single-row write and closes the read window on the next query.

## The section corpus is ranked by two passes, fused

Sections — and only sections — are ranked by vector similarity **fused
with exact-term search**. Facts are not: they are short authored claims
with per-fragment ACLs, and giving them the same treatment is separate
work, not a wider `IN` clause.

**Why.** An identifier carries almost no meaning for an embedding to
encode, and identifiers are what a decision log, an ADR list, a ticket
trail and a stack trace are made of. Measured on the production corpus
before the index existed: the query `D-006` returned the section of
decision **D-001** (whose body merely cites the string), then an
unrelated changelog entry matching on `D-`, then another wiki's
`ADR-006` — never the section actually titled `D-006`. The same content
asked in prose returned that section first, at 0.68, across a language
boundary. The failure is not a tuning problem; it is the one thing
cosine cannot do.

**The lexical pass.** `wiki_sections_fts` is an FTS5 index over
`wiki_sections`, external-content (`content='wiki_sections'`) so the
bytes live in one place, maintained by three triggers in the schema
rather than by the Rust write path — so the reindex sweep, the boot-time
reconciliation and an operator's manual repair cannot bypass it. Free to
rebuild: 2.5 MB and 60 ms on the 4 220-section production corpus, with no
embedder involved.

- **Tokenization is plain `unicode61`.** It splits `D-006` into `d` and
  `006`, and the query is split the same way, so searching the identifier
  as a *phrase* matches only where those tokens are adjacent. Adding `-`
  to `tokenchars` would keep identifiers whole but weld `well-known` into
  a token a search for `known` could never reach.
- **Every query term is a quoted phrase joined by `OR`**
  ([`sections::lexical_query`]). Quoting makes a malformed expression
  unconstructible and makes a user typing `memory OR nothing` search for
  the words. `OR` because the result is a *ranking* input, not a filter:
  `AND` on a prose turn returns nothing.
- **The heading chain is its own weighted column.** `wiki_sections."text"`
  already begins with the heading, so indexing `heading_path` again
  counts a heading term twice — which is exactly the difference between
  the section that *is* `D-006` and one that *cites* it, and nothing else
  in the row expresses it. Measured on the telaiojs decision log
  (D-001…D-007, each split across 2–4 sections by the chunk cap): **in
  the lexical pass alone**, one column ranked the defining section first
  for **4 of 7** identifiers, two columns for **7 of 7**. The 4.0 weight
  then buys ranks 2 and 3, where it promotes the sibling pieces of the
  same decision over unrelated citations; 10.0 and 25.0 behave
  identically, so the value sits on a plateau. Prose queries do not move.
  *That result is about this pass, not about what a consumer receives* —
  see the definition tier below for what the fusion then does with it.

**Both passes always run.** A gate deciding per query whether the lexical
pass is "needed" would spend an LLM judgement to guard a sub-millisecond
index lookup, would discriminate on a *surface property of the query
string* rather than on the invisible intent
`[[feedback-no-hardcoded-gates-llm-decides]]` was written for, and would
drop the hit silently whenever it answered wrongly. The ranking decides
instead of a switch.

**Only the definition tier crosses the corpus boundary.** Inside the
section corpus both signals rank: the `OR`-joined ranking list and the
`AND`-on-heading definition set. On the **merged** list of `search_all`
only the definition set is applied, because a fact's handle is a
`fact_id` and can never key into a list of `source_path#ord` — so the
ranking bonus there is reachable by one corpus only. Its magnitude
settles the matter: `1/(60 + lexical_rank)` is larger than the entire
span of the vector-rank term `1/(60 + r)` over a list of at most
`2 · top_k`, so a section sharing **any** token with the query outranked
**every** fact whatever the cosines were. Measured on the production
corpus, that inverted 11 of 14 probe queries, once placing a section at
cosine 0.25 above a fact at 0.63. `search_lexical_headings` is the signal
that actually means *"the query names this section"* — the guarantee the
tier was built for — and it is empty on prose (13 of those same 14
probes), so it cannot re-open the same hole.

**Fusion changes the order and nothing else.** The two rankings are not
commensurable — a cosine is a distance in `[-1, 1]`, `bm25` an unbounded
corpus-relative weight — so reciprocal rank fusion (`Σ 1/(60 + rank)`)
keeps only positions and discards both magnitudes. `SectionHit::score`
stays the **cosine**, because three callers read it as one: the signpost
floor is a cosine threshold applied to it, `search_all` merges the two
corpora by comparing it against a *fact's* cosine, and `wiki_search`
serializes it to the consumer in the same `score` field a fact hit uses.
A fused number written there would fail all three with no error and no
failing test.

**A definition outranks a citation — as a tier, not a weight.** Rank
fusion alone cannot separate the section *titled* `D-006` from one that
merely *quotes* the string, because the quoting section is in **both**
lists. Measured on the production corpus right after the fusion shipped:
`D-001` cites `D-006`, led the vector list, and sat two places behind in
the lexical one — `1/60 + 1/62` against the definition's `1/78 + 1/60`,
so the citation stayed first and the defining sections came back at #2
and #3. Checked before writing any code: neither a smaller `RRF_K` nor a
heavier lexical term flips that, because both are monotone in a rank gap
of two. So [`sections::search_lexical_headings`] asks the index a second,
sharper question — which sections carry **every** term of the query in
`heading_path` — and those get a bonus larger than any achievable RRF sum
(`DEFINITION_TIER`). `AND` here, where the ranking pass uses `OR`: a
heading holding every term is the query's subject, a heading sharing one
word with a prose sentence is a coincidence. On a prose query the set
comes back empty and the tier is inert — verified on the live corpus, where
the identifier query returns exactly the two defining sections and a
five-word prose query returns none.

**The floor is applied before the fusion, and that ordering is
load-bearing.** The lexical pass matches on `OR`, so on any ordinary
sentence *something* in the corpus shares a word with it — "has a lexical
match" is not evidence, only a high lexical rank is. If the floor were
waived for lexically ranked sections, «stasera ceniamo da mia sorella»
(best cosine 0.427, floor 0.55, digs nothing today) would start dragging
in documentation because one page contains "sorella". So the floor keeps
deciding **whether** to dig, exactly as measured below, and the fusion
decides only **what surfaces** among the sections that already cleared
it. A turn that *names* its project comes through
`recall_named_project_docs` with floor 0, which is why the founder's
`D-006` case is answered by the pass that can read it literally.

## The orchestrators

| API | Scoring | ACL filter | Bumps recall counter? | Surface |
|---|---|---|---|---|
| `wiki_search` | cosine over embedding | post-fetch via [`acl::can_read`] | ✓ on returned ids (`wiki_search_unrecorded` is the bump-free sibling for measurement paths — the eval harness) | top-K vector search over the **fact** corpus |
| `search_sections` | cosine **fused with `bm25`** by rank; `score` stays the cosine | **pre-fetch**, per wiki, from the `smart_wikis` registry | ✓ on returned `(source_path, section_ord)` positions | top-K over the **section** corpus |
| `search_all` | inherited from both, then the **definition tier only** on the merged list | inherited from both, **plus** the signpost funnel on the section half | ✓ inherited | the merged view for `wiki_search` (MCP) |
| `wiki_facts_for` | constant `1.0` | post-fetch | ✗ (audit/list view) | structured SQL query |
| `wiki_recall` | delegates to `wiki_search` today | inherited | ✓ inherited | semantic recall the LLM ingest uses (stable call site) |
| `wiki_multi_hop_facts` | seed-fact + per-hop `wiki_search` | inherited | ✓ inherited | early multi-hop link resolution; lives in [`recall.rs`](../../crates/mwe-core/src/recall.rs) and returns a `MultiHopOutcome`. Exported and tested, but the agentic chat and `wiki_ingest_message` do not call it yet, pending the cap-10-hop traversal protection that gates the consumer hookup (see [What is intentionally out of scope](#what-is-intentionally-out-of-scope)). |
| `recall_fresh_captures` | cosine over **re-embedded** buffered captures | post-fetch via `buffered_visible_to` → [`acl::can_read`] | ✗ (not `fact_index` rows yet) | mid-range "fresh" slot — un-promoted captures; **ingest path only** (see [The mid-range bridge](#the-mid-range-bridge--the-fresh-slot)) |
| `recall_due_soon` | constant `1.0`, ordered by `valid_to` imminence | post-fetch | ✗ (mechanical time-driven pull — counting it would inflate recency without semantic re-use) | the **due-soon slot**: facts whose validity window closes/fires inside `[now, now + horizon]`, most imminent first — a dated commitment surfaces even when nothing in the turn resembles it. Backed by `fact_index::find_due_between`; `now` is caller-supplied (one clock per turn), the horizon is an operator setting (recall-settings panel); the window reads `valid_to`, which stays the only stored firing time — a separate `remind_at` column was considered for reminder delivery and declined, because a `valid_to` on a day boundary means "a date, no hour stated" and the hour is then a delivery-side policy, not a per-fact datum. Wired into the ingest turn as the recall block's `UPCOMING` slot — pulled on **every** LLM-routed turn (time-driven, no LLM cost), see [ingest-pipeline.md](ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate). |

### The relevance floor — a fourth gate, but on rendering, not on recall

None of the rows above are the last word on what the ingest turn's
`RELEVANT MEMORY` slot shows. `wiki_recall`'s hits still feed that slot,
still seed the navigator's entry fan, and still reach the classifier as
context, **unfiltered** — the floor below touches none of that. It only
decides whether [`ingest::format_snippet`](../../crates/mwe-core/src/ingest.rs)
is allowed to *render* the promoted hits it was handed.

[`DEFAULT_RELEVANCE_FLOOR`](../../crates/mwe-core/src/recall.rs) (default
`0.45`, `recall.relevance_floor`) is **turn-level, not per-hit**: measured
over 60 real user turns, a real answer's score band and injected noise's
score band overlap too much for any per-hit cut to separate them, but the
**best promoted (non-fresh) hit of the turn** does — `0.5474` on the turn
that held the answer against `0.4306` on the turn whose recall block
recited unrelated noise back to the user. So the gate reads that one
number per turn: below it, none of the turn's promoted hits render (the
slot is not opened, not "trimmed"); at or above it, every promoted hit
renders, including ones individually weaker than the floor. Fresh
(un-promoted) captures, the `UPCOMING` slot, and the project-docs slot are
all unaffected by design — `0` disables the gate, same idiom as the
smart-corpus funnel above. Full writeup, including the measurement table:
[ingest-pipeline.md](ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate).

## `wiki_search` step-by-step

1. **Embed** the query via the supplied `Arc<dyn Embedder>`.
2. **Fetch** candidates via [`fact_index::find_by_filters`] so
   structured filters narrow the working set *before* scoring.
3. **Score** every candidate with [`cosine_similarity`] against the
   query embedding, then apply the **validity down-rank**: a hit whose
   window is closed at query time (`valid_to` in the past) has its score
   multiplied by [`CLOSED_WINDOW_DOWNRANK`](../../crates/mwe-core/src/recall.rs)
   — a ranking **signal, never a filter** (the deviating fact is often
   the gold; the closed fact still surfaces, just below the open ones;
   the gold set's `parigi-stato` case is the evidence). Multiplicative,
   so ordering *within* the closed set is preserved; a future `valid_to`
   (an appointment to come) is open and unaffected; the fresh-captures
   slot applies the same rule to buffered windows.
4. **ACL filter** — drop rows the sender cannot read.
5. **Sort** descending by score, take `top_k`.
6. **Bump** `last_recall_at` + `recall_count_30d` on every returned id
   via [`fact_index::bump_recall_hits`] (one transaction).

Order rationale:
- *Filter → score → ACL → top-K* keeps the working set small first
  (cheap), spends CPU on the survivors, then drops the rows that
  must not leave the process. ACL post-filter is mandatory: a hit
  count that included unreadable rows would leak existence.
- Recall counter bump happens **last**, on the rows actually returned
  to the caller — so a query that gets ACL-filtered out of every hit
  does not inflate counters on rows the caller never saw.

NaN scores from a zero-magnitude embedding (model upgrade dim drift)
sort as "lowest" rather than panicking — the caller sees no signal,
not a crash.

## `FactFilters` shape

`fact_index::FactFilters` (used by both `wiki_search` and
`wiki_facts_for`) supports:

| Field | Semantics |
|---|---|
| `wiki_id` | scope to a single wiki |
| `owner_id` | scope to an owner Principal |
| `fact_type` | scope to a fact-type tag |
| `created_after`/`created_before` | ISO 8601 string-compare range |
| `topics_any` | ANY-match against `topics` JSON array (uses `json_each` from the SQLite JSON1 extension) |
| `valid_at` | the **dated-query selector**: keep only facts whose validity window contains this instant ("what was true on June 4th?") — a filter *by design*, unlike the default down-rank; bounds compare via SQLite `datetime()` so mixed `Z`/`+00:00` suffixes normalize. Surfaced on the `wiki_search` tool as `scope.valid_at`. |
| `limit` | hard SQL `LIMIT`; 0 = no cap |

Tombstoned / superseded rows are excluded by construction — recall is
the read-side primitive, so by definition it returns active rows only.

## The mid-range bridge — the fresh slot

`wiki_search` (and therefore `wiki_recall`) only sees **promoted** facts in
`fact_index`. A standard-wiki capture lives in the
[`capture_buffer`](narrative-buffer.md) until the light dream promotes it, so
material a consumer said but the dream has not yet consolidated is invisible to
topic recall — the **mid-range gap**: a claim said a few turns ago, already out
of the consumer's recent window but not yet a durable fact.

`recall_fresh_captures` closes it. It fetches the pending buffered captures
([`capture_buffer::find_all_buffered`](../../crates/mwe-core/src/capture_buffer.rs),
capped at `FRESH_CANDIDATE_CAP`), ACL-filters each via `buffered_visible_to`,
**re-embeds the body at recall time**, cosine-ranks against the query, and
returns the top `recall_fresh_top_k` as `RecallHit`s flagged `fresh: true`. No
`fact_index` row exists yet, so it does **not** bump recall counters and the
hits carry no published-page offsets (`region_start`/`region_end` are `None`).

**Scope** — the fresh slot is wired into the **ingest / conversational path
only**: [`wiki_ingest_message`](ingest-pipeline.md) merges it after `wiki_recall`
and renders it under a `Recent (not yet consolidated):` heading in the
`context_snippet`. `wiki_recall` itself stays promoted-only, so the dashboard's
`wiki_recall`-backed flows — whose edit/locate logic assumes published-page
offsets the buffer lacks — are unchanged.

**Status — provisional.** This is the minimal bridge: re-embed the few pending
captures per turn (cheap, the buffer drains at the light-dream backlog
threshold). The recall-strategy work (roadmap: flat-over-facts vs
navigation through the prose wiki) tracks the follow-up. The tracked optimisation is to embed once at
capture time and store the vector (reused at recall and at promotion), removing
the per-turn re-embed; see
the per-turn context model.

## Entry-point gathering — `recall_nav` (navigation, phase 1)

[`mwe-core::recall_nav`](../../crates/mwe-core/src/recall_nav.rs) computes the
**entry-point fan** for recall-as-navigation: the deduplicated, weight-sorted
list of `(wiki, page?)` places a navigator should start reading for the turn.
`gather_entry_points` is deterministic — no LLM call, no embedding, no recall
counters touched — and draws on four seed families:

| Family | Source | Weight |
|---|---|---|
| **Principal** | sender identity wiki + sender's groups + classified owners (each owner expanded to their groups via `enrollment::groups_for` — an owner may *be* a group or *belong* to one) | `0.6` — a wiki-level seed (`page: None`), same rung as topic-wiki: it asserts "this wiki matches the turn" on the strength of the subject being in the turn rather than a card word matching |
| **Rag** | the turn's flat-recall hits mapped back to `(wiki, page)` via `source_path`; a `fresh` hit (no published page yet) seeds the wiki root | the hit's score, clamped to `[0, 1]` |
| **Topic** | classified topics matched (case-insensitive substring) against the **reader-relative cards**: the per-wiki topic union, then — inside a matched wiki — the per-page topic union, both recomputed for the sender (see *Reader-relative cards* below) | `0.6` wiki / `0.8` page |
| **Situational** | free host-supplied strings (location, occasion), matched like topics; empty until a host sends them | `0.4` wiki / `0.5` page |

Two invariants:

- **Reader-relative cards.** Topic/situational seeds match a card recomputed
  per turn for the sender by `meta_annotate::build_reader_card`: the union of
  `topics` over the facts in a wiki (and on a page) the sender can read
  (`acl::can_read`), **not** the owner-tier `_meta`/testata keywords the
  [keyword sync](narrative-compiler.md#keyword-sync--fact-topics-into-_meta-and-the-page-testate-recall-navigation)
  writes into the `.md` for the operator's Obsidian view. So a fact the sender
  cannot read never contributes its theme — a restricted fact's topic words can
  neither act as an entry-point nor surface in the candidate cards / root index
  the navigator LLM sees. This is the serve-time enforcement of the
  [ACL card boundary](../concepts/identity-and-acl.md#the-acl-card-boundary--what-card-metadata-may-carry):
  the compile-time `.md` card stays owner-tier, the served card is reader-relative.
  The wiki's one-line abstract (`summary`/page `description`) is gated separately —
  served only to a reader whose read-set covers the wiki's default visibility
  (i.e. matches its resolved `scope`).
- **Only readable wikis seed.** A wiki the reader can read no fact in
  surfaces nowhere — it never seeds. This is *derived* from the same
  reader-relative card: `build_reader_card` populates
  `ReaderCard::readable_wikis` from every `fact_index` row the reader
  `can_read`, and the gatherer skips any wiki absent from that set. There
  is no wiki-level visibility flag — see
  [`../concepts/identity-and-acl.md` §5](../concepts/identity-and-acl.md#5-wiki-visibility-is-derived--there-is-no-wiki-level-access-gate).
- **Smart wikis are excluded from the funnel entirely** — they are free
  markdown pushed by the consumer, with no synced testata cards, no
  `[[wikilink]]` graph, and wiki-level (not per-fragment) ACL, so there is
  nothing for the funnel to hop through. They are dropped from `infos` in
  `gather_entry_points` (no seed family reaches them) and from the navigable
  `by_id` map in `navigate` (never a candidate, sibling, or link target).
  Smart content is reached via flat recall instead (cf. the REM cross-wiki
  refile sweep, which likewise skips smart). This holds for both call sites —
  the ingest navigator and the `wiki_navigate` tool below.

Duplicates collapse on `(wiki, page)` keeping the heaviest seed (ties → the
earlier family above). Page-card descent happens only inside a wiki whose own
card matched — sound because `build_reader_card` derives the wiki-level topic
union and the per-page unions from the **same** reader-visible fact set, so a
wiki card matches iff one of its page cards does.

The fan feeds the **navigator funnel** (`recall_nav::navigate`): a
Rust-owned loop where the `navigator` LLM slot (strong-but-cheap tier — see
[the config schema](../protocol/config-schema.md)) decides, one completion
per hop, which candidate pages to open and when to stop. The division of
labour is strict — **semantics in the prompt, resources in the knobs**:

- The bundled, operator-overridable prompt
  ([`prompts/navigator.md`](../../crates/mwe-core/prompts/navigator.md))
  owns link choice and the stopping judgment ("would a careful assistant be
  embarrassed to act without this page?"). There is **no link-interestingness
  heuristic in Rust**.
- `NavigatorPolicy` owns the resources: depth dial (`max_hops`, clamped to
  the hard hop cap), `pages_per_hop`, total `char_budget`, `max_candidates`
  per hop, decision token cap. Conservative defaults, overridable per
  deployment from the **operator recall-settings panel**
  (`/dashboard/admin/recall-settings`, hot-reloaded) backed by the
  [`recall:` config section](../protocol/config-schema.md#recall); the
  dogfood tunes the values.

Per hop the navigator receives the turn, the per-sender root index, the
prose collected so far, and a numbered candidate list with **reader-relative
cards** (topics the sender can read, abstract gated to default visibility — the
same `build_reader_card` projection as the seeds); it answers one strict JSON
object (`open[]` / `done`). Rust then vets every pick against the offered
candidates (a hallucinated target is discarded, never opened), reads the page,
drops the testata, and **projects it per-sender** (`render_for_sender`) — the
navigator never sees a raw ACL marker. Opening a page grows the next hop's
candidates: the entered wiki's sibling pages and the destinations reachable
via `[[wikilinks]]` from the collected prose (`Visible`-only) — a wiki hop
offers the linked wiki, a page hop offers the linked **page directly**, each
with the same reader-relative card (see the link grammar below).

Before the next prompt is built, `prune_pool` drops already-visited /
duplicate candidates, then **stably ranks the survivors by tier** before
truncating to `max_candidates`: wikilink destinations first (an authored
rail out of the page just read), then the still-unpicked entry-point fan in
the gatherer's own weight order (seeds already offered on an earlier hop
that the navigator did not choose), then sibling pages last. A wiki's
siblings are still offered wholesale — breadth stays structural, not a
leak, and a page nobody links to needs some way to be reachable — but as
the demoted tail: they fill the pool first (`sibling_page_candidates` fires
on every wiki entry, `[[wikilink]]` targets are comparatively rare), so
without this ranking a positional truncate lets the directory dump crowd
out both the rails and the fan.

Degradation contract: an LLM failure or an unparseable decision stops the
funnel and returns the partial collection — recall degrades, the turn
survives. An empty fan returns empty without spending a completion.

## Link grammar

Wikilinks are the **navigator's rails** — the memory wiki links pages so
recall-by-navigation can walk them. Humans click the same links in the
**dashboard memory explorer** ([dashboard-memory-mvp.md](dashboard-memory-mvp.md)
§Wiki view); resolvability in any external markdown viewer is a non-goal.
One canonical grammar, two forms plus a presentation alias:

| Form | Meaning | Example |
|---|---|---|
| `[[wiki_id]]` | **wiki hop** — the linked wiki (its overview); person links like `[[franz]]` are this | `[[famiglia]]` |
| `[[wiki_id/page-slug]]` | **page hop** — one page of that wiki; the slug is the page file's stem (no `.md`), and may itself contain `/` for a nested page | `[[famiglia-bruno-battaglia/referto_oculistica_bruno_2026_02_11]]` |
| `[[target\|display]]` | either form with a **display alias** — presentation only, stripped before resolution; renders as the label | `[[famiglia/index\|famiglia]]` |

Wiki ids are **flat** (`famiglia-bruno-battaglia`), never directory paths —
the id is the address, the tree position is the tree's business.

**Legacy fallback — emit canonical, resolve legacy** (the same stance the
[marker grammar](marker-grammar.md) takes on the full inline marker): a
bare target that names no wiki is retried as a **page slug over the whole
tree**, in deterministic order — the wiki the prose belongs to, its
ancestors nearest-first, its sub-wikis nearest-first, then the remaining
wikis in id order. The pre-canonical corpus links pages by bare name across
wiki lines (`[[cucina]]` on a `famiglia` page names an `morgana` page; a
sub-wiki page names its parent's dossier stub), and page prose is copied
verbatim across compiles, so those rails never self-canonicalize.
Precedence is deterministic: a wiki id always wins over a same-named page,
and a link resolves to the same destination for every reader (visibility
gates apply at the destination, they never re-route the link). What still
matches nothing (an underscored restyling of a wiki id, a slug-as-directory,
a slug with no page behind it anywhere) is a **dead rail**: the navigator
drops it, the dashboard leaves it as plain text — never a broken link.

Two consumers resolve the grammar:

- **The recall navigator** — [`recall::extract_wikilinks`](../../crates/mwe-core/src/recall.rs)
  parses both forms (alias stripped) out of collected prose;
  `recall_nav::linked_wiki_candidates` offers the linked wiki (its `_meta`
  card) for a wiki hop and the linked page itself (its testata card) for a
  page hop — one hop to the addressed destination, no forced descent through
  the wiki root — with the legacy bare slug resolved in the tree order above
  (`recall_nav::resolve_bare_slug_wiki`). Candidates stay reader-gated
  (`reader_can_read_in`) and a
  page hop is vetted against the filesystem before it is offered —
  Obsidian-style (`wiki::resolve_page_case_insensitive`): byte-exact
  first, else the unique ASCII-case-insensitive match, so a link whose
  case drifted from the filename resolves the same way it does on the
  consumer's local mirror instead of dying as a dead rail. The
  wiki-granular projection `extract_wikilink_wiki_ids` backs the structural
  hop graph (`wiki_multi_hop_facts`) and the REM back-pressure/backlink
  sweeps.
- **The dashboard click-through** — the memory explorer's rendered page view
  and the fact record's rendered body linkify both forms (legacy fallback
  included, resolved against the page's / the fact's own wiki) into
  in-dashboard navigation, alias as the label, dangling target as plain text
  ([dashboard-memory-mvp.md](dashboard-memory-mvp.md)).

Every mechanical emitter writes the canonical forms —
`capture::wiki_link`, the document-ingest dossier anchor, the smart-push
`authored_refs`, the root indexes, and the compiler feeds
(`compiler::plan_page_wikilink`: the starvation index, the recommended
links, the Hub Writer children — a page hop everywhere, collapsing to the
wiki hop for a wiki's own `index.md`). The prose-writing prompts
(`cronista`, `regenerate-index`) carry the copy-verbatim instruction: a
model never mints or restyles a link target. There is no mechanical corpus
rewriter — the verbatim-copied legacy links simply stay navigable through
the read-side fallback above.

**Wired into the ingest turn** as the recall block's `NAVIGATED PAGES`
section: the funnel runs **after** the classification, reusing its
`topics`/`owner_id`s as gather seeds (the Step-1 flat recall stays the cheap
seed and classifier input — «RAG for the entrances»), only for intents that
justify the LLM spend (capture / recall / disambiguation), and only when the
call site wired the optional `navigator` backend. The reserved `rules.md`
policy page is never a door (channel-only — the fan skips it, a RAG hit on
it seeds the wiki root, and the open step discards it as a fail-safe). The
assembled role-labelled `context_snippet` (`WHO YOU ARE` → `WHO IS
SPEAKING` → `YOUR RECENT HISTORY WITH THIS USER` → `RELEVANT MEMORY` →
`NAVIGATED PAGES` → `UPCOMING`) is documented in
[ingest-pipeline.md](ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate).

## Consumer-facing deep recall — the `wiki_navigate` tool

The same funnel is also exposed as a callable MCP tool, `wiki_navigate`
(family D — see [mcp-tools.md](../protocol/mcp-tools.md)), so a consumer that
searches **explicitly** (any smart consumer; the web one in particular, which
gets no per-turn injection) reaches deep recall, not just flat top-K. It is the
**deep counterpart of `wiki_search`** (whole visible corpus, ACL-filtered), and
returns the **navigated fragments with their `(wiki, page)`** (the path that
built the context) **plus** the flat hits — depth is a superset of breadth, so
the consumer never needs to call both. It degrades to flat-only when no
`navigator` slot is wired (`navigator_available: false`).

The standard consumer is **not** taught this tool: its deep recall is the
automatic ingest injection above, and it keeps `wiki_search`/`wiki_read` as its
flat explicit escape hatch. The tool is not class-gated (whole-corpus +
ACL-filtered is safe for any caller) — this is positioning, not a gate.

Seed cascade (the tool has no classifier in the loop, unlike ingest): the
caller's explicit `topics`/`owners` win (**C**); else a small dedicated
extraction over the query on the `navigator` slot
(`recall_nav::extract_query_seeds`, prompt
[`prompts/query-seeds.md`](../../crates/mwe-core/prompts/query-seeds.md);
entity names resolved against enrollment, unresolved names folded into topics)
(**B**); else principal + RAG seeds only (**A**). Each step degrades to the
next.

## Recall traces — the last-10 journal

[`mwe-core::recall_trace`](../../crates/mwe-core/src/recall_trace.rs) journals
the **whole route** a recall took, so the operator can finally see *why* a
fact did or did not surface. Two producers write it:

- the **ingest per-turn injection** — `wiki_ingest_message` records every
  LLM-routed turn (and the ephemeral guest turn) right after the recall
  block is assembled;
- the **`wiki_navigate` tool** — the MCP handler records each run, with the
  consumer id from the token.

One row per run in the `recall_traces` table (migration `0057`): stamp,
source (`ingest` | `navigate`), sender, and a **versioned JSON payload**
(`RecallTrace`, every field `serde(default)` so older rows keep decoding).
The payload carries the turn text (capped), the classification seeds and
where they came from (`classifier` / the `wiki_navigate` cascade rungs /
`guest`), the flat / fresh / due-soon hits (score, page, byte region,
capped body), the entry-point fan (family + weight), the **funnel journal**
— per hop: the candidate cards exactly as offered, the decision (`open[]`,
`done`, and the navigator's own one-line `note`, which the Rust binding now
captures), the vetting outcome of every pick, the opened pages (chars,
excerpt, links discovered) — the stop reason ([`NavStop`](../../crates/mwe-core/src/recall_nav.rs):
`done` / `budget` / `hop_cap` / `llm_degraded` / `nothing_opened` /
`pool_exhausted` / `empty_fan`), the budget spent, and the **injected block
verbatim** (`context_snippet` + the `rules` field on ingest; the result
payload on `wiki_navigate`).

The funnel's own half lives in [`NavigationOutcome::trace`](../../crates/mwe-core/src/recall_nav.rs)
(`Vec<HopTrace>` + `stop`), populated on every `navigate` run — string
clones only, no extra I/O or LLM cost. Recording is **best-effort
telemetry**: a journal failure logs a warning and never touches the turn.
The journal prunes to the newest `TRACE_KEEP` (10) rows after each insert —
a resource cap; `tool_executions` remains the audit surface. The dashboard
surface (journal list + the animated 3D replay viewer) is open to
[every signed-in user](dashboard.md) and **scoped to the reader's own
recalls** — a trace belongs to the sender it was recorded for, so reading
your own is transparency about the answer you were given rather than
operator telemetry. Widening it to the whole deployment is
[admin reveal](redaction-policy.md#dashboard-admin-reveal), the one switch,
because a trace does cross wiki and ACL lines by construction.

## The recall eval harness — `recall_eval`

[`mwe-core::recall_eval`](../../crates/mwe-core/src/recall_eval.rs) turns
"navigation beats flat RAG" from an anecdote into numbers. A YAML **gold
set** is replayed against a workdir; both paths run exactly as the ingest
turn runs them (flat = `wiki_search`; navigation = gather → navigate with
the flat hits as RAG seeds) and are scored by case-insensitive substring
containment of the **expectations**:

```yaml
queries:
  - id: galadriel-dinner            # optional label
    query: cosa cucino stasera per gli ospiti?
    sender_id: alice
    topics: [cucina]                # optional classifier-style seeds
    owners: ["user:galadriel"]      # optional
    expect:                         # ground truth: snippets recall MUST surface
      - celiaca
      - senza glutine
```

| Metric | Meaning |
|---|---|
| **hit@1 / hit@3** | flat baseline: ≥ 1 expectation in the top-1 / top-3 hit texts |
| **coverage** (per path) | fraction of expectations found anywhere in that path's output |
| **deviating** | expectations navigation surfaced that flat similarity missed — the metric the navigation design exists for |
| **combined / missing** | union of the two paths / expectations neither surfaced |

Measurement discipline: the flat pass uses `wiki_search_unrecorded` (no
recall-counter bump — synthetic queries must not inflate the corpus's
recency signal) and the harness takes no lockfile (read-only, safe next
to a live `serve`). CLI: `mwe-mcp recall eval --gold <file> [--flat-only]`
([build-run.md](../development/build-run.md)); knobs come from the
[`recall:` config section](../protocol/config-schema.md#recall). The gold
set itself is **filled by the dogfood** on real material — the harness
ships first so the queries can be captured as they come up.

## The hindsight log + the judge-free miss signal

Self-correcting REM's detection floor
([`mwe-core::recall_log`](../../crates/mwe-core/src/recall_log.rs),
migration `0058`): the system notices, from the user's own behaviour,
when recall **failed to surface a fact it already held** — no LLM verdict
anywhere in the signal.

- **The hindsight log** (`recall_log`, one lean row per LLM-routed ingest
  turn): the fact ids the turn surfaced (flat + fresh + due-soon hits)
  and the navigated pages' workdir-relative source paths. Written
  best-effort at the end of the turn; age-pruned (30 days). Freshly
  buffered captures get the row's id stamped on them
  (`capture_buffer.recall_log_id`) so the offline half below can look
  back at the turn that produced them.
- **The miss signal**: when the write-time dedup
  ([`capture::best_dedup_candidate`]) skips a capture as a restatement of
  an existing fact X, the user just re-said something memory knew — if X
  was also absent from that turn's surfaced set (not a hit, not on a
  navigated page), recall demonstrably missed it, and one `recall_misses`
  row lands (fact, home page, dedup score, the restatement, the turn's
  log row; 90-day prune). Detected on **both dedup surfaces**: the direct
  capture path checks in-memory at the end of the same turn; the light
  dream's promotion fold checks the buffered capture's logged turn.
  Rules-page facts are out of scope (channel-delivered, never recalled
  memory).

Both tables are telemetry-class (every writer best-effort, resource-cap
pruned). Their consumer is the REM
[recall-repair sub-job](rem-cycle.md#recall-repair-sub-job--self-correcting-rems-repair-stage):
each pending miss is judged for a re-file repair that commits **only**
through the gold-set gate
([`mwe-core::recall_gate`](../../crates/mwe-core/src/recall_gate.rs)) —
the candidate move is applied to a scratch snapshot and must make the
missed query surface its fact without regressing the deployment's gold
set (`<workdir>/recall-gold.yaml`, this harness's YAML shape). Every
real miss also lands as a **candidate** gold case in
`<workdir>/recall-gold-candidates.yaml` — the operator reviews, distils
the expectations, and merges them into the gold file by hand: the loop
that grows the judge from the system's own confirmed failures, without
ever letting an unreviewed case *become* the judge.

## ACL projection

Every orchestrator routes through `row_visible_to(row, sender)`,
which builds an `Acl { owner: Some(row.owner_id), allow:
row.allow_ids }` and calls [`acl::can_read`] with the row's
optional `sender_id` for the cross-user attribution invariant
(the `sender=` marker rule in
[`identity-and-acl.md §2`](../concepts/identity-and-acl.md#2-block-level-acl-model)).
The check uses bare ids — the
[`SenderContext`] type carries `sender_id: String` (no `user:`
prefix) and `sender_groups: Vec<String>`, mirroring what the JWT
puts on the wire.

The fresh slot mirrors this: `buffered_visible_to(cap, sender)` builds the same
`Acl` from the buffered capture's `owner` / `allow` / `sender` and calls the
identical [`acl::can_read`], so an un-promoted capture is ACL-gated exactly like
a promoted fact.

The `sender_groups` vector is **populated at every production
construction site** via
[`enrollment::groups_for(pool, sender_id)`](../../crates/mwe-core/src/enrollment.rs),
which reads the `enrollment_groups.members` JSON array through a
SQLite `json_each` predicate. The five production call sites are:
the ingest orchestrator
([`wiki_ingest_message`](../../crates/mwe-core/src/ingest.rs) — which
calls the scope-carrying sibling
[`groups_with_scope_for`](../../crates/mwe-core/src/enrollment.rs) and
derives the bare ids from `.0`, so the prompt also gets each group's
`scope` for owner routing; see
[`ingest-pipeline.md`](ingest-pipeline.md)),
the `wiki_search` MCP tool
([`call_wiki_search`](../../crates/mwe-mcp-server/src/mcp/tools.rs)),
the dashboard `/facts` index + open-in-chat handlers
([`routes/facts.rs`](../../crates/mwe-dashboard/src/routes/facts.rs)),
and the dashboard chat agentic loop
([`routes/chat.rs`](../../crates/mwe-dashboard/src/routes/chat.rs)).
The convenience constructor `SenderContext::user(id)` deliberately
leaves the vector empty and is therefore reserved for tests and the
`anonymous()` path.

Tests cover:
- owner-user self vs other
- cross-user attribution (sender = bob captures on alice's wiki ⇒
  bob can read his own region)
- group membership (now wired end-to-end in production via
  `enrollment::groups_for`)
- global owner ⇒ anyone (anonymous + named user)

## Why brute-force cosine for now

Both orchestrators scan every candidate with a flat O(N) cosine loop:
each query reads the candidate rows **with their vectors** and scores
them in memory. There is no ANN index; a `sqlite-vec` integration is not
built today. (The lexical pass is *not* part of this cost — `bm25` over
FTS5 is an index lookup, and it returns identities rather than rows.)

The cost is therefore linear in the candidate count, and the candidate
count is what the corpus split above controls. Measured on a production
store of ~4k active rows with bge-m3 1024-d vectors (≈16 MB of
embeddings): reading the full candidate set costs ~30-45 ms warm, and
observed end-to-end `wiki_search` latency ran ~170-260 ms including the
query embed — up from ~100 ms when the same store held ~1k rows. So the
scan is not the dominant term yet, but it is the term that **grows**, and
it grows fastest with documentation, which is exactly the corpus a
conversational turn no longer scans.

Watch `wiki_search` latency (it is journalled per call in
`tool_executions`) rather than row counts: when the scan starts to
dominate the embed, the `sqlite-vec` work is due. Splitting the corpora
also means the index can be added to one of them first — the section
table is the larger and the more regenerable of the two.

## Test coverage

- Pure helpers: 6 cosine tests (identical / orthogonal / opposite /
  zero / mismatched-dim / empty).
- ACL projection: 4 row-visibility tests (user/cross-user/group/global).
- `score_and_filter`: 3 (descending sort + truncate / ACL drop / empty).
- `fact_index::find_by_filters`: 3 (wiki scope / owner+type combo /
  topics_any with `json_each`).
- `wiki_search`: 4 (top-K by cosine / ACL drop / recall-counter bump
  / top-K=0 returns empty).
- `search_sections`: 5 (cosine ranking + telemetry bump / wiki-level ACL
  across owner, group grantee and stranger / a revoke closes the read
  window with one row write / an exact term outranks a better cosine
  while `score` stays the cosine / a query with no lexical match leaves
  the cosine order untouched).
- Lexical pass, in [`sections`](../../crates/mwe-core/src/sections.rs): 5
  (query building — identifiers stay adjacent, operator words are quoted
  not obeyed, repeats and punctuation collapse, a `"` cannot reach the
  expression, term cap / identifier ranking + wiki ACL + the three empty
  short-circuits / a section *titled* with an identifier beats a shorter
  one *citing* it, which fails without the heading column / the index
  tracks in-place edits and page deletions through its triggers).
- `search_all`: 4 (both corpora merge into one ranking while
  `wiki_search` stays facts-only / `top_k` honoured across the merge / a
  section the query **names** outranks a perfect-cosine fact, and does so
  through the definition tier rather than its score / a section that merely
  **mentions** a query token does *not* evict a better-scoring fact — the
  regression guard for the cross-corpus ranking bonus).
- `admitted_smart_wikis` (the funnel): 3 — a project whose description does
  not match stays shut and its perfect-cosine section never surfaces / the
  description *or* the project's name opens it, the name regardless of the
  floor / it fails closed with no description, a floor of 0 is the explicit
  off switch, and another user's unshared description neither admits the
  project nor reveals it.
- `recall_named_project_docs` + `rank_project_sections`, fusion: 2 (naming
  the project puts its identifier in the single slot / a lexical match
  does **not** lift a section over the signpost floor).
- `recall_project_docs`: 10 — the match rule (case/punctuation
  insensitive, hyphenated slug either spelling / a compound slug never
  fires on one of its words / a slug inside a longer word does not count
  / a too-short slug never triggers); the name path (fires only when the
  message names the project, naming an unreadable project yields nothing,
  the char budget bounds the slot with whole sections and `top_k = 0`
  disables it); and the signpost path (a surfaced signpost opens its
  project with the project never named / the floor keeps a
  weakly-matching turn's slot empty while a *named* project is served
  regardless / a signpost never opens a project its reader cannot see).
- `signposts`: 11 — the write lands on the owner's reserved page carrying
  the project name; an unchanged refresh writes nothing; a new
  description supersedes; the day window rolls; the same day twice
  supersedes; both caps refuse rather than truncate; only the owner may
  signpost; a standard wiki cannot be signposted; read access mirrors the
  project's roster; a malformed day is refused.
- `wiki_facts_for`: 2 (filtered without counter bump / ACL filter).
- `wiki_recall`: 1 (delegates to search today).
- `recall_fresh_captures`: 1 (un-promoted buffered capture surfaces, ACL-scoped, flagged `fresh`; another owner's capture is filtered out).
- `recall_nav` (gatherer): per-family seed tests (principal expansion /
  topic wiki→page descent / situational / rag path mapping), the
  per-family ACL-cascade matrix, dedup/sort, `page_within`.
- `recall_nav` (funnel, scripted-LLM): vetted open + per-sender
  projection, hallucinated-target discard, wikilink follow-through
  (wiki hop, direct page hop with alias stripped, dead page hop never
  offered), char-budget truncation, soft-fail on unparseable decisions,
  done / empty-fan short-circuits, fence-tolerant decision parse.
- Link grammar: `recall.rs` pins `extract_wikilinks` (page hops kept,
  aliases stripped, trailing-slash and whitespace degenerate forms).

## What is intentionally out of scope

These are not implemented today (planned — see the
roadmap):

| Not yet supported | Why |
|---|---|
| **`sqlite-vec` integration** | Profile-driven. Still fast enough at current size; the signal to watch is `wiki_search` latency, and the corpus split bought headroom by keeping documentation out of the conversational scan. |
| **LLM rerank** | `wiki_recall` stays a stable call site so the ingest LLM can layer rerank on top without breaking signatures. |
| **`recent_messages` weighting** | Accepted as parameter today, ignored; the weighting will use context-cache hooks the ingest LLM owns. |
| **Multi-hop link resolution wired into consumer surfaces** | `wiki_multi_hop_facts` already lives in [`recall.rs`](../../crates/mwe-core/src/recall.rs) with tests, but it is not yet called by `wiki_ingest_message`, the agentic chat, or `wiki_navigate`. The cap-10-hop traversal protection plus the right rerank policy are needed before the consumer hookup. |
| **Aggregate telemetry** (latency histograms, candidate-count distributions) | Only the per-run [recall trace](#recall-traces--the-last-10-journal) exists (the last 10 routes, replayable); aggregates and counters beyond `recall_count_30d` are not built. |
