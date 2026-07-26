---
title: Recall pipeline — the read-side orchestrators and the entry-point gatherer
area: design-notes
status: implemented
last_review: "2026-07-26"
---

# Recall pipeline

[`mwe-core::recall`](../../crates/mwe-core/src/recall.rs) hosts the
read-side orchestrators that complement [capture](capture-and-dedup.md).
The module is layered: pure helpers at the top (n-gram + jaccard +
cosine) and the async orchestrators at the bottom.

## The two corpora

Recall reads **two** stores, and which one a caller gets is an explicit
choice made by calling a different function — there is no flag that can
be forgotten:

| Corpus | Table | What it holds | Entry point |
|---|---|---|---|
| **Facts** | `fact_index` | Standard-wiki memory: governed claims with per-fragment ACL, supersedence, validity, attribution | `wiki_search` (and everything built on it) |
| **Sections** | `wiki_sections` | Smart-wiki documentation: heading-delimited chunks of pages a smart consumer authored, ACL held per wiki in `smart_wikis` | `search_sections` |
| Both | — | merged into one ranking, `top_k` applied after the merge | `search_all` |

`wiki_search` returns [`RecallHit`]s and **cannot** return
documentation — the two live in different tables, and `SectionHit` is a
different type. That is the point: the ingest turn, `recall_core_global`,
the recall gate and the eval harness all take the fact corpus, so
project documentation can no longer crowd out personal memory in a
conversational turn. Only the two consumer surfaces whose contract is
"search everything I can see" — `wiki_search` (MCP) and `wiki_navigate` —
reach for the merged view.

**ACL is resolved once per wiki, not once per row.** `search_sections`
loads the `smart_wikis` registry (a handful of rows), keeps the wikis the
sender may read (`owner ∪ shared_with`, the same effective set
[`acl::can_read`] evaluates), and only then loads those wikis' sections.
An unreadable wiki's bytes never leave the DB. A sharing revoke is a
single-row write and closes the read window on the next query.

## The orchestrators

| API | Scoring | ACL filter | Bumps recall counter? | Surface |
|---|---|---|---|---|
| `wiki_search` | cosine over embedding | post-fetch via [`acl::can_read`] | ✓ on returned ids (`wiki_search_unrecorded` is the bump-free sibling for measurement paths — the eval harness) | top-K vector search over the **fact** corpus |
| `search_sections` | cosine over embedding | **pre-fetch**, per wiki, from the `smart_wikis` registry | ✓ on returned `(source_path, section_ord)` positions | top-K vector search over the **section** corpus |
| `search_all` | inherited from both | inherited from both | ✓ inherited | the merged view for `wiki_search` (MCP) + `wiki_navigate` |
| `wiki_facts_for` | constant `1.0` | post-fetch | ✗ (audit/list view) | structured SQL query |
| `wiki_recall` | delegates to `wiki_search` today | inherited | ✓ inherited | semantic recall the LLM ingest uses (stable call site) |
| `wiki_multi_hop_facts` | seed-fact + per-hop `wiki_search` | inherited | ✓ inherited | early multi-hop link resolution; lives in [`recall.rs`](../../crates/mwe-core/src/recall.rs) and returns a `MultiHopOutcome`. Exported and tested, but the agentic chat and `wiki_ingest_message` do not call it yet, pending the cap-10-hop traversal protection that gates the consumer hookup (see [What is intentionally out of scope](#what-is-intentionally-out-of-scope)). |
| `recall_fresh_captures` | cosine over **re-embedded** buffered captures | post-fetch via `buffered_visible_to` → [`acl::can_read`] | ✗ (not `fact_index` rows yet) | mid-range "fresh" slot — un-promoted captures; **ingest path only** (see [The mid-range bridge](#the-mid-range-bridge--the-fresh-slot)) |
| `recall_due_soon` | constant `1.0`, ordered by `valid_to` imminence | post-fetch | ✗ (mechanical time-driven pull — counting it would inflate recency without semantic re-use) | the **due-soon slot**: facts whose validity window closes/fires inside `[now, now + horizon]`, most imminent first — a dated commitment surfaces even when nothing in the turn resembles it. Backed by `fact_index::find_due_between`; `now` is caller-supplied (one clock per turn), the horizon is an operator setting (recall-settings panel); the window reads `valid_to` until a distinct `remind_at` lands with reminder delivery. Wired into the ingest turn as the recall block's `UPCOMING` slot — pulled on **every** LLM-routed turn (time-driven, no LLM cost), see [ingest-pipeline.md](ingest-pipeline.md#the-recall-block--recalled-memory-the-rules-field-is-separate). |

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
| **Principal** | sender identity wiki + sender's groups + classified owners (each owner expanded to their groups via `enrollment::groups_for` — an owner may *be* a group or *belong* to one) | `1.0` (maximum — identity anchors always survive dedup) |
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
surface (journal list + the animated 3D replay viewer) is
[admin-only](dashboard.md) — a trace crosses wiki and ACL lines by
construction.

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
built today.

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
- `search_sections`: 3 (cosine ranking + telemetry bump / wiki-level ACL
  across owner, group grantee and stranger / a revoke closes the read
  window with one row write).
- `search_all`: 2 (both corpora merge into one ranking while
  `wiki_search` stays facts-only / `top_k` honoured across the merge).
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
