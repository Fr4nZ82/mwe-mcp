---
title: REM nightly cycle — orchestrator + sub-job roster
area: design-notes
status: implemented
last_review: "2026-07-05"
---

# REM nightly cycle

[`mwe-core::rem`](../../crates/mwe-core/src/rem.rs) ships the cron-driven
nightly job that runs without users connected. The pipeline wires a
fixed sequence of sub-jobs around a single orchestrator
([`rem::run_cycle`](../../crates/mwe-core/src/rem.rs)) and the
[`wal`](../../crates/mwe-core/src/wal.rs) journaling primitives — the
authoritative roster and order are the call sequence in `run_cycle`;
the table below mirrors it. The **write-jobs**
(revisor, auto_promote, the consolidation/hygiene sweeps,
archive_detector, hub_writer)
carry an `is_smart_wiki` skip gate so they leave wikis of the
smart family to the smart consumer. The two smart-wiki read-jobs
(Briefing dispatcher + Backlink reciprocity detector) invert the
gate — they are smart-wiki-only and post observations into
`_briefing.md`. The Briefing-processor non-smart sub-job is the
symmetrical drain for standard families: REM is
the conceptual maintainer of non-smart wikis (`wiki-user`,
`wiki-group`, `wiki-root`, and emergent standard sub-wikis), the smart
consumer is the maintainer of
smart wikis; same queue (`wiki_briefing_items`), two processors split by
family classification. The two proposal sweeps (auto_apply /
auto_finalize) and the lease expirer have no smart-family gate at all —
they are pure proposal/lease housekeeping that is family-agnostic by
construction.

## Pipeline (per cycle)

```text
 1. auto_apply sweep             proposals::apply_proposal on rows whose timeout_at < now
 2. auto_finalize sweep          proposals::auto_finalize_unconfirmed
 3. revisor jaccard              recall::jaccard_sets + rem_dedup_semantic LLM → direct apply + `structure_applied` notice (dedup_merge) *[skips smart]*
 4. auto_promote                 rem_promotions LLM → direct apply + `structure_applied` notice (file_to_subwiki + paragraph_to_file)   *[skips smart]*
 5. page_merge                   plan/reviewer signals nominate → rem_dedup_semantic LLM confirms → direct apply + `structure_applied` notice (page_merge)
 6. completion_sweep             fresh evidence × similar open items → rem-completion LLM confirms → close_validity + `validity_close` receipt + notice *[skips smart]*
 7. contradiction_sweep          freshly contradicted seeds × similar open items → rem-contradiction LLM confirms → satellites close as contradicted, same paper trail *[skips smart]*
 8. refile_sweep                 cosine pre-filter nominates misfiled facts → rem-refile LLM picks a dest wiki → cross-wiki move onto the dest wiki's `index.md` + `fact_refile` receipt + notice *[skips smart both ends]*
 8b. recall_repair               pending recall misses → rem-recall-repair LLM proposes a re-file → gold-set gate replays it on a scratch snapshot → commit only on proven flip (same mover/receipt as 8) else discard/queue operator notice *[skips smart + rules pages]*
 9. provenance_hygiene           deterministic trailing-`([[…]])` detector → pointer moved into `authored_refs`, suffix stripped, text re-embedded (no LLM) *[skips smart]*
10. date_normalizer              deictic lexicon flags → rem-dates LLM rewrites relative→absolute on canonical text + re-embed *[skips smart]*
11. archive_detector             stale-path detector → `archive_proposals`                  *[skips smart]*
12. briefing_dispatcher          scans *smart wikis only* for stale drafts + recall-hot     → briefing::notify_as_rem
13. backlink_reciprocity         standard → smart-wiki `[[wiki:...]]` links lacking inverse → briefing::notify_as_rem
14. lease_expirer                wiki_admin_leases::expire_stale (mark active-past-grace as released + delete released-past-retention)
15. briefing_processor           drains `wiki_briefing_items` on *non-smart* wikis past grace → briefing_processor::process_briefing_item
16. husk_gc                      plan-absent page files whose rows are all tombstoned/superseded past the revert window → remove_file + settle offsets (no LLM) *[skips smart]*
17. hub_writer                   hub_writer LLM + atomic_write index.md                     *[skips smart + plan-owned indexes]*
```

Order rationale: the auto_apply + auto_finalize sweeps catch up on any
structure proposals that timed out overnight before new emitters
re-suggest them; revisor / archive emit fresh proposals and
auto_promote applies its structural changes against the
post-maintenance snapshot; the consolidation passes (completion
sweep, contradiction sweep, provenance hygiene, date normalizer) run after the structural movers — they mutate
fact validity/text, not layout, and the compile that follows the cycle
picks both up through the render-content fingerprint; the cross-wiki
`refile_sweep` runs after the contradiction sweep so it judges against
this post-consolidation snapshot, moving a misfiled fact (act-first,
revertable) onto the destination wiki's foundation page; the
deterministic provenance-hygiene sweep runs right before the date
normalizer — its LLM sibling on the same edit+re-embed shape — so the
normalizer (and every later sub-job) already sees pointer-clean text;
the two smart-wiki sub-jobs run
next so their findings reach the smart consumer's inbox alongside the
night's other observations; lease_expirer cleans up stale rows in
`wiki_admin_leases` so the dashboard `/wikis/<id>/op-log` view stays
bounded (no LLM, no filesystem — pure SQL housekeeping); the
non-smart briefing-processor drains operator comments on standard
wikis past the grace period (the smart-consumer-on-smart-wikis dual);
the husk-page GC runs after every mover has settled the night's fact
state (a page one of them just emptied is judged on the final shape);
hub_writer summarises last so its prompt sees a stable state. Running
hub_writer first would force re-runs whenever any earlier sub-job
changed a wiki.

## Smart-wiki classification

A single cycle-scoped `SmartWikiIndex`
(`HashMap<wiki_id, smart: bool>`) is built up-front by
[`load_smart_wiki_index`] in one tree walk that reads the per-wiki
smart flag from each `_meta.md`. Every smart-wiki-aware sub-job shares the same map, so a
classification race between sub-jobs is impossible. Unknown wiki ids
(deleted between the snapshot and now) default to `false` via
[`is_smart_wiki`] — the legacy write-jobs keep operating on
partially-broken trees rather than silently dropping work, and the
non-smart briefing-processor treats them as standard wikis
(best-effort drain).

The write-jobs add `if is_smart_wiki(smart_wiki_index, …) { continue; }`
at the top of their `tree.walk()` loop. The two smart-wiki sub-jobs invert
the gate: they keep smart wikis and skip the rest. The
briefing-processor uses the same map to leave smart-wiki inbox rows
to the smart consumer.

The orchestrator entry point is [`run_cycle`]:

```rust
pub async fn run_cycle(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    policy: &RemPolicy,
) -> Result<RemCycleReport>;
```

`RemLlms` carries the per-sub-job model handles — `hub_writer` and
`revisor` are mandatory `&dyn LlmBackend`, `auto_promote` and `apply`
are `Option<&dyn LlmBackend>` — wired from the operator's
`mwe-mcp.config.yaml > llm`: per the
[REM LLM functions](llm-functions.md) and the
[runtime topology](../architecture/runtime-topology.md), `hub_writer`
and `apply` take the workhorse (Qwen 7-9B locally is fine), `revisor`
takes a small model, and `auto_promote` takes the strong model. A `None`
optional slot **gates** the matching sub-job to a
short-circuited no-op (with `disabled_reason` populated) rather than
hard-refusing the whole cycle. The `embedder` argument feeds every
sub-job that re-embeds text it touched: the date normalizer and
provenance hygiene (edited fact text), the revisor's dedup **apply**
(`dedup::apply_dedup_merge_direct` excises the loser's page region and
re-syncs the page via `reindex::strip_fact_region`), and the briefing
processor's comment-apply path (corrected / added claims). Only the
revisor's **nomination** needs no embedder call: its **semantic**
channel reads the embedding vectors already stored on the `fact_index`
rows (cosine ≥ `revisor_cosine_min`, alongside the surface character
6-gram jaccard from [`recall::jaccard_sets`](recall-pipeline.md)).

> **Fact expiry is the per-fact `valid_to` window**, not a tombstoning
> cron: an expired fact is soft down-ranked at recall and handled by REM
> housekeeping. mwe-mcp does **not** emit proactive reminder events by
> default — modern consumers run their own cron, and actionable state
> lives for **recall** ([memory-model.md](../concepts/memory-model.md)).

## Family scope — the consolidation passes' unit (leva-2)

The four consolidation passes below (revisor dedup, page-merge,
completion sweep, contradiction sweep) pool their candidates per
**family line**: a top-level standard wiki plus its own sub-wiki
descendants, one scope ([`family_scopes`](../../crates/mwe-core/src/rem.rs)).
That is what lets the fragments of a subject split across a wiki and
its emergent sub-wiki finally reconcile — the duplicated identity facts
("è il padre di Franz") that intra-wiki passes could never pair, the
contradiction whose satellites live on the other side of the line.
Membership is **directory nesting** (component-wise `abs_dir` prefix),
never the id string (a legit top-level id may contain hyphens —
`famiglia-bruno-battaglia` is famiglia's child because of where it
lives, not what it is called). Smart wikis are excluded entirely;
**arbitrary cross-wiki pairs stay out of scope** (the `morgana` ↔
`hermes1` class of contradiction is
self-correcting REM's future
business). Within a line the passes stay god-mode as they always were —
the family is the same subject's own tree; per-fragment ACL travels
untouched with every move (`move_to_wiki` keeps owner/allow/sender).
Pinned by `family_scopes_partition_by_directory_nesting_not_id`.

## Revisor / Conciliatore sub-job

