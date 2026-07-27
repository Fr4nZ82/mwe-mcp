// SPDX-License-Identifier: AGPL-3.0-or-later
//! REM (Reorganization Memory) nightly cycle.
//!
//! Cron-driven job that runs without users connected. The write-jobs
//! skip smart-family wikis (the smart consumer owns those writes via
//! `wiki_admin_push`), while two smart-wiki-only read-jobs scan them
//! for observations worth surfacing in `_briefing.md`.
//!
//! ## Sub-jobs
//!
//! **[`run_cycle`] is the SSOT for the sub-job roster and their fixed
//! order — read its body**; the per-sub-job contract is documented in
//! rem-cycle.md. The shape
//! of the cycle: the two proposal sweeps settle overdue
//! `structure_proposals` first; the consolidation and hygiene sweeps
//! (dedup, promote, merge, completion, contradiction, refile,
//! provenance, dates) reorganise the fact set act-first; the archive
//! detector and the smart-wiki read-jobs emit proposals/briefing items;
//! and `hub_writer` runs last so its prompt sees a stable state.
//!
//! ## Cycle invariants
//!
//! - Sub-jobs run in the fixed order wired in [`run_cycle`]; ordering
//!   is load-bearing only at the edges (the proposal sweeps settle
//!   pending state before the write-jobs touch it, provenance hygiene
//!   runs right before the date normalizer so later sub-jobs see
//!   pointer-clean text, and `hub_writer` summarises last).
//! - Every state-mutating sub-step is journaled in `rem_ops_log` via
//!   [`crate::wal::begin_rem_op`] → `complete_rem_op` / `fail_rem_op`.
//!   The floor's sub-step inverses are idempotent (`atomic_write`
//!   handles partial `index.md` writes; `mark_superseded` and
//!   `mark_forgotten` are no-ops on already-superseded / already-
//!   tombstoned rows; `insert_event` is gated by an idempotency probe).
//!   A crashed cycle is safe to retry on the next REM tick — there is
//!   no per-step rollback driver in this milestone, which is
//!   documented as a follow-up alongside the proposal-side WAL
//!   apply driver.
//! - Soft failures inside a sub-job (one wiki's template missing, one
//!   fact body that fails YAML parse, the LLM hanging up on one pair)
//!   are collected in the sub-job's `errors` list and the cycle
//!   continues. Only infrastructure failures (DB / filesystem) bubble
//!   up as [`RemError`].
//!
//! ## What is intentionally out of scope
//!
//! | Out of scope here | Why |
//! |---|---|
//! | The compile pass (planner + Cronista + reviewer) | Composed in [`crate::dream`], which runs it after this cycle on the full cadence — not a `run_cycle` sub-job. |
//! | Per-step rollback driver for `proposal_ops_log` | Deferred alongside the proposal apply engine (rollback shape mirrors REM's). |

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::SqlitePool;
use thiserror::Error;

pub mod briefing_processor;

use crate::archive::{self, ArchiveError};
use crate::briefing::{self, BriefingError, BriefingSourceKind, NotifyRequest};
use crate::dedup::{self, DedupMergeHints};
use crate::embedder::Embedder;
use crate::events::EventsError;
use crate::events::{self, EventKind};
use crate::fact_index::{self, FactIndexRow};
use crate::llm::{CompletionRequest, LlmBackend};
use crate::planner::{CompilationPlan, PagePlan, PageType};
use crate::promote::{self, PageMergeParams, ParagraphToFileHints};
use crate::prompts;
use crate::proposals::{self, ProposalsError};
use crate::recall;
use crate::recall_gate;
use crate::recall_log;
use crate::rem_verdicts;
use crate::reviewer;
use crate::sections;
use crate::types::{FactId, WikiId};
use crate::wal;
use crate::wiki::{self, WikiTree};

// ---------- Policy ----------

/// Operator-tunable knobs for one [`run_cycle`] invocation.
#[derive(Debug, Clone)]
pub struct RemPolicy {
    /// Cycle id used in `rem_ops_log` and audit. `None` ⇒ generated
    /// from the current timestamp.
    pub cycle_id: Option<String>,
    /// Wall-clock anchor for lifecycle evaluation (`now` in the rule
    /// expressions). `None` ⇒ [`Utc::now`].
    pub now: Option<DateTime<Utc>>,
    /// Maximum number of wikis whose `index.md` is regenerated per
    /// cycle. Hub Writer is the most expensive sub-job per call (LLM
    /// + `atomic_write`), so the cap shields the operator's budget.
    pub hub_writer_cap: usize,
    /// Maximum number of dedup supersedes per cycle. Mirrors the
    /// `cap_promotions_per_night` figure in
    /// engine DB and migrations — REM never
    /// silently rewrites the whole corpus in one tick.
    pub revisor_cap: usize,
    /// Lower bound for the jaccard-6gram pre-pass: pairs below this
    /// score are dismissed without asking the LLM (probably unrelated).
    pub revisor_jaccard_min: f32,
    /// Upper bound for the pre-pass: pairs at or above this score are
    /// write-time dedup territory ([`recall::DEFAULT_DEDUP_THRESHOLD`] —
    /// the direct capture scan, and the light dream re-running it at
    /// promotion), so the revisor focuses on the **interesting** band in
    /// between.
    pub revisor_jaccard_max: f32,
    /// Floor for the **embedding cosine** nomination channel: a pair
    /// whose vectors sit at or above this similarity goes to the LLM
    /// even when its surface jaccard falls below `revisor_jaccard_min` —
    /// the same claim with the subject spelled out vs elided ("È nato il
    /// 23 maggio 1984" / "Franz è nato il 23 maggio 1984") shares
    /// meaning, not n-grams. A nomination channel only — the LLM still
    /// makes the verdict.
    pub revisor_cosine_min: f32,
    /// Cap on LLM confirm calls per cycle across both nomination
    /// channels — a resource guard (cost), not a semantic gate. When it
    /// trips, the remaining pairs wait for the next cycle and the
    /// truncation is logged (never silent).
    pub revisor_examined_cap: usize,
    /// Maximum number of structural changes the auto-promote
    /// sub-job applies per cycle. The spec
    /// ([memory model](../../../docs/concepts/memory-model.md))
    /// pins it at 5/night by default — REM never carpet-bombs the
    /// operator's inbox.
    pub auto_promote_cap: usize,
    /// Minimum **page mass** for the paragraph-to-file deterministic
    /// filter: the number of active facts a fact must share its page
    /// (`source_path`) with before it becomes a promotion candidate. The
    /// promotion trigger is **mass/ramification**, not the word count of
    /// any single fact (forma fisica):
    /// a topic that has accumulated many atomic facts on one page is the
    /// thing worth splitting off, given that facts are atomic. This
    /// is a cheap **resource** pre-filter (skip asking the LLM about
    /// thin pages), not a semantic gate — the LLM still makes the
    /// promote verdict.
    pub auto_promote_min_page_facts: usize,
    /// Minimum **group size**, in pages, for a new sub-wiki to be born
    /// out of the *page-group → wiki* regrouping pass: the LLM must find
    /// at least this many pages of one wiki that are the same subject
    /// area before any of them moves (`pages_to_subwiki`).
    ///
    /// This is the page→folder rung of the forma fisica scale, above
    /// [`Self::auto_promote_min_page_facts`] (line→page), and the two no
    /// longer compete: a page that has accumulated mass is *split into
    /// pages*, and only a **set** of pages that already exist becomes a
    /// wiki. A wiki is therefore never born with a single page, and the
    /// trigger is evidence on disk rather than a bet on future
    /// ramification.
    ///
    /// The floor governs **birth** only. Moving pages into a sub-wiki
    /// that already exists (`pages_move_wiki`) has no floor — the home
    /// is already there, so a single stray page belongs inside just as
    /// much as nine do.
    pub auto_promote_group_min_pages: usize,
    /// Maximum number of page-merge candidate pairs the merge sub-job
    /// sends to the LLM confirmer per cycle (the cure front of semantic
    /// page consolidation — see the
    /// page-merge sub-job).
    /// A **resource** cap on confirmation calls, not a semantic gate —
    /// structural signals only nominate, the LLM decides. `0` disables
    /// the sub-job.
    pub page_merge_cap: usize,
    /// Maximum number of evidence facts the completion sweep sends to
    /// the LLM per cycle (the REM safety net behind the ingest closure
    /// verb — see the REM cycle).
    /// A **resource** cap: embedding similarity only nominates open
    /// candidates per evidence fact, the LLM decides what completed.
    /// `0` disables the sub-job.
    pub completion_sweep_cap: usize,
    /// Maximum number of candidate facts the cross-wiki refile sweep
    /// sends to the LLM per cycle (the LLM-decided refile of a single
    /// misfiled fact into a different existing wiki — see the
    /// REM cycle). A
    /// **resource** cap: a deterministic cosine pre-filter only nominates
    /// facts that embed materially closer to a foreign wiki than to home,
    /// the revisor LLM decides whether (and where) each really belongs.
    /// `0` disables the sub-job. Smart wikis are skipped as both source
    /// and destination (the ownership boundary is the consumer's).
    pub refile_sweep_cap: usize,
    /// How far back the two closure sweeps look for fresh seeds — new
    /// evidence facts (completion sweep, by `created_at`) and freshly
    /// contradicted rows (contradiction sweep, by `superseded_at` /
    /// `updated_at`). Bounded so each seed is judged in the cycle(s)
    /// right after it lands instead of re-judging the corpus.
    pub closure_sweep_window: chrono::Duration,
    /// Maximum number of freshly contradicted seeds the contradiction
    /// sweep sends to the LLM per cycle (the cluster half of the
    /// temporal-validity model — the satellites of a cancelled event).
    /// A **resource** cap: embedding similarity only nominates, the LLM
    /// decides which candidates fall with the seed. `0` disables.
    pub contradiction_sweep_cap: usize,
    /// Maximum number of lexically flagged facts the date normalizer
    /// sends to the LLM per cycle (oldest first, so a pre-existing
    /// backlog drains deterministically). The deictic lexicon is a
    /// **resource** pre-filter (skip the LLM on unflagged facts), not a
    /// semantic gate — the LLM decides whether each flagged fact really
    /// needs the rewrite. `0` disables the sub-job.
    pub date_normalize_cap: usize,
    /// Maximum number of facts the provenance-hygiene sweep repairs per
    /// cycle (oldest first, so a pre-existing backlog drains
    /// deterministically). The sweep is fully deterministic — mechanical
    /// repair of the known trailing-`([[…]])` source-pointer defect, no
    /// LLM — so the cap bounds embedder spend only, a **resource** cap
    /// like the sibling sweeps. `0` disables the sub-job.
    pub provenance_hygiene_cap: usize,
    /// Maximum number of `archive_proposals` emitted per cycle.
    /// Default 10/week — archive sweep runs weekly, not nightly, so
    /// the cap is per weekly invocation.
    pub archive_cap: usize,
    /// Inactivity window for the archive detector: a fact qualifies as
    /// candidate when `last_recall_at` (or `created_at` when null) is
    /// older than this duration. Default 365 days.
    pub archive_inactivity: chrono::Duration,
    /// Max notifications emitted by each new smart-wiki sub-job
    /// (Briefing dispatcher + Backlink reciprocity detector) per wiki
    /// per cycle. Per the [memory model](../../../docs/concepts/memory-model.md)
    /// the per-wiki cap is 10 — the global 50/h cap in
    /// [`crate::briefing`] backstops at the inbox level. Default 10.
    pub briefing_notify_cap: usize,
    /// Briefing dispatcher: a fact carrying `status: draft` (top-level
    /// YAML key) whose age exceeds this window triggers a stale-draft
    /// notify. Default 14 days.
    pub briefing_stale_draft_age: chrono::Duration,
    /// Briefing dispatcher: a fact whose `recall_count_30d` is at or
    /// above this threshold triggers a recall-hot notify suggesting the
    /// smart consumer promote it. Default 20.
    pub briefing_recall_hot_threshold: i64,
    /// Briefing dispatcher + Backlink reciprocity detector: same
    /// `(wiki_id, source_ref)` is not re-emitted if a row already exists
    /// in `wiki_briefing_items` within this window. Default 7 days.
    pub briefing_dedup_window: chrono::Duration,
    /// Lease expirer: an active lease whose `expires_at` lies
    /// further than this grace period in the past is treated as
    /// crashed-without-release and marked `released_at = now`. Default
    /// 1 hour — slow clients about to re-acquire still win the race.
    pub lease_expirer_grace: chrono::Duration,
    /// Lease expirer: released rows older than this retention
    /// window are deleted. The dashboard `/wikis/<id>/op-log` reads
    /// the table for past leases, so the window doubles as the UI
    /// visibility budget. Default 7 days.
    pub lease_expirer_retention: chrono::Duration,
    /// Briefing-processor (sub-job 10): master switch. `false`
    /// disables the sub-job for this cycle without forcing the
    /// operator to clear other policy fields. Default `true`.
    pub briefing_processor_enabled: bool,
    /// Briefing-processor (sub-job 10): a row whose `ts` is
    /// within this grace period of `now` is left alone — the operator
    /// might still be editing the comment in the dashboard. The
    /// synchronous Submit endpoint on the dashboard bypasses the
    /// grace period, the cycle does not. Default 15 minutes.
    pub briefing_processor_grace: chrono::Duration,
    /// Husk-page GC: page FILES removed per full cycle. A husk is a
    /// plan-absent, non-reserved page whose fact rows are all tombstoned
    /// or superseded past the receipts' revert window — the files the
    /// compiler's orphan sweep must keep while a superseded row's marker
    /// may still serve a revert. Default 4; `0` disables.
    pub husk_gc_cap: usize,
    /// Recall-repair sub-job: pending misses processed per cycle. Each
    /// candidate repair costs one proposal completion plus a gold-set
    /// gate replay (two eval passes on a scratch snapshot), so the cap
    /// is deliberately small. A **resource** cap; `0` disables. Default 3.
    pub recall_repair_cap: usize,
    /// Recall-repair sub-job: when the same fact has missed at least
    /// this many times and no local repair committed, an operator
    /// `recall_tuning_proposed` notice is emitted (the review-queue
    /// entry — rule/prompt-level levers are never auto-applied).
    /// Default 3.
    pub recall_tuning_recurrence: i64,
    /// How long a **negative** confirmer verdict stays on record in
    /// `rem_verdicts` before the question is asked again
    /// ([`crate::rem_verdicts`]). The memo already self-invalidates on
    /// content, prompt, and model changes, so this TTL is not a
    /// correctness lever — it bounds the table and buys every settled
    /// question an eventual second opinion. Default 90 days.
    pub verdict_memo_ttl: chrono::Duration,
    /// Recall knobs the gold-set gate replays with
    /// ([`crate::recall_gate`]) — flat top-K + the navigator funnel
    /// budgets. Defaults mirror production's [`IngestPolicy`] defaults.
    pub gate_recall: crate::ingest::IngestPolicy,
}

impl Default for RemPolicy {
    fn default() -> Self {
        Self {
            cycle_id: None,
            now: None,
            hub_writer_cap: 10,
            revisor_cap: 30,
            revisor_jaccard_min: 0.45,
            revisor_jaccard_max: recall::DEFAULT_DEDUP_THRESHOLD,
            revisor_cosine_min: 0.80,
            revisor_examined_cap: 120,
            auto_promote_cap: 5,
            auto_promote_min_page_facts: 8,
            auto_promote_group_min_pages: 9,
            page_merge_cap: 3,
            completion_sweep_cap: 8,
            refile_sweep_cap: 5,
            closure_sweep_window: chrono::Duration::hours(48),
            contradiction_sweep_cap: 8,
            date_normalize_cap: 16,
            provenance_hygiene_cap: 32,
            archive_cap: 10,
            archive_inactivity: chrono::Duration::days(365),
            briefing_notify_cap: 10,
            briefing_stale_draft_age: chrono::Duration::days(14),
            briefing_recall_hot_threshold: 20,
            briefing_dedup_window: chrono::Duration::days(7),
            lease_expirer_grace: chrono::Duration::hours(1),
            lease_expirer_retention: chrono::Duration::days(7),
            briefing_processor_enabled: true,
            briefing_processor_grace: chrono::Duration::minutes(15),
            husk_gc_cap: 4,
            recall_repair_cap: 3,
            recall_tuning_recurrence: 3,
            verdict_memo_ttl: chrono::Duration::days(90),
            gate_recall: crate::ingest::IngestPolicy::default(),
        }
    }
}

// ---------- Reports ----------

/// Aggregated outcome of one [`run_cycle`].
#[derive(Debug, Clone)]
pub struct RemCycleReport {
    /// Identifier used as `cycle_id` in `rem_ops_log` rows.
    pub cycle_id: String,
    /// Wall-clock anchor used (matches `RemPolicy::now` when supplied).
    pub started_at: DateTime<Utc>,
    /// Wall-clock when the cycle returned.
    pub ended_at: DateTime<Utc>,
    /// Auto-apply sweep report — applies pending proposals past
    /// `timeout_at` before the new emitters run.
    pub auto_apply: AutoApplyReport,
    /// Auto-finalize sweep report — flips
    /// `applied_pending_confirm` proposals past `confirm_deadline` to
    /// `applied` (silent, locked, no `revert_token`, no event).
    pub auto_finalize: AutoFinalizeReport,
    /// Revisor sub-job report.
    pub revisor: RevisorReport,
    /// Auto-promote sub-job report.
    pub auto_promote: AutoPromoteReport,
    /// Page-merge sub-job report — LLM-confirmed consolidation of
    /// near-synonym concept pages (act-first, receipt + revert window).
    pub page_merge: PageMergeReport,
    /// Completion sweep report — the REM safety net of the closure verb
    /// (closes open items whose completion ingest could not see).
    pub completion_sweep: CompletionSweepReport,
    /// Cross-wiki refile sweep report — moves single facts the revisor
    /// LLM deems misfiled into a different existing wiki (act-first +
    /// revert, smart-skip).
    pub refile_sweep: RefileSweepReport,
    /// Contradiction sweep report — closes the satellites of a freshly
    /// contradicted fact that ingest could not see.
    pub contradiction_sweep: ContradictionSweepReport,
    /// Recall-repair sub-job report — self-correcting REM's repair
    /// stage: pending recall misses judged, re-files committed only
    /// through the gold-set gate, recurrence notices queued.
    pub recall_repair: RecallRepairReport,
    /// Provenance-hygiene sweep report — trailing source-pointer
    /// wikilinks moved off canonical fact text into `authored_refs`.
    pub provenance_hygiene: ProvenanceHygieneReport,
    /// Date normalizer report — relative→absolute date rewrites on
    /// canonical fact text.
    pub date_normalizer: DateNormalizeReport,
    /// Archive detector report.
    pub archive_detector: ArchiveDetectorReport,
    /// Briefing dispatcher sub-job report — scans smart-wiki
    /// wikis for stale drafts + recall-hot facts and posts items to the
    /// owner's `_briefing.md` via [`crate::briefing::notify_as_rem`].
    pub briefing_dispatcher: BriefingDispatcherReport,
    /// Backlink reciprocity detector sub-job report — flags
    /// `[[wiki:<smart-wiki>#...]]` links from standard wikis that
    /// lack a reciprocal back-link inside the smart wiki.
    pub backlink_reciprocity: BacklinkReciprocityReport,
    /// Lease expirer sub-job report — prunes stale rows
    /// from `wiki_admin_leases`. Two passes: active-but-expired beyond
    /// grace get `released_at` stamped (treated as crashed without
    /// release), released rows beyond retention get deleted.
    pub lease_expirer: crate::wiki_admin_leases::ExpirerReport,
    /// Briefing-processor sub-job report (sub-job 10) —
    /// drains pending `wiki_briefing_items` rows on non-smart
    /// wikis past the grace period.
    pub briefing_processor: BriefingProcessorReport,
    /// Husk-page GC sub-job report — plan-absent page files whose
    /// rows are all past any revert removed from disk.
    pub husk_gc: HuskGcReport,
    /// Hub Writer sub-job report (runs last).
    pub hub_writer: HubWriterReport,
    /// Negative-verdict memos dropped by the TTL sweep at cycle start
    /// ([`crate::rem_verdicts`]).
    pub verdict_memo_purged: u64,
    /// Live `rem_verdicts` rows once the cycle finished. Read together
    /// with each sub-job's `examined` count — which now means *asked the
    /// model*, memo hits never reach it — this is how an operator sees
    /// the memo working.
    pub verdict_memo_rows: i64,
}

/// Sub-report for the jaccard semantic revisor / Conciliatore emitter.
#[derive(Debug, Clone, Default)]
pub struct RevisorReport {
    /// Pairs the jaccard pre-pass forwarded to the LLM.
    pub pairs_examined: usize,
    /// Pairs the LLM confirmed as semantically equivalent.
    pub pairs_confirmed: usize,
    /// `proposal_id`s of the born-applied `dedup_merge` receipts: each
    /// confirmed pair merged **act-first** in-cycle, revertible from the
    /// dashboard within the standard window
    /// ([memory model](../../../docs/concepts/memory-model.md)).
    pub applied: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the page-merge sub-job.
///
/// The cure front of semantic page consolidation: structural signals
/// nominate near-synonym concept-page pairs, a dedicated LLM call confirms
/// "same concept?" and picks the survivor, and the merge executes act-first
/// on the move machinery (every husk fact onto the survivor, husk deleted,
/// plan re-homed, born-applied receipt + `structure_applied` notice).
#[derive(Debug, Clone, Default)]
pub struct PageMergeReport {
    /// Candidate pairs that reached the LLM confirmation call.
    pub candidates_examined: usize,
    /// Pairs the LLM confirmed as the same concept.
    pub candidates_confirmed: usize,
    /// Born-applied receipt ids of executed merges.
    pub applied: Vec<String>,
    /// Pairs skipped because a page-merge receipt already covers them —
    /// including a reverted one, which is the operator's standing veto.
    pub skipped_judged: usize,
    /// Pairs skipped because the husk's `fact_index` rows were not all
    /// settled on its compiled page (pending renders) — retried on a
    /// later cycle once the compiler has caught up.
    pub skipped_unsettled: usize,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the completion sweep.
///
/// The REM safety net of the closure verb: fresh evidence facts are
/// paired with similar open items, the LLM confirms what completed, and
/// the confirmed closures land act-first with the same `validity_close`
/// receipt + notice the ingest half uses.
#[derive(Debug, Clone, Default)]
pub struct CompletionSweepReport {
    /// Evidence facts (created inside the window) that reached the LLM.
    pub evidence_examined: usize,
    /// Open candidates judged across all evidence calls.
    pub candidates_judged: usize,
    /// `fact_id`s whose validity the sweep closed as completed.
    pub closed: Vec<String>,
    /// Born-applied `validity_close` receipt ids (one per evidence fact
    /// that closed something).
    pub receipts: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the cross-wiki refile sweep.
///
/// The LLM-decided refile of a single misfiled fact into a different
/// existing wiki: a deterministic cosine pre-filter nominates facts that
/// embed materially closer to a foreign wiki than to home, the revisor
/// LLM decides whether (and where) each really belongs, and a confirmed
/// move lands act-first via [`crate::promote::apply_fact_refile_direct`]
/// (born-applied receipt + `structure_applied` notice — the dashboard is
/// the undo surface). Smart wikis are skipped as both source and dest.
#[derive(Debug, Clone, Default)]
pub struct RefileSweepReport {
    /// Reviewer-fed candidates seeded from the parked plan (the
    /// `cross_subject_bloat` → refile bridge), before the cap.
    pub bridge_candidates: usize,
    /// Candidate facts the cosine pre-filter nominated.
    pub candidates_examined: usize,
    /// Candidate facts that reached the LLM verdict (== examined unless a
    /// nominee vanished between gather and judge).
    pub candidates_judged: usize,
    /// `fact_id`s the sweep moved to a different wiki.
    pub refiled: Vec<String>,
    /// Born-applied `wiki_promote` (`fact_refile`) receipt ids.
    pub receipts: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the contradiction sweep.
///
/// The cluster half of the temporal-validity model: a freshly
/// contradicted fact seeds an LLM judgment over its similar open
/// neighbours — the satellites of a cancelled event — and the confirmed
/// ones close as `contradicted` with the same act-first paper trail.
#[derive(Debug, Clone, Default)]
pub struct ContradictionSweepReport {
    /// Freshly contradicted seeds that reached the LLM.
    pub seeds_examined: usize,
    /// Open candidates judged across all seed calls.
    pub candidates_judged: usize,
    /// `fact_id`s the sweep closed as contradicted.
    pub closed: Vec<String>,
    /// Born-applied `validity_close` receipt ids (one per seed that
    /// closed something).
    pub receipts: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the provenance-hygiene sweep.
///
/// Mechanical repair of the known document-ingest defect: a claim whose
/// canonical text ends with a trailing source-pointer parenthetical
/// ` ([[wiki/page]])`. The sweep strips the suffix, moves the pointer
/// into `authored_refs` (dedup'd), and re-embeds the cleaned text in
/// place (offsets kept; the render-content fingerprint recompiles the
/// touched pages). Deterministic and convergent: once the corpus is
/// clean the detector flags nothing and the sweep no-ops forever.
#[derive(Debug, Clone, Default)]
pub struct ProvenanceHygieneReport {
    /// Active facts whose text matched the trailing-pointer defect.
    pub flagged: usize,
    /// Flagged facts processed this cycle (cap applied).
    pub examined: usize,
    /// `fact_id`s repaired: suffix stripped, pointer moved into
    /// `authored_refs`, text re-embedded.
    pub moved: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the husk-page GC sweep.
///
/// The aggressive tail of page cleanup: the compiler's orphan sweep
/// keeps a plan-absent file while ANY non-tombstoned row points at it
/// (a superseded row's on-disk marker may still serve a revert); this
/// sweep removes the file once every remaining row is tombstoned or
/// superseded past the receipts' revert window — the husks the
/// delete/supersede machinery leaves behind. Inbound links degrade to
/// literal text (the link grammar's dead-rail posture) and the compile
/// feed's dead-ref vetting keeps prose clean.
#[derive(Debug, Clone, Default)]
pub struct HuskGcReport {
    /// Plan-absent, non-reserved page files checked against the DB.
    pub pages_examined: usize,
    /// `wiki_id/page` husk files removed this cycle (cap applied).
    pub removed: Vec<String>,
    /// Removable husks left for a later cycle by the per-cycle cap.
    pub deferred: usize,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the date normalizer.
///
/// Relative→absolute date rewrites on canonical fact text: the lexical
/// pre-filter flags candidates, one batched LLM call decides the
/// rewrites, each applied text is re-embedded in place (offsets kept;
/// the render-content fingerprint recompiles the touched pages).
#[derive(Debug, Clone, Default)]
pub struct DateNormalizeReport {
    /// Active facts the deictic lexicon flagged.
    pub flagged: usize,
    /// Flagged facts sent to the LLM this cycle (cap applied).
    pub examined: usize,
    /// `fact_id`s whose text was rewritten + re-embedded.
    pub rewritten: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the `hub_writer`.
#[derive(Debug, Clone, Default)]
pub struct HubWriterReport {
    /// Wiki ids whose `index.md` was rewritten.
    pub regenerated: Vec<String>,
    /// Wiki ids that did not qualify (no children / no active facts /
    /// past the cap).
    pub skipped: Vec<String>,
    /// Soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the Briefing dispatcher.
///
/// Walks every wiki of the smart family looking for stale drafts and
/// recall-hot facts; per finding posts a single item to the owner's
/// `_briefing.md` via [`crate::briefing::notify_as_rem`]. Per-wiki cap
/// = [`RemPolicy::briefing_notify_cap`], idempotency window =
/// [`RemPolicy::briefing_dedup_window`].
#[derive(Debug, Clone, Default)]
pub struct BriefingDispatcherReport {
    /// Smart wikis whose facts were scanned.
    pub wikis_examined: usize,
    /// Notifications appended (paired with the brief topic for audit).
    pub notifications_emitted: Vec<(String, String)>,
    /// Candidate findings that were skipped because an identical
    /// `(wiki_id, source_ref)` row already existed within the dedup
    /// window — counted separately so the operator can tell apart
    /// "no work" from "all work absorbed by idempotency".
    pub deduplicated: usize,
    /// Per-finding soft errors (invalid body parse, briefing rate-limit
    /// from the inbox, etc.) collected without aborting the cycle.
    pub errors: Vec<String>,
}

/// Sub-report for the Briefing-processor non-smart (sub-job 10).
///
/// Drains `wiki_briefing_items` rows whose `wiki_id` is a
/// **non-smart** wiki (smart consumers maintain their own
/// smart wikis; REM maintains the standard families). Mark-passive
/// policy: stamp `processed_at = NOW()` after a pro-forma read of the
/// cited context. The same core function
/// ([`briefing_processor::process_briefing_item`]) is also invoked
/// synchronously from the dashboard "Submit" button on a per-row
/// basis; that path bypasses the grace period because the operator
/// has explicitly asked for immediate drain.
#[derive(Debug, Clone, Default)]
pub struct BriefingProcessorReport {
    /// Number of candidate rows the SQL scan returned (pending +
    /// non-smart + past grace).
    pub items_examined: usize,
    /// Rows actually drained — wiki resolved, `processed_at` stamped.
    pub items_processed: usize,
    /// Rows skipped because the row was already `processed_at IS NOT
    /// NULL` between the scan and the per-row processor call. Real-
    /// world cause: a synchronous Submit drained the row between the
    /// candidate list and the per-row call.
    pub items_already_processed: usize,
    /// Rows whose `wiki_id` did not resolve to a known wiki on disk
    /// (deleted wiki with rows still in the inbox). Left untouched —
    /// surfaced here for the operator to follow up.
    pub items_wiki_missing: usize,
    /// Standard-wiki comments applied as fact ops: facts corrected in place.
    pub facts_corrected: usize,
    /// Standard-wiki comments applied as fact ops: facts added.
    pub facts_added: usize,
    /// Standard-wiki comment `add` ops skipped by the write-time dedup
    /// (near-duplicate of an existing same-owner fact — nothing inserted).
    pub facts_deduped: usize,
    /// Standard-wiki comments applied as fact ops: facts removed.
    pub facts_removed: usize,
    /// Standard-wiki comments applied as fact ops: facts moved to another
    /// page or wiki (born-applied + revertible — the `_direct` wrappers
    /// mint a receipt).
    pub facts_moved: usize,
    /// Per-row soft errors (DB / filesystem / invalid `wiki_id` row).
    /// Hard failures bubble as [`RemError`]; everything else is
    /// collected here and the cycle keeps going.
    pub errors: Vec<String>,
}

/// Sub-report for the Backlink reciprocity detector.
///
/// Walks every wiki *outside* the smart family, scans active fact
/// bodies for `[[wiki:<id>...]]` references whose target is a smart wiki
/// of the same owner, and emits a notify on the smart wiki when the
/// reciprocal link is missing.
#[derive(Debug, Clone, Default)]
pub struct BacklinkReciprocityReport {
    /// Smart wiki ids in the universe of targets.
    pub smart_wikis_known: usize,
    /// Standard-wiki source facts whose body was scanned.
    pub source_facts_scanned: usize,
    /// `[[wiki:...]]` wikilinks pointing at a smart-wiki target.
    pub incoming_links: usize,
    /// Notifications appended (paired with the source wiki id for audit).
    pub notifications_emitted: Vec<(String, String)>,
    /// Candidate findings absorbed by `(wiki_id, source_ref)`
    /// idempotency.
    pub deduplicated: usize,
    /// Per-finding soft errors.
    pub errors: Vec<String>,
}

/// Sub-report for the auto-finalize sweep.
///
/// Walks every `applied_pending_confirm` proposal past
/// `confirm_deadline` and calls
/// [`crate::proposals::auto_finalize_unconfirmed_proposals`] which
/// performs a single-statement flip to `applied`. **No kind inverse
/// handler is invoked, no `revert_token` is minted, no event is
/// emitted** — silence within the `confirm_window` is treated as
/// consent (the user was already notified at auto-apply time via the
/// `auto_applied` event).
#[derive(Debug, Clone, Default)]
pub struct AutoFinalizeReport {
    /// Rows the sweep loaded from `structure_proposals`.
    pub candidates_examined: usize,
    /// `proposal_id`s the sweep flipped to `applied`.
    pub finalized: Vec<String>,
}

/// Sub-report for the auto-apply sweep.
///
/// Walks every pending proposal past `timeout_at` and calls
/// [`crate::proposals::auto_apply_overdue_proposals`] which dispatches
/// to [`crate::proposals::auto_apply_proposal`] with the `recommended`
/// answers derived from the questionnaire. The 5-state lifecycle
/// lands rows on `applied_pending_confirm` — `applied` here is
/// a legacy field name preserved for the REM cycle JSON wire shape; it
/// counts auto-applied rows in either state machine.
#[derive(Debug, Clone, Default)]
pub struct AutoApplyReport {
    /// Rows the sweep loaded from `structure_proposals`.
    pub candidates_examined: usize,
    /// `(proposal_id, kind)` of the rows the sweep moved out of
    /// `pending`. In the 5-state model the rows now sit in
    /// `applied_pending_confirm` (the auto-apply path), waiting for
    /// the user to confirm or revert within `confirm_deadline`.
    pub applied: Vec<(String, String)>,
    /// `(proposal_id, error_message)` for proposals the chassis or the
    /// handler refused. Soft errors only — the sweep keeps going.
    pub errors: Vec<(String, String)>,
}

/// Sub-report for the archive detector.
#[derive(Debug, Clone, Default)]
pub struct ArchiveDetectorReport {
    /// `source_path` entries the scanner examined.
    pub paths_examined: usize,
    /// `proposal_id`s of the archive proposals emitted.
    pub proposals_emitted: Vec<String>,
    /// Soft errors collected without aborting the cycle.
    pub errors: Vec<String>,
}

/// Sub-report for the auto-promotion emitter.
#[derive(Debug, Clone, Default)]
pub struct AutoPromoteReport {
    /// Pages that survived the mass pre-filter and were shown whole to
    /// the LLM (or skipped silently when no LLM is wired).
    pub candidates_examined: usize,
    /// Pages the LLM split (one sub-topic moved to its own page).
    pub candidates_promoted: usize,
    /// Wikis whose page inventory was shown whole to the LLM by the
    /// *page-group → wiki* regrouping pass (one call per wiki, not one
    /// per page).
    pub grouping_wikis_examined: usize,
    /// Groups the LLM cut that survived the Rust-side floors and
    /// applied — a new sub-wiki born, or pages filed into one that
    /// already existed.
    pub grouping_groups_applied: usize,
    /// Receipt ids of the structural changes **applied directly** this
    /// cycle (born-applied `wiki_promote` rows: `paragraph_to_file`,
    /// `pages_to_subwiki`, and `pages_move_wiki` share the
    /// `auto_promote_cap`). Each carries an open revert window and was
    /// announced with a `structure_applied` notice.
    pub applied: Vec<String>,
    /// Reason the sub-job was a no-op for the whole cycle. `None`
    /// when the sub-job ran. `Some("no rem_promotions LLM wired")`
    /// when the operator disabled it by leaving the slot unconfigured.
    pub disabled_reason: Option<String>,
    /// Soft errors collected per candidate without aborting the cycle.
    pub errors: Vec<String>,
}

/// LLM bag passed to [`run_cycle`]. Bundles the per-sub-job model
/// handles so adding a new sub-job (auto-promote, archive, cronista…)
/// does not grow the public signature of `run_cycle` linearly.
pub struct RemLlms<'a> {
    /// `hub_writer` slot — regenerates `index.md`.
    pub hub_writer: &'a dyn LlmBackend,
    /// `rem_dedup_semantic` slot — confirms suspicious dedup pairs.
    pub revisor: &'a dyn LlmBackend,
    /// `rem_promotions` slot — decides paragraph/file/wiki promotion.
    /// `None` disables the auto-promotion sub-job (the operator simply
    /// doesn't configure the slot).
    pub auto_promote: Option<&'a dyn LlmBackend>,
    /// `ingest` slot — the cheap (Flash-tier) backend the **light** dream
    /// runs every compile stage on (tier-per-cadence: the strong
    /// model works only at REM, the light pass uses this slot via
    /// [`crate::dream`]). `None` ⇒ the light dream degrades to whatever
    /// strong slots are configured. `run_cycle` itself does not read this
    /// slot — only the [`crate::dream`] compositions do.
    pub apply: Option<&'a dyn LlmBackend>,
    /// `ingest` slot, reused to interpret parked dashboard comments on
    /// **standard** pages into fact ops (correct / remove / add / move) — the
    /// same class of judgment as ingesting a message. `None` keeps the briefing
    /// processor on the mark-passive policy for standard wikis too (comments
    /// drain without semantic action).
    pub comment_applier: Option<&'a dyn LlmBackend>,
    /// `cronista` slot (strong model) — drives the narrative
    /// compiler ([`crate::compiler`]) when a dream recompiles dirty pages
    /// via [`crate::dream`]. `None` ⇒ the compile step is skipped (facts
    /// stay buffered/promoted but unwritten). `run_cycle` itself does not
    /// read this slot — only the [`crate::dream`] compositions do.
    pub cronista: Option<&'a dyn LlmBackend>,
    /// `navigator` slot — the recall navigator the recall-repair
    /// sub-job's gold-set gate replays with ([`crate::recall_gate`]).
    /// `None` ⇒ the gate replays flat-only, which cannot prove a
    /// navigation-reachability repair — refile candidates then never
    /// commit (conservative, not an error).
    pub navigator: Option<&'a dyn LlmBackend>,
}

// ---------- Error ----------

/// Errors raised by the REM cycle. Only **infrastructure-level**
/// failures surface here; per-sub-job soft errors are collected in the
/// individual reports.
#[derive(Debug, Error)]
pub enum RemError {
    /// Underlying SQL failure (sqlx surface).
    #[error("rem db: {0}")]
    Db(#[from] sqlx::Error),
    /// WAL journaling failure.
    #[error("rem wal: {0}")]
    Wal(#[from] wal::WalError),
    /// Filesystem traversal failure.
    #[error("rem wiki tree: {0}")]
    Wiki(#[from] wiki::WikiError),
    /// Fact-index layer failure.
    #[error("rem fact_index: {0}")]
    FactIndex(#[from] fact_index::FactIndexError),
    /// Smart-wiki section-index failure (the read-jobs that scan a smart
    /// wiki's content).
    #[error("rem wiki_sections: {0}")]
    Sections(#[from] sections::SectionError),
    /// Events layer failure.
    #[error("rem events: {0}")]
    Events(#[from] EventsError),
    /// Standard-wiki comment-application failure (action-taking path). Only
    /// infrastructure-level errors (DB / tree) bubble here; per-page
    /// interpreter failures stay in the sub-job report.
    #[error("rem comment_apply: {0}")]
    CommentApply(#[from] crate::comment_apply::CommentApplyError),
    /// Structure-proposal emission failure (Conciliatore path).
    #[error("rem proposals: {0}")]
    Proposals(#[from] ProposalsError),
    /// Structure-proposal apply / auto-apply failure surfaced by the
    /// auto-apply sweep wiring (sql / dispatch / chassis-level errors only;
    /// per-row handler failures stay in the report).
    #[error("rem proposals apply: {0}")]
    ProposalsApply(#[from] proposals::ApplyError),
    // Note: the `ProposalsRevert` variant was removed when the
    // auto-finalize sweep stopped calling the kind inverse
    // handler, so it cannot surface `RevertError`. The manual revert
    // path still uses `RevertError`, but it lives in the dashboard
    // handler outside the REM cycle.
    /// Archive-proposal emission failure (archive detector path).
    #[error("rem archive: {0}")]
    Archive(#[from] ArchiveError),
    /// Briefing inbox failure surfaced by the Briefing dispatcher
    /// or Backlink reciprocity detector when the notify pipeline raises
    /// an infrastructure-level error (sql / io). Per-finding soft errors
    /// (rate-limited inbox, invalid input on a synthesised row) are
    /// collected in the sub-job report rather than bubbling here.
    #[error("rem briefing: {0}")]
    Briefing(#[from] BriefingError),
    /// LLM call failed mid-cycle. Per the
    /// REM cycle, this aborts
    /// the sub-job (and therefore the cycle) rather than being
    /// soft-collected: the operator configured a specific model and
    /// expects that quality bar, not a silently degraded run.
    #[error("rem llm: {0}")]
    Llm(String),
    /// Hybrid prompt loader failure: either the workdir override at
    /// `<workdir>/prompts/<name>.md` is malformed (missing the
    /// `text` fence, etc.) or — much less likely — the bundled
    /// default itself is malformed. Surfaces loudly so the operator
    /// notices a hand-edit mistake before the cycle silently runs on
    /// the wrong prompt.
    #[error("rem prompt loader: {0}")]
    Prompt(#[from] prompts::PromptError),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, RemError>;

// ---------- Public entrypoint ----------

/// Run one full REM cycle.
///
/// Dependencies are split cleanly: `pool` for SQL, `tree` for the
/// memory-wiki filesystem, `embedder` for the sub-jobs that re-embed
/// text they touch (revisor dedup apply, provenance hygiene, the date
/// normalizer, the briefing processor's comment-apply path — the
/// revisor's semantic nomination channel instead reads the vectors
/// already stored on the rows), and a [`RemLlms`] bag carrying the
/// per-sub-job model handles so the operator can wire each function to
/// a different profile (`hub_writer` → workhorse, `revisor` → small,
/// `auto_promote` → strong; per the ingest pipeline).
///
/// Order of sub-jobs is fixed:
/// **auto-apply → revisor → auto-promote → hub-writer**.
/// The auto-apply/auto-finalize sweeps land overdue proposals first,
/// revisor + auto-promote emit fresh proposals, and hub-writer
/// summarises last so its prompt sees a stable snapshot.
///
/// # Errors
///
/// See [`RemError`].
#[allow(
    clippy::too_many_lines,
    reason = "orchestrator threads each sub-job's pool/tree/policy bag through a fixed call sequence; splitting it just to dodge the line cap would hurt readability"
)]
pub async fn run_cycle(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: Arc<dyn Embedder>,
    llms: &RemLlms<'_>,
    policy: &RemPolicy,
) -> Result<RemCycleReport> {
    let cycle_id = policy
        .cycle_id
        .clone()
        .unwrap_or_else(|| format!("cycle-{}", chrono::Utc::now().timestamp()));
    let now = policy.now.unwrap_or_else(Utc::now);
    let started_at = now;
    tracing::info!(cycle_id, "rem: cycle start");

    // Build the family index once per cycle: which wikis are
    // smart wikis (per-wiki `_meta.md` flag). Drives the
    // write-job exclusion for the legacy sub-jobs and the inclusion
    // filter for the two new ones.
    let smart_wiki_index = load_smart_wiki_index(tree)?;

    // Expire aged confirmer memos before any sub-job reads them, so a
    // question whose TTL ran out is re-asked in THIS cycle rather than
    // the next one.
    let verdict_memo_purged =
        rem_verdicts::purge_older_than(pool, now - policy.verdict_memo_ttl).await?;

    let auto_apply = run_auto_apply_sweep(pool, tree, now).await?;
    let auto_finalize = run_auto_finalize_sweep(pool, now).await?;
    let revisor = run_revisor_jaccard(
        pool,
        tree,
        &embedder,
        llms.revisor,
        &cycle_id,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let auto_promote = run_auto_promote(
        pool,
        tree,
        llms.auto_promote,
        &cycle_id,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let page_merge = run_page_merge(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let completion_sweep = run_completion_sweep(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        now,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let contradiction_sweep = run_contradiction_sweep(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        now,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let refile_sweep = run_refile_sweep(
        pool,
        tree,
        llms.revisor,
        &cycle_id,
        now,
        policy,
        &smart_wiki_index,
    )
    .await?;
    // The recall-repair sub-job runs after the refile sweep so a fact the
    // sweep just moved is re-checked against its NEW home (a repaired miss
    // goes stale instead of double-moving).
    let recall_repair = run_recall_repair(
        pool,
        tree,
        &embedder,
        llms.revisor,
        llms.navigator,
        &cycle_id,
        now,
        policy,
        &smart_wiki_index,
    )
    .await?;
    // Provenance hygiene runs right before the date normalizer — its
    // deterministic sibling on the same edit+re-embed shape — so the
    // normalizer (and every later sub-job) already sees pointer-clean text.
    let provenance_hygiene =
        run_provenance_hygiene(pool, &embedder, &cycle_id, policy, &smart_wiki_index).await?;
    let date_normalizer = run_date_normalizer(
        pool,
        tree,
        llms.revisor,
        &embedder,
        &cycle_id,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let archive_detector =
        run_archive_detector(pool, tree, &cycle_id, now, policy, &smart_wiki_index).await?;
    let briefing_dispatcher =
        run_briefing_dispatcher(pool, tree, &cycle_id, now, policy, &smart_wiki_index).await?;
    let backlink_reciprocity =
        run_backlink_reciprocity(pool, tree, &cycle_id, policy, &smart_wiki_index).await?;
    let lease_expirer = run_lease_expirer(pool, now, policy).await?;
    let briefing_processor = run_briefing_processor_non_smart(
        pool,
        tree,
        &embedder,
        llms.comment_applier,
        now,
        policy,
        &smart_wiki_index,
    )
    .await?;
    let husk_gc = run_husk_gc(pool, tree, &cycle_id, now, policy, &smart_wiki_index).await?;
    let hub_writer = run_hub_writer(
        pool,
        tree,
        llms.hub_writer,
        &cycle_id,
        policy,
        &smart_wiki_index,
    )
    .await?;

    let ended_at = Utc::now();
    tracing::info!(
        cycle_id,
        auto_applied = auto_apply.applied.len(),
        auto_finalized = auto_finalize.finalized.len(),
        pairs_examined = revisor.pairs_examined,
        pairs_confirmed = revisor.pairs_confirmed,
        dedup_applied = revisor.applied.len(),
        promote_candidates = auto_promote.candidates_examined,
        grouping_wikis = auto_promote.grouping_wikis_examined,
        grouping_applied = auto_promote.grouping_groups_applied,
        promote_proposals = auto_promote.applied.len(),
        merge_candidates = page_merge.candidates_examined,
        merges_applied = page_merge.applied.len(),
        completion_evidence = completion_sweep.evidence_examined,
        completions_closed = completion_sweep.closed.len(),
        refile_bridge_seeded = refile_sweep.bridge_candidates,
        refile_candidates = refile_sweep.candidates_examined,
        refiled = refile_sweep.refiled.len(),
        contradiction_seeds = contradiction_sweep.seeds_examined,
        satellites_closed = contradiction_sweep.closed.len(),
        provenance_flagged = provenance_hygiene.flagged,
        provenance_moved = provenance_hygiene.moved.len(),
        dates_flagged = date_normalizer.flagged,
        dates_rewritten = date_normalizer.rewritten.len(),
        archive_paths_examined = archive_detector.paths_examined,
        archive_proposals = archive_detector.proposals_emitted.len(),
        briefing_wikis = briefing_dispatcher.wikis_examined,
        briefing_notifies = briefing_dispatcher.notifications_emitted.len(),
        backlink_incoming = backlink_reciprocity.incoming_links,
        backlink_notifies = backlink_reciprocity.notifications_emitted.len(),
        leases_marked_released = lease_expirer.stale_active_marked_released,
        leases_aged_deleted = lease_expirer.aged_released_rows_deleted,
        briefing_processor_examined = briefing_processor.items_examined,
        briefing_processor_processed = briefing_processor.items_processed,
        comment_facts_corrected = briefing_processor.facts_corrected,
        comment_facts_added = briefing_processor.facts_added,
        comment_facts_deduped = briefing_processor.facts_deduped,
        comment_facts_removed = briefing_processor.facts_removed,
        comment_facts_moved = briefing_processor.facts_moved,
        husk_pages_examined = husk_gc.pages_examined,
        husk_pages_removed = husk_gc.removed.len(),
        hub_regenerated = hub_writer.regenerated.len(),
        verdict_memo_purged,
        "rem: cycle done"
    );
    let verdict_memo_rows = rem_verdicts::count(pool).await?;
    Ok(RemCycleReport {
        cycle_id,
        started_at,
        ended_at,
        auto_apply,
        auto_finalize,
        revisor,
        auto_promote,
        page_merge,
        completion_sweep,
        refile_sweep,
        contradiction_sweep,
        recall_repair,
        provenance_hygiene,
        date_normalizer,
        archive_detector,
        briefing_dispatcher,
        backlink_reciprocity,
        lease_expirer,
        briefing_processor,
        husk_gc,
        hub_writer,
        verdict_memo_purged,
        verdict_memo_rows,
    })
}

// ---------- Smart family index ----------

/// Cycle-scoped cache of `wiki_id -> smart`. One tree walk reading
/// the per-wiki smart flag from each `_meta.md` (replaces
/// the old `wiki_types_registry` round-trip). The 5 legacy write-jobs
/// and the 3 smart-wiki-aware sub-jobs share the same map so they all
/// classify the same wikis identically (no race between sub-jobs).
type SmartWikiIndex = HashMap<String, bool>;

fn load_smart_wiki_index(tree: &WikiTree) -> Result<SmartWikiIndex> {
    let mut idx = SmartWikiIndex::new();
    for d in tree.walk()? {
        idx.insert(d.meta.wiki_id.as_str().to_owned(), d.meta.smart);
    }
    Ok(idx)
}

/// `true` when this is a **smart wiki** — its per-wiki smart
/// flag (`_meta.md`) is set. Unknown `wiki_id`s — typically a wiki
/// deleted between the snapshot and now — default to `false` (treated
/// like a non-smart standard wiki). This keeps the legacy
/// write-jobs working on partially-broken trees rather than silently
/// dropping work.
fn is_smart_wiki(smart_wiki_index: &SmartWikiIndex, wiki_id: &str) -> bool {
    smart_wiki_index.get(wiki_id).copied().unwrap_or(false)
}

// ---------- Family scopes (leva-2) ----------

/// One consolidation scope: a FAMILY LINE — a top-level standard wiki
/// plus its sub-wiki descendants (inclusive), in walk order.
///
/// The consolidation passes (dedup revisor, completion sweep,
/// contradiction sweep, page-merge) pool their candidates per family,
/// so the fragments of a subject split across a wiki and its own
/// emergent sub-wiki finally reconcile — the parent↔sub-wiki dedup gap.
/// Arbitrary cross-wiki pairs stay out of scope (self-correcting REM's
/// future business); smart wikis are excluded entirely, as every pass
/// already skips them.
struct FamilyScope {
    /// The family root's wiki id — the label WAL ops carry.
    root_id: String,
    /// Every member wiki id, root first (walk order).
    wiki_ids: Vec<String>,
}

/// Partition the non-smart wikis into family lines.
///
/// Membership is DIRECTORY nesting (component-wise prefix on
/// `abs_dir`) — never the id string: a legit top-level wiki id may
/// contain hyphens (`famiglia-bruno-battaglia` is famiglia's child
/// because it lives at `wikis/famiglia/bruno-battaglia/`, not because
/// of its name). `walk()` is path-sorted, so a root always precedes
/// its descendants and the linear scan below sees the root first.
fn family_scopes(tree: &WikiTree, smart_wiki_index: &SmartWikiIndex) -> Result<Vec<FamilyScope>> {
    let mut scopes: Vec<(std::path::PathBuf, FamilyScope)> = Vec::new();
    for d in tree.walk()? {
        if is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        let id = d.meta.wiki_id.as_str().to_owned();
        if let Some((_, scope)) = scopes
            .iter_mut()
            .find(|(root_dir, _)| d.abs_dir.starts_with(root_dir))
        {
            scope.wiki_ids.push(id);
        } else {
            let scope = FamilyScope {
                root_id: id.clone(),
                wiki_ids: vec![id],
            };
            scopes.push((d.abs_dir.clone(), scope));
        }
    }
    Ok(scopes.into_iter().map(|(_, s)| s).collect())
}

/// `wiki_id → family root id`, for pair gating where only a relation
/// test is needed (the page-merge nomination). Derived from
/// [`family_scopes`] so every pass classifies families identically.
fn family_roots(scopes: &[FamilyScope]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for s in scopes {
        for w in &s.wiki_ids {
            map.insert(w.clone(), s.root_id.clone());
        }
    }
    map
}

/// The active rows of every member of one family, member walk order
/// (each member's rows already `created_at ASC` from the per-wiki
/// query).
async fn find_active_in_family(
    pool: &SqlitePool,
    scope: &FamilyScope,
) -> Result<Vec<FactIndexRow>> {
    let mut rows = Vec::new();
    for w in &scope.wiki_ids {
        rows.extend(fact_index::find_active_in_wiki(pool, w).await?);
    }
    Ok(rows)
}

// ---------- Auto-apply sweep sub-job ----------

/// Thin wrapper around [`proposals::auto_apply_overdue_proposals`]
/// that adapts the call's report shape to the REM
/// sub-job report shape ([`AutoApplyReport`]).
///
/// Per the [memory model](../../../docs/concepts/memory-model.md)
/// the sweep flips `pending → applied_pending_confirm` with
/// `apply_mode='auto'` and starts the 7 d confirm window; silence past
/// the deadline triggers the auto-revert sweep. Per-row
/// handler failures are collected and the sweep keeps going.
async fn run_auto_apply_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    now: DateTime<Utc>,
) -> Result<AutoApplyReport> {
    let sweep = proposals::auto_apply_overdue_proposals(pool, tree, now).await?;
    Ok(AutoApplyReport {
        candidates_examined: sweep.candidates_examined,
        applied: sweep.auto_applied,
        errors: sweep.errors,
    })
}

/// Thin wrapper around [`proposals::auto_finalize_unconfirmed_proposals`]
/// that adapts the call's report
/// shape to the REM sub-job report shape ([`AutoFinalizeReport`]).
///
/// Per the [memory model](../../../docs/concepts/memory-model.md)
/// the sweep flips `applied_pending_confirm → applied` once
/// `confirm_deadline` has elapsed (silence = consent). No kind inverse
/// handler, no `revert_token` minted, no event emitted — the user was
/// already notified at auto-apply time via `EventKind::AutoApplied`,
/// silence is now a valid form of consent.
async fn run_auto_finalize_sweep(
    pool: &SqlitePool,
    now: DateTime<Utc>,
) -> Result<AutoFinalizeReport> {
    let sweep = proposals::auto_finalize_unconfirmed_proposals(pool, now).await?;
    Ok(AutoFinalizeReport {
        candidates_examined: sweep.candidates_examined,
        finalized: sweep.finalized,
    })
}

// ---------- Revisor jaccard semantic sub-job ----------

#[allow(
    clippy::too_many_lines,
    reason = "pairwise pre-pass + LLM confirm + proposal emit live as one loop on purpose"
)]
async fn run_revisor_jaccard(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<RevisorReport> {
    let mut report = RevisorReport::default();
    // Resource guard on the LLM confirms (both nomination channels);
    // logged when it trips — never a silent truncation.
    let mut examined_capped = false;
    // Family scope (leva-2): a wiki + its own sub-wiki descendants pool
    // their facts, so the duplicated identity facts of a subject split
    // across the line (parent wiki ↔ emergent sub-wiki) finally meet.
    // Smart wikis are out entirely — the smart consumer owns those
    // writes via `wiki_admin_push`, REM never dedups them.
    for scope in family_scopes(tree, smart_wiki_index)? {
        if report.applied.len() >= policy.revisor_cap || examined_capped {
            break;
        }
        let facts = find_active_in_family(pool, &scope).await?;
        if facts.len() < 2 {
            continue;
        }
        let ngrams: Vec<HashSet<String>> = facts
            .iter()
            .map(|f| recall::ngrams(&f.text, recall::DEFAULT_NGRAM))
            .collect();
        // Channel-page membership per fact: dedup pairs never cross the
        // boundary (both sides on a reserved channel page, or neither).
        let on_channel_page: Vec<bool> = facts
            .iter()
            .map(|f| wiki::is_channel_page(&f.source_path))
            .collect();
        // Sort by created_at descending so the *newer* fact in a pair
        // is the survivor (capture's natural flow).
        let mut idxs: Vec<usize> = (0..facts.len()).collect();
        idxs.sort_by(|a, b| facts[*b].created_at.cmp(&facts[*a].created_at));
        // Track losers we already proposed so we don't double-fire on
        // the same row within one cycle.
        let mut proposed_losers: HashSet<String> = HashSet::new();
        for (i_pos, &new_idx) in idxs.iter().enumerate() {
            if report.applied.len() >= policy.revisor_cap || examined_capped {
                break;
            }
            if proposed_losers.contains(facts[new_idx].fact_id.as_str()) {
                continue;
            }
            for &old_idx in idxs.iter().skip(i_pos + 1) {
                if report.applied.len() >= policy.revisor_cap || examined_capped {
                    break;
                }
                if proposed_losers.contains(facts[old_idx].fact_id.as_str()) {
                    continue;
                }
                // A behaviour rule dedups only against another rules-page
                // fact (rule-vs-rule; in practice the same page — one
                // `rules.md` per wiki). A pair mixing a rule with an
                // ordinary fact is never nominated: if the rule lost, its
                // content would survive only OFF `rules.md`, out of the
                // behaviour-rules channel — the dedup twin of the compiler
                // and refile skips. A structural channel invariant, not a
                // semantic gate: rule-vs-rule pairs still go to the LLM.
                if on_channel_page[new_idx] != on_channel_page[old_idx] {
                    continue;
                }
                // Identity-core stickiness: background dedup never retires a
                // fact from the owner's always-on identity core (role /
                // relationship / bio, `salience=high`). The loser is
                // `facts[old_idx]` (the pair sorts newest-first, older side
                // retired); if that is an identity-core fact, skip the pair so
                // a relationship like "Frodo è il compagno di Galadriel" is
                // changed only by an explicit correction, never silently
                // consolidated away. A structural channel invariant, same
                // shape as the rules-page guard above — the LLM never sees it.
                if facts[old_idx].is_identity_core() {
                    continue;
                }
                let score = recall::jaccard_sets(&ngrams[new_idx], &ngrams[old_idx]);
                // At/above the threshold the pair is write-time dedup
                // territory (the direct capture scan, and the light dream
                // re-running it at promotion) — the revisor leaves it.
                if score >= policy.revisor_jaccard_max {
                    continue;
                }
                // Two nomination channels; the LLM makes the verdict
                // either way. SURFACE: the jaccard band. SEMANTIC: the
                // embedding cosine — catches the same claim restated with
                // the subject spelled out vs elided, which shares meaning
                // but few n-grams. Bit-identical vectors carry no signal
                // (identical text is threshold-fold territory, and a
                // fixed-vector test embedder would otherwise nominate
                // every pair).
                let surface = score >= policy.revisor_jaccard_min;
                // `Some(cosine)` when the SEMANTIC channel nominated the
                // pair — kept so the persisted receipt reason can name the
                // nominating channel with its score.
                let semantic = (!surface
                    && facts[new_idx].embedding.len() == facts[old_idx].embedding.len()
                    && facts[new_idx].embedding != facts[old_idx].embedding)
                    .then(|| {
                        recall::cosine_similarity(
                            &facts[new_idx].embedding,
                            &facts[old_idx].embedding,
                        )
                    })
                    .filter(|&cosine| cosine >= policy.revisor_cosine_min);
                if !surface && semantic.is_none() {
                    continue;
                }
                let prompt = revisor_prompt(tree, &facts[new_idx], &facts[old_idx])?;
                // The memo check sits BEFORE the examined cap on purpose:
                // a pair whose "not the same" verdict is already on record
                // must not consume tonight's confirm budget. That budget
                // exists to reach pairs nobody has judged yet — the live
                // corpus had 156 nominable pairs against a cap of 120, so
                // re-buying settled verdicts meant the tail was never
                // examined at all.
                let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
                if rem_verdicts::is_settled(pool, rem_verdicts::kind::DEDUP_PAIR, &memo_key).await?
                {
                    continue;
                }
                if report.pairs_examined >= policy.revisor_examined_cap {
                    examined_capped = true;
                    break;
                }
                report.pairs_examined += 1;
                let resp = llm
                    .complete(
                        CompletionRequest::new(prompt)
                            .with_temperature(0.1)
                            .with_max_tokens(60),
                    )
                    .await
                    .map_err(|e| {
                        RemError::Llm(format!(
                            "revisor failed on pair ({}, {}): {e}",
                            facts[new_idx].fact_id.as_str(),
                            facts[old_idx].fact_id.as_str()
                        ))
                    })?;
                if !parse_llm_yes(&resp.text) {
                    rem_verdicts::record_negative(
                        pool,
                        rem_verdicts::kind::DEDUP_PAIR,
                        &memo_key,
                        &format!(
                            "{} vs {}",
                            facts[new_idx].fact_id.as_str(),
                            facts[old_idx].fact_id.as_str()
                        ),
                    )
                    .await?;
                    continue;
                }
                report.pairs_confirmed += 1;
                let op_id = wal::begin_rem_op(
                    pool,
                    cycle_id,
                    "dedup_merge_apply",
                    Some(scope.root_id.as_str()),
                    None,
                )
                .await?;
                // 0032: address the merge receipt to the winner fact's
                // human (the survivor is the one that stays on the page).
                let recipient = proposals::recipient_from_fact(
                    &facts[new_idx].owner_id,
                    facts[new_idx].sender_id.as_ref(),
                );
                let hints = DedupMergeHints {
                    jaccard: Some(score),
                    // The winner's own wiki: on a pair straddling the
                    // family line the survivor stays where it lives.
                    source_wiki_id: Some(facts[new_idx].wiki_id.clone()),
                    reason: Some(semantic.map_or_else(
                        || format!("rem revisor: jaccard={score:.2} + revisor confirm"),
                        |cosine| format!(
                            "rem revisor: cosine={cosine:.2} nominated (jaccard={score:.2} sub-band) + revisor confirm"
                        ),
                    )),
                };
                match dedup::apply_dedup_merge_direct(
                    pool,
                    tree,
                    embedder.clone(),
                    &facts[new_idx].fact_id,
                    &facts[old_idx].fact_id,
                    &hints,
                    recipient.clone(),
                )
                .await
                {
                    Ok(receipt) => {
                        wal::complete_rem_op(pool, op_id).await?;
                        report.applied.push(receipt.proposal_id.clone());
                        proposed_losers.insert(facts[old_idx].fact_id.as_str().to_owned());
                        events::insert_event(
                            pool,
                            EventKind::StructureApplied,
                            Some(facts[old_idx].wiki_id.as_str()),
                            Some(facts[old_idx].fact_id.as_str()),
                            &json!({
                                "proposal_id": receipt.proposal_id,
                                "variant": "dedup_merge",
                                "winner_fact_id": facts[new_idx].fact_id.as_str(),
                                "loser_fact_id": facts[old_idx].fact_id.as_str(),
                                "jaccard": score,
                                "recipient_id": recipient,
                                "revert_deadline": receipt.revert_deadline.to_rfc3339(),
                                "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
                            }),
                        )
                        .await?;
                    },
                    Err(e) => {
                        wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                        report.errors.push(format!("dedup_merge apply failed: {e}"));
                    },
                }
            }
        }
    }
    if examined_capped {
        tracing::info!(
            examined = report.pairs_examined,
            cap = policy.revisor_examined_cap,
            "rem revisor: examined cap reached — remaining candidate pairs wait for the next cycle"
        );
    }
    Ok(report)
}

/// Bundled default for the `rem-dedup` system prompt.
///
/// The verbatim prompt body lives in
/// `crates/mwe-core/prompts/rem-dedup.md` (frontmatter + a single
/// ```text ... ``` fenced block) and is loaded through
/// [`prompts::render`]; an operator override at
/// `<workdir>/prompts/rem-dedup.md` wins when present. Referenced
/// from [`prompts::BUNDLED`] so `mwe-mcp init` materialises it
/// under the workdir.
pub const BUNDLED_REM_DEDUP_MD: &str = include_str!("../prompts/rem-dedup.md");

fn revisor_prompt(tree: &WikiTree, new: &FactIndexRow, old: &FactIndexRow) -> Result<String> {
    // The page each region lives on frames its subject: compiled prose
    // routinely elides a subject the page itself establishes ("È nato il
    // 23 maggio 1984" on Franz's page). Without this context the model
    // cannot tell whether two subject-elided claims talk about the same
    // entity — the confirm would fail-safe to "not the same" and the
    // duplicate would survive every night.
    let new_page = format!("{} · {}", new.wiki_id, new.source_path);
    let old_page = format!("{} · {}", old.wiki_id, old.source_path);
    prompts::render(
        "rem-dedup",
        tree.workdir(),
        BUNDLED_REM_DEDUP_MD,
        &[
            ("new", new.text.as_str()),
            ("old", old.text.as_str()),
            ("new_page", new_page.as_str()),
            ("old_page", old_page.as_str()),
        ],
    )
    .map_err(RemError::from)
}

fn parse_llm_yes(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let Some(start) = bytes.iter().position(|&b| b == b'{') else {
        return false;
    };
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &raw[start..=i];
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                        return v
                            .get("same")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                    }
                    return false;
                }
            },
            _ => {},
        }
    }
    false
}

// ---------- Auto-promote sub-job ----------

/// Per page over the floor (`auto_promote_min_page_facts`, the only
/// deterministic gate — a resource pre-filter), show the **whole
/// page** to the `rem_promotions` LLM with each fact's 30-day recall
/// count and ask whether one sub-topic outgrew its siblings; on a
/// split verdict **apply the move directly** (act-first: born-applied
/// receipt + `structure_applied` notice, no pending proposal).
/// Hard-capped by `policy.auto_promote_cap`.
///
/// No LLM → the sub-job short-circuits cleanly with
/// `disabled_reason = Some("no rem_promotions LLM wired")`. See the
/// REM cycle.
#[allow(
    clippy::too_many_lines,
    reason = "filter + LLM call + dedup check + emit live as one orchestrator"
)]
async fn run_auto_promote(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: Option<&dyn LlmBackend>,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<AutoPromoteReport> {
    let mut report = AutoPromoteReport::default();
    let Some(llm) = llm else {
        report.disabled_reason = Some("no rem_promotions LLM wired".to_owned());
        return Ok(report);
    };
    // The whole tree up front: the grouping pass needs to see a wiki's
    // existing sub-wikis to prefer filing pages into them over founding
    // a second home for the same subject.
    let all_wikis = tree.walk()?;
    for d in &all_wikis {
        if report.applied.len() >= policy.auto_promote_cap {
            break;
        }
        // REM never auto-promotes a smart-wiki fact — the
        // smart consumer is the sole writer.
        if is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        let facts = fact_index::find_active_in_wiki(pool, d.meta.wiki_id.as_str()).await?;
        // Mass-per-page: how many active facts each page (`source_path`)
        // carries. A page that has accumulated mass is the promotion
        // candidate — the trigger is forma fisica (mass/ramification),
        // not a single fact's word count.
        let mut page_mass: HashMap<&str, usize> = HashMap::new();
        for f in &facts {
            *page_mass.entry(f.source_path.as_str()).or_default() += 1;
        }
        // Page-group → wiki regrouping. Runs *before* the paragraph
        // loop, but the two no longer compete for the same signal: this
        // pass moves whole pages between wikis on the strength of how
        // many of them are one subject, while the loop below splits a
        // page that has grown too heavy. A page this pass relocated is
        // skipped below — the `facts` snapshot predates the move and
        // still points at the page's old home.
        let regrouped = run_page_grouping_for_wiki(
            pool,
            tree,
            llm,
            cycle_id,
            policy,
            d,
            &all_wikis,
            smart_wiki_index,
            &facts,
            &page_mass,
            &mut report,
        )
        .await?;
        // Paragraph → page split, **per page** (rule 2 of the forma
        // scale): the LLM reads the whole page — every fact annotated
        // with its 30-day recall count — and decides whether one
        // sub-topic outgrew its siblings (mass) and/or is hot
        // (recall), naming the facts that move out. The page floor is
        // the only deterministic gate, a cheap resource pre-filter so
        // tiny pages never reach the LLM; everything semantic is the
        // LLM's call ([memory model](../../../docs/concepts/memory-model.md)).
        let mut pages: Vec<&str> = page_mass
            .iter()
            .filter(|&(_, &m)| m >= policy.auto_promote_min_page_facts)
            .map(|(&p, _)| p)
            .filter(|p| !regrouped.contains(*p))
            .collect();
        pages.sort_unstable();
        for source_path in pages {
            if report.applied.len() >= policy.auto_promote_cap {
                break;
            }
            let page_facts: Vec<&FactIndexRow> = facts
                .iter()
                .filter(|f| f.source_path == source_path)
                .collect();
            // The promote handler joins `source_page` onto the wiki's
            // abs_dir, so it must be wiki-relative (`index.md`), NOT the
            // fact's workdir-relative `source_path`
            // (`wikis/<id>/index.md`) — passing the latter doubled the
            // prefix and made every REM paragraph_to_file apply miss on
            // disk. Compute it up front: it gates a cheap malformed-path
            // skip AND scopes the dedup below to receipts promoted FROM
            // this page.
            let Some(source_page_rel) = wiki_relative_page(d, source_path) else {
                report.errors.push(format!(
                    "auto_promote: {source_path} is not under wiki {}",
                    d.meta.wiki_id.as_str(),
                ));
                continue;
            };
            // Coarse dedup: skip the page only if a genuine page-promotion
            // receipt (paragraph_to_file / file_to_subwiki) already moved
            // one of THESE facts OUT OF THIS SAME page — the emergence pass
            // above may have just done so. Lifecycle ops that share
            // kind='wiki_promote' and receipts for other pages must not
            // veto (see already_promoted_for).
            let mut already = false;
            for f in &page_facts {
                if already_promoted_for(pool, &f.fact_id, d.meta.wiki_id.as_str(), &source_page_rel)
                    .await?
                {
                    already = true;
                    break;
                }
            }
            if already {
                continue;
            }
            // Already answered "no" on this exact page content? Don't
            // re-buy the verdict — this pass runs on the strong model and
            // ships the whole page in the prompt, so a byte-identical
            // re-ask is the most expensive no-op in the cycle.
            let memo_prompt = paragraph_split_memo_prompt(tree, &source_page_rel, &page_facts)?;
            let memo_key = rem_verdicts::key(llm.model_id(), &memo_prompt);
            if rem_verdicts::is_settled(pool, rem_verdicts::kind::PAGE_SPLIT, &memo_key).await? {
                continue;
            }
            report.candidates_examined += 1;

            let mass = page_facts.len();
            let prompt = paragraph_split_prompt(tree, &source_page_rel, &page_facts)?;
            let resp = llm
                .complete(
                    CompletionRequest::new(prompt)
                        .with_temperature(0.2)
                        .with_max_tokens(4_000),
                )
                .await
                .map_err(|e| {
                    RemError::Llm(format!(
                        "auto_promote failed on page {source_page_rel}: {e}"
                    ))
                })?;
            let Some(decision) = parse_split_decision(&resp.text) else {
                report.errors.push(format!(
                    "auto_promote llm returned unparseable verdict for page {source_page_rel}",
                ));
                continue;
            };
            if !decision.split {
                rem_verdicts::record_negative(
                    pool,
                    rem_verdicts::kind::PAGE_SPLIT,
                    &memo_key,
                    &format!("{}/{source_page_rel}", d.meta.wiki_id.as_str()),
                )
                .await?;
                continue;
            }
            // Validate the named facts: every handle must resolve on the
            // page, and the set must be a *proper* subset — moving
            // everything is a rename, not a split (that is the
            // page→sub-wiki rung).
            let mut moving: Vec<&FactIndexRow> = Vec::with_capacity(decision.fact_ids.len());
            let mut invalid = None;
            for id in &decision.fact_ids {
                if let Some(f) = resolve_split_handle(&page_facts, id) {
                    moving.push(f);
                } else {
                    invalid = Some(id.clone());
                    break;
                }
            }
            if let Some(id) = invalid {
                report.errors.push(format!(
                    "auto_promote llm named fact {id} not on page {source_page_rel}",
                ));
                continue;
            }
            if moving.is_empty() || moving.len() >= page_facts.len() {
                report.errors.push(format!(
                    "auto_promote llm split of {source_page_rel} must move a proper, non-empty \
                     subset ({} of {mass} named)",
                    moving.len(),
                ));
                continue;
            }
            report.candidates_promoted += 1;

            // Same canonical chokepoint as the ingest classifier: the
            // LLM-proposed name (or the fallback) must not coin a second
            // spelling of an existing concept.
            let canonical = decision
                .target_page
                .as_deref()
                .and_then(crate::planner::canonical_page_path)
                .unwrap_or_else(|| default_target_page(&moving[0].text));
            // Flatten to the single-segment concept-leaf form (`<slug>.md`):
            // plan pages never nest, and the plan-sync re-home below keys the
            // destination by slug — a nested split target would leave the
            // plan pointing at a different file than the move wrote.
            let target_slug =
                crate::planner::slugify(canonical.strip_suffix(".md").unwrap_or(&canonical));
            if target_slug.is_empty() {
                report.errors.push(format!(
                    "auto_promote: unusable target page name for {source_page_rel}",
                ));
                continue;
            }
            let recommended_target = format!("{target_slug}.md");
            let op_id = wal::begin_rem_op(
                pool,
                cycle_id,
                "auto_promote_apply",
                Some(d.meta.wiki_id.as_str()),
                None,
            )
            .await?;
            let recipient =
                proposals::recipient_from_fact(&moving[0].owner_id, moving[0].sender_id.as_ref());
            let hot = moving.iter().map(|f| f.recall_count_30d).max();
            let hints = ParagraphToFileHints {
                trigger_page_facts: Some(mass),
                recall_count_30d: hot,
                reason: Some(format!(
                    "rem per-page split: {n} of {mass} facts move to {recommended_target}",
                    n = moving.len(),
                )),
            };
            let fact_ids: Vec<FactId> = moving.iter().map(|f| f.fact_id.clone()).collect();
            match promote::apply_paragraph_to_file_direct(
                pool,
                tree,
                d.meta.wiki_id.as_str(),
                &source_page_rel,
                &fact_ids,
                &recommended_target,
                &hints,
                recipient.clone(),
            )
            .await
            {
                Ok(receipt) => {
                    wal::complete_rem_op(pool, op_id).await?;
                    report.applied.push(receipt.proposal_id.clone());
                    events::insert_event(
                        pool,
                        EventKind::StructureApplied,
                        Some(d.meta.wiki_id.as_str()),
                        Some(moving[0].fact_id.as_str()),
                        &json!({
                            "proposal_id": receipt.proposal_id,
                            "variant": "paragraph_to_file",
                            "source_page": source_page_rel,
                            "target_page": recommended_target,
                            "moved_facts": fact_ids.iter().map(FactId::as_str).collect::<Vec<_>>(),
                            "recipient_id": recipient,
                            "revert_deadline": receipt.revert_deadline.to_rfc3339(),
                            "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
                        }),
                    )
                    .await?;
                    // Plan-sync seam: re-home the moved facts in the persisted
                    // compilation plan so the next build's carry-over does not
                    // fight the move, and the target page gets woven by the
                    // next compile. Soft — the move is applied and journaled
                    // either way.
                    let seed = crate::planner::RehomePageSeed::concept(
                        &target_slug,
                        d.meta.wiki_id.as_str(),
                    );
                    let plan_moves: Vec<(&FactIndexRow, &crate::planner::RehomePageSeed)> =
                        moving.iter().map(|f| (*f, &seed)).collect();
                    match crate::planner::rehome_facts_in_persisted_plan(
                        tree,
                        &plan_moves,
                        &[],
                        &chrono::Utc::now().to_rfc3339(),
                    ) {
                        Ok(n) if n > 0 => tracing::debug!(
                            rehomed = n,
                            target = %target_slug,
                            "auto_promote: persisted plan re-homed"
                        ),
                        Ok(_) => {},
                        Err(e) => report.errors.push(format!(
                            "auto_promote: plan re-home failed (move applied): {e}",
                        )),
                    }
                },
                Err(e) => {
                    wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                    report
                        .errors
                        .push(format!("apply paragraph_to_file failed: {e}"));
                },
            }
        }
    }
    Ok(report)
}

/// Coarse dedup for the auto-promote passes: has `fact_id` already been
/// promoted OUT OF this same `(source_wiki_id, source_page)` by a genuine
/// page-promotion receipt? An `applied` row is a receipt of a promote
/// already performed; a `pending` row is one in flight. Both suppress
/// re-promoting the same fact over successive REM cycles.
///
/// Two filters make this precise — without them the pass was inert
/// (`candidates_examined` stuck at exactly 0 for every over-mass page):
///
/// - **Variant.** `kind = 'wiki_promote'` is overloaded: routine
///   fact-lifecycle ops (`validity_close`, `fact_refile`, `acl_change`,
///   `validity_edit`, `page_merge`) share the kind and each stamp their
///   `fact_id` into `context`. A `kind`-only match let ANY once-closed /
///   refiled / re-ACL'd fact veto its whole page. Only `paragraph_to_file`
///   and `file_to_subwiki` are real page-promotion receipts, so match
///   those.
/// - **Source scope.** A receipt records the page a fact was promoted
///   FROM. Matching `source_wiki_id`/`source_page` stops an old receipt
///   from vetoing a fact that has since migrated onto a *different* page
///   (e.g. a fact promoted off `index.md` that later landed on
///   `esperienze_agente.md` must not freeze the latter).
async fn already_promoted_for(
    pool: &SqlitePool,
    fact_id: &FactId,
    source_wiki_id: &str,
    source_page: &str,
) -> Result<bool> {
    let needle = fact_id.as_str();
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM structure_proposals \
         WHERE kind = 'wiki_promote' \
           AND status IN ('pending', 'applied') \
           AND json_extract(context, '$.variant') IN ('paragraph_to_file', 'file_to_subwiki') \
           AND json_extract(context, '$.source_wiki_id') = ? \
           AND json_extract(context, '$.source_page') = ? \
           AND context LIKE '%' || ? || '%'",
    )
    .bind(source_wiki_id)
    .bind(source_page)
    .bind(needle)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Bundled default for the `rem-promotions` system prompt.
/// See [`BUNDLED_REM_DEDUP_MD`] for the loader contract.
pub const BUNDLED_REM_PROMOTIONS_MD: &str = include_str!("../prompts/rem-promotions.md");

/// Bundled default for the `rem-page-grouping` system prompt
/// (page-group → wiki cartography verdict). Same hybrid loader
/// contract as [`BUNDLED_REM_PROMOTIONS_MD`].
pub const BUNDLED_REM_PAGE_GROUPING_MD: &str = include_str!("../prompts/rem-page-grouping.md");

/// Coarse recall band. Used **only** to build the memo key
/// ([`paragraph_split_memo_prompt`]) — never shown to the model, which
/// keeps seeing the exact count.
///
/// A page's split verdict does not turn on one extra recall hit; it
/// turns on whether a sub-topic is cold, warm, or hot. Keying the memo
/// on the raw counter would re-open every page every night for a number
/// the model does not read that finely — which is the same waste the
/// memo exists to remove.
const fn recall_band(count: i64) -> &'static str {
    match count {
        ..=0 => "none",
        1..=4 => "low",
        5..=19 => "medium",
        _ => "high",
    }
}

/// Render the per-page split prompt: the whole page, each fact
/// annotated with a short positional handle and its 30-day recall count,
/// so the LLM weighs mass and recall together and names the facts that
/// move out.
///
/// The handle (`[n1]`, `[n2]`, …) replaces the fact's UUID. The model
/// never reasons over a UUID — it only echoes one back to name what
/// moves — and a UUID costs ~18 tokens of pure noise per fact on the
/// strong model this slot runs on. [`resolve_split_handle`] maps the
/// answer back, and still accepts a raw fact id so an operator prompt
/// override (or a model that echoes an id anyway) keeps working.
///
/// `canonical` swaps each exact recall count for its [`recall_band`] —
/// that rendering is the memo key, never a request body.
fn paragraph_split_prompt_inner(
    tree: &WikiTree,
    page: &str,
    page_facts: &[&FactIndexRow],
    canonical: bool,
) -> Result<String> {
    use std::fmt::Write as _;
    let mass_s = page_facts.len().to_string();
    let mut facts_block = String::new();
    for (i, f) in page_facts.iter().enumerate() {
        let recall = if canonical {
            recall_band(f.recall_count_30d).to_owned()
        } else {
            f.recall_count_30d.to_string()
        };
        let _ = writeln!(
            facts_block,
            "- [n{handle}] recall30d: {recall}\n  {text}",
            handle = i + 1,
            text = f.text.replace('\n', "\n  "),
        );
    }
    prompts::render(
        "rem-promotions",
        tree.workdir(),
        BUNDLED_REM_PROMOTIONS_MD,
        &[
            ("page", page),
            ("page_facts", mass_s.as_str()),
            ("facts", facts_block.as_str()),
        ],
    )
    .map_err(RemError::from)
}

/// The prompt actually sent to the model.
fn paragraph_split_prompt(
    tree: &WikiTree,
    page: &str,
    page_facts: &[&FactIndexRow],
) -> Result<String> {
    paragraph_split_prompt_inner(tree, page, page_facts, false)
}

/// The canonical rendering hashed into the memo key: same template, same
/// facts, recall counters bucketed so day-to-day drift does not re-open
/// a page whose content has not moved.
fn paragraph_split_memo_prompt(
    tree: &WikiTree,
    page: &str,
    page_facts: &[&FactIndexRow],
) -> Result<String> {
    paragraph_split_prompt_inner(tree, page, page_facts, true)
}

/// Resolve one entry of the split verdict's `fact_ids` list against the
/// page the model was shown: a positional handle (`n3`, `[n3]`, `N3`) or
/// a raw fact id. Returns `None` when it matches neither — the caller
/// treats that as a hallucinated name and drops the whole split.
fn resolve_split_handle<'a>(
    page_facts: &[&'a FactIndexRow],
    token: &str,
) -> Option<&'a FactIndexRow> {
    let t = token.trim().trim_matches(|c| c == '[' || c == ']').trim();
    if let Some(digits) = t.strip_prefix(['n', 'N'])
        && let Ok(idx) = digits.parse::<usize>()
        && idx >= 1
        && let Some(f) = page_facts.get(idx - 1)
    {
        return Some(f);
    }
    page_facts.iter().copied().find(|f| f.fact_id.as_str() == t)
}

/// Extract the first brace-balanced JSON object from `raw` (tolerant
/// to prose around the JSON) and parse it. Shared by the strict-JSON
/// verdict parsers of the auto-promote passes.
fn first_json_object(raw: &str) -> Option<serde_json::Value> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&raw[start..=i]).ok();
                }
            },
            _ => {},
        }
    }
    None
}

/// Verdict of the per-page split pass.
#[derive(Debug, Clone, Default)]
struct SplitDecision {
    /// Whether one sub-topic should move to its own page.
    split: bool,
    /// Fact ids that move (validated against the page by the caller:
    /// non-empty, proper subset).
    fact_ids: Vec<String>,
    /// Target page filename, or `None` for the slugified fallback.
    target_page: Option<String>,
}

fn parse_split_decision(raw: &str) -> Option<SplitDecision> {
    let v = first_json_object(raw)?;
    Some(SplitDecision {
        split: v.get("split").and_then(serde_json::Value::as_bool)?,
        fact_ids: v
            .get("fact_ids")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        target_page: v
            .get("target_page")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    })
}

/// Fallback target name when the LLM omits one. Takes the first 4
/// words of the body and canonicalises them through
/// [`crate::planner::slugify`] — the same spelling every other
/// LLM-coined page name gets.
fn default_target_page(body: &str) -> String {
    let words = body
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    let stem = crate::planner::slugify(&words);
    if stem.is_empty() {
        "promoted_paragraph.md".to_owned()
    } else {
        format!("{stem}.md")
    }
}

/// Convert a fact's **workdir-relative** `source_path`
/// (`wikis/<id>/<page>`) to the page path **relative to the wiki dir**
/// (`<page>`) that the `promote` handlers expect — they `join` it onto
/// the wiki's `abs_dir`, so a workdir-relative path would double the
/// `wikis/<id>/` prefix and miss on disk. Returns `None` when the path
/// does not sit under the wiki (defensive; never expected in practice).
fn wiki_relative_page(d: &wiki::DiscoveredWiki, source_path: &str) -> Option<String> {
    std::path::Path::new(source_path)
        .strip_prefix(&d.rel_dir)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// Relative dashboard path for the undo surface of one applied
/// structural receipt: the open-in-chat bridge primes the chat with a
/// modify/undo summary of that receipt. Relative on purpose — the
/// consumer prepends whatever base URL it knows the operator serves
/// the dashboard from (same contract as
/// [`proposals::PENDING_CONFIRMS_DASHBOARD_PATH`]).
fn receipt_dashboard_path(proposal_id: &str) -> String {
    format!("/dashboard/proposals/{proposal_id}/open-in-chat")
}

// ---------- Page-group → wiki regrouping ----------

/// How many verbatim excerpts of a page ride in the inventory the
/// cartographer reads. Two is enough to tell `bagnetto_neonata.md` from
/// `bucato_neonata.md` without shipping the whole corpus in the prompt.
const GROUPING_SNIPPETS_PER_PAGE: usize = 2;

/// Character budget per excerpt.
const GROUPING_SNIPPET_CHARS: usize = 110;

/// The *page-group → wiki* regrouping pass for one wiki — the
/// page→folder rung of the forma fisica scale.
///
/// One LLM call per wiki (not per page): the model reads the wiki's
/// whole page inventory plus the sub-wikis that already exist under it,
/// and cuts groups of pages that are **already** one subject area. A
/// group either becomes a new sub-wiki (`pages_to_subwiki`, floor
/// `policy.auto_promote_group_min_pages`) or moves into a sub-wiki that
/// already exists (`pages_move_wiki`, no floor — the home is there).
///
/// The trigger is therefore **evidence on disk**, never a bet: a wiki
/// is born holding every page of its subject, and can never be born
/// with one page. A page that has merely accumulated mass is the
/// *paragraph → page* pass's business, and that pass now runs
/// unopposed — the two rungs no longer compete for the same signal.
///
/// Returns the workdir-relative `source_path`s the pass moved, so the
/// paragraph pass below can skip them: its `facts` snapshot predates
/// the move and still points at the old home.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors run_auto_promote's orchestrator bag; threading a struct would just hide the same fields"
)]
#[allow(
    clippy::too_many_lines,
    reason = "linear per-wiki pipeline (inventory → prompt → validate → apply); splitting hides the order"
)]
async fn run_page_grouping_for_wiki(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    policy: &RemPolicy,
    d: &wiki::DiscoveredWiki,
    all_wikis: &[wiki::DiscoveredWiki],
    smart_wiki_index: &SmartWikiIndex,
    facts: &[FactIndexRow],
    page_mass: &HashMap<&str, usize>,
    report: &mut AutoPromoteReport,
) -> Result<HashSet<String>> {
    let mut moved: HashSet<String> = HashSet::new();
    if report.applied.len() >= policy.auto_promote_cap {
        return Ok(moved);
    }

    // Candidate pages: every page carrying mass except the wiki's own
    // front page (moving index.md out would decapitate the wiki).
    let mut candidates: Vec<(String, &str, usize)> = page_mass
        .iter()
        .filter_map(|(&source_path, &mass)| {
            let rel = wiki_relative_page(d, source_path)?;
            (rel != "index.md").then_some((rel, source_path, mass))
        })
        .collect();
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // Sub-wikis that already exist under this wiki and can receive
    // pages. A smart wiki never qualifies: its consumer is the sole
    // writer, REM does not file into it.
    let children: Vec<&wiki::DiscoveredWiki> = all_wikis
        .iter()
        .filter(|c| {
            c.meta.parent_wiki_id.as_ref().map(WikiId::as_str) == Some(d.meta.wiki_id.as_str())
                && !is_smart_wiki(smart_wiki_index, c.meta.wiki_id.as_str())
        })
        .collect();

    // Nothing can fire: too few pages for a birth and nowhere to file.
    if candidates.len() < policy.auto_promote_group_min_pages && children.is_empty() {
        return Ok(moved);
    }

    let inventory = grouping_inventory(&candidates, facts);
    let existing = grouping_existing_wikis(&children);
    let prompt = page_grouping_prompt(
        tree,
        d,
        candidates.len(),
        policy.auto_promote_group_min_pages,
        &existing,
        &inventory,
    )?;
    // The memo keys on the rendered prompt, so it re-opens by itself
    // the moment the inventory changes (a page added, split, renamed).
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::PAGE_GROUPING, &memo_key).await? {
        return Ok(moved);
    }
    report.grouping_wikis_examined += 1;

    let resp = llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.2)
                .with_max_tokens(1_200),
        )
        .await
        .map_err(|e| {
            RemError::Llm(format!(
                "page grouping failed on {wiki}: {e}",
                wiki = d.meta.wiki_id.as_str()
            ))
        })?;
    let Some(groups) = parse_page_groups(&resp.text) else {
        report.errors.push(format!(
            "page grouping llm returned unparseable verdict for {wiki}",
            wiki = d.meta.wiki_id.as_str(),
        ));
        return Ok(moved);
    };
    if groups.is_empty() {
        rem_verdicts::record_negative(
            pool,
            rem_verdicts::kind::PAGE_GROUPING,
            &memo_key,
            d.meta.wiki_id.as_str(),
        )
        .await?;
        return Ok(moved);
    }

    let known: HashSet<&str> = candidates.iter().map(|(rel, _, _)| rel.as_str()).collect();
    let child_ids: HashSet<&str> = children.iter().map(|c| c.meta.wiki_id.as_str()).collect();
    let mut claimed: HashSet<String> = HashSet::new();

    for group in groups {
        if report.applied.len() >= policy.auto_promote_cap {
            break;
        }
        // Every named page must exist in this wiki, carry mass, and be
        // claimed by exactly one group. A model that repeats a page
        // across two groups loses the second one, not both.
        let mut pages: Vec<String> = Vec::with_capacity(group.pages.len());
        let mut rejected = false;
        for page in &group.pages {
            if !known.contains(page.as_str()) {
                report.errors.push(format!(
                    "page grouping named unknown page {page} in {wiki}",
                    wiki = d.meta.wiki_id.as_str(),
                ));
                rejected = true;
                break;
            }
            if !claimed.insert(page.clone()) {
                report.errors.push(format!(
                    "page grouping claimed {page} twice in {wiki}",
                    wiki = d.meta.wiki_id.as_str(),
                ));
                rejected = true;
                break;
            }
            pages.push(page.clone());
        }
        if rejected || pages.is_empty() {
            continue;
        }

        let recipient = facts
            .iter()
            .find(|f| wiki_relative_page(d, &f.source_path).is_some_and(|r| r == pages[0]))
            .and_then(|f| proposals::recipient_from_fact(&f.owner_id, f.sender_id.as_ref()));
        let hints = promote::PageGroupHints {
            group_pages: Some(pages.len()),
            source_wiki_pages: Some(candidates.len()),
            reason: Some(format!(
                "rem grouping: {n} of {total} pages of {wiki}",
                n = pages.len(),
                total = candidates.len(),
                wiki = d.meta.wiki_id.as_str(),
            )),
        };

        let outcome = match &group.action {
            GroupAction::Create {
                slug,
                title,
                style,
                description,
            } => {
                // The birth floor. Below it the group is not a subject
                // area with a home to earn — it is a handful of pages,
                // and they stay where they are.
                if pages.len() < policy.auto_promote_group_min_pages {
                    continue;
                }
                let op_id = wal::begin_rem_op(
                    pool,
                    cycle_id,
                    "page_grouping_create",
                    Some(d.meta.wiki_id.as_str()),
                    None,
                )
                .await?;
                let res = promote::apply_pages_to_subwiki_direct(
                    pool,
                    tree,
                    d.meta.wiki_id.as_str(),
                    &pages,
                    slug,
                    title.as_deref(),
                    style.as_deref(),
                    description.as_deref(),
                    &hints,
                    recipient.clone(),
                )
                .await;
                (op_id, res, "pages_to_subwiki")
            },
            GroupAction::Move { target } => {
                if !child_ids.contains(target.as_str()) {
                    report.errors.push(format!(
                        "page grouping named {target}, which is not a sub-wiki of {wiki}",
                        wiki = d.meta.wiki_id.as_str(),
                    ));
                    continue;
                }
                let op_id = wal::begin_rem_op(
                    pool,
                    cycle_id,
                    "page_grouping_move",
                    Some(d.meta.wiki_id.as_str()),
                    None,
                )
                .await?;
                let res = promote::apply_pages_move_wiki_direct(
                    pool,
                    tree,
                    d.meta.wiki_id.as_str(),
                    target,
                    &pages,
                    &hints,
                    recipient.clone(),
                )
                .await;
                (op_id, res, "pages_move_wiki")
            },
        };

        let (op_id, res, variant) = outcome;
        match res {
            Ok(receipt) => {
                wal::complete_rem_op(pool, op_id).await?;
                report.applied.push(receipt.proposal_id.clone());
                report.grouping_groups_applied += 1;
                for (rel, source_path, _) in &candidates {
                    if pages.iter().any(|p| p == rel) {
                        moved.insert((*source_path).to_owned());
                    }
                }
                events::insert_event(
                    pool,
                    EventKind::StructureApplied,
                    Some(d.meta.wiki_id.as_str()),
                    None,
                    &json!({
                        "proposal_id": receipt.proposal_id,
                        "variant": variant,
                        "source_wiki_id": d.meta.wiki_id.as_str(),
                        "pages": pages,
                        "target_wiki_id": receipt.spec.get("target_wiki_id"),
                        "new_wiki_id": receipt.spec.get("new_wiki_id"),
                        "recipient_id": recipient,
                        "revert_deadline": receipt.revert_deadline.to_rfc3339(),
                        "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
                    }),
                )
                .await?;
            },
            Err(e) => {
                wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                report.errors.push(format!("apply {variant} failed: {e}"));
            },
        }
    }

    Ok(moved)
}

/// The page inventory the cartographer reads: one line per page with
/// its mass and a couple of verbatim excerpts.
///
/// The excerpts are the load-bearing part. A page's stored
/// `page_description` is written per fact at routing time and drifts
/// (it routinely describes a neighbouring page, and mixes languages),
/// so it is deliberately **not** used — a wrong label is worse than no
/// label. The filename plus two real sentences is ground truth.
fn grouping_inventory(candidates: &[(String, &str, usize)], facts: &[FactIndexRow]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (rel, source_path, mass) in candidates {
        let _ = write!(out, "- {rel} [{mass}]");
        for (shown, f) in facts
            .iter()
            .filter(|f| &f.source_path == source_path)
            .take(GROUPING_SNIPPETS_PER_PAGE)
            .enumerate()
        {
            let sep = if shown == 0 { " — " } else { " / " };
            let snippet = truncate_chars(f.text.trim(), GROUPING_SNIPPET_CHARS);
            let _ = write!(out, "{sep}\"{snippet}\"");
        }
        out.push('\n');
    }
    out
}

/// The sub-wikis already living under this wiki, with whatever "what
/// goes in here" their `_meta` carries — the model needs them to prefer
/// filing over founding.
fn grouping_existing_wikis(children: &[&wiki::DiscoveredWiki]) -> String {
    use std::fmt::Write as _;
    if children.is_empty() {
        return "(none)\n".to_owned();
    }
    let mut out = String::new();
    for c in children {
        let summary = c
            .meta
            .extra
            .get(serde_yaml::Value::from("summary"))
            .and_then(serde_yaml::Value::as_str)
            .unwrap_or("");
        let pages = std::fs::read_dir(&c.abs_dir).map_or(0, |rd| {
            rd.filter_map(std::result::Result::ok)
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    std::path::Path::new(name.as_ref())
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                        && name != "_meta.md"
                })
                .count()
        });
        let _ = writeln!(
            out,
            "- {id} — {title}{sep}{summary} ({pages} pages)",
            id = c.meta.wiki_id.as_str(),
            title = c.meta.title,
            sep = if summary.is_empty() { "" } else { ": " },
        );
    }
    out
}

/// Truncate on a character boundary, with an ellipsis when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

fn page_grouping_prompt(
    tree: &WikiTree,
    d: &wiki::DiscoveredWiki,
    wiki_pages: usize,
    min_pages: usize,
    existing: &str,
    inventory: &str,
) -> Result<String> {
    let wiki_pages_s = wiki_pages.to_string();
    let min_pages_s = min_pages.to_string();
    prompts::render(
        "rem-page-grouping",
        tree.workdir(),
        BUNDLED_REM_PAGE_GROUPING_MD,
        &[
            ("wiki", d.meta.title.as_str()),
            ("wiki_pages", wiki_pages_s.as_str()),
            ("min_pages", min_pages_s.as_str()),
            ("existing", existing),
            ("inventory", inventory),
        ],
    )
    .map_err(RemError::from)
}

/// What the cartographer decided to do with one group of pages.
#[derive(Debug, Clone)]
enum GroupAction {
    /// Found a new sub-wiki for them (subject to the page floor).
    Create {
        slug: String,
        title: Option<String>,
        /// Dominant style **default** for the newborn wiki's `_meta`, or
        /// `None` when genuinely mixed. A hint, not a gate.
        style: Option<String>,
        /// Free-text "what goes in here" for the newborn wiki's `_meta`.
        description: Option<String>,
    },
    /// File them into a sub-wiki that already exists.
    Move { target: String },
}

#[derive(Debug, Clone)]
struct PageGroup {
    action: GroupAction,
    pages: Vec<String>,
}

/// Parse the cartographer's strict-JSON verdict. Tolerant to prose
/// around the object; a group missing its discriminator, its pages, or
/// (for a birth) its slug is dropped rather than guessed at.
fn parse_page_groups(raw: &str) -> Option<Vec<PageGroup>> {
    let v = first_json_object(raw)?;
    let arr = v.get("groups")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for g in arr {
        let pages: Vec<String> = g
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if pages.is_empty() {
            continue;
        }
        let str_field = |k: &str| {
            g.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let action = match g.get("action").and_then(serde_json::Value::as_str) {
            Some("create") => {
                let Some(slug) = str_field("slug") else {
                    continue;
                };
                GroupAction::Create {
                    slug,
                    title: str_field("title"),
                    style: str_field("style"),
                    description: str_field("description"),
                }
            },
            Some("move") => {
                let Some(target) = str_field("target") else {
                    continue;
                };
                GroupAction::Move { target }
            },
            _ => continue,
        };
        out.push(PageGroup { action, pages });
    }
    Some(out)
}

// ---------- Page-merge sub-job (semantic page consolidation, cure front) ----------

/// Bundled default for the page-merge confirmation prompt.
pub const BUNDLED_REM_MERGE_MD: &str = include_str!("../prompts/rem-merge.md");

/// Verdict shape of the merge confirmer.
#[derive(Debug, serde::Deserialize)]
struct MergeDecision {
    merge: bool,
    #[serde(default)]
    survivor: String,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_merge_decision(raw: &str) -> Option<MergeDecision> {
    serde_json::from_value(first_json_object(raw)?).ok()
}

/// Whether two page slugs are name-kin: they share a long token
/// (`viaggi` / `viaggi_parigi_2026`) or a long common prefix
/// (`presenze` / `presenza`). A **nomination** heuristic only — the LLM
/// confirmer makes the semantic call; a resemblance is never sufficient.
fn slug_kinship(a: &str, b: &str) -> bool {
    let ta: std::collections::BTreeSet<&str> = a.split('_').filter(|t| t.len() >= 4).collect();
    let tb: std::collections::BTreeSet<&str> = b.split('_').filter(|t| t.len() >= 4).collect();
    if ta.intersection(&tb).next().is_some() {
        return true;
    }
    let common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    common >= 6
}

/// Nominate candidate pairs for the merge confirmer: the reviewer's
/// `duplicate_prose` pairs plus page-name kinship, restricted to fact-bearing
/// **concept leaves of the same family line** (a wiki plus its own
/// sub-wikis — `family` maps `wiki_id → family root`; a wiki absent from
/// the map, smart or vanished, never pairs), deduped, capped at `cap`
/// (the resource bound on confirmation calls). Returns
/// `(slug_a, slug_b, signal)`.
fn merge_candidates(
    plan: &CompilationPlan,
    duplicate_prose: &[(String, String, f32)],
    cap: usize,
    family: &BTreeMap<String, String>,
) -> Vec<(String, String, String)> {
    fn eligible<'p>(plan: &'p CompilationPlan, slug: &str) -> Option<&'p PagePlan> {
        plan.pages
            .get(slug)
            .filter(|p| p.page_type == PageType::ConceptLeaf && !p.primary_facts.is_empty())
    }
    let same_family = |a: &str, b: &str| match (family.get(a), family.get(b)) {
        (Some(fa), Some(fb)) => fa == fb,
        _ => false,
    };
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut consider = |a: &str, b: &str, signal: String| {
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        let (Some(pa), Some(pb)) = (eligible(plan, x), eligible(plan, y)) else {
            return;
        };
        if !same_family(&pa.wiki_id, &pb.wiki_id) {
            return;
        }
        if seen.insert((x.to_owned(), y.to_owned())) {
            out.push((x.to_owned(), y.to_owned(), signal));
        }
    };
    for (a, b, score) in duplicate_prose {
        consider(a, b, format!("duplicate prose, jaccard {score:.2}"));
    }
    let leaves: Vec<&PagePlan> = plan
        .pages
        .values()
        .filter(|p| p.page_type == PageType::ConceptLeaf && !p.primary_facts.is_empty())
        .collect();
    for (i, p) in leaves.iter().enumerate() {
        for q in leaves.iter().skip(i + 1) {
            if same_family(&p.wiki_id, &q.wiki_id) && slug_kinship(&p.slug, &q.slug) {
                consider(&p.slug, &q.slug, "page-name kinship".to_owned());
            }
        }
    }
    out.truncate(cap);
    out
}

/// Whether a page-merge receipt already covers this pair (either
/// orientation). An `applied` row means the merge is done or inside its
/// revert window; a `reverted` row is the **operator's standing veto** —
/// either way the pair is not re-judged.
async fn merge_already_judged(pool: &SqlitePool, page_a: &str, page_b: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM structure_proposals
          WHERE kind = 'wiki_promote'
            AND context LIKE '%page_merge%'
            AND context LIKE ? AND context LIKE ?",
    )
    .bind(format!("%{page_a}%"))
    .bind(format!("%{page_b}%"))
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// One page's block for the merge prompt: identity + numbered claims.
/// The `wiki:` line matters on a family-scope pair — the judge sees
/// where each page lives (parent wiki vs emergent sub-wiki).
fn describe_merge_page(p: &PagePlan) -> String {
    let facts = p
        .primary_facts
        .iter()
        .enumerate()
        .map(|(i, f)| format!("  {}. {}", i + 1, f.text.replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "wiki: {}\nslug: {}\ntitle: {}\ndescription: {}\nstyle: {}\nfacts:\n{facts}",
        p.wiki_id,
        p.slug,
        p.title,
        p.description,
        p.style.as_deref().unwrap_or("—"),
    )
}

fn merge_prompt(tree: &WikiTree, a: &PagePlan, b: &PagePlan, signal: &str) -> Result<String> {
    // On a family-scope pair the label names both wikis of the line.
    let scope_label = if a.wiki_id == b.wiki_id {
        a.wiki_id.clone()
    } else {
        format!("{} + {}", a.wiki_id, b.wiki_id)
    };
    Ok(prompts::render(
        "rem-merge",
        tree.workdir(),
        BUNDLED_REM_MERGE_MD,
        &[
            ("wiki_id", scope_label.as_str()),
            ("signal", signal),
            ("page_a", describe_merge_page(a).as_str()),
            ("page_b", describe_merge_page(b).as_str()),
        ],
    )?)
}

/// Page-merge sub-job — the **cure front** of semantic page consolidation
/// (rem-cycle.md §Page-merge sub-job).
///
/// Structural signals (the reviewer's `duplicate_prose` over the compiled
/// pages, page-name kinship in the persisted plan) **nominate**
/// concept-leaf pairs of the same **family line** (a wiki plus its own
/// sub-wikis — leva-2; a pair may straddle the parent↔sub-wiki boundary,
/// never an arbitrary wiki pair); a dedicated confirmer call (the
/// `rem_dedup_semantic` slot — the low binary-classifier confirmer tier,
/// the same slot the Conciliatore runs on at REM) **confirms**
/// "same concept?" and picks the survivor; the merge then executes
/// **act-first** via [`promote::apply_page_merge_direct`] — every husk fact
/// onto the survivor (re-homing its `wiki_id` when the pair crossed the
/// line), husk file deleted, persisted plan re-homed — with a
/// born-applied receipt (revert window) and a `structure_applied` notice
/// pointing at the dashboard. Capped by [`RemPolicy::page_merge_cap`]
/// confirmation calls per cycle; a pair with any prior page-merge receipt
/// (including a reverted one — the operator's veto) is never re-judged.
#[allow(
    clippy::too_many_lines,
    reason = "linear per-pair pipeline (nominate → confirm → execute); splitting hides the order, as in run_auto_promote"
)]
async fn run_page_merge(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<PageMergeReport> {
    let mut report = PageMergeReport::default();
    if policy.page_merge_cap == 0 {
        return Ok(report);
    }
    // Candidates come from the persisted plan + the compiled pages; before
    // the first compile there is nothing to consolidate.
    let plan = match crate::planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(report),
        Err(e) => {
            report.errors.push(format!("merge: plan load failed: {e}"));
            return Ok(report);
        },
    };
    // Only the duplicate-prose signal is consumed here; the default (empty)
    // identity context skips the reviewer's cross-subject check, which the
    // post-compile review in `dream::run_compile` already runs with the real
    // enrollment context.
    let duplicate_prose = match reviewer::review(tree, &plan, &reviewer::IdentityContext::default())
    {
        Ok(r) => r.duplicate_prose,
        Err(e) => {
            report
                .errors
                .push(format!("merge: reviewer signals unavailable: {e}"));
            Vec::new()
        },
    };
    let family = family_roots(&family_scopes(tree, smart_wiki_index)?);
    for (slug_a, slug_b, signal) in
        merge_candidates(&plan, &duplicate_prose, policy.page_merge_cap, &family)
    {
        // Both slugs come from `merge_candidates`, so the lookups hold.
        let (Some(pa), Some(pb)) = (plan.pages.get(&slug_a), plan.pages.get(&slug_b)) else {
            continue;
        };
        if merge_already_judged(pool, &pa.page_path, &pb.page_path).await? {
            report.skipped_judged += 1;
            continue;
        }
        let prompt = merge_prompt(tree, pa, pb, &signal)?;
        let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
        if rem_verdicts::is_settled(pool, rem_verdicts::kind::PAGE_MERGE, &memo_key).await? {
            continue;
        }
        report.candidates_examined += 1;
        let resp = llm
            .complete(
                CompletionRequest::new(prompt)
                    .with_temperature(0.1)
                    .with_max_tokens(200),
            )
            .await
            .map_err(|e| {
                RemError::Llm(format!("page merge failed on {slug_a} vs {slug_b}: {e}"))
            })?;
        let Some(verdict) = parse_merge_decision(&resp.text) else {
            report.errors.push(format!(
                "merge: unparseable verdict for {slug_a} vs {slug_b}",
            ));
            continue;
        };
        if !verdict.merge {
            rem_verdicts::record_negative(
                pool,
                rem_verdicts::kind::PAGE_MERGE,
                &memo_key,
                &format!("{slug_a} vs {slug_b}"),
            )
            .await?;
            continue;
        }
        let survivor_slug = crate::planner::slugify(&verdict.survivor);
        let (survivor, husk) = if survivor_slug == slug_a {
            (pa, pb)
        } else if survivor_slug == slug_b {
            (pb, pa)
        } else {
            report.errors.push(format!(
                "merge: confirmer named unknown survivor `{}` for {slug_a} vs {slug_b}",
                verdict.survivor,
            ));
            continue;
        };
        report.candidates_confirmed += 1;

        // The husk must be settled: every one of its `fact_index` rows
        // rendered on its compiled page. The move set is the DB's view
        // (every active row claiming the husk page), so the handler's
        // completeness guard holds by construction.
        let Ok(wiki_id) = WikiId::parse(&husk.wiki_id) else {
            report
                .errors
                .push(format!("merge: bad wiki id {}", husk.wiki_id));
            continue;
        };
        let Ok(handle) = tree.locate(&wiki_id) else {
            report
                .errors
                .push(format!("merge: wiki {} not found", husk.wiki_id));
            continue;
        };
        let husk_rel = wiki::workdir_relative_source_path(
            tree.workdir(),
            &handle.abs_dir().join(&husk.page_path),
        );
        let wiki_rows = fact_index::find_active_in_wiki(pool, husk.wiki_id.as_str()).await?;
        let husk_rows: Vec<&FactIndexRow> = wiki_rows
            .iter()
            .filter(|r| r.source_path == husk_rel)
            .collect();
        let on_page: HashSet<&str> = husk_rows.iter().map(|r| r.fact_id.as_str()).collect();
        let unsettled = husk_rows.is_empty()
            || husk
                .primary_facts
                .iter()
                .any(|f| !on_page.contains(f.fact_id.as_str()));
        if unsettled {
            report.skipped_unsettled += 1;
            continue;
        }
        let fact_ids: Vec<FactId> = husk_rows.iter().map(|r| r.fact_id.clone()).collect();

        let op_id = wal::begin_rem_op(
            pool,
            cycle_id,
            "page_merge_apply",
            Some(husk.wiki_id.as_str()),
            None,
        )
        .await?;
        let recipient =
            proposals::recipient_from_fact(&husk_rows[0].owner_id, husk_rows[0].sender_id.as_ref());
        let params = PageMergeParams {
            wiki_id: husk.wiki_id.as_str(),
            survivor_wiki_id: survivor.wiki_id.as_str(),
            husk_page: husk.page_path.as_str(),
            survivor_page: survivor.page_path.as_str(),
            fact_ids: &fact_ids,
            husk_title: husk.title.as_str(),
            husk_description: husk.description.as_str(),
            husk_style: husk.style.as_deref(),
            reason: Some(format!(
                "LLM confirmed same concept ({signal}): {}",
                verdict.reason.as_deref().unwrap_or("no reason given"),
            )),
        };
        match promote::apply_page_merge_direct(pool, tree, &params, recipient.clone()).await {
            Ok(receipt) => {
                wal::complete_rem_op(pool, op_id).await?;
                report.applied.push(receipt.proposal_id.clone());
                events::insert_event(
                    pool,
                    EventKind::StructureApplied,
                    Some(husk.wiki_id.as_str()),
                    Some(fact_ids[0].as_str()),
                    &json!({
                        "proposal_id": receipt.proposal_id,
                        "variant": "page_merge",
                        "source_page": husk.page_path,
                        "target_page": survivor.page_path,
                        "target_wiki_id": survivor.wiki_id,
                        "moved_facts": fact_ids.iter().map(FactId::as_str).collect::<Vec<_>>(),
                        "recipient_id": recipient,
                        "revert_deadline": receipt.revert_deadline.to_rfc3339(),
                        "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
                    }),
                )
                .await?;
            },
            Err(e) => {
                wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                report.errors.push(format!("merge apply failed: {e}"));
            },
        }
    }
    Ok(report)
}

// ---------- Completion sweep sub-job ----------

/// Bundled confirmation prompt for the completion sweep. Operator
/// override: `<workdir>/prompts/rem-completion.md`.
pub const BUNDLED_REM_COMPLETION_MD: &str = include_str!("../prompts/rem-completion.md");

/// The LLM confirmer's verdict for one evidence fact.
#[derive(Debug, serde::Deserialize)]
struct CompletionDecision {
    #[serde(default)]
    completions: Vec<CompletionItem>,
}

/// One confirmed completion inside a [`CompletionDecision`].
#[derive(Debug, serde::Deserialize)]
struct CompletionItem {
    target: String,
    #[serde(default)]
    valid_to: Option<String>,
}

/// One nominated evidence→candidates pairing, ready for the confirmer.
struct CompletionCase<'a> {
    evidence: &'a FactIndexRow,
    candidates: Vec<&'a FactIndexRow>,
}

/// Short single-line preview of a fact's claim for receipts and logs.
fn fact_preview(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    let mut out: String = one_line.chars().take(120).collect();
    if one_line.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// Nominate completion cases: fresh evidence facts (created inside
/// `policy.closure_sweep_window`) paired with the most similar
/// OPEN facts of the same scope — a family line, the wiki plus its own
/// sub-wikis — (embedding cosine, top 3, older than the evidence).
/// Newest evidence first, capped by `policy.completion_sweep_cap`;
/// evidence with no open candidate never reaches the LLM.
fn completion_cases<'a>(
    by_scope: &'a BTreeMap<String, Vec<FactIndexRow>>,
    now: DateTime<Utc>,
    policy: &RemPolicy,
) -> Vec<CompletionCase<'a>> {
    let since = now - policy.closure_sweep_window;
    let mut cases: Vec<CompletionCase<'a>> = Vec::new();
    for rows in by_scope.values() {
        for evidence in rows {
            // The reserved rules page sits outside the completion model on
            // BOTH axes: a standing directive is policy, not an event — it
            // completes nothing (the live incident: franz's "il tuo nome per
            // questo utente è Gandalf" read as evidence "completing"
            // morgana's parallel Ernest naming rule), and it is never
            // completed by neighbouring evidence — it leaves the channel
            // only via supersede, tombstone, or its owner's explicit
            // closure. Structural perimeter, like the dedup channel-boundary
            // — and a project signpost is fenced out the same way: it is a
            // pointer maintained by its channel, never evidence that
            // something else finished.
            if wiki::is_channel_page(&evidence.source_path) {
                continue;
            }
            let Ok(created) = DateTime::parse_from_rfc3339(&evidence.created_at) else {
                continue;
            };
            if created < since {
                continue;
            }
            let mut scored: Vec<(f32, &FactIndexRow)> = rows
                .iter()
                .filter(|c| {
                    c.fact_id != evidence.fact_id
                        && c.valid_to.is_none()
                        && c.created_at < evidence.created_at
                        && !wiki::is_channel_page(&c.source_path)
                })
                .map(|c| {
                    (
                        recall::cosine_similarity(&evidence.embedding, &c.embedding),
                        c,
                    )
                })
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let candidates: Vec<&FactIndexRow> =
                scored.into_iter().take(3).map(|(_, c)| c).collect();
            if candidates.is_empty() {
                continue;
            }
            cases.push(CompletionCase {
                evidence,
                candidates,
            });
        }
    }
    // Newest evidence first; the cap then favors what just happened.
    cases.sort_by(|a, b| b.evidence.created_at.cmp(&a.evidence.created_at));
    cases.truncate(policy.completion_sweep_cap);
    cases
}

/// The completion sweep — the REM safety net of the closure verb.
///
/// The ingest path closes the open items its recall window shows it
/// (see the ingest pipeline);
/// this sub-job catches the rest with the global view: each fresh
/// **evidence** fact is paired with the most similar open items of its
/// wiki (embedding similarity **nominates only** — a resource cap, not
/// a semantic gate), a dedicated LLM call decides what the evidence
/// actually completed, and the confirmed closures land **act-first**
/// with the same `validity_close` receipt + `structure_applied` notice
/// the ingest half emits — the dashboard stays the one undo surface.
async fn run_completion_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<CompletionSweepReport> {
    let mut report = CompletionSweepReport::default();
    if policy.completion_sweep_cap == 0 {
        return Ok(report);
    }
    // Family scope (leva-2): the bucket is the family line, so evidence
    // in the parent wiki can complete an open item in the sub-wiki and
    // vice versa — the pairing logic below is untouched.
    let mut by_family: BTreeMap<String, Vec<FactIndexRow>> = BTreeMap::new();
    for scope in family_scopes(tree, smart_wiki_index)? {
        let rows = find_active_in_family(pool, &scope).await?;
        if !rows.is_empty() {
            by_family.insert(scope.root_id.clone(), rows);
        }
    }
    let cases = completion_cases(&by_family, now, policy);
    // The candidate snapshot (`by_wiki`) is built once at the top of the
    // sweep, so two different evidence facts can both nominate the same
    // open item. Track what THIS cycle has already closed and drop those
    // candidates before the confirmer sees them — otherwise the same fact
    // is closed two or three times in one cycle, burning LLM calls and
    // (because `close_validity` has no re-close guard) corrupting the
    // revert snapshot of every receipt after the first.
    let mut closed_this_cycle: HashSet<String> = HashSet::new();
    for case in cases {
        let candidates: Vec<&FactIndexRow> = case
            .candidates
            .into_iter()
            .filter(|c| !closed_this_cycle.contains(c.fact_id.as_str()))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let case = CompletionCase {
            evidence: case.evidence,
            candidates,
        };
        report.evidence_examined += 1;
        report.candidates_judged += case.candidates.len();
        match judge_completion_case(pool, tree, llm, cycle_id, &case).await {
            Ok(Some((receipt_id, closed))) => {
                closed_this_cycle.extend(closed.iter().cloned());
                report.receipts.push(receipt_id);
                report.closed.extend(closed);
            },
            Ok(None) => {},
            Err(e) => report
                .errors
                .push(format!("completion {}: {e}", case.evidence.fact_id)),
        }
    }
    Ok(report)
}

/// Ask the confirmer about one evidence fact and apply what it
/// confirms. Returns the receipt id + closed fact ids, or `None` when
/// nothing closed (LLM down, unparseable answer, or an honest empty
/// verdict — all conservative no-ops).
#[expect(
    clippy::too_many_lines,
    reason = "linear per-evidence pipeline (prompt → confirm → close → receipt); splitting hides the order, as in run_page_merge"
)]
async fn judge_completion_case(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    case: &CompletionCase<'_>,
) -> Result<Option<(String, Vec<String>)>> {
    let candidates_text = case
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}. {} · {} · {}",
                i + 1,
                c.fact_id.as_str(),
                c.created_at,
                fact_preview(&c.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = prompts::render(
        "rem-completion",
        tree.workdir(),
        BUNDLED_REM_COMPLETION_MD,
        &[
            ("evidence_text", case.evidence.text.as_str()),
            ("evidence_date", case.evidence.created_at.as_str()),
            ("candidates", candidates_text.as_str()),
        ],
    )?;
    // Evidence stays inside the 48 h window for two cycles, so without a
    // memo the same evidence × same candidate set is judged twice.
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::COMPLETION, &memo_key).await? {
        return Ok(None);
    }
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(400),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rem completion: confirmer unavailable — skipped");
            return Ok(None);
        },
    };
    let Some(raw) = first_json_object(&resp.text) else {
        tracing::warn!("rem completion: unparseable confirmer answer — skipped");
        return Ok(None);
    };
    let decision: CompletionDecision = match serde_json::from_value(raw) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "rem completion: bad confirmer JSON shape — skipped");
            return Ok(None);
        },
    };
    if decision.completions.is_empty() {
        rem_verdicts::record_negative(
            pool,
            rem_verdicts::kind::COMPLETION,
            &memo_key,
            case.evidence.fact_id.as_str(),
        )
        .await?;
        return Ok(None);
    }

    let op_id = wal::begin_rem_op(
        pool,
        cycle_id,
        "completion_close_apply",
        Some(case.evidence.wiki_id.as_str()),
        None,
    )
    .await?;
    let mut applied: Vec<promote::AppliedClosure> = Vec::new();
    for item in &decision.completions {
        // Anti-hallucination: only ids from the candidate list close.
        let Some(target) = case
            .candidates
            .iter()
            .find(|c| c.fact_id.as_str() == item.target)
        else {
            tracing::warn!(
                target = item.target,
                "rem completion: confirmer named a non-candidate — skipped"
            );
            continue;
        };
        if applied.iter().any(|a| a.fact_id == target.fact_id) {
            continue;
        }
        // The completion instant: the confirmer's resolved date, else the
        // evidence's own capture instant (when we learned it happened).
        let valid_to = item
            .valid_to
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| case.evidence.created_at.clone(), str::to_owned);
        // The evidence fact IS the successor: it states the outcome the
        // closed fact was waiting for, so the page can point at its home.
        let Some(prev) = fact_index::close_validity(
            pool,
            &target.fact_id,
            &valid_to,
            fact_index::decay::COMPLETED,
            Some(&case.evidence.fact_id),
        )
        .await?
        else {
            continue; // vanished between gather and apply
        };
        tracing::info!(
            fact_id = %target.fact_id,
            evidence = %case.evidence.fact_id,
            valid_to,
            "rem completion: validity CLOSED (safety-net sweep)"
        );
        applied.push(promote::AppliedClosure {
            fact_id: target.fact_id.clone(),
            wiki_id: target.wiki_id.clone(),
            preview: fact_preview(&target.text),
            valid_to,
            reason: fact_index::decay::COMPLETED.to_owned(),
            prev,
            surface: promote::ClosureSurface::Fact,
        });
    }
    if applied.is_empty() {
        wal::complete_rem_op(pool, op_id).await?;
        return Ok(None);
    }

    // The same act-first paper trail as the ingest half: one receipt per
    // evidence fact + the dashboard notice.
    let recipient =
        proposals::recipient_from_fact(&applied_owner(case, &applied), sender_of(case, &applied));
    let gesture = format!(
        "REM completion sweep — evidence: {}",
        fact_preview(&case.evidence.text)
    );
    let receipt = match promote::emit_validity_close_receipt(
        pool,
        &applied,
        Some(&gesture),
        None,
        recipient.clone(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
            return Err(RemError::Proposals(match e {
                promote::DirectPromoteError::Receipt(p) => p,
                promote::DirectPromoteError::Apply(a) => {
                    ProposalsError::Db(sqlx::Error::Protocol(a.to_string()))
                },
            }));
        },
    };
    wal::complete_rem_op(pool, op_id).await?;
    events::insert_event(
        pool,
        EventKind::StructureApplied,
        Some(applied[0].wiki_id.as_str()),
        Some(applied[0].fact_id.as_str()),
        &json!({
            "proposal_id": receipt.proposal_id,
            "variant": "validity_close",
            "closed_facts": applied.iter().map(|c| c.fact_id.as_str()).collect::<Vec<_>>(),
            "recipient_id": recipient,
            "revert_deadline": receipt.revert_deadline.to_rfc3339(),
            "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
        }),
    )
    .await?;
    let closed = applied
        .iter()
        .map(|c| c.fact_id.as_str().to_owned())
        .collect();
    Ok(Some((receipt.proposal_id, closed)))
}

/// Owner principal of the first closed target (the receipt addressee
/// follows the closed fact, as everywhere else).
fn applied_owner(
    case: &CompletionCase<'_>,
    applied: &[promote::AppliedClosure],
) -> crate::types::Principal {
    case.candidates
        .iter()
        .find(|c| c.fact_id == applied[0].fact_id)
        .map_or_else(|| case.evidence.owner_id.clone(), |c| c.owner_id.clone())
}

/// Sender attribution of the first closed target, for the addressee.
fn sender_of<'a>(
    case: &'a CompletionCase<'_>,
    applied: &[promote::AppliedClosure],
) -> Option<&'a crate::types::Principal> {
    case.candidates
        .iter()
        .find(|c| c.fact_id == applied[0].fact_id)
        .and_then(|c| c.sender_id.as_ref())
}

// ---------- Cross-wiki refile sweep sub-job ----------

/// Bundled judgment prompt for the cross-wiki refile sweep. Operator
/// override: `<workdir>/prompts/rem-refile.md`.
pub const BUNDLED_REM_REFILE_MD: &str = include_str!("../prompts/rem-refile.md");

/// How much closer-to-foreign-than-home a fact must embed before the
/// cosine pre-filter nominates it. A pure **resource** margin (skip the
/// LLM on facts that sit at least as close to home as to anything
/// foreign), NOT a semantic "belongs elsewhere" gate — the LLM still
/// makes the verdict ([[feedback-no-hardcoded-gates-llm-decides]]). The
/// margin keeps a fact home unless a foreign wiki is materially more
/// similar, so a fact merely adjacent to two subjects is never nominated.
const REFILE_COSINE_MARGIN: f32 = 0.05;

/// Per-wiki view used to score + present refile candidates: the
/// discovered wiki plus its active facts (the home pool to beat).
struct RefileWikiView<'a> {
    d: &'a wiki::DiscoveredWiki,
    facts: Vec<FactIndexRow>,
}

/// One nominated refile: the candidate fact, its home wiki view, and the
/// foreign wikis it embeds materially closer to than home (newest-first
/// ordering is applied across nominations, not here).
struct RefileCase<'a> {
    fact: &'a FactIndexRow,
    home: &'a RefileWikiView<'a>,
    /// Foreign wiki ids the fact embeds closer to than home, best first.
    foreign: Vec<&'a RefileWikiView<'a>>,
}

/// The LLM's verdict for one refile candidate (shared by the refile
/// sweep and the recall-repair proposal — same closed shape).
#[derive(Debug, Default, serde::Deserialize)]
struct RefileDecision {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    dest_wiki_id: Option<String>,
    // The judge picks only the destination WIKI; the fact always lands on
    // that wiki's foundation `index.md` (collision-safe — see the apply
    // site), so no per-page field is read. A `dest_page` in the model's
    // JSON is ignored by serde.
    #[serde(default)]
    reason: Option<String>,
}

/// Max cosine of `fact` to any *other* active fact of `view` (the fact's
/// own home-similarity floor to beat, or a wiki's foreign-similarity
/// ceiling). Empty pool ⇒ `f32::MIN` so a wiki with no facts never wins.
fn best_cosine_to_wiki(fact: &FactIndexRow, view: &RefileWikiView<'_>) -> f32 {
    view.facts
        .iter()
        .filter(|c| c.fact_id != fact.fact_id)
        .map(|c| recall::cosine_similarity(&fact.embedding, &c.embedding))
        .fold(f32::MIN, f32::max)
}

/// Nominate refile candidates with the deterministic cosine pre-filter
/// (nominate only — a resource cap, NOT a gate). For each active fact of
/// each non-smart wiki, compare its best similarity to its HOME wiki's
/// other facts against its best similarity to facts in OTHER non-smart
/// wikis; nominate when a foreign wiki beats home by at least
/// [`REFILE_COSINE_MARGIN`]. Facts created inside `policy.closure_sweep_window`
/// are preferred (newest-first), and the result is truncated to
/// `policy.refile_sweep_cap`.
fn refile_cases<'a>(
    views: &'a [RefileWikiView<'a>],
    now: DateTime<Utc>,
    policy: &RemPolicy,
) -> Vec<RefileCase<'a>> {
    let since = now - policy.closure_sweep_window;
    let mut cases: Vec<(bool, &str, RefileCase<'a>)> = Vec::new();
    for home in views {
        for fact in &home.facts {
            // The reserved policy page is the rules pipeline's perimeter, not
            // the refile's: a per-user behaviour rule embeds close to its
            // *user's* wiki by nature (it names how the agent behaves with
            // them), so it is a natural false nominee — and a confirmed move
            // would eject it from the behaviour-rules channel, which reads
            // `rules.md` in the agent's own wiki (the refile twin of the
            // compiler-door skip in `planner::gather_standard_facts`), and the
            // signposts page is fenced the same way. Channel facts still count
            // in the similarity pools above/below; they are only never
            // nominated as the fact to move.
            if wiki::is_channel_page(&fact.source_path) {
                continue;
            }
            let foreign = ranked_foreign(views, home, fact, true);
            if foreign.is_empty() {
                continue;
            }
            let fresh = DateTime::parse_from_rfc3339(&fact.created_at).is_ok_and(|c| c >= since);
            cases.push((
                fresh,
                fact.created_at.as_str(),
                RefileCase {
                    fact,
                    home,
                    foreign,
                },
            ));
        }
    }
    // Fresh candidates first, then newest-first within each band; the cap
    // then favours what just landed.
    cases.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    cases.truncate(policy.refile_sweep_cap);
    cases.into_iter().map(|(_, _, c)| c).collect()
}

/// The foreign wikis ranked by best cosine to `fact`, best first. With
/// `margin` the cosine pre-filter applies (a foreign wiki must beat home
/// by [`REFILE_COSINE_MARGIN`] — the self-nomination valve); without it
/// every foreign wiki ranks (the reviewer-fed bridge already nominated
/// the fact, so the pre-filter has nothing left to decide).
fn ranked_foreign<'a>(
    views: &'a [RefileWikiView<'a>],
    home: &RefileWikiView<'a>,
    fact: &FactIndexRow,
    margin: bool,
) -> Vec<&'a RefileWikiView<'a>> {
    let home_best = best_cosine_to_wiki(fact, home);
    let mut foreign: Vec<(f32, &RefileWikiView<'a>)> = views
        .iter()
        .filter(|v| v.d.meta.wiki_id != home.d.meta.wiki_id)
        .map(|v| (best_cosine_to_wiki(fact, v), v))
        .filter(|(score, _)| !margin || *score >= home_best + REFILE_COSINE_MARGIN)
        .collect();
    foreign.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    foreign.into_iter().map(|(_, v)| v).collect()
}

/// One short presentation line for a wiki in the prompt
/// (`wiki_id · title — summary`).
fn refile_wiki_line(view: &RefileWikiView<'_>) -> String {
    let summary = view
        .d
        .meta
        .extra
        .get("summary")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or("");
    format!(
        "{} · {} — {}",
        view.d.meta.wiki_id.as_str(),
        view.d.meta.title,
        summary,
    )
}

/// The cross-wiki refile sweep — the LLM-decided refile of a single
/// misfiled fact into a different existing wiki.
///
/// A deterministic cosine pre-filter nominates facts that embed
/// materially closer to a foreign wiki than to home (a **resource** cap —
/// it only nominates, never decides
/// [[feedback-no-hardcoded-gates-llm-decides]]); the revisor LLM
/// (`llms.revisor` — the low binary-classifier confirmer tier)
/// decides whether (and where) each really belongs. A confirmed move
/// lands **act-first** via [`promote::apply_fact_refile_direct`] with the
/// same born-applied receipt + `structure_applied` notice the other REM
/// act-first sub-jobs emit — the dashboard is the one undo surface. Smart
/// wikis are skipped as **both** source and destination: the smart-family
/// is the consumer's, and refiling into/out of it would corrupt the
/// ownership boundary (smart rows carry projected wiki-level ACL).
async fn run_refile_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<RefileSweepReport> {
    let mut report = RefileSweepReport::default();
    if policy.refile_sweep_cap == 0 {
        return Ok(report);
    }
    // Build the per-wiki views once: every NON-smart wiki + its active
    // facts. Smart wikis never appear as a home (skip source) nor as a
    // foreign candidate (skip dest).
    let mut views: Vec<RefileWikiView<'_>> = Vec::new();
    let discovered = tree.walk()?;
    for d in &discovered {
        if is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        let facts = fact_index::find_active_in_wiki(pool, d.meta.wiki_id.as_str()).await?;
        views.push(RefileWikiView { d, facts });
    }
    if views.len() < 2 {
        return Ok(report); // nothing to refile between
    }

    // The reviewer→refile bridge: last night's `cross_subject_bloat`
    // nominations, drained from the plan (one judge pass each — the
    // review re-parks whatever still stands next compile, so a drained
    // nomination the cap squeezed out converges anyway). They skip the
    // cosine margin — the reviewer already nominated them — but the
    // judge still decides, and refuses what does not apply. A parked id
    // that vanished or re-homed since is silently done.
    let parked = match crate::planner::take_refile_candidates(tree) {
        Ok(p) => p,
        Err(e) => {
            report
                .errors
                .push(format!("refile: parked candidates load failed: {e}"));
            Vec::new()
        },
    };
    let mut cases: Vec<RefileCase<'_>> = Vec::new();
    let mut seeded: HashSet<String> = HashSet::new();
    for fid in &parked {
        let Some((home, fact)) = views.iter().find_map(|v| {
            v.facts
                .iter()
                .find(|f| f.fact_id.as_str() == fid)
                .map(|f| (v, f))
        }) else {
            continue;
        };
        if wiki::is_channel_page(&fact.source_path) {
            continue;
        }
        let foreign = ranked_foreign(&views, home, fact, false);
        if foreign.is_empty() {
            continue;
        }
        seeded.insert(fact.fact_id.as_str().to_owned());
        cases.push(RefileCase {
            fact,
            home,
            foreign,
        });
    }
    report.bridge_candidates = cases.len();
    for case in refile_cases(&views, now, policy) {
        if !seeded.contains(case.fact.fact_id.as_str()) {
            cases.push(case);
        }
    }
    cases.truncate(policy.refile_sweep_cap);
    for case in cases {
        report.candidates_examined += 1;
        match judge_refile_case(pool, tree, llm, cycle_id, &case).await {
            Ok(Some((receipt_id, fact_id))) => {
                report.candidates_judged += 1;
                report.receipts.push(receipt_id);
                report.refiled.push(fact_id);
            },
            Ok(None) => report.candidates_judged += 1,
            Err(e) => report
                .errors
                .push(format!("refile {}: {e}", case.fact.fact_id)),
        }
    }
    Ok(report)
}

/// Ask the revisor confirmer whether one candidate fact belongs in a
/// different wiki and, on a confident verdict naming a candidate foreign
/// wiki + page, apply the cross-wiki move act-first. Returns the receipt
/// id + moved fact id, or `None` for a conservative no-op (LLM down,
/// unparseable answer, a "stay" verdict, or a verdict naming a
/// non-candidate dest).
#[expect(
    clippy::too_many_lines,
    reason = "linear per-candidate pipeline (prompt → judge → move → receipt); splitting hides the order, as in judge_completion_case"
)]
async fn judge_refile_case(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    case: &RefileCase<'_>,
) -> Result<Option<(String, String)>> {
    let candidates_text = case
        .foreign
        .iter()
        .map(|v| refile_wiki_line(v))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = prompts::render(
        "rem-refile",
        tree.workdir(),
        BUNDLED_REM_REFILE_MD,
        &[
            ("fact_text", case.fact.text.as_str()),
            ("home_wiki", refile_wiki_line(case.home).as_str()),
            ("candidates", candidates_text.as_str()),
        ],
    )?;
    // A fact that "stays" is re-nominated by the cosine pre-filter every
    // night until its neighbourhood changes — and its neighbourhood is
    // exactly what the prompt (and so the key) is made of.
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::REFILE, &memo_key).await? {
        return Ok(None);
    }
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(300),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rem refile: confirmer unavailable — skipped");
            return Ok(None);
        },
    };
    let Some(raw) = first_json_object(&resp.text) else {
        tracing::warn!("rem refile: unparseable confirmer answer — skipped");
        return Ok(None);
    };
    let decision: RefileDecision = match serde_json::from_value(raw) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "rem refile: bad confirmer JSON shape — skipped");
            return Ok(None);
        },
    };
    if !decision.verdict.eq_ignore_ascii_case("move") {
        rem_verdicts::record_negative(
            pool,
            rem_verdicts::kind::REFILE,
            &memo_key,
            case.fact.fact_id.as_str(),
        )
        .await?;
        return Ok(None);
    }
    let Some(dest_wiki_id) = decision
        .dest_wiki_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    // Anti-hallucination: the dest MUST be one of the candidate foreign
    // wikis (never the home wiki, never an invented id).
    let Some(dest_view) = case
        .foreign
        .iter()
        .find(|v| v.d.meta.wiki_id.as_str() == dest_wiki_id)
    else {
        tracing::warn!(
            dest = dest_wiki_id,
            "rem refile: confirmer named a non-candidate dest — skipped"
        );
        return Ok(None);
    };
    // Destination page: ALWAYS the dest wiki's foundation `index.md`. The
    // compilation plan keys pages by a bare slug across the whole forest,
    // so landing a fact on a NAMED page of a foreign wiki can collide with
    // a same-slug page already homed in another wiki — the rehome would
    // attach the fact to the WRONG wiki's page and the next compile would
    // strand `wiki_id != source_path` (a cross-wiki leak). `index.md` is
    // keyed by the dest wiki's own id-slug, so it is collision-safe: the
    // fact crosses into the right wiki and that wiki's own dream
    // (auto_promote / page_merge) re-files it onto the right page. Finer
    // cross-wiki page placement waits on a wiki-qualified plan keyspace.
    let dest_page = "index.md";
    // Source page wiki-relative (the apply joins it onto the source wiki's
    // abs_dir, so a workdir-relative path would double the prefix).
    let Some(source_page) = wiki_relative_page(case.home.d, &case.fact.source_path) else {
        tracing::warn!(
            fact_id = %case.fact.fact_id,
            source_path = case.fact.source_path,
            "rem refile: fact source_path not under its home wiki — skipped"
        );
        return Ok(None);
    };

    let op_id = wal::begin_rem_op(
        pool,
        cycle_id,
        "fact_refile_apply",
        Some(case.home.d.meta.wiki_id.as_str()),
        None,
    )
    .await?;

    let recipient =
        proposals::recipient_from_fact(&case.fact.owner_id, case.fact.sender_id.as_ref());
    let reason = decision
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let applied = match promote::apply_fact_refile_direct(
        pool,
        tree,
        &case.fact.fact_id,
        case.home.d.meta.wiki_id.as_str(),
        &source_page,
        dest_view.d.meta.wiki_id.as_str(),
        dest_page,
        reason,
        recipient.clone(),
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
            return Err(RemError::Proposals(match e {
                promote::DirectPromoteError::Receipt(p) => p,
                promote::DirectPromoteError::Apply(a) => {
                    ProposalsError::Db(sqlx::Error::Protocol(a.to_string()))
                },
            }));
        },
    };
    wal::complete_rem_op(pool, op_id).await?;

    tracing::info!(
        fact_id = %case.fact.fact_id,
        source_wiki = case.home.d.meta.wiki_id.as_str(),
        dest_wiki = dest_view.d.meta.wiki_id.as_str(),
        dest_page,
        "rem refile: fact MOVED cross-wiki (act-first)"
    );

    events::insert_event(
        pool,
        EventKind::StructureApplied,
        Some(dest_view.d.meta.wiki_id.as_str()),
        Some(case.fact.fact_id.as_str()),
        &json!({
            "proposal_id": applied.proposal_id,
            "variant": "fact_refile",
            "fact_id": case.fact.fact_id.as_str(),
            "source_wiki_id": case.home.d.meta.wiki_id.as_str(),
            "dest_wiki_id": dest_view.d.meta.wiki_id.as_str(),
            "dest_page": dest_page,
            "recipient_id": recipient,
            "revert_deadline": applied.revert_deadline.to_rfc3339(),
            "dashboard_path": receipt_dashboard_path(&applied.proposal_id),
        }),
    )
    .await?;

    Ok(Some((
        applied.proposal_id,
        case.fact.fact_id.as_str().to_owned(),
    )))
}

// ---------- Contradiction sweep sub-job ----------

/// Bundled confirmation prompt for the contradiction sweep. Operator
/// override: `<workdir>/prompts/rem-contradiction.md`.
pub const BUNDLED_REM_CONTRADICTION_MD: &str = include_str!("../prompts/rem-contradiction.md");

/// The LLM confirmer's verdict for one contradicted seed.
#[derive(Debug, serde::Deserialize)]
struct ContradictionDecision {
    #[serde(default)]
    invalidated: Vec<CompletionItem>,
}

/// The contradiction sweep — the cluster half of the validity model.
///
/// A contradiction lands on one fact (the supersede chokepoint, or an
/// ingest `contradicted` closure) while its **satellites** stay wrongly
/// open — the dogfood's cancelled trip whose itinerary days kept feeding
/// the due-soon slot. The ingest path closes the satellites its recall
/// window shows it; this sub-job follows the cluster with the global
/// view: each freshly contradicted **seed** (window-bounded) is paired
/// with its most similar open neighbours (embedding **nominates only**),
/// the [`rem-contradiction`](../../crates/mwe-core/prompts/rem-contradiction.md)
/// confirmer — shown the successor statement when one exists — decides
/// which candidates fall with it, and the confirmed closures land
/// act-first with the same `validity_close` receipt + notice. The
/// cluster definition is the LLM's judgment, never a hardcoded gate.
async fn run_contradiction_sweep(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<ContradictionSweepReport> {
    let mut report = ContradictionSweepReport::default();
    if policy.contradiction_sweep_cap == 0 {
        return Ok(report);
    }
    let since = (now - policy.closure_sweep_window).to_rfc3339();
    let mut cases: Vec<(FactIndexRow, Vec<FactIndexRow>)> = Vec::new();
    // Family scope (leva-2): seeds and open neighbours pool over the
    // family line, so a contradiction landing in the parent wiki can
    // fell its satellites in the sub-wiki and vice versa.
    for scope in family_scopes(tree, smart_wiki_index)? {
        let mut seeds = Vec::new();
        for wiki in &scope.wiki_ids {
            seeds.extend(fact_index::find_recently_contradicted(pool, wiki, &since).await?);
        }
        if seeds.is_empty() {
            continue;
        }
        let open_rows: Vec<FactIndexRow> = find_active_in_family(pool, &scope)
            .await?
            .into_iter()
            .filter(|r| r.valid_to.is_none())
            .collect();
        for seed in seeds {
            // Candidate-pool hygiene — structural perimeter, never a
            // semantic gate (the cluster judgment stays the LLM's):
            //
            // 1. The seed's whole successor LINEAGE is off-limits, not just
            //    its direct successor: a fact revised twice is otherwise
            //    nominatable as a "satellite" of its own grandparent, and
            //    the sweep would cannibalise the very revision that
            //    contradicted the seed (observed live 2026-07-01: the
            //    freshly revised TTS rules fell as satellites of their own
            //    dead predecessors).
            // 2. The reserved channel pages are channel-governed: a standing
            //    directive — or a project signpost — leaves its page only via
            //    supersede, tombstone, or its owner's explicit closure, never
            //    as collateral of a neighbouring contradiction. Same fence the
            //    dedup/refile sweeps already honour
            //    ([`crate::wiki::is_channel_page`]).
            // 3. An identity-core fact (a role / relationship — `bio` +
            //    `salience=high`) is sticky: it changes only on the owner's
            //    explicit correction (the classifier supersede path), never
            //    as collateral of a background contradiction judgment. The
            //    same perimeter the dedup revisor honours (leva 3), so who a
            //    person is to another is never rewritten by the background.
            let lineage = successor_lineage(pool, &seed).await?;
            let mut scored: Vec<(f32, &FactIndexRow)> = open_rows
                .iter()
                .filter(|c| {
                    c.fact_id != seed.fact_id
                        && !lineage.contains(&c.fact_id)
                        && !wiki::is_channel_page(&c.source_path)
                        && !c.is_identity_core()
                })
                .map(|c| (recall::cosine_similarity(&seed.embedding, &c.embedding), c))
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let candidates: Vec<FactIndexRow> =
                scored.into_iter().take(5).map(|(_, c)| c.clone()).collect();
            if candidates.is_empty() {
                continue;
            }
            cases.push((seed, candidates));
        }
    }
    // Freshest contradictions first; the cap favors what just fell.
    cases.sort_by(|a, b| b.0.updated_at.cmp(&a.0.updated_at));
    cases.truncate(policy.contradiction_sweep_cap);

    for (seed, candidates) in cases {
        report.seeds_examined += 1;
        report.candidates_judged += candidates.len();
        match judge_contradiction_case(pool, tree, llm, cycle_id, &seed, &candidates).await {
            Ok(Some((receipt_id, closed))) => {
                report.receipts.push(receipt_id);
                report.closed.extend(closed);
            },
            Ok(None) => {},
            Err(e) => report
                .errors
                .push(format!("contradiction {}: {e}", seed.fact_id)),
        }
    }
    Ok(report)
}

/// The seed's successor lineage: the chain of facts that replaced it —
/// `superseded_by` walked transitively (cycle-safe, bounded). The live
/// head of a revised-twice fact is this chain's tail; none of it is ever
/// a satellite candidate of its own ancestor.
async fn successor_lineage(pool: &SqlitePool, seed: &FactIndexRow) -> Result<Vec<FactId>> {
    const LINEAGE_WALK_CAP: usize = 32;
    let mut lineage: Vec<FactId> = Vec::new();
    let mut cursor = seed.superseded_by.clone();
    while let Some(id) = cursor {
        if lineage.contains(&id) || lineage.len() >= LINEAGE_WALK_CAP {
            break; // cycle guard / runaway bound
        }
        cursor = fact_index::find_by_id(pool, &id)
            .await?
            .and_then(|r| r.superseded_by);
        lineage.push(id);
    }
    Ok(lineage)
}

/// Ask the confirmer about one contradicted seed and close what it
/// confirms — the same conservative no-op semantics as the completion
/// sweep's judge.
#[expect(
    clippy::too_many_lines,
    reason = "linear per-seed pipeline (prompt → confirm → close → receipt); splitting hides the order, as in run_page_merge"
)]
async fn judge_contradiction_case(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    seed: &FactIndexRow,
    candidates: &[FactIndexRow],
) -> Result<Option<(String, Vec<String>)>> {
    let successor_text = match &seed.superseded_by {
        Some(succ) => fact_index::find_by_id(pool, succ)
            .await?
            .map_or_else(|| "(none)".to_owned(), |r| r.text),
        None => "(none)".to_owned(),
    };
    let candidates_text = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}. {} · {} · {}",
                i + 1,
                c.fact_id.as_str(),
                c.created_at,
                fact_preview(&c.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = prompts::render(
        "rem-contradiction",
        tree.workdir(),
        BUNDLED_REM_CONTRADICTION_MD,
        &[
            ("contradicted_text", seed.text.as_str()),
            ("successor_text", successor_text.as_str()),
            ("candidates", candidates_text.as_str()),
        ],
    )?;
    // Same 48 h window as the completion sweep: a seed is re-judged
    // against the same satellites on the next cycle unless it is settled.
    let memo_key = rem_verdicts::key(llm.model_id(), &prompt);
    if rem_verdicts::is_settled(pool, rem_verdicts::kind::CONTRADICTION, &memo_key).await? {
        return Ok(None);
    }
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(400),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "rem contradiction: confirmer unavailable — skipped");
            return Ok(None);
        },
    };
    let parsed = first_json_object(&resp.text)
        .and_then(|v| serde_json::from_value::<ContradictionDecision>(v).ok());
    let Some(decision) = parsed else {
        tracing::warn!("rem contradiction: unparseable confirmer answer — skipped");
        return Ok(None);
    };
    if decision.invalidated.is_empty() {
        rem_verdicts::record_negative(
            pool,
            rem_verdicts::kind::CONTRADICTION,
            &memo_key,
            seed.fact_id.as_str(),
        )
        .await?;
        return Ok(None);
    }

    let op_id = wal::begin_rem_op(
        pool,
        cycle_id,
        "contradiction_close_apply",
        Some(seed.wiki_id.as_str()),
        None,
    )
    .await?;
    // The invalidation instant: the seed's own closure time when it has
    // one, else tonight — the satellites fall when the event fell.
    let seed_closed_at = seed
        .valid_to
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let mut applied: Vec<promote::AppliedClosure> = Vec::new();
    for item in &decision.invalidated {
        let Some(target) = candidates
            .iter()
            .find(|c| c.fact_id.as_str() == item.target)
        else {
            tracing::warn!(
                target = item.target,
                "rem contradiction: confirmer named a non-candidate — skipped"
            );
            continue;
        };
        if applied.iter().any(|a| a.fact_id == target.fact_id) {
            continue;
        }
        let valid_to = item
            .valid_to
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| seed_closed_at.clone(), str::to_owned);
        // A satellite falls with the seed, so it inherits the seed's
        // superseding fact as its successor (None when the seed was closed
        // without one — the pointer stays empty rather than guessing).
        let Some(prev) = fact_index::close_validity(
            pool,
            &target.fact_id,
            &valid_to,
            fact_index::decay::CONTRADICTED,
            seed.superseded_by.as_ref(),
        )
        .await?
        else {
            continue;
        };
        tracing::info!(
            fact_id = %target.fact_id,
            seed = %seed.fact_id,
            valid_to,
            "rem contradiction: satellite CLOSED (cluster sweep)"
        );
        applied.push(promote::AppliedClosure {
            fact_id: target.fact_id.clone(),
            wiki_id: target.wiki_id.clone(),
            preview: fact_preview(&target.text),
            valid_to,
            reason: fact_index::decay::CONTRADICTED.to_owned(),
            prev,
            surface: promote::ClosureSurface::Fact,
        });
    }
    if applied.is_empty() {
        wal::complete_rem_op(pool, op_id).await?;
        return Ok(None);
    }

    let first = candidates
        .iter()
        .find(|c| c.fact_id == applied[0].fact_id)
        .unwrap_or(seed);
    let recipient = proposals::recipient_from_fact(&first.owner_id, first.sender_id.as_ref());
    let gesture = format!(
        "REM contradiction sweep — fell with: {}",
        fact_preview(&seed.text)
    );
    let receipt = match promote::emit_validity_close_receipt(
        pool,
        &applied,
        Some(&gesture),
        None,
        recipient.clone(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
            return Err(RemError::Proposals(match e {
                promote::DirectPromoteError::Receipt(p) => p,
                promote::DirectPromoteError::Apply(a) => {
                    ProposalsError::Db(sqlx::Error::Protocol(a.to_string()))
                },
            }));
        },
    };
    wal::complete_rem_op(pool, op_id).await?;
    events::insert_event(
        pool,
        EventKind::StructureApplied,
        Some(applied[0].wiki_id.as_str()),
        Some(applied[0].fact_id.as_str()),
        &json!({
            "proposal_id": receipt.proposal_id,
            "variant": "validity_close",
            "closed_facts": applied.iter().map(|c| c.fact_id.as_str()).collect::<Vec<_>>(),
            "recipient_id": recipient,
            "revert_deadline": receipt.revert_deadline.to_rfc3339(),
            "dashboard_path": receipt_dashboard_path(&receipt.proposal_id),
        }),
    )
    .await?;
    let closed = applied
        .iter()
        .map(|c| c.fact_id.as_str().to_owned())
        .collect();
    Ok(Some((receipt.proposal_id, closed)))
}

// ---------- Recall-repair sub-job (self-correcting REM) ----------

/// Bundled proposal prompt for the recall-repair sub-job. Operator
/// override: `<workdir>/prompts/rem-recall-repair.md`.
pub const BUNDLED_REM_RECALL_REPAIR_MD: &str = include_str!("../prompts/rem-recall-repair.md");

/// Sub-report for the recall-repair sub-job.
#[derive(Debug, Default, Clone)]
pub struct RecallRepairReport {
    /// Pending misses examined this cycle.
    pub misses_examined: usize,
    /// Re-files that passed the gold-set gate and committed for real.
    pub repairs_committed: usize,
    /// Candidate repairs the gate refused (no flip, or a gold regression).
    pub gate_rejected: usize,
    /// Misses whose target already surfaces again (corpus healed itself)
    /// or whose fact is gone.
    pub stale: usize,
    /// Recurrence notices queued for the operator
    /// (`recall_tuning_proposed`).
    pub queued: usize,
    /// Misses the confirmer judged to have no local filing repair.
    pub no_repair: usize,
    /// Candidate gold-set cases appended
    /// ([`recall_gate::append_gold_candidate`]).
    pub gold_candidates_appended: usize,
    /// Receipt ids of committed repairs.
    pub receipts: Vec<String>,
    /// Per-miss soft errors; the sub-job continues.
    pub errors: Vec<String>,
}

/// The repair stage of self-correcting REM: judge each pending
/// [`recall_log`] miss, propose the lowest-blast-radius repair (a
/// cross-wiki re-file, the same act-first mover as the refile sweep),
/// and commit it **only through the gold-set gate**
/// ([`recall_gate::gate_repair`]) — a repair that cannot prove it made
/// the missed query reachable without regressing the gold set does not
/// commit. A miss with no provable local repair either discards or — on
/// recurrence — queues a `recall_tuning_proposed` operator notice
/// (rule/prompt/knob levers are never auto-applied). Every processed
/// miss also feeds the gold set's candidates file (the 15f loop).
#[allow(
    clippy::too_many_arguments,
    reason = "cycle plumbing, as its sibling sub-jobs"
)]
async fn run_recall_repair(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    navigator: Option<&dyn LlmBackend>,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<RecallRepairReport> {
    let mut report = RecallRepairReport::default();
    if policy.recall_repair_cap == 0 {
        return Ok(report);
    }
    let misses = recall_log::pending_misses(pool, policy.recall_repair_cap).await?;
    if misses.is_empty() {
        return Ok(report);
    }
    // The operator's gold set is the judge — a malformed file must not
    // silently demote every repair to target-only gating: skip the whole
    // sub-job loudly instead.
    let gold = match recall_gate::load_gold_set(tree.workdir()) {
        Ok(g) => g,
        Err(e) => {
            report.errors.push(format!(
                "recall_repair: gold set unreadable, sub-job skipped: {e}"
            ));
            return Ok(report);
        },
    };
    let discovered = tree.walk()?;
    // One recurrence notice per fact per cycle.
    let mut noticed: HashSet<String> = HashSet::new();

    for miss in misses {
        report.misses_examined += 1;
        if let Err(e) = repair_one_miss(
            pool,
            tree,
            embedder,
            llm,
            navigator,
            cycle_id,
            now,
            policy,
            smart_wiki_index,
            &discovered,
            &gold,
            &miss,
            &mut noticed,
            &mut report,
        )
        .await
        {
            report
                .errors
                .push(format!("recall_repair {}: {e}", miss.fact_id));
        }
    }
    Ok(report)
}

/// One miss through detect-was-it-real → propose → gate → commit/queue.
/// Soft errors bubble as strings to the caller's report; the miss keeps
/// its `new` status on a transient error and is retried next cycle.
#[allow(
    clippy::too_many_arguments,
    reason = "per-miss pipeline over the cycle's shared context"
)]
#[expect(
    clippy::too_many_lines,
    reason = "linear per-miss pipeline (lookup → candidates → propose → gate → commit); splitting hides the order, as in judge_refile_case"
)]
async fn repair_one_miss(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    llm: &dyn LlmBackend,
    navigator: Option<&dyn LlmBackend>,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
    discovered: &[wiki::DiscoveredWiki],
    gold: &crate::recall_eval::GoldSet,
    miss: &crate::recall_log::MissRow,
    noticed: &mut HashSet<String>,
    report: &mut RecallRepairReport,
) -> std::result::Result<(), String> {
    let soft = |e: &dyn std::fmt::Display| e.to_string();

    // The fact may have moved on since the miss.
    let fact_id = FactId::parse(&miss.fact_id).map_err(|e| soft(&e))?;
    let Some(fact) = fact_index::find_by_id(pool, &fact_id)
        .await
        .map_err(|e| soft(&e))?
        .filter(|f| f.superseded_at.is_none() && f.deleted_at.is_none())
    else {
        recall_log::set_miss_status(pool, miss.miss_id, "stale", Some("fact_gone"))
            .await
            .map_err(|e| soft(&e))?;
        report.stale += 1;
        return Ok(());
    };
    if is_smart_wiki(smart_wiki_index, &fact.wiki_id) || wiki::is_channel_page(&fact.source_path) {
        recall_log::set_miss_status(pool, miss.miss_id, "discarded", Some("out_of_scope_home"))
            .await
            .map_err(|e| soft(&e))?;
        return Ok(());
    }

    // The 15f loop: every real miss is a candidate gold case, whatever
    // the repair outcome (the operator distils and merges by hand).
    match recall_gate::append_gold_candidate(tree.workdir(), miss, &fact.text) {
        Ok(true) => report.gold_candidates_appended += 1,
        Ok(false) => {},
        Err(e) => report.errors.push(format!(
            "recall_repair {}: gold candidate: {e}",
            miss.fact_id
        )),
    }

    // Propose: which (non-smart, non-home) wiki would make it reachable?
    let home = discovered
        .iter()
        .find(|d| d.meta.wiki_id.as_str() == fact.wiki_id)
        .ok_or_else(|| format!("home wiki {} not on disk", fact.wiki_id))?;
    let candidates: Vec<&wiki::DiscoveredWiki> = discovered
        .iter()
        .filter(|d| {
            d.meta.wiki_id.as_str() != fact.wiki_id
                && !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str())
        })
        .collect();
    let decision = if candidates.is_empty() {
        None
    } else {
        propose_repair(tree, llm, &miss.restated_text, &fact, home, &candidates).await
    };

    let Some((dest, reason)) = decision else {
        // No local repair — on recurrence, queue the operator notice.
        return finish_unrepaired(
            pool,
            policy,
            miss,
            &fact,
            None,
            noticed,
            report,
            "no_repair_proposed",
        )
        .await;
    };

    // The gate: prove it on a scratch snapshot before touching anything.
    let source_page = wiki_relative_page(home, &fact.source_path).ok_or_else(|| {
        format!(
            "fact source_path {} not under its home wiki",
            fact.source_path
        )
    })?;
    let recipient = proposals::recipient_from_fact(&fact.owner_id, fact.sender_id.as_ref());
    let target = recall_gate::TargetCase {
        query: &miss.restated_text,
        sender_id: &miss.sender_id,
        topics: &miss.seed_topics,
        fact_id: &miss.fact_id,
    };
    let dest_id = dest.meta.wiki_id.as_str();
    let home_id = fact.wiki_id.as_str();
    let scratch_recipient = recipient.clone();
    let scratch_reason = reason.clone();
    let verdict = recall_gate::gate_repair(
        pool,
        tree.workdir(),
        Arc::clone(embedder),
        navigator,
        &policy.gate_recall,
        gold,
        &target,
        async |s_pool: &SqlitePool, s_tree: &WikiTree| {
            promote::apply_fact_refile_direct(
                s_pool,
                s_tree,
                &fact_id,
                home_id,
                &source_page,
                dest_id,
                "index.md",
                scratch_reason.as_deref(),
                scratch_recipient,
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
        },
    )
    .await
    .map_err(|e| format!("{e:#}"))?;

    if verdict.stale() {
        recall_log::set_miss_status(pool, miss.miss_id, "stale", Some("already_surfaces"))
            .await
            .map_err(|e| soft(&e))?;
        report.stale += 1;
        return Ok(());
    }
    if !verdict.passes() {
        report.gate_rejected += 1;
        let note = format!(
            "gate refused refile → {dest_id} (target_after={}, gold_regressed={}, gold_queries={})",
            verdict.target_after, verdict.gold_regressed, verdict.gold_queries
        );
        return finish_unrepaired(
            pool,
            policy,
            miss,
            &fact,
            Some(&note),
            noticed,
            report,
            &note,
        )
        .await;
    }

    // Proven — commit for real, act-first, same paper trail as the
    // refile sweep (born-applied receipt + structure_applied notice).
    let op_id = wal::begin_rem_op(pool, cycle_id, "recall_repair_apply", Some(home_id), None)
        .await
        .map_err(|e| soft(&e))?;
    let applied = match promote::apply_fact_refile_direct(
        pool,
        tree,
        &fact_id,
        home_id,
        &source_page,
        dest_id,
        "index.md",
        reason.as_deref(),
        recipient.clone(),
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            let _ = wal::fail_rem_op(pool, op_id, &format!("{e}")).await;
            return Err(soft(&e));
        },
    };
    wal::complete_rem_op(pool, op_id)
        .await
        .map_err(|e| soft(&e))?;
    events::insert_event(
        pool,
        EventKind::StructureApplied,
        Some(dest_id),
        Some(miss.fact_id.as_str()),
        &json!({
            "proposal_id": applied.proposal_id,
            "variant": "recall_repair_refile",
            "fact_id": miss.fact_id,
            "source_wiki_id": home_id,
            "dest_wiki_id": dest_id,
            "dest_page": "index.md",
            "missed_query": miss.restated_text,
            "recipient_id": recipient,
            "revert_deadline": applied.revert_deadline.to_rfc3339(),
            "dashboard_path": receipt_dashboard_path(&applied.proposal_id),
        }),
    )
    .await
    .map_err(|e| soft(&e))?;
    recall_log::set_miss_status(pool, miss.miss_id, "repaired", Some(&applied.proposal_id))
        .await
        .map_err(|e| soft(&e))?;
    tracing::info!(
        fact_id = %miss.fact_id,
        dest_wiki = dest_id,
        receipt = %applied.proposal_id,
        "recall repair: gated re-file COMMITTED (act-first)"
    );
    report.repairs_committed += 1;
    report.receipts.push(applied.proposal_id);
    let _ = now; // the sub-job keeps the cycle clock for symmetry with its siblings
    Ok(())
}

/// Ask the proposal confirmer; `Some((dest, reason))` only for a vetted
/// `move` verdict naming a candidate wiki. Conservative no-op on LLM
/// outage / unparseable / `stay` / hallucinated dest.
async fn propose_repair<'a>(
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    query: &str,
    fact: &FactIndexRow,
    home: &wiki::DiscoveredWiki,
    candidates: &[&'a wiki::DiscoveredWiki],
) -> Option<(&'a wiki::DiscoveredWiki, Option<String>)> {
    let candidates_text = candidates
        .iter()
        .enumerate()
        .map(|(i, d)| {
            format!(
                "{}. {} · {} — {}",
                i + 1,
                d.meta.wiki_id.as_str(),
                d.meta.title,
                wiki::meta_summary(&d.meta).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let home_line = format!("{} · {}", home.meta.wiki_id.as_str(), fact.source_path);
    let prompt = match prompts::render(
        "rem-recall-repair",
        tree.workdir(),
        BUNDLED_REM_RECALL_REPAIR_MD,
        &[
            ("query", query),
            ("fact_text", fact.text.as_str()),
            ("home_wiki", home_line.as_str()),
            ("candidates", candidates_text.as_str()),
        ],
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "recall repair: prompt render failed — skipped");
            return None;
        },
    };
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(300),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "recall repair: confirmer unavailable — skipped");
            return None;
        },
    };
    let decision: RefileDecision = first_json_object(&resp.text)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    if !decision.verdict.eq_ignore_ascii_case("move") {
        return None;
    }
    let dest_id = decision.dest_wiki_id.as_deref().map(str::trim)?;
    let dest = candidates
        .iter()
        .find(|d| d.meta.wiki_id.as_str() == dest_id)?;
    Some((dest, decision.reason))
}

/// Shared tail of every unrepaired outcome: on recurrence the operator
/// notice queues (once per fact per cycle), otherwise the miss discards
/// with its reason tag.
#[allow(
    clippy::too_many_arguments,
    reason = "per-miss pipeline tail over the cycle's shared context"
)]
async fn finish_unrepaired(
    pool: &SqlitePool,
    policy: &RemPolicy,
    miss: &crate::recall_log::MissRow,
    fact: &FactIndexRow,
    gate_note: Option<&str>,
    noticed: &mut HashSet<String>,
    report: &mut RecallRepairReport,
    reason_tag: &str,
) -> std::result::Result<(), String> {
    let count = recall_log::miss_count_for_fact(pool, &miss.fact_id)
        .await
        .map_err(|e| e.to_string())?;
    if count >= policy.recall_tuning_recurrence && !noticed.contains(&miss.fact_id) {
        events::insert_event(
            pool,
            EventKind::RecallTuningProposed,
            Some(&fact.wiki_id),
            Some(miss.fact_id.as_str()),
            &json!({
                "fact_id": miss.fact_id,
                "wiki_id": fact.wiki_id,
                "source_path": fact.source_path,
                "miss_count": count,
                "sample_query": miss.restated_text,
                "gate": gate_note,
                "hint": "recurring recall miss with no provable local repair — a recall-tuning \
                         lever (fact topics, recall knobs, navigator prompt) needs the operator; \
                         never auto-applied",
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
        noticed.insert(miss.fact_id.clone());
        recall_log::set_miss_status(pool, miss.miss_id, "queued", Some("recall_tuning_proposed"))
            .await
            .map_err(|e| e.to_string())?;
        report.queued += 1;
    } else {
        recall_log::set_miss_status(pool, miss.miss_id, "discarded", Some(reason_tag))
            .await
            .map_err(|e| e.to_string())?;
        report.no_repair += 1;
    }
    Ok(())
}

// ---------- Provenance-hygiene sweep sub-job ----------

/// One strip step of the trailing-pointer detector: `text` (already
/// `trim_end`ed) ends with a parenthetical wikilink `([[target]])`
/// preceded by whitespace → `(head, target)`.
///
/// The match pins the exact shape the document path's file phase used to
/// emit — ` ([[wiki/page]])` appended to a non-empty claim — and nothing
/// else: the target must be a plain `wiki/page` pointer (a `/`, no
/// brackets, no parens, no whitespace), the parenthetical must be
/// whitespace-separated from the claim, and the claim before it must be
/// non-empty. Anything looser (prose inside the parenthetical, a glued
/// suffix, a bare `[[link]]` without parens) is *content*, not the
/// defect, and is left alone.
fn strip_one_trailing_ref(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_suffix("]])")?;
    let open = rest.rfind("([[")?;
    let target = &rest[open + 3..];
    if target.is_empty()
        || !target.contains('/')
        || target.contains(['[', ']', '(', ')'])
        || target.chars().any(char::is_whitespace)
    {
        return None;
    }
    let head = &rest[..open];
    if !head.ends_with(char::is_whitespace) {
        return None;
    }
    let head = head.trim_end();
    if head.is_empty() {
        return None;
    }
    Some((head, target))
}

/// Deterministic detector for the trailing source-pointer defect:
/// repeatedly strips trailing ` ([[wiki/page]])` parentheticals off the
/// end of `text`, returning the cleaned claim and the stripped targets as
/// plain `[[wiki/page]]` refs in document order. `None` when the text does
/// not end with the defect — a wikilink **mid-prose** is legitimate
/// content and never matches (the scan anchors on the trailing pattern
/// only).
fn split_trailing_provenance_refs(text: &str) -> Option<(String, Vec<String>)> {
    let mut head = text.trim_end();
    let mut refs: Vec<String> = Vec::new();
    while let Some((h, target)) = strip_one_trailing_ref(head) {
        head = h;
        refs.push(format!("[[{target}]]"));
    }
    if refs.is_empty() {
        return None;
    }
    // Stripped right-to-left; restore document order.
    refs.reverse();
    Some((head.to_owned(), refs))
}

/// The provenance-hygiene sweep — trailing source pointers move off the
/// claim text into `authored_refs`.
///
/// Mechanical repair of a known defect pattern, not a semantic gate: the
/// document path's file phase used to append the dossier backlink to the
/// claim body (` ([[wiki/page]])`), flooding the document page with
/// inbound links, feeding link noise to embeddings and dedup, and
/// freezing prose the Cronista cannot restyle. The go-forward writer is
/// fixed (provenance rides `authored_refs` —
/// document ingest); this
/// sweep converges the pre-existing corpus. Per flagged fact: move the
/// pointer into `authored_refs` (dedup'd), strip the suffix, re-embed the
/// cleaned text, and write text + embedding + refs in **one atomic
/// statement** ([`fact_index::update_region_and_authored_refs`], offsets
/// kept — the render-content fingerprint recompiles the touched pages).
/// Fully deterministic (no LLM) and convergent: once the corpus is clean
/// the detector flags nothing and the sweep no-ops forever. Oldest first,
/// capped by `policy.provenance_hygiene_cap` — a resource cap on embedder
/// spend, like the sibling sweeps' caps.
async fn run_provenance_hygiene(
    pool: &SqlitePool,
    embedder: &Arc<dyn Embedder>,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<ProvenanceHygieneReport> {
    let mut report = ProvenanceHygieneReport::default();
    if policy.provenance_hygiene_cap == 0 {
        return Ok(report);
    }
    let mut flagged: Vec<(FactIndexRow, String, Vec<String>)> = Vec::new();
    for (wiki, smart) in smart_wiki_index {
        // Smart-wiki rows are section projections of consumer-authored
        // files — REM never edits them.
        if *smart {
            continue;
        }
        for row in fact_index::find_active_in_wiki(pool, wiki).await? {
            if let Some((clean, refs)) = split_trailing_provenance_refs(&row.text) {
                flagged.push((row, clean, refs));
            }
        }
    }
    report.flagged = flagged.len();
    if flagged.is_empty() {
        return Ok(report);
    }
    // Oldest first: a pre-existing backlog drains deterministically.
    flagged.sort_by(|a, b| a.0.created_at.cmp(&b.0.created_at));
    flagged.truncate(policy.provenance_hygiene_cap);
    report.examined = flagged.len();

    let op_id = wal::begin_rem_op(pool, cycle_id, "provenance_hygiene_apply", None, None).await?;
    for (row, clean, refs) in &flagged {
        // Append the moved pointer(s) to the row's existing refs, dedup'd —
        // re-running over a partially repaired corpus never double-records.
        let mut authored_refs = row.authored_refs.clone();
        for r in refs {
            if !authored_refs.contains(r) {
                authored_refs.push(r.clone());
            }
        }
        let embedding = match embedder
            .embed(&crate::parser::strip_embed_markers(clean))
            .await
        {
            Ok(e) => e,
            Err(e) => {
                report
                    .errors
                    .push(format!("provenance embed {}: {e}", row.fact_id));
                continue;
            },
        };
        // In-place update, offsets kept: the on-disk marker still frames
        // the old prose; the row text now disagrees with the rendered page,
        // which is exactly the drift the render-content fingerprint notices
        // — the next compile rewrites the touched pages.
        let update = fact_index::RegionUpdate {
            region_start: row.region_start,
            region_end: row.region_end,
            text: clean.clone(),
            embedding,
        };
        if fact_index::update_region_and_authored_refs(pool, &row.fact_id, &update, &authored_refs)
            .await?
            > 0
        {
            tracing::info!(
                fact_id = %row.fact_id,
                old = %fact_preview(&row.text),
                new = %fact_preview(clean),
                refs = %refs.join(" "),
                "rem provenance: trailing source pointer moved into authored_refs"
            );
            report.moved.push(row.fact_id.as_str().to_owned());
        }
    }
    wal::complete_rem_op(pool, op_id).await?;
    Ok(report)
}

// ---------- Husk-page GC sub-job ----------

/// Remove husk page FILES: plan-absent, non-reserved pages whose fact
/// rows are ALL tombstoned or superseded past the receipts' revert
/// window ([`proposals::REVERT_WINDOW`]).
///
/// The compiler's orphan sweep (`sweep_orphan_page_files`, every
/// compile) already drops a plan-absent file with **no** non-tombstoned
/// rows; it must keep a file while a superseded row points at it,
/// because that row's on-disk marker may still serve a supersede
/// revert. Once the window is past nothing can revert onto the page —
/// the file is a husk (a supersede's leftover obituary page, a
/// placeholder whose only fact fell): this sweep removes it and settles
/// the retired rows' stale offsets. Inbound links degrade to literal
/// text at render (the link grammar's dead-rail posture — never a
/// broken link) and the compile feed's dead-ref vetting keeps prose
/// clean, so no link rewriter is needed.
///
/// Deterministic, no LLM: a structural GC behind DB-first guards, not a
/// semantic judgment (each fact was closed by its own judged path).
/// `index.md` / `rules.md` / `_`-prefixed files never qualify; smart
/// wikis are skipped (consumer-authored files are never REM's to
/// delete); **no plan on disk → no-op** (a fresh workdir's pages are
/// unplanned, not husks). Bounded by `policy.husk_gc_cap` per cycle,
/// oldest wikis/pages first by path order so a backlog drains
/// deterministically.
async fn run_husk_gc(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<HuskGcReport> {
    let mut report = HuskGcReport::default();
    if policy.husk_gc_cap == 0 {
        return Ok(report);
    }
    // No plan yet = a fresh workdir whose pages are simply unplanned, not
    // husks; a load failure is a soft skip like the page-merge sibling.
    let plan = match crate::planner::load_previous_plan(tree) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(report),
        Err(e) => {
            report.errors.push(format!("husk: plan load failed: {e}"));
            return Ok(report);
        },
    };
    let mut planned: HashMap<&str, std::collections::BTreeSet<&str>> = HashMap::new();
    for page in plan.pages.values() {
        planned
            .entry(page.wiki_id.as_str())
            .or_default()
            .insert(page.page_path.as_str());
    }
    let horizon = (now - proposals::REVERT_WINDOW).to_rfc3339();

    // Candidates: every plan-absent, non-reserved page file of every
    // non-smart wiki, in deterministic (wiki, page) order.
    let mut removable: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    for d in tree.walk()? {
        let wiki_id = d.meta.wiki_id.as_str();
        if smart_wiki_index.get(wiki_id).copied().unwrap_or(false) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&d.abs_dir) else {
            continue;
        };
        let pages = planned.get(wiki_id);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !std::path::Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                || name.starts_with('_')
                || name == "index.md"
                || name == wiki::RULES_FILENAME
                || pages.is_some_and(|p| p.contains(name))
                || !entry.path().is_file()
            {
                continue;
            }
            let source_path = wiki::workdir_relative_source_path(tree.workdir(), &entry.path());
            report.pages_examined += 1;
            match fact_index::count_husk_blocking_rows(pool, &source_path, &horizon).await {
                Ok(0) => {
                    removable.push((wiki_id.to_owned(), source_path, entry.path()));
                },
                Ok(_) => {}, // an active row or a revertible marker keeps the file
                Err(e) => report.errors.push(format!("husk count {source_path}: {e}")),
            }
        }
    }
    if removable.is_empty() {
        return Ok(report);
    }
    removable.sort_by(|a, b| a.1.cmp(&b.1));
    report.deferred = removable.len().saturating_sub(policy.husk_gc_cap);
    removable.truncate(policy.husk_gc_cap);

    let op_id = wal::begin_rem_op(pool, cycle_id, "husk_gc_apply", None, None).await?;
    for (wiki_id, source_path, abs) in &removable {
        if let Err(e) = std::fs::remove_file(abs) {
            report
                .errors
                .push(format!("husk remove {source_path}: {e}"));
            continue;
        }
        // The bytes are gone: settle the retired rows still pointing at
        // the page so the retirement sweep converges without reopening it.
        if let Err(e) = fact_index::clear_region_offsets_retired_on_page(pool, source_path).await {
            report
                .errors
                .push(format!("husk settle {source_path}: {e}"));
        }
        tracing::info!(
            wiki_id,
            source_path,
            "rem husk-gc: husk page removed (plan-absent, all rows past any revert)"
        );
        report.removed.push(source_path.clone());
    }
    wal::complete_rem_op(pool, op_id).await?;
    Ok(report)
}

// ---------- Date normalizer sub-job ----------

/// Bundled rewrite prompt for the date normalizer. Operator override:
/// `<workdir>/prompts/rem-dates.md`.
pub const BUNDLED_REM_DATES_MD: &str = include_str!("../prompts/rem-dates.md");

/// The LLM's batched rewrite answer.
#[derive(Debug, serde::Deserialize)]
struct DateRewrites {
    #[serde(default)]
    rewrites: Vec<DateRewrite>,
}

/// One rewritten fact inside a [`DateRewrites`].
#[derive(Debug, serde::Deserialize)]
struct DateRewrite {
    // Both fields default so one malformed element (a missing `fact_id`
    // or `text`) degrades to an empty string rather than failing the
    // whole batch deserialize. Safe here because neither empty value has
    // a silent-apply path: an empty `fact_id` fails the batch-containment
    // lookup, and empty `text` hits the `is_empty` refusal in the apply
    // loop.
    #[serde(default)]
    fact_id: String,
    #[serde(default)]
    text: String,
}

/// Cheap lexical pre-filter: does the text contain a phrase that *looks*
/// like an unresolved relative date? A resource optimisation only (skip
/// the LLM on unflagged facts) — the LLM decides whether a flagged fact
/// really needs the rewrite, and an unflagged miss simply waits for a
/// richer lexicon. Italian + English, case-insensitive, word-boundary.
fn looks_deictic(text: &str) -> bool {
    const LEXICON: &[&str] = &[
        "oggi",
        "ieri",
        "domani",
        "dopodomani",
        "stasera",
        "stamattina",
        "stanotte",
        "questa settimana",
        "settimana prossima",
        "settimana scorsa",
        "questo mese",
        "mese prossimo",
        "mese scorso",
        "quest'anno",
        "anno prossimo",
        "anno scorso",
        "today",
        "yesterday",
        "tomorrow",
        "tonight",
        "this week",
        "next week",
        "last week",
        "this month",
        "next month",
        "last month",
        "this year",
        "next year",
    ];
    let lower = text.to_lowercase();
    LEXICON.iter().any(|phrase| {
        lower.match_indices(phrase).any(|(i, _)| {
            let before_ok = i == 0
                || !lower[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric);
            let after = i + phrase.len();
            let after_ok = after >= lower.len()
                || !lower[after..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            before_ok && after_ok
        })
    })
}

/// The date normalizer — relative→absolute rewrites on canonical text.
///
/// Capture-side resolution (the ingest prompt's `current_time` anchor)
/// handles new facts; this sub-job heals what slipped through and the
/// pre-existing backlog: every active fact the deictic lexicon flags is
/// sent — oldest first, capped — in ONE batched call to the revisor
/// model (`llms.revisor`), which rewrites each relative phrase
/// against **that fact's own capture instant**. The anchor fed to the model is `valid_from` (the
/// stored projection of the turn's semantic clock — a replayed or
/// backfilled fact carries the day it was *uttered* there) with
/// `created_at` as the fallback for rows without a window; the row
/// insertion instant alone would resolve a backfilled "oggi" against
/// the wrong day. An applied rewrite re-embeds the text and updates
/// the row in place (offsets kept); the render-content fingerprint then
/// recompiles exactly the touched pages, so prose and `lista` records
/// alike stop reading "oggi" days later.
#[expect(
    clippy::too_many_lines,
    reason = "linear batch pipeline (flag → prompt → rewrite → re-embed); splitting hides the order, as in run_page_merge"
)]
async fn run_date_normalizer(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    embedder: &Arc<dyn Embedder>,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<DateNormalizeReport> {
    let mut report = DateNormalizeReport::default();
    if policy.date_normalize_cap == 0 {
        return Ok(report);
    }
    let mut flagged: Vec<FactIndexRow> = Vec::new();
    for (wiki, smart) in smart_wiki_index {
        if *smart {
            continue;
        }
        for row in fact_index::find_active_in_wiki(pool, wiki).await? {
            if looks_deictic(&row.text) {
                flagged.push(row);
            }
        }
    }
    report.flagged = flagged.len();
    if flagged.is_empty() {
        return Ok(report);
    }
    // Oldest first: a pre-existing backlog drains deterministically.
    flagged.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    flagged.truncate(policy.date_normalize_cap);
    report.examined = flagged.len();

    let facts_text = flagged
        .iter()
        .enumerate()
        .map(|(i, f)| {
            // The "captured_at" the prompt promises: the semantic capture
            // instant, i.e. `valid_from` when stamped (deduced against the
            // turn's `occurred_at` clock at ingest), `created_at` otherwise.
            let anchor = f.valid_from.as_deref().unwrap_or(&f.created_at);
            format!(
                "{}. {} · {} · {}",
                i + 1,
                f.fact_id.as_str(),
                anchor,
                f.text.replace('\n', " ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = prompts::render(
        "rem-dates",
        tree.workdir(),
        BUNDLED_REM_DATES_MD,
        &[("facts", facts_text.as_str())],
    )?;
    let resp = match llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.1)
                .with_max_tokens(2048),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            report.errors.push(format!("normalizer LLM: {e}"));
            return Ok(report);
        },
    };
    let parsed =
        first_json_object(&resp.text).and_then(|v| serde_json::from_value::<DateRewrites>(v).ok());
    let Some(decision) = parsed else {
        report
            .errors
            .push("normalizer: unparseable LLM answer".to_owned());
        return Ok(report);
    };
    if decision.rewrites.is_empty() {
        return Ok(report);
    }

    let op_id = wal::begin_rem_op(pool, cycle_id, "date_normalize_apply", None, None).await?;
    for rw in &decision.rewrites {
        let new_text = rw.text.trim();
        // Validate against the batch (anti-hallucination) + the body rules.
        let Some(row) = flagged.iter().find(|f| f.fact_id.as_str() == rw.fact_id) else {
            report
                .errors
                .push(format!("normalizer named non-batch fact {}", rw.fact_id));
            continue;
        };
        // Marker guard: the rewrite may carry braces only as well-formed
        // self-closing embeds, and its embed set must equal the
        // original's — a date rewrite may never add, drop, or alter a
        // media link (see media pipeline).
        let embeds_ok = if new_text.contains("{{") || new_text.contains("}}") {
            crate::parser::embed_only_markers(new_text)
                .is_some_and(|new_embeds| new_embeds == crate::parser::collect_embeds(&row.text))
        } else {
            crate::parser::collect_embeds(&row.text).is_empty()
        };
        if new_text.is_empty() || new_text == row.text || !embeds_ok || new_text.contains("<!--") {
            report
                .errors
                .push(format!("normalizer rewrite refused for {}", rw.fact_id));
            continue;
        }
        let embedding = match embedder
            .embed(&crate::parser::strip_embed_markers(new_text))
            .await
        {
            Ok(e) => e,
            Err(e) => {
                report
                    .errors
                    .push(format!("normalizer embed {}: {e}", row.fact_id));
                continue;
            },
        };
        // In-place update, offsets kept: the marker is still on disk; the
        // row text now disagrees with the rendered prose, which is exactly
        // the drift the render-content fingerprint notices and recompiles.
        let update = fact_index::RegionUpdate {
            region_start: row.region_start,
            region_end: row.region_end,
            text: new_text.to_owned(),
            embedding,
        };
        if fact_index::update_region(pool, &row.fact_id, &update).await? > 0 {
            tracing::info!(
                fact_id = %row.fact_id,
                old = %fact_preview(&row.text),
                new = %fact_preview(new_text),
                "rem dates: canonical text normalized (relative → absolute)"
            );
            report.rewritten.push(row.fact_id.as_str().to_owned());
        }
    }
    wal::complete_rem_op(pool, op_id).await?;
    Ok(report)
}

// ---------- Archive detector sub-job ----------

/// Walk every active fact, group by `(wiki_id, source_path)`, and for
/// each path where **every** active fact is older than
/// `policy.archive_inactivity` (using `last_recall_at`, falling back to
/// `created_at` when null) emit one `archive_proposals` row. The
/// page is the unit because the apply step moves whole files into
/// `_archive/`; partial-page archival is reserved for a future
/// dashboard "selection" flow.
///
/// Pages whose oldest active fact is fresher than the threshold are
/// left alone — one recent capture is enough to keep the page off the
/// archive queue.
async fn run_archive_detector(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<ArchiveDetectorReport> {
    let mut report = ArchiveDetectorReport::default();
    let threshold = now - policy.archive_inactivity;
    let threshold_iso = threshold.to_rfc3339();
    for d in tree.walk()? {
        if report.proposals_emitted.len() >= policy.archive_cap {
            break;
        }
        // REM never archives a smart-wiki page — the smart
        // consumer manages staleness through `_briefing.md`.
        if is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        let facts = fact_index::find_active_in_wiki(pool, d.meta.wiki_id.as_str()).await?;
        // Group by source_path; the "freshest" timestamp per group
        // decides whether the page is stale.
        let mut by_path: std::collections::HashMap<String, DateTime<Utc>> =
            std::collections::HashMap::new();
        for f in &facts {
            let stamp = f
                .last_recall_at
                .as_deref()
                .or(Some(f.created_at.as_str()))
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map_or(now, |t| t.with_timezone(&Utc));
            let entry = by_path.entry(f.source_path.clone()).or_insert(stamp);
            if stamp > *entry {
                *entry = stamp;
            }
        }
        for (path, freshest) in by_path {
            if report.proposals_emitted.len() >= policy.archive_cap {
                break;
            }
            report.paths_examined += 1;
            if freshest >= threshold {
                continue;
            }
            if archive::already_proposed(pool, d.meta.wiki_id.as_str(), &path).await? {
                continue;
            }
            let op_id = wal::begin_rem_op(
                pool,
                cycle_id,
                "archive_emit",
                Some(d.meta.wiki_id.as_str()),
                None,
            )
            .await?;
            match archive::emit_archive_proposal(
                pool,
                d.meta.wiki_id.as_str(),
                &path,
                archive::reason::NO_RECALL_HIT_365D,
            )
            .await
            {
                Ok(pid) => {
                    wal::complete_rem_op(pool, op_id).await?;
                    report.proposals_emitted.push(pid.clone());
                    events::insert_event(
                        pool,
                        EventKind::ArchiveProposed,
                        Some(d.meta.wiki_id.as_str()),
                        None,
                        &json!({
                            "proposal_id": pid,
                            "kind": "archive",
                            "path": path,
                            "reason": archive::reason::NO_RECALL_HIT_365D,
                            "freshest_at": freshest.to_rfc3339(),
                            "threshold": threshold_iso,
                            // 0032: archive proposals live in a separate
                            // table without a recipient column — unaddressed.
                            "recipient_id": serde_json::Value::Null,
                        }),
                    )
                    .await?;
                },
                Err(e) => {
                    wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                    report.errors.push(format!("emit archive failed: {e}"));
                },
            }
        }
    }
    Ok(report)
}

// ---------- Lease expirer sub-job ----------

/// Thin wrapper around [`crate::wiki_admin_leases::expire_stale`].
/// Runs once per REM cycle, between the briefing/backlink emitters
/// and the Hub Writer. Two passes (see the module docstring of
/// `wiki_admin_leases` for the contract):
///
/// 1. Active rows whose `expires_at < now - grace` get
///    `released_at = now` (treated as crashed without release).
/// 2. Released rows older than `now - retention` are deleted.
///
/// No per-row reporting; just the two counts. Per-row soft errors
/// cannot happen — the only failure mode is SQL infrastructure,
/// which bubbles up as [`RemError::Db`].
async fn run_lease_expirer(
    pool: &SqlitePool,
    now: DateTime<Utc>,
    policy: &RemPolicy,
) -> Result<crate::wiki_admin_leases::ExpirerReport> {
    let report = crate::wiki_admin_leases::expire_stale(
        pool,
        now,
        policy.lease_expirer_grace.num_seconds(),
        policy.lease_expirer_retention.num_seconds(),
    )
    .await?;
    Ok(report)
}

// ---------- Briefing-processor non-smart (sub-job 10) ----------

/// Drain pending `wiki_briefing_items` rows whose `wiki_id` is a
/// non-smart wiki and whose `ts` is older than the configured
/// grace period.
///
/// On smart wikis the inbox is drained by the smart consumer at
/// `smart_bootstrap` via `mark_processed` on the next `wiki_admin_push`.
/// Narrative families (every non-smart wiki: `wiki-user`,
/// `wiki-root`, `wiki-group`, and emerged sub-wikis) have no smart
/// consumer, so REM
/// fills the gap by calling the **same** core function
/// ([`briefing_processor::process_briefing_item`]) the dashboard
/// "Submit" endpoint uses synchronously — one branch, two callers, no
/// drift.
///
/// Policy: **mark-passive** (see [`briefing_processor`] module doc).
/// The grace period guards against draining a comment the operator is
/// still editing through the dashboard — the synchronous Submit
/// endpoint bypasses the grace, the cycle does not.
///
/// Per-row outcomes from `process_briefing_item`:
///
/// - `Processed` → counted in `items_processed`.
/// - `AlreadyProcessed` → counted in `items_already_processed`
///   (real-world cause: an interactive Submit drained the row between
///   the candidate scan and the per-row call).
/// - `WikiNotFound` → counted in `items_wiki_missing`, no DB write,
///   the operator gets a heads-up via the report.
///
/// Per-row errors surface in `report.errors`; infrastructure-level
/// failures bubble as [`RemError`].
async fn run_briefing_processor_non_smart(
    pool: &SqlitePool,
    tree: &WikiTree,
    embedder: &Arc<dyn Embedder>,
    comment_applier: Option<&dyn LlmBackend>,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<BriefingProcessorReport> {
    let mut report = BriefingProcessorReport::default();
    if !policy.briefing_processor_enabled {
        return Ok(report);
    }

    let cutoff = (now - policy.briefing_processor_grace).to_rfc3339();
    let candidates: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, wiki_id FROM wiki_briefing_items \
         WHERE processed_at IS NULL AND ts < ? \
         ORDER BY id ASC",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;

    // Partition the candidate rows by their wiki's smart flag
    // (`smart_wiki_index`, the per-cycle `_meta.md` snapshot). A
    // non-smart wiki is a standard wiki now that the `wiki_type` registry
    // is retired, so its comments get **action-taking**: they are
    // interpreted into fact ops, batched per wiki (read together,
    // applied together). When the `ingest` slot is unconfigured the
    // action path is unavailable, so they fall back to mark-passive.
    let mut standard_by_wiki: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut mark_passive: Vec<i64> = Vec::new();
    for (bi_id, wiki_id) in candidates {
        // Smart family: the smart consumer owns the drain, REM must not
        // touch it. Unknown wikis (deleted between the snapshot and now) are
        // absent from the index and fall through to the per-row handler which
        // surfaces them as `WikiNotFound`.
        match smart_wiki_index.get(&wiki_id) {
            // Companion: the smart consumer owns the drain — skip (no-op).
            Some(true) => {},
            Some(false) if comment_applier.is_some() => {
                standard_by_wiki.entry(wiki_id).or_default().push(bi_id);
            },
            _ => mark_passive.push(bi_id),
        }
    }

    if let Some(llm) = comment_applier {
        let now_str = now.to_rfc3339();
        for (wiki_id, bi_ids) in &standard_by_wiki {
            report.items_examined += bi_ids.len();
            let Ok(parsed) = WikiId::parse(wiki_id) else {
                report
                    .errors
                    .push(format!("comment_apply: invalid wiki_id {wiki_id}"));
                continue;
            };
            let applied = crate::comment_apply::apply_comments(
                pool, tree, embedder, llm, &parsed, bi_ids, &now_str,
            )
            .await?;
            report.items_processed += applied.comments_processed;
            report.facts_corrected += applied.facts_corrected;
            report.facts_added += applied.facts_added;
            report.facts_deduped += applied.facts_deduped;
            report.facts_removed += applied.facts_removed;
            report.facts_moved += applied.facts_moved;
            report.errors.extend(applied.errors);
        }
    }

    for bi_id in mark_passive {
        report.items_examined += 1;
        match briefing_processor::process_briefing_item(pool, tree, bi_id).await {
            Ok(briefing_processor::ProcessOutcome::Processed { .. }) => {
                report.items_processed += 1;
            },
            Ok(briefing_processor::ProcessOutcome::AlreadyProcessed { .. }) => {
                report.items_already_processed += 1;
            },
            Ok(briefing_processor::ProcessOutcome::WikiNotFound { .. }) => {
                report.items_wiki_missing += 1;
            },
            Err(e) => {
                report
                    .errors
                    .push(format!("briefing_processor bi_{bi_id}: {e}"));
            },
        }
    }
    Ok(report)
}

// ---------- Hub Writer sub-job ----------

async fn run_hub_writer(
    pool: &SqlitePool,
    tree: &WikiTree,
    llm: &dyn LlmBackend,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<HubWriterReport> {
    let mut report = HubWriterReport::default();
    // Every wiki whose `index.md` the compilation plan owns is off-limits:
    // the compiler is its writer (person / group_theme / emerged_index
    // foundation nodes), and a REM-side regeneration would fight it over
    // the same file. With the emerged-index foundation pass this covers
    // every standard wiki the plan has seen; the walk below only serves
    // wikis a plan does not cover (no plan yet, or a wiki outside it).
    let plan_owned_indexes: std::collections::BTreeSet<String> =
        match crate::planner::load_previous_plan(tree) {
            Ok(Some(plan)) => plan
                .pages
                .values()
                .filter(|p| p.page_path == "index.md")
                .map(|p| p.wiki_id.clone())
                .collect(),
            Ok(None) => std::collections::BTreeSet::new(),
            Err(e) => {
                tracing::warn!(error = %e, "hub_writer: persisted plan unreadable — treating no index as plan-owned");
                std::collections::BTreeSet::new()
            },
        };
    for d in tree.walk()? {
        if report.regenerated.len() >= policy.hub_writer_cap {
            break;
        }
        let wiki_id = d.meta.wiki_id.as_str().to_owned();
        // Hub Writer never rewrites a smart wiki's `index.md` —
        // the smart consumer crafts its own hub pages via
        // `wiki_admin_push`.
        if is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            report.skipped.push(wiki_id);
            continue;
        }
        if plan_owned_indexes.contains(&wiki_id) {
            report.skipped.push(wiki_id);
            continue;
        }
        // Trigger: the wiki must have at least one child AND at least one active fact.
        if d.meta.children.is_empty() {
            report.skipped.push(wiki_id);
            continue;
        }
        let active = fact_index::count_active_in_wiki(pool, &wiki_id).await?;
        if active == 0 {
            report.skipped.push(wiki_id);
            continue;
        }
        let op_id = wal::begin_rem_op(pool, cycle_id, "hub_writer", Some(&wiki_id), None).await?;
        match regenerate_index(pool, tree, &d, llm).await {
            Ok(()) => {
                wal::complete_rem_op(pool, op_id).await?;
                report.regenerated.push(wiki_id);
            },
            Err(e) => {
                wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
                report.errors.push(format!("hub_writer on {wiki_id}: {e}"));
            },
        }
    }
    Ok(report)
}

/// Bundled default for the `regenerate-index` system prompt — the
/// Hub Writer's REM-side consumer that regenerates the `index.md` of
/// a non-smart parent wiki.
///
/// The verbatim prompt body lives in
/// `crates/mwe-core/prompts/regenerate-index.md` (frontmatter + a
/// single ```text ... ``` fenced block) and is loaded through
/// [`prompts::render`]; an operator override at
/// `<workdir>/prompts/regenerate-index.md` wins when present.
/// Referenced from [`prompts::BUNDLED`] so `mwe-mcp init` materialises
/// it under the workdir. Pre-extraction this prompt was built
/// inline as a `format!()` string; an audit noted
/// that the operator override mechanism could not apply to a
/// hardcoded prompt, and the extraction closes the gap.
pub const BUNDLED_REGENERATE_INDEX_MD: &str = include_str!("../prompts/regenerate-index.md");

fn regenerate_index_prompt(
    tree: &WikiTree,
    title: &str,
    wiki_type: &str,
    wiki_id: &str,
    children: &str,
    snippet: &str,
) -> Result<String> {
    prompts::render(
        "regenerate-index",
        tree.workdir(),
        BUNDLED_REGENERATE_INDEX_MD,
        &[
            ("title", title),
            ("wiki_type", wiki_type),
            ("wiki_id", wiki_id),
            ("children", children),
            ("snippet", snippet),
        ],
    )
    .map_err(RemError::from)
}

async fn regenerate_index(
    pool: &SqlitePool,
    tree: &WikiTree,
    d: &wiki::DiscoveredWiki,
    llm: &dyn LlmBackend,
) -> Result<()> {
    let facts = fact_index::find_active_in_wiki(pool, d.meta.wiki_id.as_str()).await?;
    // The prompt shows the 20 most-recent facts, most-recent first. The
    // query returns the whole wiki oldest-first (`created_at ASC`), so
    // take the tail via `.rev()` (newest first) and cap at 20 — never the
    // 20 oldest, which a plain `truncate(20)` on the ASC list would keep.
    let bodies: Vec<&str> = facts
        .iter()
        .rev()
        .take(20)
        .map(|f| f.text.as_str())
        .collect();
    let snippet = bodies.join("\n\n---\n\n");
    // Child wikis as canonical `[[wiki_id]]` wiki hops (the link grammar in
    // recall-pipeline.md): the regenerated index must carry rails the recall
    // navigator and the dashboard click-through can follow — a relative
    // markdown link resolves for neither.
    let children = d
        .meta
        .children
        .iter()
        .map(|c| format!("- [[{}]]", c.wiki_id))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = regenerate_index_prompt(
        tree,
        &d.meta.title,
        &d.meta.wiki_type,
        d.meta.wiki_id.as_str(),
        &children,
        &snippet,
    )?;
    let resp = llm
        .complete(
            CompletionRequest::new(prompt)
                .with_temperature(0.2)
                .with_max_tokens(2_000),
        )
        .await
        .map_err(|e| {
            RemError::Llm(format!(
                "hub_writer failed on wiki {}: {e}",
                d.meta.wiki_id.as_str()
            ))
        })?;
    let index_path = d.abs_dir.join("index.md");
    wiki::atomic_write(&index_path, resp.text.as_bytes())?;
    Ok(())
}

// ---------- Briefing dispatcher sub-job ----------

/// Scan every smart-family wiki for two flavours of finding worth
/// putting in front of the smart consumer:
///
/// - **stale draft** — a fact whose top-level YAML mapping carries
///   `status: draft` and whose age exceeds
///   [`RemPolicy::briefing_stale_draft_age`];
/// - **recall-hot** — a fact whose `recall_count_30d` is at or above
///   [`RemPolicy::briefing_recall_hot_threshold`], signalling the
///   smart consumer might want to promote it into its own structure.
///
/// Each finding produces one `_briefing.md` item via
/// [`crate::briefing::notify_as_rem`]. The per-finding `source_ref` is
/// deterministic (`rem:briefing_dispatcher:<kind>:<fact_id>`), so the
/// idempotency probe in [`briefing_recently_emitted`] absorbs the same
/// finding in subsequent cycles for [`RemPolicy::briefing_dedup_window`]
/// — REM never spams the same item night after night. The per-wiki
/// hard cap is [`RemPolicy::briefing_notify_cap`].
async fn run_briefing_dispatcher(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    now: DateTime<Utc>,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<BriefingDispatcherReport> {
    let mut report = BriefingDispatcherReport::default();
    let stale_threshold = now - policy.briefing_stale_draft_age;
    for d in tree.walk()? {
        if !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        report.wikis_examined += 1;
        let mut per_wiki = 0_usize;
        // A smart wiki's content lives in `wiki_sections`, not `fact_index`.
        let facts = sections::find_wiki_sections(pool, d.meta.wiki_id.as_str()).await?;
        for fact in &facts {
            if per_wiki >= policy.briefing_notify_cap {
                break;
            }
            try_emit_stale_draft(
                pool,
                tree,
                cycle_id,
                &d.meta.wiki_id,
                fact,
                stale_threshold,
                policy.briefing_dedup_window,
                &mut report,
                &mut per_wiki,
            )
            .await?;
            if per_wiki >= policy.briefing_notify_cap {
                break;
            }
            try_emit_recall_hot(
                pool,
                tree,
                cycle_id,
                &d.meta.wiki_id,
                fact,
                policy.briefing_recall_hot_threshold,
                policy.briefing_dedup_window,
                &mut report,
                &mut per_wiki,
            )
            .await?;
        }
    }
    Ok(report)
}

/// Inner helper: post a stale-draft notify when applicable. Counts the
/// emission against `per_wiki` (so the caller's per-wiki cap stays in
/// sync) and the dedup row against `report.deduplicated`. Hard errors
/// from the idempotency probe bubble; soft errors from the briefing
/// pipeline land in `report.errors`.
#[allow(
    clippy::too_many_arguments,
    reason = "splitting the briefing dispatcher loop into per-finding helpers requires threading the per-wiki state in; the call site reads cleanly once the helper is small"
)]
async fn try_emit_stale_draft(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    wiki_id: &WikiId,
    fact: &sections::SectionRow,
    stale_threshold: DateTime<Utc>,
    dedup_window: chrono::Duration,
    report: &mut BriefingDispatcherReport,
    per_wiki: &mut usize,
) -> Result<()> {
    if !fact_status_is_draft(&fact.text) {
        return Ok(());
    }
    let Some(created_ts) = chrono::DateTime::parse_from_rfc3339(&fact.created_at)
        .ok()
        .map(|t| t.with_timezone(&Utc))
    else {
        return Ok(());
    };
    if created_ts >= stale_threshold {
        return Ok(());
    }
    let source_ref = format!("rem:briefing_dispatcher:stale_draft:{}", fact.handle());
    if briefing_recently_emitted(pool, wiki_id, &source_ref, dedup_window).await? {
        report.deduplicated += 1;
        return Ok(());
    }
    let topic = format!(
        "Stale draft on `{path}` (created {created})",
        path = fact.source_path,
        created = fact.created_at,
    );
    let body = format!(
        "Section `{id}` on page `{path}` has carried `status: draft` since {created}. \
         Promote, supersede, or archive it during the next session.",
        id = fact.handle(),
        path = fact.source_path,
        created = fact.created_at,
    );
    match emit_dispatcher_notify(
        pool,
        tree,
        cycle_id,
        wiki_id,
        topic.clone(),
        body,
        source_ref,
    )
    .await
    {
        Ok(()) => {
            report
                .notifications_emitted
                .push((wiki_id.as_str().to_owned(), topic));
            *per_wiki += 1;
        },
        Err(e) => report
            .errors
            .push(format!("briefing_dispatcher stale_draft: {e}")),
    }
    Ok(())
}

/// Inner helper: post a recall-hot notify when applicable. Same
/// counter-threading contract as [`try_emit_stale_draft`].
#[allow(clippy::too_many_arguments, reason = "see try_emit_stale_draft")]
async fn try_emit_recall_hot(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    wiki_id: &WikiId,
    fact: &sections::SectionRow,
    threshold: i64,
    dedup_window: chrono::Duration,
    report: &mut BriefingDispatcherReport,
    per_wiki: &mut usize,
) -> Result<()> {
    if fact.recall_count_30d < threshold {
        return Ok(());
    }
    let source_ref = format!("rem:briefing_dispatcher:recall_hot:{}", fact.handle());
    if briefing_recently_emitted(pool, wiki_id, &source_ref, dedup_window).await? {
        report.deduplicated += 1;
        return Ok(());
    }
    let topic = format!(
        "Recall-hot on `{path}` ({hits}/30d)",
        path = fact.source_path,
        hits = fact.recall_count_30d,
    );
    let body = format!(
        "Section `{id}` on page `{path}` was recalled {hits} times in the last 30 days — \
         consider promoting it into a dedicated page or surfacing it more prominently.",
        id = fact.handle(),
        path = fact.source_path,
        hits = fact.recall_count_30d,
    );
    match emit_dispatcher_notify(
        pool,
        tree,
        cycle_id,
        wiki_id,
        topic.clone(),
        body,
        source_ref,
    )
    .await
    {
        Ok(()) => {
            report
                .notifications_emitted
                .push((wiki_id.as_str().to_owned(), topic));
            *per_wiki += 1;
        },
        Err(e) => report
            .errors
            .push(format!("briefing_dispatcher recall_hot: {e}")),
    }
    Ok(())
}

/// Heuristic: top-level YAML mapping with `status: draft`. Returns
/// `false` on any parse failure (free-form prose, list, scalar, broken
/// YAML) — the briefing inbox prefers silence to noise on malformed
/// bodies.
fn fact_status_is_draft(body: &str) -> bool {
    let Ok(map) = serde_yaml::from_str::<serde_yaml::Mapping>(body) else {
        return false;
    };
    map.get(serde_yaml::Value::String("status".into()))
        .and_then(serde_yaml::Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("draft"))
}

/// Idempotency probe — `true` when an item with the exact
/// `(wiki_id, source_ref)` pair already exists in the dedup window.
async fn briefing_recently_emitted(
    pool: &SqlitePool,
    wiki_id: &WikiId,
    source_ref: &str,
    window: chrono::Duration,
) -> Result<bool> {
    let cutoff = (Utc::now() - window).to_rfc3339();
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wiki_briefing_items \
         WHERE wiki_id = ? AND source_ref = ? AND ts > ?",
    )
    .bind(wiki_id.as_str())
    .bind(source_ref)
    .bind(&cutoff)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Shared emit path for the Briefing dispatcher findings. Wraps the
/// notify in a WAL op so a partial crash leaves a clean breadcrumb.
async fn emit_dispatcher_notify(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    wiki_id: &WikiId,
    topic: String,
    body: String,
    source_ref: String,
) -> Result<()> {
    let op_id = wal::begin_rem_op(
        pool,
        cycle_id,
        "briefing_dispatcher_emit",
        Some(wiki_id.as_str()),
        None,
    )
    .await?;
    // Semantic routing: stale_draft + recall_hot are passive
    // observations (REM noticed something), not recommendations the
    // consumer must explicitly decide on.
    let req = NotifyRequest {
        wiki_id: wiki_id.clone(),
        topic,
        body,
        source_kind: BriefingSourceKind::Rem,
        source_ref,
        kind: Some(briefing::BriefingKind::Observation.as_str().to_owned()),
        target_cite: None,
        ts: None,
    };
    match briefing::notify_as_rem(pool, tree, req).await {
        Ok(_) => {
            wal::complete_rem_op(pool, op_id).await?;
            Ok(())
        },
        Err(e) => {
            wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
            Err(e.into())
        },
    }
}

// ---------- Backlink reciprocity detector sub-job ----------

/// For every non-smart wiki, scan its active facts for
/// `[[<wiki_id>...]]` references whose target is a smart-family
/// wiki and post one `_briefing.md` item per missing reciprocal link.
///
/// "Reciprocal" is defined narrowly for MVP: at least one active fact
/// in the smart wiki must mention the source wiki id with
/// `[[<source_id>...]]`. If none do, the inverse is missing and the
/// smart consumer is invited (via briefing) to add it the next session.
///
/// Per-wiki cap = [`RemPolicy::briefing_notify_cap`]; idempotency window
/// = [`RemPolicy::briefing_dedup_window`].
#[allow(
    clippy::too_many_lines,
    reason = "linear gather → scan → check pipeline; splitting would scatter the data dependencies that make the flow readable"
)]
async fn run_backlink_reciprocity(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    policy: &RemPolicy,
    smart_wiki_index: &SmartWikiIndex,
) -> Result<BacklinkReciprocityReport> {
    let mut report = BacklinkReciprocityReport::default();

    // Build smart wiki set + cache active-fact bodies for reverse
    // lookup. Loading bodies once per cycle keeps the inner loop O(N).
    let mut smart_wiki_ids: HashSet<String> = HashSet::new();
    let mut smart_wiki_bodies: HashMap<String, Vec<String>> = HashMap::new();
    let mut smart_wiki_id_lookup: HashMap<String, WikiId> = HashMap::new();
    for d in tree.walk()? {
        if !is_smart_wiki(smart_wiki_index, d.meta.wiki_id.as_str()) {
            continue;
        }
        let id_str = d.meta.wiki_id.as_str().to_owned();
        smart_wiki_ids.insert(id_str.clone());
        smart_wiki_id_lookup.insert(id_str.clone(), d.meta.wiki_id.clone());
        // A smart wiki's content is its indexed sections, not fact rows.
        let secs = sections::find_wiki_sections(pool, d.meta.wiki_id.as_str()).await?;
        smart_wiki_bodies.insert(id_str, secs.into_iter().map(|s| s.text).collect());
    }
    report.smart_wikis_known = smart_wiki_ids.len();
    if smart_wiki_ids.is_empty() {
        return Ok(report);
    }

    // Per-(target smart wiki) emission counter to enforce the per-wiki
    // cap regardless of how many sources reference it.
    let mut per_target: HashMap<String, usize> = HashMap::new();

    for source in tree.walk()? {
        if is_smart_wiki(smart_wiki_index, source.meta.wiki_id.as_str()) {
            continue;
        }
        let source_id = source.meta.wiki_id.as_str().to_owned();
        let source_facts =
            fact_index::find_active_in_wiki(pool, source.meta.wiki_id.as_str()).await?;
        for fact in &source_facts {
            report.source_facts_scanned += 1;
            let mut already_flagged: HashSet<String> = HashSet::new();
            for target_id in recall::extract_wikilink_wiki_ids(&fact.text) {
                if !smart_wiki_ids.contains(&target_id) {
                    continue;
                }
                if already_flagged.contains(&target_id) {
                    continue;
                }
                already_flagged.insert(target_id.clone());
                report.incoming_links += 1;

                // Check reciprocity inside the smart-wiki bodies.
                let reciprocated = smart_wiki_bodies.get(&target_id).is_some_and(|bodies| {
                    bodies.iter().any(|body| {
                        recall::extract_wikilink_wiki_ids(body)
                            .iter()
                            .any(|id| id == &source_id)
                    })
                });
                if reciprocated {
                    continue;
                }

                let counter = per_target.entry(target_id.clone()).or_insert(0);
                if *counter >= policy.briefing_notify_cap {
                    continue;
                }
                let source_ref = format!("rem:backlink_reciprocity:{source_id}");
                let Some(target_wiki) = smart_wiki_id_lookup.get(&target_id) else {
                    continue;
                };
                let dedup_skip = briefing_recently_emitted(
                    pool,
                    target_wiki,
                    &source_ref,
                    policy.briefing_dedup_window,
                )
                .await?;
                if dedup_skip {
                    report.deduplicated += 1;
                    continue;
                }
                let topic = format!("Missing back-link from `{source_id}`");
                let body = format!(
                    "Wiki `{source_id}` references this smart wiki via `[[{target_id}…]]` \
                     (see fact `{fact_id}` on page `{path}`) but no active fact in this \
                     smart wiki mentions `[[{source_id}…]]`. Add a reciprocal link the next \
                     time you edit the relevant page so the navigation stays bidirectional.",
                    fact_id = fact.fact_id.as_str(),
                    path = fact.source_path,
                );
                match emit_backlink_notify(
                    pool,
                    tree,
                    cycle_id,
                    target_wiki,
                    topic.clone(),
                    body,
                    source_ref,
                )
                .await
                {
                    Ok(()) => {
                        *counter += 1;
                        report
                            .notifications_emitted
                            .push((target_id.clone(), source_id.clone()));
                    },
                    Err(e) => report.errors.push(format!(
                        "backlink_reciprocity {source_id} -> {target_id}: {e}"
                    )),
                }
            }
        }
    }

    Ok(report)
}

/// Shared emit path for backlink-reciprocity findings.
async fn emit_backlink_notify(
    pool: &SqlitePool,
    tree: &WikiTree,
    cycle_id: &str,
    wiki_id: &WikiId,
    topic: String,
    body: String,
    source_ref: String,
) -> Result<()> {
    let op_id = wal::begin_rem_op(
        pool,
        cycle_id,
        "backlink_reciprocity_emit",
        Some(wiki_id.as_str()),
        None,
    )
    .await?;
    // Semantic routing: backlink reciprocity is a *recommended
    // action* (add the inverse link), not a passive observation. The
    // smart consumer is supposed to decide whether to act on it.
    let req = NotifyRequest {
        wiki_id: wiki_id.clone(),
        topic,
        body,
        source_kind: BriefingSourceKind::Rem,
        source_ref,
        kind: Some(briefing::BriefingKind::Reasoning.as_str().to_owned()),
        target_cite: None,
        ts: None,
    };
    match briefing::notify_as_rem(pool, tree, req).await {
        Ok(_) => {
            wal::complete_rem_op(pool, op_id).await?;
            Ok(())
        },
        Err(e) => {
            wal::fail_rem_op(pool, op_id, &format!("{e}")).await?;
            Err(e.into())
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{self, CaptureRequest};
    use crate::db;
    use crate::embedder::FakeEmbedder;
    use crate::llm::FakeLlmBackend;
    use crate::types::{FactId, Principal, WikiId};

    /// Bundle two LLMs the way the existing tests want (only hub+revisor;
    /// auto-promote and auto-apply default disabled).
    fn test_llms<'a>(hub: &'a FakeLlmBackend, revisor: &'a FakeLlmBackend) -> RemLlms<'a> {
        RemLlms {
            hub_writer: hub,
            revisor,
            auto_promote: None,
            apply: None,
            comment_applier: None,
            cronista: None,
            navigator: None,
        }
    }
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ---------- helpers ----------

    async fn setup_workdir() -> (TempDir, WikiTree, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open_or_init(dir.path()).await.expect("db open");
        std::fs::create_dir_all(dir.path().join("wikis")).unwrap();
        let tree = WikiTree::open(dir.path()).expect("open tree");
        (dir, tree, pool)
    }

    fn write_wiki(tree: &WikiTree, slug: &str, title: &str, wiki_type: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: {wiki_type}\nslug: {slug}\ntitle: {title}\nacl_default: 'user:{slug}'\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# placeholder\n").unwrap();
    }

    fn fake_embedder() -> Arc<dyn Embedder> {
        Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.1, 0.2, 0.3, 0.4],
        ))
    }

    async fn plant_fact(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        body: &str,
        owner: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from("index.md"),
            body: body.to_owned(),
            owner: Principal::User(owner.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: Vec::new(),
            dedup_threshold: Some(0.999),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        capture::wiki_capture(tree, pool, fake_embedder(), req)
            .await
            .expect("plant")
            .fact_id
    }

    /// Plant one **section** of a smart wiki's page, the smart-family
    /// counterpart of [`plant_fact`]. Smart content is content-indexed in
    /// `wiki_sections` (no capture, no ACL, no lifecycle), so the REM
    /// read-jobs that scan a smart wiki read these rows.
    ///
    /// Returns the section's stable `"<source_path>#<ord>"` handle.
    async fn plant_section(pool: &SqlitePool, wiki: &str, body: &str) -> String {
        let source_path = format!("wikis/{wiki}/index.md");
        let existing = sections::find_page_sections(pool, &source_path)
            .await
            .expect("read sections");
        let mut desired: Vec<sections::NewSection> = existing
            .iter()
            .map(|r| sections::NewSection {
                wiki_id: r.wiki_id.clone(),
                source_path: r.source_path.clone(),
                section_ord: r.section_ord,
                heading_path: r.heading_path.clone(),
                text: r.text.clone(),
                embedding: r.embedding.clone(),
            })
            .collect();
        let ord = i64::try_from(desired.len()).unwrap();
        desired.push(sections::NewSection {
            wiki_id: wiki.to_owned(),
            source_path: source_path.clone(),
            section_ord: ord,
            heading_path: None,
            text: body.to_owned(),
            embedding: vec![0.1; 8],
        });
        sections::replace_page_sections(pool, &source_path, &desired)
            .await
            .expect("plant section");
        format!("{source_path}#{ord}")
    }

    /// Plant a fact with a **caller-chosen embedding** so a test can shape
    /// the cosine geometry the refile pre-filter reads. The marker is
    /// written on disk by the capture path (the apply handler parses the
    /// page for it); only the embedding differs from [`plant_fact`].
    async fn plant_fact_with_embedding(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        body: &str,
        owner: &str,
        embedding: Vec<f32>,
    ) -> FactId {
        plant_page_fact_with_embedding(tree, pool, wiki, "index.md", body, owner, embedding).await
    }

    /// [`plant_fact_with_embedding`] on a caller-chosen page (e.g. the
    /// reserved `rules.md`, for the behaviour-rules channel guards).
    async fn plant_page_fact_with_embedding(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        body: &str,
        owner: &str,
        embedding: Vec<f32>,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: Principal::User(owner.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: Vec::new(),
            dedup_threshold: Some(0.999),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        let emb: Arc<dyn Embedder> =
            Arc::new(FakeEmbedder::with_fixed_embedding("fake-bge", embedding));
        capture::wiki_capture(tree, pool, emb, req)
            .await
            .expect("plant")
            .fact_id
    }

    use chrono::TimeZone;

    // ---------- auto-promote gate: already_promoted_for scoping (item 47-x1) ----------

    /// Insert a `wiki_promote` structure proposal with a chosen variant and
    /// source page, for the [`already_promoted_for`] gate tests.
    async fn insert_wiki_promote_proposal(
        pool: &SqlitePool,
        id: &str,
        variant: &str,
        wiki: &str,
        page: &str,
        fact_ids: &[&str],
        status: &str,
    ) {
        let context = serde_json::json!({
            "variant": variant,
            "source_wiki_id": wiki,
            "source_page": page,
            "fact_ids": fact_ids,
        })
        .to_string();
        sqlx::query(
            "INSERT INTO structure_proposals \
             (proposal_id, kind, context, questions, proposed_at, timeout_at, status) \
             VALUES (?, 'wiki_promote', ?, '[]', \
                     '2026-07-20T00:00:00Z', '2026-07-21T00:00:00Z', ?)",
        )
        .bind(id)
        .bind(context)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert proposal");
    }

    /// Regression for the inert auto-promote pass. `kind = 'wiki_promote'`
    /// is overloaded — routine lifecycle ops (`validity_close`,
    /// `fact_refile`, …) share it and stamp their `fact_id` into `context`.
    /// The old `kind`-only match let any once-touched fact veto its whole
    /// page, so `candidates_examined` was stuck at exactly 0 for every
    /// over-mass page. `already_promoted_for` must count ONLY genuine
    /// page-promotion receipts (`paragraph_to_file` / `file_to_subwiki`),
    /// scoped to the same `(source_wiki_id, source_page)`.
    #[tokio::test]
    async fn already_promoted_for_only_genuine_receipts_on_same_source_page() {
        let (_dir, _tree, pool) = setup_workdir().await;

        let f = FactId::parse("018f1234-5678-7abc-9def-000000000001").unwrap();

        // Routine lifecycle ops mentioning the fact must NOT veto the page.
        insert_wiki_promote_proposal(
            &pool,
            "p-refile",
            "fact_refile",
            "hermes1",
            "esperienze_agente.md",
            &[f.as_str()],
            "applied",
        )
        .await;
        insert_wiki_promote_proposal(
            &pool,
            "p-close",
            "validity_close",
            "hermes1",
            "esperienze_agente.md",
            &[f.as_str()],
            "applied",
        )
        .await;
        assert!(
            !already_promoted_for(&pool, &f, "hermes1", "esperienze_agente.md")
                .await
                .unwrap(),
            "lifecycle ops sharing kind='wiki_promote' must not veto"
        );

        // A genuine promote receipt, but promoted FROM another page: the
        // fact later migrated onto esperienze_agente.md — must NOT veto here.
        insert_wiki_promote_proposal(
            &pool,
            "p-para-foreign",
            "paragraph_to_file",
            "hermes1",
            "index.md",
            &[f.as_str()],
            "applied",
        )
        .await;
        assert!(
            !already_promoted_for(&pool, &f, "hermes1", "esperienze_agente.md")
                .await
                .unwrap(),
            "a receipt for another source page must not veto a migrated-in fact"
        );
        // ...but it DOES veto its own source page (genuine anti-re-promote).
        assert!(
            already_promoted_for(&pool, &f, "hermes1", "index.md")
                .await
                .unwrap(),
            "a genuine paragraph_to_file receipt must veto its own source page"
        );

        // A reverted receipt must not veto; a pending one (in flight) must.
        let g = FactId::parse("018f1234-5678-7abc-9def-000000000002").unwrap();
        insert_wiki_promote_proposal(
            &pool,
            "p-sub-reverted",
            "file_to_subwiki",
            "hermes1",
            "trio.md",
            &[g.as_str()],
            "reverted",
        )
        .await;
        assert!(
            !already_promoted_for(&pool, &g, "hermes1", "trio.md")
                .await
                .unwrap(),
            "a reverted receipt must not veto"
        );
        insert_wiki_promote_proposal(
            &pool,
            "p-sub-pending",
            "file_to_subwiki",
            "hermes1",
            "malessere.md",
            &[g.as_str()],
            "pending",
        )
        .await;
        assert!(
            already_promoted_for(&pool, &g, "hermes1", "malessere.md")
                .await
                .unwrap(),
            "a pending genuine receipt must veto (promote in flight)"
        );
    }

    // ---------- revisor: confirms similar facts and emits dedup_merge proposal ----------

    #[tokio::test]
    async fn revisor_emits_dedup_proposal_when_llm_confirms() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Two facts with a moderate-but-not-identical jaccard (in the
        // 0.45-0.85 band). Re-use most words but reorder to keep score
        // inside the window.
        let old_id = plant_fact(
            &tree,
            &pool,
            "bob",
            "bob prefers tea with milk every morning",
            "bob",
        )
        .await;
        let new_id = plant_fact(
            &tree,
            &pool,
            "bob",
            "bob likes morning tea with a splash of milk",
            "bob",
        )
        .await;

        let policy = RemPolicy {
            // Loosen the window so any moderate jaccard pair is asked about.
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert!(
            report.revisor.pairs_examined >= 1,
            "must examine the pair, got {report:?}"
        );
        assert_eq!(
            report.revisor.pairs_confirmed, 1,
            "LLM said `same: true` so must confirm exactly once"
        );
        assert_eq!(
            report.revisor.applied.len(),
            1,
            "must apply exactly one dedup_merge act-first"
        );
        // Act-first: the loser is superseded by the winner in-cycle.
        let (superseded_at, superseded_by): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT superseded_at, superseded_by FROM fact_index WHERE fact_id = ?")
                .bind(old_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(superseded_at.is_some(), "the merge landed in-cycle");
        assert_eq!(superseded_by.as_deref(), Some(new_id.as_str()));
        // The born-applied receipt carries the undo anchor + context shape.
        let proposal_id = &report.revisor.applied[0];
        let (kind, status, context, revert_token): (String, String, String, Option<String>) =
            sqlx::query_as(
                "SELECT kind, status, context, revert_token FROM structure_proposals \
                 WHERE proposal_id = ?",
            )
            .bind(proposal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "dedup_merge");
        assert_eq!(status, "applied", "born-applied receipt, no pending stage");
        assert!(revert_token.is_some(), "revert window is open");
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(
            ctx.get("winner_fact_id").and_then(|v| v.as_str()),
            Some(new_id.as_str()),
        );
        assert_eq!(
            ctx.get("loser_fact_id").and_then(|v| v.as_str()),
            Some(old_id.as_str()),
        );
        // The event stream announces the applied merge, not a proposal.
        let (event_kind,): (String,) = sqlx::query_as(
            "SELECT kind FROM wiki_events WHERE json_extract(payload,'$.proposal_id') = ? \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event_kind, "structure_applied");
        drop(dir);
    }

    // ---------- revisor: llm says `same: false` ⇒ no proposal ----------

    #[tokio::test]
    async fn revisor_does_nothing_when_llm_refuses() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, "bob", "bob likes tea", "bob").await;
        plant_fact(&tree, &pool, "bob", "bob likes coffee", "bob").await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(report.revisor.pairs_confirmed, 0);
        assert!(report.revisor.applied.is_empty());
        drop(dir);
    }

    // ---------- revisor: a "not the same" verdict is bought once ----------

    /// The memo's whole point. Before it, the confirm budget was spent
    /// re-buying verdicts: on the live workdir the revisor burned all 120
    /// confirms every night on the same pairs (156 nominable corpus-wide,
    /// 2 merges), which also meant the 36 pairs past the cap were never
    /// examined once. `pairs_examined` now means *asked the model* — a
    /// settled pair never reaches it, and never consumes the cap.
    #[tokio::test]
    async fn revisor_negative_verdict_is_not_re_asked_next_cycle() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, "bob", "bob likes tea", "bob").await;
        plant_fact(&tree, &pool, "bob", "bob likes coffee", "bob").await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");

        let first = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert!(
            first.revisor.pairs_examined > 0,
            "the pair must be judged the first time"
        );
        assert!(
            first.verdict_memo_rows > 0,
            "the negative verdict must be recorded"
        );

        let second = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(
            second.revisor.pairs_examined, 0,
            "a settled pair must not be re-asked, and must not eat the cap"
        );
        assert_eq!(
            second.verdict_memo_rows, first.verdict_memo_rows,
            "a memo hit records nothing new"
        );
        drop(dir);
    }

    /// The safety half of the same contract: the memo is keyed on what
    /// the model actually reads, so touching a fact re-opens its pair.
    /// A memo that survived an edit would silently freeze a stale verdict.
    #[tokio::test]
    async fn revisor_memo_reopens_when_a_fact_text_changes() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let a = plant_fact(&tree, &pool, "bob", "bob likes tea", "bob").await;
        plant_fact(&tree, &pool, "bob", "bob likes coffee", "bob").await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let llms = test_llms(&hub_llm, &rev_llm);

        run_cycle(&pool, &tree, fake_embedder(), &llms, &policy)
            .await
            .unwrap();

        // The user corrects one of the two claims.
        sqlx::query("UPDATE fact_index SET text = ? WHERE fact_id = ?")
            .bind("bob likes tea in the evening")
            .bind(a.as_str())
            .execute(&pool)
            .await
            .unwrap();

        let after = run_cycle(&pool, &tree, fake_embedder(), &llms, &policy)
            .await
            .unwrap();
        assert!(
            after.revisor.pairs_examined > 0,
            "an edited fact must re-open its pair"
        );
        drop(dir);
    }

    // ---------- hub_writer: regenerates index.md when triggers met ----------

    #[tokio::test]
    async fn hub_writer_rewrites_index_for_qualifying_wiki() {
        let (dir, mut tree, pool) = setup_workdir().await;
        // Parent has children + an active fact ⇒ regen required.
        let parent = "alice";
        let child = "alice-acmecorp";
        // Build parent wiki with one child entry in meta.
        let parent_dir = tree.wikis_dir().join(parent);
        std::fs::create_dir_all(&parent_dir).unwrap();
        let fm = format!(
            "---\nwiki_id: {parent}\nwiki_type: wiki-user\nslug: {parent}\ntitle: Alice\nacl_default: 'user:alice'\nchildren:\n  - wiki_id: {child}\n    slug: acmecorp\n    title: ACME Corp\n    wiki_type: wiki-tech\n---\n"
        );
        std::fs::write(parent_dir.join("_meta.md"), fm).unwrap();
        std::fs::write(parent_dir.join("index.md"), "# old\n").unwrap();
        // Child wiki.
        write_wiki(&tree, child, "ACME Corp", "wiki-tech");
        tree = WikiTree::open(dir.path()).unwrap();
        // Plant an active fact in the parent so count_active > 0.
        plant_fact(&tree, &pool, parent, "alice landed in mwe-mcp", "alice").await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# regenerated by hub writer\n\nfresh body\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert!(
            report.hub_writer.regenerated.iter().any(|w| w == parent),
            "parent wiki must be in regenerated list, got {report:?}"
        );
        let index = std::fs::read_to_string(parent_dir.join("index.md")).unwrap();
        assert!(index.contains("regenerated by hub writer"));
        drop(dir);
    }

    // ---------- regenerate_index: feeds the 20 MOST-RECENT facts, newest first ----------

    #[tokio::test]
    async fn regenerate_index_snippet_takes_most_recent_facts_newest_first() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();

        // Plant 22 facts oldest→newest (fact-00 .. fact-21). Dedup is
        // disabled (threshold 1.01) so the shared fake embedding does not
        // collapse them; created_at is then backdated monotonically so the
        // ASC query order is deterministic.
        for i in 0..22u32 {
            let req = CaptureRequest {
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: PathBuf::from("index.md"),
                body: format!("fact-{i:02}"),
                owner: Principal::User("alice".to_owned()),
                allow: Vec::new(),
                sender: None,
                fact_type: None,
                topics: Vec::new(),
                dedup_threshold: Some(1.01),
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            };
            let id = capture::wiki_capture(&tree, &pool, fake_embedder(), req)
                .await
                .expect("plant")
                .fact_id;
            // Minutes 00..21 sort lexicographically the same way they sort
            // chronologically — fact-00 oldest, fact-21 newest.
            let created = format!("2026-01-01T00:{i:02}:00Z");
            sqlx::query("UPDATE fact_index SET created_at = ? WHERE fact_id = ?")
                .bind(&created)
                .bind(id.as_str())
                .execute(&pool)
                .await
                .unwrap();
        }

        let d = tree
            .walk()
            .unwrap()
            .into_iter()
            .find(|d| d.meta.wiki_id.as_str() == "alice")
            .expect("alice discovered");
        let hub_llm = FakeLlmBackend::new("hub", "# regenerated\n");
        regenerate_index(&pool, &tree, &d, &hub_llm).await.unwrap();

        let prompt = hub_llm.last_prompt().expect("hub writer was called");
        // The two oldest facts are dropped; the newest is kept.
        assert!(
            prompt.contains("fact-21"),
            "newest fact must be in the snippet, prompt: {prompt}"
        );
        assert!(
            !prompt.contains("fact-00") && !prompt.contains("fact-01"),
            "the 2 oldest facts must be dropped (only the 20 most recent survive), prompt: {prompt}"
        );
        // Most-recent first: the newest fact leads the oldest surviving one.
        let pos_newest = prompt.find("fact-21").unwrap();
        let pos_oldest_surviving = prompt.find("fact-02").unwrap();
        assert!(
            pos_newest < pos_oldest_surviving,
            "snippet must be most-recent first (fact-21 before fact-02), prompt: {prompt}"
        );
        drop(dir);
    }

    // ---------- hub_writer: skips wikis without children or active facts ----------

    #[tokio::test]
    async fn hub_writer_skips_wiki_without_children() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "lonely", "Lonely", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, "lonely", "lonely fact", "lonely").await;
        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# never written\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert!(report.hub_writer.regenerated.is_empty());
        assert!(report.hub_writer.skipped.iter().any(|w| w == "lonely"));
        // index.md untouched.
        let index =
            std::fs::read_to_string(tree.wikis_dir().join("lonely").join("index.md")).unwrap();
        assert!(index.contains("placeholder"));
        drop(dir);
    }

    // ---------- policy caps ----------

    #[tokio::test]
    async fn hub_writer_cap_bounds_regeneration_count() {
        let (dir, mut tree, pool) = setup_workdir().await;
        // Two qualifying parents, but cap = 1 ⇒ only one regen.
        for parent in ["p1", "p2"] {
            let pdir = tree.wikis_dir().join(parent);
            std::fs::create_dir_all(&pdir).unwrap();
            let fm = format!(
                "---\nwiki_id: {parent}\nwiki_type: wiki-user\nslug: {parent}\ntitle: P\nacl_default: 'user:{parent}'\nchildren:\n  - wiki_id: {parent}-c\n    slug: c\n    title: C\n    wiki_type: wiki-tech\n---\n"
            );
            std::fs::write(pdir.join("_meta.md"), fm).unwrap();
            std::fs::write(pdir.join("index.md"), "# old\n").unwrap();
            write_wiki(&tree, &format!("{parent}-c"), "C", "wiki-tech");
        }
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, "p1", "f1", "p1").await;
        plant_fact(&tree, &pool, "p2", "f2", "p2").await;

        let policy = RemPolicy {
            hub_writer_cap: 1,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# regen\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(report.hub_writer.regenerated.len(), 1);
        drop(dir);
    }

    // ---------- journaling ----------

    #[tokio::test]
    async fn rem_cycle_journals_each_action_into_rem_ops_log() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Two similar-but-not-identical facts trigger a revisor dedup
        // apply, which journals a `dedup_merge_apply` op into rem_ops_log.
        plant_fact(
            &tree,
            &pool,
            "bob",
            "bob prefers tea with milk every morning",
            "bob",
        )
        .await;
        plant_fact(
            &tree,
            &pool,
            "bob",
            "bob likes morning tea with a splash of milk",
            "bob",
        )
        .await;
        let policy = RemPolicy {
            cycle_id: Some("cycle-test-1".into()),
            now: Some(chrono::Utc.with_ymd_and_hms(2026, 5, 18, 12, 0, 0).unwrap()),
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM rem_ops_log WHERE cycle_id = ? AND status = 'done'",
        )
        .bind("cycle-test-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(count >= 1, "at least one journaled `done` op expected");
        drop(dir);
    }

    // ---------- auto-promote ----------

    /// Bump a fact's `recall_count_30d` so it clears the deterministic
    /// filter without going through the recall pipeline.
    async fn bump_recall(pool: &SqlitePool, fact_id: &FactId, hits: i64) {
        sqlx::query("UPDATE fact_index SET recall_count_30d = ? WHERE fact_id = ?")
            .bind(hits)
            .bind(fact_id.as_str())
            .execute(pool)
            .await
            .unwrap();
    }

    /// Plant `n` distinct short facts on `wiki`'s `index.md` so the page
    /// accumulates mass. Bodies are distinct topics so the jaccard
    /// pre-pass never flags them as dedup siblings.
    async fn plant_distinct(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        n: usize,
        owner: &str,
    ) -> Vec<FactId> {
        const TOPICS: [&str; 8] = [
            "alice hikes in the dolomites every summer",
            "alice works as a structural engineer in turin",
            "alice drives a red dacia sandero",
            "alice studied architecture in milan",
            "alice keeps bees on her balcony",
            "alice plays the cello on sundays",
            "alice volunteers at the river cleanup",
            "alice collects vintage maps of liguria",
        ];
        let mut out = Vec::with_capacity(n);
        for t in TOPICS.iter().take(n) {
            out.push(plant_fact(tree, pool, wiki, t, owner).await);
        }
        out
    }

    /// REM policy with a low page-mass bar so a handful of planted facts
    /// trips the deterministic pre-filter in tests (production default
    /// is 8).
    fn mass_policy() -> RemPolicy {
        RemPolicy {
            auto_promote_min_page_facts: 3,
            ..RemPolicy::default()
        }
    }

    /// Like [`plant_fact`] but onto a named page of the wiki.
    async fn plant_fact_on_page(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        body: &str,
        owner: &str,
    ) -> FactId {
        plant_fact_with_embedder(tree, pool, fake_embedder(), wiki, page, body, owner).await
    }

    /// [`plant_fact_on_page`] with a caller-chosen embedder, for tests
    /// that need per-fact vectors (the revisor's cosine channel).
    async fn plant_fact_with_embedder(
        tree: &WikiTree,
        pool: &SqlitePool,
        embedder: Arc<dyn Embedder>,
        wiki: &str,
        page: &str,
        body: &str,
        owner: &str,
    ) -> FactId {
        let req = CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: Principal::User(owner.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: Vec::new(),
            dedup_threshold: Some(0.999),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        capture::wiki_capture(tree, pool, embedder, req)
            .await
            .expect("plant")
            .fact_id
    }

    /// A minimal persisted plan holding the given concept-leaf pages of
    /// `alice` (facts read back from `fact_index`).
    async fn save_leaf_plan(tree: &WikiTree, pool: &SqlitePool, pages: &[(&str, &[FactId])]) {
        let mut map = std::collections::BTreeMap::new();
        for (slug, fids) in pages {
            let mut facts = Vec::new();
            for fid in *fids {
                let row = fact_index::find_by_id(pool, fid).await.unwrap().unwrap();
                facts.push(crate::planner::FactForPage::from_row(&row));
            }
            // The page lives where its facts live (alice for a factless
            // fixture page).
            let wiki_id = facts.first().map_or_else(
                || "alice".to_owned(),
                |f: &crate::planner::FactForPage| f.source_wiki_id.clone(),
            );
            map.insert(
                (*slug).to_owned(),
                PagePlan {
                    slug: (*slug).to_owned(),
                    title: (*slug).to_owned(),
                    description: format!("about {slug}"),
                    style: None,
                    page_type: PageType::ConceptLeaf,
                    owner_scope: None,
                    parent_hub: None,
                    child_leaves: Vec::new(),
                    primary_facts: facts,
                    outgoing_links: Vec::new(),
                    incoming_links: Vec::new(),
                    wiki_id,
                    page_path: format!("{slug}.md"),
                },
            );
        }
        let order: Vec<String> = map.keys().cloned().collect();
        let plan = CompilationPlan {
            pages: map,
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: order.clone(),
            generated_at: "t".to_owned(),
            fact_count: order.len(),
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        crate::planner::save_plan(tree, &plan).unwrap();
    }

    #[tokio::test]
    async fn page_merge_consolidates_an_llm_confirmed_pair_act_first() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Two near-synonym concept pages with rendered facts.
        let f1 = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "viaggi.md",
            "the paris trip leaves on july 3",
            "alice",
        )
        .await;
        let f2 = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "viaggi_parigi.md",
            "the hotel in paris is already booked",
            "alice",
        )
        .await;
        let f3 = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "viaggi_parigi.md",
            "the louvre tickets are bought",
            "alice",
        )
        .await;
        save_leaf_plan(
            &tree,
            &pool,
            &[
                ("viaggi", std::slice::from_ref(&f1)),
                ("viaggi_parigi", &[f2.clone(), f3.clone()]),
            ],
        )
        .await;

        // The confirmer says: same concept, `viaggi` survives.
        let merge_llm = FakeLlmBackend::new(
            "rev",
            "{\"merge\": true, \"survivor\": \"viaggi\", \"reason\": \"same trip\"}",
        );
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_page_merge(
            &pool,
            &tree,
            &merge_llm,
            "cycle-t",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("merge sub-job");
        assert_eq!(report.candidates_examined, 1);
        assert_eq!(report.candidates_confirmed, 1);
        assert_eq!(report.applied.len(), 1, "errors: {:?}", report.errors);

        // Husk deleted; survivor carries every marker; rows repointed.
        assert!(!tree.wikis_dir().join("alice/viaggi_parigi.md").exists());
        let survivor = std::fs::read_to_string(tree.wikis_dir().join("alice/viaggi.md")).unwrap();
        for f in [&f1, &f2, &f3] {
            assert!(survivor.contains(&format!("f={f}")), "marker {f} present");
        }
        let row = fact_index::find_by_id(&pool, &f2).await.unwrap().unwrap();
        assert_eq!(row.source_path, "wikis/alice/viaggi.md");

        // Born-applied receipt with an undo token (the dashboard's revert).
        let (status, token): (String, Option<String>) = sqlx::query_as(
            "SELECT status, revert_token FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&report.applied[0])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        assert!(token.is_some(), "undo token minted");

        // The structure_applied notice carries the merge for the dashboard.
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_events WHERE kind = 'structure_applied' AND payload LIKE '%page_merge%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "structure_applied notice emitted");

        // Persisted plan re-homed: husk gone, survivor holds all the facts
        // and is parked for the next compile's weave.
        let plan = crate::planner::load_previous_plan(&tree).unwrap().unwrap();
        assert!(!plan.pages.contains_key("viaggi_parigi"));
        assert_eq!(plan.pages["viaggi"].primary_facts.len(), 3);
        assert!(plan.force_dirty.contains(&"viaggi".to_owned()));

        // The pair is now judged: also the standing veto after a revert.
        assert!(
            merge_already_judged(&pool, "viaggi.md", "viaggi_parigi.md")
                .await
                .unwrap()
        );
        drop(dir);
    }

    #[tokio::test]
    async fn page_merge_respects_the_confirmers_refusal() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let f1 = plant_fact_on_page(&tree, &pool, "alice", "viaggi.md", "trip note", "alice").await;
        let f2 = plant_fact_on_page(
            &tree,
            &pool,
            "alice",
            "viaggi_lavoro.md",
            "work travel policy",
            "alice",
        )
        .await;
        save_leaf_plan(
            &tree,
            &pool,
            &[("viaggi", &[f1] as &[FactId]), ("viaggi_lavoro", &[f2])],
        )
        .await;

        let merge_llm = FakeLlmBackend::new("rev", "{\"merge\": false}");
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_page_merge(
            &pool,
            &tree,
            &merge_llm,
            "cycle-t",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("merge sub-job");
        assert_eq!(report.candidates_examined, 1, "the pair was nominated");
        assert_eq!(report.candidates_confirmed, 0);
        assert!(report.applied.is_empty());
        assert!(tree.wikis_dir().join("alice/viaggi.md").exists());
        assert!(tree.wikis_dir().join("alice/viaggi_lavoro.md").exists());
        let plan = crate::planner::load_previous_plan(&tree).unwrap().unwrap();
        assert!(plan.pages.contains_key("viaggi_lavoro"), "plan untouched");
        drop(dir);
    }

    /// A fact-bearing concept leaf for the merge-nomination fixtures.
    fn kin_leaf(slug: &str, wiki: &str, n_facts: usize) -> PagePlan {
        let facts = (0..n_facts)
            .map(|i| crate::planner::FactForPage {
                authored_refs: Vec::new(),
                fact_id: FactId::parse(&format!("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5d{i:02x}"))
                    .unwrap(),
                text: format!("fact {i}"),
                fact_type: None,
                owner: "user:alice".parse().unwrap(),
                allow: Vec::new(),
                sender: None,
                source_wiki_id: wiki.to_owned(),
                valid_from: None,
                valid_to: None,
                decay_reason: None,
                successor_fact_id: None,
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
            })
            .collect();
        PagePlan {
            slug: slug.to_owned(),
            title: slug.to_owned(),
            description: String::new(),
            style: None,
            page_type: PageType::ConceptLeaf,
            owner_scope: None,
            parent_hub: None,
            child_leaves: Vec::new(),
            primary_facts: facts,
            outgoing_links: Vec::new(),
            incoming_links: Vec::new(),
            wiki_id: wiki.to_owned(),
            page_path: format!("{slug}.md"),
        }
    }

    #[test]
    fn merge_candidates_nominate_same_family_kin_leaves_only() {
        let leaf = kin_leaf;
        let mut pages = std::collections::BTreeMap::new();
        // Kin pair in the same wiki → nominated.
        pages.insert("viaggi".to_owned(), leaf("viaggi", "alice", 1));
        pages.insert(
            "viaggi_parigi_2026".to_owned(),
            leaf("viaggi_parigi_2026", "alice", 2),
        );
        // Long-common-prefix pair (no shared token) → nominated.
        pages.insert("presenze".to_owned(), leaf("presenze", "alice", 1));
        pages.insert("presenza".to_owned(), leaf("presenza", "alice", 1));
        // Kin names across UNRELATED wikis → never nominated.
        pages.insert("spesa".to_owned(), leaf("spesa", "alice", 1));
        pages.insert("spesa_casa".to_owned(), leaf("spesa_casa", "bob", 1));
        // Kin names across the SAME family line (parent ↔ sub-wiki) →
        // nominated (leva-2).
        pages.insert("dossier".to_owned(), leaf("dossier", "famiglia", 1));
        pages.insert(
            "dossier_bruno".to_owned(),
            leaf("dossier_bruno", "famiglia-bruno", 2),
        );
        // A factless leaf is not a merge candidate.
        pages.insert("viaggi_vuota".to_owned(), leaf("viaggi_vuota", "alice", 0));
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        // alice and bob are their own families; famiglia-bruno is
        // famiglia's sub-wiki (one family line).
        let family: BTreeMap<String, String> = [
            ("alice", "alice"),
            ("bob", "bob"),
            ("famiglia", "famiglia"),
            ("famiglia-bruno", "famiglia"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
        let got = merge_candidates(&plan, &[], 10, &family);
        let pairs: Vec<(&str, &str)> = got
            .iter()
            .map(|(a, b, _)| (a.as_str(), b.as_str()))
            .collect();
        assert!(
            pairs.contains(&("viaggi", "viaggi_parigi_2026")),
            "{pairs:?}"
        );
        assert!(pairs.contains(&("presenza", "presenze")), "{pairs:?}");
        assert!(
            pairs.contains(&("dossier", "dossier_bruno")),
            "parent↔sub-wiki kin nominate within the family line: {pairs:?}"
        );
        assert!(
            !pairs
                .iter()
                .any(|(a, b)| *a == "spesa" || *b == "spesa_casa"),
            "unrelated cross-wiki kin must not be nominated: {pairs:?}"
        );
        assert!(
            !pairs
                .iter()
                .any(|(a, b)| *a == "viaggi_vuota" || *b == "viaggi_vuota"),
            "factless pages are not candidates: {pairs:?}"
        );
        // The cap bounds the confirmation spend.
        assert_eq!(merge_candidates(&plan, &[], 1, &family).len(), 1);
    }

    #[tokio::test]
    async fn auto_promote_is_noop_without_rem_promotions_llm() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Mass is irrelevant — the sub-job short-circuits before walking
        // when no rem_promotions LLM is wired.
        plant_distinct(&tree, &pool, "alice", 3, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &mass_policy(),
        )
        .await
        .unwrap();
        assert!(report.auto_promote.applied.is_empty());
        assert_eq!(
            report.auto_promote.disabled_reason.as_deref(),
            Some("no rem_promotions LLM wired"),
        );
        drop(dir);
    }

    /// Bundle the `run_cycle` LLMs for the split tests: the promote slot
    /// returns `response` verbatim.
    fn split_llms<'a>(
        hub: &'a FakeLlmBackend,
        rev: &'a FakeLlmBackend,
        promote: &'a FakeLlmBackend,
    ) -> RemLlms<'a> {
        RemLlms {
            hub_writer: hub,
            revisor: rev,
            auto_promote: Some(promote),
            apply: None,
            comment_applier: None,
            cronista: None,
            navigator: None,
        }
    }

    #[tokio::test]
    async fn auto_promote_splits_page_directly() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // One page over the mass floor; one hot fact. The LLM sees the
        // whole page (no per-fact recall gate) and names the hot fact.
        let facts = plant_distinct(&tree, &pool, "alice", 3, "alice").await;
        bump_recall(&pool, &facts[0], 7).await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The verdict names the real fact id — built after planting.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            format!(
                "{{\"split\": true, \"fact_ids\": [\"{}\"], \"target_page\": \"acme-corp.md\"}}",
                facts[0].as_str()
            ),
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&hub_llm, &rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();

        // One page examined whole, one split applied.
        assert_eq!(report.auto_promote.candidates_examined, 1);
        assert_eq!(report.auto_promote.candidates_promoted, 1);
        assert_eq!(report.auto_promote.applied.len(), 1);
        let proposal_id = &report.auto_promote.applied[0];
        // Act-first: the receipt is born `applied` with an undo token —
        // there is no `pending` stage and no approval step.
        let (kind, status, context, token): (String, String, String, Option<String>) =
            sqlx::query_as(
                "SELECT kind, status, context, revert_token FROM structure_proposals \
                 WHERE proposal_id = ?",
            )
            .bind(proposal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "wiki_promote");
        assert_eq!(status, "applied");
        assert!(token.is_some(), "born-applied receipt must carry a token");
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "paragraph_to_file");
        assert_eq!(ctx["source_wiki_id"], "alice");
        // The stored source_page is wiki-relative, not
        // `wikis/alice/index.md`.
        assert_eq!(ctx["source_page"], "index.md");
        assert_eq!(ctx["recommended_target_page"], "acme_corp.md");
        // The hint records page mass, not a word count.
        assert_eq!(ctx["trigger_page_facts"], 3);
        let fact_ids = ctx["fact_ids"].as_array().unwrap();
        assert_eq!(fact_ids.len(), 1);
        assert_eq!(fact_ids[0].as_str(), Some(facts[0].as_str()));

        // The fact has already moved — in-cycle, with no apply step.
        let row = fact_index::find_by_id(&pool, &facts[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.source_path, "wikis/alice/acme_corp.md");
        let target =
            std::fs::read_to_string(tree.wikis_dir().join("alice").join("acme_corp.md")).unwrap();
        assert!(target.contains(&format!("f={}", facts[0])), "{target}");

        // Exactly one notice, naming the affected user, the moved facts
        // and the undo path.
        let payloads: Vec<(String,)> =
            sqlx::query_as("SELECT payload FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(payloads.len(), 1, "exactly one notice per applied split");
        let notice: serde_json::Value = serde_json::from_str(&payloads[0].0).unwrap();
        assert_eq!(notice["proposal_id"].as_str(), Some(proposal_id.as_str()));
        assert_eq!(notice["recipient_id"], "user:alice");
        assert_eq!(notice["moved_facts"][0].as_str(), Some(facts[0].as_str()));
        assert_eq!(
            notice["dashboard_path"].as_str(),
            Some(format!("/dashboard/proposals/{proposal_id}/open-in-chat").as_str()),
        );
        drop(dir);
    }

    /// v2.1 of the prompt presents facts as positional handles (`n1`,
    /// `n2`, …) instead of UUIDs: the model never reasons over an id, it
    /// only echoes one back, and an id costs ~18 tokens of noise per fact
    /// on the strong slot. A verdict naming handles must apply exactly
    /// like one naming ids — `auto_promote_splits_page_directly` above
    /// still answers with a raw id and must keep passing, which is the
    /// backward-compatibility half of the same contract.
    #[tokio::test]
    async fn auto_promote_accepts_positional_handles() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let facts = plant_distinct(&tree, &pool, "alice", 3, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // Bracketed, as the prompt renders them — the resolver strips them.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"split\": true, \"fact_ids\": [\"[n1]\"], \"target_page\": \"acme-corp.md\"}",
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&hub_llm, &rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();

        assert_eq!(
            report.auto_promote.candidates_promoted, 1,
            "a handle must resolve, not read as a hallucinated name"
        );
        assert_eq!(report.auto_promote.applied.len(), 1);
        let context: String =
            sqlx::query_scalar("SELECT context FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.auto_promote.applied[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        let moved = ctx["fact_ids"].as_array().unwrap();
        assert_eq!(moved.len(), 1, "a proper subset moves, not the whole page");
        let moved_id = moved[0].as_str().unwrap();
        assert!(
            facts.iter().any(|f| f.as_str() == moved_id),
            "the handle must resolve to a fact that was on the page"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn auto_promote_skips_thin_pages() {
        // The mass floor is the only deterministic gate — a resource
        // pre-filter. A page under it never reaches the LLM.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_distinct(&tree, &pool, "alice", 2, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"split\": true, \"fact_ids\": [\"whatever\"], \"target_page\": \"x.md\"}",
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&hub_llm, &rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();
        assert_eq!(report.auto_promote.candidates_examined, 0);
        assert!(report.auto_promote.applied.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn auto_promote_respects_llm_refusal() {
        // A page of evenly-small facts yields nothing: the LLM reads
        // the whole page and declines, and nothing is applied.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_distinct(&tree, &pool, "alice", 3, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new("rp", "{\"split\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&hub_llm, &rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();
        assert_eq!(report.auto_promote.candidates_examined, 1);
        assert_eq!(report.auto_promote.candidates_promoted, 0);
        assert!(report.auto_promote.applied.is_empty());
        assert!(report.auto_promote.errors.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn auto_promote_rejects_full_page_split() {
        // Naming every fact on the page is a rename, not a split — the
        // proper-subset guard refuses and nothing is applied.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let facts = plant_distinct(&tree, &pool, "alice", 3, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let all_ids = facts
            .iter()
            .map(|f| format!("\"{}\"", f.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            format!("{{\"split\": true, \"fact_ids\": [{all_ids}], \"target_page\": \"all.md\"}}"),
        );
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &split_llms(&hub_llm, &rev_llm, &promote_llm),
            &mass_policy(),
        )
        .await
        .unwrap();
        assert_eq!(report.auto_promote.candidates_examined, 1);
        assert_eq!(report.auto_promote.candidates_promoted, 0);
        assert!(report.auto_promote.applied.is_empty());
        assert_eq!(report.auto_promote.errors.len(), 1, "guard must log");
        drop(dir);
    }

    #[tokio::test]
    async fn auto_promote_skips_already_promoted_pages() {
        // After a split, the receipt covers the moved fact: the page it
        // landed on is left alone by later cycles even if it has mass.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let facts = plant_distinct(&tree, &pool, "alice", 3, "alice").await;
        bump_recall(&pool, &facts[0], 7).await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            format!(
                "{{\"split\": true, \"fact_ids\": [\"{}\"], \"target_page\": \"acme-corp.md\"}}",
                facts[0].as_str()
            ),
        );
        let llms = split_llms(&hub_llm, &rev_llm, &promote_llm);
        let r1 = run_cycle(&pool, &tree, fake_embedder(), &llms, &mass_policy())
            .await
            .unwrap();
        assert_eq!(r1.auto_promote.applied.len(), 1);
        // Cycle 2: index.md dropped under the floor (2 facts) and the
        // moved fact's new page is covered by the receipt — nothing to
        // do.
        let r2 = run_cycle(&pool, &tree, fake_embedder(), &llms, &mass_policy())
            .await
            .unwrap();
        assert!(
            r2.auto_promote.applied.is_empty(),
            "second cycle must not re-split: {r2:?}",
        );
        drop(dir);
    }

    // ---------- page-group → wiki regrouping ----------

    /// Plant `n` distinct facts on a specific (non-index) page so the
    /// regrouping pass — which excludes `index.md` — has real topic
    /// pages to work with. Bodies are namespaced by page so two pages
    /// never collide on the dedup threshold.
    async fn plant_on_page(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        n: usize,
        owner: &str,
    ) -> Vec<FactId> {
        const TOPICS: [&str; 8] = [
            "trim the hedge in early spring before the birds nest",
            "tomatoes want full sun and a deep weekly soak",
            "basil planted next to tomatoes keeps the aphids down",
            "the compost bin needs turning every two weeks",
            "lavender by the south wall draws the bees",
            "mulch the beds before the first frost",
            "the fig tree fruits twice if you prune the suckers",
            "rosemary survives the winter in a sheltered pot",
        ];
        let mut out = Vec::with_capacity(n);
        for t in TOPICS.iter().take(n) {
            let req = CaptureRequest {
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse(wiki).unwrap(),
                page: PathBuf::from(page),
                body: format!("{page}: {t}"),
                owner: Principal::User(owner.to_owned()),
                allow: Vec::new(),
                sender: None,
                fact_type: None,
                topics: Vec::new(),
                dedup_threshold: Some(0.999),
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            };
            out.push(
                capture::wiki_capture(tree, pool, fake_embedder(), req)
                    .await
                    .expect("plant")
                    .fact_id,
            );
        }
        out
    }

    /// Materialise an existing sub-wiki under `parent` so the move
    /// branch has somewhere to file pages into.
    fn write_subwiki(tree: &WikiTree, parent: &str, slug: &str, title: &str) {
        let dir = tree.wikis_dir().join(parent).join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {parent}-{slug}\nwiki_type: wiki-tech\nparent_wiki_id: {parent}\n\
             slug: {slug}\ntitle: {title}\nacl_default: 'user:{parent}'\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# placeholder\n").unwrap();
    }

    /// REM policy with a low birth floor (production default is 9) so a
    /// handful of planted pages can found a wiki, and the paragraph bar
    /// left at its default so only the grouping pass fires.
    fn grouping_policy() -> RemPolicy {
        RemPolicy {
            auto_promote_group_min_pages: 3,
            ..RemPolicy::default()
        }
    }

    fn grouping_llms<'a>(
        hub: &'a FakeLlmBackend,
        rev: &'a FakeLlmBackend,
        promote: &'a FakeLlmBackend,
    ) -> RemLlms<'a> {
        RemLlms {
            hub_writer: hub,
            revisor: rev,
            auto_promote: Some(promote),
            apply: None,
            comment_applier: None,
            cronista: None,
            navigator: None,
        }
    }

    #[tokio::test]
    async fn page_grouping_founds_a_wiki_from_a_group_of_pages() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Three sibling pages that are one subject. The trigger is how
        // many pages there are, never one page's mass — two facts each
        // is plenty.
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"style\":\"prosa\",\"description\":\"Everything about the garden\",\
             \"pages\":[\"orto.md\",\"potatura.md\",\"compost.md\"]}]}",
        );
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_wikis_examined, 1);
        assert_eq!(report.auto_promote.grouping_groups_applied, 1);
        assert_eq!(report.auto_promote.applied.len(), 1);

        // The wiki is born holding all three pages under their own
        // names — never a single page, and never one fat index.
        let new_dir = tree.wikis_dir().join("alice").join("giardino");
        assert!(new_dir.join("_meta.md").exists(), "sub-wiki must exist");
        for page in ["orto.md", "potatura.md", "compost.md"] {
            assert!(new_dir.join(page).exists(), "{page} must have moved in");
            assert!(
                !tree.wikis_dir().join("alice").join(page).exists(),
                "{page} must be gone from the parent",
            );
        }
        // Its front page is a bare stub: the compiler owns the real one.
        let index = std::fs::read_to_string(new_dir.join("index.md")).unwrap();
        assert!(index.contains("Giardino"), "index stub carries the title");
        assert!(!index.contains("{{f="), "no facts land on the stub index");

        let rows = fact_index::find_active_in_wiki(&pool, "alice-giardino")
            .await
            .unwrap();
        assert_eq!(rows.len(), 6, "every fact followed its page");

        let proposal_id = &report.auto_promote.applied[0];
        let (kind, status, context): (String, String, String) = sqlx::query_as(
            "SELECT kind, status, context FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "wiki_promote");
        assert_eq!(status, "applied", "act-first: born applied");
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "pages_to_subwiki");
        assert_eq!(ctx["source_wiki_id"], "alice");
        assert_eq!(ctx["group_pages"], 3);
        assert_eq!(ctx["new_wiki_style"], "prosa");
        assert_eq!(ctx["new_wiki_description"], "Everything about the garden");

        let (payload,): (String,) =
            sqlx::query_as("SELECT payload FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let notice: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(notice["variant"], "pages_to_subwiki");
        assert_eq!(notice["new_wiki_id"], "alice-giardino");
        assert_eq!(notice["recipient_id"], "user:alice");
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_refuses_a_group_under_the_birth_floor() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // The model cut a two-page group; the floor is three. A wiki is
        // never born for a pair — they stay where they are.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"pages\":[\"orto.md\",\"potatura.md\"]}]}",
        );
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_wikis_examined, 1);
        assert_eq!(report.auto_promote.grouping_groups_applied, 0);
        assert!(report.auto_promote.applied.is_empty());
        assert!(
            !tree.wikis_dir().join("alice").join("giardino").exists(),
            "no wiki may be born under the floor",
        );
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_files_into_an_existing_subwiki_without_a_floor() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_subwiki(&tree, "alice", "giardino", "Giardino");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_on_page(&tree, &pool, "alice", "orto.md", 2, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // One stray page whose subject already has a home. No floor
        // applies — the home exists, so there is nothing to justify.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"move\",\"target\":\"alice-giardino\",\
             \"pages\":[\"orto.md\"]}]}",
        );
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_groups_applied, 1);
        let moved = tree
            .wikis_dir()
            .join("alice")
            .join("giardino")
            .join("orto.md");
        assert!(moved.exists(), "the page moved into the existing wiki");
        assert!(!tree.wikis_dir().join("alice").join("orto.md").exists());
        let rows = fact_index::find_active_in_wiki(&pool, "alice-giardino")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "facts followed the page");

        let proposal_id = &report.auto_promote.applied[0];
        let (context,): (String,) =
            sqlx::query_as("SELECT context FROM structure_proposals WHERE proposal_id = ?")
                .bind(proposal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let ctx: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(ctx["variant"], "pages_move_wiki");
        assert_eq!(ctx["target_wiki_id"], "alice-giardino");
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_skips_a_wiki_that_can_neither_found_nor_file() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Two pages, floor three, and no sub-wiki to file into: neither
        // move is reachable, so the LLM must never be asked.
        for page in ["orto.md", "potatura.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\
             \"pages\":[\"orto.md\",\"potatura.md\"]}]}",
        );
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(
            report.auto_promote.grouping_wikis_examined, 0,
            "the pre-filter must spend no LLM call",
        );
        assert!(report.auto_promote.applied.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_never_carries_the_index_page() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }
        plant_distinct(&tree, &pool, "alice", 2, "alice").await;

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // index.md is the wiki's own front page: it is not a candidate,
        // so naming it invalidates the whole group rather than
        // decapitating the parent.
        let promote_llm = FakeLlmBackend::new(
            "rp",
            "{\"groups\":[{\"action\":\"create\",\"slug\":\"giardino\",\"title\":\"Giardino\",\
             \"pages\":[\"orto.md\",\"potatura.md\",\"index.md\"]}]}",
        );
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_groups_applied, 0);
        assert!(report.auto_promote.applied.is_empty());
        assert!(
            tree.wikis_dir().join("alice").join("index.md").exists(),
            "the parent keeps its front page",
        );
        drop(dir);
    }

    #[tokio::test]
    async fn page_grouping_respects_an_empty_verdict() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        for page in ["orto.md", "potatura.md", "compost.md"] {
            plant_on_page(&tree, &pool, "alice", page, 2, "alice").await;
        }

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        // A tidy wiki: the model finds nothing worth grouping.
        let promote_llm = FakeLlmBackend::new("rp", "{\"groups\":[]}");
        let llms = grouping_llms(&hub_llm, &rev_llm, &promote_llm);
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &grouping_policy())
            .await
            .unwrap();

        assert_eq!(report.auto_promote.grouping_wikis_examined, 1);
        assert_eq!(report.auto_promote.grouping_groups_applied, 0);
        assert!(report.auto_promote.applied.is_empty());
        // The "nothing to group" verdict is memoized, so an unchanged
        // inventory never re-buys it.
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM rem_verdicts WHERE kind = 'page_grouping'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1, "the empty verdict is settled");
        drop(dir);
    }

    // ---------- archive detector ----------

    #[tokio::test]
    async fn archive_detector_emits_for_stale_pages() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let stale = plant_fact(&tree, &pool, "alice", "old bio entry", "alice").await;
        // Back-date both created_at and last_recall_at so the path
        // qualifies for archival.
        let past = (chrono::Utc::now() - chrono::Duration::days(400)).to_rfc3339();
        sqlx::query("UPDATE fact_index SET created_at = ?, last_recall_at = ? WHERE fact_id = ?")
            .bind(&past)
            .bind(&past)
            .bind(stale.as_str())
            .execute(&pool)
            .await
            .unwrap();

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.archive_detector.proposals_emitted.len(), 1);
        let pid = &report.archive_detector.proposals_emitted[0];
        let (wiki_id, path, reason, status): (String, String, String, String) = sqlx::query_as(
            "SELECT wiki_id, path, reason, status FROM archive_proposals WHERE proposal_id = ?",
        )
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(wiki_id, "alice");
        assert!(path.ends_with("alice/index.md"), "got path {path:?}");
        assert_eq!(reason, "no_recall_hit_365d");
        assert_eq!(status, "pending");
        drop(dir);
    }

    #[tokio::test]
    async fn archive_detector_skips_fresh_pages() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, "alice", "fresh bio", "alice").await;
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        assert!(report.archive_detector.proposals_emitted.is_empty());
        drop(dir);
    }

    // ---------- auto-apply sweep ----------

    #[tokio::test]
    async fn auto_apply_sweep_applies_dedup_merge_past_timeout() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let loser = plant_fact(&tree, &pool, "alice", "Alice has a cat", "alice").await;
        let winner = plant_fact(&tree, &pool, "alice", "Alice owns a cat", "alice").await;
        // Emit a dedup_merge proposal directly, then back-date its
        // timeout_at so the sweep picks it up on the next cycle.
        let proposal_id = crate::dedup::emit_dedup_merge(
            &pool,
            &winner,
            &loser,
            &crate::dedup::DedupMergeHints::default(),
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE structure_proposals SET timeout_at = ? WHERE proposal_id = ?")
            .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339())
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.auto_apply.candidates_examined, 1);
        assert_eq!(report.auto_apply.applied.len(), 1);
        assert_eq!(report.auto_apply.applied[0].1, "dedup_merge");
        // The loser must now be superseded.
        let (superseded_at,): (Option<String>,) =
            sqlx::query_as("SELECT superseded_at FROM fact_index WHERE fact_id = ?")
                .bind(loser.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(superseded_at.is_some());
        // The sweep lands the row on `applied_pending_confirm`
        // with `apply_mode='auto'` and a `confirm_deadline`, and emits
        // a `wiki_events.kind='auto_applied'` row for the consumer.
        let (status, apply_mode, confirm_deadline): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, apply_mode, confirm_deadline
                       FROM structure_proposals WHERE proposal_id = ?",
            )
            .bind(&proposal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "applied_pending_confirm");
        assert_eq!(apply_mode.as_deref(), Some("auto"));
        assert!(confirm_deadline.is_some());
        let event_kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM wiki_events ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            event_kinds.iter().any(|k| k == "auto_applied"),
            "expected an auto_applied wiki_events row, got {event_kinds:?}",
        );
        drop(dir);
    }

    #[tokio::test]
    async fn auto_finalize_sweep_locks_dedup_merge_past_confirm_deadline() {
        // Build a row in `applied_pending_confirm` end-to-end through
        // the auto-apply sweep, back-date `confirm_deadline`, then run
        // the cycle again and assert the auto-revert sweep unwinds the
        // dedup_merge (loser is no longer superseded) and emits the
        // wiki_events row for the consumer.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let loser = plant_fact(&tree, &pool, "alice", "Alice has a cat", "alice").await;
        let winner = plant_fact(&tree, &pool, "alice", "Alice owns a cat", "alice").await;
        let proposal_id = crate::dedup::emit_dedup_merge(
            &pool,
            &winner,
            &loser,
            &crate::dedup::DedupMergeHints::default(),
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE structure_proposals SET timeout_at = ? WHERE proposal_id = ?")
            .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339())
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();

        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");

        // Cycle 1: auto-apply lands the row on applied_pending_confirm.
        run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        // Back-date the freshly-set confirm_deadline.
        sqlx::query("UPDATE structure_proposals SET confirm_deadline = ? WHERE proposal_id = ?")
            .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
            .bind(&proposal_id)
            .execute(&pool)
            .await
            .unwrap();

        // Cycle 2: auto-finalize sweep flips silently to applied.
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &RemPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.auto_finalize.candidates_examined, 1);
        assert_eq!(report.auto_finalize.finalized.len(), 1);
        assert_eq!(report.auto_finalize.finalized[0], proposal_id);

        // Contract: the kind handler is NOT re-invoked on finalize,
        // so the loser stays superseded (the auto-apply did the work
        // and silence = consent means we keep it).
        let (superseded_at,): (Option<String>,) =
            sqlx::query_as("SELECT superseded_at FROM fact_index WHERE fact_id = ?")
                .bind(loser.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            superseded_at.is_some(),
            "loser should stay superseded — silence = consent"
        );

        let (status, apply_mode, revert_token, triggered_by): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, apply_mode, revert_token, revert_triggered_by
                   FROM structure_proposals WHERE proposal_id = ?",
        )
        .bind(&proposal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied", "silence finalizes to applied");
        assert_eq!(apply_mode.as_deref(), Some("auto"), "apply_mode preserved");
        assert!(
            revert_token.is_none(),
            "no revert_token minted on silent finalize"
        );
        assert!(
            triggered_by.is_none(),
            "finalize is not a revert, no triggered_by stamped"
        );

        // Only the `auto_applied` event from cycle 1 — finalize emits nothing.
        let event_kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM wiki_events ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            event_kinds,
            vec!["auto_applied".to_owned()],
            "only auto_applied from cycle 1; finalize sweep emits nothing",
        );
        drop(dir);
    }

    // build_recommended_answers moved to crate::proposals so the
    // auto-apply sweep can live there as a first-class API. Tests for
    // the helper live next to it in proposals.rs.

    // ---------- parse helper ----------

    #[test]
    fn parse_llm_yes_accepts_strict_json() {
        assert!(parse_llm_yes("{\"same\": true}"));
        assert!(!parse_llm_yes("{\"same\": false}"));
        assert!(!parse_llm_yes("not json"));
        assert!(parse_llm_yes("Sure: {\"same\":true}\nthanks"));
    }

    #[test]
    fn parse_split_decision_round_trips_strict_json() {
        let d = parse_split_decision(
            "{\"split\": true, \"fact_ids\": [\"f-1\", \"f-2\"], \"target_page\": \"a.md\"}",
        )
        .unwrap();
        assert!(d.split);
        assert_eq!(d.fact_ids, vec!["f-1".to_owned(), "f-2".to_owned()]);
        assert_eq!(d.target_page.as_deref(), Some("a.md"));
        let d = parse_split_decision("{\"split\": false}").unwrap();
        assert!(!d.split);
        assert!(d.fact_ids.is_empty());
        assert!(d.target_page.is_none());
        // Tolerant to prose around the JSON, like the other verdicts.
        let d = parse_split_decision("Sure: {\"split\": false} done").unwrap();
        assert!(!d.split);
        assert!(parse_split_decision("not json").is_none());
        assert!(parse_split_decision("{\"split\": \"yes\"}").is_none());
    }

    #[test]
    fn default_target_page_slugifies_body_prefix() {
        assert_eq!(
            default_target_page("Notes about ACME Corp partnership"),
            "notes_about_acme_corp.md"
        );
        assert_eq!(default_target_page("    "), "promoted_paragraph.md");
    }

    // ---------- smart-wiki-aware sub-jobs ----------

    /// Materialise a smart-family wiki on disk + sync the registry.
    /// Mirrors `write_wiki` but for `wiki-companion` (companion = true;
    /// renamed from `wiki-project-companion`).
    /// Returns the wiki id so callers can plant facts in it.
    fn write_smart_wiki(tree: &WikiTree, slug: &str, title: &str, owner: &str) {
        let dir = tree.wikis_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {slug}\nwiki_type: wiki-companion\nslug: {slug}\ntitle: {title}\nacl_default: 'user:{owner}'\nsmart: true\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# placeholder companion\n").unwrap();
    }

    #[tokio::test]
    async fn briefing_dispatcher_emits_stale_draft_notify_for_smart_wiki() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();

        let draft = "status: draft\ntopic: MFA recovery codes\nbody: 'TODO write up'\n";
        let handle = plant_section(&pool, "alice-lnprint", draft).await;

        // Stale-draft window of -1 ns ⇒ threshold = now + 1 ns, so the
        // just-planted fact is unambiguously "older" than the threshold.
        let policy = RemPolicy {
            briefing_stale_draft_age: chrono::Duration::nanoseconds(-1),
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(report.briefing_dispatcher.wikis_examined, 1);
        assert_eq!(
            report.briefing_dispatcher.notifications_emitted.len(),
            1,
            "exactly one stale-draft notify expected, got {:?}",
            report.briefing_dispatcher
        );
        let (wiki, topic) = &report.briefing_dispatcher.notifications_emitted[0];
        assert_eq!(wiki, "alice-lnprint");
        assert!(topic.starts_with("Stale draft"));

        // Source_ref is the deterministic key the idempotency probe keys on.
        // stale_draft is routed to `observation` (REM noticed something).
        let (source_ref, kind): (String, Option<String>) = sqlx::query_as(
            "SELECT source_ref, kind FROM wiki_briefing_items WHERE wiki_id = 'alice-lnprint'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            source_ref,
            format!("rem:briefing_dispatcher:stale_draft:{handle}")
        );
        assert_eq!(kind.as_deref(), Some("observation"));
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_dispatcher_is_idempotent_across_cycles() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        let draft = "status: draft\ntopic: stale\nbody: 'TODO'\n";
        let _ = plant_section(&pool, "alice-lnprint", draft).await;
        let policy = RemPolicy {
            briefing_stale_draft_age: chrono::Duration::nanoseconds(-1),
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");

        let r1 = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle 1");
        assert_eq!(r1.briefing_dispatcher.notifications_emitted.len(), 1);
        assert_eq!(r1.briefing_dispatcher.deduplicated, 0);

        let r2 = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle 2");
        assert_eq!(
            r2.briefing_dispatcher.notifications_emitted.len(),
            0,
            "second cycle must not re-emit the same finding"
        );
        assert_eq!(
            r2.briefing_dispatcher.deduplicated, 1,
            "second cycle must record the dedup absorption"
        );

        // Exactly one DB row across both cycles.
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wiki_briefing_items WHERE wiki_id = 'alice-lnprint'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_dispatcher_skips_non_smart_wikis() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let _ = plant_fact(
            &tree,
            &pool,
            "alice",
            "status: draft\ntopic: a\nbody: b\n",
            "alice",
        )
        .await;
        let policy = RemPolicy {
            briefing_stale_draft_age: chrono::Duration::nanoseconds(-1),
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# x\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(report.briefing_dispatcher.wikis_examined, 0);
        assert!(report.briefing_dispatcher.notifications_emitted.is_empty());
        drop(dir);
    }

    #[tokio::test]
    async fn backlink_reciprocity_emits_when_smart_wiki_lacks_inverse() {
        let (dir, mut tree, pool) = setup_workdir().await;
        // Standard wiki "alice" + smart wiki "alice-lnprint" (same owner).
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        // alice references the smart wiki ⇒ reciprocity expected.
        plant_fact(
            &tree,
            &pool,
            "alice",
            "Saw the docs at [[alice-lnprint]] today.",
            "alice",
        )
        .await;
        // The smart wiki has no fact mentioning [[alice]] ⇒ inverse missing.

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(report.backlink_reciprocity.smart_wikis_known, 1);
        assert!(report.backlink_reciprocity.incoming_links >= 1);
        assert_eq!(
            report.backlink_reciprocity.notifications_emitted.len(),
            1,
            "expected exactly one missing-backlink notify, got {:?}",
            report.backlink_reciprocity
        );
        let (target, source) = &report.backlink_reciprocity.notifications_emitted[0];
        assert_eq!(target, "alice-lnprint");
        assert_eq!(source, "alice");

        // Backlink reciprocity is routed to `reasoning` —
        // REM is recommending a concrete action (add the inverse link),
        // not just observing a gap.
        let (source_ref, kind): (String, Option<String>) = sqlx::query_as(
            "SELECT source_ref, kind FROM wiki_briefing_items WHERE wiki_id = 'alice-lnprint'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source_ref, "rem:backlink_reciprocity:alice");
        assert_eq!(kind.as_deref(), Some("reasoning"));
        drop(dir);
    }

    #[tokio::test]
    async fn backlink_reciprocity_skips_when_smart_wiki_has_reciprocal() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(
            &tree,
            &pool,
            "alice",
            "Reference to [[alice-lnprint]].",
            "alice",
        )
        .await;
        plant_section(
            &pool,
            "alice-lnprint",
            "Back-reference to [[alice]] on the owner wiki.",
        )
        .await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");
        assert!(
            report.backlink_reciprocity.notifications_emitted.is_empty(),
            "reciprocal back-link present ⇒ no notify, got {:?}",
            report.backlink_reciprocity
        );
        drop(dir);
    }

    #[tokio::test]
    async fn legacy_write_jobs_skip_smart_family() {
        let (dir, mut tree, pool) = setup_workdir().await;
        // Smart wiki with children + an active fact ⇒ would normally
        // qualify Hub Writer; the smart-family gate must filter it out.
        let parent = "alice-lnprint";
        let child = "alice-lnprint-auth";
        let parent_dir = tree.wikis_dir().join(parent);
        std::fs::create_dir_all(&parent_dir).unwrap();
        let fm = format!(
            "---\nwiki_id: {parent}\nwiki_type: wiki-companion\nslug: {parent}\ntitle: lnprint\nacl_default: 'user:alice'\nsmart: true\nchildren:\n  - wiki_id: {child}\n    slug: auth\n    title: Auth\n    wiki_type: wiki-tech\n---\n"
        );
        std::fs::write(parent_dir.join("_meta.md"), fm).unwrap();
        std::fs::write(parent_dir.join("index.md"), "# companion-original\n").unwrap();
        write_wiki(&tree, child, "Auth", "wiki-tech");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(&tree, &pool, parent, "active fact body", "alice").await;

        let policy = RemPolicy::default();
        let hub_llm =
            FakeLlmBackend::new("hub", "# REGEN — MUST NEVER BE WRITTEN OVER COMPANION\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert!(
            !report.hub_writer.regenerated.iter().any(|w| w == parent),
            "Hub Writer must skip smart wiki, got {:?}",
            report.hub_writer
        );
        let index = std::fs::read_to_string(parent_dir.join("index.md")).unwrap();
        assert!(
            index.contains("companion-original"),
            "smart-wiki index.md must remain untouched, got {index:?}",
        );
        drop(dir);
    }

    // ---------- Briefing-processor non-smart (sub-job 10) ----------

    /// INSERT a row directly into `wiki_briefing_items` mirroring what
    /// the dashboard comment route writes. `ts_offset` shifts the
    /// timestamp relative to "now" so the test can land rows either
    /// inside or outside the grace period.
    async fn insert_pending_briefing_row(
        pool: &SqlitePool,
        wiki_id: &str,
        ts_offset: chrono::Duration,
        target_cite: Option<&str>,
    ) -> i64 {
        let ts = (chrono::Utc::now() - ts_offset).to_rfc3339();
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO wiki_briefing_items
                (wiki_id, source_kind, source_ref, topic, body, kind, ts, target_cite, \
                 author_sender_id, processed_at)
             VALUES (?, 'dashboard_comment', 'dashboard:alice', 'topic', 'body', 'external', \
                     ?, ?, 'alice', NULL)
             RETURNING id",
        )
        .bind(wiki_id)
        .bind(&ts)
        .bind(target_cite)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    #[tokio::test]
    async fn briefing_processor_drains_non_smart_row_past_grace() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Row aged 48h ⇒ comfortably past the 15-minute default grace.
        let bi_id = insert_pending_briefing_row(
            &pool,
            "alice",
            chrono::Duration::hours(48),
            Some("wiki://alice/index.md"),
        )
        .await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(
            report.briefing_processor.items_examined, 1,
            "expected one eligible row, got {:?}",
            report.briefing_processor
        );
        assert_eq!(report.briefing_processor.items_processed, 1);
        assert_eq!(report.briefing_processor.items_already_processed, 0);
        assert_eq!(report.briefing_processor.items_wiki_missing, 0);
        assert!(report.briefing_processor.errors.is_empty());

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_some(),
            "processed_at must be stamped after sub-job 10 ran, got {processed:?}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_processor_applies_standard_comment_as_fact_correction() {
        // Action-taking: with the comment_applier (ingest) slot wired, a
        // parked comment on a NARRATIVE page is interpreted into a fact op and
        // applied — not just mark-passive drained.
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();

        let fid = crate::types::FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5daa").unwrap();
        crate::fact_index::insert(
            &pool,
            &crate::fact_index::NewFact {
                authored_refs: Vec::new(),
                fact_id: fid.clone(),
                wiki_id: "alice".to_owned(),
                source_path: "wikis/alice/index.md".to_owned(),
                region_start: Some(0),
                region_end: Some(20),
                text: "Alice was born in 1985".to_owned(),
                embedding: vec![0.1, 0.2, 0.3, 0.4],
                owner_id: "user:alice".parse::<crate::types::Principal>().unwrap(),
                allow_ids: Vec::new(),
                sender_id: None,
                fact_type: None,
                topics: Vec::new(),
                valid_from: None,
                valid_to: None,
                // Inert: re-derived/non-ingest fact — no
                // classifier placement proposal to carry.
                target_page: None,
                style: None,
                page_description: None,
                salience: None,
                source_ref: None,
            },
        )
        .await
        .unwrap();
        insert_pending_briefing_row(
            &pool,
            "alice",
            chrono::Duration::hours(48),
            Some("wiki://alice/index.md#bio"),
        )
        .await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let applier = FakeLlmBackend::new(
            "ingest",
            format!(
                "{{\"ops\":[{{\"action\":\"correct\",\"fact_id\":\"{}\",\"text\":\"Alice was born in 1986\"}}]}}",
                fid.as_str()
            ),
        );
        let llms = RemLlms {
            hub_writer: &hub_llm,
            revisor: &rev_llm,
            auto_promote: None,
            apply: None,
            comment_applier: Some(&applier),
            cronista: None,
            navigator: None,
        };
        let report = run_cycle(&pool, &tree, fake_embedder(), &llms, &policy)
            .await
            .expect("cycle");

        assert_eq!(
            report.briefing_processor.facts_corrected, 1,
            "standard-wiki comment must apply as a fact correction, got {:?}",
            report.briefing_processor
        );
        assert_eq!(report.briefing_processor.items_processed, 1);
        let row = crate::fact_index::find_by_id(&pool, &fid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.text, "Alice was born in 1986",
            "the claim must be corrected in place"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_processor_skips_smart_wikis() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        let bi_id =
            insert_pending_briefing_row(&pool, "alice-lnprint", chrono::Duration::hours(48), None)
                .await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(
            report.briefing_processor.items_examined, 0,
            "smart-wiki rows must not be examined — smart consumer owns the drain"
        );
        assert_eq!(report.briefing_processor.items_processed, 0);

        // Row must remain pending.
        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_none(),
            "smart-wiki row must stay pending after REM cycle"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_processor_respects_grace_period() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Row aged only 5 minutes, default grace is 15 minutes ⇒ must
        // be skipped.
        let bi_id =
            insert_pending_briefing_row(&pool, "alice", chrono::Duration::minutes(5), None).await;

        let policy = RemPolicy::default();
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(
            report.briefing_processor.items_examined, 0,
            "fresh row inside grace must not be examined"
        );

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(processed.is_none(), "row inside grace must stay pending");
        drop(dir);
    }

    #[tokio::test]
    async fn briefing_processor_disabled_is_a_noop() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let bi_id =
            insert_pending_briefing_row(&pool, "alice", chrono::Duration::hours(48), None).await;

        let policy = RemPolicy {
            briefing_processor_enabled: false,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# unused\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": false}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .expect("cycle");

        assert_eq!(report.briefing_processor.items_examined, 0);
        assert_eq!(report.briefing_processor.items_processed, 0);

        let processed: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM wiki_briefing_items WHERE id = ?")
                .bind(bi_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            processed.is_none(),
            "row must stay pending when sub-job is disabled"
        );
        drop(dir);
    }

    // ---------- completion sweep ----------

    /// Evidence ("hanno visto Jumanji") + a similar open item ("vuole
    /// vedere Jumanji") + a confirming LLM → the open item closes as
    /// completed, act-first, with the `validity_close` receipt + notice.
    #[tokio::test]
    async fn completion_sweep_closes_a_confirmed_open_item_act_first() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let open_item = plant_fact(&tree, &pool, "alice", "Vuole vedere Jumanji", "alice").await;
        let evidence = plant_fact(
            &tree,
            &pool,
            "alice",
            "Hanno visto Jumanji ieri sera",
            "alice",
        )
        .await;
        assert_ne!(open_item, evidence);

        let resp = format!(
            "{{\"completions\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            open_item.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_completion_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert!(report.evidence_examined >= 1);
        assert_eq!(report.closed, vec![open_item.as_str().to_owned()]);
        assert_eq!(report.receipts.len(), 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let row = fact_index::find_by_id(&pool, &open_item)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_some(), "window closed");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::COMPLETED)
        );
        assert!(row.deleted_at.is_none(), "closure is never a tombstone");

        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt");
        assert_eq!(status, "applied");
        let notices: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(notices, 1);
        drop(dir);
    }

    /// Rules-page facts sit outside the completion model on both axes: a
    /// standing directive completes nothing and is never completed by
    /// neighbouring evidence (the 2026-07-05 live incident: franz's
    /// "Gandalf" naming rule read as evidence completing morgana's
    /// parallel "Ernest" naming rule — cross-user collateral).
    #[tokio::test]
    async fn completion_sweep_never_touches_rules_page_facts() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bot", "Bot", "wiki-user");
        let plant_rule = |body: &str| {
            let body = body.to_owned();
            let tree = &tree;
            let pool = &pool;
            async move {
                capture::wiki_capture(
                    tree,
                    pool,
                    fake_embedder(),
                    CaptureRequest {
                        authored_refs: Vec::new(),
                        wiki_id: WikiId::parse("bot").unwrap(),
                        page: PathBuf::from("rules.md"),
                        body,
                        owner: Principal::User("bot".to_owned()),
                        allow: Vec::new(),
                        sender: None,
                        fact_type: Some("rule".to_owned()),
                        topics: Vec::new(),
                        dedup_threshold: Some(0.999),
                        valid_from: None,
                        valid_to: None,
                        style: None,
                        page_description: None,
                        salience: None,
                    },
                )
                .await
                .expect("plant rule")
                .fact_id
            }
        };
        let ernest = plant_rule("Il tuo nome per questo utente è Ernest.").await;
        let gandalf = plant_rule("Il tuo nome per questo utente è Gandalf.").await;
        assert_ne!(ernest, gandalf);

        // Confirm-everything confirmer: Ernest falls if the sweep ever
        // builds a case around the rules page — both fences must hold.
        let resp = format!(
            "{{\"completions\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            ernest.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_completion_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert_eq!(
            report.evidence_examined, 0,
            "a standing rule is never completion evidence: {report:?}"
        );
        let row = fact_index::find_by_id(&pool, &ernest)
            .await
            .unwrap()
            .expect("row");
        assert!(
            row.valid_to.is_none() && row.decay_reason.is_none(),
            "a rules-page fact must never fall as completed collateral"
        );
        drop(dir);
    }

    /// The confirmer's empty verdict is the conservative no-op: nothing
    /// closes, no receipt, no notice.
    #[tokio::test]
    async fn completion_sweep_respects_the_confirmers_refusal() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let open_item = plant_fact(&tree, &pool, "alice", "Vuole vedere Jumanji", "alice").await;
        plant_fact(&tree, &pool, "alice", "Hanno parlato di Jumanji", "alice").await;

        let llm = FakeLlmBackend::new("confirmer", "{\"completions\":[]}");
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_completion_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert!(report.closed.is_empty());
        assert!(report.receipts.is_empty());
        let row = fact_index::find_by_id(&pool, &open_item)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_none(), "refusal leaves the item open");
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        drop(dir);
    }

    /// The candidate snapshot is built once per cycle, so two different
    /// evidence facts can both nominate the same open item. The intra-cycle
    /// guard closes it ONCE: the second evidence finds the item already
    /// closed this cycle, drops it before the confirmer, and emits no
    /// redundant receipt — which also keeps the single receipt's revert
    /// snapshot honest (`close_validity` has no re-close guard of its own).
    #[tokio::test]
    async fn completion_sweep_closes_a_shared_item_only_once_per_cycle() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let open_item = plant_fact(&tree, &pool, "alice", "Comprare il pane", "alice").await;
        // Two distinct evidence facts that both attest the same open item.
        plant_fact(
            &tree,
            &pool,
            "alice",
            "Ho comprato il pane stamattina",
            "alice",
        )
        .await;
        plant_fact(&tree, &pool, "alice", "Pane preso, fatto", "alice").await;

        // The confirmer always names the one open item.
        let resp = format!(
            "{{\"completions\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            open_item.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_completion_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        // Closed exactly once, one receipt — not the two the snapshot
        // staleness would otherwise produce.
        assert_eq!(report.closed, vec![open_item.as_str().to_owned()]);
        assert_eq!(report.receipts.len(), 1);
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            receipts, 1,
            "the shared item must yield a single closure receipt"
        );
        drop(dir);
    }

    // ---------- cross-wiki refile sweep ----------

    /// A clearly misfiled fact in wiki A embeds toward wiki B; the cosine
    /// pre-filter nominates it, the confirmer says "move", and the fact
    /// lands in B act-first with a born-applied receipt + notice.
    #[tokio::test]
    async fn refile_sweep_moves_a_misfiled_fact_act_first() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "bob", "Bob", "wiki-user");

        // alice's own fact and bob's own fact sit on orthogonal axes; the
        // misfiled fact (filed in alice) embeds onto bob's axis.
        let _alice_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Alice loves pasta",
            "alice",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let _bob_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob plays the trumpet",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        let misfiled = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Bob bought a new trumpet",
            "alice",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;

        // The model even names a specific dest page — which the engine
        // deliberately IGNORES, forcing the fact onto the dest wiki's
        // collision-safe `index.md`. (A named cross-wiki page can collide
        // with a same-slug page already homed in another wiki under the
        // bare-slug plan keyspace, stranding wiki_id != source_path — the
        // regression this asserts.) The move must still land on bob/index.md.
        let resp = "{\"verdict\":\"move\",\"dest_wiki_id\":\"bob\",\"dest_page\":\"cooking.md\",\"reason\":\"this fact is about bob\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert!(
            report.candidates_examined >= 1,
            "must nominate, got {report:?}"
        );
        assert_eq!(report.refiled, vec![misfiled.as_str().to_owned()]);
        assert_eq!(report.receipts.len(), 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        // The row moved to bob.
        let row = fact_index::find_by_id(&pool, &misfiled)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.wiki_id, "bob");
        assert_eq!(row.source_path, "wikis/bob/index.md");
        assert!(row.deleted_at.is_none(), "refile is never a tombstone");

        // Born-applied receipt + one notice.
        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt");
        assert_eq!(status, "applied");
        let notices: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_events WHERE kind = 'structure_applied'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(notices, 1);
        drop(dir);
    }

    /// The reviewer→refile bridge: a parked `cross_subject_bloat`
    /// nomination is seeded into the judge pass even when the cosine
    /// margin would never nominate it (the fact embeds AT home), and the
    /// park drains on consumption.
    #[tokio::test]
    async fn refile_sweep_seeds_parked_reviewer_candidates_past_the_margin() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        // The parked fact embeds exactly on home's own axis — the margin
        // pre-filter has no reason to nominate it.
        let anchor = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Nota archiviata da alice",
            "alice",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let parked = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Bob ha cambiato lavoro",
            "bob",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let _bob_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob plays the trumpet",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        drop(anchor);
        save_unrelated_plan(&tree);
        crate::planner::park_bridge_signals(&tree, &[parked.as_str().to_owned()], &[])
            .expect("park");

        let resp =
            "{\"verdict\":\"move\",\"dest_wiki_id\":\"bob\",\"reason\":\"the subject is bob\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-bridge",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(report.bridge_candidates, 1, "{report:?}");
        assert!(
            report.refiled.contains(&parked.as_str().to_owned()),
            "the parked nomination reached the judge and moved: {report:?}"
        );
        let row = fact_index::find_by_id(&pool, &parked)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wiki_id, "bob");
        assert!(
            crate::planner::load_previous_plan(&tree)
                .unwrap()
                .unwrap()
                .refile_candidates
                .is_empty(),
            "the park drained on consumption"
        );
        drop(dir);
    }

    /// A smart wiki is skipped as BOTH source and destination — the
    /// ownership boundary is the consumer's, so a fact in / toward a smart
    /// wiki is never refiled by REM.
    #[tokio::test]
    async fn refile_sweep_skips_smart_wikis_both_ends() {
        let (dir, tree, pool) = setup_workdir().await;
        // alice is smart; bob is standard. A fact filed in the standard
        // bob embeds toward the smart alice — but alice is not a candidate
        // dest, so nothing moves. (And a fact in smart alice is never a
        // source.)
        write_smart_wiki(&tree, "alice", "Alice", "alice");
        write_wiki(&tree, "bob", "Bob", "wiki-user");

        let _alice_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Alice's project note",
            "alice",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        let _bob_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob likes hiking",
            "bob",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let _toward_smart = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Alice shipped the project",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;

        // The LLM would say "move" if asked — but it must never be asked,
        // because alice (the only closer wiki) is smart and excluded.
        let resp = "{\"verdict\":\"move\",\"dest_wiki_id\":\"alice\",\"dest_page\":\"index.md\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert!(
            report.refiled.is_empty(),
            "smart wikis must be skipped: {report:?}"
        );
        assert!(report.receipts.is_empty());
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM structure_proposals")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        drop(dir);
    }

    /// A `rules.md` fact is never NOMINATED for refile, however foreign it
    /// embeds: a per-user behaviour rule naturally embeds toward its user's
    /// wiki, and a confirmed move would eject it from the behaviour-rules
    /// channel (the refile twin of the compiler-door skip). The non-rules
    /// counterpart (`refile_sweep_moves_a_misfiled_fact_act_first`) pins that
    /// an ordinary fact with the same geometry IS still nominated.
    #[tokio::test]
    async fn refile_sweep_never_nominates_rules_page_facts() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "bob", "Bob", "wiki-user");

        // alice's own content sits on one axis; bob's wiki holds TWO facts on
        // the other axis (so bob's own facts are at home and never nominated
        // themselves). The behaviour rule lives on alice's `rules.md` and
        // embeds squarely onto bob's axis — the exact geometry that moved the
        // misfiled fact in the act-first test above.
        let _alice_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            "Alice loves pasta",
            "alice",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;
        let _bob_own = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob plays the trumpet",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        let _bob_own_2 = plant_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            "Bob rehearses with the band",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;
        let rule = plant_page_fact_with_embedding(
            &tree,
            &pool,
            "alice",
            crate::wiki::RULES_FILENAME,
            "Parla a Bob sempre in italiano.",
            "bob",
            vec![0.0, 1.0, 0.0, 0.0],
        )
        .await;

        // The confirmer would say "move" — it must never be asked.
        let resp = "{\"verdict\":\"move\",\"dest_wiki_id\":\"bob\",\"dest_page\":\"index.md\"}";
        let llm = FakeLlmBackend::new("confirmer", resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_refile_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert_eq!(
            report.candidates_examined, 0,
            "a rules.md fact must not be nominated: {report:?}"
        );
        assert!(report.refiled.is_empty());
        // The rule is untouched, still on the agent wiki's rules page.
        let row = fact_index::find_by_id(&pool, &rule)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.wiki_id, "alice");
        assert_eq!(row.source_path, "wikis/alice/rules.md");
        drop(dir);
    }

    /// The revisor never pairs a `rules.md` fact with a non-rules fact
    /// (both-or-neither): an episodic restatement of a directive must not
    /// fold the rule off its page. The confirmer would say "same" — it must
    /// never be asked about the mixed pair.
    #[tokio::test]
    async fn revisor_never_pairs_a_rule_with_a_non_rules_fact() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        // Same wording family as the confirmed-pair test, but one side lives
        // on the reserved rules page.
        let rule_id = plant_page_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            crate::wiki::RULES_FILENAME,
            "bob prefers tea with milk every morning",
            "bob",
            vec![0.1, 0.2, 0.3, 0.4],
        )
        .await;
        let _fact_id = plant_fact(
            &tree,
            &pool,
            "bob",
            "bob likes morning tea with a splash of milk",
            "bob",
        )
        .await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(
            report.revisor.pairs_examined, 0,
            "the mixed rule/non-rule pair must be filtered before the LLM: {report:?}"
        );
        assert!(report.revisor.applied.is_empty());
        let row = fact_index::find_by_id(&pool, &rule_id)
            .await
            .unwrap()
            .expect("row");
        assert!(
            row.superseded_at.is_none(),
            "the rule must never lose a cross-boundary dedup"
        );
        drop(dir);
    }

    /// Rule-vs-rule pairs stay nominable: two near-duplicate directives on
    /// the same `rules.md` page still reach the confirmer and merge.
    #[tokio::test]
    async fn revisor_still_pairs_rules_with_rules() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "bob", "Bob", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let old_id = plant_page_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            crate::wiki::RULES_FILENAME,
            "bob prefers tea with milk every morning",
            "bob",
            vec![0.1, 0.2, 0.3, 0.4],
        )
        .await;
        let _new_id = plant_page_fact_with_embedding(
            &tree,
            &pool,
            "bob",
            crate::wiki::RULES_FILENAME,
            "bob likes morning tea with a splash of milk",
            "bob",
            vec![0.1, 0.2, 0.3, 0.4],
        )
        .await;

        let policy = RemPolicy {
            revisor_jaccard_min: 0.05,
            revisor_jaccard_max: 0.99,
            ..RemPolicy::default()
        };
        let hub_llm = FakeLlmBackend::new("hub", "# index\n");
        let rev_llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_cycle(
            &pool,
            &tree,
            fake_embedder(),
            &test_llms(&hub_llm, &rev_llm),
            &policy,
        )
        .await
        .unwrap();
        assert!(
            report.revisor.pairs_examined >= 1,
            "rule-vs-rule must still be examined: {report:?}"
        );
        assert_eq!(report.revisor.applied.len(), 1, "{report:?}");
        let old = fact_index::find_by_id(&pool, &old_id)
            .await
            .unwrap()
            .expect("row");
        assert!(
            old.superseded_at.is_some(),
            "the older duplicate rule merges away"
        );
        drop(dir);
    }

    // ---------- contradiction sweep ----------

    /// A freshly superseded fact (the cancelled departure) seeds the
    /// sweep; the confirmer names a satellite (the itinerary day) and it
    /// closes as contradicted, act-first, with the receipt + notice.
    #[tokio::test]
    async fn contradiction_sweep_closes_a_confirmed_satellite() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let departure = plant_fact(
            &tree,
            &pool,
            "alice",
            "Partenza per Parigi il 15 giugno",
            "alice",
        )
        .await;
        let satellite = plant_fact(
            &tree,
            &pool,
            "alice",
            "Itinerario giorno 1: Louvre",
            "alice",
        )
        .await;
        let cancellation = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il viaggio a Parigi è annullato",
            "alice",
        )
        .await;
        fact_index::mark_superseded(&pool, &departure, &cancellation)
            .await
            .expect("supersede");

        let resp = format!(
            "{{\"invalidated\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            satellite.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_contradiction_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert_eq!(report.seeds_examined, 1, "one freshly contradicted seed");
        assert_eq!(report.closed, vec![satellite.as_str().to_owned()]);
        assert_eq!(report.receipts.len(), 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let row = fact_index::find_by_id(&pool, &satellite)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_some(), "the satellite fell with the event");
        assert_eq!(
            row.decay_reason.as_deref(),
            Some(fact_index::decay::CONTRADICTED)
        );
        assert!(row.deleted_at.is_none(), "closure is never a tombstone");
        // The satellite's window closes at the SEED's closure instant
        // (the moment the event fell), which the supersede stamped.
        let seed = fact_index::find_by_id(&pool, &departure)
            .await
            .unwrap()
            .expect("seed");
        assert_eq!(row.valid_to, seed.valid_to);
        drop(dir);
    }

    /// Rules-page facts are never satellite candidates: a standing
    /// directive leaves the channel only via supersede / tombstone / its
    /// owner's explicit closure — never as collateral of a neighbouring
    /// contradiction (the 2026-07-01 live incident: the freshly revised
    /// TTS rules fell as satellites of their own dead predecessors).
    #[tokio::test]
    async fn contradiction_sweep_never_nominates_rules_page_facts() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let departure = plant_fact(
            &tree,
            &pool,
            "alice",
            "Partenza per Parigi il 15 giugno",
            "alice",
        )
        .await;
        let cancellation = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il viaggio a Parigi è annullato",
            "alice",
        )
        .await;
        // A standing rule on the reserved page, embedding-similar by
        // construction (the fake embedder is content-agnostic).
        let rule = capture::wiki_capture(
            &tree,
            &pool,
            fake_embedder(),
            CaptureRequest {
                authored_refs: Vec::new(),
                wiki_id: WikiId::parse("alice").unwrap(),
                page: PathBuf::from("rules.md"),
                body: "Rispondi sempre anche a voce.".to_owned(),
                owner: Principal::User("alice".to_owned()),
                allow: Vec::new(),
                sender: None,
                fact_type: Some("rule".to_owned()),
                topics: Vec::new(),
                dedup_threshold: Some(0.999),
                valid_from: None,
                valid_to: None,
                style: None,
                page_description: None,
                salience: None,
            },
        )
        .await
        .expect("plant rule")
        .fact_id;
        fact_index::mark_superseded(&pool, &departure, &cancellation)
            .await
            .expect("supersede");

        // A confirm-everything confirmer: if the rule were ever OFFERED it
        // would fall — the guard must keep it out of the candidate pool.
        let resp = format!(
            "{{\"invalidated\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            rule.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_contradiction_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        let row = fact_index::find_by_id(&pool, &rule)
            .await
            .unwrap()
            .expect("rule row");
        assert!(
            row.valid_to.is_none() && row.decay_reason.is_none(),
            "a rules-page fact must never fall as a satellite: {report:?}"
        );
        drop(dir);
    }

    /// Identity-core stickiness (leva 3) in the contradiction sweep: a
    /// role / relationship fact (`bio` + `salience=high`) is never closed
    /// as a collateral satellite of a neighbouring contradiction — it
    /// changes only on an explicit correction. Mirror of the rules-page
    /// perimeter test above.
    #[tokio::test]
    async fn contradiction_sweep_never_closes_an_identity_core_satellite() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let departure = plant_fact(
            &tree,
            &pool,
            "alice",
            "Partenza per Parigi il 15 giugno",
            "alice",
        )
        .await;
        let cancellation = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il viaggio a Parigi è annullato",
            "alice",
        )
        .await;
        // An identity-core relationship, embedding-similar by construction
        // (the fake embedder is content-agnostic).
        let relation =
            plant_fact(&tree, &pool, "alice", "Bruno è il padre di Alice", "alice").await;
        sqlx::query("UPDATE fact_index SET fact_type = 'bio', salience = 'high' WHERE fact_id = ?")
            .bind(relation.as_str())
            .execute(&pool)
            .await
            .expect("mark identity core");
        fact_index::mark_superseded(&pool, &departure, &cancellation)
            .await
            .expect("supersede");

        // A confirm-everything confirmer: if the relation were ever OFFERED
        // it would fall — the guard must keep it out of the candidate pool.
        let resp = format!(
            "{{\"invalidated\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            relation.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_contradiction_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-core",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        let row = fact_index::find_by_id(&pool, &relation)
            .await
            .unwrap()
            .expect("relation row");
        assert!(
            row.valid_to.is_none() && row.decay_reason.is_none(),
            "an identity-core relationship must never fall as a satellite: {report:?}"
        );
        drop(dir);
    }

    /// The seed's successor LINEAGE is off-limits transitively: the live
    /// head of a revised-twice fact must not fall as a "satellite" of its
    /// own grandparent (the one-hop exclusion missed exactly this).
    #[tokio::test]
    async fn contradiction_sweep_never_eats_the_seeds_own_lineage() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let v1 = plant_fact(&tree, &pool, "alice", "Parto per Parigi il 15", "alice").await;
        let v2 = plant_fact(&tree, &pool, "alice", "Parto per Parigi il 16", "alice").await;
        let v3 = plant_fact(&tree, &pool, "alice", "Parto per Parigi il 17", "alice").await;
        // v1 → v2 → v3: two in-place revisions; v3 is the live head.
        fact_index::mark_superseded(&pool, &v1, &v2)
            .await
            .expect("supersede v1");
        fact_index::mark_superseded(&pool, &v2, &v3)
            .await
            .expect("supersede v2");

        // Confirm-everything confirmer: v3 falls if it is ever offered as a
        // satellite of seed v1 (or v2).
        let resp = format!(
            "{{\"invalidated\":[{{\"target\":\"{}\",\"valid_to\":null}}]}}",
            v3.as_str()
        );
        let llm = FakeLlmBackend::new("confirmer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_contradiction_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        let head = fact_index::find_by_id(&pool, &v3)
            .await
            .unwrap()
            .expect("head row");
        assert!(
            head.valid_to.is_none() && head.decay_reason.is_none(),
            "the live head must never fall as a satellite of its own ancestors: {report:?}"
        );
        drop(dir);
    }

    /// The confirmer's empty verdict leaves every candidate open — the
    /// cluster ends at the seed.
    #[tokio::test]
    async fn contradiction_sweep_respects_an_empty_cluster() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let departure = plant_fact(
            &tree,
            &pool,
            "alice",
            "Partenza per Parigi il 15 giugno",
            "alice",
        )
        .await;
        let unrelated = plant_fact(&tree, &pool, "alice", "Galadriel è celiaca", "alice").await;
        let cancellation = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il viaggio a Parigi è annullato",
            "alice",
        )
        .await;
        fact_index::mark_superseded(&pool, &departure, &cancellation)
            .await
            .expect("supersede");

        let llm = FakeLlmBackend::new("confirmer", "{\"invalidated\":[]}");
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_contradiction_sweep(
            &pool,
            &tree,
            &llm,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert!(report.closed.is_empty());
        assert!(report.receipts.is_empty());
        let row = fact_index::find_by_id(&pool, &unrelated)
            .await
            .unwrap()
            .expect("row");
        assert!(row.valid_to.is_none(), "the unrelated fact stays open");
        drop(dir);
    }

    // ---------- recall-repair sub-job ----------

    /// Plant a fact with an explicit page + topics (the sibling of
    /// [`plant_fact_on_page`] the recall-repair tests need — topics feed
    /// the reader card the gather fan matches).
    async fn plant_topic_fact(
        tree: &WikiTree,
        pool: &SqlitePool,
        wiki: &str,
        page: &str,
        body: &str,
        owner: &str,
        topics: &[&str],
    ) -> FactId {
        let req = crate::capture::CaptureRequest {
            authored_refs: Vec::new(),
            wiki_id: WikiId::parse(wiki).unwrap(),
            page: PathBuf::from(page),
            body: body.to_owned(),
            owner: Principal::User(owner.to_owned()),
            allow: Vec::new(),
            sender: None,
            fact_type: None,
            topics: topics.iter().map(|t| (*t).to_owned()).collect(),
            dedup_threshold: Some(0.999),
            valid_from: None,
            valid_to: None,
            style: None,
            page_description: None,
            salience: None,
        };
        crate::capture::wiki_capture(tree, pool, fake_embedder(), req)
            .await
            .expect("plant")
            .fact_id
    }

    /// The full committed-repair loop: a pending miss, a confirmer that
    /// proposes the re-file, a navigator that reaches the destination —
    /// the gate proves the flip on the scratch and the move commits for
    /// real with the receipt, the miss resolves `repaired`, and the gold
    /// candidates file grows.
    #[tokio::test]
    #[cfg_attr(windows, ignore = "gated refile rejected on Windows — see issue #1")]
    #[expect(
        clippy::too_many_lines,
        reason = "one linear end-to-end scenario (fixture → miss → propose → gate → commit)"
    )]
    async fn recall_repair_commits_a_gated_refile() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        write_wiki(&tree, "ricette", "Ricette", "wiki-user");
        // The missed fact: topic-less, on a page no fan can see.
        let target = plant_topic_fact(
            &tree,
            &pool,
            "alice",
            "misc.md",
            "La crostata di mele si fa con le renette",
            "alice",
            &[],
        )
        .await;
        // The destination wiki has a readable fact whose topic makes its
        // card match the turn's seed ("ricette") for the gather fan.
        plant_topic_fact(
            &tree,
            &pool,
            "ricette",
            "index.md",
            "Le ricette di famiglia sono raccolte qui",
            "alice",
            &["ricette"],
        )
        .await;
        crate::recall_log::record_miss(
            &pool,
            &crate::recall_log::NewMiss {
                created_at: "2026-07-05T10:00:00+00:00",
                sender_id: "alice",
                fact_id: target.as_str(),
                wiki_id: "alice",
                source_path: "wikis/alice/misc.md",
                surface: crate::recall_log::MissSurface::Direct,
                similarity: 0.9,
                restated_text: "come si fa la crostata di mele?",
                log_id: None,
                seed_topics: &["ricette".to_owned()],
            },
        )
        .await
        .expect("miss");

        let confirmer = FakeLlmBackend::new(
            "confirmer",
            "{\"verdict\":\"move\",\"dest_wiki_id\":\"ricette\",\"reason\":\"è una ricetta\"}",
        );
        let navigator = FakeLlmBackend::new(
            "nav",
            "{\"open\":[{\"wiki_id\":\"ricette\"},{\"wiki_id\":\"ricette\",\"page\":\"index.md\"}],\"done\":true}",
        );
        // Flat replay blind (top_k 0) → the gate's verdict rides navigation.
        let policy = RemPolicy {
            gate_recall: crate::ingest::IngestPolicy {
                recall_top_k: 0,
                recall_fresh_top_k: 0,
                ..crate::ingest::IngestPolicy::default()
            },
            ..RemPolicy::default()
        };
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_recall_repair(
            &pool,
            &tree,
            &fake_embedder(),
            &confirmer,
            Some(&navigator),
            "cycle-test",
            Utc::now(),
            &policy,
            &index,
        )
        .await
        .expect("sub-job");

        assert_eq!(report.misses_examined, 1);
        assert_eq!(
            report.repairs_committed, 1,
            "the gated re-file commits: {report:?}"
        );
        assert_eq!(report.receipts.len(), 1);
        let moved = fact_index::find_by_id(&pool, &target)
            .await
            .unwrap()
            .expect("moved fact");
        assert_eq!(
            moved.source_path, "wikis/ricette/index.md",
            "the fact landed on the destination's foundation page"
        );
        let misses = crate::recall_log::recent_misses(&pool, 10).await.unwrap();
        assert_eq!(misses[0].status, "repaired");
        assert_eq!(
            misses[0].resolution.as_deref(),
            Some(report.receipts[0].as_str())
        );
        assert_eq!(report.gold_candidates_appended, 1);
        assert!(
            dir.path()
                .join(crate::recall_gate::RECALL_GOLD_CANDIDATES_FILENAME)
                .is_file(),
            "the 15f candidates file grew"
        );
        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.receipts[0])
                .fetch_one(&pool)
                .await
                .expect("receipt row");
        assert_eq!(status, "applied", "born-applied receipt with revert window");
        drop(dir);
    }

    /// The operator-queue path: a recurring miss with no local repair
    /// (no candidate wikis at all) queues ONE `recall_tuning_proposed`
    /// notice per fact and discards the siblings with their reason tag.
    #[tokio::test]
    async fn recall_repair_queues_recurring_unrepairable_misses() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let target = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il codice del cancello è 4711",
            "alice",
        )
        .await;
        for i in 0..3 {
            crate::recall_log::record_miss(
                &pool,
                &crate::recall_log::NewMiss {
                    created_at: &format!("2026-07-05T10:0{i}:00+00:00"),
                    sender_id: "alice",
                    fact_id: target.as_str(),
                    wiki_id: "alice",
                    source_path: "wikis/alice/index.md",
                    surface: crate::recall_log::MissSurface::Direct,
                    similarity: 0.9,
                    restated_text: "qual è il codice del cancello?",
                    log_id: None,
                    seed_topics: &[],
                },
            )
            .await
            .expect("miss");
        }

        // Only one wiki on disk → no destination candidates → no proposal
        // call at all; the confirmer must never be needed.
        let confirmer = FakeLlmBackend::new("confirmer", "{\"verdict\":\"stay\"}");
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_recall_repair(
            &pool,
            &tree,
            &fake_embedder(),
            &confirmer,
            None,
            "cycle-test",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sub-job");

        assert_eq!(report.misses_examined, 3);
        assert_eq!(
            report.queued, 1,
            "one notice per fact per cycle: {report:?}"
        );
        assert_eq!(report.no_repair, 2, "the sibling misses discard");
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM wiki_events WHERE kind = 'recall_tuning_proposed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1);
        let misses = crate::recall_log::recent_misses(&pool, 10).await.unwrap();
        assert!(misses.iter().any(|m| m.status == "queued"));
        assert_eq!(misses.iter().filter(|m| m.status == "discarded").count(), 2);
        drop(dir);
    }

    // ---------- date normalizer ----------

    /// A flagged deictic fact is rewritten in place against its own
    /// capture date: text + embedding updated, offsets kept, row active.
    #[tokio::test]
    async fn date_normalizer_rewrites_flagged_text_in_place() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let stale = plant_fact(&tree, &pool, "alice", "Oggi ha giocato 31 minuti", "alice").await;
        plant_fact(&tree, &pool, "alice", "Vive a Lisbona", "alice").await;

        let resp = format!(
            "{{\"rewrites\":[{{\"fact_id\":\"{}\",\"text\":\"Il 10 giugno 2026 ha giocato 31 minuti\"}}]}}",
            stale.as_str()
        );
        let llm = FakeLlmBackend::new("normalizer", &resp);
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_date_normalizer(
            &pool,
            &tree,
            &llm,
            &fake_embedder(),
            "cycle-test",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("normalize");

        assert_eq!(report.flagged, 1, "only the deictic fact is flagged");
        assert_eq!(report.examined, 1);
        assert_eq!(report.rewritten, vec![stale.as_str().to_owned()]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let row = fact_index::find_by_id(&pool, &stale)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.text, "Il 10 giugno 2026 ha giocato 31 minuti");
        assert!(row.deleted_at.is_none());
        drop(dir);
    }

    /// The anchor shown to the model is the SEMANTIC capture instant:
    /// `valid_from` (the stored projection of the turn's `occurred_at`
    /// clock) when present, `created_at` only as fallback. A replayed or
    /// backfilled fact must resolve "oggi" against the day it was
    /// uttered, not the wall-clock day its row was inserted.
    #[tokio::test]
    async fn date_normalizer_anchors_on_valid_from_not_created_at() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let stale = plant_fact(&tree, &pool, "alice", "Oggi ha giocato 31 minuti", "alice").await;
        // Backfilled history: the row was inserted now, but the turn's
        // semantic clock said 2026-04-20.
        sqlx::query("UPDATE fact_index SET valid_from = '2026-04-20T09:42:00Z' WHERE fact_id = ?")
            .bind(stale.as_str())
            .execute(&pool)
            .await
            .unwrap();

        let llm = FakeLlmBackend::new("normalizer", "{\"rewrites\":[]}");
        let index = load_smart_wiki_index(&tree).expect("index");
        run_date_normalizer(
            &pool,
            &tree,
            &llm,
            &fake_embedder(),
            "cycle-test",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("normalize");

        let prompt = llm.last_prompt().expect("one batched call");
        assert!(
            prompt.contains("2026-04-20T09:42:00Z"),
            "the batch line carries the semantic anchor: {prompt}"
        );
        let row = fact_index::find_by_id(&pool, &stale)
            .await
            .unwrap()
            .expect("row");
        assert!(
            !prompt.contains(&row.created_at),
            "the wall-clock insertion instant is not the anchor: {prompt}"
        );
        drop(dir);
    }

    /// The lexical pre-filter is word-bounded: "oggi" flags, a word that
    /// merely contains it ("oggigiorno") does not.
    #[test]
    fn looks_deictic_is_word_bounded() {
        assert!(looks_deictic("Oggi ha giocato 31 minuti"));
        assert!(looks_deictic("ci vediamo domani alle 9"));
        assert!(looks_deictic("la recita è la settimana prossima"));
        assert!(looks_deictic("watched it yesterday evening"));
        assert!(!looks_deictic("oggigiorno tutto cambia"));
        assert!(!looks_deictic("il viaggio del 10 giugno 2026"));
    }

    // ---------- provenance-hygiene sweep ----------

    /// The detector anchors on the exact trailing defect shape the
    /// document worker used to emit — and nothing else: mid-prose links,
    /// prose-bearing parentheticals, glued suffixes, and slash-less
    /// targets are content and never match.
    #[test]
    fn trailing_provenance_detector_matches_defect_shape_only() {
        // The defect: trailing ` ([[wiki/page]])`, whitespace-separated.
        assert_eq!(
            split_trailing_provenance_refs(
                "Bruno ha il diabete di tipo 2. ([[famiglia/dossier_clinico_bruno_2026]])"
            ),
            Some((
                "Bruno ha il diabete di tipo 2.".to_owned(),
                vec!["[[famiglia/dossier_clinico_bruno_2026]]".to_owned()],
            ))
        );
        // Trailing whitespace after the parenthetical is tolerated.
        assert_eq!(
            split_trailing_provenance_refs("Claim. ([[a/b]])  "),
            Some(("Claim.".to_owned(), vec!["[[a/b]]".to_owned()]))
        );
        // Multiple trailing parentheticals all move, in document order.
        assert_eq!(
            split_trailing_provenance_refs("Claim. ([[a/b]]) ([[c/d]])"),
            Some((
                "Claim.".to_owned(),
                vec!["[[a/b]]".to_owned(), "[[c/d]]".to_owned()],
            ))
        );
        // A wikilink mid-prose is content, never touched.
        assert_eq!(
            split_trailing_provenance_refs("vedi [[famiglia/dossier]] per i dettagli"),
            None
        );
        // A parenthetical link that is not trailing is content.
        assert_eq!(
            split_trailing_provenance_refs("il valore ([[a/b]]) è fuori range"),
            None
        );
        // Prose inside the parenthetical is not the defect shape.
        assert_eq!(
            split_trailing_provenance_refs("deciso al meeting (vedi [[a/b]])"),
            None
        );
        // Glued to the claim (no whitespace) is not the worker's emission.
        assert_eq!(split_trailing_provenance_refs("claim([[a/b]])"), None);
        // A target without `/` is not a wiki/page pointer.
        assert_eq!(split_trailing_provenance_refs("claim ([[dossier]])"), None);
        // A whitespace-bearing target is not the defect shape.
        assert_eq!(split_trailing_provenance_refs("claim ([[a b/c]])"), None);
        // A body that is nothing but the pointer is left alone.
        assert_eq!(split_trailing_provenance_refs("([[a/b]])"), None);
        // Already-clean text is a no-op.
        assert_eq!(
            split_trailing_provenance_refs("Bruno ha il diabete di tipo 2."),
            None
        );
    }

    /// A defect-suffixed fact is repaired in place: suffix stripped from
    /// the canonical text, pointer moved into `authored_refs`, text
    /// re-embedded, offsets kept, row active.
    #[tokio::test]
    async fn provenance_hygiene_moves_trailing_pointer_into_authored_refs() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let suffixed = plant_fact(
            &tree,
            &pool,
            "alice",
            "Bruno ha il diabete di tipo 2. ([[famiglia/dossier_clinico_bruno_2026]])",
            "alice",
        )
        .await;
        plant_fact(&tree, &pool, "alice", "Vive a Lisbona", "alice").await;
        let before = fact_index::find_by_id(&pool, &suffixed)
            .await
            .unwrap()
            .expect("row");

        // A different fixed vector than plant time proves the re-embed.
        let sweep_embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::with_fixed_embedding(
            "fake-bge",
            vec![0.9, 0.8, 0.7, 0.6],
        ));
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_provenance_hygiene(
            &pool,
            &sweep_embedder,
            "cycle-test",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");

        assert_eq!(report.flagged, 1, "only the suffixed fact is flagged");
        assert_eq!(report.examined, 1);
        assert_eq!(report.moved, vec![suffixed.as_str().to_owned()]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let row = fact_index::find_by_id(&pool, &suffixed)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.text, "Bruno ha il diabete di tipo 2.");
        assert_eq!(
            row.authored_refs,
            vec!["[[famiglia/dossier_clinico_bruno_2026]]".to_owned()]
        );
        assert_eq!(row.embedding, vec![0.9, 0.8, 0.7, 0.6], "text re-embedded");
        assert_eq!(row.region_start, before.region_start, "offsets kept");
        assert_eq!(row.region_end, before.region_end, "offsets kept");
        assert!(row.deleted_at.is_none());
        drop(dir);
    }

    /// A pointer already recorded in `authored_refs` is not duplicated by
    /// the move, and a second pass over the repaired corpus is a no-op —
    /// the sweep is convergent.
    #[tokio::test]
    async fn provenance_hygiene_dedups_refs_and_is_idempotent() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let suffixed = plant_fact(
            &tree,
            &pool,
            "alice",
            "Il 12 giugno Bruno ha fatto le analisi. ([[famiglia/dossier_2026]])",
            "alice",
        )
        .await;
        // The pointer is already recorded (a partially repaired corpus).
        sqlx::query(r#"UPDATE fact_index SET authored_refs = '["[[famiglia/dossier_2026]]"]' WHERE fact_id = ?"#)
            .bind(suffixed.as_str())
            .execute(&pool)
            .await
            .unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_provenance_hygiene(
            &pool,
            &fake_embedder(),
            "cycle-test",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(report.moved, vec![suffixed.as_str().to_owned()]);
        let row = fact_index::find_by_id(&pool, &suffixed)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.text, "Il 12 giugno Bruno ha fatto le analisi.");
        assert_eq!(
            row.authored_refs,
            vec!["[[famiglia/dossier_2026]]".to_owned()],
            "the already-present pointer is not duplicated"
        );

        // Second pass: the corpus is clean, the sweep no-ops.
        let again = run_provenance_hygiene(
            &pool,
            &fake_embedder(),
            "cycle-test-2",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("second sweep");
        assert_eq!(again.flagged, 0);
        assert!(again.moved.is_empty());
        let row2 = fact_index::find_by_id(&pool, &suffixed)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row2.text, row.text);
        assert_eq!(row2.authored_refs, row.authored_refs);
        drop(dir);
    }

    /// The per-cycle cap bounds the sweep; the residue drains on the next
    /// cycle (convergence across cycles).
    #[tokio::test]
    async fn provenance_hygiene_respects_cap() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        for body in [
            "Bruno ha il diabete di tipo 2. ([[famiglia/dossier_a]])",
            "Le analisi mostrano glicemia alta. ([[famiglia/dossier_b]])",
            "La visita di controllo è fissata. ([[famiglia/dossier_c]])",
        ] {
            plant_fact(&tree, &pool, "alice", body, "alice").await;
        }
        let policy = RemPolicy {
            provenance_hygiene_cap: 2,
            ..RemPolicy::default()
        };
        let index = load_smart_wiki_index(&tree).expect("index");
        let first = run_provenance_hygiene(&pool, &fake_embedder(), "cycle-1", &policy, &index)
            .await
            .expect("sweep");
        assert_eq!(first.flagged, 3);
        assert_eq!(first.examined, 2, "cap applied");
        assert_eq!(first.moved.len(), 2);

        let second = run_provenance_hygiene(&pool, &fake_embedder(), "cycle-2", &policy, &index)
            .await
            .expect("sweep");
        assert_eq!(second.flagged, 1, "the residue drains next cycle");
        assert_eq!(second.moved.len(), 1);
        for row in fact_index::find_active_in_wiki(&pool, "alice")
            .await
            .unwrap()
        {
            assert!(
                split_trailing_provenance_refs(&row.text).is_none(),
                "corpus converged: {}",
                row.text
            );
        }
        drop(dir);
    }

    /// Smart-wiki rows are section projections of consumer-authored files
    /// — the sweep never edits them.
    #[tokio::test]
    async fn provenance_hygiene_skips_smart_wikis() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        plant_fact(
            &tree,
            &pool,
            "alice-lnprint",
            "Design note. ([[alice-lnprint/notes]])",
            "alice",
        )
        .await;

        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_provenance_hygiene(
            &pool,
            &fake_embedder(),
            "cycle-test",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(report.flagged, 0, "smart-wiki rows are never flagged");
        assert!(report.moved.is_empty());
        drop(dir);
    }

    // ---------- family scopes (leva-2) ----------

    /// A sub-wiki nested under `parent` — the directory nesting IS the
    /// family relation [`family_scopes`] reads (the id is deliberately
    /// NOT `parent-child` shaped in one test case, to pin that ids are
    /// never string-matched).
    fn write_sub_wiki(tree: &WikiTree, parent: &str, child_slug: &str, wiki_id: &str, owner: &str) {
        let dir = tree.wikis_dir().join(parent).join(child_slug);
        std::fs::create_dir_all(&dir).unwrap();
        let frontmatter = format!(
            "---\nwiki_id: {wiki_id}\nwiki_type: wiki-tech\nslug: {child_slug}\ntitle: {child_slug}\nacl_default: 'user:{owner}'\nparent_wiki_id: {parent}\n---\n",
        );
        std::fs::write(dir.join("_meta.md"), frontmatter).unwrap();
        std::fs::write(dir.join("index.md"), "# sub\n").unwrap();
    }

    #[tokio::test]
    async fn family_scopes_partition_by_directory_nesting_not_id() {
        let (dir, mut tree, _pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        write_sub_wiki(&tree, "famiglia", "bruno", "famiglia-bruno", "alice");
        // A top-level wiki whose id LOOKS like a child of famiglia — the
        // partition must not be fooled by the hyphen.
        write_wiki(&tree, "famiglia-amici", "Amici", "wiki-group");
        write_smart_wiki(&tree, "alice-lnprint", "lnprint companion", "alice");
        tree = WikiTree::open(dir.path()).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let scopes = family_scopes(&tree, &index).expect("scopes");
        let mut got: Vec<(String, Vec<String>)> = scopes
            .into_iter()
            .map(|s| (s.root_id, s.wiki_ids))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    "famiglia".to_owned(),
                    vec!["famiglia".to_owned(), "famiglia-bruno".to_owned()]
                ),
                (
                    "famiglia-amici".to_owned(),
                    vec!["famiglia-amici".to_owned()]
                ),
            ],
            "nesting groups, hyphens don't, smart wikis are out"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn revisor_dedups_across_the_family_line() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        write_sub_wiki(&tree, "famiglia", "bruno", "famiglia-bruno", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        // The split-subject duplicate: the same identity fact captured in
        // the parent wiki and again in the emergent sub-wiki. Jaccard-kin
        // but not identical (the capture-time dedup threshold is off).
        let older = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "bruno_battaglia.md",
            "Bruno Battaglia è il padre di Franz e vive a Ferrara",
            "alice",
        )
        .await;
        let newer = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia-bruno",
            "anagrafica.md",
            "Bruno Battaglia è il padre di Franz e vive a Ferrara in centro",
            "alice",
        )
        .await;

        let llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_revisor_jaccard(
            &pool,
            &tree,
            &fake_embedder(),
            &llm,
            "cycle-fam",
            &RemPolicy::default(),
            &load_smart_wiki_index(&tree).expect("index"),
        )
        .await
        .expect("revisor");
        assert_eq!(
            report.applied.len(),
            1,
            "the parent↔sub-wiki pair merged: {report:?}"
        );
        let loser = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .unwrap();
        assert!(
            loser.superseded_at.is_some(),
            "the older parent-side copy retired"
        );
        assert_eq!(loser.superseded_by.as_ref(), Some(&newer));
        let winner = fact_index::find_by_id(&pool, &newer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            winner.wiki_id, "famiglia-bruno",
            "the survivor stays in its own wiki"
        );
        drop(dir);
    }

    /// Identity-core stickiness (leva 3): the REM dedup revisor never
    /// retires a fact from a person's always-on identity core
    /// (`bio` + `salience=high`), even a genuine near-duplicate. A
    /// relationship like "X è il compagno di Y" changes only on an
    /// explicit correction, never by silent background consolidation.
    /// Same near-duplicate pair as `revisor_dedups_across_the_family_line`,
    /// but the would-be loser is identity-core — so nothing merges.
    #[tokio::test]
    async fn revisor_never_retires_an_identity_core_fact() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        write_sub_wiki(&tree, "famiglia", "bruno", "famiglia-bruno", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        let older = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "bruno_battaglia.md",
            "Bruno Battaglia è il padre di Franz e vive a Ferrara",
            "alice",
        )
        .await;
        // Mark the older (would-be loser) copy as identity core.
        sqlx::query("UPDATE fact_index SET fact_type = 'bio', salience = 'high' WHERE fact_id = ?")
            .bind(older.as_str())
            .execute(&pool)
            .await
            .expect("mark identity core");
        let _newer = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia-bruno",
            "anagrafica.md",
            "Bruno Battaglia è il padre di Franz e vive a Ferrara in centro",
            "alice",
        )
        .await;

        let llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_revisor_jaccard(
            &pool,
            &tree,
            &fake_embedder(),
            &llm,
            "cycle-core",
            &RemPolicy::default(),
            &load_smart_wiki_index(&tree).expect("index"),
        )
        .await
        .expect("revisor");
        assert!(
            report.applied.is_empty(),
            "no dedup applied — the identity-core loser is skipped: {report:?}"
        );
        let loser = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .unwrap();
        assert!(
            loser.superseded_at.is_none(),
            "the identity-core fact stays active — never silently retired"
        );
        drop(dir);
    }

    /// The SEMANTIC nomination channel: a subject-elided restatement
    /// shares meaning but few n-grams — its jaccard sits below the
    /// surface floor, so only the embedding cosine can nominate the
    /// pair (the prod shape: "È nato il 23 maggio 1984" woven into the
    /// person's page vs the spelled-out re-capture of the same claim).
    /// The confirm prompt must carry each region's page so the model
    /// can resolve the elided subject.
    #[tokio::test]
    async fn revisor_cosine_channel_nominates_what_jaccard_misses() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "franz", "Franz", "wiki-user");
        tree = WikiTree::open(dir.path()).unwrap();
        let old_text = "È nato il 23 maggio 1984";
        let new_text = "Francesco Battaglia (Franz) è nato il 23 maggio 1984.";
        // Sanity: the pair really is invisible to the surface band.
        assert!(
            recall::jaccard_6gram(new_text, old_text) < RemPolicy::default().revisor_jaccard_min,
            "test premise: below the jaccard floor"
        );
        // Two CLOSE but non-identical vectors (cosine ≈ 0.999): the
        // bit-identity guard must not block a genuine near-duplicate.
        let older = plant_fact_with_embedder(
            &tree,
            &pool,
            Arc::new(FakeEmbedder::with_fixed_embedding(
                "e1",
                vec![1.0, 0.0, 0.0, 0.0],
            )),
            "franz",
            "index.md",
            old_text,
            "franz",
        )
        .await;
        let newer = plant_fact_with_embedder(
            &tree,
            &pool,
            Arc::new(FakeEmbedder::with_fixed_embedding(
                "e2",
                vec![0.999, 0.04, 0.0, 0.0],
            )),
            "franz",
            "index.md",
            new_text,
            "franz",
        )
        .await;

        let llm = FakeLlmBackend::new("rev", "{\"same\": true}");
        let report = run_revisor_jaccard(
            &pool,
            &tree,
            &fake_embedder(),
            &llm,
            "cycle-cos",
            &RemPolicy::default(),
            &load_smart_wiki_index(&tree).expect("index"),
        )
        .await
        .expect("revisor");
        assert_eq!(report.pairs_examined, 1, "{report:?}");
        assert_eq!(
            report.applied.len(),
            1,
            "cosine-nominated pair merged: {report:?}"
        );
        let loser = fact_index::find_by_id(&pool, &older)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loser.superseded_by.as_ref(), Some(&newer));
        // The confirm prompt framed both sides with their page.
        let prompt = llm.last_prompt().expect("prompt recorded");
        assert!(prompt.contains("franz · wikis/franz/index.md"), "{prompt}");
        drop(dir);
    }

    #[tokio::test]
    async fn page_merge_crosses_the_family_line_and_reverts() {
        let (dir, mut tree, pool) = setup_workdir().await;
        write_wiki(&tree, "famiglia", "Famiglia", "wiki-group");
        write_sub_wiki(&tree, "famiglia", "bruno", "famiglia-bruno", "alice");
        tree = WikiTree::open(dir.path()).unwrap();
        // The dossier told twice: a parent-side page and the sub-wiki
        // detail page. The confirmer picks the sub-wiki page as survivor.
        let f1 = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia",
            "dossier_clinico.md",
            "gli esami del sangue mostrano anemia",
            "alice",
        )
        .await;
        let f2 = plant_fact_on_page(
            &tree,
            &pool,
            "famiglia-bruno",
            "dossier_clinico_bruno.md",
            "il referto oculistico è pulito",
            "alice",
        )
        .await;
        save_leaf_plan(
            &tree,
            &pool,
            &[
                ("dossier_clinico", std::slice::from_ref(&f1)),
                ("dossier_clinico_bruno", std::slice::from_ref(&f2)),
            ],
        )
        .await;

        let merge_llm = FakeLlmBackend::new(
            "rev",
            "{\"merge\": true, \"survivor\": \"dossier_clinico_bruno\", \"reason\": \"same dossier\"}",
        );
        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_page_merge(
            &pool,
            &tree,
            &merge_llm,
            "cycle-fam",
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("merge sub-job");
        assert_eq!(report.applied.len(), 1, "errors: {:?}", report.errors);

        // The husk (parent side) is gone; the moved row re-homed its
        // wiki_id INTO the sub-wiki (move_to_wiki, not move_region).
        assert!(
            !tree
                .wikis_dir()
                .join("famiglia/dossier_clinico.md")
                .exists()
        );
        let moved = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert_eq!(moved.wiki_id, "famiglia-bruno");
        assert_eq!(
            moved.source_path,
            "wikis/famiglia/bruno/dossier_clinico_bruno.md"
        );
        let survivor_body = std::fs::read_to_string(
            tree.wikis_dir()
                .join("famiglia/bruno/dossier_clinico_bruno.md"),
        )
        .unwrap();
        assert!(survivor_body.contains(&format!("f={f1}")), "marker moved");

        // The revert walks the whole road back: husk file recreated in
        // the PARENT wiki, the row's wiki_id restored.
        let spec: String =
            sqlx::query_scalar("SELECT spec FROM structure_proposals WHERE proposal_id = ?")
                .bind(&report.applied[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
        promote::revert_wiki_promote(&pool, &tree, &spec)
            .await
            .expect("revert");
        let back = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert_eq!(back.wiki_id, "famiglia", "wiki_id restored by the revert");
        assert_eq!(back.source_path, "wikis/famiglia/dossier_clinico.md");
        assert!(
            tree.wikis_dir()
                .join("famiglia/dossier_clinico.md")
                .exists(),
            "husk recreated"
        );
        drop(dir);
    }

    // ---------- husk-page GC ----------

    /// Supersede `fact_id` and back-date the retirement past the revert
    /// window, so the on-disk marker no longer serves any revert.
    async fn supersede_aged(pool: &SqlitePool, fact_id: &FactId) {
        let succ = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dff").unwrap();
        fact_index::mark_superseded(pool, fact_id, &succ)
            .await
            .expect("supersede");
        sqlx::query("UPDATE fact_index SET superseded_at = ? WHERE fact_id = ?")
            .bind("2026-01-01T00:00:00+00:00")
            .bind(fact_id.as_str())
            .execute(pool)
            .await
            .expect("age");
    }

    /// A present plan none of the husk fixtures belong to. The sweep
    /// needs a persisted plan (a fresh workdir's pages are unplanned,
    /// not husks) — and `load_previous_plan` reads an all-empty plan as
    /// no-plan, so this one carries an unrelated `compilation_order`
    /// entry.
    fn save_unrelated_plan(tree: &WikiTree) {
        let plan = CompilationPlan {
            pages: std::collections::BTreeMap::new(),
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: vec!["altrove".to_owned()],
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        crate::planner::save_plan(tree, &plan).unwrap();
    }

    /// The husk shape end-to-end: a plan-absent page whose only row is
    /// superseded past the revert window is removed (offsets settled);
    /// the per-cycle cap defers the rest in deterministic path order; a
    /// missing plan is a no-op.
    #[tokio::test]
    async fn husk_gc_removes_plan_absent_pages_once_rows_are_past_any_revert() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        let f1 = plant_fact_on_page(&tree, &pool, "alice", "vecchia.md", "husk uno", "alice").await;
        let f2 = plant_fact_on_page(&tree, &pool, "alice", "vetusta.md", "husk due", "alice").await;
        supersede_aged(&pool, &f1).await;
        supersede_aged(&pool, &f2).await;
        let index = load_smart_wiki_index(&tree).expect("index");

        // No plan on disk → no-op (unplanned ≠ husk).
        let report = run_husk_gc(
            &pool,
            &tree,
            "cycle-husk",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(report.pages_examined, 0, "no plan → nothing examined");
        assert!(report.removed.is_empty());

        save_unrelated_plan(&tree);
        let capped = RemPolicy {
            husk_gc_cap: 1,
            ..RemPolicy::default()
        };
        let report = run_husk_gc(&pool, &tree, "cycle-husk", Utc::now(), &capped, &index)
            .await
            .expect("sweep");
        assert_eq!(report.pages_examined, 2);
        assert_eq!(
            report.removed,
            vec!["wikis/alice/vecchia.md".to_owned()],
            "path order is deterministic; the cap takes the first"
        );
        assert_eq!(report.deferred, 1, "the second husk waits its cycle");
        assert!(!dir.path().join("wikis/alice/vecchia.md").exists());
        assert!(dir.path().join("wikis/alice/vetusta.md").exists());

        // The removed page's retired row is settled (offsets NULL) so the
        // retirement sweep never reopens a file that no longer exists.
        let row = fact_index::find_by_id(&pool, &f1).await.unwrap().unwrap();
        assert!(row.region_start.is_none() && row.region_end.is_none());

        // Next cycle drains the backlog.
        let report = run_husk_gc(
            &pool,
            &tree,
            "cycle-husk-2",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(report.removed, vec!["wikis/alice/vetusta.md".to_owned()]);
        assert!(!dir.path().join("wikis/alice/vetusta.md").exists());
        drop(dir);
    }

    /// The DB-first guards: an active row keeps the file, a supersession
    /// still inside the revert window keeps the file, plan membership
    /// keeps the file (never examined), and reserved names never qualify.
    #[tokio::test]
    async fn husk_gc_keeps_active_recent_planned_and_reserved_pages() {
        let (dir, tree, pool) = setup_workdir().await;
        write_wiki(&tree, "alice", "Alice", "wiki-user");
        // Active fact → blocks.
        plant_fact_on_page(&tree, &pool, "alice", "attiva.md", "fatto vivo", "alice").await;
        // Superseded NOW (inside the revert window) → blocks.
        let fresh =
            plant_fact_on_page(&tree, &pool, "alice", "fresca.md", "appena caduto", "alice").await;
        let succ = FactId::parse("0190f3c2-7a4e-7c31-9b02-2f6a1c8e5dfe").unwrap();
        fact_index::mark_superseded(&pool, &fresh, &succ)
            .await
            .expect("supersede");
        // Plan-member page with no rows → never a candidate.
        std::fs::write(dir.path().join("wikis/alice/pianificata.md"), "# planned\n").unwrap();
        // Reserved name with no rows → never a candidate.
        std::fs::write(dir.path().join("wikis/alice/rules.md"), "# rules\n").unwrap();

        let mut pages = std::collections::BTreeMap::new();
        pages.insert(
            "pianificata".to_owned(),
            PagePlan {
                slug: "pianificata".to_owned(),
                title: "Pianificata".to_owned(),
                description: String::new(),
                style: None,
                page_type: PageType::ConceptLeaf,
                owner_scope: None,
                parent_hub: None,
                child_leaves: Vec::new(),
                primary_facts: Vec::new(),
                outgoing_links: Vec::new(),
                incoming_links: Vec::new(),
                wiki_id: "alice".to_owned(),
                page_path: "pianificata.md".to_owned(),
            },
        );
        let plan = CompilationPlan {
            pages,
            merged_pages: Vec::new(),
            link_graph: std::collections::BTreeMap::new(),
            compilation_order: Vec::new(),
            generated_at: "t".to_owned(),
            fact_count: 0,
            dirty_pages: Vec::new(),
            force_dirty: Vec::new(),
            refile_candidates: Vec::new(),
            reopen_pages: Vec::new(),
        };
        crate::planner::save_plan(&tree, &plan).unwrap();

        let index = load_smart_wiki_index(&tree).expect("index");
        let report = run_husk_gc(
            &pool,
            &tree,
            "cycle-husk",
            Utc::now(),
            &RemPolicy::default(),
            &index,
        )
        .await
        .expect("sweep");
        assert_eq!(
            report.pages_examined, 2,
            "only the two plan-absent, non-reserved pages are checked"
        );
        assert!(report.removed.is_empty(), "every guard held: {report:?}");
        for page in ["attiva.md", "fresca.md", "pianificata.md", "rules.md"] {
            assert!(
                dir.path().join("wikis/alice").join(page).exists(),
                "{page} must survive"
            );
        }
        drop(dir);
    }
}