For each **family line** ([family scope](#family-scope--the-consolidation-passes-unit-leva-2)):

1. Fetch the line's active facts in member walk order.
2. Precompute character 6-gram sets per fact body (cheap, reuses the
   capture-side machinery in `recall::ngrams`).
3. Iterate `(new_idx, old_idx)` pairs (newer = survivor) and nominate
   through **either deterministic channel** — the LLM makes the verdict
   either way:
   - **surface** — `jaccard_sets` inside the suspicious band
     `revisor_jaccard_min..revisor_jaccard_max` (defaults 0.45 ..
     `recall::DEFAULT_DEDUP_THRESHOLD`; at/above the max the pair is
     write-time dedup territory — the capture scan, re-run by the light
     dream at promotion — and the revisor leaves it alone);
   - **semantic** — embedding `cosine_similarity ≥ revisor_cosine_min`
     (default 0.80; same-dimension, non-bit-identical vectors only).
     This catches the same claim restated with the subject spelled out
     vs elided ("È nato il 23 maggio 1984" / "Franz è nato il 23 maggio
     1984"), which shares meaning but too few n-grams for the surface
     band. The vectors are already on the `fact_index` rows — no
     embedder call.

   A pair is nominable only when **both sides are `rules.md` facts or
   neither is**
   ([`wiki::is_rules_page`](../../crates/mwe-core/src/wiki.rs)): a
   behaviour rule dedups rule-vs-rule only — if it lost against an
   episodic restatement, its content would survive only off `rules.md`,
   outside the behaviour-rules channel
   ([ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-by-scope-outside-fact-memory)).
   A structural channel invariant, not a semantic gate — rule-vs-rule
   pairs still go to the LLM.
4. Ask the `rem_dedup_semantic` LLM with a strict-JSON prompt:
   `{"same": true|false}`. Each side is framed with the page it lives
   on (`wiki_id · source_path`, the `{new_page}`/`{old_page}`
   placeholders of [`rem-dedup`](../../crates/mwe-core/prompts/rem-dedup.md)):
   compiled prose routinely elides a subject the page itself
   establishes, and without the page the confirmer cannot tell whether
   two subject-elided claims share their subject (it would fail-safe to
   "not the same" and the duplicate would survive every night).
5. On confirm, merge **act-first** via
   [`dedup::apply_dedup_merge_direct`](../../crates/mwe-core/src/dedup.rs):
   the loser is superseded by the winner in-cycle (inside a
   `dedup_merge_apply` WAL op), the loser's **on-disk region is excised**
   (`reindex::strip_fact_region`, best-effort — the retirement disk half,
   [redaction-policy](redaction-policy.md)), a **born-applied**
   `dedup_merge` receipt records the undo anchor (7-day revert window,
   addressed to the winner's human), and a `structure_applied` notice
   with the `dashboard_path` lands on the event stream — the same
   authority model as auto-promote and the page merge. No pending stage,
   no 24 h auto-apply fuse. A reverted merge reactivates the loser as a
   pending render (its prose returns at the page's next compile; a rules
   fact keeps serving from the DB either way).

The pair scanner short-circuits when `RevisorReport.applied`
hits `revisor_cap` (default 30), and caps LLM confirms across both
nomination channels at `revisor_examined_cap` (default 120) — a
resource guard, logged when it trips (never a silent truncation);
remaining pairs wait for the next cycle. It also tracks
`loser_fact_id` values already merged in the current cycle so a chain
of similar facts is collapsed at most once per pass — the next cycle
picks up any residual.

`RevisorReport.errors` collects LLM failures and apply failures without
aborting the whole sub-job; downstream calls observe them via the
cycle report.

## Auto-apply sweep sub-job

Runs first in the cycle, before the new emitters. Selects every
`pending` row in `structure_proposals` whose `timeout_at < now`, builds the
`recommended` answers from the questionnaire JSON (option's `value` if
present, `id` otherwise), and calls `proposals::apply_proposal` with
`applied_by = None` to mark the row as auto-applied. Since the act-first
conversion **no REM emitter creates `pending` rows** (the revisor dedup
joined the born-applied receipts) — the sweep stays honoured for
questionnaire rows planted by a non-REM emitter.

- Handler-level failures (filesystem error, marker drift) are
  collected into `AutoApplyReport.errors` as `(proposal_id, message)`
  and the sweep moves on.
- Infrastructure failures (SQL) bubble up via `RemError`.
- The 7 d revert window starts ticking from the auto-applied
  `applied_at` — same contract as a user-driven apply.

No proposal kind needs an LLM at apply time today, so the auto-apply
sweep takes no LLM. `RemLlms::apply` is the cheap (Flash-tier)
backend the **light** dream compile runs on, not an apply slot.

## Auto-promote sub-job

Two passes share the `rem_promotions` LLM slot, the
`auto_promote_cap`, and the `applied` list: a **page →
sub-wiki emergence** pass (whole-page promotion, run first) and the
**paragraph → page** pass (per-page split, below). Both are the forma
fisica scale of [memory-model.md](../concepts/memory-model.md) — line→page (paragraph) and
page→folder (sub-wiki).

Both passes are **act-first** ("apply + notice"): a structural change is
never a blocking proposal the user must approve. The pass applies the
change **directly in-cycle**, records a **born-applied** `wiki_promote`
receipt (status `applied`, `revert_token` + 7-day `revert_deadline`
minted at insert — the undo anchor), and emits **exactly one
`structure_applied` notice** on the event stream at the apply site. The
notice payload carries the variant, source → target, the
`revert_deadline`, the undo `dashboard_path`
(`/dashboard/proposals/<id>/open-in-chat`), and the **`recipient_id` of
the affected user** (derived from the triggering fact via
`proposals::recipient_from_fact`) so a multi-user consumer knows whom to
forward it to. The dashboard is the **undo** surface — *le grosse si
fanno vedere*: if the LLM promotes badly, the user/admin reverts or
declasses from there; the product never stops to ask permission.

### Page → sub-wiki emergence pass

Per wiki, *before* the paragraph pass, for each **non-`index.md`** page
whose `page_mass >= auto_promote_subwiki_min_page_facts` (default 20):

1. Skip `index.md` (the wiki's own root/hub — promoting it would delete
   the index), any page whose normalized `style:` testata is **`lista`**
   (a list is structurally terminal — it never emerges into a sub-wiki on
   mass; list items that outgrow the list promote individually through
   the paragraph→page rung first, after which the container is a folder
   of pages, not a list), and any page where a fact is already covered
   by a `wiki_promote` row (receipt or legacy pending). The lista skip is a **form
   invariant**, not a semantic gate: the LLM still makes every judgement
   *inside* a prosa page and still decides whether a prosa page has grown
   into a subject area. **No recall floor** applies —
   emergence is mass-driven, so a fresh wiki with a
   dense page can emerge before any recall accrues.
2. Ask the `rem_promotions` LLM (`rem-subwiki-emergence` prompt) with
   the page's fact bodies, the page mass, and the **parent wiki's total
   active facts** (weigh the page against its parent): strict JSON
   `{"promote", "slug", "style", "description"}`. `style` is the emerged
   wiki's dominant style **default** (`prosa`/`prosa-tecnica`/`lista`, or
   `null` = generic) and `description` is a free-text "what goes in here"
   whose wording encodes how strict the hint is — both a **hint, not a
   gate** ([memory-model.md](../concepts/memory-model.md)).
3. On `promote: true`, call `promote::apply_file_to_subwiki_direct` with
   **every** active fact on the page (the handler refuses partial moves):
   the sub-wiki is created on the spot, the born-applied receipt recorded,
   and the `structure_applied` notice emitted (`variant: file_to_subwiki`,
   plus the `new_wiki_id` from the apply spec). The `style` +
   `description` ride in the receipt **context** (emergence-decided, not
   operator-chosen); the apply stamps them onto the emerged wiki's `_meta`
   (`extra["style"]` validated to the closed palette, `extra["summary"]`)
   so the new wiki is **not born blind** to placement + recall navigation
   (the metadata is deposited ahead, like the
   `keywords`/`summary` of recall-nav prep).

Running emergence first gives whole-page promotion **precedence**: the
facts it moves become `already_promoted_for`, so the paragraph pass
below skips carving them piecemeal. The threshold is a resource
pre-filter (default 20, overridable as
`rem.policy.auto_promote_subwiki_min_page_facts`); the LLM makes the
emerge verdict — no semantic gate.

### Paragraph → page split pass (per page)

The line→page rung is judged **per page, not per fact**: the internal
LLM reads the whole page and decides whether to split it. For each page
in every wiki:

1. Apply the **mass pre-filter** —
   `page_mass >= auto_promote_min_page_facts` (default 8, where
   `page_mass` is the number of active facts sharing the page's
   `source_path`). This is the **only deterministic gate**, a cheap
   **resource** pre-filter (skip thin pages before asking the LLM) —
   there is **no recall floor**: recall is information the LLM weighs,
   not a hardcoded gate ([memory-model.md](../concepts/memory-model.md)).
   The trigger is **mass / ramification**, not the word count of any
   single fact: facts are atomic, so a sub-topic earns its own page once
   it has *accumulated* enough of them. The floor (plus
   `auto_promote_cap`) is overridable under `rem.policy:`
   in `mwe-mcp.config.yaml` (`RemConfig::resolved_policy`, honoured by the
   scheduler, the `rem run-cycle` CLI, **and** the Dream console) and
   editable live from the dashboard REM settings panel
   (`/dashboard/admin/rem-settings` — the scheduler snapshots the shared
   policy handle at each cycle start, so a save applies to the next
   cycle without a restart).
2. Skip pages where any fact is already covered by a `wiki_promote` row
   (an `applied` row is the receipt of a promote already performed, a
   legacy `pending` one is in flight) — the dedup check is a coarse
   `LIKE` over `context` so the same split does not pile up over
   multiple nights, and a page the emergence pass just moved wholesale
   is left alone.
3. Show the **whole page** to the `rem_promotions` LLM
   (`paragraph_split_prompt`): every fact annotated with its id and
   30-day recall count, so the model weighs **mass and recall
   together** — one sub-topic that outgrew its siblings and/or is hot is
   the candidate. Strict-JSON verdict:
   `{"split": true|false, "fact_ids": ["<id>", …], "target_page": "<filename.md>"}`.
4. Validate the named facts in Rust (each must be on the page; the set
   must be a **proper, non-empty subset** — moving everything is a
   rename, not a split: that is the page→sub-wiki rung) and call
   `promote::apply_paragraph_to_file_direct` with the LLM's
   `target_page` canonicalised through
   [`planner::canonical_page_path`](../../crates/mwe-core/src/planner.rs)
   (the same chokepoint the [ingest classifier](ingest-pipeline.md)
   uses, so REM cannot coin a second spelling of an existing concept;
   the no-target fallback slugifies the first words of the fact body
   the same way) and then **flattened to the single-segment
   `<slug>.md` form** (plan pages never nest, and the plan re-home
   below keys the destination by slug): the facts move on the spot, the
   born-applied receipt is recorded, and exactly one `structure_applied`
   notice is emitted.
5. **Re-home the move in the persisted compilation plan**
   ([`planner::rehome_facts_in_persisted_plan`](../../crates/mwe-core/src/planner.rs)
   — the plan-sync seam, see the
   [narrative compiler](narrative-compiler.md#act-first-moves-and-the-plan--the-re-home-seam)):
   without it the planner's carry-over still assigns the moved facts to
   the source slug, and the next recompile of the source page pulls
   them back — silently undoing the split. The seam re-homes the facts
   onto the target slug (seeding the page + registry entry), and parks
   both pages on the plan's `force_dirty` so the next compile weaves
   the target. Soft: a re-home failure is reported, the applied move
   stands.
   **Both the `source_page` and `target_page` stored in the receipt are
   wiki-relative** (`index.md`, `acme_corp.md`) — the apply handler joins
   them onto the wiki's `abs_dir`, so the page's *workdir*-relative
   `source_path` (`wikis/<id>/index.md`) is stripped of the `wikis/<id>/`
   prefix via `wiki_relative_page` first; the same helper feeds the
   page→sub-wiki pass. The receipt
   and its notice both carry the **recipient** (0032), derived from the
   first moved fact with `proposals::recipient_from_fact` (the fact's
   `sender_id`, else the owning user, else `null`); the dedup sub-job
   does the same for its `DedupProposed` event, and the archive
   emitter carries `recipient_id: null` (no single fact in scope).

The sub-job is **gated by the LLM slot**: when `RemLlms.auto_promote`
is `None` (operator has not configured `llm.rem_promotions` in
`mwe-mcp.config.yaml`), it short-circuits cleanly with
`disabled_reason = Some("no rem_promotions LLM wired")`. This is the
right default — splitting without an LLM verdict over-promotes on size
alone; mass is only the entry ticket, the judgement is the model's.

Hard-capped by `policy.auto_promote_cap` (default 5/night).

The emergence of new wikis comes from the **mass-per-page** trigger
(auto-promote on page mass → the page→sub-wiki pass), not from
schema-shape clustering. See the emergence principle in
[memory-model.md](../concepts/memory-model.md).

## Page-merge sub-job (semantic page consolidation)

`run_page_merge` is the **cure front** of semantic page consolidation
(the [Conciliatore](narrative-compiler.md#stage-15--the-conciliatore-strong-model-dedup)
is the prevention front) — the inverse verb of the split above: where auto-promote carves a grown page
apart, the merge folds near-synonym concept pages back together (the
dogfood's `viaggi` / `viaggi_parigi_2026` / `lista_viaggio_parigi`
fragmentation, which measurably degraded recall-as-navigation). Pipeline,
per cycle:

1. **Nominate** (deterministic, no LLM): candidate pairs of fact-bearing
   **concept leaves of the same [family line](#family-scope--the-consolidation-passes-unit-leva-2)**
   (same wiki, or straddling the parent↔sub-wiki boundary — never an
   arbitrary wiki pair), from two structural signals —
   the [reviewer](narrative-compiler.md#the-reviewer)'s `duplicate_prose`
   pairs over the compiled bodies (finally consumed by someone), and
   **page-name kinship** (a shared long slug token, `viaggi` /
   `viaggi_parigi_2026`, or a long common prefix, `presenze` /
   `presenza`). Nomination needs the persisted compilation plan; before
   the first compile the sub-job is a no-op. Capped at
   `policy.page_merge_cap` pairs per cycle (default 3, `0` disables) — a
   **resource** cap on confirmation spend, not a semantic gate.
2. **Confirm** (the mandatory LLM call — a name resemblance is *never*
   sufficient): the `rem_dedup_semantic` slot judges the pair through the
   [`rem-merge` prompt](../../crates/mwe-core/prompts/rem-merge.md) — both
   pages' identity (incl. each page's `wiki:` on a straddling pair) +
   numbered claims — and returns strict JSON
   `{"merge", "survivor", "reason"}`. The same call **picks the
   survivor** (the better long-term name; on a parent↔sub-wiki retelling
   the prompt leans toward the subject's own sub-wiki page). Fail-safe:
   an unparseable verdict or an unknown survivor slug means no merge — a
   wrong merge is more disruptive than a wrong split.
3. **Execute act-first** via `promote::apply_page_merge_direct`
   ([promote handler](proposal-apply-engine.md#promote-handler), variant
   `page_merge`): every active fact of the husk moves onto the survivor
   (DB-first commit point; a pair that crossed the family line re-homes
   each row's `wiki_id` too — `fact_index::move_to_wiki`, the only
   primitive that flips it, per-fragment ACL untouched), the husk file
   is deleted, and the persisted plan is re-homed (husk out of plan +
   registry, survivor parked on `force_dirty` so the next compile weaves
   the appended records into prose — seeded in the survivor's own wiki).
   A born-applied receipt opens the revert window and the
   `structure_applied` notice points the affected user at the dashboard —
   the undo surface, where the revert recreates the husk from the shell
   stored in the spec (and walks the rows' `wiki_id` back across the
   line; pre-family receipts carry no `target_wiki_id` and revert
   single-wiki as before). Pinned by
   `page_merge_crosses_the_family_line_and_reverts`.

Two standing guards: a pair with **any** prior `page_merge` receipt —
including a **reverted** one, which is the operator's veto — is never
re-judged (`merge_already_judged`); and a husk whose `fact_index` rows
are not all settled on its compiled page (pending renders) is skipped
for the cycle and retried once the compiler has caught up.

The first sub-job that emits into `archive_proposals` (see the
[engine DDL](engine-db-and-migrations.md)), a flow **separate** from
`structure_proposals`. The detector targets filesystem-level archival:
pages that have not been recalled or touched in a long time.

For each non-smart wiki, for each `source_path` (page):

1. Compute the page's *freshest* timestamp — the maximum over its active
   facts of `last_recall_at` (falling back to `created_at` when the
   recall stamp is null).
2. Apply the deterministic rule: the freshest timestamp is older than
   `now - policy.archive_inactivity` (default 365 days). One recent
   recall is enough to keep the whole page off the queue. The emitter
   writes the [`archive::reason::NO_RECALL_HIT_365D`](../../crates/mwe-core/src/archive.rs)
   reason; a `NO_MODIFY_180D` reason is reserved in `archive::reason`
   for a follow-up sub-job that also weighs modification recency.
3. Call `archive::already_proposed(pool, wiki_id, path)` to skip
   duplicate emissions across nights.
4. Otherwise call `archive::emit_archive_proposal(...)` to insert one
   `archive_proposals` row with `status=pending`. The actual apply
   (filesystem move into `_archive/` + cascade `wiki_forget`) and
   approval UI are not yet implemented — the detector emits proposals
   today, but no reaper flips a row to `applied`.

The detector does **not** call the LLM — the binary "stale enough?"
question is deterministic. This is the cheapest emitting sub-job of the
cycle.

Cap: `policy.archive_cap` (default 10) keeps a freshly loaded workdir
from emitting a deluge on the first night.

## Completion sweep sub-job

The REM half of the closure verb — the safety net behind the
[ingest closure path](ingest-pipeline.md#the-closure-verb--completion--the-relayed-forget-gesture):
ingest closes the open items its own recall window shows it; this
sub-job catches the completions ingest could not see, with the global
view ([`run_completion_sweep`](../../crates/mwe-core/src/rem.rs)).

1. **Evidence**: every active fact of a non-smart wiki whose
   `created_at` falls inside `policy.closure_sweep_window`
   (default 48 h) — bounding the sweep to what just landed, so the
   corpus is never re-judged wholesale. The reserved `rules.md` policy
   page is fenced out on **both axes** (structural perimeter, like the
   dedup rules-boundary): a standing directive is policy, not an event —
   it completes nothing (the live incident: one user's naming rule read
   as evidence "completing" another user's parallel naming rule), and it
   is never completed by neighbouring evidence — a rule leaves the
   channel only via supersede, tombstone, or its owner's explicit
   closure.
2. **Nomination** (no LLM): for each evidence fact, the most similar
   **open** facts of the same
   [family line](#family-scope--the-consolidation-passes-unit-leva-2)
   (embedding cosine, top 3, older than the evidence,
   `valid_to IS NULL`, never a rules-page fact — evidence landing in the
   parent wiki can complete an open item in the sub-wiki and vice
   versa). Similarity **nominates only** — a resource cap, not a
   semantic gate. Evidence with no open candidate
   never reaches the LLM. Newest evidence first, capped by
   `policy.completion_sweep_cap` (default 8; `0` disables). The candidate
   snapshot is loaded **once** per sweep, so two evidence facts can both
   nominate the same open item; a candidate already closed earlier in the
   **same cycle** is dropped before confirmation, so a shared item closes
   exactly once (no redundant receipts, and the single receipt's revert
   snapshot stays the pre-closure state).
3. **Confirmation** (LLM, `rem_dedup_semantic` / revisor slot — the
   low-tier confirmer shared by every REM verdict sweep): the
   [`rem-completion`](../../crates/mwe-core/prompts/rem-completion.md)
   prompt asks what the evidence actually *completed* — closure requires
   **positive evidence the action took place**, never a merely related
   fact. The guards: discussing / advising on / helping plan an item is
   not completing it; a **restatement** or near-duplicate of the candidate
   ("necessita di X" vs "ha l'indicazione per X") is a *dedup* case, not a
   completion; a **standing** condition / decision / medical indication /
   diagnosis is not a consumable intention (it closes only on evidence the
   procedure or event happened); a **future** plan does not complete before
   its time; an **episode**, a record of what already happened, never
   closes. The confirmer also resolves the completion instant against the
   evidence's capture date. Targets must come from the candidate list
   (anti-hallucination); an empty verdict, an LLM outage, or an unparseable
   answer are all no-ops.
4. **Execution** (act-first): each confirmed target closes via
   `fact_index::close_validity` (`decay_reason = completed`; `valid_to`
   = the confirmer's resolved instant, else the evidence's capture
   date; `successor_fact_id` = the **evidence fact** — it states the
   outcome the closed item was waiting for, so the compiled page can
   point at its home, the
   [succession pointer](narrative-compiler.md#the-succession-pointer--one-hop-from-the-obituary-to-todays-truth)),
   inside a `completion_close_apply` WAL op, then the same paper
   trail as the ingest half — one born-applied
   [`validity_close` receipt](proposal-apply-engine.md#promote-handler)
   per evidence fact + the `structure_applied` notice. The dashboard
   stays the one undo surface; the recompile rides the render-content
   fingerprint.

## Cross-wiki refile sweep sub-job

The **LLM-decided refile** of a single misfiled fact into a different
**existing** wiki ([`run_refile_sweep`](../../crates/mwe-core/src/rem.rs)).
A fact captured into wiki A that is really *about* wiki B's subject is
moved to B, act-first and revertible — the same authority model as the
auto-promote / completion / contradiction sweeps (the dashboard revert is
the safety net).

1. **Views**: every **non-smart** wiki + its active facts. Smart wikis
   are excluded as **both** source and destination — the smart-family is
   the consumer's, and refiling into/out of it would corrupt the
   ownership boundary (smart rows carry projected wiki-level ACL).
2. **Nomination** (no LLM), two feeds:
   - **Reviewer-fed** (the findings→healing bridge): the
     `refile_candidates` last night's post-compile review parked on the
     plan (each `cross_subject_bloat` fact — see
     [narrative-compiler §the findings→healing bridge](narrative-compiler.md#the-findingshealing-bridge))
     are drained (`planner::take_refile_candidates`, one judge pass per
     nomination — the review re-parks whatever still stands) and seeded
     **past the cosine margin**: the reviewer already nominated them.
     A parked id that vanished or re-homed since is silently done.
   - **Cosine-computed**: for each active fact, compare its best
     embedding cosine to its **home** wiki's other facts against its best
     cosine to facts in **other** non-smart wikis; nominate when a foreign
     wiki beats home by at least a fixed margin. The pre-filter
     **nominates only** — a resource cap, never a "belongs elsewhere"
     threshold ([[feedback-no-hardcoded-gates-llm-decides]]).

   A `rules.md` fact is **never nominated** by either feed
   ([`wiki::is_rules_page`](../../crates/mwe-core/src/wiki.rs)): a
   per-user behaviour rule embeds toward its *user's* wiki by nature, and
   a confirmed move would eject it from the behaviour-rules channel — the
   refile twin of the compiler-door skip
   ([ingest-pipeline.md](ingest-pipeline.md#agent-behaviour-rules--routed-by-scope-outside-fact-memory));
   rules facts still count in the similarity pools. Reviewer-fed first,
   then fresh facts (inside `policy.closure_sweep_window`), then
   newest-first, capped by `policy.refile_sweep_cap` (default 5; `0`
   disables — the parked candidates stay parked while disabled).
3. **Verdict** (LLM, `rem_dedup_semantic` / revisor slot — the low-tier
   confirmer shared by every REM verdict sweep): the
   [`rem-refile`](../../crates/mwe-core/prompts/rem-refile.md) prompt asks
   whether the fact belongs in a different wiki and which (chosen only
   from the candidate foreign wikis). The judge picks the **wiki only** —
   a `dest_page` in its JSON is deliberately ignored. Instructed to be
   conservative (topical similarity is not misfiling); a `stay` verdict,
   a non-candidate dest, an LLM outage, or an unparseable answer are all
   no-ops.
4. **Execution** (act-first): the confirmed move runs via
   [`promote::apply_fact_refile_direct`](proposal-apply-engine.md#promote-handler)
   — the `fact_refile` `wiki_promote` variant repoints the row's
   `wiki_id` (`fact_index::move_to_wiki`, the only primitive that touches
   `wiki_id`), splices the marker off A's page and weaves it onto **B's
   `index.md`** — always the foundation page, because the plan keys pages
   by a bare slug across the whole forest, so landing on a *named* page
   of a foreign wiki could collide with a same-slug page homed elsewhere
   (a cross-wiki leak); the id-keyed index is collision-safe — and
   re-homes the persisted plan onto the dest page (force-dirtying both
   source and dest) — wrapped in a `fact_refile_apply` WAL op, with one
   born-applied `wiki_promote` receipt + the `structure_applied` notice.
   The dashboard stays the one undo surface. The index is a **landing
   pad, not the final home**: as deposits accumulate, the reviewer's
   `oversized` nomination (and `cross_subject_bloat`, when the deposit is
   foreign to the identity index — the agent-wiki case) parks a placement
   re-open, and the next full's Cartografo distributes the pile onto the
   right pages
   ([narrative-compiler §reviewer](narrative-compiler.md#the-reviewer)).

> The gold-set self-correction discipline (a recall-eval regression gate
> for REM's structural moves) stays in roadmap group 15;
> this sub-job is the LLM-decided refile only, with the dashboard revert
> as the safety net.

## Contradiction sweep sub-job

The **cluster half** of the temporal-validity model
([`run_contradiction_sweep`](../../crates/mwe-core/src/rem.rs)): a
contradiction lands on one fact — the supersede chokepoint, or an ingest
`contradicted` closure — while its **satellites** stay wrongly open (the
dogfood's cancelled trip whose itinerary days kept feeding the due-soon
slot). The ingest path closes the satellites its recall window shows it
(prompt v2.23's third closure reason); this sub-job follows the cluster
with the global view.

1. **Seeds**: rows freshly closed by a contradiction
   ([`fact_index::find_recently_contradicted`](../../crates/mwe-core/src/fact_index.rs)
   — superseded, or closure-verb `contradicted` — inside
   `policy.closure_sweep_window`), pooled per
   [family line](#family-scope--the-consolidation-passes-unit-leva-2)
   (non-smart only).
2. **Nomination** (no LLM): the seed's most similar **open** facts of
   the same family line (embedding cosine, top 5 — a contradiction
   landing in the parent wiki can fell its satellites in the sub-wiki
   and vice versa). Two structural fences on the candidate pool (never a
   semantic gate — the cluster judgment stays the LLM's): the seed's
   whole **successor lineage** (`superseded_by` walked transitively) is
   off-limits — a fact revised twice is otherwise nominatable as a
   "satellite" of its own grandparent, and the sweep would cannibalise
   the very revision that contradicted the seed (observed live
   2026-07-01: the freshly revised TTS rules fell as satellites of their
   own dead predecessors); and **rules-page facts are never
   candidates** — a standing directive leaves the channel only via
   supersede, tombstone, or its owner's explicit closure, never as
   collateral of a neighbouring contradiction. Similarity nominates
   only; freshest seeds first, capped by
   `policy.contradiction_sweep_cap` (default 8; `0` disables).
3. **Confirmation** (LLM, `rem_dedup_semantic` / revisor slot — the
   low-tier confirmer shared by every REM verdict sweep): the
   [`rem-contradiction`](../../crates/mwe-core/prompts/rem-contradiction.md)
   prompt shows the seed, the successor statement when one exists, and
   the candidates, and asks which candidates only made sense while the
   seed held — instructed to be conservative (topic overlap is not
   invalidation; a fact that survives on its own merits stays open).
   The **cluster definition is the LLM's judgment** — same page, same
   topics, none of it is hardcoded.
4. **Execution** (act-first): confirmed satellites close as
   `contradicted` with `valid_to` anchored to the **seed's own closure
   instant** (the moment the event fell — which also drops them out of
   the due-soon slot) and `successor_fact_id` inherited from the
   **seed's superseding fact** when it has one (the satellites fell with
   the seed, so they point at the same replacement — the
   [succession pointer](narrative-compiler.md#the-succession-pointer--one-hop-from-the-obituary-to-todays-truth);
   a seed closed without a successor stamps none), inside a
   `contradiction_close_apply` WAL op, with the shared
   `validity_close` receipt + `structure_applied` notice.

## Recall-repair sub-job — self-correcting REM's repair stage

The consumer of the [hindsight miss records](recall-pipeline.md#the-hindsight-log--the-judge-free-miss-signal)
([`run_recall_repair`](../../crates/mwe-core/src/rem.rs)): each pending
`recall_misses` row — the user restated a fact memory held and that
turn's recall did not surface — is judged for the lowest-blast-radius
repair, and **nothing commits on an LLM's opinion alone**.

1. **Intake** (deterministic): pending misses, oldest first, capped by
   `policy.recall_repair_cap` (default 3; `0` disables). A miss whose
   fact is gone/superseded resolves `stale`; smart-wiki and rules-page
   homes are out of scope. Every real miss also appends a **candidate
   gold case** to `<workdir>/recall-gold-candidates.yaml`
   ([`recall_gate::append_gold_candidate`]) — the loop that grows the
   eval harness from the system's own confirmed failures; the operator
   reviews and merges candidates into `recall-gold.yaml` by hand, never
   automatically (a noisy case must not become the judge).
2. **Proposal** (LLM, revisor slot — the shared low-tier confirmer):
   the [`rem-recall-repair`](../../crates/mwe-core/prompts/rem-recall-repair.md)
   prompt sees the missed query, the fact, its home, and the non-smart
   wiki roster, and proposes a **re-file** (destination wiki only —
   landing on its foundation `index.md`, the refile sweep's own
   discipline) or `stay`. Conservative by instruction; anti-hallucination
   vets the destination against the roster.
3. **The gold-set gate** ([`recall_gate::gate_repair`](../../crates/mwe-core/src/recall_gate.rs)):
   the candidate move is applied to a **scratch snapshot** of the workdir
   (`VACUUM INTO` + a `wikis/` copy) and two replays decide — the
   **target check** (does the missed query now surface the fact, judged
   by fact id in the flat top-K or by its home page among the navigated
   fragments; the query replays with the miss's own classifier topic
   seeds, `RemLlms.navigator` drives the funnel — absent navigator ⇒
   flat-only, so navigation repairs simply never prove and never
   commit), and the **gold-set regression** (`<workdir>/recall-gold.yaml`
   replayed via [`recall_eval`](recall-pipeline.md#the-recall-eval-harness--recall_eval)
   before/after — per-query coverage must not drop; an absent gold set
   degrades the gate to the target check, a malformed one skips the
   sub-job loudly). A baseline that already surfaces the target resolves
   the miss `stale` — the corpus healed itself.
4. **Commit / queue**: a proven repair re-applies on the real workdir —
   the same act-first mover as the refile sweep
   ([`promote::apply_fact_refile_direct`]: born-applied receipt, 7-day
   revert, `structure_applied` notice with `variant:
   "recall_repair_refile"`) — and the miss resolves `repaired` with the
   receipt id. An unproven or unproposed repair discards — unless the
   same fact has missed `policy.recall_tuning_recurrence` times
   (default 3): then ONE `recall_tuning_proposed` notice per fact per
   cycle lands on `wiki_events` with the evidence (miss count, sample
   query, gate outcome) and the miss resolves `queued`. Rule / prompt /
   recall-knob levers are the highest blast radius in the system and are
   **never auto-applied** — the notice is the operator review queue.

## Provenance-hygiene sweep sub-job

Mechanical repair of a **known defect pattern** on canonical fact text
([`run_provenance_hygiene`](../../crates/mwe-core/src/rem.rs)) — data
repair, not a semantic gate. The document path's file phase used to
append the dossier backlink to the claim body (` ([[wiki/page]])`),
flooding the document page with inbound links, feeding link noise to
embeddings and dedup, and freezing prose the Cronista cannot restyle;
the designed provenance channel is `authored_refs`
([document ingest](document-ingest.md#provenance-and-acl)). The
go-forward writer is fixed; this sweep converges the pre-existing
corpus and then no-ops forever.

1. **Detect** (deterministic, no LLM): a fact whose text ends with one
   or more trailing parenthetical wikilinks ` ([[wiki/page]])` —
   `split_trailing_provenance_refs`, anchored on the **trailing**
   pattern only and pinned to the exact shape the worker emitted
   (whitespace-separated parenthetical, plain `wiki/page` target,
   non-empty claim before it). A wikilink **mid-prose** is legitimate
   content and never matches. Non-smart wikis only — smart-wiki rows
   are section projections of consumer-authored files.
2. **Repair** (per fact, oldest first, capped by
   `policy.provenance_hygiene_cap` — default 32, `0` disables,
   YAML-overridable as `rem.policy.provenance_hygiene_cap`; a resource
   cap on embedder spend, the sweep spends no LLM): the pointer moves
   into `authored_refs` (dedup'd — re-running over a partially repaired
   corpus never double-records), the suffix is stripped from the text,
   the cleaned text is re-embedded, and text + embedding +
   `authored_refs` are written in **one atomic statement**
   (`fact_index::update_region_and_authored_refs`, offsets kept, ACL
   untouched) inside a `provenance_hygiene_apply` WAL op, with the old
   and new text in the trace log and the per-fix + count in the cycle
   report (`ProvenanceHygieneReport`).
3. The row's text now disagrees with the rendered prose — the drift the
   [render-content fingerprint](narrative-compiler.md#page_fingerprint--the-dirty-set)
   notices, so the compile that follows the cycle rewrites exactly the
   touched pages.

It lives in the **full cycle** next to the date normalizer — its
closest sibling, the other canonical-text edit + re-embed pass — not in
the light dream (whose retirement sweep is page-level region excision);
running immediately before the normalizer means the same cycle's LLM
passes already judge pointer-clean text.

## Date normalizer sub-job

Relative→absolute date normalization on **canonical fact text**
([`run_date_normalizer`](../../crates/mwe-core/src/rem.rs)). Capture-side
resolution (the ingest prompt's `current_time` anchor) handles new
facts; this heals what slipped through and the pre-existing backlog —
the dogfood's *"oggi ha giocato 31 minuti"* still reading "oggi" days
later.

1. **Flag** (no LLM): a small Italian+English deictic lexicon
   (`looks_deictic`, word-boundary, case-insensitive) marks candidate
   facts across the non-smart wikis. A resource pre-filter only — the
   LLM decides whether a flagged fact really needs the rewrite, and an
   unflagged miss waits for a richer lexicon rather than a wrong guess.
2. **Rewrite** (one batched LLM call, `rem_dedup_semantic` / revisor slot
   — the low-tier confirmer shared by every REM verdict sweep): the
   [`rem-dates`](../../crates/mwe-core/prompts/rem-dates.md) prompt
   receives the flagged facts — oldest first, capped by
   `policy.date_normalize_cap` (default 16; `0` disables) — each with
   its own capture instant, and resolves every relative phrase against
   **that fact's** date, never against tonight. The anchor fed per fact
   is the **semantic** capture instant: `valid_from` (the stored
   projection of the turn's `occurred_at` clock) when present,
   `created_at` only as fallback — a replayed or backfilled fact
   resolves "oggi" against the day it was *uttered*, not the wall-clock
   day its row was inserted. Everything else in the
   text must stay identical; omitting a fact is always safe.
3. **Apply**: each accepted rewrite (batch-membership checked, marker
   characters refused, no-op skipped) is re-embedded and written
   in place via `fact_index::update_region` — offsets kept, ACL
   untouched — inside a `date_normalize_apply` WAL op, with the old and
   new text in the trace log. The row's text now disagrees with the
   rendered prose, which is exactly the drift the
   [render-content fingerprint](narrative-compiler.md#page_fingerprint--the-dirty-set)
   notices: the next compile rewrites exactly the touched pages, so
   prose and `lista` records alike stop rotting.

## Hub Writer sub-job

The last sub-job in the cycle, and one of the simplest:

- Trigger: wiki has children **and** at least one active fact **and**
  is not in the smart family **and** its `index.md` is not a page of the
  persisted compilation plan — the compiler is the writer of plan-owned
  indexes (`person` / `group_theme` / `emerged_index` foundation nodes),
  and a REM-side regeneration would fight it over the same file. With the
  Fonditore's topic-wiki pass this covers every standard wiki a plan has
  seen, so the sub-job serves only wikis outside a plan (or a workdir with
  no plan yet).
- For each qualifying wiki (bounded by `hub_writer_cap`, default 10):
  - Build a prompt from the wiki's title + type + children list + 20
    most-recent active facts.
  - Call the `hub_writer` LLM with `max_tokens=2000` and
    `temperature=0.2`.
  - `atomic_write` the response to `<wiki_dir>/index.md`.

The "regen on every cycle that qualifies" model intentionally skips
the "have children changed since the last regen?" detection: the cost
of a hub_writer call with a small model is small, and the regen is
idempotent (atomic_write handles partial writes; the next cycle
overwrites whatever we wrote last night). Tracking last-hub-run state
in a side table is a later optimisation if profiling demands it.

## Briefing dispatcher sub-job

For every smart-family wiki (per the cycle-scoped `SmartWikiIndex`),
scans its **sections** (`wiki_sections` — a smart wiki has no
`fact_index` rows) for two flavours of finding and posts one
`_briefing.md` item per finding via [`briefing::notify_as_rem`]:

| Finding | Trigger | Source ref |
|---|---|---|
| **Stale draft** | YAML body has top-level `status: draft` and `created_at < now - briefing_stale_draft_age` (default 14 days). | `rem:briefing_dispatcher:stale_draft:<source_path>#<ord>` |
| **Recall-hot** | `wiki_sections.recall_count_30d >= briefing_recall_hot_threshold` (default 20). | `rem:briefing_dispatcher:recall_hot:<source_path>#<ord>` |

Idempotency: a deterministic `source_ref` keyed by `(wiki_id, source_ref)`
absorbs the same finding for `briefing_dedup_window` (default 7 days).
The ref keys on the section's **positional** handle, which survives an
edit elsewhere on the page — so the dedup window holds instead of
re-firing under a fresh key every time the page is touched.
Per-wiki cap: `briefing_notify_cap` (default 10). The briefing inbox's
own `50/wiki/h` cap from [`briefing::NOTIFY_RATE_PER_HOUR`] still
applies as the global backstop.

`notify_as_rem` bypasses the user ACL check (REM is a server-internal
actor with no `sender_id`), forces `source_kind = Rem` regardless of
the input, and shares the validate → rate-limit → DB → filesystem
pipeline with the user-facing `notify` via a private `notify_append`
helper.

## Backlink reciprocity detector sub-job

Builds a `(target smart wiki, source_wiki)` matrix in one pass:

1. Collect smart wikis + cache their **section** bodies once
   (`wiki_sections`).
2. For each non-smart source wiki, scan each active fact body with
   [`recall::extract_wikilink_wiki_ids`] (made `pub` for this use).
3. For each `[[<wiki_id>...]]` whose target is a smart wiki of step 1,
   check whether at least one section body in the target smart wiki
   mentions `[[<source_wiki_id>...]]`. If not, the inverse is missing.
4. Post one `_briefing.md` item on the smart wiki with source_ref
   `rem:backlink_reciprocity:<source_wiki_id>` (also dedup-keyed) and
   the same per-wiki cap.

The reciprocity check is intentionally narrow: the smart consumer
decides what "good back-link shape" means — REM only points at the
gap.

## Lease expirer sub-job

Thin SQL housekeeper for the `wiki_admin_leases` table populated by
the [`wiki_admin_leases::acquire`] / [`release`][rls] API (see
[`smart-wikis.md` milestone table][cw] for the rationale —
optional cooperative lease for multi-device authoring on a single
smart-wiki). The whole job is one delegation to
[`wiki_admin_leases::expire_stale`], driven by two policy knobs on
[`RemPolicy`]:

[rls]: ../../crates/mwe-core/src/wiki_admin_leases.rs
[cw]: smart-wikis.md

- `lease_expirer_grace` (default `chrono::Duration::hours(1)`) — an
  active row (`released_at IS NULL`) with `expires_at < now - grace`
  is treated as crashed-without-release; the job stamps
  `released_at = now` so the slot frees up for the next caller.
- `lease_expirer_retention` (default `chrono::Duration::days(7)`) —
  released rows older than `released_at + retention` are deleted
  outright. The retention window doubles as the dashboard
  `/wikis/<id>/op-log` visibility budget for past leases.

Both numbers count seconds at SQL time; the helper passes them
straight through to two `UPDATE` / `DELETE` statements without
fetching rows into Rust memory. The job emits no `wiki_events`,
opens no LLM call, does not touch the filesystem — it is the
lowest-cost sub-job of the cycle, run after the two smart-wiki
emitters so the same cycle that surfaces stale-draft / backlink
findings to the briefing also cleans up the lease audit tail. No
per-row reporting; the [`ExpirerReport`] just carries
`stale_active_marked_released` + `aged_released_rows_deleted` as
unsigned counts. The only failure path is SQL infrastructure,
which bubbles as [`RemError::Db`].

## Briefing-processor non-smart sub-job

This sub-job closes the comments loop on standard wikis.
Smart wikis are drained by the smart consumer at
`smart_bootstrap` via `mark_processed` on the next `wiki_admin_push`;
non-smart wikis (the identity wikis and every emerged
sub-wiki) have no smart consumer, so
`wiki_briefing_items` rows on them would pile up forever with
`processed_at IS NULL`. REM fills the gap with sub-job 11, which
calls the shared core function
[`briefing_processor::process_briefing_item`] — the same function
the dashboard "Submit" endpoint
`POST /dashboard/wiki/:id/briefing-items/:bi_id/process` invokes
synchronously when the operator wants the row drained right away.
One branch, two callers, no drift — for the **mark-passive** path.
Standard wikis take the **action-taking** path below instead (and do
not offer a per-comment Submit — see [the compiler note](narrative-compiler.md#human-edits-on-compiled-pages)).

[bp]: ../../crates/mwe-core/src/rem/briefing_processor.rs

**Policy: action-taking on standard wikis, mark-passive elsewhere.**
For a **standard** wiki, when the `ingest` slot is wired the sub-job now does
**action-taking**: it batches that wiki's pending comments per anchored page and
applies each as a contained fact op — `correct` / `remove` / `add` / `move` — via
[`comment_apply::apply_comments`](../../crates/mwe-core/src/comment_apply.rs);
the [content-aware fingerprint](narrative-compiler.md#page_fingerprint--the-dirty-set)
then makes the compile pass recompile only the touched page(s). A `move` is the
operator's relocation intent ("questo starebbe meglio sulla wiki salute"): the LLM
picks a destination from a bounded list of the wiki owner's other non-smart wikis
+ this wiki's other pages, and the fact moves act-first via the same engine the
[cross-wiki refile sweep](#cross-wiki-refile-sweep-sub-job) uses
(`promote::apply_paragraph_to_file_direct` same-wiki, `promote::apply_fact_refile_direct`
cross-wiki onto the dest `index.md`) — born-applied + revertible, unlike the bare
`correct` / `remove` / `add`. Containment + ACL invariants are described in
[the compiler note](narrative-compiler.md#human-edits-on-compiled-pages). This
is the batched dream applying the parked comments together — the maintainer's
"comments stay until a dream applies them all" — so it runs in the **full cycle**
(nightly or admin "run REM"), never the frequent light dream, and never a
user-triggered per-comment click.

**Mark-passive** remains the policy for structured non-smart types, and the
fallback for standard wikis when the `ingest` slot is unconfigured: the
processor stamps `processed_at = NOW()` after a pro-forma read of the cited
target, with zero LLM calls and zero structural mutations
([`briefing_processor::process_briefing_item`]). The
[`ProcessOutcome::Processed`] variant exposes a `context_loaded`
flag so the operator can see how often REM was given a citable
anchor to read versus a free comment without a target.

Two policy knobs on [`RemPolicy`]:

- `briefing_processor_enabled` (default `true`) — master switch for
  the sub-job. When `false` the cycle returns an empty
  [`BriefingProcessorReport`] without scanning.
- `briefing_processor_grace` (default `chrono::Duration::minutes(15)`) —
  rows newer than `now - grace` are left alone (the operator might
  still be editing the comment via the dashboard). The synchronous
  Submit endpoint bypasses the grace — it is an explicit "drain this
  one now" signal. YAML-overridable as
  `rem.policy.briefing_processor_grace_secs` and editable live from
  the dashboard REM settings panel (`/dashboard/admin/rem-settings` —
  see [config schema §rem](../protocol/config-schema.md#rem)).

Per-row outcomes from `process_briefing_item`:

- `Processed` → counted in `items_processed`. The row was eligible
  and stamped.
- `AlreadyProcessed` → counted in `items_already_processed`. Real-
  world cause: a synchronous Submit drained the row between the
  candidate scan and the per-row call.
- `WikiNotFound` → counted in `items_wiki_missing`. The `wiki_id` no
  longer resolves on disk (deleted wiki with rows still in the
  inbox). No DB write — the row is surfaced via the report so the
  operator can decide what to do.

Smart-wiki row classification reuses the cycle-scoped
`SmartWikiIndex` — the same map shared by every smart-wiki-aware
sub-job, so the "REM-maintainer vs
smart-consumer-maintainer" cut stays consistent across the whole
pipeline.

## Husk-page GC sub-job

Removes husk page **files**: plan-absent, non-reserved pages whose
fact rows are all tombstoned or superseded past the receipts' revert
window ([`run_husk_gc`](../../crates/mwe-core/src/rem.rs)). The
compiler's orphan sweep (`sweep_orphan_page_files`, every compile)
already drops a plan-absent file with **no** non-tombstoned rows, but
must keep one while a superseded row points at it — that row's on-disk
marker may still serve a revert. Once the window
(`proposals::REVERT_WINDOW`) is past, nothing can revert onto the page:
the file is a husk (a supersede's leftover obituary page, a placeholder
whose only fact fell — the delete-page verb's leftovers land here too),
and this sweep is the aggressive tail the orphan sweep defers to.

Deterministic, no LLM — a structural GC behind **DB-first guards**
(`fact_index::count_husk_blocking_rows`: any active row blocks, a
validity-closed row is still content; any supersession inside the
window blocks; tombstones never block, the same posture as the orphan
sweep), not a semantic judgment: every fact on a husk was already
closed by its own judged path. `index.md` / `rules.md` / `_`-prefixed
files never qualify; smart wikis are skipped (consumer-authored files
are never REM's to delete); **no plan on disk → no-op** (a fresh
workdir's pages are unplanned, not husks). Bounded by
`rem.policy.husk_gc_cap` per full cycle (default in
`RemPolicy::default()`; `0` disables; panel + YAML), path order so a
backlog drains deterministically; removals ride a `husk_gc_apply` WAL
op, each removed page's retired rows get their stale offsets settled
(`clear_region_offsets_retired_on_page` — the retirement sweep never
reopens a missing file), and the count surfaces in the dream summary
(`husk-gc N`).

Inbound links need no rewriter: a wikilink whose target vanished
degrades to **literal text** at render (the
[link grammar](recall-pipeline.md#link-grammar)'s dead-rail posture —
never a broken link) and the compile feed's
[dead-ref vetting](narrative-compiler.md#the-provenance-link--link-dont-duplicate)
keeps prose clean. Pinned by
`husk_gc_removes_plan_absent_pages_once_rows_are_past_any_revert` and
`husk_gc_keeps_active_recent_planned_and_reserved_pages`.

## Cycle invariants and crash semantics

- Every state-mutating sub-step is journaled in `rem_ops_log` via
  [`wal::begin_rem_op`] → `complete_rem_op` / `fail_rem_op`.
- The write-jobs' sub-step inverses are idempotent:
  - `atomic_write` handles partial `index.md` writes.
  - `mark_superseded` and `mark_forgotten` are no-ops on already-
    superseded / already-tombstoned rows.
  - `insert_event` is gated by `find_recent_event_for`.
- A cycle that crashes mid-job is safe to retry on the next REM tick.
  There is **no per-step rollback driver** today — it shares the same
  shape as the proposal-side WAL apply driver, which is also not yet
  built.
- Soft failures inside a sub-job (one wiki's template missing, one
  fact body that fails YAML parse, the LLM returning unparseable JSON
  on one candidate) are collected in the sub-job's `errors` list and
  the cycle continues.
- **LLM transport failures are fatal — in the reorg sub-jobs.**
  When a `complete()` call against the configured slot returns `Err`
  — Ollama down, network bug, API key revoked, model unloaded — the
  sub-job aborts with `RemError::Llm(String)` and so does the cycle.
  Rationale: the operator configured a specific model and expects
  that quality bar; soft-skipping silently the LLM call would degrade
  the memory inconsistently across nights. The next REM tick retries;
  in the meantime the error is loud (tracing + propagated `RemError`).
  This fatal model is deliberately scoped to the reorg: the narrative
  **compile pass** that follows the cycle handles LLM failures **per
  page** — transport errors and unparseable output alike get one retry,
  then the
  [degraded guard-only rewrite](narrative-compiler.md#degraded-mode--the-guard-only-rewrite)
  — so one flaky Cronista call can never cost the night's compile.
- Infrastructure failures (DB, filesystem, lockfile) bubble up
  as [`RemError`] as before.

## LLM-error semantics

Every LLM-using sub-job (revisor, auto_promote, hub_writer)
distinguishes two failure categories:

| Category | Example | Handling |
|---|---|---|
| Transport / config | Ollama not running, HTTP 500, timeout, model unloaded | `RemError::Llm(msg)` — sub-job + cycle abort |
| Verdict noise | LLM returned prose instead of strict JSON | soft — `report.errors.push`, continue with next candidate |

The boot-time + `doctor`-time `health_check_llm_slots` in
`mwe-mcp-server::main` catches most transport failures before the
listener binds — by the time REM runs, the slot is normally reachable.
A transport failure mid-REM means something regressed since boot
(Ollama restart, OOM, network blip); REM aborts and the next cycle
retries.

The **compile pass** is the exception to the abort row: `run_compile`'s
writers treat both categories per page (the Cronista through the
retry→degraded ladder, the Hub Writer as a plain soft error), so a
transport failure there costs at most one page's rewrite, never the pass
(see the
[degraded mode](narrative-compiler.md#degraded-mode--the-guard-only-rewrite)
and the [failure ledger](#per-page-compile-failure-surfacing)).

## Test coverage

The `#[cfg(test)]` modules in
[`rem.rs`](../../crates/mwe-core/src/rem.rs),
[`rem/briefing_processor.rs`](../../crates/mwe-core/src/rem/briefing_processor.rs),
`events.rs`, and `briefing.rs` are the SSOT for the
exact roster — the headlines:

- **events**: wire strings + insert payload roundtrip + null payload +
  dedup probe within window + dedup probe across kinds.
- **rem**: revisor, auto-promote, archive,
  auto-apply, auto-finalize, hub_writer (qualifying wiki + no-children +
  cap), the provenance-hygiene sweep (defect-shape-only detector,
  move+strip+re-embed, ref dedup + idempotence, cap, smart skip), the
  smart-wiki-aware sub-jobs
  (`briefing_dispatcher_emits_stale_draft_notify_for_smart_wiki`,
  `briefing_dispatcher_is_idempotent_across_cycles`,
  `briefing_dispatcher_skips_non_smart_wikis`,
  `backlink_reciprocity_emits_when_smart_wiki_lacks_inverse`,
  `backlink_reciprocity_skips_when_smart_wiki_has_reciprocal`,
  `legacy_write_jobs_skip_smart_family`), the
  briefing-processor drain (grace gate, mark-passive stamp, smart-wiki
  skip, `WikiNotFound` surfacing), and the helper parsers (`parse_llm_yes`,
  `parse_split_decision`,
  `default_target_page_slugifies_body_prefix`).
- **briefing**: the
  `notify_as_rem_bypasses_acl_and_forces_source_kind` and
  `notify_as_rem_still_rejects_non_smart`.

## Scheduler wiring

The scheduler drives **two cadences** from a single
`rem.schedule` config block: the nightly REM **full cycle** documented
above (`run_cycle`, LLM-backed, `interval_secs` default 24 h) and the
**light dream** (deterministic, no-LLM, far more frequent — see
[Light dream cadence](#light-dream-cadence)). They share
`rem.schedule.mode`: `disabled` turns **both** off. The light dream is
**not** a `run_cycle` sub-job — it is a separate, cheaper loop, not part
of the roster above. Both cadences — and the three manual entry points
(the two CLI hatches `run-cycle` / `run-light` and the dashboard trigger) —
share **one** definition of each dream via
[`mwe-core::dream`](../../crates/mwe-core/src/dream.rs)
(`run_compile` / `run_light` / `run_full`).

The plumbing that calls `run_cycle` (the full cycle) has two entry points:

- **Long-lived HTTP server.** [`cmd_serve_http`](../../crates/mwe-mcp-server/src/main.rs)
  builds an [`OwnedRemLlms`](../../crates/mwe-mcp-server/src/rem_scheduler.rs)
  bag (`hub_writer` + `rem_dedup_semantic` mandatory, `rem_promotions`
  + `ingest` optional) once at startup, then calls
  `rem_scheduler::spawn(...)` to fire one cycle after
  `rem.schedule.initial_delay_secs` and another every
  `rem.schedule.interval_secs` until ctrl-c. The shutdown signal is a
  `tokio::sync::broadcast::<()>` shared with axum's
  `with_graceful_shutdown`, so SIGINT exits the ticker cleanly without
  waiting for the next interval. Per-cycle failures log + retry at the
  next tick rather than crashing the server.
- **CLI escape hatch.** `mwe-mcp rem run-cycle` acquires the workdir
  lockfile (so it never races with a running `mwe-mcp serve`), builds
  the same bag, runs `rem_scheduler::run_once` (a **full dream** — reorg
  **plus** the compile pass), and prints a one-line summary per
  sub-job followed by the compile counts. Its sibling `mwe-mcp rem
  run-light` drives one light dream synchronously (same lockfile guard):
  it promotes the buffer and, when the LLM slots are configured, **also
  compiles** the pages the promotion dirtied. A third hatch `mwe-mcp rem
  run-compile` runs the compile pass alone. All three delegate to the
  single composition in
  [`mwe-core::dream`](../../crates/mwe-core/src/dream.rs), so the
  cycle+compile shape can never drift from the scheduler. The escape
  hatches are the right tool for headless deployments that drive REM
  from systemd / cron / a cloud scheduler; flip
  `rem.schedule.mode: disabled` in `mwe-mcp.config.yaml` so the
  in-process scheduler stays quiet in that setup.

Default profile: `mode: interval`, `interval_secs: 86_400` (nightly),
`initial_delay_secs: 300` (5 min warm-up). A fresh deployment
auto-organises memory out of the box; an operator who wants stricter
control can flip `mode: disabled` and the binary stays inert until the
external scheduler invokes the CLI.

### Light dream cadence

Alongside the full-cycle ticker, `cmd_serve_http` spawns
[`rem_scheduler::spawn_light`](../../crates/mwe-mcp-server/src/rem_scheduler.rs),
the **light dream** — a far more frequent, **deterministic**
loop that drains the [narrative captures buffer](narrative-buffer.md)
into `fact_index` via
[`mwe-core::dream_light`](../../crates/mwe-core/src/dream_light.rs). It
needs **no LLM bag** (promotion is exact-dup skip + embed + insert +
deterministic supersede-hint application — no model verdict), so it is
wired independently of the full cycle's `OwnedRemLlms` and runs even
when the REM LLM slots are unconfigured. An applied supersede hint also
performs the retirement **disk half** — the retired fact's on-disk region
is excised via `reindex::strip_fact_region`, best-effort, exactly like
`capture::wiki_supersede` ([redaction-policy](redaction-policy.md)).

`spawn_light` is a **"timer + threshold" poll loop**: it wakes on a short poll interval and
fires a light cycle when **either** `light_interval_secs` has elapsed
since the last run (the timer, default 1 h) **or** the buffered backlog
has reached `light_backlog_threshold` (the early trigger, default 20; a
threshold of `0` disables the early trigger and leaves the timer alone).
The new fields live on `RemScheduleConfig`
([`config.rs`](../../crates/mwe-core/src/config.rs)) and are all
serde-defaulted, so existing YAMLs keep parsing. Because both loops read
`rem.schedule.mode`, `mode: disabled` stops the light dream too; per-run
failures are soft (logged, retried at the next poll).

**Recall consequence.** A standard-wiki capture is *buffered but not yet
recallable* in the [buffered-not-yet-recallable](narrative-buffer.md#not-yet) gap; the light
dream **closes** that gap — once it promotes a capture (within
`light_interval_secs`, or sooner if the backlog crosses the threshold)
the claim is recallable.

**Compile pass.** Both cadences also run the narrative
compile pass via
[`mwe-core::dream::run_compile`](../../crates/mwe-core/src/dream.rs)
(→ [`mwe-core::compiler`](narrative-compiler.md)): the light dream compiles
after a promotion (`dream::run_light`, fresh pages without waiting for the
night — maintainer option 2), the REM full cycle compiles after the reorg
sub-jobs settle the fact set (`dream::run_full`). The two are distinguished by
a [`Cadence`](../../crates/mwe-core/src/dream.rs) argument: the light dream runs
at `Cadence::Light`, which **skips the strong-model Conciliatore** (it is
REM-only — [memory-model.md](../concepts/memory-model.md); the nightly `Cadence::Full` pass, and the
operator-driven compile, reconcile/dedup the pages the light dream accepted
as-is) and **runs every compile stage on the cheap ingest-tier (Flash)
backend** — the strong (Pro) slots are REM-only (see the
[tier-per-cadence note](narrative-compiler.md#the-strong-model-tier)). The pass
rebuilds the plan incrementally and writes only the dirty pages (cost-guard),
then ends with the deterministic post-compile reviewer
([narrative-compiler.md §The reviewer](narrative-compiler.md#the-reviewer)),
whose findings include **`cross_subject_bloat`**: an identity index (a
`wiki-user`'s `index.md`, the agent wiki included) whose plan carries a
**foreign-subject** fact — owner is a different user, or a group the page's
user is not a member of (enrollment-fed `reviewer::IdentityContext`, loaded
best-effort by `dream::run_compile`). Observability for the Cartografo's
identity-page discipline — counts in the report/log, never a gate. The pass is **skipped**
when the `cronista` slot is unconfigured — so the light dream still promotes,
it just cannot write prose. For this, the full-cycle `OwnedRemLlms` (carrying
the `cronista` slot) is built once and shared with the light dream via an
`Arc`, then projected to `RemLlms` (which carries a `cronista` field) on
each tick.

**Retirement hygiene sweep.** Every light dream ends with
[`reindex::sweep_retired_regions`](../../crates/mwe-core/src/reindex.rs)
(called from `dream::run_light`, best-effort — a failure never fails the
dream): it excises retired-fact regions still sitting on pages **outside
the current compilation plan** (`rules.md`, husk pages — where residue is
otherwise permanent; plan pages self-clean at their next compile). This
is the convergent backstop behind the act-time strips, covering the
retire paths that resolve inside the proposal apply chassis (a pending
`dedup_merge` apply, the silent-deadline `fact_forget` sweep) with no
engine context at hand. Candidates come from retired rows still holding
region offsets; each processed page re-parses its markers (robust to
offset drift), excises the retired ones, and settles the rows' offsets so
the page drops out of the candidate set. Bounded at
`RETIRED_SWEEP_MAX_PAGES` pages per cycle (a per-cycle IO cap, not a
semantic gate — excess candidates wait for the next tick) and logged.
Which retire paths strip act-time vs. ride this sweep is tabulated in
[redaction-policy](redaction-policy.md).

What the scheduler does **not** do today (deferred to follow-ups):

- Cron-string scheduling ("nightly at 03:00 local time") — interval
  ticker covers the standard PWA-as-permanent-daemon deployment and
  was cheaper to land. Cron parsing comes when an operator asks for
  it.
- Per-cycle backpressure — the LLM bag is reused across cycles but
  there is no global lock; back-to-back cycles can in principle
  overlap if `interval_secs` is shorter than the cycle's own runtime.
  Daily default makes this a non-issue; if/when the interval shortens
  meaningfully, add a `tokio::sync::Mutex` around `run_once` calls
  inside the spawn loop.

## Manual trigger + run history — the admin Dream console

Beyond the interval scheduler, an admin runs a dream on demand from the
**Dream** console at `GET /dashboard/dream` (admin-only): a full page with the
three trigger forms at the top and the persisted **run history** (newest first)
below. The forms post to three routes, each delegating to
[`mwe-core::dream`](../../crates/mwe-core/src/dream.rs):

| Route | Composition |
|---|---|
| `POST /dashboard/dream/light`   | `dream::run_light` — promote captures, then compile what went dirty |
| `POST /dashboard/dream/compile` | `dream::run_compile` — recompile the dirty pages only (isolate the compiler) |
| `POST /dashboard/dream/full`    | `dream::run_full` — full reorg + comment application + compile |
| `GET /dashboard/dream/status`   | JSON the topnav indicator polls while a background dream runs |
| `GET /dashboard/dream/runs/:id` | HTML fragment with one run's full log — the per-row modal injects it |

Each handler resolves the LLM bag from `MemoryHandles::backend_for` (the
dashboard's `Arc<dyn LlmBackend>` holder, vs the scheduler's owned `Box`) and
calls the **same** composition the scheduler and the CLI use, so a manual
trigger can never diverge from a scheduled one. In particular
**Full REM runs the compile pass**: applying parked comments via the
console also recompiles the affected prose in the same click. The console is the only way to trigger a dream while the server
holds the workdir lock — the `mwe-mcp rem run-*` CLI needs that same lock and
so cannot run alongside `serve`.

With JS each form runs in the background: it POSTs with `Accept: application/json`,
the server kicks the dream off on a task and acks immediately, a "dream…" pill
animates in the topnav (polling `/dream/status`), and on completion the console
page reloads to reveal the new history row. No-JS users get the synchronous
full-page report instead (the server branches on the `Accept` header).

Concurrency: each handler `try_lock`s a per-`DashboardState` gate
(`rem_gate`) so two manual dreams can never overlap (a busy gate renders a
"dream already running" page instead of racing). Sharing that gate with the
interval scheduler — so a manual run also excludes a *scheduled* cycle — is a
tracked follow-up (`rem-manual-trigger-scheduler-gate`) and is not wired
today. During interactive use the scheduler is effectively dormant (24h
interval), so the manual-only gate suffices.

### Run history journal

Every finished run is recorded in the `dream_runs` table via
[`mwe-core::dream_journal`](../../crates/mwe-core/src/dream_journal.rs) — both the
manual console runs (`trigger_source = 'manual'`) and the scheduler's nightly /
interval runs (`'scheduled'`), the latter written from `rem_scheduler`'s
`fire_once` / `fire_light`. The DB is the single home because the scheduler and
the dashboard run in different layers and share only the SQLite pool; it is also
what makes the history survive a restart (the older in-memory one-liner did
not). A row carries the `kind`, the `trigger_source`, the `ok` outcome, the
structured **`pages_failed` / `pages_degraded`** counts (the compile pass's
per-page outcomes, fed through `dream::journal_counts`), the one-line `summary`
(the same `dream::summarize_*` text the topnav pill and the scheduler log use),
the full report dump as `log_text`, and the start / finish stamps. The console
table shows the summary per row; clicking **log** opens a modal that fetches
`log_text` from `/dream/runs/:id`.

**A completed run is not plain ok when pages failed or degraded**: nonzero
counts flip `DreamRun::needs_attention`, the console badges the row **warn**
(amber) instead of ok, and the summary string carries the same counts
(`— N pages FAILED · M degraded`) so the existing rendering surfaces it
everywhere the summary travels. A run that errored outright keeps the error
badge and `(0, 0)` counts — nothing compiled.

The journal is bounded to the newest `dream_journal::MAX_HISTORY` (100) rows,
pruned after each insert — a resource cap, not a semantic gate. A *scheduled*
light tick that scanned nothing is **not** recorded (the loop runs it
constantly; journaling no-ops would bury the meaningful runs), matching the
condition that already gates its info-level log. Manual runs and every full /
errored run are always recorded.

### Per-page compile failure surfacing

A page the Cronista keeps failing must reach the operator, not only the run's
log dump. The compiler tracks **consecutive** per-page compile failures in the
`compile_failures` ledger
([`mwe-core::compile_failures`](../../crates/mwe-core/src/compile_failures.rs),
migration `0055`): one row per failing page, keyed by the workdir-relative
`source_path`, carrying `consecutive`, `last_error`, and `updated_at`. After
every page compile the compiler updates it:

- a **failed** page (per-page soft error) increments the streak;
- a **degraded** page (the
  [guard-only rewrite](narrative-compiler.md#degraded-mode--the-guard-only-rewrite))
  **also increments** — the page made progress, but the Cronista keeps failing
  there;
- only a **clean full rewrite** (leaf / list / hub / unchanged) resets the
  streak (deletes the row).

When a streak reaches `compile_failures::NOTICE_THRESHOLDS` — exactly **2**,
and again at exactly **5** — the compiler emits one **`compile_failure_streak`**
event on `wiki_events` (the same channel the `structure_applied` notices ride,
drained by consumers over `events_poll`): payload `slug`, `source_path`,
`consecutive`, `last_error`, and a `dashboard_path` to the Dream console, with
the page's wiki in the `wiki_id` column. Once per threshold per streak by
construction (the count passes each value once; a clean rewrite resets it).
The thresholds are **observability thresholds on a failure ledger, not
semantic gates** — nothing about the memory's content is decided here. Both
ledger updates and the notice are best-effort: an observability hiccup never
fails the compile.

## What is intentionally out of scope

These are not implemented today (planned — see the
roadmap):

| Not yet supported | Why |
|---|---|
| `archive_proposals` apply (filesystem move + cascade `wiki_forget`) and the dashboard approval UI | The detector is wired today; the reaper that flips a row to `applied` and moves the page into `_archive/` is not yet built. |
| `NO_MODIFY_180D` archive reason | Reserved in [`archive::reason`](../../crates/mwe-core/src/archive.rs) for a sub-job that mixes recall and modification recency — today the detector writes `NO_RECALL_HIT_365D`. |
| Briefing-processor **action-taking** (recall context → LLM verdict → apply a structural action → stamp) | The processor ships mark-passive today; semantic action on the cited target is not yet implemented. |
| Per-step rollback driver for the `bundle` kind | Lands with the `bundle` apply handler. Today's handlers (`promote`, `dedup`) lean on atomic-write idempotency. |
